use std::path::Path;

use dolos::{
    prelude::*,
    storage::{applydb::ApplyDB, rolldb::RollDB},
};
use tracing::warn;

#[derive(Debug, clap::Args)]
pub struct Args {}

#[tokio::main]
pub async fn run(
    config: super::Config,
    policy: &gasket::runtime::Policy,
    _args: &Args,
) -> Result<(), Error> {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::DEBUG)
            .finish(),
    )
    .unwrap();

    let rolldb_path = config
        .rolldb
        .path
        .as_deref()
        .unwrap_or_else(|| Path::new("/rolldb"));

    let rolldb =
        RollDB::open(rolldb_path, config.rolldb.k_param.unwrap_or(1000)).map_err(Error::storage)?;

    let applydb_path = config
        .applydb
        .path
        .as_deref()
        .unwrap_or_else(|| Path::new("/applydb"));

    let applydb = ApplyDB::open(applydb_path).map_err(Error::storage)?;

    // channel that connects output from sync pipeline to gRPC server
    let (to_serve, from_sync) = gasket::messaging::tokio::broadcast_channel(100);

    let rolldb_copy = rolldb.clone();

    if let Some(grpc_config) = config.serve.grpc {
        let server = tokio::spawn(dolos::serve::grpc::serve(
            grpc_config,
            rolldb_copy,
            from_sync.try_into().unwrap(),
        ));

        dolos::sync::pipeline(&config.upstream, rolldb, applydb, to_serve, policy)
            .unwrap()
            .block();

        server.abort();
    } else {
        warn!("no gRPC config found")
    }

    Ok(())
}
