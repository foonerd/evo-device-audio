// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Just a Nerd

//! The unit's process. Started by systemd as
//! `evo-apply-update@<token>.service`; not in any sudoers grant.

use std::process::ExitCode;
use std::time::Duration;

use evo_apply_update::ctx::{host_arch, host_triple, Ctx};
use evo_apply_update::docs::{Allow, Config};
use evo_apply_update::fetch::CurlFetch;
use evo_apply_update::host::SystemHost;
use evo_apply_update::roots::Roots;
use evo_apply_update::state::State;
use evo_apply_update::timeutil;
use evo_apply_update::trust::TrustStore;
use evo_apply_update::worker::run_token;

fn refuse(state: &State, code: &str, detail: &str) -> ExitCode {
    let s = serde_json::json!({
        "v": 1, "state": "refused",
        "finished_at_utc": timeutil::format_utc(timeutil::now_epoch()),
        "error": {"code": code, "detail": detail},
    });
    let _ = state.write_status(&s);
    eprintln!("evo-apply-update-worker: {code}: {detail}");
    ExitCode::from(1)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(token), None) = (args.next(), args.next()) else {
        eprintln!("usage: evo-apply-update-worker <token>");
        return ExitCode::from(2);
    };
    let roots = Roots::system();
    let state = State::new(&roots.state_dir);
    if let Err(e) = roots.check_for_read() {
        return refuse(&state, e.code, &e.detail);
    }
    let cfg = match Config::load(&roots.config_path()) {
        Ok(c) => c,
        Err(e) => return refuse(&state, e.code, &e.detail),
    };
    let trust = match TrustStore::load(&roots.trust_dir()) {
        Ok(t) => t,
        Err(e) => return refuse(&state, e.code, &e.detail),
    };
    let allow = match Allow::load(&roots.allow_path()) {
        Ok(a) => a,
        Err(e) => return refuse(&state, e.code, &e.detail),
    };
    if let Err(e) = state.ensure_stage() {
        return refuse(&state, e.code, &e.detail);
    }
    let fetch = CurlFetch {
        ca_file: cfg.ca_file.clone(),
        scratch: state.stage_dir(),
    };
    let host = SystemHost {
        socket_dir: roots.socket_dir.clone(),
        ready_timeout: Duration::from_secs(90),
    };
    let ctx = Ctx {
        roots: &roots,
        state: &state,
        cfg: &cfg,
        trust: &trust,
        allow: &allow,
        fetch: &fetch,
        host: &host,
        now: timeutil::now_epoch(),
        triple: host_triple(),
        arch: host_arch(),
    };
    let (status, ok) = run_token(&ctx, &token);
    println!("{status}");
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
