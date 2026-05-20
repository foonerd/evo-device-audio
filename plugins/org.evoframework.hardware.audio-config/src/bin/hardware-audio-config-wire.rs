//! # hardware-audio-config-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.hardware.audio-config` plugin.
//!
//! Listens on a Unix socket given as its sole positional argument,
//! accepts exactly one connection, serves that connection through
//! the plugin SDK's [`evo_plugin_sdk::host::run_oop`] helper, and
//! exits when the steward disconnects.
//!
//! Logging goes to stderr. The log filter can be overridden via the
//! `RUST_LOG` environment variable; the default is `warn`.
//!
//! ## Lifecycle and exit codes
//!
//! * `0` — steward disconnected cleanly, [`run_oop`] returned `Ok`.
//! * `1` — argument parsing, socket binding, accept, or
//!   [`run_oop`] errored.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop, HostConfig};
use org_evoframework_hardware_audio_config::{
    HardwareAudioConfigPlugin, PLUGIN_NAME,
};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "hardware-audio-config-wire starting"
    );
    let plugin = HardwareAudioConfigPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop(plugin, config, &socket_path).await?;
    tracing::info!("hardware-audio-config-wire: steward disconnected, exiting");
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
    let path = args.next().ok_or_else(|| {
        anyhow!("usage: hardware-audio-config-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: hardware-audio-config-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
