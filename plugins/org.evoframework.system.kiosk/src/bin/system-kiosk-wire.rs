// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # system-kiosk-wire
//!
//! Out-of-process reference binary for the
//! `org.evoframework.system.kiosk` plugin.
//!
//! Listens on a Unix socket given as its sole positional
//! argument, accepts exactly one connection, serves that
//! connection through the plugin SDK's
//! [`evo_plugin_sdk::host::run_oop`] helper, and exits when the
//! steward disconnects.
//!
//! Logging goes to stderr. The log filter can be overridden via
//! the `RUST_LOG` environment variable; the default is `warn`.
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
use org_evoframework_system_kiosk::{SystemKioskPlugin, PLUGIN_NAME};
use std::path::PathBuf;

fn main() -> ! {
    evo_plugin_sdk::wire_logging::init();
    let socket_path = match parse_args() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("system-kiosk-wire: {e}");
            std::process::exit(2);
        }
    };
    tracing::info!(
        socket = %socket_path.display(),
        plugin = PLUGIN_NAME,
        "system-kiosk-wire starting"
    );
    let plugin = SystemKioskPlugin::new();
    let config = HostConfig::new(PLUGIN_NAME);
    run_oop_and_exit(plugin, config, &socket_path, "system-kiosk-wire")
}

fn parse_args() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow!("usage: system-kiosk-wire <socket-path>"))?;
    if args.next().is_some() {
        return Err(anyhow!(
            "usage: system-kiosk-wire <socket-path> (too many arguments)"
        ));
    }
    Ok(PathBuf::from(path))
}
