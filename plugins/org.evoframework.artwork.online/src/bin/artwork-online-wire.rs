//! # artwork-online-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.artwork.online` plugin. Mirrors the
//! `artwork-local-wire` shape so the publish CI cross-builds both
//! online and local providers through the same release plane.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop, HostConfig};
use org_evoframework_artwork_online::{ArtworkOnlinePlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();

    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "artwork-online-wire starting"
    );

    let plugin = ArtworkOnlinePlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop(plugin, config, &socket_path).await?;
    tracing::info!("artwork-online-wire: steward disconnected, exiting");
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
        .ok_or_else(|| anyhow!("usage: artwork-online-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: artwork-online-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
