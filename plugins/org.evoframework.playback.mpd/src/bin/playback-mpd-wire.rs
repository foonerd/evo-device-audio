//! # playback-mpd-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.playback.mpd` plugin.
//!
//! Plays through the SDK's `run_oop_warden_with_respondent`
//! entry — mpd is both a warden (custody-aware
//! course_correct dispatcher for `play / pause / seek /
//! set_volume / next / previous / stop`) and a respondent
//! (source-verb handler for `play_now` / `play` against the
//! `mpd-path` URI scheme). The combined dispatch loop on the
//! SDK side accepts both wire frame classes without the
//! warden's default rejection of respondent verbs firing.
//!
//! Logging goes to stderr. The log filter can be overridden via
//! the `RUST_LOG` environment variable; the default is `warn`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_warden_with_respondent, HostConfig};
use org_evoframework_playback_mpd::{MpdPlaybackPlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_logging();
    let socket_path = parse_args()?;
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "playback-mpd-wire starting"
    );
    let plugin = MpdPlaybackPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_warden_with_respondent(plugin, config, &socket_path).await?;
    tracing::info!("playback-mpd-wire: steward disconnected, exiting");
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
        .ok_or_else(|| anyhow!("usage: playback-mpd-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: playback-mpd-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
