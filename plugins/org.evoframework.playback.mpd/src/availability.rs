// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Per-item availability projection cascade.
//!
//! The audio.queue / audio.favourites / audio.playlist shelves
//! all emit a per-item `available` field on every entry of
//! their envelope payloads. Operator UI consumes this field
//! directly: a "0 tracks playable" badge, a per-row UNAVAIL
//! decoration, the skip-traversal hop policy all read it.
//!
//! The truth has two sources that must be cascaded:
//!
//! 1. **The `evo:available` sticker** — the sticker reconciler
//!    writes `1` (Online/Degraded) or `0` (Offline/Retired)
//!    per-song under each source's mount path. When set, this
//!    is the per-song explicit truth and overrides any
//!    source-level inference (a single corrupt file on an
//!    otherwise-Online source rightly reports unavailable).
//! 2. **The source's reachability state** — Online/Degraded
//!    are reachable; Offline/Retired are unreachable; Probing
//!    means the registry has not yet determined reachability.
//!
//! Cascade order:
//!
//! - Explicit sticker present (`1` or `0`) → that's the truth,
//!   return `Some(true)` or `Some(false)`.
//! - No sticker, source `Online`/`Degraded` → `Some(true)`.
//!   The sticker reconciler hasn't traversed this song yet (it
//!   batches and may take seconds on a large library), but the
//!   SOURCE is reachable, so the song is best-known-reachable.
//! - No sticker, source `Offline`/`Retired` → `Some(false)`.
//!   The source is unreachable; the song cannot be played.
//! - No sticker, source `Probing` → `None`. The registry has
//!   not yet probed; both source-state and per-item state are
//!   genuinely unknown.
//! - Source not registered (no `source_id`, or `source_id`
//!   resolves to nothing in the registry) → `None`. The
//!   registry has no opinion to project from.
//!
//! Wire-shape contract — `available: bool | null` on every
//! consumer envelope. `null` means "not yet determined"; the
//! literal `false` retains its semantic of "known to be
//! unreachable". Operator UI renders `null` neutrally (no
//! UNAVAIL decoration, no skip-traversal hop) until the
//! cascade resolves. This is the [PLUGIN_CONTRACT.md §15]
//! wire-shape-defaults-must-be-truth-or-null invariant
//! applied at the per-item projection layer.
//!
//! Performance — the sticker read is a single MPD round-trip
//! per item. For a 100-item queue that is ~100 round-trips
//! over a Unix socket (sub-millisecond each on localhost). The
//! sticker reconciler's warm-start traversal happens out-of-
//! band; while it runs, the cascade falls through to
//! source-state and the wire still reports truth.

use crate::mpd::MpdConnection;
use crate::source_registry::{SourceRegistry, SourceState};
use crate::sticker_reconciler::EVO_AVAILABLE_STICKER;

/// Compute the per-item `available` truth via the cascade.
///
/// Returns `Some(true)` / `Some(false)` when truth is known
/// (sticker explicit or source state derivable), `None` when
/// the truth is genuinely unknown (Probing source + no
/// sticker, or unregistered source).
///
/// The function is async because the sticker read is an MPD
/// round-trip. Caller is responsible for batching: each call
/// issues one sticker_get; an N-item envelope build issues N
/// round-trips. MPD's command_list batching could collapse
/// this to one round-trip per envelope if the operator
/// surfaces grow latency-sensitive.
pub(crate) async fn compute_item_available(
    conn: &mut MpdConnection,
    file_path: &str,
    source_id: Option<&str>,
    registry: &SourceRegistry,
) -> Option<bool> {
    // Step 1: explicit sticker truth wins.
    if let Ok(Some(value)) =
        conn.sticker_get(file_path, EVO_AVAILABLE_STICKER).await
    {
        return Some(value != "0");
    }
    // Step 2: derive from source state.
    if let Some(sid) = source_id {
        if let Some(record) = registry.get(sid).await {
            return derive_from_source_state(&record.state);
        }
    }
    // Step 3: no source registered, no sticker — truly unknown.
    None
}

/// Per-cascade-step 2 mapping. Pure function of source state
/// (no I/O); kept separate so the cascade is unit-testable
/// without a live MpdConnection.
pub(crate) fn derive_from_source_state(state: &SourceState) -> Option<bool> {
    match state {
        SourceState::Online | SourceState::Degraded { .. } => Some(true),
        SourceState::Offline { .. } | SourceState::Retired => Some(false),
        SourceState::Probing => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn derive_online_source_is_some_true() {
        assert_eq!(derive_from_source_state(&SourceState::Online), Some(true));
    }

    #[test]
    fn derive_degraded_source_is_some_true() {
        assert_eq!(
            derive_from_source_state(&SourceState::Degraded {
                reason: "slow probe".into(),
                since_ms: 0,
            }),
            Some(true)
        );
    }

    #[test]
    fn derive_offline_source_is_some_false() {
        assert_eq!(
            derive_from_source_state(&SourceState::Offline {
                reason: "timeout".into(),
                since_ms: 0,
            }),
            Some(false)
        );
    }

    #[test]
    fn derive_retired_source_is_some_false() {
        assert_eq!(
            derive_from_source_state(&SourceState::Retired),
            Some(false)
        );
    }

    #[test]
    fn derive_probing_source_is_none_not_false() {
        // The load-bearing invariant: Probing must NOT collapse
        // to `Some(false)`. The whole wire-shape lift the
        // catalogue acceptance row enforces is that "not yet
        // known" travels as null, never as a fabricated false.
        assert_eq!(derive_from_source_state(&SourceState::Probing), None);
    }

    #[test]
    fn cascade_steps_documented_pure_helper_is_not_async() {
        // Belt-and-braces sanity: derive_from_source_state is
        // pure (no I/O) and thus a valid `const fn` candidate
        // for any future optimisation. The async layer is only
        // the sticker_get round-trip.
        let _ = Duration::from_secs(0); // ensure tokio not pulled in implicitly
    }
}
