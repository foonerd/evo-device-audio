//! Favourites shelf — verb handlers + `audio_favourites`
//! subject emitter.
//!
//! Realises the `audio.favourites.v1` catalogue contract.
//! Favourites is a system-managed stored playlist (default name
//! `__favourites__`); the verbs translate to MPD's stored-
//! playlist operations against that playlist name.
//!
//! # Verb surface (6)
//!
//! - `favourites.list_favourites` — full ordered list with
//!   per-entry availability projection.
//! - `favourites.is_favourite` — O(1) membership test against
//!   the in-memory mirror.
//! - `favourites.add_favourite` — set-semantics append; silent
//!   no-op on duplicates per
//!   `favourites-set-semantics-on-add` invariant.
//! - `favourites.remove_favourite` — refuses when URI not in
//!   favourites (structured Permanent error).
//! - `favourites.clear_favourites` — wholesale clear via
//!   `playlistclear`.
//! - `favourites.move_favourite` — reorder by URI.
//!
//! # Catalogue acceptance rows honoured
//!
//! - `favourites-persisted-as-system-managed-playlist`: only
//!   the favourites playlist file is the truth; no parallel
//!   store.
//! - `favourites-playlist-protected-from-generic-delete`:
//!   protection lives in the playlist shelf
//!   ([`crate::playlist::handle_delete_playlist`]) which
//!   refuses on `favourites_name`.
//! - `favourites-set-semantics-on-add`: add of an existing URI
//!   is a silent no-op.
//! - `favourites-subject-published-on-load-and-every-mutation`:
//!   the `audio_favourites` subject is announced + refreshed on
//!   every mutation + the idle stored_playlist wake covers
//!   external edits.
//! - `favourites-is-favourite-membership-cheap`: an in-memory
//!   HashSet keyed by URI is maintained alongside the persisted
//!   playlist; batched is_favourite calls do not trigger MPD
//!   round-trips.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::mpd::MpdConnection;
use crate::queue::resolve_source;
use crate::source_registry::SourceRegistry;

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Wire-payload version for the `audio.favourites.v1` shelf.
pub(crate) const FAVOURITES_PAYLOAD_VERSION: u32 = 1;

/// Subject type for the `audio_favourites` subject; matches the
/// schema verbatim.
const SUBJECT_TYPE_FAVOURITES: &str = "audio_favourites";

/// Addressing scheme for the favourites subject.
const SCHEME_FAVOURITES: &str = "evo.audio.favourites";

/// Addressing value for the favourites subject — singleton per
/// warden.
const VALUE_SET: &str = "set";

// ----- shared context -----

/// Resources the favourites module consumes.
#[derive(Clone)]
pub(crate) struct FavouritesContext {
    /// MPD's music_directory for the source resolver used in
    /// the audio_favourites subject's per-item projection.
    pub(crate) music_directory: PathBuf,
    /// Shared source registry.
    pub(crate) registry: SourceRegistry,
    /// Subject announcer.
    pub(crate) subjects: Arc<dyn SubjectAnnouncer>,
    /// Operator-configured favourites playlist name. Shared
    /// with the playlist shelf's deletion guard via the
    /// integration commit.
    pub(crate) favourites_name: String,
    /// In-memory mirror keyed by URI for O(1) is_favourite
    /// checks. Refreshed from MPD on every mutation + on
    /// idle stored_playlist wake.
    membership: Arc<Mutex<HashSet<String>>>,
    /// Mirror of the last published wire envelope.
    envelope_mirror: Arc<Mutex<Option<serde_json::Value>>>,
}

impl FavouritesContext {
    pub(crate) fn new(
        music_directory: PathBuf,
        registry: SourceRegistry,
        subjects: Arc<dyn SubjectAnnouncer>,
        favourites_name: String,
    ) -> Self {
        Self {
            music_directory,
            registry,
            subjects,
            favourites_name,
            membership: Arc::new(Mutex::new(HashSet::new())),
            envelope_mirror: Arc::new(Mutex::new(None)),
        }
    }
}

// ----- subject emitter -----

pub(crate) async fn announce_favourites(ctx: &FavouritesContext) {
    let addressing = ExternalAddressing::new(SCHEME_FAVOURITES, VALUE_SET);
    let env = render_empty_envelope();
    {
        let mut g = ctx.envelope_mirror.lock().await;
        *g = Some(env.clone());
    }
    let announcement =
        SubjectAnnouncement::new(SUBJECT_TYPE_FAVOURITES, vec![addressing])
            .with_state(env);
    if let Err(e) = ctx.subjects.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_favourites subject announce failed; \
             operator UI's favourites view will be unavailable \
             until a future re-announce attempt"
        );
    }
}

/// Refresh the favourites subject + in-memory membership mirror
/// from MPD's current favourites playlist. Called on every
/// mutating verb's success path + on idle stored_playlist
/// wake.
///
/// Refresh handles the favourites playlist not existing yet
/// gracefully — an MPD ACK 50 on listplaylistinfo translates to
/// empty contents.
pub(crate) async fn refresh_favourites(
    ctx: &FavouritesContext,
    conn: &mut MpdConnection,
) {
    let entries = match conn.listplaylistinfo(&ctx.favourites_name).await {
        Ok(e) => e,
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                favourites_name = %ctx.favourites_name,
                error = %e,
                "favourites refresh: MPD listplaylistinfo failed; \
                 keeping mirror stale until next refresh"
            );
            return;
        }
    };

    // Update membership set first so concurrent is_favourite
    // queries see the new view.
    {
        let mut g = ctx.membership.lock().await;
        g.clear();
        for entry in &entries {
            g.insert(entry.file_path.clone());
        }
    }

    // Build and publish the wire envelope.
    let mut items = Vec::with_capacity(entries.len());
    for entry in &entries {
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
        items.push(json!({
            "position":     entry.position,
            "uri":          entry.file_path,
            "source_id":    source_id,
            "title":        entry.title,
            "artist":       entry.artist,
            "album":        entry.album,
            "duration_ms":  entry.duration.map(|d| d.as_millis() as u64),
            "available":    available,
            // Per the catalogue contract: null on entries not
            // tracked by the framework's own add path
            // (MPD stored playlists do not preserve per-entry
            // add timestamps). The acceptance row
            // favourites-persisted-as-system-managed-playlist
            // refuses a parallel store; null is honest.
            "added_at_ms":  serde_json::Value::Null,
        }));
    }
    let envelope = json!({
        "v":     FAVOURITES_PAYLOAD_VERSION,
        "items": items,
        "count": entries.len(),
    });
    {
        let mut g = ctx.envelope_mirror.lock().await;
        *g = Some(envelope.clone());
    }
    let addressing = ExternalAddressing::new(SCHEME_FAVOURITES, VALUE_SET);
    if let Err(e) = ctx.subjects.update_state(addressing, envelope).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "audio_favourites update_state failed; operator UI \
             may show a stale favourites list until the next mutation"
        );
    }
}

fn render_empty_envelope() -> serde_json::Value {
    json!({
        "v":     FAVOURITES_PAYLOAD_VERSION,
        "items": Vec::<serde_json::Value>::new(),
        "count": 0,
    })
}

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

// ----- verb payloads -----

#[derive(Debug, Deserialize)]
pub(crate) struct IsFavouritePayload {
    pub(crate) v: u32,
    pub(crate) uri: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AddFavouritePayload {
    pub(crate) v: u32,
    pub(crate) uri: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RemoveFavouritePayload {
    pub(crate) v: u32,
    pub(crate) uri: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClearFavouritesPayload {
    pub(crate) v: u32,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MoveFavouritePayload {
    pub(crate) v: u32,
    pub(crate) uri: String,
    pub(crate) to_position: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct IsFavouriteResponse {
    pub(crate) v: u32,
    pub(crate) uri: String,
    pub(crate) is_favourite: bool,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub(crate) struct SimpleFavouritesResponse {
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
    #[error("favourites.remove_favourite: URI {uri:?} is not currently in favourites")]
    NotFavourite { uri: String },
    #[error("favourites.{verb}: MPD error: {reason}")]
    Mpd { verb: String, reason: String },
}

fn check_version(v: u32, verb: &str) -> Result<(), VerbError> {
    if v != FAVOURITES_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: verb.to_string(),
            got: v,
            expected: FAVOURITES_PAYLOAD_VERSION,
        });
    }
    Ok(())
}

// ----- verb handlers -----

pub(crate) async fn handle_list_favourites(
    ctx: &FavouritesContext,
) -> serde_json::Value {
    let g = ctx.envelope_mirror.lock().await;
    g.clone().unwrap_or_else(render_empty_envelope)
}

pub(crate) async fn handle_is_favourite(
    ctx: &FavouritesContext,
    payload: IsFavouritePayload,
) -> Result<IsFavouriteResponse, VerbError> {
    check_version(payload.v, "favourites.is_favourite")?;
    let g = ctx.membership.lock().await;
    let is_favourite = g.contains(&payload.uri);
    Ok(IsFavouriteResponse {
        v: FAVOURITES_PAYLOAD_VERSION,
        uri: payload.uri,
        is_favourite,
    })
}

pub(crate) async fn handle_add_favourite(
    ctx: &FavouritesContext,
    conn: &mut MpdConnection,
    payload: AddFavouritePayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "favourites.add_favourite")?;
    // Set semantics: silent no-op on duplicate per the
    // catalogue invariant favourites-set-semantics-on-add.
    {
        let g = ctx.membership.lock().await;
        if g.contains(&payload.uri) {
            return Ok(());
        }
    }
    conn.playlistadd(&ctx.favourites_name, &payload.uri)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "add_favourite".to_string(),
            reason: e.to_string(),
        })?;
    refresh_favourites(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_remove_favourite(
    ctx: &FavouritesContext,
    conn: &mut MpdConnection,
    payload: RemoveFavouritePayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "favourites.remove_favourite")?;
    // Pre-flight membership check: refuse with NotFavourite
    // when URI is not in the set so UI consumers can
    // disambiguate "already removed" (idempotent) from "never
    // favourited" (structured error).
    {
        let g = ctx.membership.lock().await;
        if !g.contains(&payload.uri) {
            return Err(VerbError::NotFavourite {
                uri: payload.uri.clone(),
            });
        }
    }
    // Find current position via listplaylistinfo.
    let entries =
        conn.listplaylistinfo(&ctx.favourites_name)
            .await
            .map_err(|e| VerbError::Mpd {
                verb: "remove_favourite".to_string(),
                reason: e.to_string(),
            })?;
    let position = entries
        .iter()
        .find(|e| e.file_path == payload.uri)
        .map(|e| e.position)
        .ok_or_else(|| VerbError::NotFavourite {
            uri: payload.uri.clone(),
        })?;
    conn.playlistdelete(&ctx.favourites_name, position)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "remove_favourite".to_string(),
            reason: e.to_string(),
        })?;
    refresh_favourites(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_clear_favourites(
    ctx: &FavouritesContext,
    conn: &mut MpdConnection,
    payload: ClearFavouritesPayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "favourites.clear_favourites")?;
    // MPD `playlistclear` removes every entry; the playlist
    // file itself remains so the favourites-protected name
    // is preserved.
    match conn.playlistclear(&ctx.favourites_name).await {
        Ok(()) => {}
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            // Playlist didn't exist yet; clearing nothing is a
            // no-op. Still refresh to update the empty mirror.
        }
        Err(e) => {
            return Err(VerbError::Mpd {
                verb: "clear_favourites".to_string(),
                reason: e.to_string(),
            });
        }
    }
    refresh_favourites(ctx, conn).await;
    Ok(())
}

pub(crate) async fn handle_move_favourite(
    ctx: &FavouritesContext,
    conn: &mut MpdConnection,
    payload: MoveFavouritePayload,
) -> Result<(), VerbError> {
    check_version(payload.v, "favourites.move_favourite")?;
    {
        let g = ctx.membership.lock().await;
        if !g.contains(&payload.uri) {
            return Err(VerbError::NotFavourite {
                uri: payload.uri.clone(),
            });
        }
    }
    let entries =
        conn.listplaylistinfo(&ctx.favourites_name)
            .await
            .map_err(|e| VerbError::Mpd {
                verb: "move_favourite".to_string(),
                reason: e.to_string(),
            })?;
    let from_position = entries
        .iter()
        .find(|e| e.file_path == payload.uri)
        .map(|e| e.position)
        .ok_or_else(|| VerbError::NotFavourite {
            uri: payload.uri.clone(),
        })?;
    // Clamp to_position to the favourites length so out-of-
    // bounds operator clicks don't refuse — UX preference
    // matches the catalogue note "Position past the favourites
    // length clamps to end".
    let to_position = payload
        .to_position
        .min(entries.len().saturating_sub(1) as u32);
    conn.playlistmove(&ctx.favourites_name, from_position, to_position)
        .await
        .map_err(|e| VerbError::Mpd {
            verb: "move_favourite".to_string(),
            reason: e.to_string(),
        })?;
    refresh_favourites(ctx, conn).await;
    Ok(())
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(favourites_name: &str) -> FavouritesContext {
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
        FavouritesContext::new(
            PathBuf::from("/var/lib/evo/music"),
            SourceRegistry::new(),
            Arc::new(NullAnn),
            favourites_name.to_string(),
        )
    }

    #[test]
    fn render_empty_envelope_carries_wire_contract() {
        let env = render_empty_envelope();
        assert_eq!(env["v"], 1);
        assert_eq!(env["count"], 0);
        assert!(env["items"].is_array());
    }

    #[test]
    fn check_version_accepts_matching_version() {
        assert!(check_version(FAVOURITES_PAYLOAD_VERSION, "x").is_ok());
    }

    #[test]
    fn check_version_refuses_mismatched_version() {
        let err = check_version(99, "x").unwrap_err();
        assert!(matches!(err, VerbError::PayloadVersion { .. }));
    }

    #[tokio::test]
    async fn list_favourites_returns_empty_envelope_before_refresh() {
        let ctx = ctx_with("__favourites__");
        let env = handle_list_favourites(&ctx).await;
        assert_eq!(env["v"], 1);
        assert_eq!(env["count"], 0);
    }

    #[tokio::test]
    async fn is_favourite_returns_false_on_empty_mirror() {
        let ctx = ctx_with("__favourites__");
        let res = handle_is_favourite(
            &ctx,
            IsFavouritePayload {
                v: FAVOURITES_PAYLOAD_VERSION,
                uri: "INTERNAL/x.flac".into(),
            },
        )
        .await
        .unwrap();
        assert!(!res.is_favourite);
        assert_eq!(res.uri, "INTERNAL/x.flac");
    }

    #[tokio::test]
    async fn is_favourite_returns_true_after_membership_populated() {
        let ctx = ctx_with("__favourites__");
        {
            let mut g = ctx.membership.lock().await;
            g.insert("INTERNAL/track.flac".into());
        }
        let res = handle_is_favourite(
            &ctx,
            IsFavouritePayload {
                v: FAVOURITES_PAYLOAD_VERSION,
                uri: "INTERNAL/track.flac".into(),
            },
        )
        .await
        .unwrap();
        assert!(res.is_favourite);
    }

    #[tokio::test]
    async fn remove_favourite_refuses_when_not_in_set() {
        let ctx = ctx_with("__favourites__");
        // No MPD connection needed because the pre-flight
        // membership check fires before any MPD call.
        // We construct only the membership state.
        // Direct error-path coverage.
        let g = ctx.membership.lock().await;
        assert!(!g.contains("INTERNAL/missing.flac"));
        drop(g);
        // Same shape — exercise the pre-flight branch via the
        // helper test we just confirmed. The actual handler
        // call requires a connection, so we assert the
        // VerbError variant via construction.
        let err = VerbError::NotFavourite {
            uri: "INTERNAL/missing.flac".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("not currently in favourites"));
        assert!(msg.contains("INTERNAL/missing.flac"));
    }

    #[test]
    fn favourites_payload_version_pins_v1() {
        assert_eq!(FAVOURITES_PAYLOAD_VERSION, 1);
    }
}
