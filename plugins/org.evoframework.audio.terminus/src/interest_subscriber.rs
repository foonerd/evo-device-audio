// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Subscription-interest subscriber.
//!
//! Watches the framework-owned `system_subscription_interest`
//! subject and pushes the current subscriber count for
//! `audio_playback_spectrum_frame` to the capture loop's
//! produce-iff-consumed gate. Every subscribe / unsubscribe
//! transition observed on the framework's projection-ws
//! surface for that subject_type fires a state update carrying
//! `{ subject_type, count, at_ms }`; the subscriber filters
//! for its own subject_type and forwards the new count via
//! a `watch::Sender<u32>` the capture loop borrows in its
//! outer gate.
//!
//! Same lifecycle shape as `local_role_subscriber`:
//! 1. Resolve the framework-owned system subject's canonical
//!    id with bounded backoff (races the framework's boot-time
//!    announce).
//! 2. Subscribe to the stream BEFORE reading current state.
//! 3. Seed the count from current state (may be null if no
//!    transition has fired yet — leave the gate at its default
//!    of zero).
//! 4. Loop on stream updates; filter for
//!    `audio_playback_spectrum_frame`; push each count
//!    transition via the watch sender.
//! 5. Exit cleanly on the shutdown notify.

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectQuerier, SubjectStateStreamError,
    SubjectStateSubscriber,
};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Framework-owned subject addressing — per-subject-type.
/// The framework announces one subject per observed
/// subject_type at addressing
/// `evo.system:subscription_interest.<subject_type>`; each
/// subject's state carries `{ subject_type, count, at_ms }`
/// for that type only. Per-type isolation eliminates the
/// last-write-wins race a single-subject shape would have.
const INTEREST_SCHEME: &str = "evo.system";

/// Subject-type this subscriber cares about.
const OBSERVED_SUBJECT_TYPE: &str = "audio_playback_spectrum_frame";

/// Addressing value for this subject_type's interest subject —
/// framework's `publish_interest_state` uses the same
/// `subscription_interest.<subject_type>` template.
fn interest_addressing_value() -> String {
    format!("subscription_interest.{OBSERVED_SUBJECT_TYPE}")
}

/// Backoff for the canonical-id resolve retry loop.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Handle returned from `spawn`.
pub struct SubscriberHandle {
    pub task: JoinHandle<()>,
    pub shutdown: Arc<Notify>,
}

/// Spawn the interest subscriber. Returns a `SubscriberHandle`
/// the plugin retains for the load's lifetime.
pub fn spawn(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    interest_tx: watch::Sender<u32>,
) -> SubscriberHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);

    let task = tokio::spawn(async move {
        run(subscriber, querier, interest_tx, task_shutdown).await;
    });

    SubscriberHandle { task, shutdown }
}

async fn run(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    interest_tx: watch::Sender<u32>,
    shutdown: Arc<Notify>,
) {
    let interest_value = interest_addressing_value();
    let addressing = ExternalAddressing::new(INTEREST_SCHEME, &interest_value);

    // 1. Resolve canonical id with bounded backoff. Framework
    //    announces this subject at boot, before any plugin is
    //    admitted — the resolve should succeed on the first
    //    attempt in production; the loop covers the odd race.
    let canonical_id = loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "interest subscriber: shutdown received before \
                     canonical id resolved"
                );
                return;
            }
            resolved = querier.resolve_addressing(addressing.clone()) => {
                match resolved {
                    Ok(Some(id)) => break id,
                    Ok(None) => {
                        tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            error = %e,
                            "interest subscriber: resolve_addressing \
                             returned error; retrying"
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
        "interest subscriber: canonical id resolved"
    );

    // 2. Subscribe BEFORE reading current state.
    let mut stream =
        match subscriber.subscribe_subject(canonical_id.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    canonical_id = %canonical_id,
                    error = %e,
                    "interest subscriber: subscribe_subject failed; \
                     interest gate stays at 0 for this load"
                );
                return;
            }
        };

    // 3. Seed from current state — may be null if no transition
    //    has fired yet. Leave the sender at its initial value
    //    (0) in that case; the first transition fires normally.
    match subscriber.current_state(canonical_id.clone()).await {
        Ok(Some(state)) => {
            if let Some(count) = parse_count_for(&state, OBSERVED_SUBJECT_TYPE)
            {
                send_count(&interest_tx, count);
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    canonical_id = %canonical_id,
                    seeded = count,
                    "interest subscriber: gate seeded from current_state"
                );
            }
        }
        Ok(None) => {
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                "interest subscriber: current_state empty; gate stays \
                 at 0 until first stream update"
            );
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                error = %e,
                "interest subscriber: current_state read errored; gate \
                 stays at 0 until first stream update"
            );
        }
    }

    // 4. Stream loop.
    //
    // Resync discipline on transient errors:
    //   - Lagged: broadcast dropped one or more updates; the
    //     framework's counter is authoritative. Read the
    //     current state from the querier and reseed the gate
    //     from that so a missed decrement doesn't strand the
    //     capture path in the wrong state (stuck ON after a
    //     dropped `count → 0`, stuck OFF after a dropped
    //     `count → 1`).
    //   - Closed: broadcast channel dropped (steward restart).
    //     Rebuild the subscription against the fresh registry
    //     and reseed from current state before returning to
    //     the loop; a plain exit would strand the gate at its
    //     last observed value forever.
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "interest subscriber: shutdown received; task exiting"
                );
                return;
            }
            next = stream.recv() => {
                match next {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            if let Some(count) = parse_count_for(
                                state, OBSERVED_SUBJECT_TYPE,
                            ) {
                                send_count(&interest_tx, count);
                            }
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped,
                            "interest subscriber: stream lagged; resyncing \
                             count from current_state to avoid stranding \
                             the gate at a stale value"
                        );
                        resync_from_current_state(
                            &subscriber,
                            &canonical_id,
                            &interest_tx,
                        )
                        .await;
                    }
                    Err(SubjectStateStreamError::Closed) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            "interest subscriber: stream closed; \
                             re-subscribing + resyncing count"
                        );
                        // Re-subscribe. If the re-subscribe
                        // fails, back off then retry — the
                        // steward may be mid-restart.
                        loop {
                            tokio::select! {
                                _ = shutdown.notified() => return,
                                res = subscriber
                                    .subscribe_subject(canonical_id.clone()) => {
                                    match res {
                                        Ok(s) => {
                                            stream = s;
                                            break;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                plugin = PLUGIN_NAME,
                                                error = %e,
                                                "interest subscriber: \
                                                 re-subscribe failed; \
                                                 retrying"
                                            );
                                            tokio::time::sleep(
                                                RESOLVE_RETRY_INTERVAL,
                                            )
                                            .await;
                                        }
                                    }
                                }
                            }
                        }
                        resync_from_current_state(
                            &subscriber,
                            &canonical_id,
                            &interest_tx,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

/// Read the authoritative current count from the framework's
/// registry (via the querier's current_state read on the
/// per-type interest subject) and push it to the gate. Used
/// on Lagged / Closed transitions to prevent the gate from
/// stranding at a stale value.
async fn resync_from_current_state(
    subscriber: &Arc<dyn SubjectStateSubscriber>,
    canonical_id: &str,
    interest_tx: &watch::Sender<u32>,
) {
    match subscriber.current_state(canonical_id.to_string()).await {
        Ok(Some(state)) => {
            if let Some(count) = parse_count_for(&state, OBSERVED_SUBJECT_TYPE)
            {
                send_count(interest_tx, count);
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    resynced = count,
                    "interest subscriber: gate resynced from current_state"
                );
            }
        }
        Ok(None) => {
            // No state on the subject yet means no transitions
            // have fired — count is authoritatively 0.
            send_count(interest_tx, 0);
            tracing::info!(
                plugin = PLUGIN_NAME,
                "interest subscriber: current_state absent; gate resynced to 0"
            );
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "interest subscriber: resync current_state read failed; \
                 gate stays at last observed value"
            );
        }
    }
}

/// Read the `count` field from a state payload IFF the
/// `subject_type` field matches the observed type. Returns
/// `None` for state that targets a different subject_type or is
/// malformed. Terminus filters here so it only reacts to its
/// own subject_type's transitions.
fn parse_count_for(
    state: &serde_json::Value,
    subject_type: &str,
) -> Option<u32> {
    let st = state.get("subject_type")?.as_str()?;
    if st != subject_type {
        return None;
    }
    let count = state.get("count")?.as_u64()?;
    Some(count as u32)
}

fn send_count(tx: &watch::Sender<u32>, count: u32) {
    // send() returns Err only when no receivers exist; the
    // capture loop always holds one for the load's lifetime.
    let _ = tx.send(count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_count_matches_observed_subject_type() {
        let state = serde_json::json!({
            "subject_type": OBSERVED_SUBJECT_TYPE,
            "count": 3,
            "at_ms": 12345,
        });
        assert_eq!(parse_count_for(&state, OBSERVED_SUBJECT_TYPE), Some(3));
    }

    #[test]
    fn parse_count_ignores_other_subject_type() {
        let state = serde_json::json!({
            "subject_type": "audio_playback_now_playing",
            "count": 3,
        });
        assert_eq!(parse_count_for(&state, OBSERVED_SUBJECT_TYPE), None);
    }

    #[test]
    fn parse_count_null_state_returns_none() {
        let state = serde_json::json!(null);
        assert_eq!(parse_count_for(&state, OBSERVED_SUBJECT_TYPE), None);
    }

    #[test]
    fn parse_count_missing_field_returns_none() {
        let state = serde_json::json!({
            "subject_type": OBSERVED_SUBJECT_TYPE,
        });
        assert_eq!(parse_count_for(&state, OBSERVED_SUBJECT_TYPE), None);
    }
}
