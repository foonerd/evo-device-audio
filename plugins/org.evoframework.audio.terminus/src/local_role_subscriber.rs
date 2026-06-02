//! Subscriber loop that watches the singleton
//! `audio_multiroom_local_role` subject and pushes `LocalRole`
//! updates to the capture loop's leader-of-active-group gate.
//!
//! Lifecycle (mirrors `now_playing_subscriber`):
//! 1. Resolve the local-role canonical id with bounded
//!    backoff. The multiroom plugin's announce may not have
//!    landed by the time this subscriber spawns; we retry
//!    until shutdown.
//! 2. Subscribe to the stream BEFORE reading current state —
//!    no-race-window pattern per the SDK docs.
//! 3. Seed the gate from the current state.
//! 4. Loop on stream updates; project each into a `LocalRole`
//!    and push via the watch sender.
//! 5. Exit cleanly on the shutdown notify.
//!
//! Failure semantics: the multiroom plugin may not be admitted
//! on this node at all (most reference rigs don't run
//! multiroom in production). In that case `resolve_addressing`
//! returns `None` and the retry loop spins forever — the gate
//! stays at its watch-channel initial value (`LocalRole::Auto`,
//! which opens the gate). Spectrum emission continues as a
//! solo device for the life of the load. This is the
//! conservative-permissive default and matches the operational
//! majority case.

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectQuerier, SubjectStateStreamError,
    SubjectStateSubscriber,
};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

use crate::local_role::{parse_from_state, LocalRole};

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Canonical local-role subject scheme. Matches the
/// `multiroom.evo-native` plugin's `LOCAL_ROLE_SCHEME` literal;
/// the two plugins agree on the wire-side addressing without
/// cross-plugin coupling.
const LOCAL_ROLE_SCHEME: &str = "evo.audio.multiroom.local_role";

/// Singleton addressing value. Fixed string so any plugin on
/// the same node can subscribe without needing to learn the
/// local device id.
const LOCAL_ROLE_VALUE: &str = "local";

/// Backoff for the canonical-id resolve retry loop.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Handle returned from `spawn`. Dropping the handle (or
/// notifying via the cloned shutdown) terminates the subscriber
/// task at the next stream-or-shutdown branch.
pub struct SubscriberHandle {
    pub task: JoinHandle<()>,
    pub shutdown: Arc<Notify>,
}

/// Spawn the local-role subscriber. Returns a `SubscriberHandle`
/// the plugin retains for the load's lifetime.
pub fn spawn(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    gate_tx: watch::Sender<LocalRole>,
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
    gate_tx: watch::Sender<LocalRole>,
    shutdown: Arc<Notify>,
) {
    let addressing =
        ExternalAddressing::new(LOCAL_ROLE_SCHEME, LOCAL_ROLE_VALUE);

    // 1. Resolve canonical id with bounded backoff. The
    //    multi-room plugin's announce may not have landed yet,
    //    or the multi-room plugin may not be admitted on this
    //    node at all (in which case the loop spins until
    //    shutdown and the gate stays at LocalRole::Auto — the
    //    permissive solo-device default).
    let canonical_id = loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "local-role subscriber: shutdown received before \
                     canonical id resolved (multiroom plugin likely \
                     not admitted on this node)"
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
                            "local-role subscriber: resolve_addressing \
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
        "local-role subscriber: canonical id resolved"
    );

    // 2. Subscribe to the stream BEFORE reading current state
    //    (no-race-window).
    let mut stream =
        match subscriber.subscribe_subject(canonical_id.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    canonical_id = %canonical_id,
                    error = %e,
                    "local-role subscriber: subscribe_subject failed; \
                     task exiting — leader gate stays at Auto for this \
                     load"
                );
                return;
            }
        };

    // 3. Seed the gate from the current state. The multi-room
    //    plugin announces with a seeded state on load, so a
    //    well-formed payload should be present here unless the
    //    announcement raced or the state was cleared.
    match subscriber.current_state(canonical_id.clone()).await {
        Ok(Some(state)) => {
            let role = parse_from_state(&state);
            send_role(&gate_tx, role);
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                seeded = ?role,
                "local-role subscriber: gate seeded from current_state"
            );
        }
        Ok(None) => {
            tracing::info!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                "local-role subscriber: current_state empty; gate stays \
                 at Auto until first stream update"
            );
        }
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                canonical_id = %canonical_id,
                error = %e,
                "local-role subscriber: current_state read errored; gate \
                 stays at Auto until first stream update"
            );
        }
    }

    // 4. Stream loop.
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "local-role subscriber: shutdown received; task exiting"
                );
                return;
            }
            next = stream.recv() => {
                match next {
                    Ok(update) => {
                        let role = match update.state.as_ref() {
                            Some(s) => parse_from_state(s),
                            None => LocalRole::Auto,
                        };
                        send_role(&gate_tx, role);
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped,
                            "local-role subscriber: stream lagged; resyncing \
                             from querier"
                        );
                        if let Ok(Some(state)) =
                            subscriber.current_state(canonical_id.clone()).await
                        {
                            send_role(&gate_tx, parse_from_state(&state));
                        } else {
                            send_role(&gate_tx, LocalRole::Auto);
                        }
                    }
                    Err(SubjectStateStreamError::Closed) => {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            "local-role subscriber: stream closed; task exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Send the gate value only if it changed. `watch::Sender::send`
/// dedups identical values for the receive side; skipping the
/// send at the source keeps the receiver quiet on no-op updates.
fn send_role(gate_tx: &watch::Sender<LocalRole>, next: LocalRole) {
    let current = *gate_tx.borrow();
    if current != next {
        if let Err(_e) = gate_tx.send(next) {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "local-role subscriber: watch::send had no receivers \
                 (capture loop dropped); next transition will not propagate"
            );
        }
    }
}
