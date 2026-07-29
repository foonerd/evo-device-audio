// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # playback-options-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.playback.options` plugin.
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
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_playback_options::{PlaybackOptionsPlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn main() -> ! {
    init_logging();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("playback-options-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "playback-options-wire starting"
    );
    let plugin = PlaybackOptionsPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "playback-options-wire")
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
        .ok_or_else(|| anyhow!("usage: playback-options-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: playback-options-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
