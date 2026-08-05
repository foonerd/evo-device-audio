// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # network-shares-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.network.shares` plugin. Listens on the
//! Unix socket given as its sole positional argument, accepts
//! exactly one connection, serves it through the plugin SDK's
//! [`evo_plugin_sdk::host::run_oop`] helper, and exits when the
//! steward disconnects.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_network_shares::{NetworkSharesPlugin, PLUGIN_NAME};
use std::path::PathBuf;

fn main() -> ! {
    evo_plugin_sdk::wire_logging::init();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("network-shares-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "network-shares-wire starting"
    );
    let plugin = NetworkSharesPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "network-shares-wire")
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow!("usage: network-shares-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: network-shares-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
