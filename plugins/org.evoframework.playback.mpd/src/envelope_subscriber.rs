//! Mixer-transition envelope subscriber for playback.mpd.
//!
//! The audio.options-shape orchestrator publishes the
//! `envelope_requested` subject during its safety-envelope
//! steps (pre-mute at step 2, unmute at step 7). This module
//! subscribes to that subject, dispatches a matching
//! `PlaybackCommand::Pause(true / false)` to the active
//! supervisor (if any), then publishes the
//! `envelope_observed` subject's state with the matching
//! generation so the orchestrator can advance its
//! state-machine.
//!
//! Failure semantics:
//!
//! * Pause dispatch failure → the subscriber does NOT
//!   publish a matching observed; the orchestrator's await
//!   times out and routes to its rollback chain. The
//!   chain's actual state is what the failure-mode warn-log
//!   captured.
//! * No active supervisor (no source playback in flight) →
//!   the chain is already in the requested state (silent,
//!   nothing flowing to mute / unmute). The subscriber acks
//!   immediately with the matching observed_state.
//! * Subscribe / resolve failure → the subscriber retries
//!   with bounded backoff until shutdown. The orchestrator's
//!   own await may time out before the subscriber recovers;
//!   that surfaces as a rollback on the orchestrator side.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use evo_device_audio_shared::transition_envelope::{
    parse_envelope_requested, EnvelopeObserved, EnvelopeRequested,
    EnvelopeState, ENVELOPE_OBSERVED_SUBJECT_TYPE, ENVELOPE_OBSERVED_VALUE,
    ENVELOPE_PAYLOAD_VERSION, ENVELOPE_REQUESTED_VALUE, ENVELOPE_SCHEME,
};
use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer, SubjectQuerier,
    SubjectStateStreamError, SubjectStateSubscriber,
};
use tokio::sync::{Mutex, Notify};

use crate::playback_supervisor::{PlaybackCommand, SupervisorCommandSender};

/// Handle returned by [`spawn`]. The plugin's `unload`
/// signals shutdown + awaits task completion.
pub(crate) struct EnvelopeSubscriberHandle {
    pub(crate) task: tokio::task::JoinHandle<()>,
    pub(crate) shutdown: Arc<Notify>,
}

impl EnvelopeSubscriberHandle {
    /// Signal shutdown and wait for the task to finish.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Polling interval for resolving `envelope_requested`'s
/// canonical id on startup. The orchestrator's `announce`
/// may not have landed by the time this subscriber spawns,
/// so we retry with this cadence until shutdown.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Spawn the envelope subscriber task. The plugin calls this
/// from `load()` after capturing the load context's
/// subject_announcer / subject_state_subscriber /
/// subject_querier handles. Returns the handle the plugin
/// drops on `unload`.
pub(crate) fn spawn(
    plugin_name: &'static str,
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    announcer: Arc<dyn SubjectAnnouncer>,
    active_command_sender: Arc<Mutex<Option<SupervisorCommandSender>>>,
) -> EnvelopeSubscriberHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let observed_addressing = ExternalAddressing {
        scheme: ENVELOPE_SCHEME.to_string(),
        value: ENVELOPE_OBSERVED_VALUE.to_string(),
    };
    let requested_addressing = ExternalAddressing {
        scheme: ENVELOPE_SCHEME.to_string(),
        value: ENVELOPE_REQUESTED_VALUE.to_string(),
    };

    let task = tokio::spawn(async move {
        tracing::info!(
            plugin = plugin_name,
            "envelope subscriber task entered; announcing envelope_observed"
        );
        // Announce envelope_observed so the orchestrator can
        // resolve its canonical id. The seed state's
        // generation = 0 lets the orchestrator know "warden
        // is online but hasn't acked any specific request
        // yet"; the orchestrator skips matching on
        // generation 0 (it never publishes generation 0
        // either — its first published request is
        // generation 1).
        let initial_observed = serde_json::json!({
            "v": ENVELOPE_PAYLOAD_VERSION,
            "generation": 0_u64,
            "observed_state": "unmuted",
            "observed_at_ms": 0_u64,
        });
        let announcement = SubjectAnnouncement {
            subject_type: ENVELOPE_OBSERVED_SUBJECT_TYPE.to_string(),
            addressings: vec![observed_addressing.clone()],
            claims: Vec::new(),
            state: initial_observed,
            announced_at: SystemTime::now(),
        };
        match announcer.announce(announcement).await {
            Ok(()) => tracing::info!(
                plugin = plugin_name,
                "envelope_observed announce ok"
            ),
            Err(e) => tracing::warn!(
                plugin = plugin_name,
                error = %e,
                "announce envelope_observed failed"
            ),
        }

        // Resolve envelope_requested with bounded backoff
        // until shutdown.
        let canonical_id = loop {
            tokio::select! {
                _ = task_shutdown.notified() => {
                    tracing::debug!(
                        plugin = plugin_name,
                        "envelope subscriber: shutdown received before resolve"
                    );
                    return;
                }
                resolved = querier.resolve_addressing(requested_addressing.clone()) => {
                    match resolved {
                        Ok(Some(id)) => break id,
                        Ok(None) => {
                            tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                plugin = plugin_name,
                                error = %e,
                                "resolve envelope_requested failed; retrying"
                            );
                            tokio::time::sleep(RESOLVE_RETRY_INTERVAL).await;
                        }
                    }
                }
            }
        };

        let mut stream = match subscriber
            .subscribe_subject(canonical_id.clone())
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    plugin = plugin_name,
                    canonical_id = %canonical_id,
                    error = %e,
                    "subscribe envelope_requested failed; subscriber task exiting"
                );
                return;
            }
        };

        tracing::info!(
            plugin = plugin_name,
            canonical_id = %canonical_id,
            "envelope subscriber task running; awaiting orchestrator requests"
        );

        loop {
            tokio::select! {
                _ = task_shutdown.notified() => {
                    tracing::debug!(
                        plugin = plugin_name,
                        "envelope subscriber: shutdown received"
                    );
                    return;
                }
                update = stream.recv() => {
                    match update {
                        Ok(state_update) => {
                            let Some(state) = state_update.state.as_ref() else {
                                continue;
                            };
                            let Some(req) = parse_envelope_requested(state) else {
                                continue;
                            };
                            // Skip the orchestrator's seed
                            // announce (generation = 0); the
                            // orchestrator never publishes
                            // generation 0 — only the
                            // monotonic series starting at 1
                            // counts.
                            if req.generation == 0 {
                                continue;
                            }
                            handle_envelope_request(
                                plugin_name,
                                &active_command_sender,
                                &announcer,
                                &observed_addressing,
                                req,
                            )
                            .await;
                        }
                        Err(SubjectStateStreamError::Lagged { dropped }) => {
                            tracing::warn!(
                                plugin = plugin_name,
                                dropped,
                                "envelope subscriber stream lagged"
                            );
                        }
                        Err(SubjectStateStreamError::Closed) => {
                            tracing::debug!(
                                plugin = plugin_name,
                                "envelope subscriber stream closed"
                            );
                            return;
                        }
                    }
                }
            }
        }
    });

    EnvelopeSubscriberHandle { task, shutdown }
}

/// Handle a single envelope request: dispatch Pause to the
/// active supervisor (if any), then publish the matching
/// observed state. On dispatch failure, publish nothing —
/// the orchestrator's await times out and routes to its
/// rollback chain.
async fn handle_envelope_request(
    plugin_name: &'static str,
    active_command_sender: &Arc<Mutex<Option<SupervisorCommandSender>>>,
    announcer: &Arc<dyn SubjectAnnouncer>,
    observed_addressing: &ExternalAddressing,
    req: EnvelopeRequested,
) {
    let pause_arg = matches!(req.requested_state, EnvelopeState::Muted);
    // Snapshot the sender + release the lock before the
    // async dispatch so concurrent custody-lifecycle
    // updates aren't blocked by an in-flight pause.
    let sender_snapshot = active_command_sender.lock().await.clone();

    match sender_snapshot {
        Some(sender) => {
            // An active custody is in flight — dispatch
            // Pause and verify it landed before acking.
            match sender.command(PlaybackCommand::Pause(pause_arg)).await {
                Ok(()) => {
                    publish_envelope_observed(
                        plugin_name,
                        announcer,
                        observed_addressing,
                        req,
                    )
                    .await;
                }
                Err(e) => {
                    // Dispatch failed → the chain did NOT
                    // transition. Publish nothing; the
                    // orchestrator's await times out and
                    // rolls back. Operator-readable warn so
                    // the failure is auditable.
                    tracing::warn!(
                        plugin = plugin_name,
                        error = ?e,
                        requested_state = ?req.requested_state,
                        generation = req.generation,
                        "envelope-driven Pause command failed; no envelope_observed ack \
                         published — orchestrator will time out and route to rollback"
                    );
                }
            }
        }
        None => {
            // No active custody — the chain is already in
            // the requested state (silent, nothing
            // flowing to mute / unmute). Ack immediately;
            // the orchestrator's await advances.
            publish_envelope_observed(
                plugin_name,
                announcer,
                observed_addressing,
                req,
            )
            .await;
        }
    }
}

async fn publish_envelope_observed(
    plugin_name: &'static str,
    announcer: &Arc<dyn SubjectAnnouncer>,
    observed_addressing: &ExternalAddressing,
    req: EnvelopeRequested,
) {
    let observed = EnvelopeObserved {
        v: ENVELOPE_PAYLOAD_VERSION,
        generation: req.generation,
        observed_state: req.requested_state,
        observed_at_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    let payload = match serde_json::to_value(&observed) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                plugin = plugin_name,
                error = %e,
                "serialise envelope_observed failed"
            );
            return;
        }
    };
    if let Err(e) = announcer
        .update_state(observed_addressing.clone(), payload)
        .await
    {
        tracing::warn!(
            plugin = plugin_name,
            error = %e,
            "update envelope_observed failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use evo_plugin_sdk::contract::{
        AliasRecord, ReportError, SubjectQueryResult, SubjectStateStream,
        SubjectStateUpdate,
    };
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    // ----- test stubs -----

    #[derive(Default)]
    struct StubQuerier {
        map: StdMutex<HashMap<String, String>>,
    }

    impl StubQuerier {
        fn register(&self, addressing: &ExternalAddressing, id: &str) {
            self.map
                .lock()
                .unwrap()
                .insert(addressing.value.clone(), id.to_string());
        }
    }

    impl evo_plugin_sdk::contract::SubjectQuerier for StubQuerier {
        fn resolve_addressing<'a>(
            &'a self,
            addressing: ExternalAddressing,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<String>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(self.map.lock().unwrap().get(&addressing.value).cloned())
            })
        }

        fn describe_alias<'a>(
            &'a self,
            _id: String,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<AliasRecord>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(None) })
        }

        fn describe_subject_with_aliases<'a>(
            &'a self,
            _id: String,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<SubjectQueryResult, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(SubjectQueryResult::NotFound) })
        }
    }

    #[derive(Default, Clone)]
    struct StubStateSubscriber {
        inner: Arc<StdMutex<StubSubscriberInner>>,
    }

    #[derive(Default)]
    struct StubSubscriberInner {
        senders:
            HashMap<String, tokio::sync::broadcast::Sender<SubjectStateUpdate>>,
        latest: HashMap<String, serde_json::Value>,
    }

    impl StubStateSubscriber {
        fn publish(&self, canonical_id: &str, state: serde_json::Value) {
            let mut inner = self.inner.lock().unwrap();
            inner.latest.insert(canonical_id.to_string(), state.clone());
            let sender = inner
                .senders
                .entry(canonical_id.to_string())
                .or_insert_with(|| {
                    let (tx, _rx) = tokio::sync::broadcast::channel(16);
                    tx
                });
            let _ = sender.send(SubjectStateUpdate {
                canonical_id: canonical_id.to_string(),
                subject_type: "stub".to_string(),
                state: Some(state),
                modified_at_ms: 0,
            });
        }
    }

    impl evo_plugin_sdk::contract::SubjectStateSubscriber for StubStateSubscriber {
        fn subscribe_subject<'a>(
            &'a self,
            canonical_id: String,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<SubjectStateStream, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                let mut guard = inner.lock().unwrap();
                let sender = guard
                    .senders
                    .entry(canonical_id.clone())
                    .or_insert_with(|| {
                        let (tx, _rx) = tokio::sync::broadcast::channel(16);
                        tx
                    });
                let receiver = sender.subscribe();
                Ok(SubjectStateStream::new(receiver, canonical_id))
            })
        }

        fn current_state<'a>(
            &'a self,
            canonical_id: String,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Option<serde_json::Value>, ReportError>,
                    > + Send
                    + 'a,
            >,
        > {
            let inner = Arc::clone(&self.inner);
            Box::pin(async move {
                Ok(inner.lock().unwrap().latest.get(&canonical_id).cloned())
            })
        }
    }

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    struct CapturedUpdate {
        addressing: ExternalAddressing,
        state: serde_json::Value,
    }

    #[derive(Default)]
    struct CapturingAnnouncer {
        announces: StdMutex<Vec<SubjectAnnouncement>>,
        updates: StdMutex<Vec<CapturedUpdate>>,
    }

    impl CapturingAnnouncer {
        fn observed_states(&self) -> Vec<EnvelopeObserved> {
            self.updates
                .lock()
                .unwrap()
                .iter()
                .filter(|u| {
                    u.addressing.scheme == ENVELOPE_SCHEME
                        && u.addressing.value == ENVELOPE_OBSERVED_VALUE
                })
                .filter_map(|u| {
                    serde_json::from_value::<EnvelopeObserved>(u.state.clone())
                        .ok()
                })
                // Drop the initial announce seed (generation 0)
                // — only the orchestrator-driven acks count.
                .filter(|o| o.generation > 0)
                .collect()
        }
    }

    impl evo_plugin_sdk::contract::SubjectAnnouncer for CapturingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.announces.lock().unwrap().push(announcement);
                Ok(())
            })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<(), ReportError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.updates
                    .lock()
                    .unwrap()
                    .push(CapturedUpdate { addressing, state });
                Ok(())
            })
        }
    }

    fn publish_request(
        subscriber: &StubStateSubscriber,
        canonical_id: &str,
        generation: u64,
        state: EnvelopeState,
    ) {
        let payload = serde_json::json!({
            "v": ENVELOPE_PAYLOAD_VERSION,
            "generation": generation,
            "requested_state": match state {
                EnvelopeState::Muted => "muted",
                EnvelopeState::Unmuted => "unmuted",
            },
            "requested_at_ms": 0_u64,
        });
        subscriber.publish(canonical_id, payload);
    }

    async fn wait_for_observed_count(
        announcer: &Arc<CapturingAnnouncer>,
        target: usize,
        timeout: Duration,
    ) -> Vec<EnvelopeObserved> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let observed = announcer.observed_states();
            if observed.len() >= target {
                return observed;
            }
            if std::time::Instant::now() > deadline {
                return observed;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    const REQUESTED_CID: &str = "stub-envelope-requested-cid";

    #[tokio::test]
    async fn no_custody_publishes_observed_matching_immediately() {
        // I3 single-authority edge case: when no custody is
        // active, the chain has no audio flowing. The
        // subscriber publishes envelope_observed matching
        // the request so the orchestrator's await advances —
        // there's nothing to pause / resume.
        let querier = Arc::new(StubQuerier::default());
        querier.register(
            &ExternalAddressing {
                scheme: ENVELOPE_SCHEME.to_string(),
                value: ENVELOPE_REQUESTED_VALUE.to_string(),
            },
            REQUESTED_CID,
        );
        let subscriber = StubStateSubscriber::default();
        let announcer = Arc::new(CapturingAnnouncer::default());
        let active = Arc::new(Mutex::new(None));

        let handle = spawn(
            "test",
            Arc::new(subscriber.clone()) as Arc<dyn SubjectStateSubscriber>,
            Arc::clone(&querier) as Arc<dyn SubjectQuerier>,
            Arc::clone(&announcer) as Arc<dyn SubjectAnnouncer>,
            active,
        );

        // Wait briefly for the subscriber's resolve +
        // subscribe to complete before publishing.
        tokio::time::sleep(Duration::from_millis(50)).await;
        publish_request(&subscriber, REQUESTED_CID, 1, EnvelopeState::Muted);
        publish_request(&subscriber, REQUESTED_CID, 2, EnvelopeState::Unmuted);

        let observed =
            wait_for_observed_count(&announcer, 2, Duration::from_millis(500))
                .await;

        handle.stop().await;

        assert_eq!(observed.len(), 2, "got {observed:?}");
        assert_eq!(observed[0].generation, 1);
        assert_eq!(observed[0].observed_state, EnvelopeState::Muted);
        assert_eq!(observed[1].generation, 2);
        assert_eq!(observed[1].observed_state, EnvelopeState::Unmuted);
    }

    #[tokio::test]
    async fn skips_generation_zero_seed_announcement() {
        // The orchestrator's announce seed publishes
        // generation = 0 (idle). The subscriber MUST NOT
        // dispatch / ack on that — only generations >= 1
        // count.
        let querier = Arc::new(StubQuerier::default());
        querier.register(
            &ExternalAddressing {
                scheme: ENVELOPE_SCHEME.to_string(),
                value: ENVELOPE_REQUESTED_VALUE.to_string(),
            },
            REQUESTED_CID,
        );
        let subscriber = StubStateSubscriber::default();
        let announcer = Arc::new(CapturingAnnouncer::default());
        let active = Arc::new(Mutex::new(None));

        let handle = spawn(
            "test",
            Arc::new(subscriber.clone()) as Arc<dyn SubjectStateSubscriber>,
            Arc::clone(&querier) as Arc<dyn SubjectQuerier>,
            Arc::clone(&announcer) as Arc<dyn SubjectAnnouncer>,
            active,
        );

        tokio::time::sleep(Duration::from_millis(50)).await;
        // Publish generation 0 (the orchestrator's seed).
        publish_request(&subscriber, REQUESTED_CID, 0, EnvelopeState::Unmuted);
        // Then a real generation.
        publish_request(&subscriber, REQUESTED_CID, 1, EnvelopeState::Muted);

        let observed =
            wait_for_observed_count(&announcer, 1, Duration::from_millis(500))
                .await;

        handle.stop().await;

        // Only the generation-1 ack — the seed was skipped.
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].generation, 1);
    }
}
