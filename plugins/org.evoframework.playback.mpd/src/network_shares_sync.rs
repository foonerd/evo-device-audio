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
//! and admits one `SourceRecord` per entry through the same
//! source life `library.add_source` uses: register once, probe
//! now, keep the observed state and counts on later ticks.
//! Entries no longer present in the envelope are retracted.
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
    default_probe_cadence_for, default_scan_policy_for, probe_source,
    should_start_online_scan, SourceKind, SourceRecord, SourceRegistry,
    SourceState, PROBE_BUDGET,
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
        admit_attached_store(registry, retract, record).await;
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

/// USB, NFS, SMB, and every other attached store use this
/// life: register once, probe now, keep the observed row.
///
/// A raw upsert of a Probing stub with cadence 0 is the field
/// lie: Browse stuck on probing, index 0 until a hand Rescan,
/// a later envelope tick wiping Online and the counts.
async fn admit_attached_store(
    registry: &SourceRegistry,
    retract: &RetractHandles,
    incoming: SourceRecord,
) {
    if let Some(existing) = registry.get(&incoming.id).await {
        if existing.display_name != incoming.display_name
            || existing.mount_path != incoming.mount_path
            || existing.kind != incoming.kind
        {
            let mut kept = existing;
            kept.display_name = incoming.display_name;
            kept.mount_path = incoming.mount_path;
            kept.kind = incoming.kind;
            registry.upsert(kept).await;
            crate::library::publish_subjects(&retract.library).await;
        }
        start_online_scan_if_due(retract, &incoming.id).await;
        return;
    }
    let id = incoming.id.clone();
    if let Err(e) = registry.register(incoming).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %id,
            error = %e,
            "shares-sync: admit register failed"
        );
        return;
    }
    if let Some(registered) = registry.get(&id).await {
        let outcome = probe_source(&registered, PROBE_BUDGET).await;
        if let Err(e) = registry.transition(&id, outcome.new_state).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                source_id = %id,
                error = %e,
                "shares-sync: admit probe transition failed; \
                 source stays Probing until the next probe"
            );
        }
    }
    let _ = registry.persist().await;
    crate::library::publish_subjects(&retract.library).await;
    start_online_scan_if_due(retract, &id).await;
}

/// Kick `library.update_source` for an Online store that has
/// never been scanned. Same verb as operator Rescan. Fire and
/// warn: admission has already landed.
async fn start_online_scan_if_due(retract: &RetractHandles, source_id: &str) {
    let Some(record) = retract.library.registry.get(source_id).await else {
        return;
    };
    if !should_start_online_scan(&record) {
        return;
    }
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
                "shares-sync: no MPD connection to start the first \
                 index; operator Rescan remains"
            );
            return;
        }
    };
    let payload = crate::library::UpdateSourcePayload {
        v: crate::library::LIBRARY_PAYLOAD_VERSION,
        source_id: source_id.to_string(),
        force_rescan: false,
    };
    if let Err(e) = crate::library::handle_update_source(
        &retract.library,
        &mut conn,
        payload,
    )
    .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %source_id,
            error = %e,
            "shares-sync: first online scan did not start; \
             operator Rescan remains"
        );
        return;
    }
    // Stamp last_scan so a later configured-shares tick does not
    // start a second update while the first is still walking.
    // Scan-progress overwrites this with the completion time.
    let _ = retract
        .library
        .registry
        .update_track_counts(
            source_id,
            record.track_count,
            record.track_count_available,
        )
        .await;
}

/// Translate one share entry from the envelope into a
/// [`SourceRecord`]. Returns `None` when required fields are
/// missing or the fstype is not one we handle.
fn record_from_envelope_share(
    share: &serde_json::Value,
) -> Option<SourceRecord> {
    // The shares plugin publishes `ShareRecord` as `share_id`.
    // Looking up `id` drops every Connected share: the registry
    // stays empty, the floor filter hides NAS, and retract never
    // sees a row to drop. `id` is kept only as a fallback.
    let share_id = share
        .get("share_id")
        .or_else(|| share.get("id"))
        .and_then(|v| v.as_str())?;
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
    let probe_cadence_ms = default_probe_cadence_for(&kind);
    let scan_policy = default_scan_policy_for(&kind);
    Some(SourceRecord {
        id: source_id,
        display_name: alias.to_string(),
        kind,
        mount_path: std::path::PathBuf::from(mount_root),
        mpd_storage_name: None,
        // First sight is Probing. `admit_attached_store` probes
        // immediately, the same door `library.add_source` uses
        // for USB and every other attached store.
        state: SourceState::Probing,
        last_seen_online_at_ms: None,
        probe_cadence_ms,
        scan_policy,
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
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        use crate::playback_supervisor::test_mock::{
            short_timeouts, spawn_mock_mpd, ConnBehaviour,
        };
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
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
                commands: Arc::clone(&commands),
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
        (registry, retract, ann, playlists, commands)
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
        let (registry, retract, ann, _playlists, _cmds) =
            retire_harness().await;

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
        let (registry, retract, _ann, playlists, _cmds) =
            retire_harness().await;

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
    async fn the_scrub_rewalks_the_level_the_share_vanished_from() {
        // Local library still listed the removed NAS in the
        // field. The share's own directory is already gone when
        // this runs — network.shares deletes its empty
        // mount-root on remove — so an update aimed at that path
        // walks nothing and MPD keeps every stale row beneath
        // it. The parent is the level that has to be re-walked
        // for MPD to notice the child has gone, and that is what
        // takes NAS off the floor tree.
        let (registry, retract, _ann, _playlists, cmds) =
            retire_harness().await;

        apply_envelope(&registry, &retract, &empty_envelope()).await;

        let seen = cmds.lock().unwrap().clone();
        let updates: Vec<&String> = seen
            .iter()
            .filter(|c| c.split_whitespace().next() == Some("update"))
            .collect();
        assert!(
            !updates.is_empty(),
            "the scrub must issue an update: {seen:?}",
        );
        assert!(
            updates.iter().any(|c| c.contains("NAS")),
            "the scrub re-walks a level that still exists: {updates:?}",
        );
        assert!(
            updates.iter().all(|c| !c.contains("NAS/Music")),
            "aiming the scrub at the path that is already gone walks \
             nothing and leaves NAS on the floor: {updates:?}",
        );
    }

    #[test]
    fn the_scrub_target_is_the_parent_level() {
        // The rule in one read, including a source sitting at
        // the database root, where the root is the parent.
        assert_eq!(
            crate::library::scrub_parent_of("NAS/Music"),
            Some("NAS".to_string()),
        );
        assert_eq!(
            crate::library::scrub_parent_of("USB/STICK"),
            Some("USB".to_string()),
        );
        assert_eq!(crate::library::scrub_parent_of("NAS"), None);
        assert_eq!(crate::library::scrub_parent_of(""), None);
    }

    #[test]
    fn the_floor_drops_a_mount_point_whose_source_is_gone() {
        // The operator invert, not a command string. Remove
        // deletes the record; it cannot delete a mount-root
        // directory a failed unmount left behind, and MPD lists
        // what is on disk. Local library must still stop
        // offering it.
        let music = std::path::PathBuf::from("/var/lib/evo/music");
        let live: Vec<crate::source_registry::SourceRecord> = Vec::new();

        assert!(
            !crate::library::floor_lists_directory("NAS/Test", &music, &live),
            "a leftover empty NFS/Test mount-root is a FAIL, not a skip",
        );
        assert!(
            !crate::library::floor_lists_directory("NAS", &music, &live),
            "and an emptied NAS root leaves the floor with it",
        );
        assert!(
            !crate::library::floor_lists_directory("USB/STICK", &music, &live),
            "the same rule holds for a detached stick",
        );
        assert!(
            crate::library::floor_lists_directory("INTERNAL", &music, &live),
            "ordinary content is never a mount point and always lists",
        );
        assert!(
            crate::library::floor_lists_directory(
                "INTERNAL/Albums",
                &music,
                &live
            ),
            "nor is anything under it",
        );
    }

    #[test]
    fn the_floor_keeps_a_mount_point_its_source_still_owns() {
        // The other half: a live share is still the library.
        let music = std::path::PathBuf::from("/var/lib/evo/music");
        let live = vec![nas_record()];

        assert!(
            crate::library::floor_lists_directory("NAS/Music", &music, &live),
            "a live share lists",
        );
        assert!(
            crate::library::floor_lists_directory("NAS", &music, &live),
            "and so does the root that still holds it",
        );
        assert!(
            crate::library::floor_lists_directory(
                "NAS/Music/Album",
                &music,
                &live
            ),
            "and its own tree beneath it",
        );
        assert!(
            !crate::library::floor_lists_directory("NAS/Test", &music, &live),
            "while a sibling nobody owns still does not",
        );
    }

    #[tokio::test]
    async fn a_cleared_envelope_retracts_the_same_way() {
        // The shares plugin retracting its envelope entirely is
        // the same operator outcome as removing each share.
        let (registry, retract, ann, playlists, _cmds) = retire_harness().await;

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

    /// The envelope `network.shares` actually publishes.
    /// Field `.24` 2026-09-20: Connected NFS, key is `share_id`.
    fn wire_share_envelope() -> serde_json::Value {
        serde_json::json!({
            "shares": [{
                "advanced_options": "",
                "alias": "NFS",
                "created_at_ms": 1_789_876_977_600i64,
                "credentials": { "kind": "guest" },
                "fstype": "nfs",
                "host": "192.168.30.1",
                "last_mounted_at_ms": null,
                "mount_root": "/var/lib/evo/music/NAS/NFS",
                "path": "/volume1/multimedia/broadcast/Audio",
                "persisted_vers": null,
                "share_id": "82befb0b-740a-4e65-bae2-5c29e81a6a58"
            }]
        })
    }

    #[test]
    fn a_connected_share_from_the_wire_envelope_owns_the_floor() {
        // Operator invert: Sources Connected, Local library has
        // no NAS. The published key is `share_id`. Reading `id`
        // only reddens this — the registry stays empty and
        // 4da953c hides the live mount.
        let share = &wire_share_envelope()["shares"][0];
        let record = record_from_envelope_share(share)
            .expect("the wire key is share_id");
        assert_eq!(
            record.id, "nas-82befb0b-740a-4e65-bae2-5c29e81a6a58",
            "the registry row is keyed from the published share_id",
        );
        assert_eq!(
            record.mount_path,
            PathBuf::from("/var/lib/evo/music/NAS/NFS"),
        );
        let music = PathBuf::from("/var/lib/evo/music");
        let live = vec![record];
        assert!(
            crate::library::floor_lists_directory("NAS", &music, &live),
            "a Connected share must list the NAS root",
        );
        assert!(
            crate::library::floor_lists_directory("NAS/NFS", &music, &live),
            "and its own mount point",
        );
        assert!(
            !crate::library::floor_lists_directory("NAS/Test", &music, &live),
            "a leftover nobody owns still must not list",
        );
    }

    #[tokio::test]
    async fn apply_envelope_registers_a_share_id_row() {
        let (registry, retract, _ann, _playlists, _cmds) =
            retire_harness().await;

        apply_envelope(&registry, &retract, &wire_share_envelope()).await;

        assert!(
            registry
                .get("nas-82befb0b-740a-4e65-bae2-5c29e81a6a58")
                .await
                .is_some(),
            "the live envelope must produce a registry source",
        );
    }

    #[test]
    fn an_envelope_share_uses_the_kind_source_defaults() {
        // The library model: NFS/SMB take the same cadence and
        // scan policy as the kind, not a cadence-0 stub that
        // never probes.
        let share = &wire_share_envelope()["shares"][0];
        let record = record_from_envelope_share(share)
            .expect("the wire key is share_id");
        assert_eq!(
            record.probe_cadence_ms,
            crate::source_registry::DEFAULT_NAS_PROBE_CADENCE_MS,
        );
        assert_eq!(record.scan_policy, default_scan_policy_for(&record.kind),);
        assert_ne!(record.probe_cadence_ms, 0, "cadence 0 never probes");
    }

    #[tokio::test]
    async fn admitting_a_new_share_probes_a_reachable_mount() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, retract, _ann, _playlists, _cmds) =
            retire_harness().await;
        let mut env = wire_share_envelope();
        env["shares"][0]["mount_root"] =
            serde_json::Value::String(dir.path().display().to_string());
        apply_envelope(&registry, &retract, &env).await;
        let rec = registry
            .get("nas-82befb0b-740a-4e65-bae2-5c29e81a6a58")
            .await
            .expect("admitted");
        assert_eq!(
            rec.state.discriminant(),
            crate::source_registry::SourceState::Online.discriminant(),
            "a reachable mount is observed, not left Probing",
        );
    }

    #[tokio::test]
    async fn a_republish_does_not_reset_an_online_share_to_probing() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, retract, _ann, _playlists, _cmds) =
            retire_harness().await;
        let mut env = wire_share_envelope();
        env["shares"][0]["mount_root"] =
            serde_json::Value::String(dir.path().display().to_string());
        apply_envelope(&registry, &retract, &env).await;
        let id = "nas-82befb0b-740a-4e65-bae2-5c29e81a6a58";
        registry.update_track_counts(id, 40, 40).await.unwrap();
        apply_envelope(&registry, &retract, &env).await;
        let rec = registry.get(id).await.expect("still there");
        assert_eq!(
            rec.state.discriminant(),
            crate::source_registry::SourceState::Online.discriminant(),
        );
        assert_eq!(rec.track_count, 40, "a republish must not wipe the index");
    }

    #[tokio::test]
    async fn admitting_a_reachable_share_starts_the_first_index() {
        // Operator invert: Browse NFS Online, index 0 until a
        // hand Rescan. Online + on_online must issue the same
        // update PATH Rescan uses.
        use crate::playback_supervisor::test_mock::{
            short_timeouts, spawn_mock_mpd, ConnBehaviour,
        };
        let music = tempfile::tempdir().unwrap();
        let mount = music.path().join("NAS").join("NFS");
        std::fs::create_dir_all(&mount).unwrap();
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let playlists = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::clone(&commands),
                playlists,
                library: Vec::new(),
                queue: Vec::new(),
            }])
            .await;
        let registry = SourceRegistry::new();
        let ann = Arc::new(RecordingAnn::default());
        let library = crate::library::LibraryContext::new(
            music.path().to_path_buf(),
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
            music.path().to_path_buf(),
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
        let mut env = wire_share_envelope();
        env["shares"][0]["mount_root"] =
            serde_json::Value::String(mount.display().to_string());
        apply_envelope(&registry, &retract, &env).await;
        let rec = registry
            .get("nas-82befb0b-740a-4e65-bae2-5c29e81a6a58")
            .await
            .expect("admitted");
        assert_eq!(
            rec.state.discriminant(),
            crate::source_registry::SourceState::Online.discriminant(),
        );
        let sent = commands.lock().unwrap().clone();
        assert!(
            sent.iter().any(|c| c.starts_with("update")),
            "the first Online must start an index, got {sent:?}"
        );
        apply_envelope(&registry, &retract, &env).await;
        let after = commands.lock().unwrap().clone();
        let updates = after.iter().filter(|c| c.starts_with("update")).count();
        assert_eq!(
            updates,
            sent.iter().filter(|c| c.starts_with("update")).count(),
            "a republish of an already-kicked share must not \
             start another update: {after:?}"
        );
    }
}
