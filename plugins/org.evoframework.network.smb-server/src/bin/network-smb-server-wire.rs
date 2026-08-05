// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # network-smb-server-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.network.smb-server` plugin.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_network_smb_server::{SmbServerPlugin, PLUGIN_NAME};
use std::path::PathBuf;

fn main() -> ! {
    evo_plugin_sdk::wire_logging::init();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("network-smb-server-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "network-smb-server-wire starting"
    );
    let plugin = SmbServerPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "network-smb-server-wire")
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| {
        anyhow!("usage: network-smb-server-wire <socket-path>")
    })?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: network-smb-server-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
