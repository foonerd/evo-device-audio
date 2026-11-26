// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! The sudo target. It parses the closed grammar and starts one unit
//! instance. It fetches nothing, verifies nothing and
//! installs nothing.

use serde_json::{json, Value};

use crate::error::{Res, UpdError};
use crate::grammar::{parse_args, token, Request};
use crate::host::Host;
use crate::roots::Roots;
use crate::state::State;

/// Handle one wrapper invocation. Returns the answer to print.
pub fn handle(args: &[String], roots: &Roots, host: &dyn Host) -> Res<Value> {
    let req = parse_args(args)?;
    roots.check_chain(&roots.state_dir)?;
    let state = State::new(&roots.state_dir);
    if state.busy() {
        return Err(UpdError::new("busy", "another update is running"));
    }
    let op = match &req {
        Request::Check => "check",
        Request::Apply { .. } => "apply",
        Request::Channel(_) => "channel",
    };
    // The wrapper runs inside the steward's sandbox, where the state
    // directory is read-only: it writes nothing. The worker records the
    // run; a surface that wants "starting" notes the time it asked.
    host.start_unit(&token(&req))?;
    Ok(json!({"v": 1, "accepted": true, "op": op}))
}
