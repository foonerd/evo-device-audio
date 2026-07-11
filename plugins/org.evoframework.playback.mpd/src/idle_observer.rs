// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Idle-subprotocol observer.
//!
//! Subscribes to MPD's `idle` subprotocol on a dedicated long-
//! lived connection and dispatches per-subsystem rehydration of
//! the four shelf subjects when MPD-side state changes outside
//! this plugin's verbs (operator using another mpc client, a
//! recovery shell, a multiroom peer mutating MPD on this host,
//! etc.).
//!
//! The framework's `subject_states` mirror is a cache of MPD's
//! durable truth. A cache that ignores its upstream is broken;
//! the reactive contract this observer enforces is "subject
//! mirror tracks MPD, not just this plugin's writes."
//!
//! # Subsystems observed
//!
//! - [`IdleSubsystem::Playlist`] — the live queue changed.
//!   Dispatches `queue::publish_queue`.
//! - [`IdleSubsystem::StoredPlaylist`] — a stored playlist
//!   changed. Dispatches `playlist::publish_index` for the
//!   index AND `favourites::refresh_favourites` (the
//!   favourites file is itself a stored playlist named
//!   `__favourites__`; a stored-playlist event may have
//!   touched it).
//! - [`IdleSubsystem::Database`] / [`IdleSubsystem::Update`] —
//!   the music database was scanned / updated. Dispatches
//!   `library::rehydrate_from_mpd` so the library counts +
//!   last-scan timestamp track MPD's `stats`.
//! - [`IdleSubsystem::Player`] — playback state / position
//!   changed. Dispatches `queue::publish_queue` so the queue
//!   envelope's `current_position` reflects MPD's truth.
//!
//! Other subsystems (mixer, output, options, sticker, etc.)
//! are observed but ignored — the plugin's existing supervisor
//! already handles transport-state changes via its own idle
//! connection on the playback custody path.
//!
//! # Connection isolation
//!
//! MPD's protocol blocks the connection for the lifetime of an
//! idle call. The observer owns one dedicated connection used
//! ONLY for idle; per-event publish/refresh dispatches open
//! short-lived command connections (the same pattern the shelf
//! verbs already use). This keeps the observer's idle wait from
//! contending with the supervisor's command connection or with
//! the sticker reconciler's bulk-write connection.
//!
//! # Failure mode
//!
//! Reactive sync is idempotent. A failed publish on one event
//! leaves the mirror at its previous state; the next event
//! re-runs the publish and the mirror catches up. MPD
//! disconnection triggers reconnect with a fixed backoff. The
//! observer never panics: every error path either retries or
//! exits cleanly via the shutdown notifier.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::favourites::{self, FavouritesContext};
use crate::library::{self, LibraryContext};
use crate::mpd::{
    ConnectTimeouts, IdleSubsystem, MpdConnection, MpdEndpoint, MpdError,
};
use crate::playlist::{self, PlaylistContext};
use crate::queue::{self, QueueContext};

/// Per-call idle budget. MPD's `idle` subprotocol is a long-
/// poll: the server blocks indefinitely until one of the
/// subscribed subsystems fires, suppressing the
/// `connection_timeout` MPD would otherwise apply to a quiet
/// command connection. The observer races the idle read
/// against the shutdown notifier via `tokio::select!`, so
/// shutdown latency is bounded by tokio's cancellation, not
/// by this budget.
///
/// The budget is therefore deliberately very long (one day).
/// A shorter budget would create a reconnect-storm pattern:
/// when the local read times out before MPD has a chance to
/// send anything, the observer's next `idle` re-uses a
/// connection MPD still considers in-flight; MPD treats the
/// new command as a protocol violation and closes the
/// connection, surfacing as `transport: connection closed by
/// MPD` in the journal. The framework-side fix is to keep the
/// idle read pending until MPD responds or shutdown fires;
/// network-level disconnects continue to surface via the
/// idle dispatcher's error arm and trigger reconnect there.
const IDLE_BUDGET: Duration = Duration::from_secs(86_400);

/// Backoff between reconnect attempts when the dedicated idle
/// connection is dropped or fails. The connection is permanent
/// for the observer's lifetime; a transient failure must not
/// stall reactive sync indefinitely.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Subsystems the observer subscribes to. Other subsystems are
/// ignored — the supervisor's own idle connection on the
/// playback custody path covers transport / mixer / output.
const OBSERVED: &[IdleSubsystem] = &[
    IdleSubsystem::Database,
    IdleSubsystem::Update,
    IdleSubsystem::StoredPlaylist,
    IdleSubsystem::Playlist,
    IdleSubsystem::Player,
];

/// Handle the plugin retains for the observer's lifetime.
pub(crate) struct IdleObserverHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl IdleObserverHandle {
    /// Signal shutdown + await task completion.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Spawn the observer task. Holds Arc-clones of each shelf
/// context (the contexts are themselves cheap to clone — they
/// hold Arcs to their backing state). Returns the handle the
/// plugin retains.
pub(crate) fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    queue: QueueContext,
    playlist: PlaylistContext,
    favourites: FavouritesContext,
    library: LibraryContext,
) -> IdleObserverHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        run(
            endpoint,
            timeouts,
            queue,
            playlist,
            favourites,
            library,
            task_shutdown,
        )
        .await;
    });
    IdleObserverHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    queue: QueueContext,
    playlist: PlaylistContext,
    favourites: FavouritesContext,
    library: LibraryContext,
    shutdown: Arc<Notify>,
) {
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        endpoint = %endpoint,
        "idle observer task started"
    );
    let mut conn: Option<MpdConnection> = None;
    loop {
        // Reconnect when no live connection. Race against
        // shutdown so a stuck connect doesn't block exit.
        if conn.is_none() {
            tokio::select! {
                _ = shutdown.notified() => {
                    tracing::info!(
                        plugin = crate::PLUGIN_NAME,
                        "idle observer: shutdown received pre-connect"
                    );
                    return;
                }
                result = MpdConnection::connect_with_timeouts(
                    endpoint.clone(),
                    timeouts,
                ) => {
                    match result {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            tracing::warn!(
                                plugin = crate::PLUGIN_NAME,
                                error = %e,
                                "idle observer: connect failed; backing off"
                            );
                            tokio::select! {
                                _ = shutdown.notified() => return,
                                _ = tokio::time::sleep(RECONNECT_BACKOFF) => {}
                            }
                            continue;
                        }
                    }
                }
            }
        }
        // Idle on the live connection. We unwrap because the
        // None case is handled above; the contract is invariant
        // by construction.
        let active = conn.as_mut().expect("conn is Some by construction");
        let changed = tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    "idle observer: shutdown received during idle"
                );
                return;
            }
            result = active.idle(OBSERVED, IDLE_BUDGET) => {
                match result {
                    Ok(changed) => changed,
                    Err(MpdError::Timeout { .. }) => {
                        // Budget exhausted with no MPD-side
                        // change. Normal; loop and re-issue idle.
                        continue;
                    }
                    Err(e) => {
                        // Transient transport error (MPD restart,
                        // socket reset, boot race before MPD's TCP
                        // listener is ready). The reconnect path is
                        // intrinsic to this loop — DEBUG per
                        // LOGGING.md §2; the operator has no action
                        // to take during the retry window. WARN is
                        // reserved for budget-exhausted conditions
                        // the recovery loop cannot self-heal.
                        tracing::debug!(
                            plugin = crate::PLUGIN_NAME,
                            error = %e,
                            "idle observer: idle dispatch error; \
                             dropping connection and reconnecting"
                        );
                        conn = None;
                        tokio::select! {
                            _ = shutdown.notified() => return,
                            _ = tokio::time::sleep(RECONNECT_BACKOFF) => {}
                        }
                        continue;
                    }
                }
            }
        };
        if changed.is_empty() {
            // MPD signalled no subsystems (some MPD versions
            // return an empty list on the same connection that
            // got `noidle` from the read side). Treat as a
            // no-op and re-issue idle.
            continue;
        }
        tracing::debug!(
            plugin = crate::PLUGIN_NAME,
            subsystems = ?changed,
            "idle observer: dispatching per-subsystem refresh"
        );
        dispatch_refresh(
            &endpoint,
            timeouts,
            &queue,
            &playlist,
            &favourites,
            &library,
            &changed,
        )
        .await;
    }
}

/// Open a short-lived command connection and run every
/// per-subsystem refresh implied by the changed set. Each
/// subsystem maps to one or more shelf publish/refresh calls
/// (the same calls the shelf verbs and the warm-start path
/// already use, so the wire envelopes converge on a single
/// build pipeline).
async fn dispatch_refresh(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    queue: &QueueContext,
    playlist: &PlaylistContext,
    favourites: &FavouritesContext,
    library: &LibraryContext,
    changed: &[IdleSubsystem],
) {
    let mut conn =
        match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts)
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    error = %e,
                    "idle observer: command connect failed; refresh skipped \
                     for this event burst"
                );
                return;
            }
        };
    let mut queue_published = false;
    let mut stored_playlist_handled = false;
    let mut database_rehydrated = false;
    for subsystem in changed {
        match subsystem {
            IdleSubsystem::Playlist | IdleSubsystem::Player => {
                if !queue_published {
                    queue::publish_queue(queue, &mut conn).await;
                    queue_published = true;
                }
            }
            IdleSubsystem::StoredPlaylist => {
                if !stored_playlist_handled {
                    playlist::publish_index(playlist, &mut conn).await;
                    favourites::refresh_favourites(favourites, &mut conn).await;
                    stored_playlist_handled = true;
                }
            }
            IdleSubsystem::Database | IdleSubsystem::Update
                if !database_rehydrated =>
            {
                library::rehydrate_from_mpd(library, &mut conn).await;
                database_rehydrated = true;
            }
            _ => {
                // Subsystem observed but not handled by the
                // shelf surface. The supervisor's own idle
                // connection covers transport / mixer / output;
                // sticker writes are owned by the sticker
                // reconciler.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observed_subsystems_cover_every_shelf_concern() {
        // Compile-time-style assertion: the OBSERVED constant
        // names every subsystem the shelf surface depends on.
        // If a future contributor extends the shelf set with
        // a subject driven by a new MPD subsystem (e.g. a
        // mixer-derived audio_volume mirror), this test
        // surfaces the dependency at review time.
        let observed: Vec<&IdleSubsystem> = OBSERVED.iter().collect();
        assert!(observed.contains(&&IdleSubsystem::Database));
        assert!(observed.contains(&&IdleSubsystem::Update));
        assert!(observed.contains(&&IdleSubsystem::StoredPlaylist));
        assert!(observed.contains(&&IdleSubsystem::Playlist));
        assert!(observed.contains(&&IdleSubsystem::Player));
    }
}
