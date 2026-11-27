// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The sudo target. A closed grammar in, one unit
//! instance started out. See the library for what it will and will not do.

use std::process::ExitCode;
use std::time::Duration;

use evo_apply_update::host::SystemHost;
use evo_apply_update::request::handle;
use evo_apply_update::roots::Roots;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let roots = Roots::system();
    let host = SystemHost {
        socket_dir: roots.socket_dir.clone(),
        ready_timeout: Duration::from_secs(1),
    };
    match handle(&args, &roots, &host) {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"v": 1, "accepted": false, "error": {"code": e.code, "detail": e.detail}})
            );
            ExitCode::from(2)
        }
    }
}
