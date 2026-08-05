// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # artwork-online-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.artwork.online` plugin. Mirrors the
//! `artwork-local-wire` shape so the publish CI cross-builds both
//! online and local providers through the same release plane.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_artwork_online::{ArtworkOnlinePlugin, PLUGIN_NAME};
use std::path::PathBuf;

fn main() -> ! {
    evo_plugin_sdk::wire_logging::init();

    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("artwork-online-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "artwork-online-wire starting"
    );

    let plugin = ArtworkOnlinePlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "artwork-online-wire")
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
