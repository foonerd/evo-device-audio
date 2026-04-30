//! Out-of-process wire binary for `org.evoframework.network.nm`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop, HostConfig};
use org_evoframework_network_nm::{NetworkNmPlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let socket_path = parse_args()?;
    tracing::info!(
        plugin = PLUGIN_NAME,
        socket = %socket_path.display(),
        "network-nm-wire starting"
    );
    run_oop(
        NetworkNmPlugin::new(),
        HostConfig::new(PLUGIN_NAME),
        &socket_path,
    )
    .await?;
    tracing::info!("network-nm-wire: steward disconnected, exiting");
    Ok(())
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow!("usage: network-nm-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: network-nm-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
