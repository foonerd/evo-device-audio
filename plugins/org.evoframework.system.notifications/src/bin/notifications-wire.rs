// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # notifications-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.system.notifications` plugin. Listens on the
//! Unix socket given as its sole positional argument, accepts
//! exactly one connection, serves it through the plugin SDK's
//! [`evo_plugin_sdk::host::run_oop`] helper, and exits when the
//! steward disconnects.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use evo_plugin_sdk::host::{run_oop_and_exit, HostConfig};
use org_evoframework_system_notifications::{NotificationsPlugin, PLUGIN_NAME};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn main() -> ! {
    init_logging();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("notifications-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "notifications-wire starting"
    );
    let plugin = NotificationsPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "notifications-wire")
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
        .ok_or_else(|| anyhow!("usage: notifications-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: notifications-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
