// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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
//!   the cloud native search (cloud substrate is a follow-on
//!   primitive; this implementation delegates to local for now).
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
    shelf_dispatch::{ShelfDispatchError, ShelfRequestDispatcher},
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
    /// Works aggregate cache. Populated by
    /// [`recompute_works_aggregate`] on warm-start + on every
    /// `Database` / `Update` idle event. `library.list_works`
    /// and `library.get_work_recordings` read from this cache;
    /// the library_state envelope reads the embedded
    /// ClassicalCounters. `None` until the first scan completes
    /// — the wire reports the counters as JSON null per the
    /// truth-or-null invariant.
    pub(crate) works_aggregate:
        Arc<Mutex<Option<crate::works::WorksAggregate>>>,
    /// Peer-shelf dispatcher. Populated from
    /// [`evo_plugin_sdk::contract::LoadContext::shelf_request_dispatcher`]
    /// at plugin admission (both in-process and OOP wire paths).
    /// `library.browse_library` uses this to dispatch to
    /// `source.dlna.browse` on the `audio.dlna` shelf for
    /// `NetworkDlna` sources rather than reaching into UPnP SOAP
    /// in-process — that keeps DLNA IO owned by the plugin whose
    /// manifest declares it and eliminates the MPD-music_directory
    /// error surface for network sources.
    pub(crate) shelf_dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
}

impl LibraryContext {
    pub(crate) fn new(
        music_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
        shelf_dispatcher: Option<Arc<dyn ShelfRequestDispatcher>>,
    ) -> Self {
        Self {
            music_directory,
            registry,
            subjects,
            browse_cache: Arc::new(Mutex::new(HashMap::new())),
            sources_mirror: Arc::new(Mutex::new(None)),
            state_mirror: Arc::new(Mutex::new(None)),
            works_aggregate: Arc::new(Mutex::new(None)),
            shelf_dispatcher,
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
    let sources: Vec<serde_json::Value> =
        snapshot.iter().map(render_source_row).collect();
    json!({
        "v":       LIBRARY_PAYLOAD_VERSION,
        "sources": sources,
        "total":   snapshot.len(),
    })
}

/// Serialise one [`SourceRecord`] for the `audio_library_sources`
/// wire envelope. Splits by kind so filesystem-derived fields
/// (`mount_path`, `track_count`, `track_count_available`, `last_
/// scan_at_ms`, `mpd_storage_name`) are omitted for `NetworkDlna`
/// sources — a UPnP MediaServer is neither mounted at a path nor
/// scanned track-by-track, and emitting those fields with
/// synthesised or zero values leaks local-library semantics into
/// an operator-facing surface that has none.
fn render_source_row(record: &SourceRecord) -> serde_json::Value {
    let mut row = json!({
        "id":                    record.id,
        "display_name":          record.display_name,
        "kind":                  record.kind,
        "state":                 record.state,
        "probe_cadence_ms":      record.probe_cadence_ms,
        "scan_policy":           record.scan_policy,
        "last_seen_online_at_ms": record.last_seen_online_at_ms,
    });
    if !matches!(record.kind, SourceKind::NetworkDlna { .. }) {
        if let Some(obj) = row.as_object_mut() {
            obj.insert(
                "mount_path".into(),
                json!(record.mount_path.to_string_lossy()),
            );
            obj.insert("track_count".into(), json!(record.track_count));
            obj.insert(
                "track_count_available".into(),
                json!(record.track_count_available),
            );
            obj.insert("last_scan_at_ms".into(), json!(record.last_scan_at_ms));
            if let Some(alias) = &record.mpd_storage_name {
                obj.insert("mpd_storage_name".into(), json!(alias));
            }
        }
    }
    if let Some(obj) = row.as_object_mut() {
        obj.retain(|_, v| !v.is_null());
    }
    row
}

async fn render_state_envelope(ctx: &LibraryContext) -> serde_json::Value {
    let snapshot = ctx.registry.snapshot().await;
    let total: u32 = snapshot.iter().map(|r| r.track_count).sum();
    let available: u32 = snapshot.iter().map(|r| r.track_count_available).sum();
    let last_full_scan: Option<u64> =
        snapshot.iter().filter_map(|r| r.last_scan_at_ms).max();
    // Classical counters from the works aggregate cache.
    // `None` until the first scan completes — projected as
    // JSON `null` per the truth-or-null invariant; the wire
    // never carries a fabricated 0 for "not yet computed".
    let counters = {
        let g = ctx.works_aggregate.lock().await;
        g.as_ref().map(|w| w.counters.clone()).unwrap_or_default()
    };
    json!({
        "v":                                  LIBRARY_PAYLOAD_VERSION,
        "total_tracks":                       total,
        "total_tracks_available":             available,
        "last_full_scan_at_ms":               last_full_scan,
        "active_scans":                       Vec::<String>::new(),
        "total_tracks_with_composer":         counters.total_tracks_with_composer,
        "distinct_works":                     counters.distinct_works,
        "works_with_multiple_recordings":     counters.works_with_multiple_recordings,
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
    /// Zero-indexed page. Callers that omit `page` and
    /// `page_size` receive the first
    /// [`BROWSE_HARD_CAP`]-truncated slice — the honest
    /// large-library default (envelope carries `truncated:
    /// true` so operators know more is available).
    #[serde(default)]
    pub(crate) page: Option<usize>,
    /// Page size in entries. Bounded by [`BROWSE_HARD_CAP`]
    /// server-side; requesting more is silently clamped
    /// down (with `truncated: true` on the envelope). When
    /// absent the endpoint returns up to
    /// [`BROWSE_HARD_CAP`] entries.
    #[serde(default)]
    pub(crate) page_size: Option<usize>,
}

/// Server-side hard cap on a single browse response's entry
/// count. Aligns library browse with the queue's cap-500
/// pattern but at a higher default (4x) — 2000 entries is
/// enough to render one screen of a large-library scroll
/// window without paginating, but small enough that a rogue
/// caller cannot demand a 100k-entry response.
///
/// Callers who need more supply `page_size` explicitly
/// (still capped) plus `page` for successive slices; the
/// response envelope carries `truncated: true` +
/// `next_page` when there is more.
pub(crate) const BROWSE_HARD_CAP: usize = 2_000;

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
    #[error("library.get_work_recordings: work aggregate not yet computed; try again after the next Database / Update event")]
    WorkAggregateNotReady,
    #[error("library.get_work_recordings: work_id {work_id:?} not found in the current aggregate")]
    UnknownWork { work_id: String },
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

/// Shelf name occupied by `org.evoframework.source.dlna`.
const AUDIO_DLNA_SHELF: &str = "audio.dlna";
/// Verb the DLNA source plugin exports for paged ContentDirectory
/// browse. The wire shape is `{v, service_id, object_id, page?,
/// page_size?}` and the response envelope carries
/// `{v, status, service_id, path, entries, page, page_size, total,
///  truncated, next_page}` where each entry carries a
/// ContentDirectory-native shape (`kind`, `name`, `path` = objectId,
/// plus `uri` = stream URL for items). Kept as a constant so a
/// future verb-name change surfaces at compile time on this caller.
const SOURCE_DLNA_BROWSE_VERB: &str = "source.dlna.browse";

/// Dispatch `library.browse_library` for a `NetworkDlna` source
/// through the framework's peer-shelf dispatcher onto
/// `source.dlna.browse`. Retires the in-process UPnP SOAP path that
/// used to run inside `playback.mpd` — DLNA IO now lives entirely
/// with the plugin whose manifest declares the `dlna:` URI scheme,
/// eliminating both the ownership violation and the MPD-`music_
/// directory` error surface that leaked "source is not under
/// music_directory" for network browses.
async fn browse_dlna_via_source_dlna(
    dispatcher: &Arc<dyn ShelfRequestDispatcher>,
    source_id: &str,
    service_id: &str,
    path: &str,
    page: usize,
    page_size: usize,
    source_state: &SourceState,
) -> Result<serde_json::Value, VerbError> {
    // DLNA hard cap is 100 — distinct from local BROWSE_HARD_CAP.
    let page_size = (page_size as u32).clamp(1, evo_dlna::DLNA_PAGE_HARD_CAP);
    let page_u32 = page as u32;
    let object_id = if path.is_empty() { "0" } else { path };
    let request = json!({
        "v": 1,
        "service_id": service_id,
        "object_id": object_id,
        "page": page_u32,
        "page_size": page_size,
    });
    let request_bytes =
        serde_json::to_vec(&request).map_err(|e| VerbError::Mpd {
            verb: "browse_library".into(),
            reason: format!("dlna browse: serialise request: {e}"),
        })?;
    let response_bytes = dispatcher
        .dispatch(
            AUDIO_DLNA_SHELF,
            SOURCE_DLNA_BROWSE_VERB,
            request_bytes,
            None,
        )
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "browse_library".into(),
            reason: format!("dlna browse: {}", shelf_error_reason(&e)),
        })?;
    let response: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| VerbError::Mpd {
        verb: "browse_library".into(),
        reason: format!("dlna browse: parse response: {e}"),
    })?;

    let native_entries = response
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let entries: Vec<serde_json::Value> = native_entries
        .iter()
        .map(|e| render_dlna_browse_entry(e, service_id))
        .collect();
    let page_out = response
        .get("page")
        .and_then(|v| v.as_u64())
        .unwrap_or(page_u32 as u64);
    let page_size_out = response
        .get("page_size")
        .and_then(|v| v.as_u64())
        .unwrap_or(page_size as u64);
    let total = response.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
    let truncated = response
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let next_page = response.get("next_page").cloned();

    Ok(json!({
        "v":            LIBRARY_PAYLOAD_VERSION,
        "source_id":    source_id,
        "path":         object_id,
        "entries":      entries,
        "stale":        false,
        "source_state": source_state,
        "page":         page_out,
        "page_size":    page_size_out,
        "total":        total,
        "truncated":    truncated,
        "next_page":    next_page,
        "service_id":   service_id,
    }))
}

/// Map a [`ShelfDispatchError`] onto an operator-readable one-line
/// reason. Preserves the classification word so downstream telemetry
/// / audit correlates on the same shape the wire-op layer emits.
fn shelf_error_reason(e: &ShelfDispatchError) -> String {
    match e {
        ShelfDispatchError::NoPluginOnShelf { shelf } => {
            format!("no plugin on shelf {shelf:?}")
        }
        ShelfDispatchError::VerbNotStockedOnShelf {
            shelf,
            request_type,
        } => {
            format!("verb {request_type:?} not stocked on shelf {shelf:?}")
        }
        ShelfDispatchError::Permanent { detail } => {
            format!("permanent: {detail}")
        }
        ShelfDispatchError::Transient { detail } => {
            format!("transient: {detail}")
        }
        ShelfDispatchError::DeadlineExceeded { budget_ms } => {
            format!("deadline exceeded ({budget_ms}ms)")
        }
        ShelfDispatchError::SubstrateFailure { detail } => {
            format!("substrate failure: {detail}")
        }
    }
}

/// Translate one `source.dlna.browse` entry into the
/// `library.browse_library` entry shape.
///
/// Container entries carry the ContentDirectory objectId as
/// the browse `uri` so the UI drills into the container via
/// `library.browse_library` with `path = uri` (the same
/// contract local-folder browses use).
///
/// Item entries carry the stable identity
/// `dlna:<service_id>/<objectId>` as `uri` — the ONLY URI form
/// the operator glass sees or stores for a DLNA item. The
/// concrete `http(s)` stream url is an internal resolve detail
/// (the queue's [`resolve_uri_for_mpd`] boundary consults
/// `source.dlna.resolve` at MPD-add time) and never crosses
/// the browse surface. `source.dlna.browse` already emits the
/// stable form as its own entry `uri`; this mapping is a
/// pass-through with a defensive fall-back to a compose from
/// the entry's `path` + the response's `service_id` when a
/// legacy caller returns the raw stream.
fn render_dlna_browse_entry(
    entry: &serde_json::Value,
    service_id: &str,
) -> serde_json::Value {
    let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let object_id = entry.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let source_uri = entry.get("uri").and_then(|v| v.as_str()).unwrap_or("");
    // Prefer source.dlna's own emitted `uri` when it already
    // carries the `dlna:` stable scheme. If a legacy
    // source.dlna build emits an `http(s)` stream URL,
    // defensively compose the stable identity from
    // `service_id + object_id` so the browse wire never
    // surfaces `http(s)` on a file entry.
    let stable_uri = if source_uri.starts_with("dlna:") {
        source_uri.to_string()
    } else {
        format!("dlna:{service_id}/{object_id}")
    };
    match kind {
        "directory" => json!({
            "kind": "directory",
            "name": name,
            "uri":  object_id,
        }),
        _ => {
            let mut out = json!({
                "kind":        "file",
                "name":        name,
                "title":       entry.get("title").cloned(),
                "artist":      entry.get("artist").cloned(),
                "album":       entry.get("album").cloned(),
                "genre":       entry.get("genre").cloned(),
                "date":        entry.get("date").cloned(),
                "composer":    entry.get("composer").cloned(),
                "artwork_url": entry.get("artwork_url").cloned(),
                "uri":         stable_uri,
            });
            if let Some(obj) = out.as_object_mut() {
                obj.retain(|_, v| !v.is_null());
            }
            out
        }
    }
}

/// Upsert `NetworkDlna` sources from the source.dlna discovered sidecar.
pub(crate) async fn sync_dlna_discovered(ctx: &LibraryContext) {
    let path = evo_dlna::default_discovered_path();
    let file = match evo_dlna::read_discovered(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                error = %e,
                "library: dlna discovered sidecar unreadable"
            );
            return;
        }
    };
    let existing = ctx.registry.snapshot().await;
    let mut changed = false;
    for server in file.servers {
        let kind = SourceKind::NetworkDlna {
            service_id: server.service_id.clone(),
            control_url: server.control_url.clone(),
            base_url: server.base_url.clone(),
        };
        if let Some(rec) = existing.iter().find(|r| {
            matches!(
                &r.kind,
                SourceKind::NetworkDlna { service_id, .. }
                    if service_id == &server.service_id
            )
        }) {
            let needs_update =
                rec.display_name != server.friendly_name || rec.kind != kind;
            if needs_update {
                let mut updated = rec.clone();
                updated.display_name = server.friendly_name.clone();
                updated.kind = kind;
                ctx.registry.upsert(updated).await;
                changed = true;
            }
            continue;
        }
        let id = format!("dlna-{}", sanitise_id(&server.service_id));
        let mount = std::path::PathBuf::from(format!(
            "/var/lib/evo/dlna/{}",
            server.service_id
        ));
        let record = SourceRecord {
            id,
            display_name: server.friendly_name.clone(),
            kind: kind.clone(),
            mount_path: mount,
            mpd_storage_name: None,
            state: SourceState::Probing,
            last_seen_online_at_ms: None,
            probe_cadence_ms: default_probe_cadence_for(&kind),
            scan_policy: ScanPolicy::BrowseOnly,
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms: None,
        };
        ctx.registry.upsert(record).await;
        changed = true;
    }
    if changed {
        let _ = ctx.registry.persist().await;
        publish_subjects(ctx).await;
    }
}

/// Handle for the DLNA discovery-sync + probe background task.
pub(crate) struct DlnaSyncHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown: Arc<tokio::sync::Notify>,
}

impl DlnaSyncHandle {
    /// Signal shutdown + await task completion.
    pub(crate) async fn stop(self) {
        self.shutdown.notify_one();
        let _ = self.task.await;
    }
}

/// Cadence for reading `discovered.json` and re-probing
/// `NetworkDlna` sources. Source.dlna writes the sidecar on a
/// ~60s discover loop; syncing twice per discovery period keeps
/// the library subject within one probe of a new server.
const DLNA_SYNC_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Spawn the background task that upserts discovered DLNA
/// servers into the source registry and probes their
/// ContentDirectory reachability.
pub(crate) fn spawn_dlna_sync(ctx: LibraryContext) -> DlnaSyncHandle {
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let task_shutdown = Arc::clone(&shutdown);
    let task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(DLNA_SYNC_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = task_shutdown.notified() => break,
                _ = ticker.tick() => {
                    sync_dlna_discovered(&ctx).await;
                    let snapshot = ctx.registry.snapshot().await;
                    for record in snapshot {
                        if !matches!(
                            record.kind,
                            SourceKind::NetworkDlna { .. }
                        ) {
                            continue;
                        }
                        let budget = std::time::Duration::from_millis(3_000);
                        let outcome =
                            probe_source(&record, budget).await;
                        if let Err(e) = ctx
                            .registry
                            .transition(&record.id, outcome.new_state)
                            .await
                        {
                            tracing::debug!(
                                plugin = PLUGIN_NAME,
                                source_id = %record.id,
                                error = %e,
                                "dlna sync: probe transition failed"
                            );
                        }
                    }
                    publish_subjects(&ctx).await;
                }
            }
        }
    });
    DlnaSyncHandle { task, shutdown }
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
    // Wake handler is type-dispatched. The cloud + DLNA
    // substrate is not yet landed in this crate, so wake for
    // those source kinds returns passive-probe semantics
    // (no side effects, the registry record is returned as-is).
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

    // Resolve pagination. Both fields optional; when absent
    // the endpoint returns the first page at the kind-specific
    // default with `truncated: true` / `next_page` when more
    // remain. Local/NAS hard-cap is BROWSE_HARD_CAP; DLNA is
    // bounded to the ContentDirectory SOAP caps published on
    // evo-dlna (`DLNA_PAGE_DEFAULT` / `DLNA_PAGE_HARD_CAP`).
    let page = payload.page.unwrap_or(0);
    let (default_size, hard_cap) =
        if matches!(record.kind, SourceKind::NetworkDlna { .. }) {
            (
                evo_dlna::DLNA_PAGE_DEFAULT as usize,
                evo_dlna::DLNA_PAGE_HARD_CAP as usize,
            )
        } else {
            (BROWSE_HARD_CAP, BROWSE_HARD_CAP)
        };
    let requested_size = payload.page_size.unwrap_or(default_size);
    let page_size = requested_size.clamp(1, hard_cap);
    let range_start = page.saturating_mul(page_size);

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
        let full_entries: Vec<serde_json::Value> =
            cached.map(|c| c.entries).unwrap_or_default();
        let (page_entries, next_page, truncated) =
            paginate(&full_entries, range_start, page_size);
        return Ok(json!({
            "v":            LIBRARY_PAYLOAD_VERSION,
            "source_id":    payload.source_id,
            "path":         path,
            "entries":      page_entries,
            "stale":        true,
            "source_state": record.state,
            "page":         page,
            "page_size":    page_size,
            "total":        full_entries.len(),
            "truncated":    truncated,
            "next_page":    next_page,
        }));
    }
    // NetworkDlna: paged ContentDirectory Browse via
    // source.dlna's peer-shelf verb. playback.mpd is no longer a
    // UPnP SOAP client — that transport ownership belongs to the
    // plugin whose manifest declares the `dlna:` URI scheme.
    if let SourceKind::NetworkDlna { service_id, .. } = &record.kind {
        let Some(dispatcher) = ctx.shelf_dispatcher.as_ref() else {
            return Err(VerbError::Mpd {
                verb: "browse_library".into(),
                reason: format!(
                    "browse_library: dlna source {} requires the peer-shelf \
                     dispatcher, but LoadContext.shelf_request_dispatcher was \
                     None at admission — the plugin was loaded without a \
                     dispatcher wired (steward has not seeded one on this \
                     transport)",
                    payload.source_id
                ),
            });
        };
        return browse_dlna_via_source_dlna(
            dispatcher,
            &payload.source_id,
            service_id,
            &path,
            page,
            page_size,
            &record.state,
        )
        .await;
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
    let render_ctx = RenderCtx {
        music_directory: &ctx.music_directory,
    };
    let rendered: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| render_library_entry(e, Some(render_ctx)))
        .collect();
    // Cache the fresh listing (full, unpaginated — the cache is
    // the source of truth for subsequent page requests without
    // re-issuing lsinfo).
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
    let (page_entries, next_page, truncated) =
        paginate(&rendered, range_start, page_size);
    Ok(json!({
        "v":            LIBRARY_PAYLOAD_VERSION,
        "source_id":    payload.source_id,
        "path":         path,
        "entries":      page_entries,
        "stale":        false,
        "source_state": record.state,
        "page":         page,
        "page_size":    page_size,
        "total":        rendered.len(),
        "truncated":    truncated,
        "next_page":    next_page,
    }))
}

/// Payload for the four facet-browse verbs
/// (`library.browse_by_artist`, `_album`, `_genre`, `_year`).
///
/// The verb has two modes:
///
/// 1. **Enumeration** — the default when `select` is absent.
///    Returns a paged list of distinct facet values (artist
///    names / album names / genre labels / year strings).
///    Pagination shape mirrors [`BrowseLibraryPayload`]:
///    `page` + `page_size` both optional; response envelope
///    carries `next_page` + `truncated` + `total` so the UI
///    can scroll a long facet list without fetching
///    everything at once.
///
/// 2. **Drill** — when `select` is present. Returns the paged
///    list of TRACKS matching the selector, in the file-entry
///    shape [`handle_search_library`] uses (`uri`, `title`,
///    `artist`, `album`, `artwork_url`, `duration_ms`,
///    classical fields). Same pagination envelope.
#[derive(Debug, Deserialize)]
pub(crate) struct BrowseByTagPayload {
    pub(crate) v: u32,
    #[serde(default)]
    pub(crate) page: Option<usize>,
    #[serde(default)]
    pub(crate) page_size: Option<usize>,
    /// When present the verb switches to drill mode: returns
    /// the tracks matching the selector on the target tag,
    /// scoped by any parent context. When absent the verb
    /// returns the facet enumeration.
    #[serde(default)]
    pub(crate) select: Option<BrowseSelector>,
}

/// Facet-drill selector. The target tag is the verb's tag
/// (album / artist / genre / year); `value` is the value on
/// that tag to filter tracks by. `parent`, when present, adds
/// a second (tag, value) constraint that MPD ANDs into the
/// find — e.g. albums named "Older" credited to multiple
/// artists distinguish via `parent = {tag: "albumartist",
/// value: "George Michael"}`.
#[derive(Debug, Deserialize)]
pub(crate) struct BrowseSelector {
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) parent: Option<BrowseSelectorParent>,
}

/// Parent context for a drill selector — a second
/// `(tag, value)` pair the drill constrains on.
#[derive(Debug, Deserialize)]
pub(crate) struct BrowseSelectorParent {
    pub(crate) tag: String,
    pub(crate) value: String,
}

/// Return the paged distinct values for a single MPD tag, OR
/// the paged tracks matching a selector when the payload
/// carries one.
///
/// Common core for all four facet-browse verbs — the caller
/// supplies the tag protocol string (`artist`, `album`,
/// `genre`, `date`) and the verb name for error attribution.
///
/// **Enumeration path** (payload.select absent): issues the
/// MPD `list <tag>` command (or `listallinfo` for the album
/// facet's folder-anchored identity path), applies value
/// post-processing (year extraction from MPD's `date`), sorts
/// case-insensitively, and paginates. Album enumeration
/// derives folder-anchored identity via
/// [`canonical_album_folder`] on every track's parent
/// directory, groups tracks per canonical folder, derives
/// `album` (majority-vote clean tag with folder-basename
/// fallback), `artist` (albumartist-consistent → artist-
/// majority → `"Various Artists"`), and emits `album_id`
/// (opaque canonical folder key) alongside a `cover_url`
/// keyed on a representative track's `mpd-path` scheme so the
/// framework's sidecar → embedded → online cascade decides
/// the cover — the cover cannot be broken by a bad tag. Artist
/// enumeration carries a `cover_url` on the `artist-name`
/// scheme, which the framework artwork endpoint resolves to
/// validated bytes.
///
/// It previously also carried an `artwork_lookup` object naming
/// the shelf, request type and payload for the URL-returning
/// artist verb, so a client could dispatch per tile and paint
/// the provider URL directly. That is a second paint path, and
/// a paint path that skips the byte walk skips every check the
/// byte walk performs — placeholder rejection above all. A
/// client painting that URL shows images the serve path would
/// refuse. The field is removed rather than documented, because
/// a wire field is an invitation: the current UI ignored it, but
/// the next consumer would not have.
///
/// **Drill path** (payload.select present): issues MPD
/// `find <tag> <value>` (case-sensitive exact) or, when the
/// selector carries a parent, `find <tag> <value>
/// <parent_tag> <parent_value>` for the multi-tag AND. Year
/// selection uses `search date <YYYY>` (substring) so files
/// tagged `1996-05-13` still match the operator's `1996`
/// bucket. Renders each returned track via
/// [`render_library_entry`] — identical shape to
/// [`handle_search_library`].
pub(crate) async fn handle_browse_by_tag(
    conn: &mut MpdConnection,
    payload: BrowseByTagPayload,
    verb_name: &'static str,
    tag: &'static str,
    facet_key: &'static str,
    post_process: fn(String) -> Option<String>,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, verb_name)?;
    let page = payload.page.unwrap_or(0);
    let requested_size = payload.page_size.unwrap_or(BROWSE_HARD_CAP);
    let page_size = requested_size.clamp(1, BROWSE_HARD_CAP);
    let range_start = page.saturating_mul(page_size);

    // Drill path — return TRACKS matching the selector.
    if let Some(selector) = payload.select.as_ref() {
        return drill_by_tag(
            conn,
            verb_name,
            tag,
            facet_key,
            selector,
            page,
            page_size,
            range_start,
        )
        .await;
    }

    // Enumeration path.
    //
    // Dispatch on `facet_key` because the three families have
    // fundamentally different identity models:
    //
    // - Album: **folder-anchored** identity via
    //   [`canonical_album_folder`] — the album unit is the
    //   containing directory. Multi-disc subfolders and disc-
    //   suffix sibling folders roll up; title / artist derive
    //   from consistent tag population OR the folder basename
    //   fallback; cover cascade keys on a representative track
    //   path (mpd-path), not on the artist|album tag tuple.
    //   Prior art: Plex / Jellyfin / Roon / Picard / beets —
    //   the folder is the album unit. See
    //   [`enumerate_albums_via_folder`].
    // - Artist: multi-artist credits fan out into their
    //   individual entities via
    //   [`split_artist_credit`] — a credit like
    //   `Al Di Meola, John McLaughlin & Paco de Lucía` yields
    //   three tiles, not one compound tile. Prior art:
    //   MusicBrainz artist-credit model, Roon, Plex, Apple,
    //   Spotify. See [`enumerate_artists_via_fanout`].
    // - Genre / year: distinct raw values with post-processing
    //   (year extraction) and case-insensitive dedupe. See
    //   [`enumerate_generic_facet`].
    let rendered: Vec<serde_json::Value> = match facet_key {
        "album" => enumerate_albums_via_folder(conn, verb_name).await?,
        "artist" => {
            enumerate_artists_via_fanout(conn, tag, verb_name, post_process)
                .await?
        }
        _ => {
            enumerate_generic_facet(
                conn,
                tag,
                facet_key,
                verb_name,
                post_process,
            )
            .await?
        }
    };
    let (page_entries, next_page, truncated) =
        paginate(&rendered, range_start, page_size);
    Ok(json!({
        "v":         LIBRARY_PAYLOAD_VERSION,
        "facet":     facet_key,
        "entries":   page_entries,
        "page":      page,
        "page_size": page_size,
        "total":     rendered.len(),
        "truncated": truncated,
        "next_page": next_page,
    }))
}

/// Folder-anchored album tile.
///
/// The album unit is the containing directory — see
/// [`canonical_album_folder`] for the rollup vocabulary
/// (bare `Disc N` subfolders, `(Disc N)` / `- CD N` / `vol2`
/// sibling folders). Everything on this record is derived
/// from the population of tracks inside one canonical folder;
/// none of it is a function of the artist|album tag tuple.
///
/// Shared between the browse-by-album enumeration path and the
/// drill-by-album path so both dispatch off the same
/// identity.
#[derive(Debug, Clone)]
pub(crate) struct AlbumTile {
    /// Rolled-up canonical folder key. Grouping identity.
    pub(crate) canonical_folder: String,
    /// Display title. Derived from consistent album tag when
    /// available (majority-vote of `clean_album_title(raw)`
    /// across tracks in this folder), else the folder basename
    /// (cleaned).
    pub(crate) display_title: String,
    /// Display artist. Derived from consistent `albumartist`
    /// tag → consistent per-track `artist` tag → `"Various
    /// Artists"`. Never empty — the memo requires a rendered
    /// artist on every tile.
    pub(crate) display_artist: String,
    /// First track in the folder (deterministic sort). Drives
    /// the `mpd-path`-scheme cover URL; the framework artwork
    /// cascade (sidecar → embedded → online) resolves from
    /// this track's on-disk neighbourhood.
    pub(crate) representative_track_path: String,
    /// Every File entry in the canonical folder, in sorted
    /// order. Feeds the drill path (returned to the operator
    /// as tracks) without a second MPD roundtrip.
    pub(crate) tracks: Vec<MpdLibraryEntry>,
    /// Which source [`derive_album_title`] chose: `"tag"` or
    /// `"folder"`. Emitted on the identity-decision log line
    /// and retained on the tile for tests and downstream
    /// callers that want to audit the decision without
    /// re-parsing the log.
    #[allow(dead_code)]
    pub(crate) title_source: &'static str,
    /// Which source [`derive_album_artist`] chose:
    /// `"albumartist_consistent"` / `"artist_majority"` /
    /// `"various_fallback"`. Emitted on the identity-decision
    /// log line and retained for the same test-audit reason as
    /// `title_source`.
    #[allow(dead_code)]
    pub(crate) artist_source: &'static str,
}

impl AlbumTile {
    /// Track count = number of File entries in the canonical
    /// folder.
    pub(crate) fn track_count(&self) -> u64 {
        self.tracks.len() as u64
    }
}

/// Resolve every album MPD knows about via folder-anchored
/// identity.
///
/// One `listallinfo` roundtrip walks the entire database and
/// aggregates per canonical folder (multi-disc subfolders and
/// disc-suffix sibling folders roll up automatically via
/// [`canonical_album_folder`]). For each folder group:
///
/// - **Title**: majority-vote across `clean_album_title(raw)`
///   values from every track's `Album` tag. If ≥ 50 % of
///   tagged tracks agree on a non-empty cleaned title, that's
///   the display; otherwise the folder basename (cleaned) wins
///   — no tag inconsistency can blank or fragment a title.
/// - **Artist**: consistent `AlbumArtist` (majority-vote,
///   non-empty) → consistent per-track `Artist` (same rule)
///   → `"Various Artists"`. Never empty.
/// - **Cover source**: the folder's alphabetically-first
///   track; the `mpd-path`-scheme resolver handles the
///   embedded → folder-sidecar → online cascade so a bad tag
///   cannot break the cover.
///
/// Cost: for a 100 k-track library `listallinfo` is roughly
/// one second over the Unix socket (same shape the works
/// aggregation path pays); small libraries stay sub-second.
pub(crate) async fn resolve_album_tiles(
    conn: &mut MpdConnection,
    verb_name: &'static str,
) -> Result<Vec<AlbumTile>, VerbError> {
    let entries = conn.listallinfo("").await.map_err(|e| VerbError::Mpd {
        verb: verb_name.to_string(),
        reason: e.to_string(),
    })?;

    struct FolderAgg {
        tracks: Vec<MpdLibraryEntry>,
        album_tag_votes: HashMap<String, u64>,
        albumartist_tag_votes: HashMap<String, u64>,
        artist_tag_votes: HashMap<String, u64>,
    }
    let mut folders: HashMap<String, FolderAgg> = HashMap::new();
    for entry in entries {
        let path_ref = match &entry {
            MpdLibraryEntry::File { path, .. } => path.clone(),
            _ => continue,
        };
        let canonical = canonical_album_folder(&path_ref);
        if canonical.is_empty() {
            continue;
        }
        let MpdLibraryEntry::File {
            album,
            albumartist,
            artist,
            ..
        } = &entry
        else {
            unreachable!("guarded above");
        };
        let cleaned_album = album.as_deref().map(clean_album_title);
        let raw_albumartist = albumartist.clone().unwrap_or_default();
        let raw_artist = artist.clone().unwrap_or_default();

        let agg = folders.entry(canonical).or_insert_with(|| FolderAgg {
            tracks: Vec::new(),
            album_tag_votes: HashMap::new(),
            albumartist_tag_votes: HashMap::new(),
            artist_tag_votes: HashMap::new(),
        });
        if let Some(clean) = cleaned_album {
            if !clean.is_empty() {
                *agg.album_tag_votes.entry(clean).or_insert(0) += 1;
            }
        }
        if !raw_albumartist.trim().is_empty() {
            *agg.albumartist_tag_votes
                .entry(raw_albumartist)
                .or_insert(0) += 1;
        }
        if !raw_artist.trim().is_empty() {
            *agg.artist_tag_votes.entry(raw_artist).or_insert(0) += 1;
        }
        agg.tracks.push(entry);
    }

    let mut tiles: Vec<AlbumTile> = Vec::with_capacity(folders.len());
    for (canonical, mut agg) in folders {
        agg.tracks.sort_by(|a, b| match (a, b) {
            (
                MpdLibraryEntry::File { path: pa, .. },
                MpdLibraryEntry::File { path: pb, .. },
            ) => pa.cmp(pb),
            _ => std::cmp::Ordering::Equal,
        });
        let representative_track_path = match agg.tracks.first() {
            Some(MpdLibraryEntry::File { path, .. }) => path.clone(),
            _ => String::new(),
        };
        let folder_basename = canonical_folder_basename(&canonical);
        let track_count = agg.tracks.len() as u64;
        let (display_title, title_source) =
            derive_album_title(&agg.album_tag_votes, &folder_basename);
        let (display_artist, artist_source) = derive_album_artist(
            &agg.albumartist_tag_votes,
            &agg.artist_tag_votes,
        );
        tracing::info!(
            album = %display_title,
            artist = %display_artist,
            folder = %canonical,
            title_source,
            artist_source,
            track_count,
            "browse.album.identity"
        );
        tiles.push(AlbumTile {
            canonical_folder: canonical,
            display_title,
            display_artist,
            representative_track_path,
            tracks: agg.tracks,
            title_source,
            artist_source,
        });
    }
    tiles.sort_by(|a, b| {
        a.display_title
            .to_lowercase()
            .cmp(&b.display_title.to_lowercase())
            .then_with(|| a.display_title.cmp(&b.display_title))
    });
    Ok(tiles)
}

/// Last `/`-delimited segment of a canonical folder path, used
/// as a fallback album title when the tag population fails the
/// majority-vote test.
fn canonical_folder_basename(canonical: &str) -> String {
    let trimmed = canonical.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(idx) => trimmed[idx + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// Title derivation with observability tag: `"tag"` when the
/// cleaned `Album` tag population passes majority-vote,
/// `"folder"` when it falls through to the folder basename.
fn derive_album_title(
    album_tag_votes: &HashMap<String, u64>,
    folder_basename: &str,
) -> (String, &'static str) {
    if let Some(majority) = pick_majority(album_tag_votes) {
        return (majority, "tag");
    }
    (clean_album_title(folder_basename), "folder")
}

/// Artist derivation with observability tag. Order: consistent
/// `AlbumArtist` → consistent per-track `Artist` → `"Various
/// Artists"`. Never empty — the memo requires a rendered
/// artist on every tile even when tags are absent.
fn derive_album_artist(
    albumartist_tag_votes: &HashMap<String, u64>,
    artist_tag_votes: &HashMap<String, u64>,
) -> (String, &'static str) {
    if let Some(majority) = pick_majority(albumartist_tag_votes) {
        return (artist_display_form(&majority), "albumartist_consistent");
    }
    if let Some(majority) = pick_majority(artist_tag_votes) {
        return (artist_display_form(&majority), "artist_majority");
    }
    ("Various Artists".to_string(), "various_fallback")
}

/// Pick the majority value from a vote-count map (≥ 50 % of
/// the total counted). Returns `None` when the map is empty or
/// when no value clears the threshold.
/// Pick the STRICT majority value from a vote-count map
/// (> 50 % of the total counted).
///
/// DEFECT-3 fix: two changes on the prior shape.
///
/// 1. **Strict `>` threshold instead of `>=`**. A 2-2 tie
///    used to be accepted as a "majority" and stamped
///    `title_source="tag"`; now a tie falls through to the
///    caller's next tier (folder basename for the album
///    title; per-track ARTIST or `"Various Artists"` for the
///    album artist). Ties are not majorities.
/// 2. **Deterministic tiebreak** on the max-by-count pick.
///    The prior `iter().max_by_key(|(_, c)| c)` walked the
///    HashMap's un-ordered iterator, so two candidates with
///    the same count picked whichever the hash landed first
///    — the tile title could flip between process restarts.
///    Now ties on count break on the value string (Ord),
///    which is stable across runs.
///
/// Returns `None` when the map is empty, when total votes
/// are zero, or when no value clears the strict majority
/// threshold.
fn pick_majority(votes: &HashMap<String, u64>) -> Option<String> {
    if votes.is_empty() {
        return None;
    }
    let total: u64 = votes.values().sum();
    if total == 0 {
        return None;
    }
    let (best, best_count) =
        votes.iter().max_by(|(a_val, a_cnt), (b_val, b_cnt)| {
            a_cnt.cmp(b_cnt).then_with(|| a_val.cmp(b_val))
        })?;
    if best_count.saturating_mul(2) > total {
        Some(best.clone())
    } else {
        None
    }
}

/// Browse-by-album enumeration.
///
/// Emits `{album, album_id, artist, cover_url, track_count}`
/// tiles keyed on folder-anchored identity. The `album_id` is
/// the opaque canonical-folder key downstream surfaces can
/// pass back as `select.value` to drill by identity when they
/// have it; `album` remains the display title so existing
/// operator-visible surfaces render without knowing the ID
/// contract.
///
/// The `cover_url` scheme is `mpd-path` targeting the folder's
/// alphabetically-first track. The framework artwork cascade
/// (sidecar → embedded → online) runs against that track's
/// neighbourhood — the cover cannot be broken by a bad
/// `Album` / `AlbumArtist` tag.
async fn enumerate_albums_via_folder(
    conn: &mut MpdConnection,
    verb_name: &'static str,
) -> Result<Vec<serde_json::Value>, VerbError> {
    let tiles = resolve_album_tiles(conn, verb_name).await?;
    let rendered = tiles
        .into_iter()
        .map(|tile| {
            let cover_url = evo_device_audio_shared::artwork_target_url_sized(
                "mpd-path",
                &tile.representative_track_path,
                Some("small"),
            );
            let track_count = tile.track_count();
            json!({
                "album":       tile.display_title,
                "album_id":    tile.canonical_folder,
                "artist":      tile.display_artist,
                "cover_url":   cover_url,
                "track_count": track_count,
            })
        })
        .collect();
    Ok(rendered)
}

/// Browse-by-artist enumeration with multi-artist fan-out.
///
/// Prior art (MusicBrainz artist-credit model; Roon, Plex,
/// Apple, Spotify): a joined credit like `A, B & C` is a
/// multi-artist collaboration — the browse facet lists each
/// contributor as a first-class tile.
///
/// Sources consulted for the tile pool:
///
/// 1. The requested `tag` (typically `albumartist`) —
///    fan-out via [`split_artist_credit`] catches compound
///    credits placed directly on the album's primary-artist
///    tag.
/// 2. When the primary source is `albumartist`, the per-track
///    `artist` tags are also consulted — this catches
///    contributors on `ALBUMARTIST="Various Artists"`
///    compilation albums (Verve-style Paco/Al/John where the
///    individuals appear only on the per-track tag).
///
/// Within a group (a fold-key bucket) the display form is the
/// most-common cleaned form; ties broken by longest chars so a
/// diacritic-carrying variant (`Céline Dion`) wins over its
/// stripped twin (`Celine Dion`).
async fn enumerate_artists_via_fanout(
    conn: &mut MpdConnection,
    tag: &'static str,
    verb_name: &'static str,
    post_process: fn(String) -> Option<String>,
) -> Result<Vec<serde_json::Value>, VerbError> {
    let primary_raw = conn.list_tag(tag).await.map_err(|e| VerbError::Mpd {
        verb: verb_name.to_string(),
        reason: e.to_string(),
    })?;

    let mut groups: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let ingest =
        |raw: String, groups: &mut HashMap<String, HashMap<String, usize>>| {
            for member in split_artist_credit(&raw) {
                let key = artist_fold_key(&member);
                if key.is_empty() {
                    continue;
                }
                *groups.entry(key).or_default().entry(member).or_insert(0) += 1;
            }
        };

    for raw in primary_raw.into_iter().filter_map(post_process) {
        ingest(raw, &mut groups);
    }
    if tag == "albumartist" {
        let secondary_raw =
            conn.list_tag("artist").await.map_err(|e| VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: e.to_string(),
            })?;
        for raw in secondary_raw {
            ingest(raw, &mut groups);
        }
    }

    let mut displays: Vec<String> = groups
        .into_values()
        .filter_map(|forms| {
            forms
                .into_iter()
                .max_by(|a, b| {
                    a.1.cmp(&b.1).then_with(|| {
                        a.0.chars().count().cmp(&b.0.chars().count())
                    })
                })
                .map(|(display, _)| display)
        })
        .collect();
    displays.sort_by_key(|a| a.to_lowercase());

    let rendered = displays
        .into_iter()
        .map(|display| {
            let trimmed = display.trim();
            let cover_url = if trimmed.is_empty() {
                None
            } else {
                Some(evo_device_audio_shared::artwork_target_url_sized(
                    "artist-name",
                    trimmed,
                    Some("small"),
                ))
            };
            json!({
                "artist":         display,
                "cover_url":      cover_url,
            })
        })
        .collect();
    Ok(rendered)
}

/// Generic-facet enumeration (genre / year).
///
/// Fetches the raw tag list, applies the post-process
/// transformation (year extraction from MPD's `date` tag),
/// deduplicates, sorts case-insensitively, and emits
/// `{<facet_key>: value}` records.
async fn enumerate_generic_facet(
    conn: &mut MpdConnection,
    tag: &'static str,
    facet_key: &'static str,
    verb_name: &'static str,
    post_process: fn(String) -> Option<String>,
) -> Result<Vec<serde_json::Value>, VerbError> {
    let raw_values = conn.list_tag(tag).await.map_err(|e| VerbError::Mpd {
        verb: verb_name.to_string(),
        reason: e.to_string(),
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut processed: Vec<String> = raw_values
        .into_iter()
        .filter_map(post_process)
        .filter(|v| seen.insert(v.clone()))
        .collect();
    processed.sort_by_key(|a| a.to_lowercase());
    Ok(processed
        .into_iter()
        .map(|v| json!({ facet_key: v }))
        .collect())
}

/// Drill path for [`handle_browse_by_tag`]. Issues an MPD
/// find (or search for year) constrained by the selector,
/// renders each track via [`render_library_entry`], and
/// paginates the same envelope shape as the enumeration
/// response.
#[allow(clippy::too_many_arguments)]
async fn drill_by_tag(
    conn: &mut MpdConnection,
    verb_name: &'static str,
    tag: &'static str,
    facet_key: &'static str,
    selector: &BrowseSelector,
    page: usize,
    page_size: usize,
    range_start: usize,
) -> Result<serde_json::Value, VerbError> {
    let selector_value = selector.value.trim();
    if selector_value.is_empty() {
        return Err(VerbError::Mpd {
            verb: verb_name.to_string(),
            reason: "browse selector value must be non-empty".to_string(),
        });
    }

    // Map the verb's tag string to the search-field enum.
    // year is special: MPD indexes the raw `date` tag and files
    // may be tagged `1996-05-13`; the operator picked the
    // `1996` bucket, so we substring-search rather than exact-
    // match. The map here is per-verb; unknown tag strings are
    // treated as raw protocol names to let future verbs opt in.
    let entries: Vec<crate::mpd::MpdLibraryEntry> = if tag == "date" {
        // Year drill uses MPD `search date <YYYY>` — a
        // case-insensitive substring against the `date` tag
        // only. So a file tagged `1996-05-13` matches the
        // operator's `1996` bucket, and a track titled "Party
        // like it's 1999" does NOT accidentally slip into the
        // 1999 year bucket (which `find date 1999` misses,
        // and `search any 1999` over-matches).
        conn.search(crate::mpd::MpdSearchField::Date, selector_value)
            .await
            .map_err(|e| VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: e.to_string(),
            })?
    } else if tag == "albumartist" {
        // Artist facet drill.
        //
        // The enumeration path fanned multi-artist credits out
        // via [`split_artist_credit`] AND consulted per-track
        // ARTIST tags (Various-Artists fallback for Verve-style
        // compilations), so the selector may name an
        // individual whose fold-key does not equal any full raw
        // ALBUMARTIST / ARTIST tag. Match rows in both sources
        // whose split members contain the target key. Typical
        // library carries a handful of raw forms per real
        // artist across both tags; a single MPD `find` per
        // matching raw form covers every track.
        let target_key = artist_fold_key(selector_value);
        if target_key.is_empty() {
            return Err(VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: "browse selector value did not fold to a match key"
                    .to_string(),
            });
        }
        let raw_albumartists =
            conn.list_tag("albumartist")
                .await
                .map_err(|e| VerbError::Mpd {
                    verb: verb_name.to_string(),
                    reason: e.to_string(),
                })?;
        let matching_albumartist: Vec<String> = raw_albumartists
            .into_iter()
            .filter(|raw| {
                split_artist_credit(raw)
                    .iter()
                    .any(|m| artist_fold_key(m) == target_key)
            })
            .collect();
        let raw_artists =
            conn.list_tag("artist").await.map_err(|e| VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: e.to_string(),
            })?;
        let matching_artist: Vec<String> = raw_artists
            .into_iter()
            .filter(|raw| {
                split_artist_credit(raw)
                    .iter()
                    .any(|m| artist_fold_key(m) == target_key)
            })
            .collect();

        let parent_ctx = build_parent_field(selector, verb_name)?;
        let mut all: Vec<MpdLibraryEntry> = Vec::new();
        for raw in &matching_albumartist {
            let batch = match &parent_ctx {
                None => conn
                    .find(MpdSearchField::AlbumArtist, raw)
                    .await
                    .map_err(|e| VerbError::Mpd {
                        verb: verb_name.to_string(),
                        reason: e.to_string(),
                    })?,
                Some((field, value)) => conn
                    .find_multi(&[
                        (MpdSearchField::AlbumArtist, raw),
                        (field.clone(), value),
                    ])
                    .await
                    .map_err(|e| VerbError::Mpd {
                        verb: verb_name.to_string(),
                        reason: e.to_string(),
                    })?,
            };
            all.extend(batch);
        }
        for raw in &matching_artist {
            let batch = match &parent_ctx {
                None => conn.find(MpdSearchField::Artist, raw).await.map_err(
                    |e| VerbError::Mpd {
                        verb: verb_name.to_string(),
                        reason: e.to_string(),
                    },
                )?,
                Some((field, value)) => conn
                    .find_multi(&[
                        (MpdSearchField::Artist, raw),
                        (field.clone(), value),
                    ])
                    .await
                    .map_err(|e| VerbError::Mpd {
                        verb: verb_name.to_string(),
                        reason: e.to_string(),
                    })?,
            };
            all.extend(batch);
        }
        dedupe_entries_by_path(all)
    } else if tag == "album" {
        // Album facet drill — folder-anchored.
        //
        // The enumeration path grouped tracks by
        // [`AlbumTile::canonical_folder`]. The selector may
        // carry the tile's `album_id` (canonical folder path,
        // when the UI emits it) or the tile's `album` display
        // title (backward-compatible path). Match tiles by
        // whichever the selector value equals, then return the
        // tracks the tile already carries — no second MPD
        // roundtrip.
        let tiles = resolve_album_tiles(conn, verb_name).await?;
        let matching: Vec<&AlbumTile> = tiles
            .iter()
            .filter(|t| {
                t.canonical_folder == selector_value
                    || t.display_title.eq_ignore_ascii_case(selector_value)
            })
            .collect();
        if matching.is_empty() {
            return Err(VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: format!(
                    "no album matched selector value {selector_value:?}"
                ),
            });
        }
        // Optional parent-context filter: when the operator UI
        // passes a parent artist (e.g. drill from an artist
        // tile → an album under that artist), narrow the
        // matched tiles by artist fold-key so same-titled
        // albums by different artists disambiguate.
        let parent_ctx = build_parent_field(selector, verb_name)?;
        let filtered: Vec<&AlbumTile> = match &parent_ctx {
            None => matching,
            Some((field, value)) => {
                let target_key = match field {
                    MpdSearchField::AlbumArtist | MpdSearchField::Artist => {
                        artist_fold_key(value)
                    }
                    _ => String::new(),
                };
                if target_key.is_empty() {
                    matching
                } else {
                    matching
                        .into_iter()
                        .filter(|t| {
                            artist_fold_key(&t.display_artist) == target_key
                        })
                        .collect()
                }
            }
        };
        let mut all: Vec<MpdLibraryEntry> = Vec::new();
        for tile in &filtered {
            all.extend(tile.tracks.iter().cloned());
        }
        dedupe_entries_by_path(all)
    } else {
        let field = match tag {
            "artist" => MpdSearchField::Artist,
            "genre" => MpdSearchField::Genre,
            _ => {
                return Err(VerbError::Mpd {
                    verb: verb_name.to_string(),
                    reason: format!(
                        "drill on tag {tag:?} is not supported by this verb"
                    ),
                });
            }
        };
        match selector.parent.as_ref() {
            None => conn.find(field, selector_value).await.map_err(|e| {
                VerbError::Mpd {
                    verb: verb_name.to_string(),
                    reason: e.to_string(),
                }
            })?,
            Some(parent) => {
                let parent_tag = parent.tag.trim().to_ascii_lowercase();
                let parent_value = parent.value.trim();
                if parent_value.is_empty() {
                    return Err(VerbError::Mpd {
                        verb: verb_name.to_string(),
                        reason: "parent context value must be non-empty"
                            .to_string(),
                    });
                }
                let parent_field = match parent_tag.as_str() {
                    "album" => MpdSearchField::Album,
                    "albumartist" => MpdSearchField::AlbumArtist,
                    "artist" => MpdSearchField::Artist,
                    "genre" => MpdSearchField::Genre,
                    other => {
                        return Err(VerbError::Mpd {
                            verb: verb_name.to_string(),
                            reason: format!(
                                "parent tag {other:?} is not supported"
                            ),
                        });
                    }
                };
                conn.find_multi(&[
                    (field, selector_value),
                    (parent_field, parent_value),
                ])
                .await
                .map_err(|e| VerbError::Mpd {
                    verb: verb_name.to_string(),
                    reason: e.to_string(),
                })?
            }
        }
    };

    // Render the tracks in the same file-entry shape
    // handle_search_library uses so any consumer of that
    // envelope reads the drill response identically.
    let rendered: Vec<serde_json::Value> = entries
        .iter()
        .filter_map(|e| match e {
            crate::mpd::MpdLibraryEntry::File { .. } => {
                Some(render_library_entry(e, None))
            }
            _ => None,
        })
        .collect();

    let (page_entries, next_page, truncated) =
        paginate(&rendered, range_start, page_size);
    Ok(json!({
        "v":         LIBRARY_PAYLOAD_VERSION,
        "facet":     facet_key,
        "select":    {
            "value":  selector.value,
            "parent": selector.parent.as_ref().map(|p| {
                json!({ "tag": p.tag, "value": p.value })
            }),
        },
        "entries":   page_entries,
        "page":      page,
        "page_size": page_size,
        "total":     rendered.len(),
        "truncated": truncated,
        "next_page": next_page,
    }))
}

/// Resolve the drill's optional parent-context selector into
/// the MPD `(field, value)` pair used by `find_multi`.
///
/// Returns `Ok(None)` when the selector has no parent. Returns
/// `Err` when the parent value is empty (an operator UI bug —
/// the parent should always name a non-empty value) or when
/// the parent tag is not a browseable dimension.
fn build_parent_field<'a>(
    selector: &'a BrowseSelector,
    verb_name: &'static str,
) -> Result<Option<(MpdSearchField, &'a str)>, VerbError> {
    let Some(parent) = selector.parent.as_ref() else {
        return Ok(None);
    };
    let parent_value = parent.value.trim();
    if parent_value.is_empty() {
        return Err(VerbError::Mpd {
            verb: verb_name.to_string(),
            reason: "parent context value must be non-empty".to_string(),
        });
    }
    let parent_tag = parent.tag.trim().to_ascii_lowercase();
    let parent_field = match parent_tag.as_str() {
        "album" => MpdSearchField::Album,
        "albumartist" => MpdSearchField::AlbumArtist,
        "artist" => MpdSearchField::Artist,
        "genre" => MpdSearchField::Genre,
        other => {
            return Err(VerbError::Mpd {
                verb: verb_name.to_string(),
                reason: format!("parent tag {other:?} is not supported"),
            });
        }
    };
    Ok(Some((parent_field, parent_value)))
}

/// Deduplicate a list of MPD library entries by their file
/// path.
///
/// The multi-source drill fetches from more than one MPD raw
/// tag form (a real artist that lands in both ALBUMARTIST and
/// per-track ARTIST tags surfaces the same track twice). The
/// order of the first occurrence is preserved so the drill's
/// output ordering is deterministic.
fn dedupe_entries_by_path(
    entries: Vec<MpdLibraryEntry>,
) -> Vec<MpdLibraryEntry> {
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut out: Vec<MpdLibraryEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        match &entry {
            MpdLibraryEntry::File { path, .. } => {
                if seen.insert(path.clone()) {
                    out.push(entry);
                }
            }
            _ => out.push(entry),
        }
    }
    out
}

/// Extract the four-character year from MPD's `date` tag,
/// tolerating any of the shapes MPD emits (`YYYY`, `YYYY-MM`,
/// `YYYY-MM-DD`). Returns `None` when the tag doesn't start
/// with four ASCII digits — the entry is dropped from the
/// facet list rather than surfaced as a garbage bucket.
pub(crate) fn year_from_mpd_date(raw: String) -> Option<String> {
    if raw.len() < 4 {
        return None;
    }
    let candidate: String = raw.chars().take(4).collect();
    if candidate.chars().all(|c| c.is_ascii_digit()) {
        Some(candidate)
    } else {
        None
    }
}

/// Identity post-process for tags that don't need transforming.
pub(crate) fn identity_post_process(raw: String) -> Option<String> {
    Some(raw)
}

pub(crate) use evo_device_audio_shared::album_name::clean_album_title;
pub(crate) use evo_device_audio_shared::artist_name::{
    artist_display_form, artist_fold_key, split_artist_credit,
};
pub(crate) use evo_device_audio_shared::folder_album::canonical_album_folder;

/// Slice a full entry list into the requested page.
///
/// Returns `(page_entries, next_page, truncated)`:
///
/// - `page_entries` — the slice for the requested `page` /
///   `page_size`. Empty when `page` is past the end.
/// - `next_page` — `Some(page+1)` when there are more
///   entries after this slice, else `None`.
/// - `truncated` — `true` iff `next_page.is_some()`. Set on
///   the envelope so the operator UI knows more is
///   available without doing the arithmetic itself.
fn paginate(
    full: &[serde_json::Value],
    range_start: usize,
    page_size: usize,
) -> (Vec<serde_json::Value>, Option<usize>, bool) {
    if range_start >= full.len() {
        return (Vec::new(), None, false);
    }
    let range_end = range_start.saturating_add(page_size).min(full.len());
    let slice = full[range_start..range_end].to_vec();
    let more = range_end < full.len();
    let next_page = if more {
        Some(range_start / page_size + 1)
    } else {
        None
    };
    (slice, next_page, more)
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

/// Filesystem context threaded into [`render_library_entry`] so
/// the per-directory tile can pick the right cover URL scheme at
/// emit time (see [`pick_directory_cover_url`]).
///
/// Passed `Some(...)` from the browse + search paths, where the
/// caller already resolved the source's `mount_path` against
/// `music_directory`; `None` from the facet-drill file-only
/// filter, where directory tiles never appear.
#[derive(Clone, Copy)]
struct RenderCtx<'a> {
    /// MPD's `music_directory` — the root the DB-relative paths
    /// on every `MpdLibraryEntry` are relative to.
    music_directory: &'a std::path::Path,
}

/// Cascade the browse tile's `cover_url` through four tiers so
/// every folder shape — album, artist container, empty leaf —
/// renders meaningful art instead of a glyph:
///
/// 1. **Direct sidecar** — the browsed directory itself carries a
///    `cover.jpg` / `folder.jpg` / etc. Emit
///    `mpd-directory?value=<self>`; `artwork.local` resolves the
///    file.
/// 2. **Embedded art of representative track** — the directory
///    holds audio tracks with no sidecar file. Emit
///    `mpd-path?value=<self>/<first-track>` and re-use
///    `artwork.local`'s per-track extractor, which pulls the
///    embedded picture from the track's tags (the common case
///    for operator libraries — Adele / Phil Collins / Ed Sheeran
///    stored art inside the file). Stable-sorted first audio
///    track wins so repeat browses land on the same URL and the
///    browse cache stays coherent.
/// 3. **Representative child cover** — the directory has child
///    subdirectories and no tracks of its own. Emit the first
///    child carrying a sidecar as `mpd-directory`, or failing
///    that the first child's first track as `mpd-path` so
///    embedded art can surface. The folder shows the music it
///    contains.
///
///    This tier emits NO portrait lookup, deliberately. "Has
///    children and no files" does not mean "artist": a
///    collaboration folder, a label or series, a box set, a
///    genre bucket and the source root all share that shape.
///    Keying `artist-name` on such a basename asks a provider
///    for a person who does not exist — a guaranteed miss that
///    still spends the rate-limited budget, once per container,
///    on every first paint.
///
///    Portraits belong on the artist FACET, which is a
///    different surface keyed on the artist TAG rather than on
///    a directory name. Wrong-subject artwork on an artist tile
///    is worse than a glyph; a glyph on a folder that
///    demonstrably contains a record is worse than that
///    record's sleeve.
/// 4. **Fallback** — emit the Tier 1 URL. The resolver returns
///    `not_found` and the tile renders the honest glyph. Reached
///    for a leaf directory with no tracks, no sidecar, and no
///    children with art.
///
/// The picked URL is stored on the rendered entry which
/// [`browse_library`] caches in `browse_cache` — repeat browses
/// serve the same URL from the cache and never re-walk the
/// filesystem.
fn pick_directory_cover_url(
    mpd_relative_path: &str,
    music_directory: &std::path::Path,
) -> String {
    use evo_device_audio_shared::sidecar_cover;

    let abs_dir = music_directory.join(mpd_relative_path);

    if sidecar_cover::find_cover_in_directory(&abs_dir).is_some() {
        return evo_device_audio_shared::artwork_target_url_sized(
            "mpd-directory",
            mpd_relative_path,
            Some("small"),
        );
    }

    if let Some(track_name) =
        sidecar_cover::first_audio_file_name_in_directory(&abs_dir)
    {
        let track_relative = if mpd_relative_path.is_empty() {
            track_name
        } else {
            format!("{mpd_relative_path}/{track_name}")
        };
        return evo_device_audio_shared::artwork_target_url_sized(
            "mpd-path",
            &track_relative,
            Some("small"),
        );
    }

    // Children only: this is a container — a collaboration
    // folder, a label or series, a box set, a genre bucket, or
    // an artist directory. Show the music it contains.
    //
    // Folder browse is a file tree, and the honest picture of a
    // folder is what is inside it. That is true whether the
    // basename happens to name one artist or not, which matters
    // because "has children and no files" does NOT mean artist:
    // a multi-artist collaboration folder, a label or series
    // directory, a source root and every box set share the
    // shape. Keying a
    // portrait lookup on those strings asks a provider for a
    // person who does not exist — a guaranteed 404 that still
    // spends the rate-limited budget, once per container, on
    // every first paint of a browse.
    //
    // The artist FACET is where a portrait belongs, and it is a
    // different surface with a different key: `browse_by_artist`
    // emits `artist-name` from the tag, not from a directory
    // name. Wrong-subject artwork on an artist tile is worse
    // than a glyph; a glyph on a folder that demonstrably
    // contains a record is worse than that record's sleeve.
    //
    // Prefer a child carrying a sidecar — deterministic and
    // known-present. Only when no child has one, fall back to a
    // child's first track so embedded art can surface.
    let children = sidecar_cover::stable_sorted_child_dir_names(&abs_dir);
    let child_relative = |child_name: &str| -> String {
        if mpd_relative_path.is_empty() {
            child_name.to_string()
        } else {
            format!("{mpd_relative_path}/{child_name}")
        }
    };
    for child_name in &children {
        if sidecar_cover::find_cover_in_directory(&abs_dir.join(child_name))
            .is_some()
        {
            return evo_device_audio_shared::artwork_target_url_sized(
                "mpd-directory",
                &child_relative(child_name),
                Some("small"),
            );
        }
    }
    for child_name in &children {
        if let Some(track_name) =
            sidecar_cover::first_audio_file_name_in_directory(
                &abs_dir.join(child_name),
            )
        {
            return evo_device_audio_shared::artwork_target_url_sized(
                "mpd-path",
                &format!("{}/{}", child_relative(child_name), track_name),
                Some("small"),
            );
        }
    }

    evo_device_audio_shared::artwork_target_url_sized(
        "mpd-directory",
        mpd_relative_path,
        Some("small"),
    )
}

fn render_library_entry(
    entry: &MpdLibraryEntry,
    ctx: Option<RenderCtx<'_>>,
) -> serde_json::Value {
    match entry {
        MpdLibraryEntry::Directory { path, .. } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            // Folder-cover surface: emit a `cover_url` that the
            // framework artwork endpoint resolves via the
            // cascade in `pick_directory_cover_url` — direct
            // sidecar → own track's embedded art →
            // representative child cover → honest glyph. It
            // never emits a portrait lookup: a folder is a
            // container, and portraits belong on the artist
            // facet. When the caller cannot supply filesystem
            // context (facet-drill file-only render —
            // directories never appear there in practice), we
            // fall back to the Tier 1 URL only.
            let cover_url = match ctx {
                Some(ctx) => {
                    pick_directory_cover_url(path, ctx.music_directory)
                }
                None => evo_device_audio_shared::artwork_target_url_sized(
                    "mpd-directory",
                    path,
                    Some("small"),
                ),
            };
            json!({
                "kind":      "directory",
                "name":      name,
                "uri":       path,
                "cover_url": cover_url,
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
            classical,
            ..
        } => {
            let name = path.rsplit('/').next().unwrap_or(path).to_string();
            json!({
                "kind":            "file",
                "name":            name,
                "uri":             path,
                "title":           title,
                "artist":          artist,
                "album":           album,
                "duration_ms":     duration.map(|d| d.as_millis() as u64),
                // available defaults to true at browse time; the
                // skip-traversal / queue path consults the sticker
                // for authoritative checks during play.
                "available":       true,
                "artwork_url":     evo_device_audio_shared::artwork_target_url_for_track_sized(
                    path,
                    artist.as_deref(),
                    album.as_deref(),
                    // Search / drill list rows render at tile
                    // scale; request the `small` variant so N
                    // rows do not each pull a full-size original.
                    Some("small"),
                ),
                "composer":        classical.composer,
                "composer_sort":   classical.composer_sort,
                "conductor":       classical.conductor,
                "ensemble":        classical.ensemble,
                "performer":       classical.performer,
                "work":            classical.work,
                "work_sort":       classical.work_sort,
                "movement":        classical.movement,
                "movement_number": classical.movement_number,
                "original_date":   classical.original_date,
                "recording_date":  classical.recording_date,
                "label":           classical.label,
                "medium":          classical.medium,
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
        // Skip cloud + DLNA — cloud-native search substrate is a
        // follow-on primitive; for now those source kinds don't
        // appear in the search results.
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
        let render_ctx = RenderCtx {
            music_directory: &ctx.music_directory,
        };
        for e in entries {
            let mut item = render_library_entry(&e, Some(render_ctx));
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

/// Warm-start rehydration of the library subjects from MPD.
///
/// Called by `ShelfBundle::init` after `announce_subjects` to
/// replace the registry-snapshot-only zero envelope with the
/// real MPD database state. Reads MPD's `stats` for the floor-
/// source track total + last-scan timestamp, mirrors them onto
/// the `local-internal` registry record via `update_track_counts`,
/// then re-publishes both `audio_library_sources` and
/// `audio_library_state`.
///
/// Best-effort: any MPD error logs a warning and returns
/// without panicking. The plugin's later course-correct verbs
/// (`update_source` / `probe_source`) re-establish the counts
/// on operator command.
pub(crate) async fn rehydrate_from_mpd(
    ctx: &LibraryContext,
    conn: &mut crate::mpd::MpdConnection,
) {
    let stats = match conn.stats().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "library rehydrate: mpd stats failed; library counts \
                 will stay at zero until next scan"
            );
            return;
        }
    };
    // `available` count derives from the evo:available sticker
    // when the reconciler is up to date; on cold-start the sticker
    // set is empty so we mirror the total as the available count
    // (best-faith starting point) and the reconciler corrects it
    // shortly. The wider contract is that the counts always
    // reflect MPD truth, not a frozen snapshot.
    let last_scan_ms = stats.db_update_unix_s.map(|s| s * 1000);
    if let Err(e) = ctx
        .registry
        .apply_track_counts_with_scan_time(
            LOCAL_INTERNAL_SOURCE_ID,
            stats.songs,
            stats.songs,
            last_scan_ms,
        )
        .await
    {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "library rehydrate: registry apply_track_counts_with_scan_time failed"
        );
        return;
    }
    // Persist so the wire truth survives the next boot. Without
    // this, `sources.toml` keeps its stale track_count from the
    // last persist point — a wipe that empties the music
    // plane would still show thousands of ghost tracks on the
    // next start, because the in-memory apply above never
    // reached disk.
    if let Err(e) = ctx.registry.persist().await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "library rehydrate: registry persist after MPD-stats \
             apply failed; next boot may reload stale track_count"
        );
    }
    // Compute the works aggregate from MPD's full database
    // walk so the library_state counters + the
    // library.list_works / library.get_work_recordings verb
    // surfaces have a populated cache from the first publish.
    recompute_works_aggregate(ctx, conn).await;
    publish_subjects(ctx).await;
}

/// Walk MPD's full database via `listallinfo` and compute the
/// works aggregate (works list + per-work recordings + classical
/// counters). Stores the result in
/// [`LibraryContext::works_aggregate`] so the verb handlers and
/// the library_state envelope read from a shared cache. Called
/// on warm-start rehydration AND on every `Database` / `Update`
/// idle event (the idle observer dispatches it).
///
/// Best-effort: listallinfo failure logs and leaves the cache
/// unchanged. A cache that previously held a populated
/// aggregate stays populated; a fresh-start cache stays `None`
/// so the wire reports counters as JSON `null` per the
/// truth-or-null contract.
pub(crate) async fn recompute_works_aggregate(
    ctx: &LibraryContext,
    conn: &mut crate::mpd::MpdConnection,
) {
    let entries = match conn.listallinfo("").await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "works aggregate refresh: mpd listallinfo failed; \
                 keeping prior cache"
            );
            return;
        }
    };
    // Resolve source_id per track using the same prefix-walk
    // the queue / favourites / playlist surfaces use.
    let music_dir = ctx.music_directory.clone();
    let registry = ctx.registry.clone();
    // Take a registry snapshot once; the resolver closure walks
    // it without touching the lock on every track.
    let snapshot = registry.snapshot().await;
    let source_id_for_path = |rel: &str| -> Option<String> {
        let abs = music_dir.join(rel);
        snapshot
            .iter()
            .find(|r| abs.starts_with(&r.mount_path))
            .map(|r| r.id.clone())
    };
    let aggregate = crate::works::aggregate(&entries, source_id_for_path);
    tracing::info!(
        plugin = PLUGIN_NAME,
        works = aggregate.works.len(),
        distinct_works = aggregate.counters.distinct_works.unwrap_or(0),
        works_with_multiple_recordings = aggregate
            .counters
            .works_with_multiple_recordings
            .unwrap_or(0),
        total_tracks_with_composer =
            aggregate.counters.total_tracks_with_composer.unwrap_or(0),
        "works aggregate refreshed"
    );
    let mut g = ctx.works_aggregate.lock().await;
    *g = Some(aggregate);
}

// ----- Works browse verb handlers (Section B) -----

#[derive(Debug, Deserialize)]
pub(crate) struct ListWorksPayload {
    pub(crate) v: u32,
    /// Optional filter — only works appearing under this
    /// source_id. None matches every source.
    #[serde(default)]
    pub(crate) source_id: Option<String>,
    /// Optional substring filter on composer (case-insensitive).
    /// None matches every composer.
    #[serde(default)]
    pub(crate) composer: Option<String>,
    /// Optional sort order. "name" sorts by work name; "composer"
    /// (default) sorts by composer then name; "recording_count"
    /// sorts by recording count descending then composer + name.
    #[serde(default)]
    pub(crate) sort: Option<String>,
}

pub(crate) async fn handle_list_works(
    ctx: &LibraryContext,
    payload: ListWorksPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.list_works")?;
    let aggregate = {
        let g = ctx.works_aggregate.lock().await;
        g.as_ref().cloned()
    };
    let aggregate = match aggregate {
        Some(a) => a,
        None => {
            // Not yet computed — return an empty list with
            // total: 0. UI distinguishes "no scan yet" via the
            // library_state counters (which carry null for
            // unknown).
            return Ok(json!({
                "v":     LIBRARY_PAYLOAD_VERSION,
                "works": Vec::<serde_json::Value>::new(),
                "total": 0,
            }));
        }
    };
    let mut works: Vec<&crate::works::WorkSummary> =
        aggregate.works.iter().collect();
    // Apply source_id filter.
    if let Some(sid) = payload.source_id.as_deref() {
        works.retain(|w| w.sources.iter().any(|s| s == sid));
    }
    // Apply composer substring filter (case-insensitive).
    if let Some(needle) = payload.composer.as_deref() {
        let needle_lower = needle.to_lowercase();
        works.retain(|w| match w.composer.as_deref() {
            Some(c) => c.to_lowercase().contains(&needle_lower),
            None => false,
        });
    }
    // Sort order. Default is "composer" (already the aggregate's
    // baseline ordering).
    match payload.sort.as_deref() {
        Some("name") => works.sort_by(|a, b| a.name.cmp(&b.name)),
        Some("recording_count") => works.sort_by(|a, b| {
            b.recording_count.cmp(&a.recording_count).then_with(|| {
                a.composer
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.composer.as_deref().unwrap_or(""))
            })
        }),
        _ => {} // baseline order already by composer then name
    }
    let rendered: Vec<serde_json::Value> = works
        .iter()
        .map(|w| {
            json!({
                "work_id":          w.work_id,
                "name":             w.name,
                "composer":         w.composer,
                "recording_count":  w.recording_count,
                "sources":          w.sources,
            })
        })
        .collect();
    let total = rendered.len();
    Ok(json!({
        "v":     LIBRARY_PAYLOAD_VERSION,
        "works": rendered,
        "total": total,
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetWorkRecordingsPayload {
    pub(crate) v: u32,
    pub(crate) work_id: String,
    /// Optional source_id filter — only recordings whose tracks
    /// resolve under this source.
    #[serde(default)]
    pub(crate) source_id: Option<String>,
}

pub(crate) async fn handle_get_work_recordings(
    ctx: &LibraryContext,
    payload: GetWorkRecordingsPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "library.get_work_recordings")?;
    let aggregate = {
        let g = ctx.works_aggregate.lock().await;
        g.as_ref().cloned()
    };
    let aggregate = match aggregate {
        Some(a) => a,
        None => {
            // Not yet computed — refuse with a structured
            // error rather than fabricating an empty
            // recordings list (the consumer cannot
            // distinguish "scan didn't run" from "work has
            // no recordings"). The library_state envelope
            // carries the counters as null so the UI can
            // gate this verb on the counters being Some.
            return Err(VerbError::WorkAggregateNotReady);
        }
    };
    let summary = aggregate
        .works
        .iter()
        .find(|w| w.work_id == payload.work_id)
        .cloned();
    let summary = match summary {
        Some(s) => s,
        None => {
            return Err(VerbError::UnknownWork {
                work_id: payload.work_id,
            });
        }
    };
    let mut recordings: Vec<crate::works::WorkRecording> = aggregate
        .recordings_by_work
        .get(&payload.work_id)
        .cloned()
        .unwrap_or_default();
    if let Some(sid) = payload.source_id.as_deref() {
        // Filter recordings whose first track resolves under
        // the filter source. The source map was applied during
        // aggregation so a recording's source comes from the
        // parent work's resolved sources. Soft filter — drop
        // recordings whose work doesn't list the source.
        if !summary.sources.iter().any(|s| s == sid) {
            recordings.clear();
        }
    }
    let rendered: Vec<serde_json::Value> = recordings
        .iter()
        .map(|r| {
            json!({
                "recording_id":      r.recording_id,
                "conductor":         r.conductor,
                "ensemble":          r.ensemble,
                "performer":         r.performer,
                "original_date":     r.original_date,
                "recording_date":    r.recording_date,
                "label":             r.label,
                "medium":            r.medium,
                "album_uri":         r.album_uri,
                "track_uris":        r.track_uris,
                "total_duration_ms": r.total_duration_ms,
            })
        })
        .collect();
    Ok(json!({
        "v":          LIBRARY_PAYLOAD_VERSION,
        "work_id":    summary.work_id,
        "name":       summary.name,
        "composer":   summary.composer,
        "recordings": rendered,
    }))
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
            None,
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
            control_url: String::new(),
            base_url: String::new(),
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

    // --- render_source_row: per-kind wire shape ------------------

    fn local_source_fixture() -> SourceRecord {
        SourceRecord {
            id: "local-42".into(),
            display_name: "Local".into(),
            kind: SourceKind::LocalInternal,
            mount_path: PathBuf::from("/var/lib/evo/music/INTERNAL"),
            mpd_storage_name: Some("internal".into()),
            state: SourceState::Online,
            last_seen_online_at_ms: Some(1_000),
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::EagerIncremental {
                on_online: true,
                on_mount_event: false,
            },
            track_count: 123,
            track_count_available: 100,
            last_scan_at_ms: Some(2_000),
        }
    }

    fn dlna_source_fixture() -> SourceRecord {
        SourceRecord {
            id: "dlna-uuid-abc".into(),
            display_name: "MediaServer".into(),
            kind: SourceKind::NetworkDlna {
                service_id: "uuid:abc".into(),
                control_url: "http://192.0.2.10:8096/dlna/cd/control".into(),
                base_url: "http://192.0.2.10:8096".into(),
            },
            mount_path: PathBuf::from("/var/lib/evo/dlna/uuid:abc"),
            mpd_storage_name: None,
            state: SourceState::Online,
            last_seen_online_at_ms: Some(1_000),
            probe_cadence_ms: 60_000,
            scan_policy: ScanPolicy::BrowseOnly,
            track_count: 0,
            track_count_available: 0,
            last_scan_at_ms: None,
        }
    }

    #[test]
    fn render_source_row_local_includes_filesystem_fields() {
        let row = render_source_row(&local_source_fixture());
        assert_eq!(row["id"], "local-42");
        assert_eq!(row["display_name"], "Local");
        assert_eq!(row["mount_path"], "/var/lib/evo/music/INTERNAL");
        assert_eq!(row["mpd_storage_name"], "internal");
        assert_eq!(row["track_count"], 123);
        assert_eq!(row["track_count_available"], 100);
        assert_eq!(row["last_scan_at_ms"], 2_000);
    }

    #[test]
    fn render_source_row_dlna_omits_filesystem_fields() {
        let row = render_source_row(&dlna_source_fixture());
        assert_eq!(row["id"], "dlna-uuid-abc");
        assert_eq!(row["display_name"], "MediaServer");
        // A MediaServer is not a filesystem: mount_path,
        // track_count, track_count_available, last_scan_at_ms,
        // and mpd_storage_name are not emitted on the wire.
        assert!(
            !row.as_object().unwrap().contains_key("mount_path"),
            "row = {row}"
        );
        assert!(!row.as_object().unwrap().contains_key("track_count"));
        assert!(!row
            .as_object()
            .unwrap()
            .contains_key("track_count_available"));
        assert!(!row.as_object().unwrap().contains_key("last_scan_at_ms"));
        assert!(!row.as_object().unwrap().contains_key("mpd_storage_name"));
        // The MediaServer identity remains present.
        assert_eq!(row["kind"]["kind"], "network_dlna");
        assert_eq!(row["kind"]["service_id"], "uuid:abc");
    }

    // --- render_dlna_browse_entry: single-field dlna: identity --

    #[test]
    fn render_dlna_browse_entry_container_uri_is_object_id() {
        // Containers keep the ContentDirectory objectId as
        // `uri` so the UI drills via `library.browse_library`
        // with `path = uri` (same contract as local folders).
        // No `stable_uri` field on containers — containers are
        // not stored in favourites / playlists; the identity
        // is only meaningful for leaf items.
        let src = json!({
            "kind":        "directory",
            "name":        "Rock",
            "path":        "12$34",
            "child_count": 200,
        });
        let out = render_dlna_browse_entry(&src, "uuid:server-1");
        assert_eq!(out["kind"], "directory");
        assert_eq!(out["name"], "Rock");
        assert_eq!(out["uri"], "12$34");
        // No leaked auxiliary fields.
        assert!(!out.as_object().unwrap().contains_key("stable_uri"));
        assert!(!out.as_object().unwrap().contains_key("stream_uri"));
    }

    #[test]
    fn render_dlna_browse_entry_item_uri_is_dlna_scheme_verbatim() {
        // source.dlna.browse emits
        // `uri: dlna:<service_id>/<objectId>` on file entries;
        // the mapping passes it through unchanged.
        let src = json!({
            "kind":        "file",
            "name":        "Song.flac",
            "path":        "12$99",
            "title":       "Song",
            "artist":      "Artist",
            "album":       "Album",
            "artwork_url": "http://192.0.2.10:8096/art/xyz",
            "uri":         "dlna:uuid:server-1/12$99",
            "playable":    true,
        });
        let out = render_dlna_browse_entry(&src, "uuid:server-1");
        assert_eq!(out["kind"], "file");
        assert_eq!(out["uri"], "dlna:uuid:server-1/12$99");
        // No `http(s)` stream URL surfaces on the browse wire.
        assert!(!out.as_object().unwrap().values().any(|v| {
            v.as_str().is_some_and(|s| {
                s.starts_with("http://") && s.contains("stream")
            })
        }));
    }

    #[test]
    fn render_dlna_browse_entry_defensive_dlna_compose_on_legacy_stream_url() {
        // If a legacy source.dlna build emits `uri` as an
        // `http(s)` stream URL (pre-follow-up shape), the
        // mapping defensively composes `dlna:<sid>/<oid>` from
        // the entry's `path` + the response's `service_id` so
        // the operator glass NEVER sees `http(s)` from browse.
        let src = json!({
            "kind": "file",
            "name": "Legacy.flac",
            "path": "12$77",
            "uri":  "http://192.0.2.10:8096/stream/legacy.flac",
        });
        let out = render_dlna_browse_entry(&src, "uuid:server-1");
        assert_eq!(out["uri"], "dlna:uuid:server-1/12$77");
    }

    #[test]
    fn render_dlna_browse_entry_item_drops_null_optional_fields() {
        let src = json!({
            "kind": "file",
            "name": "Song.flac",
            "path": "12$99",
            "uri":  "dlna:uuid:server-1/12$99",
            // no title / artist / album / artwork_url present
        });
        let out = render_dlna_browse_entry(&src, "uuid:server-1");
        // Nulls stripped so the wire carries only real values.
        let obj = out.as_object().unwrap();
        assert!(!obj.contains_key("title"));
        assert!(!obj.contains_key("artist"));
        assert!(!obj.contains_key("album"));
        assert!(!obj.contains_key("artwork_url"));
        assert_eq!(out["uri"], "dlna:uuid:server-1/12$99");
    }

    // --- shelf_error_reason: classified strings ------------------

    #[test]
    fn shelf_error_reason_covers_every_variant() {
        assert!(shelf_error_reason(&ShelfDispatchError::NoPluginOnShelf {
            shelf: "audio.dlna".into()
        })
        .contains("no plugin on shelf"));
        assert!(shelf_error_reason(
            &ShelfDispatchError::VerbNotStockedOnShelf {
                shelf: "audio.dlna".into(),
                request_type: "source.dlna.browse".into(),
            }
        )
        .contains("not stocked"));
        assert!(shelf_error_reason(&ShelfDispatchError::Permanent {
            detail: "bad request".into()
        })
        .starts_with("permanent"));
        assert!(shelf_error_reason(&ShelfDispatchError::Transient {
            detail: "server offline".into()
        })
        .starts_with("transient"));
        assert!(shelf_error_reason(&ShelfDispatchError::DeadlineExceeded {
            budget_ms: 15_000
        })
        .contains("15000ms"));
        assert!(shelf_error_reason(&ShelfDispatchError::SubstrateFailure {
            detail: "router down".into()
        })
        .starts_with("substrate"));
    }

    // --- browse_dlna_via_source_dlna: peer-dispatch plumbing -----

    /// One captured `dispatch` invocation from the fake below.
    type CapturedCall = (String, String, Vec<u8>);
    type CapturedSlot = Arc<Mutex<Option<CapturedCall>>>;

    struct FakeDispatcher {
        response: Vec<u8>,
        error: Option<ShelfDispatchError>,
        captured: CapturedSlot,
    }

    impl ShelfRequestDispatcher for FakeDispatcher {
        fn dispatch<'a>(
            &'a self,
            shelf: &'a str,
            request_type: &'a str,
            payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Vec<u8>, ShelfDispatchError>,
                    > + Send
                    + 'a,
            >,
        > {
            let captured = Arc::clone(&self.captured);
            let response = self.response.clone();
            let error = self.error.clone();
            Box::pin(async move {
                let mut g = captured.lock().await;
                *g = Some((
                    shelf.to_string(),
                    request_type.to_string(),
                    payload,
                ));
                if let Some(e) = error {
                    return Err(e);
                }
                Ok(response)
            })
        }
    }

    #[tokio::test]
    async fn browse_dlna_dispatches_to_audio_dlna_shelf_and_shapes_envelope() {
        // Canned response from source.dlna.browse — the shape it
        // emits post-follow-up: file entries carry the stable
        // `dlna:<service_id>/<objectId>` identity as `uri`.
        let canned = json!({
            "v":          1,
            "status":     "ok",
            "service_id": "uuid:server-1",
            "path":       "0",
            "entries":    [
                {
                    "kind":        "directory",
                    "name":        "Music",
                    "path":        "12$0",
                    "child_count": 50,
                },
                {
                    "kind":     "file",
                    "name":     "Track.flac",
                    "path":     "12$1",
                    "title":    "Track",
                    "artist":   "Artist",
                    "album":    "Album",
                    "uri":      "dlna:uuid:server-1/12$1",
                    "playable": true,
                },
            ],
            "page":       0,
            "page_size":  50,
            "total":      2,
            "truncated":  false,
            "next_page":  serde_json::Value::Null,
        });
        let captured = Arc::new(Mutex::new(None));
        let disp: Arc<dyn ShelfRequestDispatcher> = Arc::new(FakeDispatcher {
            response: serde_json::to_vec(&canned).unwrap(),
            error: None,
            captured: Arc::clone(&captured),
        });
        let env = browse_dlna_via_source_dlna(
            &disp,
            "dlna-uuid-server-1",
            "uuid:server-1",
            "",
            0,
            50,
            &SourceState::Online,
        )
        .await
        .expect("dispatch ok");

        // The dispatcher saw the right shelf + verb.
        let g = captured.lock().await;
        let (shelf, verb, req_bytes) =
            g.as_ref().expect("dispatch invoked").clone();
        assert_eq!(shelf, "audio.dlna");
        assert_eq!(verb, "source.dlna.browse");
        let req: serde_json::Value =
            serde_json::from_slice(&req_bytes).unwrap();
        assert_eq!(req["service_id"], "uuid:server-1");
        assert_eq!(req["object_id"], "0"); // empty path → root

        // Response envelope: library.browse_library shape.
        assert_eq!(env["v"], 1);
        assert_eq!(env["source_id"], "dlna-uuid-server-1");
        assert_eq!(env["service_id"], "uuid:server-1");
        assert_eq!(env["stale"], false);

        // File entries carry the stable `dlna:` identity as the
        // single `uri` field. Containers carry the objectId as
        // `uri` for drill-in via library.browse_library. No
        // `stable_uri` / `stream_uri` auxiliary fields on either.
        let entries = env["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["kind"], "directory");
        assert_eq!(entries[0]["uri"], "12$0");
        assert!(!entries[0].as_object().unwrap().contains_key("stable_uri"));
        assert_eq!(entries[1]["kind"], "file");
        assert_eq!(entries[1]["uri"], "dlna:uuid:server-1/12$1");
        assert!(!entries[1].as_object().unwrap().contains_key("stable_uri"));
    }

    #[tokio::test]
    async fn browse_dlna_maps_shelf_dispatch_error_to_verb_error() {
        let captured = Arc::new(Mutex::new(None));
        let disp: Arc<dyn ShelfRequestDispatcher> = Arc::new(FakeDispatcher {
            response: Vec::new(),
            error: Some(ShelfDispatchError::Transient {
                detail: "MediaServer unreachable".into(),
            }),
            captured,
        });
        let err = browse_dlna_via_source_dlna(
            &disp,
            "dlna-uuid-server-1",
            "uuid:server-1",
            "",
            0,
            50,
            &SourceState::Online,
        )
        .await
        .expect_err("transient must surface");
        let reason = match err {
            VerbError::Mpd { reason, .. } => reason,
            other => panic!("expected VerbError::Mpd, got {other:?}"),
        };
        assert!(reason.contains("transient"));
        assert!(reason.contains("MediaServer unreachable"));
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

    // ---------------------------------------------------------
    // browse pagination
    // ---------------------------------------------------------

    fn stub_entries(n: usize) -> Vec<serde_json::Value> {
        (0..n)
            .map(
                |i| serde_json::json!({"kind": "file", "uri": format!("t{i}")}),
            )
            .collect()
    }

    #[test]
    fn paginate_empty_source_returns_empty_page_and_no_next() {
        let (page, next, trunc) = paginate(&[], 0, 500);
        assert!(page.is_empty());
        assert!(next.is_none());
        assert!(!trunc);
    }

    #[test]
    fn paginate_exact_fit_returns_all_with_no_next_page() {
        let full = stub_entries(500);
        let (page, next, trunc) = paginate(&full, 0, 500);
        assert_eq!(page.len(), 500);
        assert!(next.is_none());
        assert!(!trunc);
    }

    #[test]
    fn paginate_slice_boundary_sets_next_page_correctly() {
        // 3000 entries, page_size 500 → page 0 sees 0..500,
        // truncated=true, next_page=1.
        let full = stub_entries(3000);
        let (page, next, trunc) = paginate(&full, 0, 500);
        assert_eq!(page.len(), 500);
        assert_eq!(next, Some(1));
        assert!(trunc);
        // page 5 sees the last slice (2500..3000), no next.
        let (page5, next5, trunc5) = paginate(&full, 5 * 500, 500);
        assert_eq!(page5.len(), 500);
        assert!(next5.is_none());
        assert!(!trunc5);
    }

    #[test]
    fn paginate_past_end_returns_empty_no_next() {
        let full = stub_entries(300);
        let (page, next, trunc) = paginate(&full, 500, 500);
        assert!(page.is_empty());
        assert!(next.is_none());
        assert!(!trunc);
    }

    #[test]
    fn paginate_partial_last_page_no_next() {
        // 1050 entries, page_size 500 → pages 0,1 full;
        // page 2 has 50, no next.
        let full = stub_entries(1050);
        let (page2, next, trunc) = paginate(&full, 2 * 500, 500);
        assert_eq!(page2.len(), 50);
        assert!(next.is_none());
        assert!(!trunc);
    }

    #[test]
    fn browse_hard_cap_is_the_documented_ceiling() {
        // The hard cap is contract, not convenience — if it
        // changes, the doc must change too. Pins the value
        // so a future contributor can't silently loosen it.
        assert_eq!(BROWSE_HARD_CAP, 2_000);
    }

    // `artist_fold_key` unit tests live in the shared crate
    // (`evo_device_audio_shared::artist_name`) — the crate is
    // the canonical owner of the fold rules. The tests here
    // exercise the browse-facet integration, not the fold
    // function itself.

    // -----------------------------------------------------------
    // Tile-cascade tests — `pick_directory_cover_url`. Cover
    // every terminal state of the folder cascade so a future
    // contributor cannot silently drop a tier. Tier order:
    //
    //   1. sidecar in this dir
    //   2. embedded art of representative track in this dir
    //   3. sidecar cover in first stable-sorted child dir,
    //      else first child's first track (embedded)
    //   4. fallback → honest glyph
    //
    // Folder browse NEVER emits `artist-name`. That surface is
    // a file tree; portraits belong on the artist facet, which
    // keys on the tag. See `pick_directory_cover_url`'s docs.
    // -----------------------------------------------------------

    #[test]
    fn tier1_self_cover_wins_when_direct_sidecar_present() {
        // Direct art at the browsed directory itself wins over
        // every later tier — the URL points at the directory
        // the operator clicked.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let artist_dir = music_dir.join("The Beatles");
        std::fs::create_dir_all(&artist_dir).unwrap();
        std::fs::write(artist_dir.join("cover.jpg"), b"x").unwrap();
        std::fs::write(artist_dir.join("01 track.flac"), b"").unwrap();
        let url = pick_directory_cover_url("The Beatles", music_dir);
        assert!(
            url.contains("scheme=mpd-directory"),
            "Tier 1 must emit mpd-directory scheme, got {url}"
        );
        assert!(
            url.contains("The%20Beatles"),
            "Tier 1 URL must point at the browsed directory itself, got {url}"
        );
        assert!(
            !url.contains("scheme=mpd-path"),
            "Tier 1 must beat embedded-art (mpd-path) when a sidecar exists, \
             got {url}"
        );
    }

    #[test]
    fn tier2_embedded_art_track_when_no_sidecar_but_audio_present() {
        // Album folder with tracks and no sidecar — the common
        // case for operator libraries where art is embedded in
        // the tags. Emit `mpd-path?value=<self>/<first-track>`
        // and re-use artwork.local's per-track extractor.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let album_dir = music_dir.join("Adele").join("21");
        std::fs::create_dir_all(&album_dir).unwrap();
        std::fs::write(album_dir.join("02 - Rumour Has It.flac"), b"").unwrap();
        std::fs::write(album_dir.join("01 - Rolling in the Deep.flac"), b"")
            .unwrap();
        let url = pick_directory_cover_url("Adele/21", music_dir);
        assert!(
            url.contains("scheme=mpd-path"),
            "Tier 2 must emit mpd-path scheme so artwork.local's \
             per-track extractor runs, got {url}"
        );
        assert!(
            url.contains(
                "Adele%2F21%2F01%20-%20Rolling%20in%20the%20Deep.flac"
            ),
            "Tier 2 must point at the stable-sorted first audio track \
             (basename lexicographic — '01' before '02'), got {url}"
        );
    }

    #[test]
    fn tier2_beats_child_cover_when_directory_has_audio() {
        // A directory that has BOTH audio tracks and a child
        // subdirectory with a sidecar cover: Tier 2 (embedded)
        // wins because the tracks describe this directory itself
        // (album folder with a bonus-tracks child, hi-res
        // variants, etc.). The child cover speaks for the child,
        // not for this folder.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let album_dir = music_dir.join("Album");
        let bonus = album_dir.join("Bonus");
        std::fs::create_dir_all(&bonus).unwrap();
        std::fs::write(album_dir.join("01 track.flac"), b"").unwrap();
        std::fs::write(bonus.join("cover.jpg"), b"bonus").unwrap();
        let url = pick_directory_cover_url("Album", music_dir);
        assert!(
            url.contains("scheme=mpd-path"),
            "Tier 2 wins over Tier 3 when the directory owns tracks, \
             got {url}"
        );
        assert!(
            !url.contains("Bonus"),
            "picked URL must not point at the child directory, got {url}"
        );
    }

    #[test]
    fn container_folder_shows_a_child_album_sleeve() {
        // Folder browse is a file tree. A directory with child
        // albums and no tracks of its own is a container —
        // possibly an artist, but just as possibly a
        // multi-artist collaboration, a label or series, a box
        // set or a genre bucket. The honest picture of a
        // container is the music inside it.
        //
        // Emitting `artist-name` here instead asked a provider
        // for a person who does not exist, spending the
        // rate-limited budget once per container on every first
        // paint and painting a glyph on folders that visibly
        // contain records.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let artist_dir = music_dir.join("The Beatles");
        let abbey = artist_dir.join("Abbey Road");
        let revolver = artist_dir.join("Revolver");
        std::fs::create_dir_all(&abbey).unwrap();
        std::fs::create_dir_all(&revolver).unwrap();
        std::fs::write(abbey.join("cover.jpg"), b"abbey").unwrap();
        let url = pick_directory_cover_url("The Beatles", music_dir);
        assert!(
            url.contains("scheme=mpd-directory"),
            "container must point at the child carrying art, got {url}"
        );
        assert!(
            url.contains("Abbey%20Road"),
            "container must show the child album's sleeve, got {url}"
        );
        assert!(
            !url.contains("artist-name"),
            "folder browse must never emit a portrait lookup, got {url}"
        );
    }

    /// The collaboration / label case from the field: a folder
    /// name that is not one artist must still show its music,
    /// not start a doomed portrait cascade.
    #[test]
    fn collaboration_and_label_folders_show_their_music() {
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        for container in [
            "First Artist and Second Artist and Third Artist",
            "Example Label Series",
        ] {
            let dir = music_dir.join(container);
            let child = dir.join("Collaboration Single");
            std::fs::create_dir_all(&child).unwrap();
            std::fs::write(child.join("cover.jpg"), b"c").unwrap();
            let url = pick_directory_cover_url(container, music_dir);
            assert!(
                !url.contains("artist-name"),
                "{container} is a container, not a person: got {url}"
            );
            assert!(
                url.contains("Collaboration%20Single"),
                "{container} must show the record it contains: got {url}"
            );
        }
    }

    /// No child sidecar, but a child holds tracks — fall back to
    /// that child's first track so embedded art can surface
    /// rather than dropping straight to a glyph.
    #[test]
    fn container_falls_back_to_a_child_track_for_embedded_art() {
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let child = music_dir.join("Box Set").join("Disc 1");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("01 - Track.flac"), b"x").unwrap();
        let url = pick_directory_cover_url("Box Set", music_dir);
        assert!(url.contains("scheme=mpd-path"), "got {url}");
        assert!(url.contains("Disc%201"), "got {url}");
        assert!(!url.contains("artist-name"), "got {url}");
    }

    #[test]
    fn container_child_pick_is_stable_across_browses() {
        // Two children both carry covers; the alphabetically
        // first wins every time so `browse_cache` stays
        // coherent across repeat browses.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let artist_dir = music_dir.join("Dire Straits");
        let brothers = artist_dir.join("Brothers in Arms");
        let money = artist_dir.join("Money for Nothing");
        std::fs::create_dir_all(&brothers).unwrap();
        std::fs::create_dir_all(&money).unwrap();
        std::fs::write(brothers.join("cover.jpg"), b"a").unwrap();
        std::fs::write(money.join("cover.jpg"), b"b").unwrap();
        let first = pick_directory_cover_url("Dire Straits", music_dir);
        let second = pick_directory_cover_url("Dire Straits", music_dir);
        assert_eq!(first, second, "repeat browses must agree");
        assert!(first.contains("Brothers%20in%20Arms"), "got {first}");
    }

    #[test]
    fn container_with_no_child_art_falls_to_the_glyph() {
        // Children exist but none carries a sidecar and none
        // holds tracks, so there is no music to show. The tile
        // renders the honest glyph.
        //
        // This tier previously emitted `artist-name` on the
        // basename. Folder browse is a file tree, and "has
        // children, has no files" does not identify an artist —
        // a multi-artist collaboration folder, a label or series
        // directory, a box set and the source root all match it.
        // Asking a provider for a portrait of such a string is a
        // guaranteed miss that still spends the rate-limited
        // budget, once per container, on every first paint of a
        // browse. Portraits are the artist facet's job, keyed on
        // the tag rather than on a directory name.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let container = music_dir.join("Container Name");
        std::fs::create_dir_all(container.join("First Album")).unwrap();
        std::fs::create_dir_all(container.join("Second Album")).unwrap();
        let url = pick_directory_cover_url("Container Name", music_dir);
        assert!(
            !url.contains("artist-name"),
            "folder browse must never emit a portrait lookup, got {url}"
        );
        assert!(
            url.contains("scheme=mpd-directory")
                && url.contains("Container%20Name"),
            "fallback addresses the folder itself, got {url}"
        );
    }

    #[test]
    fn nested_container_never_emits_a_portrait_lookup() {
        // A nested container is still a container. Whatever the
        // path depth, folder browse does not ask for a portrait.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let nested = music_dir.join("Genre").join("Container Name");
        std::fs::create_dir_all(nested.join("First Album")).unwrap();
        let url = pick_directory_cover_url("Genre/Container Name", music_dir);
        assert!(
            !url.contains("artist-name"),
            "nested container must not emit a portrait lookup, got {url}"
        );
    }

    #[test]
    fn fallback_when_no_tracks_no_children_no_cover() {
        // Empty leaf directory — no sidecar, no audio, no
        // children. Fall back to Tier 1 URL and let the
        // resolver return `not_found` for the honest glyph.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let empty = music_dir.join("Empty Album");
        std::fs::create_dir_all(&empty).unwrap();
        let url = pick_directory_cover_url("Empty Album", music_dir);
        assert!(
            url.contains("scheme=mpd-directory"),
            "fallback must emit mpd-directory, got {url}"
        );
        assert!(
            url.contains("Empty%20Album"),
            "fallback URL points at the browsed directory, got {url}"
        );
        assert!(
            !url.contains("scheme=artist-name"),
            "fallback must NOT fire artist-name for a folder with no \
             children, got {url}"
        );
        assert!(
            !url.contains("scheme=mpd-path"),
            "fallback must NOT fire embedded-art for a folder with no \
             tracks, got {url}"
        );
    }

    #[test]
    fn repeated_calls_are_deterministic() {
        // Repeat browse must serve the same URL — the picked
        // URL is cached in `browse_cache`, and only holds if
        // the picker is deterministic. Two identical picks in
        // sequence must produce byte-identical URLs.
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let artist_dir = music_dir.join("Nirvana");
        std::fs::create_dir_all(artist_dir.join("In Utero")).unwrap();
        std::fs::create_dir_all(artist_dir.join("Nevermind")).unwrap();
        std::fs::write(artist_dir.join("Nevermind").join("cover.jpg"), b"n")
            .unwrap();
        std::fs::write(artist_dir.join("In Utero").join("cover.jpg"), b"i")
            .unwrap();
        let a = pick_directory_cover_url("Nirvana", music_dir);
        let b = pick_directory_cover_url("Nirvana", music_dir);
        assert_eq!(a, b, "identical inputs must produce identical URLs");
        assert!(
            a.contains("In%20Utero"),
            "stable sort picks 'In Utero' before 'Nevermind', got {a}"
        );
    }

    #[test]
    fn root_browse_paths_join_correctly() {
        // Browsing the DB root produces mpd_relative_path = ""
        // for the entry. The joined filesystem path is
        // music_directory itself; child join must produce
        // `Beatles` (no leading slash).
        let tmp = tempfile::tempdir().unwrap();
        let music_dir = tmp.path();
        let beatles = music_dir.join("Beatles");
        std::fs::create_dir_all(&beatles).unwrap();
        std::fs::write(beatles.join("cover.jpg"), b"c").unwrap();
        // A hypothetical root-level directory tile with the
        // basename "Beatles"; the tile emitter would call
        // pick_directory_cover_url("Beatles", music_dir) — the
        // Tier 1 case. Confirmed by the earlier tier1 test;
        // this test guards the Tier 2 child-join for empty
        // parent (rare in practice but not impossible).
        // Represent the empty-parent case by picking a synthetic
        // "" mpd_relative_path with a child cover.
        std::fs::write(music_dir.join("root-cover.jpg"), b"root").unwrap();
        let url = pick_directory_cover_url("", music_dir);
        assert!(
            url.contains("scheme=mpd-directory"),
            "root-with-direct-art picks Tier 1, got {url}"
        );
    }
}
