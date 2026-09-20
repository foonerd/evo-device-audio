// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Sync mounted network shares (SMB/CIFS + NFS) into the MPD
//! plugin's [`crate::source_registry::SourceRegistry`] so
//! `SourceKind::NetworkNasSmb` / `NetworkNasNfs` records exist
//! for every share the operator has configured.
//!
//! Without this the mounted files under
//! `/var/lib/evo/music/NAS/<alias>/…` still browse + play (MPD's
//! `mpc update <path>` builds the database from the FS tree —
//! see the shares plugin's mount-success hook), but the library
//! projection shows only a raw FS tree with no source labelling.
//! Source-aware media (per-source availability, per-source scan
//! policy, source-tagged tiles) needs a first-class
//! `SourceRecord`.
//!
//! ## Coupling shape
//!
//! MPD subscribes to the shares plugin's already-published
//! `system_network_shares_configured` singleton (addressing
//! scheme `evo.network.shares.configured`, value `local`). On
//! every state update the subscriber walks the `shares` array
//! and upserts one `SourceRecord` per entry, keyed by a stable
//! `source_id` derived from the share's `id` field. Entries no
//! longer present in the envelope are removed from the registry.
//!
//! One-way: MPD is a consumer of the shares subject, no reverse
//! coupling. The shares plugin does not know MPD subscribes.
//!
//! Retries the canonical-id resolve on backoff to cover the
//! (rare) admission-order window where `playback.mpd` loads
//! before `network.shares` — Phase 2 discovery walks
//! `/opt/evo/plugins/` alphabetically, so `network.shares` DOES
//! admit before `playback.mpd` on the reference distribution
//! (`network.` < `playback.`), but the retry closes the window
//! without depending on that ordering.

use std::sync::Arc;
use std::time::Duration;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectQuerier, SubjectStateStreamError,
    SubjectStateSubscriber,
};
use tokio::sync::Notify;

use crate::source_registry::{
    ScanPolicy, SourceKind, SourceRecord, SourceRegistry, SourceState,
};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Addressing for the shares plugin's configured-shares subject.
/// Matches [`evo-device-audio/plugins/org.evoframework.network.
/// shares/src/runtime.rs::CONFIGURED_SUBJECT_SCHEME`] +
/// `SINGLETON_ADDRESSING_VALUE`.
const SHARES_SUBJECT_SCHEME: &str = "evo.network.shares.configured";
const SHARES_SUBJECT_VALUE: &str = "local";

/// Backoff for canonical-id resolution when the shares plugin
/// has not announced yet.
const RESOLVE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Prefix used to derive a stable `source_id` from a share
/// record's own `id`. The registry treats source_ids as opaque
/// strings — the prefix is diagnostic only, so an operator
/// reading a projection sees `nas-<share-id>` and knows this
/// source came from the shares plugin without cross-referencing
/// documentation.
const SHARES_SOURCE_ID_PREFIX: &str = "nas-";

/// Handle for the shares-sync background task. Dropping the
/// handle detaches; call [`Self::stop`] to signal shutdown and
/// await the task's exit deterministically.
pub(crate) struct SharesSyncHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: Arc<Notify>,
}

impl SharesSyncHandle {
    pub(crate) async fn stop(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Spawn the shares-sync task. Runs until the returned
/// [`SharesSyncHandle`] is stopped or the subscribe stream
/// closes (framework shutdown / shares plugin unload).
pub(crate) fn spawn_shares_sync(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    registry: SourceRegistry,
    retract: RetractHandles,
) -> SharesSyncHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        run(subscriber, querier, registry, retract, task_shutdown).await;
    });
    SharesSyncHandle { task, shutdown }
}

/// What retiring a share needs beyond the registry.
///
/// A share leaving the envelope is a Remove: the operator is
/// not going to see that NAS again. Dropping the registry row
/// alone leaves its tracks in MPD's database, in every stored
/// playlist and in favourites, and leaves `audio_library_sources`
/// still naming it — Browse keeps listing a NAS that is gone.
/// These handles let the retirement run the same retraction the
/// Sources-page Remove runs, rather than a second implementation
/// of it.
#[derive(Clone)]
pub(crate) struct RetractHandles {
    pub(crate) library: crate::library::LibraryContext,
    pub(crate) queue: crate::queue::QueueContext,
    pub(crate) endpoint: crate::mpd::MpdEndpoint,
    pub(crate) timeouts: crate::mpd::ConnectTimeouts,
}

/// Retract one retired share through the ordinary removal verb.
///
/// `library.remove_source` with the scrub flag is the Sources-page
/// Remove's own path: it drops the source's rows from stored
/// playlists and favourites, scrubs MPD's database, re-counts the
/// floor and republishes the library subjects. Calling it here is
/// what makes a share retired by the shares plugin leave Browse
/// and the playlists, not only the registry map.
///
/// In-process, not a dispatch: this is the same plugin, so there
/// is no nested-verb wait to walk into.
///
/// Best-effort. The share is gone from the shares plugin either
/// way; a retraction that could not run must not strand the
/// registry row, so a failure falls back to the bare drop.
async fn retract_retired_source(
    retract: &RetractHandles,
    registry: &SourceRegistry,
    source_id: &str,
) {
    let mut conn = match crate::mpd::MpdConnection::connect_with_timeouts(
        retract.endpoint.clone(),
        retract.timeouts,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                source_id = %source_id,
                error = %e,
                "shares-sync: no MPD connection to retract a retired \
                 share; dropping the registry row alone"
            );
            let _ = registry.remove(source_id).await;
            return;
        }
    };
    let payload = crate::library::RemoveSourcePayload {
        v: crate::library::LIBRARY_PAYLOAD_VERSION,
        source_id: source_id.to_string(),
        scrub_mpd_entries: true,
        consumer_stop: false,
    };
    if let Err(e) = crate::library::handle_remove_source(
        &retract.library,
        &retract.queue,
        &mut conn,
        payload,
    )
    .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            error = %e,
            "shares-sync: retraction of a retired share failed; \
             dropping the registry row alone"
        );
        let _ = registry.remove(source_id).await;
    }
}

async fn run(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    registry: SourceRegistry,
    retract: RetractHandles,
    shutdown: Arc<Notify>,
) {
    let addressing =
        ExternalAddressing::new(SHARES_SUBJECT_SCHEME, SHARES_SUBJECT_VALUE);

    // 1. Resolve canonical id with bounded backoff. Bail early
    //    if the plugin is shutting down.
    let canonical_id = loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "shares-sync: shutdown before canonical id resolved"
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
                            "shares-sync: resolve_addressing errored; retrying"
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
        "shares-sync: canonical id resolved"
    );

    // 2. Subscribe FIRST so no state change lands between the
    //    seed read and the loop start.
    let mut stream =
        match subscriber.subscribe_subject(canonical_id.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    canonical_id = %canonical_id,
                    error = %e,
                    "shares-sync: subscribe failed; \
                     mounted shares will not register until next plugin reload"
                );
                return;
            }
        };

    // 3. Seed from current state — the shares plugin's envelope
    //    of already-configured shares.
    if let Ok(Some(state)) =
        subscriber.current_state(canonical_id.clone()).await
    {
        apply_envelope(&registry, &retract, &state).await;
    }

    // 4. Loop on future updates.
    loop {
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "shares-sync: shutdown received; task exiting"
                );
                return;
            }
            next = stream.recv() => {
                match next {
                    Ok(update) => {
                        if let Some(state) = update.state.as_ref() {
                            apply_envelope(&registry, &retract, state)
                                .await;
                        } else {
                            // Cleared state: shares plugin retracted
                            // its envelope entirely. Remove every
                            // NAS-prefixed source so downstream
                            // consumers do not surface dead entries.
                            drop_all_nas_sources(&registry, &retract)
                                .await;
                        }
                    }
                    Err(SubjectStateStreamError::Lagged { dropped }) => {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            dropped,
                            "shares-sync: stream lagged; resyncing from \
                             current_state"
                        );
                        if let Ok(Some(state)) = subscriber
                            .current_state(canonical_id.clone())
                            .await
                        {
                            apply_envelope(&registry, &retract, &state).await;
                        }
                    }
                    Err(SubjectStateStreamError::Closed) => {
                        tracing::info!(
                            plugin = PLUGIN_NAME,
                            "shares-sync: stream closed; task exiting"
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// Reconcile the registry against the shares envelope. Adds /
/// updates a `SourceRecord` per share entry; removes entries
/// whose `source_id` is no longer present.
async fn apply_envelope(
    registry: &SourceRegistry,
    retract: &RetractHandles,
    state: &serde_json::Value,
) {
    let shares = match state.get("shares").and_then(|v| v.as_array()) {
        Some(a) => a,
        None => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                "shares-sync: envelope missing shares array; skipping"
            );
            return;
        }
    };

    // 1. Compose desired source records (one per share in the
    //    envelope). Skip malformed entries with a debug log; the
    //    envelope is well-formed on the happy path but a mid-
    //    schema-migration envelope should not brick the sync.
    let mut desired_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    for share in shares {
        let Some(record) = record_from_envelope_share(share) else {
            continue;
        };
        desired_ids.insert(record.id.clone());
        registry.upsert(record).await;
    }

    // 2. Remove NAS-prefixed sources no longer present in the
    //    envelope (share was removed via the shares plugin's
    //    remove_share verb; the envelope shrank).
    let snapshot = registry.snapshot().await;
    for existing in snapshot {
        if !existing.id.starts_with(SHARES_SOURCE_ID_PREFIX) {
            continue;
        }
        if desired_ids.contains(&existing.id) {
            continue;
        }
        retract_retired_source(retract, registry, &existing.id).await;
    }
}

/// Remove every source in the registry whose id carries the
/// NAS prefix. Called on a cleared-envelope update.
async fn drop_all_nas_sources(
    registry: &SourceRegistry,
    retract: &RetractHandles,
) {
    let snapshot = registry.snapshot().await;
    for record in snapshot {
        if !record.id.starts_with(SHARES_SOURCE_ID_PREFIX) {
            continue;
        }
        retract_retired_source(retract, registry, &record.id).await;
    }
}

/// Translate one share entry from the envelope into a
/// [`SourceRecord`]. Returns `None` when required fields are
/// missing or the fstype is not one we handle.
fn record_from_envelope_share(
    share: &serde_json::Value,
) -> Option<SourceRecord> {
    let share_id = share.get("id").and_then(|v| v.as_str())?;
    let alias = share.get("alias").and_then(|v| v.as_str())?;
    let host = share.get("host").and_then(|v| v.as_str())?;
    let path = share.get("path").and_then(|v| v.as_str())?;
    let fstype = share.get("fstype").and_then(|v| v.as_str())?;
    let mount_root = share.get("mount_root").and_then(|v| v.as_str())?;

    let kind = match fstype {
        "cifs" => {
            // SMB username lives in the credentials block. Guest
            // shares carry no username; represent as empty string
            // so the record still lands (the operator sees the
            // source; credential shape is a separate UX).
            let username = share
                .get("credentials")
                .and_then(|c| c.get("username"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            SourceKind::NetworkNasSmb {
                server: host.to_string(),
                share: path.to_string(),
                username,
            }
        }
        "nfs" => SourceKind::NetworkNasNfs {
            server: host.to_string(),
            export: path.to_string(),
        },
        _ => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                share_id = %share_id,
                fstype = %fstype,
                "shares-sync: unknown fstype; skipping"
            );
            return None;
        }
    };

    let source_id = format!("{SHARES_SOURCE_ID_PREFIX}{share_id}");
    Some(SourceRecord {
        id: source_id,
        display_name: alias.to_string(),
        kind,
        mount_path: std::path::PathBuf::from(mount_root),
        mpd_storage_name: None,
        // Start Probing so the source is visible but not yet
        // claimed reachable. The existing per-source probe
        // machinery (see `probe_source` in source_registry) is
        // what transitions Probing → Online / Degraded / Offline
        // on its own cadence; this sync's only job is to keep
        // the registry populated against the shares envelope.
        state: SourceState::Probing,
        last_seen_online_at_ms: None,
        probe_cadence_ms: 0,
        // Same shape default_scan_policy_for uses for NAS: eager
        // incremental with `update PATH` on Online transitions
        // (mount events do not fire for NAS the way they do for
        // LocalUsb — the shares plugin's mount-success hook is
        // the mount event, and it already runs `mpc update`
        // via F1.1).
        scan_policy: ScanPolicy::EagerIncremental {
            on_online: true,
            on_mount_event: false,
        },
        track_count: 0,
        track_count_available: 0,
        last_scan_at_ms: None,
    })
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::SubjectAnnouncer;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// Records every subject state update, so a test can read
    /// what Browse would be following.
    #[derive(Default)]
    struct RecordingAnn {
        updates: std::sync::Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl RecordingAnn {
        fn states_on(&self, value: &str) -> Vec<serde_json::Value> {
            self.updates
                .lock()
                .unwrap()
                .iter()
                .filter(|(v, _)| v == value)
                .map(|(_, s)| s.clone())
                .collect()
        }
    }

    impl SubjectAnnouncer for RecordingAnn {
        fn announce<'a>(
            &'a self,
            _a: evo_plugin_sdk::contract::SubjectAnnouncement,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }

        fn retract<'a>(
            &'a self,
            _addressing: ExternalAddressing,
            _reason: Option<String>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            addressing: ExternalAddressing,
            state: serde_json::Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            (),
                            evo_plugin_sdk::contract::ReportError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.updates.lock().unwrap().push((addressing.value, state));
            Box::pin(async { Ok(()) })
        }
    }

    const NAS_ID: &str = "nas-music";
    const NAS_MOUNT: &str = "/var/lib/evo/music/NAS/Music";

    fn nas_record() -> crate::source_registry::SourceRecord {
        crate::source_registry::SourceRecord {
            id: NAS_ID.to_string(),
            display_name: "Music".to_string(),
            kind: crate::source_registry::SourceKind::NetworkNasSmb {
                server: "192.0.2.10".to_string(),
                share: "Music".to_string(),
                username: "guest".to_string(),
            },
            mount_path: PathBuf::from(NAS_MOUNT),
            mpd_storage_name: None,
            state: crate::source_registry::SourceState::Online,
            last_seen_online_at_ms: None,
            probe_cadence_ms: 60_000,
            scan_policy: crate::source_registry::ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            track_count: 2,
            track_count_available: 2,
            last_scan_at_ms: None,
        }
    }

    /// A registry holding the NAS, an MPD holding two stored
    /// lists that carry its tracks, and the handles the
    /// retirement needs.
    #[allow(clippy::type_complexity)]
    async fn retire_harness() -> (
        SourceRegistry,
        RetractHandles,
        Arc<RecordingAnn>,
        Arc<std::sync::Mutex<BTreeMap<String, Vec<String>>>>,
    ) {
        use crate::playback_supervisor::test_mock::{
            short_timeouts, spawn_mock_mpd, ConnBehaviour,
        };
        let playlists = Arc::new(std::sync::Mutex::new(BTreeMap::from([
            (
                "Road mix".to_string(),
                vec![
                    "NAS/Music/gone-one.flac".to_string(),
                    "INTERNAL/keep.flac".to_string(),
                ],
            ),
            (
                crate::playlist::DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
                vec![
                    "NAS/Music/gone-two.flac".to_string(),
                    "INTERNAL/loved.flac".to_string(),
                ],
            ),
        ])));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::new(std::sync::Mutex::new(Vec::new())),
                playlists: Arc::clone(&playlists),
                library: Vec::new(),
                queue: Vec::new(),
            }])
            .await;

        let registry = SourceRegistry::new();
        registry.register(nas_record()).await.unwrap();
        let ann = Arc::new(RecordingAnn::default());
        let library = crate::library::LibraryContext::new(
            PathBuf::from("/var/lib/evo/music"),
            registry.clone(),
            Arc::clone(&ann) as Arc<dyn SubjectAnnouncer>,
            None,
        );
        let disposition = crate::disposition_emitter::DispositionEmitter::new(
            Arc::clone(&ann) as Arc<dyn SubjectAnnouncer>,
        );
        let skip = crate::skip_traversal::SkipTraversal::new(
            registry.clone(),
            disposition,
        );
        let queue = crate::queue::QueueContext::new(
            PathBuf::from("/var/lib/evo/music"),
            registry.clone(),
            Arc::clone(&ann) as Arc<dyn SubjectAnnouncer>,
            skip,
            None,
            crate::mute_cell::MuteCell::new(),
        );
        let retract = RetractHandles {
            library,
            queue,
            endpoint,
            timeouts: short_timeouts(),
        };
        (registry, retract, ann, playlists)
    }

    /// The share is gone from the shares plugin's envelope.
    fn empty_envelope() -> serde_json::Value {
        serde_json::json!({ "shares": [] })
    }

    #[tokio::test]
    async fn a_retired_share_leaves_browse() {
        // Browse follows audio_library_sources. Dropping the
        // registry row without republishing leaves a NAS on the
        // glass that the operator already removed.
        let (registry, retract, ann, _playlists) = retire_harness().await;

        apply_envelope(&registry, &retract, &empty_envelope()).await;

        assert!(
            registry.get(NAS_ID).await.is_none(),
            "the registry row is gone",
        );
        let published = ann.states_on("sources");
        let last = published
            .last()
            .expect("the library sources subject must be republished");
        assert!(
            !serde_json::to_string(last).unwrap().contains(NAS_ID),
            "Browse must not still list the removed NAS: {last}",
        );
    }

    #[tokio::test]
    async fn a_retired_share_leaves_the_stored_playlists_and_favourites() {
        // The tracks are not open files and nothing prunes them
        // on their own. A playlist that still lists a removed
        // NAS plays nothing and says nothing.
        let (registry, retract, _ann, playlists) = retire_harness().await;

        apply_envelope(&registry, &retract, &empty_envelope()).await;

        let held = playlists.lock().unwrap().clone();
        assert_eq!(
            held.get("Road mix"),
            Some(&vec!["INTERNAL/keep.flac".to_string()]),
            "the named list keeps only what is still there",
        );
        assert_eq!(
            held.get(crate::playlist::DEFAULT_FAVOURITES_PLAYLIST_NAME),
            Some(&vec!["INTERNAL/loved.flac".to_string()]),
            "favourites is a stored list too and retracts with them",
        );
    }

    #[tokio::test]
    async fn a_cleared_envelope_retracts_the_same_way() {
        // The shares plugin retracting its envelope entirely is
        // the same operator outcome as removing each share.
        let (registry, retract, ann, playlists) = retire_harness().await;

        drop_all_nas_sources(&registry, &retract).await;

        assert!(registry.get(NAS_ID).await.is_none());
        let published = ann.states_on("sources");
        assert!(
            !serde_json::to_string(published.last().expect("republished"))
                .unwrap()
                .contains(NAS_ID),
            "Browse retracts on a cleared envelope too",
        );
        assert_eq!(
            playlists.lock().unwrap().get("Road mix"),
            Some(&vec!["INTERNAL/keep.flac".to_string()]),
            "and so do the stored lists",
        );
    }
}
