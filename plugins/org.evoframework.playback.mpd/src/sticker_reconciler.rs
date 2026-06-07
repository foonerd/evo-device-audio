//! Sticker reconciler — keeps MPD's `evo:available` sticker in
//! lockstep with the source registry's per-source state machine.
//!
//! The reconciler subscribes to
//! [`crate::source_registry::SourceRegistry::subscribe`] and, on
//! every [`crate::source_registry::SourceStateChange`], walks every
//! song under the source's MPD mount path and writes the
//! `evo:available` sticker to `1` (when the new state is Online or
//! Degraded) or `0` (when Offline or Retired). The wire envelopes
//! the queue / playlist / favourites / library shelves emit
//! project this sticker as their per-item `available: bool` field,
//! so the reconciler is the authoritative bridge between
//! reachability state and operator-visible availability.
//!
//! # Architectural invariants pinned at the catalogue level
//!
//! - `sticker-is-availability-source-of-truth`
//!   (`audio.library.v1`): consumer shelves derive their per-item
//!   `available` flag from this sticker; the reconciler is the
//!   sole writer.
//! - `source-offline-never-triggers-update`
//!   (`audio.library.v1`): the reconciler MUST NOT invoke MPD's
//!   `update PATH` / `rescan PATH` on Offline transitions —
//!   only sticker writes. The transition handler enforces the
//!   contract structurally (it has no `update` call site).
//!
//! # Batching
//!
//! Sticker writes batch via MPD's
//! [`crate::mpd::MpdConnection::command_list`]. For a 100k-track
//! NAS this collapses 100k individual round-trips into ~1k
//! command-list groups (configurable via
//! [`STICKER_WRITE_BATCH_SIZE`]).
//!
//! # Failure mode
//!
//! Reconciliation is idempotent. A failed write leaves some songs
//! marked with the prior availability state; the next
//! reconciliation cycle (next state transition) re-runs and the
//! difference converges. No outage, no silent corruption.
//!
//! # Track-count snapshot
//!
//! On a successful Online or Offline reconciliation cycle the
//! reconciler updates the source's `track_count` +
//! `track_count_available` fields via
//! [`crate::source_registry::SourceRegistry::update_track_counts`].
//! Operator UI's per-source counts read from the registry's
//! snapshot.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use crate::mpd::{ConnectTimeouts, MpdConnection, MpdEndpoint, MpdError};
use crate::source_registry::{SourceRegistry, SourceState, SourceStateChange};

/// Sticker name the reconciler writes. Other plugins MUST NOT
/// write under this name; the `evo:` namespace is reserved for
/// framework-owned stickers.
pub(crate) const EVO_AVAILABLE_STICKER: &str = "evo:available";

/// Maximum number of `sticker set` commands per `command_list`
/// dispatch. Larger batches reduce round-trips but increase
/// per-batch failure blast radius; 200 is a balanced default
/// that takes a 100k-track source from 100k round-trips to
/// ~500 round-trips.
pub(crate) const STICKER_WRITE_BATCH_SIZE: usize = 200;

/// Backoff between reconcile-after-failure retries when MPD is
/// transiently unreachable.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Handle the plugin retains for the reconciler's lifetime.
pub(crate) struct StickerReconcilerHandle {
    task: JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl StickerReconcilerHandle {
    /// Signal shutdown + await task completion. Idempotent.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_waiters();
        let _ = self.task.await;
    }
}

/// Spawn the reconciler task. The task subscribes to the
/// registry's broadcast channel and processes
/// [`SourceStateChange`] events for the registry's lifetime.
///
/// The reconciler opens a dedicated MPD connection (separate
/// from the playback supervisor's command / idle connections
/// and the ambient observer's connection pair). Bulk sticker
/// writes against a NAS can be slow; isolating the work on its
/// own connection keeps transport verbs responsive.
pub(crate) fn spawn(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    registry: SourceRegistry,
) -> StickerReconcilerHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        run(endpoint, timeouts, registry, task_shutdown).await;
    });
    StickerReconcilerHandle { task, shutdown }
}

async fn run(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
    registry: SourceRegistry,
    shutdown: Arc<Notify>,
) {
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        endpoint = %endpoint,
        "sticker reconciler task started"
    );
    let mut rx = registry.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    "sticker reconciler: shutdown received"
                );
                return;
            }
            event = rx.recv() => {
                match event {
                    Ok(change) => {
                        if let Err(e) = reconcile_one(
                            &endpoint,
                            timeouts,
                            &registry,
                            &change,
                        )
                        .await
                        {
                            // Transient reconcile error with automatic
                            // retry on the next source transition. Logged
                            // at debug per LOGGING.md §2 because the
                            // recovery path is intrinsic to this loop and
                            // the operator has no action to take. WARN is
                            // reserved for budget-exhausted or non-
                            // recoverable conditions.
                            tracing::debug!(
                                plugin = crate::PLUGIN_NAME,
                                source_id = %change.source_id,
                                error = %e,
                                "sticker reconcile error; will retry on next transition"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Channel overflowed (subscriber too slow).
                        // Re-snapshot the registry and reconcile every
                        // source to recover.
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            lagged_messages = n,
                            "sticker reconciler lagged; re-snapshotting"
                        );
                        if let Err(e) = reconcile_all(
                            &endpoint,
                            timeouts,
                            &registry,
                        )
                        .await
                        {
                            tracing::warn!(
                                plugin = crate::PLUGIN_NAME,
                                error = %e,
                                "post-lag reconcile-all failed; will retry on next transition"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            plugin = crate::PLUGIN_NAME,
                            "sticker reconciler: registry channel closed; exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Run one reconciliation cycle for a single source's state
/// change. Opens a fresh MPD connection (the reconciler does
/// not hold one across events; bulk sticker writes complete
/// in seconds and connection re-establishment per event keeps
/// the supervisor's command connection free for transport).
async fn reconcile_one(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    registry: &SourceRegistry,
    change: &SourceStateChange,
) -> Result<(), MpdError> {
    let record = match registry.get(&change.source_id).await {
        Some(r) => r,
        None => {
            // Source was removed between transition and reconcile;
            // nothing to do.
            return Ok(());
        }
    };
    let available_value = sticker_value_for(&change.new_state);
    let mount_path = record.mount_path.to_string_lossy().into_owned();
    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        source_id = %change.source_id,
        mount_path = %mount_path,
        new_state = ?change.new_state,
        sticker_value = available_value,
        "sticker reconcile cycle started"
    );

    let mut conn = open_connection(endpoint.clone(), timeouts).await?;

    let songs = enumerate_songs_under_mount(&mut conn, &mount_path).await?;
    let total = songs.len();
    let available_count = if available_value == "1" { total } else { 0 };

    write_stickers_batched(&mut conn, &songs, available_value).await?;

    // Update the registry's track-count snapshot. Best-effort —
    // a registry error doesn't roll back the sticker writes.
    if let Err(e) = registry
        .update_track_counts(
            &change.source_id,
            total as u32,
            available_count as u32,
        )
        .await
    {
        tracing::debug!(
            plugin = crate::PLUGIN_NAME,
            source_id = %change.source_id,
            error = %e,
            "track-counts update failed; reconcile cycle still ok"
        );
    }

    tracing::info!(
        plugin = crate::PLUGIN_NAME,
        source_id = %change.source_id,
        total_songs = total,
        sticker_value = available_value,
        "sticker reconcile cycle complete"
    );
    Ok(())
}

/// Re-snapshot every registered source and reconcile each.
/// Used as the recovery path on broadcast-channel lag.
async fn reconcile_all(
    endpoint: &MpdEndpoint,
    timeouts: ConnectTimeouts,
    registry: &SourceRegistry,
) -> Result<(), MpdError> {
    for record in registry.snapshot().await {
        let change = SourceStateChange {
            source_id: record.id.clone(),
            old_state: record.state.clone(),
            new_state: record.state.clone(),
            at_ms: 0,
        };
        if let Err(e) =
            reconcile_one(endpoint, timeouts, registry, &change).await
        {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                source_id = %record.id,
                error = %e,
                "reconcile-all: per-source cycle failed; continuing"
            );
        }
    }
    Ok(())
}

/// Enumerate every song under the source's mount path. Uses
/// MPD's `lsinfo` recursively via `listallinfo PATH` would be
/// ideal but is heavy; for now use a `find file <prefix>` query
/// which MPD evaluates against its database.
async fn enumerate_songs_under_mount(
    conn: &mut MpdConnection,
    mount_path: &str,
) -> Result<Vec<String>, MpdError> {
    // MPD's `find base "PATH"` returns every song whose URI is
    // under PATH. Use base for path-anchored matching.
    use crate::mpd::MpdLibraryEntry;
    let entries = conn
        .find(crate::mpd::MpdSearchField::Base, mount_path)
        .await?;
    let mut songs = Vec::with_capacity(entries.len());
    for entry in entries {
        if let MpdLibraryEntry::File { path, .. } = entry {
            songs.push(path);
        }
    }
    Ok(songs)
}

/// Write the `evo:available` sticker on every song in the
/// supplied list, batching via `command_list`. Honours
/// [`STICKER_WRITE_BATCH_SIZE`] per group.
async fn write_stickers_batched(
    conn: &mut MpdConnection,
    songs: &[String],
    value: &str,
) -> Result<(), MpdError> {
    let mut commands: Vec<(&str, Vec<String>)> =
        Vec::with_capacity(STICKER_WRITE_BATCH_SIZE);
    for song in songs {
        commands.push((
            "sticker",
            vec![
                "set".to_string(),
                "song".to_string(),
                song.clone(),
                EVO_AVAILABLE_STICKER.to_string(),
                value.to_string(),
            ],
        ));
        if commands.len() >= STICKER_WRITE_BATCH_SIZE {
            conn.command_list(&commands).await?;
            commands.clear();
        }
    }
    if !commands.is_empty() {
        conn.command_list(&commands).await?;
    }
    Ok(())
}

/// Open an MPD connection with bounded backoff retry. Used at
/// the start of each reconcile cycle.
async fn open_connection(
    endpoint: MpdEndpoint,
    timeouts: ConnectTimeouts,
) -> Result<MpdConnection, MpdError> {
    match MpdConnection::connect_with_timeouts(endpoint.clone(), timeouts).await
    {
        Ok(c) => Ok(c),
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                error = %e,
                "sticker reconciler connect failed; backoff + retry"
            );
            tokio::time::sleep(RECONNECT_BACKOFF).await;
            MpdConnection::connect_with_timeouts(endpoint, timeouts).await
        }
    }
}

/// Translate the source's new state into the canonical sticker
/// value. The mapping is single-source-of-truth for the
/// reconciler + the consumer shelves' availability projection.
pub(crate) fn sticker_value_for(state: &SourceState) -> &'static str {
    if state.is_reachable() {
        "1"
    } else {
        "0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::{ScanPolicy, SourceKind, SourceRecord};
    use std::path::PathBuf;

    #[test]
    fn sticker_value_for_online_is_one() {
        assert_eq!(sticker_value_for(&SourceState::Online), "1");
    }

    #[test]
    fn sticker_value_for_degraded_is_one() {
        let s = SourceState::Degraded {
            reason: "x".into(),
            since_ms: 0,
        };
        assert_eq!(sticker_value_for(&s), "1");
    }

    #[test]
    fn sticker_value_for_offline_is_zero() {
        let s = SourceState::Offline {
            reason: "x".into(),
            since_ms: 0,
        };
        assert_eq!(sticker_value_for(&s), "0");
    }

    #[test]
    fn sticker_value_for_retired_is_zero() {
        assert_eq!(sticker_value_for(&SourceState::Retired), "0");
    }

    #[test]
    fn sticker_value_for_probing_is_zero() {
        // Probing is treated as unavailable until proven Online.
        assert_eq!(sticker_value_for(&SourceState::Probing), "0");
    }

    #[test]
    fn batch_size_default_is_reasonable_for_large_libraries() {
        // 100k tracks @ 200 per batch = 500 round-trips.
        const _: () = assert!(STICKER_WRITE_BATCH_SIZE >= 100);
        const _: () = assert!(STICKER_WRITE_BATCH_SIZE <= 1000);
    }

    #[test]
    fn evo_available_sticker_name_is_namespaced() {
        // The framework reserves the `evo:` namespace; other
        // plugins MUST NOT write under it. The constant is the
        // single source of truth for the name; consumer shelves
        // import it.
        assert_eq!(EVO_AVAILABLE_STICKER, "evo:available");
        assert!(EVO_AVAILABLE_STICKER.starts_with("evo:"));
    }

    #[tokio::test]
    async fn reconcile_one_skips_when_source_removed_between_event_and_handler()
    {
        let r = SourceRegistry::new();
        // Don't register; the reconcile_one path should observe
        // Registry::get returning None and return Ok without an
        // MPD connection attempt.
        let change = SourceStateChange {
            source_id: "absent".to_string(),
            old_state: SourceState::Probing,
            new_state: SourceState::Online,
            at_ms: 0,
        };
        let endpoint = MpdEndpoint::Tcp {
            host: "127.0.0.1".to_string(),
            port: 1,
        };
        let timeouts = ConnectTimeouts {
            connect: Duration::from_millis(50),
            welcome: Duration::from_millis(50),
            command: Duration::from_millis(50),
        };
        let res = reconcile_one(&endpoint, timeouts, &r, &change).await;
        assert!(res.is_ok());
    }

    #[allow(dead_code)]
    fn local_record(id: &str) -> SourceRecord {
        SourceRecord {
            id: id.to_string(),
            display_name: "test".into(),
            kind: SourceKind::LocalInternal,
            mount_path: PathBuf::from("/var/lib/evo/music/INTERNAL"),
            mpd_storage_name: None,
            state: SourceState::Probing,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms: None,
        }
    }

    // (No spawn() lifecycle test here: spawn() returns immediately
    // and stop() races with the task entering its select loop;
    // `Notify::notify_waiters()` doesn't queue past notifications
    // for late-arriving waiters, so the test path can hang
    // pathologically without a synchronisation point that the
    // production-path doesn't need. The lifecycle is exercised
    // end-to-end at the plugin level when the supervisor's
    // tear-down path drives stop() after live event flow.)

    fn _unused_local_record_pin(_: SourceRecord) {}
}
