// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Out-of-process wire binary for `org.evoframework.source.dlna`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_source_dlna::{DlnaSourcePlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn main() -> ! {
    init_logging();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("source-dlna-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "source-dlna-wire starting"
    );
    let plugin = DlnaSourcePlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "source-dlna-wire")
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
        .ok_or_else(|| anyhow!("usage: source-dlna-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: source-dlna-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
