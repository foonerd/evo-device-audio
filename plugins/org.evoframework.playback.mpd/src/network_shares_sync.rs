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
) -> SharesSyncHandle {
    let shutdown = Arc::new(Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        run(subscriber, querier, registry, task_shutdown).await;
    });
    SharesSyncHandle { task, shutdown }
}

async fn run(
    subscriber: Arc<dyn SubjectStateSubscriber>,
    querier: Arc<dyn SubjectQuerier>,
    registry: SourceRegistry,
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
        apply_envelope(&registry, &state).await;
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
                            apply_envelope(&registry, state).await;
                        } else {
                            // Cleared state: shares plugin retracted
                            // its envelope entirely. Remove every
                            // NAS-prefixed source so downstream
                            // consumers do not surface dead entries.
                            drop_all_nas_sources(&registry).await;
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
                            apply_envelope(&registry, &state).await;
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
async fn apply_envelope(registry: &SourceRegistry, state: &serde_json::Value) {
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
        if let Err(e) = registry.remove(&existing.id).await {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                source_id = %existing.id,
                error = %e,
                "shares-sync: remove of retired NAS source failed"
            );
        }
    }
}

/// Remove every source in the registry whose id carries the
/// NAS prefix. Called on a cleared-envelope update.
async fn drop_all_nas_sources(registry: &SourceRegistry) {
    let snapshot = registry.snapshot().await;
    for record in snapshot {
        if !record.id.starts_with(SHARES_SOURCE_ID_PREFIX) {
            continue;
        }
        if let Err(e) = registry.remove(&record.id).await {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                source_id = %record.id,
                error = %e,
                "shares-sync: remove on cleared envelope failed"
            );
        }
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
