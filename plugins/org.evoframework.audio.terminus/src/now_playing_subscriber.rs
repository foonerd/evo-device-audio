// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Subscriber loop that watches the `audio_playback_now_playing`
//! subject and pushes `TransportGate` updates to the capture
//! loop's gate.
//!
//! Owns the SDK handles (`SubjectQuerier` for canonical-id
//! resolution + initial state read, `SubjectStateSubscriber` for
//! push-mode updates) and the `watch::Sender<TransportGate>` the
//! capture loop reads via a cloned `Receiver`.
//!
//! Lifecycle:
//! 1. Resolve the now-playing subject's canonical id with bounded
//!    backoff (the playback plugin's announce may not have landed
//!    by the time this subscriber spawns, so we retry until
//!    shutdown).
//! 2. Subscribe to the stream BEFORE reading current state — the
//!    no-race-window pattern the SDK docs describe at
//!    `SubjectStateSubscriber::subscribe_subject`.
//! 3. Seed the gate from the current state (handles the case
//!    where the subject was already announced before subscribe).
//! 4. Loop on stream updates; project each into a `TransportGate`
//!    and push via the watch sender.
//! 5. Exit cleanly on the shutdown notify.

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectQuerier, SubjectStateStreamError,
    SubjectStateSubscriber,
};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::transport_gate::{parse_from_state, TransportGate};

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Canonical now-playing subject scheme. Matches the
/// `playback.mpd` plugin's `SCHEME_STREAM_FORMAT` literal; the
/// two plugins agree on the wire-side addressing constants
/// without cross-plugin coupling.
const NOW_PLAYING_SCHEME: &str = "evo.audio.playback";

/// Canonical now-playing subject addressing value.
const NOW_PLAYING_VALUE: &str = "now_playing";

/// Backoff for the canonical-id resolve retry loop. The
/// playback plugin's `announce_now_playing` may not have landed
/// by the time this subscriber spawns; we poll at this cadence
/// until resolution succeeds or shutdown fires.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Handle returned from `spawn`. Dropping the handle (or
/// notifying via the cloned shutdown) terminates the subscriber
/// task at the next stream-or-shutdown branch.
pub struct SubscriberHandle {
    pub task: JoinHandle<()>,
    pub shutdown: Arc<Notify>,
}

/// Spawn the now-playing subscriber. Returns a `SubscriberHandle`
/// the plugin retains for the load's lifetime.
///
/// The `gate_tx` is the canonical `watch::Sender<TransportGate>`
/// the plugin constructed at load; the capture loop holds a
/// cloned `watch::Receiver` and reads the current value before
/// each emit. `gate_tx` is held by this task until shutdown.
pub fn spawn(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    gate_tx: watch::Sender<TransportGate>,
) -> SubscriberHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);

    let task = tokio::spawn(async move {
        run(subscriber, querier, gate_tx, task_shutdown).await;
    });

    SubscriberHandle { task, shutdown }
}

async fn run(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    gate_tx: watch::Sender<TransportGate>,
    shutdown: Arc<Notify>,
) {
    let addressing =
        ExternalAddressing::new(NOW_PLAYING_SCHEME, NOW_PLAYING_VALUE);

    // 1. Resolve canonical id with bounded backoff. The
    //    playback plugin's announce may not have landed yet;
    //    keep retrying until the subject exists or shutdown.
    let canonical_id = loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "transport-gate subscriber: shutdown received before \
                     now_playing canonical id resolved"
                );
                return;
            }
            resolved = querier.resolve_addressing(addressing.clone()) => {
                match resolved {
                    Ok(Some(id)) => break id,
                    Ok(None) => {
                        // Subject not yet announced. Sleep + retry.
                        tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            "transport-gate subscriber: resolve_addressing \
                             for now_playing returned error; retrying"
                        );
                        tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
                    }
                }
            }
        }
    };

    tracing::info!(
        plugin = PLUGIN_NAME,
        canonical_id = %canonical_id,
        "transport-gate subscriber: now_playing canonical id resolved"
    );

    // 2. Subscribe to the stream BEFORE reading current state
    //    (no-race-window pattern per SubjectStateSubscriber
    //    docs).
    let mut stream =
        match subscriber.subscribe_subject(canonical_id.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    canonical_id = %canonical_id,
                    error = %e,
                    "transport-gate subscriber: subscribe_subject failed; \
                     task exiting — emission stays gated NotPlaying for \
                     this load"
                );
                return;
            }
        };

    // 3. Seed the gate from the current state. The subject may
    //    already be in a Playing state (operator started playback
    //    before the terminus admitted) — without this step the
    //    gate would stay NotPlaying until the next transition.
    match subscriber.current_state(canonical_id.clone()).await {
        Ok(Some(state)) => {
            let gate = parse_from_state(&state);
            send_gate(&gate_tx, gate);
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                seeded = ?gate,
                "transport-gate subscriber: gate seeded from current_state"
            );
        }
        Ok(None) => {
            // Subject exists but no state has ever been
            // contributed. Stay NotPlaying (the watch sender's
            // initial value); the first stream update will
            // open the gate when playback starts.
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                "transport-gate subscriber: current_state empty; gate \
                 remains NotPlaying until first stream update"
            );
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                error = %e,
                "transport-gate subscriber: current_state read errored; \
                 gate remains NotPlaying until first stream update"
            );
        }
    }

    // 4. Stream loop. Project each state update onto the gate;
    //    push only when the gate value actually changes (cheap
    //    no-op subscribers shouldn't re-wake on every
    //    same-state update).
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "transport-gate subscriber: shutdown received; \
                     task exiting"
                );
                return;
            }
            next = stream.recv() => {
                match next {
                    Ok(update) => {
                        // Cleared state (`Option::None` payload)
                        // collapses to NotPlaying — the playback
                        // plugin retracted its state and we can no
                        // longer claim it's playing.
                        let gate = match update.state.as_ref() {
                            Some(s) => parse_from_state(s),
                            None => TransportGate::NotPlaying,
                        };
                        send_gate(&gate_tx, gate);
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        // Broadcast buffer overflowed. Resync by
                        // re-reading current state from the querier;
                        // the next stream recv rejoins at the live
                        // frame.
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped,
                            "transport-gate subscriber: stream lagged; \
                             resyncing from querier"
                        );
                        if let Ok(Some(state)) =
                            subscriber.current_state(canonical_id.clone()).await
                        {
                            send_gate(&gate_tx, parse_from_state(&state));
                        } else {
                            send_gate(&gate_tx, TransportGate::NotPlaying);
                        }
                    }
                    Err(SubjectStateStreamError::Closed) => {
                        // Stream closed: framework dropped the
                        // sender (plugin unload or shutdown). The
                        // capture loop's gate stays at its last
                        // value; exit cleanly.
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            "transport-gate subscriber: stream closed; \
                             task exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Send the gate value only if it actually changed. `watch::Sender`
/// dedups identical sends but still wakes any task currently
/// awaiting on the receiver's change notify; skipping the send
/// at the source keeps the receive side completely quiet on
/// no-op updates.
fn send_gate(gate_tx: &watch::Sender<TransportGate>, next: TransportGate) {
    let current = *gate_tx.borrow();
    if current != next {
        // `send` errors only if there are no live receivers;
        // the capture loop holds the only receiver and lives
        // for the load's duration, so this should never fire.
        // If it does, log + continue — the gate value the
        // capture loop reads is just stale.
        if let Err(_e) = gate_tx.send(next) {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "transport-gate subscriber: watch::send had no receivers \
                 (capture loop dropped); next transition will not propagate"
            );
        }
    }
}
