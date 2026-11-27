// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! The unit's process: parse the token with the same closed grammar, take
//! the lock, run one operation, and record how it ended.

use serde_json::{json, Value};

use crate::apply_host::apply_host;
use crate::apply_plugin::{apply_plugin, Outcome};
use crate::check::run_check;
use crate::ctx::Ctx;
use crate::error::{Res, UpdError};
use crate::grammar::{parse_token, Request, HOST_COMPONENT};
use crate::timeutil;

/// Run one request already parsed from a token. Returns the final status
/// document and whether the run counts as success.
pub fn dispatch(ctx: &Ctx, req: &Request) -> (Value, bool) {
    let started = timeutil::format_utc(ctx.now);
    let mut base = json!({"v": 1, "started_at_utc": started});
    let result: Res<(&'static str, Value)> = match req {
        Request::Check => {
            base["op"] = json!("check");
            run_check(ctx).and_then(|inv| {
                ctx.state.write_inventory(&inv)?;
                Ok(("checked", json!({"items": inv["items"].as_array().map_or(0, Vec::len)})))
            })
        }
        Request::Channel(c) => {
            base["op"] = json!("channel");
            ctx.roots
                .check_chain(&ctx.roots.state_dir)
                .and_then(|()| ctx.state.set_channel(*c))
                .map(|()| ("channel_set", json!({"channel": c.name()})))
        }
        Request::Apply { component, version } => {
            base["op"] = json!("apply");
            base["component"] = json!(component);
            base["version"] = json!(version);
            let _ = ctx.state.write_status(&{
                let mut s = base.clone();
                s["state"] = json!("applying");
                s
            });
            if component == HOST_COMPONENT {
                apply_host(ctx, version).map(|s| ("applied", s))
            } else {
                apply_plugin(ctx, component, version).and_then(
                    |(outcome, summary)| match outcome {
                        Outcome::Applied => Ok(("applied", summary)),
                        Outcome::RolledBack(reason) => {
                            Err(UpdError::new("rolled_back", reason))
                        }
                    },
                )
            }
        }
    };
    let finished = timeutil::format_utc(timeutil::now_epoch());
    base["finished_at_utc"] = json!(finished);
    let ok = match result {
        Ok((state, summary)) => {
            base["state"] = json!(state);
            base["summary"] = summary;
            true
        }
        Err(e) => {
            base["state"] = json!(if e.code == "rolled_back" {
                "rolled_back"
            } else {
                "refused"
            });
            base["error"] = json!({"code": e.code, "detail": e.detail});
            false
        }
    };
    let _ = ctx.state.write_status(&base);
    if !matches!(req, Request::Check) {
        ctx.state.history(base.clone());
    }
    (base, ok)
}

/// Parse a unit token and dispatch it.
pub fn run_token(ctx: &Ctx, token: &str) -> (Value, bool) {
    match parse_token(token) {
        Ok(req) => match ctx.state.lock() {
            Ok(_guard) => dispatch(ctx, &req),
            Err(e) => (
                json!({"v": 1, "state": "refused", "error": {"code": e.code, "detail": e.detail}}),
                false,
            ),
        },
        Err(e) => {
            let s = json!({"v": 1, "state": "refused", "error": {"code": e.code, "detail": e.detail}});
            let _ = ctx.state.write_status(&s);
            (s, false)
        }
    }
}
