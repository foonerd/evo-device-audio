// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
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
//! # Verb surface (10)
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
//! - `queue.play_from_position` — set current queue position
//!   and start playback in one call (positional address —
//!   tap-to-play + play-from-this-track surfaces consume this)
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

use crate::library::LIBRARY_PAYLOAD_VERSION;
use crate::mpd::{MpdConnection, MpdLibraryEntry};
use crate::skip_traversal::{PlayableQueueItem, SkipOutcome, SkipTraversal};
use crate::source_registry::SourceRegistry;

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
    /// Peer-shelf dispatcher. Populated from
    /// [`evo_plugin_sdk::contract::LoadContext::shelf_request_dispatcher`]
    /// at plugin admission. `queue.enqueue_selection` uses this
    /// to peer-dispatch to `source.dlna.browse` on `audio.dlna`
    /// for the Container-shape selection input; the Criteria
    /// shape never consults the dispatcher.
    pub(crate) shelf_dispatcher: Option<
        Arc<
            dyn evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher,
        >,
    >,
}

impl QueueContext {
    /// Construct a new context. Mirror starts empty; the first
    /// announce or publish populates it.
    pub(crate) fn new(
        music_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
        skip: SkipTraversal,
        shelf_dispatcher: Option<
            Arc<
                dyn evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher,
            >,
        >,
    ) -> Self {
        Self {
            music_directory,
            registry,
            subjects,
            skip,
            mirror: Arc::new(Mutex::new(None)),
            shelf_dispatcher,
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
            "id":              item.id,
            "position":        item.position,
            "uri":             item.file_path,
            "source_id":       source_id,
            "title":           item.title,
            "artist":          item.artist,
            "album":           item.album,
            "duration_ms":     item.duration.map(|d| d.as_millis() as u64),
            "available":       available,
            "artwork_url":     evo_device_audio_shared::artwork_target_url_for_track_sized(
                &item.file_path,
                item.artist.as_deref(),
                item.album.as_deref(),
                // Queue row is a list surface — request the small
                // (300 px) size variant.
                Some("small"),
            ),
            "composer":        item.classical.composer,
            "composer_sort":   item.classical.composer_sort,
            "conductor":       item.classical.conductor,
            "ensemble":        item.classical.ensemble,
            "performer":       item.classical.performer,
            "work":            item.classical.work,
            "work_sort":       item.classical.work_sort,
            "movement":        item.classical.movement,
            "movement_number": item.classical.movement_number,
            "original_date":   item.classical.original_date,
            "recording_date":  item.classical.recording_date,
            "label":           item.classical.label,
            "medium":          item.classical.medium,
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

/// Compute the per-item `available` flag — delegates to the
/// shared availability cascade so the three shelves
/// (queue / favourites / playlist) project per-item truth
/// through one primitive. See [`crate::availability`] for the
/// cascade contract: sticker > source-state > None.
async fn compute_available(
    conn: &mut MpdConnection,
    file_path: &str,
    source_id: Option<&str>,
    registry: &SourceRegistry,
) -> Option<bool> {
    crate::availability::compute_item_available(
        conn, file_path, source_id, registry,
    )
    .await
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

/// `queue.enqueue_selection` request payload — multi-
/// dimensional server-side resolution. The UI passes a
/// selection input + mode; the plugin resolves via the
/// [`crate::selection::SelectionResolver`] seam (Criteria
/// shape) or via peer-shelf dispatch to the owning source
/// plugin (Container shape) and applies the resulting URI
/// set atomically. No URI list crosses the wire from UI to
/// plugin.
#[derive(Debug, Deserialize)]
pub(crate) struct EnqueueSelectionPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// Source-registry id the selection applies to. Required
    /// for the Container shape; ignored (may be absent) for
    /// the Criteria shape.
    #[serde(default)]
    pub(crate) source_id: Option<String>,
    /// Selection input — Criteria (MPD-native) or Container
    /// (source-plugin-owned opaque identifier).
    pub(crate) selection: SelectionInput,
    /// Mode: `replace` clears + adds + plays atomically;
    /// `next` inserts after the currently-playing item;
    /// `append` adds to the tail.
    #[serde(default)]
    pub(crate) mode: EnqueueSelectionMode,
    /// Container-shape only: zero-based page index. Defaults
    /// to 0.
    #[serde(default)]
    pub(crate) page: Option<u32>,
    /// Container-shape only: page size. Defaults to 50; hard-
    /// capped at 100 by the owning source plugin.
    #[serde(default)]
    pub(crate) page_size: Option<u32>,
}

/// Selection input — either the existing MPD-native Criteria
/// shape or the new source-plugin-owned Container shape.
///
/// Serde-untagged so the two shapes discriminate on field
/// presence: Container carries `kind: "container"` and `uri`;
/// Criteria carries `dimension` (and `value`). Old callers
/// sending the pre-existing `{ dimension, value, parent }`
/// shape continue to deserialise into the Criteria variant.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum SelectionInput {
    /// New source-plugin-owned container reference.
    Container(ContainerSelection),
    /// Existing MPD-native tag/facet selection.
    Criteria(crate::selection::SelectionCriteria),
}

/// The Container-shape selection body.
#[derive(Debug, Deserialize)]
pub(crate) struct ContainerSelection {
    /// Discriminator. Must be `"container"`.
    #[allow(dead_code)]
    pub(crate) kind: ContainerSelectionKind,
    /// The container's opaque identifier. For DLNA this is the
    /// ContentDirectory objectId; other sources define their
    /// own shape.
    pub(crate) uri: String,
}

/// Discriminator enum for [`ContainerSelection`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContainerSelectionKind {
    Container,
}

/// Mode for [`EnqueueSelectionPayload`].
///
/// - `Append` (default) — add at the tail of the current
///   queue.
/// - `Next` — insert after the currently-playing item; the
///   verb resolves URIs first (materialising a filter via
///   `find` when needed) so it can position each add.
/// - `Replace` — atomic clear + add + play in one MPD
///   command list; on resolution failure the existing queue
///   is left intact (all-or-nothing).
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EnqueueSelectionMode {
    #[default]
    Append,
    Next,
    Replace,
}

impl EnqueueSelectionMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EnqueueSelectionMode::Append => "append",
            EnqueueSelectionMode::Next => "next",
            EnqueueSelectionMode::Replace => "replace",
        }
    }
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

/// `queue.play_from_position` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct PlayFromPositionPayload {
    /// Envelope version.
    pub(crate) v: u32,
    /// Zero-based queue position of the entry to play. Refuses
    /// with a structured Permanent error when out of range or
    /// when the queue is empty.
    pub(crate) position: u32,
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
    /// `queue.play_from_position` addressed an out-of-range
    /// position. Carries the offending index and the current
    /// queue length so operator UI can render the specific
    /// mismatch inline.
    #[error(
        "queue.play_from_position: position {position} out of range \
         (queue length {length})"
    )]
    PositionOutOfRange {
        /// The zero-based position the caller supplied.
        position: u32,
        /// The current queue length at the time of the check.
        length: u32,
    },
    /// `queue.play_from_position` refused because the queue is
    /// empty.
    #[error("queue.play_from_position: queue is empty")]
    QueueEmpty,
    /// MPD-side error during the verb's wire dispatch.
    #[error("queue.{verb}: MPD error: {reason}")]
    Mpd { verb: String, reason: String },
}

/// Pure position-validation used by
/// [`handle_play_from_position`]. Returns `Ok(())` when
/// `position < length`; returns [`VerbError::QueueEmpty`] when
/// `length == 0`; otherwise returns
/// [`VerbError::PositionOutOfRange`].
///
/// Extracted so unit tests can exercise every refusal branch
/// without an MPD wire round-trip.
pub(crate) fn validate_play_from_position(
    position: u32,
    length: u32,
) -> Result<(), VerbError> {
    if length == 0 {
        return Err(VerbError::QueueEmpty);
    }
    if position >= length {
        return Err(VerbError::PositionOutOfRange { position, length });
    }
    Ok(())
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
    // Resolve every `dlna:<service_id>/<objectId>` in the batch
    // to `http(s)` via peer-dispatch to source.dlna.resolve;
    // MPD's `add` / `addid` only speak library-relative paths
    // and `http(s)`. Non-`dlna:` URIs pass through unchanged.
    // Resolve BEFORE any MPD write so a mid-batch resolve
    // failure leaves the queue intact (all-or-nothing).
    //
    // The resolver returns `ResolvedTrack` carrying the MPD URI
    // AND any DIDL tags the source-plugin harvested. We hand the
    // URI to `addid` (as before) then follow up with `addtagid`
    // per non-empty tag so DLNA queue entries carry title /
    // artist / album on the projection instead of a raw URL. MPD
    // does not extract ID3 tags from arbitrary HTTP streams, so
    // this side-channel is the only way title/artist/album ever
    // land on the queue for a DLNA-resolved track.
    let mut resolved: Vec<ResolvedTrack> =
        Vec::with_capacity(payload.uris.len());
    for uri in &payload.uris {
        resolved.push(resolve_uri_for_mpd(ctx, "enqueue", uri).await?);
    }
    if let Some(start_pos) = payload.position {
        let mut current = start_pos;
        for r in &resolved {
            let id = conn.addid(&r.uri, Some(current)).await.map_err(|e| {
                VerbError::Mpd {
                    verb: "enqueue".to_string(),
                    reason: e.to_string(),
                }
            })?;
            apply_resolved_tags(conn, id, r).await;
            current = current.saturating_add(1);
        }
    } else {
        for r in &resolved {
            let id =
                conn.addid(&r.uri, None).await.map_err(|e| VerbError::Mpd {
                    verb: "enqueue".to_string(),
                    reason: e.to_string(),
                })?;
            apply_resolved_tags(conn, id, r).await;
        }
    }
    publish_queue(ctx, conn).await;
    Ok(())
}

/// `queue.enqueue_selection` — server-side resolve + apply.
///
/// The verb dispatches the selection to a
/// [`crate::selection::SelectionResolver`] chosen by the
/// caller's `source_id` (today: MPD-only, so the resolver is
/// [`crate::selection::MpdSelectionResolver`]). The
/// [`crate::selection::ResolvedSelection`] is applied
/// according to `mode`:
///
/// - `Replace` — atomic `command_list_begin, clear,
///   findadd/searchadd/add..., play, command_list_end`. On
///   resolution failure the existing queue is left intact.
///   On zero-match the queue is left intact and the response
///   returns `status: "empty"` — never a silent clear.
/// - `Append` — `findadd/searchadd` (Filter) or `add`-loop
///   (UriList) at the tail. One MPD roundtrip.
/// - `Next` — insert at `status.song + 1`. Filter is
///   materialised via `find` first because MPD's `findadd`
///   goes to end. Each URI is `addid`ed at an incrementing
///   position.
///
/// Returns a structured response with `status`, `mode`,
/// `dimension`, `added_uris_count`, `queue_pos_start`.
pub(crate) async fn handle_enqueue_selection(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    resolver: &dyn crate::selection::SelectionResolver,
    payload: EnqueueSelectionPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "queue.enqueue_selection")?;
    match payload.selection {
        SelectionInput::Container(sel) => {
            handle_enqueue_selection_container(
                ctx,
                conn,
                payload.source_id,
                sel,
                payload.mode,
                payload.page,
                payload.page_size,
            )
            .await
        }
        SelectionInput::Criteria(criteria) => {
            handle_enqueue_selection_criteria(
                ctx,
                conn,
                resolver,
                criteria,
                payload.mode,
            )
            .await
        }
    }
}

/// The existing MPD-native Criteria path, factored out of the
/// verb entrypoint so the Container path can share the
/// enqueue-apply logic (`apply_append` / `apply_replace` /
/// `Next` positional add) without reshaping the top-level
/// signature.
async fn handle_enqueue_selection_criteria(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    resolver: &dyn crate::selection::SelectionResolver,
    criteria: crate::selection::SelectionCriteria,
    mode: EnqueueSelectionMode,
) -> Result<serde_json::Value, VerbError> {
    let dimension_label = criteria.dimension.as_str().to_string();
    let mode_label = mode.as_str().to_string();
    let resolved = resolver.resolve(conn, &criteria).await.map_err(|e| {
        VerbError::Mpd {
            verb: "enqueue_selection".to_string(),
            reason: e.to_string(),
        }
    })?;
    // Zero-match short-circuit — explicit empty, queue left
    // intact, no atomic clear. For `Filter` selections the
    // resolver's `is_empty` only catches the "no pairs"
    // shape; the actual match count requires an MPD `count`
    // roundtrip (cheap — MPD answers with one songs line).
    let is_empty = match &resolved {
        crate::selection::ResolvedSelection::UriList(list) => list.is_empty(),
        crate::selection::ResolvedSelection::Filter { pairs, .. } => {
            let pairs_ref: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(t, v)| (t.as_str(), v.as_str()))
                .collect();
            conn.count_matching(&pairs_ref).await.map_err(|e| {
                VerbError::Mpd {
                    verb: "enqueue_selection".to_string(),
                    reason: e.to_string(),
                }
            })? == 0
        }
    };
    if is_empty {
        return Ok(serde_json::json!({
            "v":                LIBRARY_PAYLOAD_VERSION,
            "status":           "empty",
            "mode":             mode_label,
            "kind":             "criteria",
            "dimension":        dimension_label,
            "added_uris_count": 0,
            "detail":           "selection matched zero tracks; queue unchanged",
        }));
    }
    // Materialise the URI list for `Next` (and for reporting
    // added-count) — MPD's `findadd` cannot target a
    // position. For `Append` / `Replace` a Filter runs
    // as-is via findadd/searchadd for a single roundtrip.
    let materialise_needed = matches!(mode, EnqueueSelectionMode::Next);
    let uris: Vec<String> = if materialise_needed {
        materialise_to_uris(conn, &resolved).await.map_err(|e| {
            VerbError::Mpd {
                verb: "enqueue_selection".to_string(),
                reason: e.to_string(),
            }
        })?
    } else {
        match &resolved {
            crate::selection::ResolvedSelection::UriList(list) => list.clone(),
            crate::selection::ResolvedSelection::Filter { .. } => Vec::new(),
        }
    };
    match mode {
        EnqueueSelectionMode::Append => {
            apply_append(conn, &resolved, &uris).await?;
        }
        EnqueueSelectionMode::Next => {
            let start_pos = current_song_position(conn).await? + 1;
            let mut current = start_pos;
            for uri in &uris {
                conn.addid(uri, Some(current)).await.map_err(|e| {
                    VerbError::Mpd {
                        verb: "enqueue_selection".to_string(),
                        reason: e.to_string(),
                    }
                })?;
                current = current.saturating_add(1);
            }
        }
        EnqueueSelectionMode::Replace => {
            apply_replace(conn, &resolved, &uris).await?;
        }
    }
    publish_queue(ctx, conn).await;
    let added = if uris.is_empty() {
        // Filter path (Append) — MPD executed one findadd/searchadd;
        // we do not have an exact count without a follow-up status
        // read. Report `null` in that case; the queue subject
        // subscribers see the actual tracks arriving.
        serde_json::Value::Null
    } else {
        serde_json::Value::from(uris.len())
    };
    Ok(serde_json::json!({
        "v":                LIBRARY_PAYLOAD_VERSION,
        "status":           "ok",
        "mode":             mode_label,
        "kind":             "criteria",
        "dimension":        dimension_label,
        "added_uris_count": added,
    }))
}

// --- Container selection ---------------------------------------

pub(crate) const AUDIO_DLNA_SHELF: &str = "audio.dlna";
pub(crate) const SOURCE_DLNA_BROWSE_VERB: &str = "source.dlna.browse";

/// Default maximum recursion depth when the enqueue-container
/// handler walks a DLNA browse subtree. Six covers every
/// realistic library shape (root → artists → albums → disks →
/// tracks is 4 levels; genres / decades / collections rarely
/// push beyond 5). Overridden via plugin config
/// `dlna.enqueue.max_depth`.
const DLNA_ENQUEUE_MAX_DEPTH_DEFAULT: u32 = 6;

/// Default cap on total leaf tracks the recursive descent will
/// enqueue in a single verb call. 5000 is generous (one
/// artist's full discography is ~200 tracks, an "all favourites"
/// container tops out well below this on any reasonable
/// library) yet bounded — protects against pathological
/// MediaServer shapes (one container with a million
/// descendants). Overridden via plugin config
/// `dlna.enqueue.max_tracks`.
const DLNA_ENQUEUE_MAX_TRACKS_DEFAULT: usize = 5000;

/// Depth-first descent over a DLNA container subtree; returns the
/// stable leaf URIs collected in tree order alongside a `truncated`
/// flag set when either the depth or the total-tracks cap fired.
///
/// Pure w.r.t. MPD — takes a shelf-request dispatcher and the DLNA
/// service parameters and does one thing: browse subcontainers,
/// collect leaves, return. The caller performs URI resolution and
/// MPD writes. Extracted from
/// [`handle_enqueue_selection_container`] so the descent shape is
/// unit-testable against a scripted `ShelfRequestDispatcher` without
/// having to fake an MPD connection.
///
/// Descent contract:
///
/// - Iterative DFS. `stack` carries `(object_id, remaining_depth)`
///   pairs; the top of the stack is the container currently being
///   walked.
/// - Tree order preserved by pushing subcontainers in reverse per
///   page (so the first-listed subcontainer is popped first).
/// - Each container is browsed page-by-page until the peer-shelf
///   response's `truncated` flag clears (which the source-plugin
///   sets when `NumberReturned + StartingIndex < TotalMatches`).
/// - Depth cap: descent into a container at `depth == 0` sets
///   `truncated_by_cap = true` and skips it.
/// - Track cap: on hitting `max_tracks` collected leaves, descent
///   aborts immediately (breaks the outer loop) with
///   `truncated_by_cap = true`.
///
/// Field-name contract with the source-plugin browse response
/// (`source.dlna.browse`):
///
/// - `entries[i].kind`  — `"file"` for leaves, `"directory"` for
///   subcontainers.
/// - `entries[i].uri`   — present ONLY on leaves; carries the
///   stable playback identity (`dlna:<service_id>/<object_id>`).
///   This is what the caller feeds to `resolve_uri_for_mpd`.
/// - `entries[i].path`  — present on BOTH leaves and containers;
///   carries the raw ContentDirectory `ObjectID`. This is what we
///   feed back into the next SOAP Browse to descend into a
///   subcontainer.
///
/// A pre-fix version of this descent read `entries[i].uri` for the
/// directory branch too. Containers carry no `uri` field, so the
/// recursion silently no-op'd on every subcontainer — an operator
/// tapping an artist / genre / folder / decade tile would find the
/// queue unchanged even though the payload envelope reported
/// `status: ok`. The regression test
/// [`dlna_descent_walks_two_level_container_tree_via_path_field`]
/// pins the field-name contract so this can never regress silently
/// again.
pub(crate) async fn collect_dlna_container_leaves(
    dispatcher: &dyn evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher,
    service_id: &str,
    root_object_id: &str,
    max_depth: u32,
    max_tracks: usize,
    page_size: u32,
) -> Result<(Vec<String>, bool), VerbError> {
    let mut stable_uris: Vec<String> = Vec::new();
    let mut stack: Vec<(String, u32)> =
        vec![(root_object_id.to_string(), max_depth)];
    let mut truncated_by_cap = false;

    'descent: while let Some((oid, depth)) = stack.pop() {
        if depth == 0 {
            truncated_by_cap = true;
            continue;
        }
        let mut page: u32 = 0;
        loop {
            let request = serde_json::json!({
                "v":          1,
                "service_id": service_id,
                "object_id":  oid,
                "page":       page,
                "page_size":  page_size,
            });
            let request_bytes =
                serde_json::to_vec(&request).map_err(|e| VerbError::Mpd {
                    verb: "enqueue_selection".into(),
                    reason: format!("dlna container: serialise request: {e}"),
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
                    verb: "enqueue_selection".into(),
                    reason: format!(
                        "dlna container: {}",
                        shelf_error_reason(&e)
                    ),
                })?;
            let response: serde_json::Value =
                serde_json::from_slice(&response_bytes).map_err(|e| {
                    VerbError::Mpd {
                        verb: "enqueue_selection".into(),
                        reason: format!("dlna container: parse response: {e}"),
                    }
                })?;
            let entries = response
                .get("entries")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            // Two passes over this page in one iteration:
            // 1) collect leaves in reading order + subcontainer
            //    URIs in reading order; 2) push subcontainers onto
            //    the stack in reverse so the first-listed
            //    subcontainer pops first (tree order).
            let mut subcontainers_this_page: Vec<String> = Vec::new();
            for entry in &entries {
                let kind = entry.get("kind").and_then(|v| v.as_str());
                let uri = entry
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                let path = entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                match (kind, uri, path) {
                    (Some("file"), Some(u), _) => {
                        stable_uris.push(u.to_string());
                        if stable_uris.len() >= max_tracks {
                            truncated_by_cap = true;
                            break 'descent;
                        }
                    }
                    (Some("directory"), _, Some(p)) => {
                        subcontainers_this_page.push(p.to_string());
                    }
                    _ => {}
                }
            }
            for sub in subcontainers_this_page.into_iter().rev() {
                stack.push((sub, depth - 1));
            }
            let page_truncated = response
                .get("truncated")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !page_truncated {
                break;
            }
            page = response
                .get("next_page")
                .and_then(|v| v.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(page + 1);
        }
    }
    Ok((stable_uris, truncated_by_cap))
}

/// Container-shape handler: recursively browses a DLNA
/// container and enqueues every leaf track its subtree carries,
/// under bounded depth + total-track caps.
///
/// Descent shape: depth-first, tree-order. The caller's
/// starting container is browsed page-by-page; each page's
/// leaf items enqueue in reading order; each page's
/// subcontainers push onto a DFS stack in reverse (so the
/// first-listed subcontainer is popped first, preserving
/// tree order). Descent continues until the subtree is
/// exhausted, the depth cap fires, or the total-track cap
/// fires.
///
/// Caps: [`DLNA_ENQUEUE_MAX_DEPTH_DEFAULT`] +
/// [`DLNA_ENQUEUE_MAX_TRACKS_DEFAULT`], both plugin-config-
/// settable via `dlna.enqueue.max_depth` +
/// `dlna.enqueue.max_tracks`. When either cap fires, the
/// response envelope's `truncated` field is set to `true` and
/// the operator can see (via `enqueued_count`) how many tracks
/// were actually queued.
///
/// This shape supersedes an earlier "direct-child leaves only,
/// no recursion" behaviour that meant only album-shaped
/// containers (whose direct children are tracks) enqueued
/// anything — artist / folder / genre / decade / year /
/// collection containers all silently no-op'd because their
/// direct children were subcontainers.
///
/// The response envelope carries:
///
/// - `enqueued` + `enqueued_count`: number of leaf tracks
///   actually queued. Both fields carry the same value;
///   `enqueued_count` is the canonical name for new consumers;
///   `enqueued` is retained for back-compat with earlier UI
///   consumers.
/// - `truncated`: `true` iff descent hit a cap (depth or
///   track budget) before exhausting the subtree.
/// - `next_page`: always `null` under recursive descent
///   (the whole subtree resolves within one verb call).
async fn handle_enqueue_selection_container(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    source_id: Option<String>,
    selection: ContainerSelection,
    mode: EnqueueSelectionMode,
    _page: Option<u32>,
    page_size: Option<u32>,
) -> Result<serde_json::Value, VerbError> {
    let mode_label = mode.as_str().to_string();
    let source_id = source_id.ok_or_else(|| VerbError::Mpd {
        verb: "enqueue_selection".into(),
        reason:
            "container selection requires source_id at top level of payload"
                .into(),
    })?;
    let record =
        ctx.registry
            .get(&source_id)
            .await
            .ok_or_else(|| VerbError::Mpd {
                verb: "enqueue_selection".into(),
                reason: format!("unknown source_id {source_id:?}"),
            })?;
    let service_id = match &record.kind {
        crate::source_registry::SourceKind::NetworkDlna {
            service_id, ..
        } => service_id.clone(),
        other => {
            return Err(VerbError::Mpd {
                verb: "enqueue_selection".into(),
                reason: format!(
                    "container selection against source {source_id:?} \
                     of kind {other:?}: only network_dlna sources \
                     resolve containers today"
                ),
            });
        }
    };
    let dispatcher =
        ctx.shelf_dispatcher
            .as_ref()
            .ok_or_else(|| VerbError::Mpd {
                verb: "enqueue_selection".into(),
                reason: "container selection requires the peer-shelf \
                     dispatcher, but LoadContext.shelf_request_dispatcher \
                     was None at admission — the plugin was loaded \
                     without a dispatcher wired"
                    .into(),
            })?;

    let effective_page_size = page_size.unwrap_or(evo_dlna::DLNA_PAGE_DEFAULT);
    let max_depth = DLNA_ENQUEUE_MAX_DEPTH_DEFAULT;
    let max_tracks = DLNA_ENQUEUE_MAX_TRACKS_DEFAULT;

    let (stable_uris, truncated_by_cap) = collect_dlna_container_leaves(
        dispatcher.as_ref(),
        &service_id,
        &selection.uri,
        max_depth,
        max_tracks,
        effective_page_size,
    )
    .await?;
    // Resolve each stable identity to a concrete `http(s)` at
    // MPD-add time via the shared boundary helper. Resolve
    // BEFORE any MPD write so a mid-descent resolve failure
    // leaves the queue intact (all-or-nothing). The resolver
    // returns the full `ResolvedTrack` so the enqueue paths
    // below can `addtagid` DIDL tags onto MPD immediately after
    // `addid` (MPD does not extract ID3 from arbitrary HTTP
    // streams, so the queue would otherwise show empty
    // title/artist/album for DLNA-resolved entries).
    let mut resolved: Vec<ResolvedTrack> =
        Vec::with_capacity(stable_uris.len());
    for uri in &stable_uris {
        resolved
            .push(resolve_uri_for_mpd(ctx, "enqueue_selection", uri).await?);
    }

    if resolved.is_empty() {
        return Ok(serde_json::json!({
            "v":              LIBRARY_PAYLOAD_VERSION,
            "status":         "empty",
            "mode":           mode_label,
            "kind":           "container",
            "enqueued":       0,
            "enqueued_count": 0,
            "truncated":      truncated_by_cap,
            "next_page":      serde_json::Value::Null,
            "detail":         "container subtree carried no playable leaf items; queue unchanged",
        }));
    }

    // All three modes now go through `addid` (returns song id)
    // so we can attach tags. Append + Replace previously used
    // bare `add` (silent, no id); switched to `addid` so the
    // addtagid burst can run.
    match mode {
        EnqueueSelectionMode::Append => {
            for r in &resolved {
                let id = conn.addid(&r.uri, None).await.map_err(|e| {
                    VerbError::Mpd {
                        verb: "enqueue_selection".into(),
                        reason: e.to_string(),
                    }
                })?;
                apply_resolved_tags(conn, id, r).await;
            }
        }
        EnqueueSelectionMode::Next => {
            let start_pos = current_song_position(conn).await? + 1;
            let mut current = start_pos;
            for r in &resolved {
                let id =
                    conn.addid(&r.uri, Some(current)).await.map_err(|e| {
                        VerbError::Mpd {
                            verb: "enqueue_selection".into(),
                            reason: e.to_string(),
                        }
                    })?;
                apply_resolved_tags(conn, id, r).await;
                current = current.saturating_add(1);
            }
        }
        EnqueueSelectionMode::Replace => {
            conn.clear().await.map_err(|e| VerbError::Mpd {
                verb: "enqueue_selection".into(),
                reason: e.to_string(),
            })?;
            for r in &resolved {
                let id = conn.addid(&r.uri, None).await.map_err(|e| {
                    VerbError::Mpd {
                        verb: "enqueue_selection".into(),
                        reason: e.to_string(),
                    }
                })?;
                apply_resolved_tags(conn, id, r).await;
            }
            conn.play().await.map_err(|e| VerbError::Mpd {
                verb: "enqueue_selection".into(),
                reason: e.to_string(),
            })?;
        }
    }
    publish_queue(ctx, conn).await;
    Ok(serde_json::json!({
        "v":              LIBRARY_PAYLOAD_VERSION,
        "status":         "ok",
        "mode":           mode_label,
        "kind":           "container",
        "enqueued":       resolved.len(),
        "enqueued_count": resolved.len(),
        "truncated":      truncated_by_cap,
        "next_page":      serde_json::Value::Null,
    }))
}

pub(crate) fn shelf_error_reason(
    e: &evo_plugin_sdk::contract::shelf_dispatch::ShelfDispatchError,
) -> String {
    use evo_plugin_sdk::contract::shelf_dispatch::ShelfDispatchError as E;
    match e {
        E::NoPluginOnShelf { shelf } => {
            format!("no plugin on shelf {shelf:?}")
        }
        E::VerbNotStockedOnShelf {
            shelf,
            request_type,
        } => {
            format!("verb {request_type:?} not stocked on shelf {shelf:?}")
        }
        E::Permanent { detail } => format!("permanent: {detail}"),
        E::Transient { detail } => format!("transient: {detail}"),
        E::DeadlineExceeded { budget_ms } => {
            format!("deadline exceeded ({budget_ms}ms)")
        }
        E::SubstrateFailure { detail } => {
            format!("substrate failure: {detail}")
        }
    }
}

// --- dlna: → http resolve at MPD-add boundary -----------------

const URI_SCHEME_DLNA: &str = "dlna:";
const SOURCE_DLNA_RESOLVE_VERB: &str = "source.dlna.resolve";

/// A resolved queue-add candidate: the MPD-acceptable URI plus
/// any operator-visible tags the resolver was able to attach.
///
/// For non-`dlna:` URIs (local FS path, direct http(s) stream)
/// every tag field is `None` — the caller hands MPD the URI and
/// MPD extracts what it can (ID3 on local files; nothing on
/// arbitrary HTTP streams). For `dlna:` URIs the resolver
/// harvests the DIDL fields from `source.dlna.resolve`'s
/// response so the enqueue path can `addtagid` them onto MPD
/// immediately after the `addid`, closing the "DLNA queue
/// entry shows raw URL and no tags" gap.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTrack {
    pub uri: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    /// DIDL `dc:date` — handed to MPD as `Date` (year or ISO).
    pub date: Option<String>,
    pub composer: Option<String>,
    // artwork_url is carried across the resolve boundary but not
    // yet threaded into the MPD projection — MPD tag names are a
    // fixed set (Title / Artist / Album / …) with no `AlbumArt`
    // tag that gets picked up as artwork; the queue projection
    // synthesises its `artwork_url` from (file_path, artist,
    // album) via `artwork_target_url_for_track_sized`. Threading
    // this explicit URL into the projection needs a separate
    // side-channel (per-song sticker or a shelf-scoped cache
    // keyed by song id) — tracked as a follow-on to the DIDL
    // tags landing.
    #[allow(dead_code)]
    pub artwork_url: Option<String>,
    #[allow(dead_code)]
    pub duration: Option<String>,
}

impl ResolvedTrack {
    fn passthrough(uri: &str) -> Self {
        Self {
            uri: uri.to_string(),
            title: None,
            artist: None,
            album: None,
            genre: None,
            date: None,
            composer: None,
            artwork_url: None,
            duration: None,
        }
    }

    /// True when at least one metadata tag is present. Enqueue
    /// paths use this to decide whether the follow-up
    /// `addtagid` burst is needed at all.
    fn has_tags(&self) -> bool {
        self.title.is_some()
            || self.artist.is_some()
            || self.album.is_some()
            || self.genre.is_some()
            || self.date.is_some()
            || self.composer.is_some()
    }
}

/// Apply the resolved track's DIDL tags to a queued songid via
/// `addtagid`. Fire-and-forget: a per-tag MPD failure logs at
/// debug and continues (tags are operator-visible polish, not
/// a correctness invariant — the track still plays with an
/// empty title). Returns unconditionally so a slow MPD does
/// not block the enqueue verb's response.
///
/// Tag set covers the classical / audiophile fields ContentDirectory
/// commonly emits: Genre, Date (year), Composer — in addition to
/// Title / Artist / Album. MPD's queue projection already flattens
/// `Composer` and `Date` into the classical block on every
/// track-bearing envelope.
pub(crate) async fn apply_resolved_tags(
    conn: &mut MpdConnection,
    song_id: u32,
    resolved: &ResolvedTrack,
) {
    if !resolved.has_tags() {
        return;
    }
    for (tag, value) in [
        ("Title", resolved.title.as_deref()),
        ("Artist", resolved.artist.as_deref()),
        ("Album", resolved.album.as_deref()),
        ("Genre", resolved.genre.as_deref()),
        ("Date", resolved.date.as_deref()),
        ("Composer", resolved.composer.as_deref()),
    ] {
        let Some(v) = value else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        if let Err(e) = conn.addtagid(song_id, tag, v).await {
            tracing::debug!(
                plugin = PLUGIN_NAME,
                song_id,
                tag,
                error = %e,
                "addtagid failed; queue projection may show empty tag \
                 (track still plays)"
            );
        }
    }
}

/// Resolve a stored URI into a form MPD's `add` / `addid` will
/// accept, translating `dlna:<service_id>/<objectId>` into the
/// concrete `http(s)` stream URL via peer-shelf dispatch to
/// `source.dlna.resolve`. Non-`dlna:` URIs pass through
/// unchanged; MPD itself gates on scheme at add time.
///
/// Returns a [`ResolvedTrack`] carrying the MPD URI plus any
/// DIDL tags the resolver harvested (title / artist / album /
/// artwork_url / duration). Non-DLNA URIs return an all-None
/// tag set — MPD's own extraction handles them.
///
/// Favourites and stored playlists keep their `dlna:` stable
/// identity on disk across MediaServer IP / token churn; the
/// resolve only happens at the enqueue-to-MPD boundary. A
/// Transient reply from the source plugin (MediaServer offline,
/// cache empty) reaches the caller as
/// [`VerbError::Mpd`] with the underlying classification word
/// preserved in the reason string so operator UI can
/// distinguish "retry when server is back" from "this entry is
/// gone."
pub(crate) async fn resolve_uri_for_mpd(
    ctx: &QueueContext,
    verb: &str,
    uri: &str,
) -> Result<ResolvedTrack, VerbError> {
    if !uri.starts_with(URI_SCHEME_DLNA) {
        return Ok(ResolvedTrack::passthrough(uri));
    }
    let (service_id, object_id) =
        parse_dlna_uri(uri).ok_or_else(|| VerbError::Mpd {
            verb: verb.to_string(),
            reason: format!(
                "stored dlna: URI {uri:?} does not match \
                 `dlna:<service_id>/<objectId>`; the favourite or playlist \
                 entry was written by a caller that does not agree with the \
                 source.dlna URI-scheme owner"
            ),
        })?;
    let dispatcher =
        ctx.shelf_dispatcher
            .as_ref()
            .ok_or_else(|| VerbError::Mpd {
                verb: verb.to_string(),
                reason: format!(
                    "dlna: URI {uri:?} requires the peer-shelf dispatcher to \
                 resolve, but LoadContext.shelf_request_dispatcher was None \
                 at admission"
                ),
            })?;
    let request = json!({
        "v":          1,
        "service_id": service_id,
        "object_id":  object_id,
    });
    let request_bytes =
        serde_json::to_vec(&request).map_err(|e| VerbError::Mpd {
            verb: verb.to_string(),
            reason: format!("dlna resolve: serialise request: {e}"),
        })?;
    let response_bytes = dispatcher
        .dispatch(
            AUDIO_DLNA_SHELF,
            SOURCE_DLNA_RESOLVE_VERB,
            request_bytes,
            None,
        )
        .await
        .map_err(|e| VerbError::Mpd {
            verb: verb.to_string(),
            reason: format!("dlna resolve {uri:?}: {}", shelf_error_reason(&e)),
        })?;
    let response: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|e| VerbError::Mpd {
        verb: verb.to_string(),
        reason: format!("dlna resolve: parse response: {e}"),
    })?;
    let http_uri =
        response
            .get("uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerbError::Mpd {
                verb: verb.to_string(),
                reason: format!(
                    "dlna resolve {uri:?}: source.dlna.resolve response \
                     missing `uri` field"
                ),
            })?;
    if !(http_uri.starts_with("http://") || http_uri.starts_with("https://")) {
        return Err(VerbError::Mpd {
            verb: verb.to_string(),
            reason: format!(
                "dlna resolve {uri:?}: resolved URI {http_uri:?} is not \
                 http(s) — refusing to hand a non-MPD scheme to `add`"
            ),
        });
    }
    // Harvest the DIDL tags the resolve response carries so the
    // enqueue path can `addtagid` them onto MPD after the `addid`.
    // Missing fields collapse to `None`; the resolver never
    // fabricates a tag from other fields (e.g. does not derive a
    // title from the path). Empty strings are collapsed to `None`
    // so the follow-up `addtagid` skips them rather than
    // installing an empty tag.
    let opt_str = |key: &str| -> Option<String> {
        response
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    Ok(ResolvedTrack {
        uri: http_uri.to_string(),
        title: opt_str("title"),
        artist: opt_str("artist"),
        album: opt_str("album"),
        genre: opt_str("genre"),
        date: opt_str("date"),
        composer: opt_str("composer"),
        artwork_url: opt_str("artwork_url"),
        duration: opt_str("duration"),
    })
}

/// Split `dlna:<service_id>/<objectId>` into its parts. Service
/// IDs may themselves contain colons (`uuid:aaaaaaaa-…`); the
/// split is on the first `/` after the scheme.
fn parse_dlna_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix(URI_SCHEME_DLNA)?;
    let (service_id, object_id) = rest.split_once('/')?;
    if service_id.is_empty() || object_id.is_empty() {
        return None;
    }
    Some((service_id.to_string(), object_id.to_string()))
}

async fn apply_append(
    conn: &mut MpdConnection,
    resolved: &crate::selection::ResolvedSelection,
    uris: &[String],
) -> Result<(), VerbError> {
    match resolved {
        crate::selection::ResolvedSelection::Filter { pairs, substring } => {
            let pairs_ref: Vec<(&str, &str)> = pairs
                .iter()
                .map(|(t, v)| (t.as_str(), v.as_str()))
                .collect();
            let result = if *substring {
                conn.searchadd(&pairs_ref).await
            } else {
                conn.findadd(&pairs_ref).await
            };
            result.map_err(|e| VerbError::Mpd {
                verb: "enqueue_selection".to_string(),
                reason: e.to_string(),
            })
        }
        crate::selection::ResolvedSelection::UriList(_) => {
            for uri in uris {
                conn.add(uri).await.map_err(|e| VerbError::Mpd {
                    verb: "enqueue_selection".to_string(),
                    reason: e.to_string(),
                })?;
            }
            Ok(())
        }
    }
}

async fn apply_replace(
    conn: &mut MpdConnection,
    resolved: &crate::selection::ResolvedSelection,
    uris: &[String],
) -> Result<(), VerbError> {
    // Atomic replace: clear + add* + play in one MPD
    // command list. On resolution failure the earlier `resolve`
    // step already returned Err and the queue was never
    // touched.
    let mut commands: Vec<(&str, Vec<String>)> = vec![("clear", Vec::new())];
    match resolved {
        crate::selection::ResolvedSelection::Filter { pairs, substring } => {
            let cmd = if *substring { "searchadd" } else { "findadd" };
            let args: Vec<String> = pairs
                .iter()
                .flat_map(|(t, v)| [t.clone(), v.clone()])
                .collect();
            commands.push((cmd, args));
        }
        crate::selection::ResolvedSelection::UriList(_) => {
            for uri in uris {
                commands.push(("add", vec![uri.clone()]));
            }
        }
    }
    // Play from position 0 explicitly. MPD's plain `play`
    // uses the queue's `song_position` pointer, which can
    // retain a stale index from a pre-clear state; the
    // observed symptom was a Replace landing playback in the
    // middle of the newly-loaded queue. `play 0` starts at
    // the top of the freshly-materialised selection every
    // time.
    commands.push(("play", vec!["0".to_string()]));
    conn.command_list(&commands)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "enqueue_selection".to_string(),
            reason: e.to_string(),
        })
}

async fn materialise_to_uris(
    conn: &mut MpdConnection,
    resolved: &crate::selection::ResolvedSelection,
) -> Result<Vec<String>, crate::mpd::MpdError> {
    match resolved {
        crate::selection::ResolvedSelection::UriList(list) => Ok(list.clone()),
        crate::selection::ResolvedSelection::Filter { pairs, substring } => {
            let pairs_ref: Vec<(crate::mpd::MpdSearchField, &str)> = pairs
                .iter()
                .filter_map(|(t, v)| {
                    let field = match t.as_str() {
                        "artist" => crate::mpd::MpdSearchField::Artist,
                        "albumartist" => {
                            crate::mpd::MpdSearchField::AlbumArtist
                        }
                        "album" => crate::mpd::MpdSearchField::Album,
                        "genre" => crate::mpd::MpdSearchField::Genre,
                        "date" => crate::mpd::MpdSearchField::Date,
                        _ => return None,
                    };
                    Some((field, v.as_str()))
                })
                .collect();
            let entries = if *substring {
                // `search` semantics for substring; single-pair
                // shape covers the year case.
                if let Some((field, value)) = pairs_ref.first() {
                    conn.search(field.clone(), value).await?
                } else {
                    Vec::new()
                }
            } else {
                conn.find_multi(&pairs_ref).await?
            };
            Ok(entries
                .into_iter()
                .filter_map(|e| match e {
                    MpdLibraryEntry::File { path, .. } => Some(path),
                    _ => None,
                })
                .collect())
        }
    }
}

async fn current_song_position(
    conn: &mut MpdConnection,
) -> Result<u32, VerbError> {
    let status = conn.status().await.map_err(|e| VerbError::Mpd {
        verb: "enqueue_selection".to_string(),
        reason: e.to_string(),
    })?;
    Ok(status.song_position.unwrap_or(0))
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
    load_playlist_with_dlna_resolve(
        ctx,
        conn,
        "load_playlist_to_queue",
        &payload.playlist_name,
    )
    .await?;
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
    load_playlist_with_dlna_resolve(
        ctx,
        conn,
        "append_playlist_to_queue",
        &payload.playlist_name,
    )
    .await?;
    publish_queue(ctx, conn).await;
    Ok(())
}

/// Read a stored playlist's entries via `listplaylistinfo`,
/// resolve any `dlna:` URIs to concrete `http(s)` streams via
/// peer-shelf dispatch, and add each to the queue. Replaces
/// MPD's own `load` verb on the `queue.load_playlist_to_queue`
/// and `queue.append_playlist_to_queue` paths so a favourites
/// or playlist entry carrying the stable
/// `dlna:<service_id>/<objectId>` identity can round-trip
/// through the queue without MPD refusing the scheme it does
/// not know.
///
/// Resolve is per-entry so a single unresolvable entry (a
/// MediaServer offline, an objectId no longer valid) refuses
/// the whole load atomically before any MPD write — the queue
/// starts intact and stays intact. This matches the
/// `queue.enqueue` all-or-nothing invariant.
async fn load_playlist_with_dlna_resolve(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    verb: &str,
    playlist_name: &str,
) -> Result<(), VerbError> {
    let entries = conn.listplaylistinfo(playlist_name).await.map_err(|e| {
        VerbError::Mpd {
            verb: verb.to_string(),
            reason: e.to_string(),
        }
    })?;
    let mut resolved: Vec<ResolvedTrack> = Vec::with_capacity(entries.len());
    for e in &entries {
        resolved.push(resolve_uri_for_mpd(ctx, verb, &e.file_path).await?);
    }
    for r in &resolved {
        let id =
            conn.addid(&r.uri, None).await.map_err(|e| VerbError::Mpd {
                verb: verb.to_string(),
                reason: e.to_string(),
            })?;
        apply_resolved_tags(conn, id, r).await;
    }
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

/// `queue.play_from_position` — set the current queue position
/// and start playback in one call. Pre-validates the requested
/// position against MPD's current queue length via
/// `playlistinfo` BEFORE dispatching `play <pos>`; empty queue
/// or out-of-range refuses with a structured Permanent error
/// and leaves MPD untouched. On success, dispatches
/// [`MpdConnection::play_position`] which sets both the
/// current position AND the Play state in the same MPD command
/// regardless of prior state (Stop / Pause / already-Playing at
/// a different position).
///
/// After a successful dispatch, the queue subject is refreshed
/// so the operator UI's Queue panel reflects the new
/// `current_position` immediately (the `audio_now_playing`
/// subject also republishes via the playback shelf's idle-wake
/// path — no explicit fan-out is needed here).
///
/// Shuffle-active behaviour: MPD's `play <pos>` addresses the
/// operator-facing queue position regardless of `random 1`; the
/// shuffle order continues from the addressed entry. No plugin
/// action is needed to preserve this — the pass-through matches
/// MPD's native semantics.
pub(crate) async fn handle_play_from_position(
    ctx: &QueueContext,
    conn: &mut MpdConnection,
    payload: PlayFromPositionPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "queue.play_from_position")?;
    let items = conn.playlistinfo().await.map_err(|e| VerbError::Mpd {
        verb: "play_from_position".to_string(),
        reason: e.to_string(),
    })?;
    let length = u32::try_from(items.len()).unwrap_or(u32::MAX);
    validate_play_from_position(payload.position, length)?;
    conn.play_position(payload.position)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "play_from_position".to_string(),
            reason: e.to_string(),
        })?;
    publish_queue(ctx, conn).await;
    Ok(())
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_registry::{
        ScanPolicy, SourceKind, SourceRecord, SourceState,
    };
    use std::collections::HashMap;

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

    // ----- SelectionInput deserialisation -----

    #[test]
    fn selection_input_container_parses_from_kind_field() {
        let payload = serde_json::json!({
            "v":         1,
            "source_id": "dlna-uuid-server-1",
            "selection": { "kind": "container", "uri": "12$0" },
            "mode":      "append",
        });
        let parsed: EnqueueSelectionPayload =
            serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.source_id.as_deref(), Some("dlna-uuid-server-1"));
        match parsed.selection {
            SelectionInput::Container(sel) => assert_eq!(sel.uri, "12$0"),
            SelectionInput::Criteria(_) => {
                panic!("container JSON parsed as Criteria")
            }
        }
    }

    #[test]
    fn selection_input_criteria_parses_without_kind_field() {
        // Existing wire shape from pre-container callers: no
        // `kind`, `dimension` present. Must continue to
        // deserialise into the Criteria variant so pre-existing
        // callers keep working.
        let payload = serde_json::json!({
            "v": 1,
            "selection": {
                "dimension": "artist",
                "value":     "Beethoven",
            },
            "mode": "replace",
        });
        let parsed: EnqueueSelectionPayload =
            serde_json::from_value(payload).unwrap();
        assert!(parsed.source_id.is_none());
        match parsed.selection {
            SelectionInput::Criteria(c) => {
                assert_eq!(c.value, "Beethoven");
            }
            SelectionInput::Container(_) => {
                panic!("criteria JSON parsed as Container")
            }
        }
    }

    // ----- Container-shape helpers (pure) -----

    #[test]
    fn container_selection_kind_serde_only_accepts_container() {
        let ok: ContainerSelectionKind =
            serde_json::from_str("\"container\"").expect("container variant");
        matches!(ok, ContainerSelectionKind::Container);
        let err: Result<ContainerSelectionKind, _> =
            serde_json::from_str("\"other\"");
        assert!(err.is_err(), "unknown discriminator must refuse");
    }

    #[test]
    fn selection_input_container_carries_uri_verbatim() {
        // ContentDirectory objectIds routinely contain `$`,
        // `:`, and `/` — the parser MUST pass them through.
        let raw = serde_json::json!({
            "kind": "container",
            "uri":  "0/objects/12$34:56/x",
        });
        let parsed: SelectionInput = serde_json::from_value(raw).unwrap();
        match parsed {
            SelectionInput::Container(sel) => {
                assert_eq!(sel.uri, "0/objects/12$34:56/x");
            }
            SelectionInput::Criteria(_) => panic!("container variant expected"),
        }
    }

    // ----- dlna: URI parse -----

    #[test]
    fn parse_dlna_uri_splits_on_first_slash_after_scheme() {
        let (sid, oid) = parse_dlna_uri("dlna:uuid:abc/12$34").unwrap();
        assert_eq!(sid, "uuid:abc");
        assert_eq!(oid, "12$34");
    }

    #[test]
    fn parse_dlna_uri_preserves_slashes_within_object_id() {
        // ContentDirectory objectIds may themselves contain `/`;
        // only the FIRST `/` after the scheme separates
        // service_id from objectId.
        let (sid, oid) =
            parse_dlna_uri("dlna:uuid:aaaa-bbbb/0/objects/12$34").unwrap();
        assert_eq!(sid, "uuid:aaaa-bbbb");
        assert_eq!(oid, "0/objects/12$34");
    }

    #[test]
    fn parse_dlna_uri_refuses_missing_scheme() {
        assert!(parse_dlna_uri("uuid:abc/12$34").is_none());
        assert!(parse_dlna_uri("http://example.com").is_none());
    }

    #[test]
    fn parse_dlna_uri_refuses_empty_components() {
        assert!(parse_dlna_uri("dlna:").is_none());
        assert!(parse_dlna_uri("dlna:/").is_none());
        assert!(parse_dlna_uri("dlna:uuid:abc").is_none());
        assert!(parse_dlna_uri("dlna:uuid:abc/").is_none());
        assert!(parse_dlna_uri("dlna:/12$34").is_none());
    }

    // ----- resolve_uri_for_mpd: pass-through for non-dlna -----

    #[tokio::test]
    async fn resolve_uri_for_mpd_passes_through_non_dlna_unchanged() {
        // Http / library-relative URIs must not consult the
        // dispatcher — MPD's `add` gates on scheme itself.
        // Constructing a ctx without a dispatcher exposes the
        // non-consultation via the fact that a dispatcher-
        // requiring path would trip on the None handle.
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
        let registry = SourceRegistry::new();
        let disposition = crate::disposition_emitter::DispositionEmitter::new(
            Arc::new(NullAnn) as Arc<dyn SubjectAnnouncer>,
        );
        let skip = crate::skip_traversal::SkipTraversal::new(
            registry.clone(),
            disposition,
        );
        let ctx = QueueContext::new(
            PathBuf::from("/var/lib/evo/music"),
            registry,
            Arc::new(NullAnn),
            skip,
            None,
        );

        for uri in [
            "http://example.com/track.flac",
            "https://example.com/track.flac",
            "INTERNAL/album/track.flac",
            "",
        ] {
            let out = resolve_uri_for_mpd(&ctx, "test", uri).await.unwrap();
            assert_eq!(out.uri, uri);
            assert!(out.title.is_none());
            assert!(out.artist.is_none());
            assert!(out.album.is_none());
        }
    }

    #[tokio::test]
    async fn resolve_uri_for_mpd_refuses_malformed_dlna() {
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
        let registry = SourceRegistry::new();
        let disposition = crate::disposition_emitter::DispositionEmitter::new(
            Arc::new(NullAnn) as Arc<dyn SubjectAnnouncer>,
        );
        let skip = crate::skip_traversal::SkipTraversal::new(
            registry.clone(),
            disposition,
        );
        let ctx = QueueContext::new(
            PathBuf::from("/var/lib/evo/music"),
            registry,
            Arc::new(NullAnn),
            skip,
            None,
        );

        // Missing objectId after `dlna:<sid>/`.
        let err = resolve_uri_for_mpd(&ctx, "test", "dlna:uuid:abc/")
            .await
            .expect_err("malformed must refuse");
        match err {
            VerbError::Mpd { reason, .. } => {
                assert!(reason.contains("dlna:"));
                assert!(reason.contains("does not match"));
            }
            other => panic!("expected VerbError::Mpd, got {other:?}"),
        }
    }

    // ----- play_from_position validation -----

    #[test]
    fn validate_play_from_position_accepts_valid_index() {
        assert!(validate_play_from_position(0, 1).is_ok());
        assert!(validate_play_from_position(4, 5).is_ok());
        assert!(validate_play_from_position(0, u32::MAX).is_ok());
    }

    #[test]
    fn validate_play_from_position_rejects_empty_queue_with_queue_empty() {
        let err = validate_play_from_position(0, 0).unwrap_err();
        assert!(matches!(err, VerbError::QueueEmpty));
        let msg = format!("{err}");
        assert!(msg.contains("queue is empty"));
    }

    #[test]
    fn validate_play_from_position_rejects_out_of_range_at_boundary() {
        // position == length is invalid (zero-based indexing).
        let err = validate_play_from_position(5, 5).unwrap_err();
        match err {
            VerbError::PositionOutOfRange { position, length } => {
                assert_eq!(position, 5);
                assert_eq!(length, 5);
            }
            other => panic!("expected PositionOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn validate_play_from_position_rejects_far_out_of_range() {
        let err = validate_play_from_position(1_000, 5).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("1000"));
        assert!(msg.contains("5"));
    }

    #[test]
    fn play_from_position_payload_parses_v1_shape() {
        let json = serde_json::json!({ "v": 1, "position": 3 });
        let payload: PlayFromPositionPayload =
            serde_json::from_value(json).unwrap();
        assert_eq!(payload.v, 1);
        assert_eq!(payload.position, 3);
    }

    #[test]
    fn play_from_position_payload_rejects_missing_position() {
        let json = serde_json::json!({ "v": 1 });
        let err = serde_json::from_value::<PlayFromPositionPayload>(json)
            .unwrap_err();
        assert!(err.to_string().contains("position"));
    }

    #[test]
    fn check_version_flags_play_from_position_envelope_mismatch() {
        let err = check_version(2, "queue.play_from_position").unwrap_err();
        match err {
            VerbError::PayloadVersion {
                verb,
                got,
                expected,
            } => {
                assert_eq!(verb, "queue.play_from_position");
                assert_eq!(got, 2);
                assert_eq!(expected, QUEUE_PAYLOAD_VERSION);
            }
            other => panic!("expected PayloadVersion, got {other:?}"),
        }
    }

    // --- collect_dlna_container_leaves: descent shape + field
    //     contract regression guard ----------------------------

    /// Scripted `ShelfRequestDispatcher` that answers a
    /// `source.dlna.browse` for each `object_id` it recognises
    /// from a pre-populated map, and errors on any unrecognised
    /// object_id. Records the object_ids it was asked about in
    /// call order.
    struct ScriptedBrowseDispatcher {
        by_object_id: HashMap<String, serde_json::Value>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl evo_plugin_sdk::contract::shelf_dispatch::ShelfRequestDispatcher
        for ScriptedBrowseDispatcher
    {
        fn dispatch<'a>(
            &'a self,
            _shelf: &'a str,
            _request_type: &'a str,
            payload: Vec<u8>,
            _instance_id: Option<&'a str>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<u8>,
                            evo_plugin_sdk::contract::shelf_dispatch::ShelfDispatchError,
                        >,
                    > + Send
                    + 'a,
            >,
        >{
            Box::pin(async move {
                let body: serde_json::Value = serde_json::from_slice(&payload)
                    .expect("scripted dispatcher: payload is valid JSON");
                let oid = body
                    .get("object_id")
                    .and_then(|v| v.as_str())
                    .expect("scripted dispatcher: payload carries object_id")
                    .to_string();
                self.calls.lock().unwrap().push(oid.clone());
                let response =
                    self.by_object_id.get(&oid).unwrap_or_else(|| {
                        panic!(
                            "scripted dispatcher: no canned response for \
                         object_id {oid:?}"
                        )
                    });
                Ok(serde_json::to_vec(response).unwrap())
            })
        }
    }

    /// Build a browse-response entry shaped like the real
    /// `source.dlna.browse` handler emits — a container carries
    /// `kind: "directory"` + `path` + `name` but NO `uri`.
    fn dir_entry(name: &str, path: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "directory",
            "name": name,
            "path": path,
            "child_count": null,
        })
    }

    /// Build a browse-response entry shaped like the real
    /// `source.dlna.browse` handler emits — a leaf carries
    /// `kind: "file"` + `path` + `uri` (the stable
    /// `dlna:<service_id>/<object_id>` playback identity).
    fn file_entry(name: &str, path: &str, uri: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "file",
            "name": name,
            "path": path,
            "title": name,
            "uri": uri,
            "playable": true,
        })
    }

    fn browse_page_response(
        entries: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "status": "ok",
            "entries": entries,
            "page": 0,
            "page_size": 50,
            "total": entries.len(),
            "truncated": false,
            "next_page": serde_json::Value::Null,
        })
    }

    /// Regression guard for the pre-fix descent bug: the descent
    /// used to read `entry.uri` for the directory branch, but the
    /// peer-shelf browse response emits `uri` ONLY on leaves. So
    /// subcontainers never made it onto the DFS stack and
    /// enqueue collapsed to "direct-child leaves of the starting
    /// container only" — an operator tapping an artist / genre /
    /// folder / decade tile silently no-op'd the queue.
    ///
    /// This test walks a two-level tree
    ///   root  ->  [artist-A, artist-B]
    ///   A     ->  [track-A1, track-A2]
    ///   B     ->  [track-B1, track-B2, track-B3]
    /// and asserts:
    ///   1. The dispatcher was called for both artist object_ids
    ///      (not just root) — proving descent actually pushed
    ///      subcontainers.
    ///   2. All 5 leaf URIs came back in tree order.
    ///   3. The truncated flag is false (subtree exhausted, no
    ///      cap fired).
    #[tokio::test]
    async fn dlna_descent_walks_two_level_container_tree_via_path_field() {
        let mut by_oid: HashMap<String, serde_json::Value> = HashMap::new();
        by_oid.insert(
            "root".into(),
            browse_page_response(vec![
                dir_entry("Artist A", "oid-artist-a"),
                dir_entry("Artist B", "oid-artist-b"),
            ]),
        );
        by_oid.insert(
            "oid-artist-a".into(),
            browse_page_response(vec![
                file_entry("A1", "oid-a1", "dlna:svc/oid-a1"),
                file_entry("A2", "oid-a2", "dlna:svc/oid-a2"),
            ]),
        );
        by_oid.insert(
            "oid-artist-b".into(),
            browse_page_response(vec![
                file_entry("B1", "oid-b1", "dlna:svc/oid-b1"),
                file_entry("B2", "oid-b2", "dlna:svc/oid-b2"),
                file_entry("B3", "oid-b3", "dlna:svc/oid-b3"),
            ]),
        );
        let dispatcher = ScriptedBrowseDispatcher {
            by_object_id: by_oid,
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let (leaves, truncated) = collect_dlna_container_leaves(
            &dispatcher,
            "svc",
            "root",
            /* max_depth  */ 6,
            /* max_tracks */ 100,
            /* page_size  */ 50,
        )
        .await
        .expect("descent succeeds");

        assert_eq!(
            leaves,
            vec![
                "dlna:svc/oid-a1".to_string(),
                "dlna:svc/oid-a2".to_string(),
                "dlna:svc/oid-b1".to_string(),
                "dlna:svc/oid-b2".to_string(),
                "dlna:svc/oid-b3".to_string(),
            ],
            "leaves collected in tree order across both subcontainers"
        );
        assert!(!truncated, "subtree fully exhausted within caps");

        let calls = dispatcher.calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![
                "root".to_string(),
                "oid-artist-a".to_string(),
                "oid-artist-b".to_string(),
            ],
            "descent visited both subcontainers — the pre-fix bug \
             visited only root"
        );
    }

    /// Track-cap regression guard: hitting `max_tracks` mid-page
    /// aborts descent immediately and sets `truncated`.
    #[tokio::test]
    async fn dlna_descent_caps_tracks_and_marks_truncated() {
        let mut by_oid: HashMap<String, serde_json::Value> = HashMap::new();
        by_oid.insert(
            "root".into(),
            browse_page_response(vec![
                file_entry("t1", "o1", "dlna:svc/o1"),
                file_entry("t2", "o2", "dlna:svc/o2"),
                file_entry("t3", "o3", "dlna:svc/o3"),
            ]),
        );
        let dispatcher = ScriptedBrowseDispatcher {
            by_object_id: by_oid,
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let (leaves, truncated) = collect_dlna_container_leaves(
            &dispatcher,
            "svc",
            "root",
            6,
            /* max_tracks = */ 2,
            50,
        )
        .await
        .expect("descent succeeds");

        assert_eq!(
            leaves,
            vec!["dlna:svc/o1".to_string(), "dlna:svc/o2".to_string()]
        );
        assert!(truncated, "cap fired at 2 tracks");
    }

    /// Depth-cap regression guard: at `max_depth = 1`, the root is
    /// browsed but its subcontainers are refused entry and
    /// `truncated` is set.
    #[tokio::test]
    async fn dlna_descent_caps_depth_and_marks_truncated() {
        let mut by_oid: HashMap<String, serde_json::Value> = HashMap::new();
        by_oid.insert(
            "root".into(),
            browse_page_response(vec![dir_entry("child", "oid-child")]),
        );
        // deliberately don't register "oid-child": if descent
        // reached it the dispatcher would panic.
        let dispatcher = ScriptedBrowseDispatcher {
            by_object_id: by_oid,
            calls: std::sync::Mutex::new(Vec::new()),
        };

        let (leaves, truncated) = collect_dlna_container_leaves(
            &dispatcher,
            "svc",
            "root",
            /* max_depth = */ 1,
            100,
            50,
        )
        .await
        .expect("descent succeeds");

        assert!(leaves.is_empty(), "no leaves at depth 1");
        assert!(truncated, "depth cap fired on child");
    }
}
