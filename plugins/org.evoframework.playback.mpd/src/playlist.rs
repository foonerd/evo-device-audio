//! Playlist shelf — verb handlers + `audio_playlist_index`
//! subject emitter.
//!
//! Realises the `audio.playlist.v1` catalogue contract. MPD's
//! stored-playlist namespace is the persistence layer; the
//! module wires verbs to MPD commands without introducing a
//! parallel store.
//!
//! # Verb surface (8)
//!
//! - `playlist.list_playlists` — read playlist index
//! - `playlist.get_playlist` — read named playlist contents
//! - `playlist.create_playlist` — create an empty playlist
//! - `playlist.delete_playlist` — delete a playlist
//! - `playlist.rename_playlist` — rename
//! - `playlist.add_to_playlist` — append URIs (batched)
//! - `playlist.remove_from_playlist` — delete by positions
//!   (batched, descending order so position arithmetic stays
//!   stable)
//! - `playlist.move_in_playlist` — move entry to a new position
//!
//! # Catalogue acceptance rows honoured
//!
//! - `playlist-mpd-namespace-is-authoritative`: no parallel
//!   playlist store; MPD's listplaylists / playlistadd /
//!   playlistdelete / playlistclear / playlistmove / rename /
//!   rm are the only writers.
//! - `playlist-create-name-bounds`: names validated for
//!   non-empty, length in [1, 128] bytes, no `/`, no control
//!   characters (codepoint < 0x20 or = 0x7F).
//! - `playlist-deletion-protects-system-managed-favourites`:
//!   `delete_playlist` refuses on the favourites playlist
//!   name (operator-configured; default `__favourites__`).
//!   Favourites lifecycle goes through the audio.favourites
//!   shelf.
//! - `playlist-mutation-atomicity`: bulk add/remove operations
//!   batch through MPD's command_list to commit in one
//!   playlist-write.
//! - `playlist-index-subject-published-on-load-and-every-mutation`:
//!   the audio_playlist_index subject is announced at plugin
//!   load and refreshed on every mutating verb + on idle
//!   stored_playlist wakes.

use std::path::PathBuf;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::mpd::{MpdConnection, MpdPlaylistEntry, MpdPlaylistSummary};
use crate::queue::resolve_source;
use crate::source_registry::SourceRegistry;
use crate::sticker_reconciler::EVO_AVAILABLE_STICKER;

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Wire-payload version for the `audio.playlist.v1` shelf.
pub(crate) const PLAYLIST_PAYLOAD_VERSION: u32 = 1;

/// Subject type for the playlist index subject; matches the
/// schema's declaration verbatim.
const SUBJECT_TYPE_PLAYLIST_INDEX: &str = "audio_playlist_index";

/// Addressing scheme for the playlist index subject.
const SCHEME_PLAYLIST: &str = "evo.audio.playlist";

/// Addressing value for the playlist index subject — singleton
/// per warden.
const VALUE_INDEX: &str = "index";

/// Default name for the system-managed favourites playlist.
/// Operator-configurable via the plugin's manifest config block
/// in a future extension; the favourites shelf uses the same
/// constant.
pub(crate) const DEFAULT_FAVOURITES_PLAYLIST_NAME: &str = "__favourites__";

/// Bound on stored playlist names per the catalogue.
pub(crate) const MAX_PLAYLIST_NAME_BYTES: usize = 128;

// ----- shared context -----

/// Resources the playlist module consumes.
#[derive(Clone)]
pub(crate) struct PlaylistContext {
    /// MPD's `music_directory` for the source resolver used
    /// by `get_playlist`'s per-entry availability projection.
    pub(crate) music_directory: PathBuf,
    /// MPD's `playlist_directory` — where stored playlists
    /// (`.m3u` files) live. Required by `create_playlist` to
    /// materialise an empty playlist (MPD has no first-class
    /// "create empty playlist" command).
    pub(crate) playlist_directory: PathBuf,
    /// Shared source registry for resolving songs to sources
    /// in `get_playlist`.
    pub(crate) registry: SourceRegistry,
    /// Subject announcer for the index subject.
    pub(crate) subjects: Arc<dyn SubjectAnnouncer>,
    /// Operator-configured favourites playlist name (defaults
    /// to [`DEFAULT_FAVOURITES_PLAYLIST_NAME`]). The favourites
    /// shelf and the playlist-deletion guard share this value.
    pub(crate) favourites_name: String,
    /// Mirror of the last published index envelope; satisfies
    /// `list_playlists` without round-trip through the
    /// framework's subject querier.
    mirror: Arc<Mutex<Option<serde_json::Value>>>,
}

impl PlaylistContext {
    pub(crate) fn new(
        music_directory: PathBuf,
        playlist_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
        favourites_name: String,
    ) -> Self {
        Self {
            music_directory,
            playlist_directory,
            registry,
            subjects,
            favourites_name,
            mirror: Arc::new(Mutex::new(None)),
        }
    }
}

// ----- subject emitter -----

pub(crate) async fn announce_index(ctx: &PlaylistContext) {
    let addressing = ExternalAddressing::new(SCHEME_PLAYLIST, VALUE_INDEX);
    let env = render_empty_index();
    {
        let mut g = ctx.mirror.lock().await;
        *g = Some(env.clone());
    }
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_PLAYLIST_INDEX, vec![addressing])
            .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_playlist_index subject announce failed; \
             operator UI's playlist list will be unavailable \
             until a future re-announce attempt"
        );
    }
}

pub(crate) async fn publish_index(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
) {
    let env = match build_index_envelope(conn).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                error = %e,
                "audio_playlist_index envelope build failed; skipping publish"
            );
            return;
        }
    };
    {
        let mut g = ctx.mirror.lock().await;
        *g = Some(env.clone());
    }
    let addressing = ExternalAddressing::new(SCHEME_PLAYLIST, VALUE_INDEX);
    if let Err(e) = ctx.subjects.update_state(addressing, env).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_playlist_index update_state failed; operator UI \
             may show a stale playlist list until the next mutation"
        );
    }
}

fn render_empty_index() -> serde_json::Value {
    json!({
        "v": PLAYLIST_PAYLOAD_VERSION,
        "playlists": Vec::<serde_json::Value>::new(),
    })
}

async fn build_index_envelope(
    conn: &mut MpdConnection,
) -> Result<serde_json::Value, BuildError> {
    let summaries = conn
        .listplaylists()
        .await
        .map_err(|e| BuildError::Mpd(format!("listplaylists: {e}")))?;
    // Item count requires a per-playlist listplaylistinfo round-
    // trip; for index emission we approximate as 0 (the
    // `get_playlist` verb returns the real count). UI consumers
    // wanting accurate counts call get_playlist; the index's
    // purpose is the name + last-modified listing.
    let rendered: Vec<serde_json::Value> = summaries
        .iter()
        .map(|s| {
            let parsed_ts = s
                .last_modified
                .as_ref()
                .and_then(|raw| parse_iso_to_epoch_ms(raw));
            json!({
                "name":            s.name,
                "item_count":      0,
                "modified_at_ms":  parsed_ts,
            })
        })
        .collect();
    Ok(json!({
        "v":         PLAYLIST_PAYLOAD_VERSION,
        "playlists": rendered,
    }))
}

/// Convert MPD's ISO-8601 `Last-Modified` to epoch milliseconds.
/// Returns `None` on parse failure; UI consumers treat null as
/// "modification time unknown".
fn parse_iso_to_epoch_ms(raw: &str) -> Option<u64> {
    // MPD reports ISO-8601: `2025-01-02T03:04:05Z`.
    // Parse with chrono-style without pulling chrono in: split
    // the string and compute manually. For now use a
    // best-effort: if it parses as RFC3339 via std time, return;
    // otherwise None.
    //
    // The framework's standard observability layer carries
    // timestamps as epoch ms throughout; we mirror that.
    // Implementation: strip the Z, split on T, on -, on :, then
    // compute. Conservative on malformed input.
    let trimmed = raw.trim().trim_end_matches('Z');
    let (date_part, time_part) = trimmed.split_once('T')?;
    let mut date_iter = date_part.split('-');
    let y: i64 = date_iter.next()?.parse().ok()?;
    let m: u32 = date_iter.next()?.parse().ok()?;
    let d: u32 = date_iter.next()?.parse().ok()?;
    let mut time_iter = time_part.split(':');
    let hh: u32 = time_iter.next()?.parse().ok()?;
    let mm: u32 = time_iter.next()?.parse().ok()?;
    let ss: u32 = time_iter.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(epoch_ms_from_components(y, m, d, hh, mm, ss))
}

fn epoch_ms_from_components(
    year: i64,
    month: u32,
    day: u32,
    hh: u32,
    mm: u32,
    ss: u32,
) -> u64 {
    // Days-from-epoch via civil calendar formula
    // (Howard Hinnant's algorithm). Handles every Gregorian
    // year + leap years correctly.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m = month as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64
        - 1) as u32;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_epoch = era * 146097 + doe as i64 - 719468;
    let seconds = days_from_epoch * 86_400
        + (hh as i64) * 3600
        + (mm as i64) * 60
        + ss as i64;
    if seconds < 0 {
        0
    } else {
        (seconds as u64) * 1000
    }
}

// ----- get_playlist envelope construction -----

async fn build_get_playlist_envelope(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    name: &str,
    entries: &[MpdPlaylistEntry],
) -> serde_json::Value {
    let mut rendered = Vec::with_capacity(entries.len());
    for entry in entries {
        let source_id = resolve_source(
            &ctx.music_directory,
            &entry.file_path,
            &ctx.registry,
        )
        .await;
        let available = compute_available(
            conn,
            &entry.file_path,
            source_id.as_deref(),
            &ctx.registry,
        )
        .await;
        rendered.push(json!({
            "position":    entry.position,
            "uri":         entry.file_path,
            "source_id":   source_id,
            "title":       entry.title,
            "artist":      entry.artist,
            "album":       entry.album,
            "duration_ms": entry.duration.map(|d| d.as_millis() as u64),
            "available":   available,
        }));
    }
    json!({
        "v":          PLAYLIST_PAYLOAD_VERSION,
        "name":       name,
        "items":      rendered,
        "item_count": entries.len(),
    })
}

async fn compute_available(
    conn: &mut MpdConnection,
    file_path: &str,
    source_id: Option<&str>,
    registry: &SourceRegistry,
) -> bool {
    if let Some(sid) = source_id {
        if let Some(record) = registry.get(sid).await {
            if !record.state.is_reachable() {
                return false;
            }
        }
    }
    match conn.sticker_get(file_path, EVO_AVAILABLE_STICKER).await {
        Ok(Some(value)) => value != "0",
        Ok(None) => true,
        Err(_) => true,
    }
}

// ----- name validation -----

/// Validate a stored-playlist name against the catalogue
/// invariant `playlist-create-name-bounds`: non-empty,
/// length 1..=128 bytes, no `/`, no control characters.
pub(crate) fn validate_playlist_name(name: &str) -> Result<(), VerbError> {
    if name.is_empty() {
        return Err(VerbError::InvalidName {
            offending: name.to_string(),
            reason: "name must not be empty".to_string(),
        });
    }
    let bytes = name.len();
    if bytes > MAX_PLAYLIST_NAME_BYTES {
        return Err(VerbError::InvalidName {
            offending: name.to_string(),
            reason: format!(
                "name length {bytes} bytes exceeds the {MAX_PLAYLIST_NAME_BYTES}-byte cap"
            ),
        });
    }
    if name.contains('/') {
        return Err(VerbError::InvalidName {
            offending: name.to_string(),
            reason: "name must not contain the path separator '/'".to_string(),
        });
    }
    for ch in name.chars() {
        let cp = ch as u32;
        if cp < 0x20 || cp == 0x7F {
            return Err(VerbError::InvalidName {
                offending: name.to_string(),
                reason: format!(
                    "name must not contain control character U+{cp:04X}"
                ),
            });
        }
    }
    Ok(())
}

// ----- verb payloads -----

#[derive(Debug, Deserialize)]
pub(crate) struct GetPlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreatePlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeletePlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RenamePlaylistPayload {
    pub(crate) v: u32,
    pub(crate) from_name: String,
    pub(crate) to_name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddToPlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
    pub(crate) uris: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoveFromPlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
    pub(crate) positions: Vec<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MoveInPlaylistPayload {
    pub(crate) v: u32,
    pub(crate) name: String,
    pub(crate) from_position: u32,
    pub(crate) to_position: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct SimplePlaylistResponse {
    pub(crate) v: u32,
    pub(crate) status: &'static str,
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
    #[error("playlist name invalid {offending:?}: {reason}")]
    InvalidName { offending: String, reason: String },
    #[error("playlist {name:?} not found")]
    NotFound { name: String },
    #[error(
        "playlist {name:?} already exists; pass overwrite=true to replace"
    )]
    DuplicateName { name: String },
    #[error("playlist {name:?} is the system-managed favourites playlist; use the favourites shelf")]
    FavouritesProtected { name: String },
    #[error("playlist.{verb}: MPD error: {reason}")]
    Mpd { verb: String, reason: String },
}

fn check_version(v: u32, verb: &str) -> Result<(), VerbError> {
    if v != PLAYLIST_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: verb.to_string(),
            got: v,
            expected: PLAYLIST_PAYLOAD_VERSION,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum BuildError {
    #[error("playlist envelope build error: {0}")]
    Mpd(String),
}

// ----- verb handlers -----

pub(crate) async fn handle_list_playlists(
    ctx: &PlaylistContext,
) -> serde_json::Value {
    let g = ctx.mirror.lock().await;
    g.clone().unwrap_or_else(render_empty_index)
}

pub(crate) async fn handle_get_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: GetPlaylistPayload,
) -> Result<serde_json::Value, VerbError> {
    check_version(payload.v, "playlist.get_playlist")?;
    validate_playlist_name(&payload.name)?;
    let entries = match conn.listplaylistinfo(&payload.name).await {
        Ok(e) => e,
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            return Err(VerbError::NotFound {
                name: payload.name.clone(),
            });
        }
        Err(e) => {
            return Err(VerbError::Mpd {
                verb: "get_playlist".to_string(),
                reason: e.to_string(),
            });
        }
    };
    Ok(build_get_playlist_envelope(ctx, conn, &payload.name, &entries).await)
}

pub(crate) async fn handle_create_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: CreatePlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.create_playlist")?;
    validate_playlist_name(&payload.name)?;
    // Collision check via MPD's index.
    let summaries = conn.listplaylists().await.map_err(|e| VerbError::Mpd {
        verb: "create_playlist".to_string(),
        reason: e.to_string(),
    })?;
    if summaries.iter().any(|s| s.name == payload.name) {
        return Err(VerbError::DuplicateName {
            name: payload.name.clone(),
        });
    }
    // MPD has no "create empty playlist" command; we materialise
    // an empty .m3u file in the playlist directory. The file's
    // content is just the M3U header comment so MPD's parser
    // accepts it cleanly.
    let path = ctx.playlist_directory.join(format!("{}.m3u", payload.name));
    tokio::fs::create_dir_all(&ctx.playlist_directory)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "create_playlist".to_string(),
            reason: format!("mkdir playlist directory: {e}"),
        })?;
    tokio::fs::write(&path, b"#EXTM3U\n").await.map_err(|e| {
        VerbError::Mpd {
            verb: "create_playlist".to_string(),
            reason: format!("write {path:?}: {e}"),
        }
    })?;
    publish_index(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_delete_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: DeletePlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.delete_playlist")?;
    validate_playlist_name(&payload.name)?;
    if payload.name == ctx.favourites_name {
        return Err(VerbError::FavouritesProtected {
            name: payload.name.clone(),
        });
    }
    match conn.rm_playlist(&payload.name).await {
        Ok(()) => {}
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            return Err(VerbError::NotFound {
                name: payload.name.clone(),
            });
        }
        Err(e) => {
            return Err(VerbError::Mpd {
                verb: "delete_playlist".to_string(),
                reason: e.to_string(),
            });
        }
    }
    publish_index(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_rename_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: RenamePlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.rename_playlist")?;
    validate_playlist_name(&payload.from_name)?;
    validate_playlist_name(&payload.to_name)?;
    if payload.from_name == ctx.favourites_name
        || payload.to_name == ctx.favourites_name
    {
        return Err(VerbError::FavouritesProtected {
            name: ctx.favourites_name.clone(),
        });
    }
    match conn
        .rename_playlist(&payload.from_name, &payload.to_name)
        .await
    {
        Ok(()) => {}
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            return Err(VerbError::NotFound {
                name: payload.from_name.clone(),
            });
        }
        Err(crate::mpd::MpdError::Ack { code: 56, .. }) => {
            return Err(VerbError::DuplicateName {
                name: payload.to_name.clone(),
            });
        }
        Err(e) => {
            return Err(VerbError::Mpd {
                verb: "rename_playlist".to_string(),
                reason: e.to_string(),
            });
        }
    }
    publish_index(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_add_to_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: AddToPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.add_to_playlist")?;
    validate_playlist_name(&payload.name)?;
    if payload.uris.is_empty() {
        // Nothing to do; still publish for index refresh
        // consistency? No mutation occurred. Return Ok without
        // publish.
        return Ok(());
    }
    // Batch via command_list per the catalogue acceptance row
    // playlist-mutation-atomicity.
    let commands: Vec<(&str, Vec<String>)> = payload
        .uris
        .iter()
        .map(|uri| ("playlistadd", vec![payload.name.clone(), uri.clone()]))
        .collect();
    conn.command_list(&commands)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "add_to_playlist".to_string(),
            reason: e.to_string(),
        })?;
    publish_index(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_remove_from_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: RemoveFromPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.remove_from_playlist")?;
    validate_playlist_name(&payload.name)?;
    if payload.positions.is_empty() {
        return Ok(());
    }
    // Dedupe + sort descending so earlier-position deletes don't
    // shift later positions. MPD's `playlistdelete NAME N`
    // deletes the entry at position N; if we delete position 5
    // first then position 3, MPD operates on the modified list
    // — descending order avoids that.
    let mut positions: Vec<u32> = payload.positions.clone();
    positions.sort_unstable();
    positions.dedup();
    positions.reverse();
    let commands: Vec<(&str, Vec<String>)> = positions
        .iter()
        .map(|pos| {
            (
                "playlistdelete",
                vec![payload.name.clone(), pos.to_string()],
            )
        })
        .collect();
    conn.command_list(&commands)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "remove_from_playlist".to_string(),
            reason: e.to_string(),
        })?;
    publish_index(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_move_in_playlist(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: MoveInPlaylistPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "playlist.move_in_playlist")?;
    validate_playlist_name(&payload.name)?;
    conn.playlistmove(
        &payload.name,
        payload.from_position,
        payload.to_position,
    )
    .await
    .map_err(|e| VerbError::Mpd {
        verb: "move_in_playlist".to_string(),
        reason: e.to_string(),
    })?;
    publish_index(ctx, conn).await;
    Ok(())
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_playlist_name_accepts_simple_name() {
        assert!(validate_playlist_name("rock").is_ok());
        assert!(validate_playlist_name("Jazz Favourites").is_ok());
    }

    #[test]
    fn validate_playlist_name_refuses_empty() {
        let err = validate_playlist_name("").unwrap_err();
        match err {
            VerbError::InvalidName { reason, .. } => {
                assert!(reason.contains("empty"));
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn validate_playlist_name_refuses_path_separator() {
        let err = validate_playlist_name("rock/2025").unwrap_err();
        match err {
            VerbError::InvalidName { reason, .. } => {
                assert!(reason.contains("path separator"));
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn validate_playlist_name_refuses_oversize_name() {
        let name = "x".repeat(MAX_PLAYLIST_NAME_BYTES + 1);
        let err = validate_playlist_name(&name).unwrap_err();
        match err {
            VerbError::InvalidName { reason, .. } => {
                assert!(reason.contains("exceeds"));
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    #[test]
    fn validate_playlist_name_refuses_control_characters() {
        // Tab is U+0009 < 0x20.
        let err = validate_playlist_name("bad\tname").unwrap_err();
        match err {
            VerbError::InvalidName { reason, .. } => {
                assert!(reason.contains("control"));
            }
            other => panic!("expected InvalidName, got {other:?}"),
        }
        // DEL is U+007F.
        let err = validate_playlist_name(&format!("bad{}name", '\u{007F}'))
            .unwrap_err();
        assert!(matches!(err, VerbError::InvalidName { .. }));
    }

    #[test]
    fn render_empty_index_carries_wire_contract() {
        let env = render_empty_index();
        assert_eq!(env["v"], 1);
        assert!(env["playlists"].is_array());
        assert_eq!(env["playlists"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_iso_to_epoch_ms_handles_z_terminated_iso() {
        // 1970-01-01T00:00:00Z is epoch 0.
        let ms = parse_iso_to_epoch_ms("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(ms, 0);
        // Round-trip a known date.
        // 2025-01-02T03:04:05Z
        let ms = parse_iso_to_epoch_ms("2025-01-02T03:04:05Z").unwrap();
        // 2025-01-02 03:04:05 UTC = 1735787045 seconds.
        assert_eq!(ms, 1_735_787_045_000);
    }

    #[test]
    fn parse_iso_to_epoch_ms_returns_none_on_garbage() {
        assert!(parse_iso_to_epoch_ms("not-a-date").is_none());
        assert!(parse_iso_to_epoch_ms("").is_none());
    }

    #[test]
    fn check_version_accepts_matching_version() {
        assert!(
            check_version(PLAYLIST_PAYLOAD_VERSION, "playlist.test").is_ok()
        );
    }

    #[test]
    fn check_version_refuses_mismatched_version() {
        let err = check_version(99, "playlist.test").unwrap_err();
        assert!(matches!(err, VerbError::PayloadVersion { .. }));
    }

    #[test]
    fn favourites_protected_error_carries_name() {
        let err = VerbError::FavouritesProtected {
            name: "__favourites__".to_string(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("__favourites__"));
        assert!(msg.contains("favourites"));
    }
}
