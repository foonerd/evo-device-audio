// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # receiver-airplay-wire
//!
//! Out-of-process binary for the `org.evoframework.receiver.airplay` plugin.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_receiver_airplay::{ReceiverAirplayPlugin, PLUGIN_NAME};
use std::path::PathBuf;

fn main() -> ! {
    evo_plugin_sdk::wire_logging::init();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("receiver-airplay-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "receiver-airplay-wire starting"
    );
    let plugin = ReceiverAirplayPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "receiver-airplay-wire")
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow!("usage: receiver-airplay-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: receiver-airplay-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
