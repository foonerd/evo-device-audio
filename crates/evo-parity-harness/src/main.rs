// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! evo-parity-harness binary entry point.
//!
//! Connects to the steward's admin Unix socket and drives the
//! four-shelf parity surface
//! (`audio.queue` / `audio.playlist` / `audio.favourites` /
//! `audio.library`) plus the cross-cutting scenarios via
//! pre-state → mutate → post-state round-trips with
//! structured JSON output.
//!
//! Usage:
//!
//! ```text
//! sudo evo-parity-harness [/run/evo/evo.sock]
//! ```
//!
//! Stdout: one JSON line per case + one final summary line.
//! Exit: 0 iff every case is Pass. Failures and Skips both
//! non-zero so a CI gate doesn't have to distinguish.

mod favourites;
mod library;
mod playlist;
mod queue;
mod report;
mod scenarios;
mod wire;

use anyhow::Result;

use crate::report::{CaseResult, Summary};
use crate::wire::Wire;

const DEFAULT_SOCKET: &str = "/run/evo/evo.sock";

#[tokio::main]
async fn main() -> Result<()> {
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_SOCKET.to_string());

    let mut all: Vec<CaseResult> = Vec::new();
    let mut wire = Wire::connect(&socket).await?;

    let queue_cases = queue::run(&mut wire).await?;
    emit_all(&queue_cases);
    all.extend(queue_cases);

    let playlist_cases = playlist::run(&mut wire).await?;
    emit_all(&playlist_cases);
    all.extend(playlist_cases);

    let favourites_cases = favourites::run(&mut wire).await?;
    emit_all(&favourites_cases);
    all.extend(favourites_cases);

    let library_cases = library::run(&mut wire).await?;
    emit_all(&library_cases);
    all.extend(library_cases);

    let scenario_cases = scenarios::run(&mut wire).await?;
    emit_all(&scenario_cases);
    all.extend(scenario_cases);

    let summary = Summary::from(&all);
    summary.emit();

    if !summary.all_passed() {
        std::process::exit(1);
    }
    Ok(())
}

fn emit_all(cases: &[CaseResult]) {
    for c in cases {
        c.emit();
    }
}
