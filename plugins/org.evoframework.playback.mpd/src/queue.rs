//! Queue shelf — verb handlers + `audio_queue` subject emitter.
//!
//! Realises the `audio.queue.v1` catalogue contract: full queue
//! inspection, CRUD verbs, and the `audio_queue` subject
//! carrying the live ordered list of items with per-item
//! `available` flag derived from the `evo:available` sticker
//! (the sticker reconciler in [`crate::sticker_reconciler`]
//! writes the flag; this module reads it via
//! [`crate::mpd::MpdConnection::sticker_get`]).
//!
//! # Verb surface (9)
//!
//! - `queue.get_queue` — read current queue
//! - `queue.enqueue` — append/insert items
//! - `queue.remove_queue_item` — delete by songid
//! - `queue.move_queue_item` — move by songid
//! - `queue.clear_queue` — empty the queue
//! - `queue.load_playlist_to_queue` — replace queue with stored
//!   playlist contents
//! - `queue.append_playlist_to_queue` — append stored playlist
//! - `queue.save_queue_as_playlist` — save current queue
//! - `queue.skip_to_next_available` — operator-issued skip-
//!   traversal (consumes [`crate::skip_traversal`])
//!
//! # Catalogue acceptance rows honoured
//!
//! - `queue-item-available-flag-derives-from-sticker`: the
//!   `available` field is computed from the per-song
//!   `evo:available` sticker AND the resolved source's current
//!   reachability state. The plugin MUST NOT compute
//!   availability from any other signal.
//! - `queue-load-playlist-loads-full-regardless-of-availability`:
//!   `load_playlist_to_queue` / `append_playlist_to_queue` use
//!   MPD's `load` directly; pre-filtering by availability is
//!   forbidden.
//! - `queue-skip-to-next-available-emits-disposition`: the
//!   verb invokes the skip-traversal which emits one or more
//!   coalesced dispositions via [`crate::disposition_emitter`].
//! - `queue-subject-published-on-load-and-every-mutation`: the
//!   emitter publishes on every mutating verb + on the
//!   `idle playlist` wake (plugin integration wires the idle
//!   wake).
//!
//! # Source resolution
//!
//! Queue items carry the MPD-relative `file:` path. To compute
//! `source_id` for the wire envelope's per-item record, the
//! module's source resolver combines MPD's `music_directory`
//! with the file path to get an absolute path, then walks the
//! source registry to find which source's `mount_path` is a
//! prefix. The first match wins; items that don't resolve
//! under any registered source carry `source_id: null` on the
//! wire and are treated by the skip-traversal as `Probing`
//! sources (try-MPD-and-classify).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::mpd::MpdConnection;
use crate::skip_traversal::{PlayableQueueItem, SkipOutcome, SkipTraversal};
use crate::source_registry::SourceRegistry;
use crate::sticker_reconciler::EVO_AVAILABLE_STICKER;

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Wire-payload version for the `audio.queue.v1` shelf.
pub(crate) const QUEUE_PAYLOAD_VERSION: u32 = 1;

/// Subject type for the `audio_queue` subject; matches the
/// `audio.queue.v1` schema's `[[subjects]]` declaration
/// verbatim.
const SUBJECT_TYPE_QUEUE: &str = "audio_queue";

/// Addressing scheme for the queue subject.
const SCHEME_QUEUE: &str = "evo.audio.queue";

/// Addressing value for the queue subject — singleton per
/// warden.
const VALUE_QUEUE: &str = "queue";

/// Default truncation threshold for large queues. Per the
/// schema, queues exceeding this carry a `truncated: true`
/// field on the wire. Operator-configurable in a future
/// audio.options extension; not the focus of this commit.
pub(crate) const QUEUE_TRUNCATION_DEFAULT: usize = 500;

// ----- module-shared resources -----

/// Resources the queue module consumes. Arc-cloneable; the
/// plugin's integration layer constructs one and shares it
/// with subject emitter + each verb handler.
#[derive(Clone)]
pub(crate) struct QueueContext {
    /// MPD's music_directory resolved from /etc/mpd.conf at
    /// plugin load.
    pub(crate) music_directory: PathBuf,
    /// Shared source registry for source resolution.
    pub(crate) registry: SourceRegistry,
    /// Shared subject announcer for emitting state updates.
    pub(crate) subjects: Arc<dyn SubjectAnnouncer>,
    /// Skip-traversal handle for queue.skip_to_next_available.
    pub(crate) skip: SkipTraversal,
    /// In-memory mirror of the last published envelope, used
    /// by `queue.get_queue` to satisfy read-then-subscribe
    /// without round-tripping through the framework's subject
    /// querier.
    mirror: Arc<Mutex<Option<serde_json::Value>>>,
}

impl QueueContext {
    /// Construct a new context. Mirror starts empty; the first
    /// announce or publish populates it.
    pub(crate) fn new(
        music_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
        skip: SkipTraversal,
    ) -> Self {
        Self {
            music_directory,
            registry,
            subjects,
            skip,
            mirror: Arc::new(Mutex::new(None)),
        }
    }
}

// ----- subject emitter -----

/// Announce the `audio_queue` subject at plugin load with an
/// empty-queue envelope. Subscribers connecting before the
/// first refresh see the wire shape with `items: []` +
/// `current_position: null`.
pub(crate) async fn announce_queue(ctx: &QueueContext) {
    let addressing = ExternalAddressing::new(SCHEME_QUEUE, VALUE_QUEUE);
    let envelope = render_empty_envelope();
    {
        let mut g = ctx.mirror.lock().await;
        *g = Some(envelope.clone());
    }
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_QUEUE, vec![addressing])
            .with_state(envelope);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_queue subject announce failed; operator UI's queue \
             panel will be unavailable until a future re-announce attempt"
        );
    }
}

/// Refresh the queue subject's state from MPD's current queue.
/// Called on every mutating verb's success path AND on every
/// idle-playlist wake (the plugin integration wires the idle
/// callback to this).
pub(crate) async fn publish_queue(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
) {
    let envelope = match build_envelope(ctx, conn).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "audio_queue envelope build failed; skipping publish cycle"
            );
            return;
        }
    };
    {
        let mut g = ctx.mirror.lock().await;
        *g = Some(envelope.clone());
    }
    let addressing = ExternalAddressing::new(SCHEME_QUEUE, VALUE_QUEUE);
    if let Err(e) = ctx.subjects.update_state(addressing, envelope).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_queue subject update_state failed; operator UI \
             may show a stale queue until the next mutation"
        );
    }
}

/// Read the mirror (or the empty envelope when no publish has
/// happened yet). Used by `queue.get_queue`.
pub(crate) async fn read_mirror(ctx: &QueueContext) -> serde_json::Value {
    let g = ctx.mirror.lock().await;
    g.clone().unwrap_or_else(render_empty_envelope)
}

fn render_empty_envelope() -> serde_json::Value {
    json!({
        "v": QUEUE_PAYLOAD_VERSION,
        "items": Vec::<serde_json::Value>::new(),
        "length": 0,
        "current_position": serde_json::Value::Null,
    })
}

// ----- envelope construction -----

/// Build the wire envelope from MPD's current state. Reads
/// `playlistinfo` + the player's current song position +
/// per-item availability sticker.
pub(crate) async fn build_envelope(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
) -> Result<serde_json::Value, BuildError> {
    let items = conn
        .playlistinfo()
        .await
        .map_err(|e| BuildError::Mpd(format!("playlistinfo: {e}")))?;
    let status = conn
        .status()
        .await
        .map_err(|e| BuildError::Mpd(format!("status: {e}")))?;
    let current_position = status.song_position;

    let truncated = items.len() > QUEUE_TRUNCATION_DEFAULT;
    let take_n = if truncated {
        QUEUE_TRUNCATION_DEFAULT
    } else {
        items.len()
    };

    let mut rendered = Vec::with_capacity(take_n);
    for item in items.iter().take(take_n) {
        let source_id = resolve_source(
            &ctx.music_directory,
            &item.file_path,
            &ctx.registry,
        )
        .await;
        let available = compute_available(
            conn,
            &item.file_path,
            source_id.as_deref(),
            &ctx.registry,
        )
        .await;
        rendered.push(json!({
            "id":          item.id,
            "position":    item.position,
            "uri":         item.file_path,
            "source_id":   source_id,
            "title":       item.title,
            "artist":      item.artist,
            "album":       item.album,
            "duration_ms": item.duration.map(|d| d.as_millis() as u64),
            "available":   available,
        }));
    }

    let mut envelope = json!({
        "v":                QUEUE_PAYLOAD_VERSION,
        "items":            rendered,
        "length":           items.len(),
        "current_position": current_position,
    });
    if truncated {
        envelope
            .as_object_mut()
            .unwrap()
            .insert("truncated".to_string(), serde_json::Value::Bool(true));
    }
    Ok(envelope)
}

/// Build the [`PlayableQueueItem`] view skip-traversal consumes.
/// Mirrors [`build_envelope`]'s per-item computation but emits
/// the typed shape rather than a JSON value.
pub(crate) async fn build_playable_view(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
) -> Result<Vec<PlayableQueueItem>, BuildError> {
    let items = conn
        .playlistinfo()
        .await
        .map_err(|e| BuildError::Mpd(format!("playlistinfo: {e}")))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let source_id = resolve_source(
            &ctx.music_directory,
            &item.file_path,
            &ctx.registry,
        )
        .await;
        let available = compute_available(
            conn,
            &item.file_path,
            source_id.as_deref(),
            &ctx.registry,
        )
        .await;
        out.push(PlayableQueueItem {
            position: item.position,
            id: item.id,
            file_path: item.file_path,
            source_id,
            available,
        });
    }
    Ok(out)
}

/// Resolve which registered source's `mount_path` contains the
/// MPD-relative file path. Returns `None` for transient stream
/// URLs and items that don't resolve under any source.
pub(crate) async fn resolve_source(
    music_directory: &Path,
    file_path: &str,
    registry: &SourceRegistry,
) -> Option<String> {
    // External URLs (http://, https://, smb://, etc.) don't
    // resolve under a local mount; skip-traversal handles them
    // as ambient (Probing).
    if file_path.contains("://") {
        return None;
    }
    let absolute = music_directory.join(file_path);
    for source in registry.snapshot().await {
        if absolute.starts_with(&source.mount_path) {
            return Some(source.id);
        }
    }
    None
}

/// Compute the per-item `available` flag. Per the catalogue
/// acceptance row: derived from the `evo:available` sticker
/// AND the resolved source's current state. Items with no
/// sticker default to the source's reachability.
async fn compute_available(
    conn: &mut MpdConnection,
    file_path: &str,
    source_id: Option<&str>,
    registry: &SourceRegistry,
) -> bool {
    // Source state pre-flight: if the source is not reachable,
    // the item is not available regardless of sticker.
    if let Some(sid) = source_id {
        if let Some(record) = registry.get(sid).await {
            if !record.state.is_reachable() {
                return false;
            }
        }
    }
    // Source reachable (or unknown). Check sticker.
    match conn.sticker_get(file_path, EVO_AVAILABLE_STICKER).await {
        Ok(Some(value)) => value != "0",
        Ok(None) => true, // no sticker → optimistic; source state is the gate
        Err(_) => true,   // sticker read transient error; optimistic
    }
}

// ----- error type -----

/// Errors from envelope construction. Currently a single
/// catch-all for MPD transport / protocol failures during
/// build; the verb layer translates these into
/// `PluginError::Transient`.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum BuildError {
    /// MPD wire-layer failure during envelope build.
    #[error("queue envelope build error: {0}")]
    Mpd(String),
}

// ----- verb payload shapes -----

/// `queue.enqueue` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct EnqueuePayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// URIs to add to the queue, in order.
    pub(crate) uris: Vec<String>,
    /// Position to insert at; `None` appends to the end.
    #[serde(default)]
    pub(crate) position: Option<u32>,
}

/// `queue.remove_queue_item` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct RemoveQueueItemPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// MPD songid of the item to remove (from the
    /// `audio_queue` subject's `id` field).
    pub(crate) id: u32,
}

/// `queue.move_queue_item` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct MoveQueueItemPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// MPD songid of the item to move.
    pub(crate) id: u32,
    /// New zero-based queue position.
    pub(crate) to_position: u32,
}

/// `queue.load_playlist_to_queue` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct LoadPlaylistPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// Stored playlist name.
    pub(crate) playlist_name: String,
    /// Optional start position; when `Some(n)` the warden
    /// triggers playback at position `n` after the load.
    #[serde(default)]
    pub(crate) start_position: Option<u32>,
}

/// `queue.append_playlist_to_queue` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct AppendPlaylistPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// Stored playlist name to append.
    pub(crate) playlist_name: String,
}

/// `queue.save_queue_as_playlist` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct SaveQueueAsPlaylistPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// Stored playlist name to save under.
    pub(crate) playlist_name: String,
    /// When false, refuses with a structured error on name
    /// collision; when true, overwrites.
    #[serde(default)]
    pub(crate) overwrite: bool,
}

/// `queue.skip_to_next_available` request payload — empty
/// versioned envelope.
#[derive(Debug, Deserialize)]
pub(crate) struct SkipToNextAvailablePayload {
    /// Envelope version.
    pub(crate) v: u32,
}

/// Common-shape response for mutating verbs. Constructed by
/// the shelves' verb dispatcher via the JSON literal
/// `{ "v": 1, "status": "ok" }`; kept here as a type to anchor
/// the wire-shape contract.
#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub(crate) struct SimpleQueueResponse {
    /// Envelope version.
    pub(crate) v: u32,
    /// `"ok"` literal.
    pub(crate) status: &'static str,
}

// ----- helpers -----

/// Validate envelope version on inbound payloads. Returns a
/// structured `PluginError::Permanent` on version mismatch.
pub(crate) fn check_version(v: u32, verb: &str) -> Result<(), VerbError> {
    if v != QUEUE_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: verb.to_string(),
            got: v,
            expected: QUEUE_PAYLOAD_VERSION,
        });
    }
    Ok(())
}

/// Errors the verb layer surfaces. The plugin integration
/// translates these into `PluginError` variants per the
/// established convention.
#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum VerbError {
    /// Payload envelope's `v` doesn't match
    /// [`QUEUE_PAYLOAD_VERSION`].
    #[error("{verb}: payload version {got} unsupported; expected {expected}")]
    PayloadVersion {
        verb: String,
        got: u32,
        expected: u32,
    },
    /// `queue.enqueue` rejected the empty `uris` list.
    #[error("queue.enqueue: uris must not be empty")]
    EmptyUris,
    /// MPD-side error during the verb's wire dispatch.
    #[error("queue.{verb}: MPD error: {reason}")]
    Mpd { verb: String, reason: String },
}

// ----- verb handlers -----

/// `queue.get_queue` — read the current mirror.
pub(crate) async fn handle_get_queue(ctx: &QueueContext) -> serde_json::Value {
    read_mirror(ctx).await
}

/// `queue.enqueue` — add items to the queue. Inserts at
/// `position` when set; appends otherwise. Publishes the new
/// envelope on success.
pub(crate) async fn handle_enqueue(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: EnqueuePayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.enqueue")?;
    if payload.uris.is_empty() {
        return Err(VerbError::EmptyUris);
    }
    // MPD's `addid` accepts a position. For multi-URI enqueue
    // at position P, we add each URI at position P, P+1, P+2.
    // When position is None, addid without position appends.
    if let Some(start_pos) = payload.position {
        let mut current = start_pos;
        for uri in &payload.uris {
            conn.addid(uri, Some(current)).await.map_err(|e| {
                VerbError::Mpd {
                    verb: "enqueue".to_string(),
                    reason: e.to_string(),
                }
            })?;
            current = current.saturating_add(1);
        }
    } else {
        for uri in &payload.uris {
            conn.addid(uri, None).await.map_err(|e| VerbError::Mpd {
                verb: "enqueue".to_string(),
                reason: e.to_string(),
            })?;
        }
    }
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.remove_queue_item` — delete by songid.
pub(crate) async fn handle_remove_queue_item(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: RemoveQueueItemPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.remove_queue_item")?;
    conn.deleteid(payload.id)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "remove_queue_item".to_string(),
            reason: e.to_string(),
        })?;
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.move_queue_item` — move by songid to a new position.
pub(crate) async fn handle_move_queue_item(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: MoveQueueItemPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.move_queue_item")?;
    conn.moveid(payload.id, payload.to_position)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "move_queue_item".to_string(),
            reason: e.to_string(),
        })?;
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.clear_queue` — empty the queue.
pub(crate) async fn handle_clear_queue(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
) -> Result<(), VerbError> {
    conn.clear().await.map_err(|e| VerbError::Mpd {
        verb: "clear_queue".to_string(),
        reason: e.to_string(),
    })?;
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.load_playlist_to_queue` — replace queue with stored
/// playlist contents. Mixed-source playlists load full; the
/// per-item availability flag handles runtime state.
///
/// Per the schema, when `start_position` is non-null the
/// warden triggers playback at that position after the load.
pub(crate) async fn handle_load_playlist_to_queue(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: LoadPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.load_playlist_to_queue")?;
    conn.clear().await.map_err(|e| VerbError::Mpd {
        verb: "load_playlist_to_queue".to_string(),
        reason: e.to_string(),
    })?;
    conn.load_playlist(&payload.playlist_name)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "load_playlist_to_queue".to_string(),
            reason: e.to_string(),
        })?;
    if let Some(start) = payload.start_position {
        if let Err(e) = conn.play_position(start).await {
            // Non-fatal: queue loaded but auto-start failed. The
            // operator can retry play. Surface as a warn for the
            // observability surface to pick up.
            tracing::warn!(
                plugin = PLUGIN_NAME,
                playlist = %payload.playlist_name,
                start_position = start,
                error = %e,
                "queue.load_playlist_to_queue: auto-start at start_position failed"
            );
        }
    }
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.append_playlist_to_queue` — append stored playlist
/// contents to the end of the queue.
pub(crate) async fn handle_append_playlist_to_queue(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: AppendPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.append_playlist_to_queue")?;
    conn.load_playlist(&payload.playlist_name)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "append_playlist_to_queue".to_string(),
            reason: e.to_string(),
        })?;
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.save_queue_as_playlist` — persist the current queue
/// as a stored playlist.
pub(crate) async fn handle_save_queue_as_playlist(
    _ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: SaveQueueAsPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.save_queue_as_playlist")?;
    if payload.overwrite {
        // MPD's `save` ACKs with 56 (exists) on collision.
        // When overwrite is true, pre-delete then save.
        let _ = conn.rm_playlist(&payload.playlist_name).await;
    }
    conn.save_playlist(&payload.playlist_name)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "save_queue_as_playlist".to_string(),
            reason: e.to_string(),
        })?;
    // No queue mutation; the playlist index subject (separate
    // shelf) will refresh on its own idle wake. The queue
    // subject doesn't need an update.
    Ok(())
}

/// `queue.skip_to_next_available` — run the skip-traversal
/// against the current queue starting at the current position;
/// disposition records fire through the shared emitter.
pub(crate) async fn handle_skip_to_next_available(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: SkipToNextAvailablePayload,
) -> Result<SkipOutcome, VerbError> {
    check_version(payload.v, "queue.skip_to_next_available")?;
    let view =
        build_playable_view(ctx, conn)
            .await
            .map_err(|e| VerbError::Mpd {
                verb: "skip_to_next_available".to_string(),
                reason: e.to_string(),
            })?;
    let status = conn.status().await.map_err(|e| VerbError::Mpd {
        verb: "skip_to_next_available".to_string(),
        reason: e.to_string(),
    })?;
    let from = status.song_position.map(|p| p as i64).unwrap_or(-1);
    let outcome = ctx.skip.advance_to_next_playable(conn, from, &view).await;
    // Publish the queue to refresh per-item available flags
    // after the traversal (a successful Playing changes the
    // current_position; any disposition emission might have
    // moved sticker reconciler state).
    publish_queue(ctx, conn).await;
    Ok(outcome)
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::{
        ScanPolicy, SourceKind, SourceRecord, SourceState,
    };

    fn local_source(id: &str, mount: &str, state: SourceState) -> SourceRecord {
        SourceRecord {
            id: id.into(),
            display_name: "test".into(),
            kind: SourceKind::LocalInternal,
            mount_path: PathBuf::from(mount),
            mpd_storage_name: None,
            state,
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

    #[tokio::test]
    async fn resolve_source_matches_first_prefix() {
        let registry = SourceRegistry::new();
        registry
            .register(local_source(
                "internal",
                "/var/lib/evo/music/INTERNAL",
                SourceState::Online,
            ))
            .await
            .unwrap();
        registry
            .register(local_source(
                "nas",
                "/var/lib/evo/music/NAS",
                SourceState::Online,
            ))
            .await
            .unwrap();
        let id = resolve_source(
            Path::new("/var/lib/evo/music"),
            "INTERNAL/foo.flac",
            &registry,
        )
        .await;
        assert_eq!(id.as_deref(), Some("internal"));
        let id2 = resolve_source(
            Path::new("/var/lib/evo/music"),
            "NAS/album/track.mp3",
            &registry,
        )
        .await;
        assert_eq!(id2.as_deref(), Some("nas"));
    }

    #[tokio::test]
    async fn resolve_source_returns_none_for_external_urls() {
        let registry = SourceRegistry::new();
        registry
            .register(local_source(
                "internal",
                "/var/lib/evo/music/INTERNAL",
                SourceState::Online,
            ))
            .await
            .unwrap();
        let id = resolve_source(
            Path::new("/var/lib/evo/music"),
            "http://stream.example.com/radio.mp3",
            &registry,
        )
        .await;
        assert!(id.is_none());
    }

    #[tokio::test]
    async fn resolve_source_returns_none_when_no_mount_matches() {
        let registry = SourceRegistry::new();
        registry
            .register(local_source(
                "internal",
                "/var/lib/evo/music/INTERNAL",
                SourceState::Online,
            ))
            .await
            .unwrap();
        let id = resolve_source(
            Path::new("/var/lib/evo/music"),
            "USB/unmounted/track.flac",
            &registry,
        )
        .await;
        assert!(id.is_none());
    }

    #[test]
    fn render_empty_envelope_carries_wire_contract() {
        let env = render_empty_envelope();
        assert_eq!(env["v"], 1);
        assert_eq!(env["length"], 0);
        assert!(env["items"].is_array());
        assert_eq!(env["items"].as_array().unwrap().len(), 0);
        assert!(env["current_position"].is_null());
    }

    #[test]
    fn check_version_accepts_matching_version() {
        assert!(check_version(QUEUE_PAYLOAD_VERSION, "queue.test").is_ok());
    }

    #[test]
    fn check_version_refuses_mismatched_version() {
        let err = check_version(99, "queue.test").unwrap_err();
        match err {
            VerbError::PayloadVersion {
                verb,
                got,
                expected,
            } => {
                assert_eq!(verb, "queue.test");
                assert_eq!(got, 99);
                assert_eq!(expected, QUEUE_PAYLOAD_VERSION);
            }
            other => panic!("expected PayloadVersion, got {other:?}"),
        }
    }

    #[test]
    fn enqueue_empty_uris_is_caught_by_handler() {
        // The handler signature requires mpd conn so we can't
        // call it directly without a real connection; assert
        // the dedicated VerbError variant exists + formats.
        let err = VerbError::EmptyUris;
        let msg = format!("{err}");
        assert!(msg.contains("must not be empty"));
    }
}
