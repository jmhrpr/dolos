use futures_core::Stream;
use pallas::{
    crypto::hash::Hash,
    storage::rolldb::{
        chain,
        wal::{self, RollStream},
    },
};
use std::pin::Pin;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status};
use tracing::error;
use utxorpc_spec::utxorpc::v1alpha::{self as u5c, sync::BlockRef};

fn bytes_to_hash(raw: &[u8]) -> Hash<32> {
    let array: [u8; 32] = raw.try_into().unwrap();
    Hash::<32>::new(array)
}

// fn raw_to_anychain2(raw: &[u8]) -> AnyChainBlock {
//     let block = any_chain_block::Chain::Raw(Bytes::copy_from_slice(raw));
//     AnyChainBlock { chain: Some(block) }
// }

fn raw_to_anychain(raw: &[u8]) -> u5c::sync::AnyChainBlock {
    let block = pallas::interop::utxorpc::map_block_cbor(raw);

    u5c::sync::AnyChainBlock {
        chain: u5c::sync::any_chain_block::Chain::Cardano(block).into(),
    }
}

fn roll_to_tip_response(log: wal::Log) -> u5c::sync::FollowTipResponse {
    u5c::sync::FollowTipResponse {
        action: match log {
            wal::Log::Apply(_, _, block) => {
                u5c::sync::follow_tip_response::Action::Apply(raw_to_anychain(&block)).into()
            }
            wal::Log::Undo(_, _, block) => {
                u5c::sync::follow_tip_response::Action::Undo(raw_to_anychain(&block)).into()
            }
            wal::Log::Mark(slot, hash, _) => {
                u5c::sync::follow_tip_response::Action::Reset(BlockRef {
                    index: slot,
                    hash: hash.to_vec().into(),
                })
                .into()
            }
            // TODO: shouldn't we have a u5c event for origin?
            wal::Log::Origin => None,
        },
    }
}

pub struct ChainSyncServiceImpl {
    wal: wal::Store,
    chain: chain::Store,
}

impl ChainSyncServiceImpl {
    pub fn new(wal: wal::Store, chain: chain::Store) -> Self {
        Self { wal, chain }
    }
}

#[async_trait::async_trait]
impl u5c::sync::chain_sync_service_server::ChainSyncService for ChainSyncServiceImpl {
    type FollowTipStream =
        Pin<Box<dyn Stream<Item = Result<u5c::sync::FollowTipResponse, Status>> + Send + 'static>>;

    async fn fetch_block(
        &self,
        request: Request<u5c::sync::FetchBlockRequest>,
    ) -> Result<Response<u5c::sync::FetchBlockResponse>, Status> {
        let message = request.into_inner();

        let blocks: Result<Vec<_>, _> = message
            .r#ref
            .iter()
            .map(|r| bytes_to_hash(&r.hash))
            .map(|hash| self.chain.get_block(hash))
            .collect();

        let out: Vec<_> = blocks
            .map_err(|_err| Status::internal("can't query block"))?
            .iter()
            .flatten()
            .map(|b| raw_to_anychain(b))
            .collect();

        let response = u5c::sync::FetchBlockResponse { block: out };

        Ok(Response::new(response))
    }

    async fn dump_history(
        &self,
        request: Request<u5c::sync::DumpHistoryRequest>,
    ) -> Result<Response<u5c::sync::DumpHistoryResponse>, Status> {
        let msg = request.into_inner();
        let from = msg.start_token.map(|r| r.index).unwrap_or_default();
        let len = msg.max_items as usize + 1;

        let mut page: Vec<_> = self
            .chain
            .read_chain_page(from, len)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_err| Status::internal("can't query history"))?;

        let next_token = if page.len() == len {
            let (next_slot, next_hash) = page.remove(len - 1);
            Some(u5c::sync::BlockRef {
                index: next_slot,
                hash: next_hash.to_vec().into(),
            })
        } else {
            None
        };

        let blocks = page
            .into_iter()
            .map(|(_, hash)| self.chain.get_block(hash))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_err| Status::internal("can't query history"))?
            .into_iter()
            .map(|x| x.ok_or(Status::internal("can't query history")))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|raw| raw_to_anychain(&raw))
            .collect();

        let response = u5c::sync::DumpHistoryResponse {
            block: blocks,
            next_token,
        };

        Ok(Response::new(response))
    }

    async fn follow_tip(
        &self,
        request: Request<u5c::sync::FollowTipRequest>,
    ) -> Result<Response<Self::FollowTipStream>, tonic::Status> {
        let request = request.into_inner();

        let intersects: Vec<_> = request
            .intersect
            .iter()
            .map(|x| (x.index, bytes_to_hash(&x.hash)))
            .collect();

        // if no intersect provided, stream WAL from start
        if intersects.is_empty() {
            let stream = RollStream::stream_wal(self.wal.clone(), None).map(|x| match x {
                Ok(log) => Ok(roll_to_tip_response(log)),
                Err(e) => {
                    error!("rollstream error: {e}");
                    Err(Status::internal("rollstream error"))
                }
            });

            return Ok(Response::new(Box::pin(stream)));
        }

        // else try intersect with the provided intersects
        for intersect in intersects {
            let maybe_wal_seq = self
                .wal
                .find_wal_seq(&[intersect])
                .map_err(|_| Status::internal("kvtable error"))?;

            if let Some(wal_seq) = maybe_wal_seq {
                let stream =
                    RollStream::stream_wal(self.wal.clone(), Some(wal_seq)).map(|x| match x {
                        Ok(log) => Ok(roll_to_tip_response(log)),
                        Err(e) => {
                            error!("rollstream error: {e}");
                            Err(Status::internal("rollstream error"))
                        }
                    });

                return Ok(Response::new(Box::pin(stream)));
            }
        }

        // error if we found no intersect
        Err(Status::not_found(
            "no intersect found in mutable part of chain",
        ))
    }
}
