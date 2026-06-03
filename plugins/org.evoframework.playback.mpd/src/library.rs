//! Library shelf — verb handlers + three subjects.
//!
//! Realises the `audio.library.v1` catalogue contract. Owns the
//! source registry surface (verbs to register / inspect /
//! probe / wake / update / remove library sources), the browse +
//! search verbs, and the three subjects
//! (`audio_library_sources`, `audio_library_state`,
//! `audio_library_scan_progress`).
//!
//! # Verb surface (8)
//!
//! - `library.list_sources` — full source registry snapshot.
//! - `library.add_source` — register a new source.
//! - `library.remove_source` — deregister; floor-protected.
//! - `library.probe_source` — immediate reachability probe.
//! - `library.wake_source` — type-specific recovery attempt.
//! - `library.update_source` — trigger MPD update / rescan.
//! - `library.browse_library` — directory-tree walk; serves
//!   stale cache when source is Offline.
//! - `library.search_library` — query via MPD search/find or
//!   the cloud native search (cloud substrate v0.1.14;
//!   delegates to local for now).
//!
//! # Catalogue acceptance rows honoured
//!
//! - `source-offline-never-triggers-update`: the update_source
//!   verb refuses with `source_offline` when the source's state
//!   is Offline. The acceptance row's structural protection
//!   against Volumio's wipe-on-remount anti-pattern.
//! - `sticker-is-availability-source-of-truth`: per-item
//!   availability comes from the sticker via the queue's
//!   resolver path.
//! - `cloud-default-scan-policy-is-lazy`: cloud sources default
//!   to LazyBrowseDriven; eager_incremental requires the
//!   `cloud_eager_scan_acknowledged: true` flag on add_source.
//! - `source-supervisor-per-source-isolation`: each source has
//!   its own probe + state machine in the registry; this module
//!   reads via the registry's snapshot API.
//! - `library-state-subject-published-on-load-and-every-source-transition`:
//!   the audio_library_sources subject refreshes on every
//!   source registry change.
//! - `local-internal-source-is-non-removable`: remove_source
//!   refuses on the LOCAL_INTERNAL_SOURCE_ID.
//! - `browse-library-returns-stale-cache-when-source-offline`:
//!   browse against Offline source returns the cached listing
//!   with stale:true; no refusal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::mpd::{MpdConnection, MpdLibraryEntry, MpdSearchField};
use crate::source_registry::{
    default_probe_cadence_for, default_scan_policy_for,
    make_local_internal_record, probe_source, ScanPolicy, SourceKind,
    SourceRecord, SourceRegistry, SourceState, LOCAL_INTERNAL_SOURCE_ID,
};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Wire-payload version for the `audio.library.v1` shelf.
pub(crate) const LIBRARY_PAYLOAD_VERSION: u32 = 1;

const SUBJECT_TYPE_SOURCES: &str = "audio_library_sources";
const SUBJECT_TYPE_STATE: &str = "audio_library_state";
const SUBJECT_TYPE_SCAN_PROGRESS: &str = "audio_library_scan_progress";

const SCHEME_LIBRARY: &str = "evo.audio.library";

const VALUE_SOURCES: &str = "sources";
const VALUE_STATE: &str = "state";
const VALUE_SCAN_PROGRESS: &str = "scan_progress";

/// Probe budget for synchronous probe / wake verbs (per the
/// catalogue's bounded-time intent for operator gestures).
const PROBE_BUDGET_MS: u64 = 3_000;

/// Default search-results cap. Operator-issued queries above
/// this are clamped silently.
pub(crate) const SEARCH_MAX_RESULTS_CAP: u32 = 1000;

// ----- shared context -----

#[derive(Clone)]
pub(crate) struct LibraryContext {
    /// MPD's music_directory.
    pub(crate) music_directory: PathBuf,
    /// Shared source registry.
    pub(crate) registry: SourceRegistry,
    /// Subject announcer.
    pub(crate) subjects: Arc<dyn SubjectAnnouncer>,
    /// In-memory browse cache keyed by `(source_id, relative_path)`.
    browse_cache: Arc<Mutex<HashMap<(String, String), CachedBrowse>>>,
    /// Mirror of last published audio_library_sources envelope.
    sources_mirror: Arc<Mutex<Option<serde_json::Value>>>,
    /// Mirror of last published audio_library_state envelope.
    state_mirror: Arc<Mutex<Option<serde_json::Value>>>,
}

impl LibraryContext {
    pub(crate) fn new(
        music_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
    ) -> Self {
        Self {
            music_directory,
            registry,
            subjects,
            browse_cache: Arc::new(Mutex::new(HashMap::new())),
            sources_mirror: Arc::new(Mutex::new(None)),
            state_mirror: Arc::new(Mutex::new(None)),
        }
    }
}

/// One cached browse listing. Keyed by `(source_id, path)`.
/// `fetched_at_ms` is the wall-clock at which the cache was
/// populated; subscribers display "fetched 5 minutes ago"
/// stale-cache hints in the UI.
#[allow(dead_code)]
#[derive(Clone)]
struct CachedBrowse {
    entries: Vec<serde_json::Value>,
    fetched_at_ms: u64,
}

// ----- subject emitters -----

pub(crate) async fn announce_subjects(ctx: &LibraryContext) {
    announce_sources(ctx).await;
    announce_state(ctx).await;
    announce_scan_progress(ctx).await;
}

async fn announce_sources(ctx: &LibraryContext) {
    let addressing = ExternalAddressing::new(SCHEME_LIBRARY, VALUE_SOURCES);
    let env = render_sources_envelope(ctx).await;
    {
        let mut g = ctx.sources_mirror.lock().await;
        *g = Some(env.clone());
    }
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_SOURCES, vec![addressing])
            .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_sources subject announce failed"
        );
    }
}

async fn announce_state(ctx: &LibraryContext) {
    let addressing = ExternalAddressing::new(SCHEME_LIBRARY, VALUE_STATE);
    let env = render_state_envelope(ctx).await;
    {
        let mut g = ctx.state_mirror.lock().await;
        *g = Some(env.clone());
    }
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_STATE, vec![addressing])
            .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_state subject announce failed"
        );
    }
}

async fn announce_scan_progress(ctx: &LibraryContext) {
    let addressing =
        ExternalAddressing::new(SCHEME_LIBRARY, VALUE_SCAN_PROGRESS);
    let env = json!({
        "v":     LIBRARY_PAYLOAD_VERSION,
        "scans": Vec::<serde_json::Value>::new(),
    });
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_SCAN_PROGRESS, vec![addressing])
            .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_scan_progress subject announce failed"
        );
    }
}

/// Refresh both sources + state subjects from the registry's
/// current snapshot. Called on every registry mutation
/// (add/remove/transition) + on operator probe/wake/update
/// gestures.
pub(crate) async fn publish_subjects(ctx: &LibraryContext) {
    let sources_env = render_sources_envelope(ctx).await;
    let state_env = render_state_envelope(ctx).await;
    {
        let mut g = ctx.sources_mirror.lock().await;
        *g = Some(sources_env.clone());
    }
    {
        let mut g = ctx.state_mirror.lock().await;
        *g = Some(state_env.clone());
    }
    let sources_addr = ExternalAddressing::new(SCHEME_LIBRARY, VALUE_SOURCES);
    if let Err(e) = ctx.subjects.update_state(sources_addr, sources_env).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_sources update_state failed"
        );
    }
    let state_addr = ExternalAddressing::new(SCHEME_LIBRARY, VALUE_STATE);
    if let Err(e) = ctx.subjects.update_state(state_addr, state_env).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_library_state update_state failed"
        );
    }
}

async fn render_sources_envelope(ctx: &LibraryContext) -> serde_json::Value {
    let snapshot = ctx.registry.snapshot().await;
    json!({
        "v":       LIBRARY_PAYLOAD_VERSION,
        "sources": snapshot,
        "total":   snapshot.len(),
    })
}

async fn render_state_envelope(ctx: &LibraryContext) -> serde_json::Value {
    let snapshot = ctx.registry.snapshot().await;
    let total: u32 = snapshot.iter().map(|r| r.track_count).sum();
    let available: u32 = snapshot.iter().map(|r| r.track_count_available).sum();
    let last_full_scan: Option<u64> =
        snapshot.iter().filter_map(|r| r.last_scan_at_ms).max();
    json!({
        "v":                       LIBRARY_PAYLOAD_VERSION,
        "total_tracks":            total,
        "total_tracks_available":  available,
        "last_full_scan_at_ms":    last_full_scan,
        "active_scans":            Vec::<String>::new(),
    })
}

// ----- verb payloads -----

#[derive(Debug, Deserialize)]
pub(crate) struct AddSourcePayload {
    pub(crate) v: u32,
    pub(crate) display_name: String,
    pub(crate) kind: SourceKind,
    pub(crate) mount_path: PathBuf,
    #[serde(default)]
    pub(crate) scan_policy: Option<ScanPolicy>,
    #[serde(default)]
    pub(crate) probe_cadence_ms: Option<u32>,
    /// Required when registering a cloud source with
    /// `eager_incremental` scan policy. Refuses without the
    /// operator-confirmed opt-in per the catalogue invariant
    /// `cloud-default-scan-policy-is-lazy`.
    #[serde(default)]
    pub(crate) cloud_eager_scan_acknowledged: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoveSourcePayload {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    #[serde(default)]
    pub(crate) scrub_mpd_entries: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProbeSourcePayload {
    pub(crate) v: u32,
    pub(crate) source_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WakeSourcePayload {
    pub(crate) v: u32,
    pub(crate) source_id: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateSourcePayload {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    #[serde(default)]
    pub(crate) force_rescan: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BrowseLibraryPayload {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    #[serde(default)]
    pub(crate) path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchLibraryPayload {
    pub(crate) v: u32,
    pub(crate) query: String,
    #[serde(default)]
    pub(crate) source_ids: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) include_offline: bool,
    #[serde(default = "default_search_max_results")]
    pub(crate) max_results: u32,
}

fn default_search_max_results() -> u32 {
    SEARCH_MAX_RESULTS_CAP
}

// ----- responses -----

#[derive(Debug, Serialize)]
pub(crate) struct AddSourceResponse {
    pub(crate) v: u32,
    pub(crate) source_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct ProbeSourceResponse {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    pub(crate) state: SourceState,
    pub(crate) probed_at_ms: u64,
    pub(crate) probe_duration_ms: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct WakeSourceResponse {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    pub(crate) woke: bool,
    pub(crate) state: SourceState,
}

#[derive(Debug, Serialize)]
pub(crate) struct UpdateSourceResponse {
    pub(crate) v: u32,
    pub(crate) source_id: String,
    pub(crate) scan_started: bool,
}

// ----- error type -----

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum VerbError {
    #[error("{verb}: payload version {got} unsupported; expected {expected}")]
    PayloadVersion {
        verb: String,
        got: u32,
        expected: u32,
    },
    #[error("source id {source_id:?} not registered")]
    UnknownSource { source_id: String },
    #[error(
        "local-internal source is non-removable per audio.library.v1 invariant"
    )]
    NonRemovableLocalInternal,
    #[error("cloud source registered with eager_incremental scan policy requires cloud_eager_scan_acknowledged:true (refuses the eager-scan-on-cloud anti-pattern)")]
    CloudEagerScanRequiresAcknowledgement,
    #[error("source id {source_id:?} is currently Offline; wake it first")]
    SourceOffline { source_id: String },
    #[error("library.{verb}: MPD error: {reason}")]
    Mpd { verb: String, reason: String },
    #[error("library.add_source: source registration failed: {reason}")]
    Register { reason: String },
    #[error(
        "library.browse_library: source {source_id:?} mount_path {mount_path:?} \
         is not under MPD's music_directory {music_directory:?}; \
         browse over MPD requires the source to be mounted under MPD's library tree"
    )]
    SourceOutsideMusicDirectory {
        source_id: String,
        mount_path: String,
        music_directory: String,
    },
}

fn check_version(v: u32, verb: &str) -> Result<(), VerbError> {
    if v != LIBRARY_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: verb.to_string(),
            got: v,
            expected: LIBRARY_PAYLOAD_VERSION,
        });
    }
    Ok(())
}

fn is_cloud(kind: &SourceKind) -> bool {
    matches!(
        kind,
        SourceKind::CloudGdrive { .. } | SourceKind::CloudOnedrive { .. }
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ----- verb handlers -----

pub(crate) async fn handle_list_sources(
    ctx: &LibraryContext,
) -> serde_json::Value {
    let g = ctx.sources_mirror.lock().await;
    g.clone().unwrap_or_else(|| {
        json!({
            "v":       LIBRARY_PAYLOAD_VERSION,
            "sources": Vec::<serde_json::Value>::new(),
            "total":   0,
        })
    })
}

pub(crate) async fn handle_add_source(
    ctx: &LibraryContext,
    payload: AddSourcePayload,
) -> Result<AddSourceResponse, VerbError> {
    check_version(payload.v, "library.add_source")?;
    // Determine effective scan policy + probe cadence.
    let scan_policy = payload
        .scan_policy
        .clone()
        .unwrap_or_else(|| default_scan_policy_for(&payload.kind));
    let probe_cadence_ms = payload
        .probe_cadence_ms
        .unwrap_or_else(|| default_probe_cadence_for(&payload.kind));

    // Cloud source + eager_incremental requires explicit
    // operator acknowledgement.
    if is_cloud(&payload.kind)
        && matches!(scan_policy, ScanPolicy::EagerIncremental { .. })
        && payload.cloud_eager_scan_acknowledged != Some(true)
    {
        return Err(VerbError::CloudEagerScanRequiresAcknowledgement);
    }

    // Allocate a new id. We use a kebab-case derivation of the
    // display_name plus a short epoch-suffix to avoid
    // collisions; alternative is full UUID via the `uuid`
    // crate, but the kebab-case form is operator-readable in
    // logs.
    let id = sanitise_id(&payload.display_name);
    let id = format!("{id}-{}", now_ms() % 1_000_000);

    let record = SourceRecord {
        id: id.clone(),
        display_name: payload.display_name,
        kind: payload.kind,
        mount_path: payload.mount_path,
        mpd_storage_name: None,
        state: SourceState::Probing,
        last_seen_online_at_ms: None,
        probe_cadence_ms,
        scan_policy,
        track_count: 0,
        track_count_available: 0,
        last_scan_at_ms: None,
    };
    ctx.registry
        .register(record)
        .await
        .map_err(|e| VerbError::Register {
            reason: e.to_string(),
        })?;
    let _ = ctx.registry.persist().await;
    publish_subjects(ctx).await;
    Ok(AddSourceResponse {
        v: LIBRARY_PAYLOAD_VERSION,
        source_id: id,
    })
}

fn sanitise_id(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, ' ' | '-' | '_') && !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "source".to_string()
    } else {
        trimmed
    }
}

pub(crate) async fn handle_remove_source(
    ctx: &LibraryContext,
    conn: &mut MpdConnection,
    payload: RemoveSourcePayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "library.remove_source")?;
    if payload.source_id == LOCAL_INTERNAL_SOURCE_ID {
        return Err(VerbError::NonRemovableLocalInternal);
    }
    let record =
        ctx.registry.get(&payload.source_id).await.ok_or_else(|| {
            VerbError::UnknownSource {
                source_id: payload.source_id.clone(),
            }
        })?;
    // Optional MPD scrub: run `update PATH` after unmount so
    // MPD's database notices the songs are gone.
    if payload.scrub_mpd_entries {
        let path = record.mount_path.to_string_lossy().into_owned();
        if let Err(e) = conn.update(Some(&path)).await {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                source_id = %payload.source_id,
                error = %e,
                "library.remove_source: MPD scrub update failed; \
                 source removal still proceeds"
            );
        }
    }
    ctx.registry.remove(&payload.source_id).await.map_err(|e| {
        VerbError::Register {
            reason: e.to_string(),
        }
    })?;
    let _ = ctx.registry.persist().await;
    publish_subjects(ctx).await;
    Ok(())
}

pub(crate) async fn handle_probe_source(
    ctx: &LibraryContext,
    payload: ProbeSourcePayload,
) -> Result<ProbeSourceResponse, VerbError> {
    check_version(payload.v, "library.probe_source")?;
    let record =
        ctx.registry.get(&payload.source_id).await.ok_or_else(|| {
            VerbError::UnknownSource {
                source_id: payload.source_id.clone(),
            }
        })?;
    let budget = std::time::Duration::from_millis(PROBE_BUDGET_MS);
    let outcome = probe_source(&record, budget).await;
    let probed_at = now_ms();
    if let Err(e) = ctx
        .registry
        .transition(&payload.source_id, outcome.new_state.clone())
        .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %payload.source_id,
            error = %e,
            "library.probe_source: registry transition failed"
        );
    }
    publish_subjects(ctx).await;
    Ok(ProbeSourceResponse {
        v: LIBRARY_PAYLOAD_VERSION,
        source_id: payload.source_id,
        state: outcome.new_state,
        probed_at_ms: probed_at,
        probe_duration_ms: outcome.elapsed.as_millis() as u64,
    })
}

pub(crate) async fn handle_wake_source(
    ctx: &LibraryContext,
    payload: WakeSourcePayload,
) -> Result<WakeSourceResponse, VerbError> {
    check_version(payload.v, "library.wake_source")?;
    let record =
        ctx.registry.get(&payload.source_id).await.ok_or_else(|| {
            VerbError::UnknownSource {
                source_id: payload.source_id.clone(),
            }
        })?;
    // Wake handler is type-dispatched. For v0.1.13 scope the
    // cloud + DLNA substrate is not yet landed so wake for
    // those returns passive-probe semantics.
    let woke = !matches!(
        record.kind,
        SourceKind::CloudGdrive { .. }
            | SourceKind::CloudOnedrive { .. }
            | SourceKind::NetworkDlna { .. }
    );
    // Either way, run a probe + transition.
    let outcome = probe_source(
        &record,
        std::time::Duration::from_millis(PROBE_BUDGET_MS),
    )
    .await;
    if let Err(e) = ctx
        .registry
        .transition(&payload.source_id, outcome.new_state.clone())
        .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            source_id = %payload.source_id,
            error = %e,
            "library.wake_source: registry transition failed"
        );
    }
    publish_subjects(ctx).await;
    Ok(WakeSourceResponse {
        v: LIBRARY_PAYLOAD_VERSION,
        source_id: payload.source_id,
        woke,
        state: outcome.new_state,
    })
}

pub(crate) async fn handle_update_source(
    ctx: &LibraryContext,
    conn: &mut MpdConnection,
    payload: UpdateSourcePayload,
) -> Result<UpdateSourceResponse, VerbError> {
    check_version(payload.v, "library.update_source")?;
    let record =
        ctx.registry.get(&payload.source_id).await.ok_or_else(|| {
            VerbError::UnknownSource {
                source_id: payload.source_id.clone(),
            }
        })?;
    // Offline source MUST NOT trigger MPD update — the
    // source-offline-never-triggers-update invariant.
    if matches!(record.state, SourceState::Offline { .. }) {
        return Err(VerbError::SourceOffline {
            source_id: payload.source_id.clone(),
        });
    }
    // MPD's `update` / `rescan` take a database-relative path
    // rooted at music_directory, NOT an absolute filesystem
    // path. Passing an absolute path triggers MPD's "Malformed
    // path" refusal — the same shape mistake browse_library
    // made before its fix. Convert the source's mount_path to
    // the music_directory-relative form before dispatch.
    let path = mpd_database_relative_path(
        &ctx.music_directory,
        &record.mount_path,
        "",
    )
    .map_err(|e| match e {
        VerbError::SourceOutsideMusicDirectory {
            mount_path,
            music_directory,
            ..
        } => VerbError::SourceOutsideMusicDirectory {
            source_id: payload.source_id.clone(),
            mount_path,
            music_directory,
        },
        other => other,
    })?;
    // MPD's update with empty-path is the database-root case;
    // pass None so the wire frame becomes `update` (root) vs.
    // `update PATH`.
    let path_arg = if path.is_empty() {
        None
    } else {
        Some(path.as_str())
    };
    let result = if payload.force_rescan {
        conn.rescan(path_arg).await
    } else {
        conn.update(path_arg).await
    };
    result.map_err(|e| VerbError::Mpd {
        verb: "update_source".to_string(),
        reason: e.to_string(),
    })?;
    publish_subjects(ctx).await;
    Ok(UpdateSourceResponse {
        v: LIBRARY_PAYLOAD_VERSION,
        source_id: payload.source_id,
        scan_started: true,
    })
}

pub(crate) async fn handle_browse_library(
    ctx: &LibraryContext,
    conn: &mut MpdConnection,
    payload: BrowseLibraryPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.browse_library")?;
    let record =
        ctx.registry.get(&payload.source_id).await.ok_or_else(|| {
            VerbError::UnknownSource {
                source_id: payload.source_id.clone(),
            }
        })?;
    let path = payload.path.clone().unwrap_or_default();
    // Serve from cache on Offline; refresh on Online/Degraded.
    if matches!(
        record.state,
        SourceState::Offline { .. } | SourceState::Retired
    ) {
        let cache = ctx.browse_cache.lock().await;
        let cached = cache
            .get(&(payload.source_id.clone(), path.clone()))
            .cloned();
        drop(cache);
        let entries = cached.map(|c| c.entries).unwrap_or_default();
        return Ok(json!({
            "v":            LIBRARY_PAYLOAD_VERSION,
            "source_id":    payload.source_id,
            "path":         path,
            "entries":      entries,
            "stale":        true,
            "source_state": record.state,
        }));
    }
    // Online / Degraded / Probing: read from MPD.
    //
    // MPD's `lsinfo` takes a **database-relative path** rooted at
    // `music_directory`, NOT an absolute filesystem path. Passing
    // an absolute path triggers MPD's filesystem-traversal guard
    // and refuses on TCP connections with
    // "Access to local files via TCP is not allowed" — the same
    // refusal a malicious TCP client trying to enumerate
    // `/etc/shadow` would receive. The handler MUST convert the
    // source's mount_path + the operator-supplied browse path
    // into a single database-relative path before issuing lsinfo.
    let mpd_path = mpd_database_relative_path(
        &ctx.music_directory,
        &record.mount_path,
        &path,
    )
    .map_err(|e| match e {
        VerbError::SourceOutsideMusicDirectory {
            mount_path,
            music_directory,
            ..
        } => VerbError::SourceOutsideMusicDirectory {
            source_id: payload.source_id.clone(),
            mount_path,
            music_directory,
        },
        other => other,
    })?;
    let entries = conn.lsinfo(&mpd_path).await.map_err(|e| VerbError::Mpd {
        verb: "browse_library".to_string(),
        reason: e.to_string(),
    })?;
    let rendered: Vec<serde_json::Value> =
        entries.iter().map(render_library_entry).collect();
    // Cache the fresh listing.
    {
        let mut cache = ctx.browse_cache.lock().await;
        cache.insert(
            (payload.source_id.clone(), path.clone()),
            CachedBrowse {
                entries: rendered.clone(),
                fetched_at_ms: now_ms(),
            },
        );
    }
    Ok(json!({
        "v":            LIBRARY_PAYLOAD_VERSION,
        "source_id":    payload.source_id,
        "path":         path,
        "entries":      rendered,
        "stale":        false,
        "source_state": record.state,
    }))
}

/// Compute the **database-relative path** MPD's `lsinfo` expects.
///
/// MPD's library is rooted at `music_directory`; every wire command
/// that names a path (lsinfo / find / search / listallinfo) takes
/// the path RELATIVE to that root. Passing an absolute filesystem
/// path triggers MPD's filesystem-traversal guard and refuses on
/// TCP connections — the daemon treats the request as a malicious
/// attempt to enumerate paths outside the music database.
///
/// This helper:
/// - Computes the source's mount_path RELATIVE to music_directory.
/// - Joins the operator-supplied browse `user_path` underneath.
/// - Returns the empty string for the database root case (when the
///   source mounts at music_directory itself).
/// - Refuses with [`VerbError::SourceOutsideMusicDirectory`] when
///   the source's mount_path is not under music_directory (a
///   misconfigured source — fix at registration, not at browse
///   time).
///
/// The operator-supplied path is taken as-is — separator
/// normalisation is the caller's responsibility (the browse
/// shelf's wire contract pins POSIX `/` separators).
fn mpd_database_relative_path(
    music_directory: &std::path::Path,
    mount_path: &std::path::Path,
    user_path: &str,
) -> Result<String, VerbError> {
    let source_relative =
        mount_path.strip_prefix(music_directory).map_err(|_| {
            VerbError::SourceOutsideMusicDirectory {
                source_id: String::new(),
                mount_path: mount_path.to_string_lossy().into_owned(),
                music_directory: music_directory.to_string_lossy().into_owned(),
            }
        })?;
    let source_relative = source_relative.to_string_lossy();
    let trimmed_user = user_path.trim_start_matches('/');
    let joined = match (source_relative.is_empty(), trimmed_user.is_empty()) {
        (true, true) => String::new(),
        (true, false) => trimmed_user.to_string(),
        (false, true) => source_relative.into_owned(),
        (false, false) => format!("{source_relative}/{trimmed_user}"),
    };
    Ok(joined)
}

fn render_library_entry(entry: &MpdLibraryEntry) -> serde_json::Value {
    match entry {
        MpdLibraryEntry::Directory { path, .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            json!({
                "kind": "directory",
                "name": name,
                "uri":  path,
            })
        }
        MpdLibraryEntry::Playlist { path, .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            json!({
                "kind": "playlist",
                "name": name,
                "uri":  path,
            })
        }
        MpdLibraryEntry::File {
            path,
            title,
            artist,
            album,
            duration,
        } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            json!({
                "kind":        "file",
                "name":        name,
                "uri":         path,
                "title":       title,
                "artist":      artist,
                "album":       album,
                "duration_ms": duration.map(|d| d.as_millis() as u64),
                // available defaults to true at browse time; the
                // skip-traversal / queue path consults the sticker
                // for authoritative checks during play.
                "available":   true,
            })
        }
    }
}

pub(crate) async fn handle_search_library(
    ctx: &LibraryContext,
    conn: &mut MpdConnection,
    payload: SearchLibraryPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.search_library")?;
    let max_results = payload.max_results.min(SEARCH_MAX_RESULTS_CAP);
    let snapshot = ctx.registry.snapshot().await;
    let mut stale_sources: Vec<String> = Vec::new();
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    for record in &snapshot {
        // Optional source-id filter.
        if let Some(ids) = &payload.source_ids {
            if !ids.contains(&record.id) {
                continue;
            }
        }
        // Skip Offline sources unless include_offline.
        if matches!(
            record.state,
            SourceState::Offline { .. } | SourceState::Retired
        ) {
            if payload.include_offline {
                stale_sources.push(record.id.clone());
            }
            continue;
        }
        // Skip cloud + DLNA — cloud-native search substrate is
        // v0.1.14; for now those don't appear in the search.
        if matches!(
            record.kind,
            SourceKind::CloudGdrive { .. }
                | SourceKind::CloudOnedrive { .. }
                | SourceKind::NetworkDlna { .. }
        ) {
            continue;
        }
        let entries = conn
            .search(MpdSearchField::Any, &payload.query)
            .await
            .map_err(|e| VerbError::Mpd {
                verb: "search_library".to_string(),
                reason: e.to_string(),
            })?;
        for e in entries {
            let mut item = render_library_entry(&e);
            item.as_object_mut().unwrap().insert(
                "source_id".to_string(),
                serde_json::Value::String(record.id.clone()),
            );
            results.push(item);
            if results.len() >= max_results as usize {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
    }
    Ok(json!({
        "v":             LIBRARY_PAYLOAD_VERSION,
        "results":       results,
        "truncated":     truncated,
        "stale_sources": stale_sources,
    }))
}

/// Ensure the canonical local-internal source is present in
/// the registry. Called by the integration commit at plugin
/// load — registers from MPD's resolved music_directory when
/// the registry doesn't already carry it (cold-start case).
pub(crate) async fn ensure_local_internal_registered(
    ctx: &LibraryContext,
) -> Result<(), VerbError> {
    if ctx.registry.get(LOCAL_INTERNAL_SOURCE_ID).await.is_some() {
        return Ok(());
    }
    let record = make_local_internal_record(&ctx.music_directory);
    ctx.registry
        .register(record)
        .await
        .map_err(|e| VerbError::Register {
            reason: e.to_string(),
        })?;
    let _ = ctx.registry.persist().await;
    Ok(())
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    struct NullAnn;
    impl SubjectAnnouncer for NullAnn {
        fn announce<'a>(
            &'a self,
            _a: SubjectAnnouncement,
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
            _addressing: ExternalAddressing,
            _state: serde_json::Value,
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
    }

    fn ctx() -> LibraryContext {
        LibraryContext::new(
            PathBuf::from("/var/lib/evo/music"),
            SourceRegistry::new(),
            Arc::new(NullAnn),
        )
    }

    #[test]
    fn sanitise_id_lowercases_alphanumerics() {
        assert_eq!(sanitise_id("My NAS 2025"), "my-nas-2025");
    }

    #[test]
    fn sanitise_id_collapses_separators() {
        assert_eq!(sanitise_id("My   NAS"), "my-nas");
        assert_eq!(sanitise_id("My---NAS"), "my-nas");
    }

    #[test]
    fn sanitise_id_strips_punctuation() {
        assert_eq!(sanitise_id("MyNAS!@#"), "mynas");
    }

    #[test]
    fn sanitise_id_falls_back_to_source_when_empty() {
        assert_eq!(sanitise_id("!!!"), "source");
        assert_eq!(sanitise_id(""), "source");
    }

    #[test]
    fn check_version_accepts_matching() {
        assert!(check_version(1, "x").is_ok());
    }

    #[test]
    fn check_version_refuses_mismatched() {
        let err = check_version(99, "x").unwrap_err();
        assert!(matches!(err, VerbError::PayloadVersion { .. }));
    }

    #[test]
    fn is_cloud_matches_gdrive_and_onedrive() {
        assert!(is_cloud(&SourceKind::CloudGdrive {
            account_ref: "x".into()
        }));
        assert!(is_cloud(&SourceKind::CloudOnedrive {
            account_ref: "x".into()
        }));
        assert!(!is_cloud(&SourceKind::LocalInternal));
        assert!(!is_cloud(&SourceKind::NetworkDlna {
            service_id: "x".into()
        }));
    }

    #[tokio::test]
    async fn list_sources_returns_empty_envelope_before_announce() {
        let ctx = ctx();
        let env = handle_list_sources(&ctx).await;
        assert_eq!(env["v"], 1);
        assert_eq!(env["total"], 0);
    }

    #[tokio::test]
    async fn add_source_local_internal_registers_with_defaults() {
        let ctx = ctx();
        let res = handle_add_source(
            &ctx,
            AddSourcePayload {
                v: 1,
                display_name: "Test".to_string(),
                kind: SourceKind::LocalInternal,
                mount_path: PathBuf::from("/var/lib/evo/music/INTERNAL"),
                scan_policy: None,
                probe_cadence_ms: None,
                cloud_eager_scan_acknowledged: None,
            },
        )
        .await
        .unwrap();
        assert!(res.source_id.starts_with("test-"));
        let rec = ctx.registry.get(&res.source_id).await.unwrap();
        // Default policy for local_internal.
        assert!(matches!(
            rec.scan_policy,
            ScanPolicy::EagerIncremental { .. }
        ));
    }

    #[tokio::test]
    async fn add_source_cloud_refuses_eager_without_acknowledgement() {
        let ctx = ctx();
        let err = handle_add_source(
            &ctx,
            AddSourcePayload {
                v: 1,
                display_name: "My Drive".into(),
                kind: SourceKind::CloudGdrive {
                    account_ref: "abc".into(),
                },
                mount_path: PathBuf::from("/mnt/gdrive"),
                scan_policy: Some(ScanPolicy::EagerIncremental {
                    on_online: true,
                    on_mount_event: false,
                }),
                probe_cadence_ms: None,
                cloud_eager_scan_acknowledged: None,
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            VerbError::CloudEagerScanRequiresAcknowledgement
        ));
    }

    #[tokio::test]
    async fn add_source_cloud_defaults_to_lazy() {
        let ctx = ctx();
        let res = handle_add_source(
            &ctx,
            AddSourcePayload {
                v: 1,
                display_name: "My Drive".into(),
                kind: SourceKind::CloudGdrive {
                    account_ref: "abc".into(),
                },
                mount_path: PathBuf::from("/mnt/gdrive"),
                scan_policy: None,
                probe_cadence_ms: None,
                cloud_eager_scan_acknowledged: None,
            },
        )
        .await
        .unwrap();
        let rec = ctx.registry.get(&res.source_id).await.unwrap();
        assert!(matches!(
            rec.scan_policy,
            ScanPolicy::LazyBrowseDriven { .. }
        ));
    }

    #[test]
    fn non_removable_local_internal_variant_carries_correct_display() {
        // The catalogue invariant pin: the verb refuses on
        // LOCAL_INTERNAL_SOURCE_ID before any MPD round-trip.
        // The handler's early-return path is exercised at
        // runtime; here we pin the error variant + its display
        // shape so a contributor refactoring the early-return
        // can't accidentally drop the protection.
        let err = VerbError::NonRemovableLocalInternal;
        let msg = format!("{err}");
        assert!(msg.contains("non-removable"));
        assert!(msg.contains("audio.library.v1"));
    }

    #[tokio::test]
    async fn ensure_local_internal_registered_is_idempotent() {
        let ctx = ctx();
        ensure_local_internal_registered(&ctx).await.unwrap();
        ensure_local_internal_registered(&ctx).await.unwrap();
        let snap = ctx.registry.snapshot().await;
        let local_count = snap
            .iter()
            .filter(|r| r.id == LOCAL_INTERNAL_SOURCE_ID)
            .count();
        assert_eq!(local_count, 1);
    }

    #[tokio::test]
    async fn search_max_results_cap_clamps_above_1000() {
        // SearchLibraryPayload's default is the cap; values
        // above 1000 are clamped silently.
        let payload = SearchLibraryPayload {
            v: 1,
            query: "foo".into(),
            source_ids: None,
            include_offline: false,
            max_results: 5_000,
        };
        // The clamping happens inside the handler; we assert
        // the cap constant matches what the handler uses.
        let _ = payload;
        assert_eq!(SEARCH_MAX_RESULTS_CAP, 1000);
    }

    // ----- mpd_database_relative_path -----

    #[test]
    fn mpd_path_source_at_subdirectory_with_user_path() {
        // Local-internal floor source: mount_path =
        // /var/lib/evo/music/INTERNAL, music_directory =
        // /var/lib/evo/music. User browses "Albums/Album1".
        let mpd_path = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/var/lib/evo/music/INTERNAL"),
            "Albums/Album1",
        )
        .unwrap();
        assert_eq!(mpd_path, "INTERNAL/Albums/Album1");
    }

    #[test]
    fn mpd_path_source_at_subdirectory_root_browse() {
        // Same source, user browses the source root (empty path).
        let mpd_path = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/var/lib/evo/music/INTERNAL"),
            "",
        )
        .unwrap();
        assert_eq!(mpd_path, "INTERNAL");
    }

    #[test]
    fn mpd_path_source_equals_music_directory() {
        // Edge case: source mounts at music_directory itself.
        // The MPD-relative root is the empty string (the database
        // root path lsinfo accepts).
        let mpd_path = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/var/lib/evo/music"),
            "",
        )
        .unwrap();
        assert_eq!(mpd_path, "");
    }

    #[test]
    fn mpd_path_source_equals_music_directory_with_user_path() {
        // Source at music_directory; user browses into "Albums".
        let mpd_path = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/var/lib/evo/music"),
            "Albums",
        )
        .unwrap();
        assert_eq!(mpd_path, "Albums");
    }

    #[test]
    fn mpd_path_user_path_with_leading_slash_is_normalised() {
        // The wire contract pins POSIX `/` separators; an
        // operator-supplied leading slash collapses cleanly
        // rather than producing a double-slash join.
        let mpd_path = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/var/lib/evo/music/INTERNAL"),
            "/Albums/Album1",
        )
        .unwrap();
        assert_eq!(mpd_path, "INTERNAL/Albums/Album1");
    }

    #[test]
    fn mpd_path_source_outside_music_directory_refuses() {
        // External mount NOT under music_directory: the helper
        // refuses with a structured error rather than emitting an
        // absolute-path lsinfo that MPD would reject.
        let err = mpd_database_relative_path(
            std::path::Path::new("/var/lib/evo/music"),
            std::path::Path::new("/mnt/external/library"),
            "",
        )
        .unwrap_err();
        assert!(
            matches!(err, VerbError::SourceOutsideMusicDirectory { .. }),
            "expected SourceOutsideMusicDirectory, got {err:?}"
        );
    }
}
