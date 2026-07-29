// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Out-of-process wire binary for `org.evoframework.network`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_network::{NetworkPlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn main() -> ! {
    init_logging();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("network-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        plugin = PLUGIN_NAME,
        socket = %socket_path.display(),
        "network-wire starting"
    );
    run_oop_and_exit(
        NetworkPlugin::new(),
        HostConfig::new(PLUGIN_NAME),
        &socket_path,
        "network-wire",
    )
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
        .ok_or_else(|| anyhow!("usage: network-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: network-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
