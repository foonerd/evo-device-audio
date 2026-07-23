// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Bio + album-notes + lyrics verb handlers.
//!
//! All three verbs share the same shape:
//!
//! 1. Validate `v == 1` + required inputs.
//! 2. Consult the [`EnrichmentCache`] for this verb's namespace.
//!    - Positive cache hit → return the cached payload with
//!      `provider_id: "cache"`.
//!    - Fresh negative → return `not_found` with cache
//!      provider_id.
//! 3. Cache miss → dispatch the provider chain:
//!    - Lyrics: LRCLIB.
//!    - Bio: Last.fm (when configured); no fallback in this
//!      cycle (MusicBrainz artist annotation is thin —
//!      queued as follow-on).
//!    - Notes: Last.fm (when configured).
//! 4. On hit → cache-positive + respond.
//! 5. On chain-exhausted → cache-negative + respond.
//! 6. When the sole provider is disabled (Last.fm key
//!    absent) → respond `not_configured` without touching
//!    the cache (transient state; can go configured any
//!    moment).

use evo_online_providers::{
    lastfm::LastfmError, DiscogsClient, DiscogsError, GeniusClient,
    GeniusError, LastfmClient, LrclibClient,
};
use serde::{Deserialize, Serialize};

use crate::cascade::{
    Attribution, CascadeResponse, CascadeStatus, EnhancementHint, EntityRef,
    EntityType, PrivacyClass, ProviderCatalogue, ProviderId,
};
use crate::enrichment_cache::EnrichmentCache;

// ---------------------------------------------------------------
// Common shape
// ---------------------------------------------------------------

#[derive(Debug, Serialize)]
pub(crate) struct EnrichmentResponse {
    v: u8,
    status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl EnrichmentResponse {
    pub(crate) fn json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Ok,
    NotFound,
    NotConfigured,
    BadRequest,
}

fn bad_request(detail: &str) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::BadRequest,
        provider_id: None,
        payload: None,
        detail: Some(detail.to_string()),
    }
}

fn not_configured(detail: &str) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::NotConfigured,
        provider_id: None,
        payload: None,
        detail: Some(detail.to_string()),
    }
}

fn from_cache_ok(
    payload: serde_json::Value,
    provider_id: Option<String>,
) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::Ok,
        provider_id: Some("cache".to_string()),
        payload: Some(serde_json::json!({
            "cached_from_provider_id": provider_id,
            "value": payload,
        })),
        detail: None,
    }
}

fn from_cache_negative(detail: Option<String>) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::NotFound,
        provider_id: Some("cache".to_string()),
        payload: None,
        detail,
    }
}

/// Normalise a string for cache keying: lower-case + collapse
/// internal whitespace + trim.
fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.extend(ch.to_lowercase());
            last_was_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------
// Lyrics — LRCLIB
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LyricsRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    duration_seconds: Option<f64>,
}

pub(crate) async fn query_lyrics(
    payload: &[u8],
    lrclib: &LrclibClient,
    cache: &EnrichmentCache,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: LyricsRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported v: {}", req.v)));
    }
    let artist = req
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let track = req
        .track
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (artist, track) = match (artist, track) {
        (Some(a), Some(t)) => (a.to_string(), t.to_string()),
        _ => {
            return Ok(bad_request(
                "artist and track are required and must be non-empty",
            ));
        }
    };
    let album_norm = req
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(normalise);
    let key = EnrichmentCache::key_for(&[
        "lyrics",
        &normalise(&artist),
        &normalise(&track),
        album_norm.as_deref().unwrap_or(""),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Ok(from_cache_ok(p, entry.provider_id));
            }
        }
        return Ok(from_cache_negative(entry.detail));
    }
    let hit = lrclib
        .get_lyrics(&artist, &track, req.album.as_deref(), req.duration_seconds)
        .await
        .map_err(|e| format!("LRCLIB error: {e}"))?;
    match hit {
        None => {
            let detail = "LRCLIB returned no lyrics for this track".to_string();
            let _ = cache.put_negative(&key, detail.clone());
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                provider_id: Some("lrclib".to_string()),
                payload: None,
                detail: Some(detail),
            })
        }
        Some(h) => {
            let payload = serde_json::json!({
                "synced_lyrics": h.synced_lyrics,
                "plain_lyrics": h.plain_lyrics,
                "is_synced": h.is_synced,
                "lrclib_id": h.lrclib_id,
                "source_url": h.lrclib_id.map(|id| format!("https://lrclib.net/lyrics/{id}")),
            });
            let _ = cache.put_positive(&key, payload.clone(), "lrclib");
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::Ok,
                provider_id: Some("lrclib".to_string()),
                payload: Some(payload),
                detail: None,
            })
        }
    }
}

// ---------------------------------------------------------------
// Artist bio — retired 2026-07-23 in favour of the entity-typed
// keyless-first cascade `query_entity_bio` below.
// ---------------------------------------------------------------

// ---------------------------------------------------------------
// Album notes — Last.fm
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NotesRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    release_mbid: Option<String>,
}

pub(crate) async fn query_album_notes(
    payload: &[u8],
    lastfm: Option<&LastfmClient>,
    cache: &EnrichmentCache,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: NotesRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported v: {}", req.v)));
    }
    let artist = req
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let album = req
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let (artist, album) = match (artist, album) {
        (Some(a), Some(b)) => (a.to_string(), b.to_string()),
        _ => {
            return Ok(bad_request(
                "artist and album are required and must be non-empty",
            ));
        }
    };
    let Some(lastfm) = lastfm else {
        return Ok(not_configured(
            "Last.fm API key not configured on this device; album-notes provider disabled",
        ));
    };
    let key = EnrichmentCache::key_for(&[
        "notes",
        &normalise(&artist),
        &normalise(&album),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Ok(from_cache_ok(p, entry.provider_id));
            }
        }
        return Ok(from_cache_negative(entry.detail));
    }
    let hit_result = lastfm
        .get_album_notes(&artist, &album, req.release_mbid.as_deref())
        .await;
    match hit_result {
        Ok(None) => {
            let detail =
                "Last.fm has no album notes for this album".to_string();
            let _ = cache.put_negative(&key, detail.clone());
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                provider_id: Some("lastfm".to_string()),
                payload: None,
                detail: Some(detail),
            })
        }
        Ok(Some(h)) => {
            let payload = serde_json::json!({
                "summary": h.summary,
                "content": h.content,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "lastfm");
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::Ok,
                provider_id: Some("lastfm".to_string()),
                payload: Some(payload),
                detail: None,
            })
        }
        Err(LastfmError::Application { code, message })
            if evo_online_providers::lastfm_is_notfound_code(code) =>
        {
            let detail = format!("Last.fm code {code}: {message}");
            let _ = cache.put_negative(&key, detail.clone());
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                provider_id: Some("lastfm".to_string()),
                payload: None,
                detail: Some(detail),
            })
        }
        Err(e) => Err(format!("Last.fm error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_collapses_and_lowers() {
        assert_eq!(normalise("  Radiohead  "), "radiohead");
        assert_eq!(normalise("OK  Computer"), "ok computer");
        assert_eq!(normalise(""), "");
    }
}

// -----------------------------------------------------------------
// query_release_credits — Discogs release detail
// -----------------------------------------------------------------
//
// Request:
//   { "v": 1, "artist": "...", "album": "..." }
//
// Response payload on hit:
//   {
//     "release_id": <u64>,
//     "label": "...",
//     "catalog_number": "...",
//     "year": <u32>,
//     "country": "...",
//     "format": "...",
//     "notes": "...",
//     "source_url": "https://www.discogs.com/release/..."
//   }
//
// No-key / no-configuration → status=not_configured, provider_id=None.

#[derive(Debug, Deserialize)]
struct ReleaseCreditsRequest {
    v: u8,
    artist: Option<String>,
    album: Option<String>,
}

pub(crate) async fn query_release_credits(
    payload: &[u8],
    discogs: Option<&DiscogsClient>,
    cache: &EnrichmentCache,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: ReleaseCreditsRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported v: {}", req.v)));
    }
    let artist = req
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let album = req
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let (Some(artist), Some(album)) = (artist, album) else {
        return Ok(bad_request(
            "artist and album are required and must be non-empty",
        ));
    };
    let Some(discogs) = discogs else {
        return Ok(not_configured(
            "Discogs Personal Access Token not configured on this device; \
             release-credits provider disabled",
        ));
    };
    let key = EnrichmentCache::key_for(&[
        "release_credits",
        &normalise(&artist),
        &normalise(&album),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Ok(from_cache_ok(p, entry.provider_id));
            }
        }
        return Ok(from_cache_negative(entry.detail));
    }
    match discogs.get_release_detail(&artist, &album).await {
        Ok(None) => {
            let detail =
                "Discogs has no release matching this pair".to_string();
            let _ = cache.put_negative(&key, detail.clone());
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                provider_id: Some("discogs".to_string()),
                payload: None,
                detail: Some(detail),
            })
        }
        Ok(Some(h)) => {
            let payload = serde_json::json!({
                "release_id": h.release_id,
                "label": h.label,
                "catalog_number": h.catalog_number,
                "year": h.year,
                "country": h.country,
                "format": h.format,
                "notes": h.notes,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "discogs");
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::Ok,
                provider_id: Some("discogs".to_string()),
                payload: Some(payload),
                detail: None,
            })
        }
        Err(e) => {
            // Transient — never cache. Same shape the transient-not-
            // cached fix locked in for artwork.online.
            Err(discogs_error_message(&e))
        }
    }
}

fn discogs_error_message(e: &DiscogsError) -> String {
    match e {
        DiscogsError::Http(err) => format!("discogs http: {err}"),
        DiscogsError::Status { status, body } => {
            format!("discogs status {status}: {body}")
        }
        DiscogsError::Decode(m) => format!("discogs decode: {m}"),
    }
}

// -----------------------------------------------------------------
// query_track_annotation — Genius song description + lyrics URL
// -----------------------------------------------------------------
//
// Request:
//   { "v": 1, "artist": "...", "track": "..." }
//
// Response payload on hit:
//   {
//     "song_id": <u64>,
//     "description": "...",
//     "source_url": "https://genius.com/..."
//   }
//
// The Genius API does NOT return lyrics text; this verb surfaces
// only what the API returns as text (annotation description) plus
// the URL of Genius's web page for the song. The operator UI
// renders the URL as an outbound "View lyrics on Genius" link.

#[derive(Debug, Deserialize)]
struct TrackAnnotationRequest {
    v: u8,
    artist: Option<String>,
    track: Option<String>,
}

pub(crate) async fn query_track_annotation(
    payload: &[u8],
    genius: Option<&GeniusClient>,
    cache: &EnrichmentCache,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: TrackAnnotationRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported v: {}", req.v)));
    }
    let artist = req
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let track = req
        .track
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let (Some(artist), Some(track)) = (artist, track) else {
        return Ok(bad_request(
            "artist and track are required and must be non-empty",
        ));
    };
    let Some(genius) = genius else {
        return Ok(not_configured(
            "Genius client access token not configured on this device; \
             track-annotation provider disabled",
        ));
    };
    let key = EnrichmentCache::key_for(&[
        "track_annotation",
        &normalise(&artist),
        &normalise(&track),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Ok(from_cache_ok(p, entry.provider_id));
            }
        }
        return Ok(from_cache_negative(entry.detail));
    }
    match genius.get_track_annotation(&artist, &track).await {
        Ok(None) => {
            let detail = "Genius has no hit for this track".to_string();
            let _ = cache.put_negative(&key, detail.clone());
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                provider_id: Some("genius".to_string()),
                payload: None,
                detail: Some(detail),
            })
        }
        Ok(Some(h)) => {
            let payload = serde_json::json!({
                "song_id": h.song_id,
                "description": h.description,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "genius");
            Ok(EnrichmentResponse {
                v: 1,
                status: ResponseStatus::Ok,
                provider_id: Some("genius".to_string()),
                payload: Some(payload),
                detail: None,
            })
        }
        Err(e) => Err(genius_error_message(&e)),
    }
}

fn genius_error_message(e: &GeniusError) -> String {
    match e {
        GeniusError::Http(err) => format!("genius http: {err}"),
        GeniusError::Status { status, body } => {
            format!("genius status {status}: {body}")
        }
        GeniusError::Decode(m) => format!("genius decode: {m}"),
    }
}

// -----------------------------------------------------------------
// KEYLESS-FIRST CASCADE — entity-typed bio via MB → Wikipedia
// → Wikidata → Last.fm (identity-bearing enhancement)
// -----------------------------------------------------------------

/// Entity-typed bio request: enrich an artist / composer / work /
/// performer / conductor / ensemble via the anonymous-first
/// cascade. Falls back to identity-bearing providers as opt-in
/// enhancement.
#[derive(Debug, Deserialize)]
pub(crate) struct EntityBioRequest {
    #[serde(default)]
    pub(crate) v: u8,
    /// Entity to enrich. New shape.
    #[serde(default)]
    pub(crate) entity: Option<EntityRef>,
    /// Backward-compat: legacy `{artist, artist_mbid}` shape from
    /// the pre-cascade query_artist_bio request. When `entity` is
    /// absent and these are present, the cascade treats them as
    /// an `EntityType::Artist` request.
    #[serde(default)]
    pub(crate) artist: Option<String>,
    #[serde(default)]
    pub(crate) artist_mbid: Option<String>,
}

pub(crate) async fn query_entity_bio(
    payload: &[u8],
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
) -> Result<CascadeResponse, String> {
    if payload.is_empty() {
        return Ok(CascadeResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: EntityBioRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(CascadeResponse::bad_request(format!(
            "unsupported v: {}",
            req.v
        )));
    }
    // Resolve entity from the new-shape `entity` field, else
    // the legacy `{artist, artist_mbid}` fields.
    let entity: EntityRef = match req.entity {
        Some(e) => e,
        None => match req.artist {
            Some(a) if !a.trim().is_empty() => EntityRef {
                entity_type: EntityType::Artist,
                name: a.trim().to_string(),
                mbid: req.artist_mbid,
            },
            _ => {
                return Ok(CascadeResponse::bad_request(
                    "either `entity` (new shape) or `artist` (legacy shape) \
                     is required and must be non-empty",
                ));
            }
        },
    };
    if entity.name.trim().is_empty() {
        return Ok(CascadeResponse::bad_request(
            "entity.name is required and must be non-empty",
        ));
    }

    // Anonymous baseline: try MusicBrainz artist search + url-rels
    // to discover Wikipedia / Wikidata URLs, then Wikipedia
    // summary, then Wikidata facts. Every step is gated on
    // per-provider enable + privacy-mode.
    let mut last_provider: Option<ProviderId> = None;

    // Positive cache pre-check per provider so an operator
    // disabling a provider does not suppress cached positives
    // from a still-enabled provider.
    // Anonymous first: Wikipedia (bio prose is our primary
    // enrichment surface).
    let want_mb = catalogue
        .config
        .is_effectively_enabled(ProviderId::MusicBrainz);
    let want_wp = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikipedia);
    let want_wd = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikidata);
    let want_lastfm =
        catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
            && catalogue.lastfm.is_some();

    if !(want_mb || want_wp || want_wd || want_lastfm) {
        return Ok(CascadeResponse::not_configured(
            "every bio provider is disabled or unavailable on this device; \
             enable at least one under Settings → Metadata → Sources",
        ));
    }

    // Resolve entity URLs via MusicBrainz artist lookup when
    // enabled. This yields Wikipedia + Wikidata URLs that the
    // downstream providers consume directly — no fuzzy search.
    let mut wikipedia_url: Option<String> = None;
    let mut wikidata_url: Option<String> = None;
    if want_mb {
        if let Some(mb) = catalogue.musicbrainz.as_ref() {
            last_provider = Some(ProviderId::MusicBrainz);
            let mbid = match &entity.mbid {
                Some(m) if !m.trim().is_empty() => Some(m.clone()),
                _ => match mb.search_artist(&entity.name).await {
                    Ok(Some(hit)) if hit.confidence_percent >= 85 => {
                        Some(hit.artist_mbid)
                    }
                    Ok(_) => None,
                    Err(e) => {
                        // MB transient — do not cache; skip MB.
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            entity = %entity.name,
                            error = %e,
                            "MB artist search transient; skipping"
                        );
                        None
                    }
                },
            };
            if let Some(mbid) = mbid {
                match mb.lookup_artist(&mbid).await {
                    Ok(al) => {
                        wikipedia_url = al.wikipedia_url;
                        wikidata_url = al.wikidata_url;
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            mbid,
                            error = %e,
                            "MB artist lookup transient; skipping"
                        );
                    }
                }
            }
        }
    }

    // Wikipedia summary — the primary anonymous bio content.
    if want_wp {
        if let Some(wp) = catalogue.wikipedia.as_ref() {
            last_provider = Some(ProviderId::Wikipedia);
            let key = EnrichmentCache::key_for(&[
                "entity_bio",
                entity.entity_type.as_str(),
                &normalise(&entity.name),
                ProviderId::Wikipedia.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikipedia,
                            entry.provider_id,
                        ));
                    }
                }
            }
            let hit = match &wikipedia_url {
                Some(url) => wp.get_summary_from_url(url).await,
                None => wp.get_summary_en(&entity.name).await,
            };
            match hit {
                Ok(Some(summary)) => {
                    let payload = serde_json::json!({
                        "title": summary.title,
                        "summary": summary.extract,
                        "language": summary.language,
                    });
                    let _ =
                        cache.put_positive(&key, payload.clone(), "wikipedia");
                    let enhancement = enhancement_hint_for_bio(
                        catalogue,
                        ProviderId::Wikipedia,
                    );
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Wikipedia.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::Anonymous.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Wikipedia".into(),
                            source_url: Some(summary.page_url),
                            license: "CC BY-SA".into(),
                        }),
                        enhancement,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        entity = %entity.name,
                        error = %e,
                        "Wikipedia summary transient / not-usable; skipping"
                    );
                }
            }
        }
    }

    // Wikidata — structured biographical facts. Fallback bio
    // surface when Wikipedia has no summary.
    if want_wd {
        if let Some(wd) = catalogue.wikidata.as_ref() {
            last_provider = Some(ProviderId::Wikidata);
            let hit = match &wikidata_url {
                Some(url) => wd.get_entity_from_url(url).await,
                None => Ok(None),
            };
            if let Ok(Some(entity_hit)) = hit {
                let payload = serde_json::json!({
                    "label": entity_hit.label_en,
                    "description": entity_hit.description_en,
                    "date_of_birth": entity_hit.date_of_birth,
                    "date_of_death": entity_hit.date_of_death,
                    "inception": entity_hit.inception,
                    "dissolution": entity_hit.dissolution,
                });
                let key = EnrichmentCache::key_for(&[
                    "entity_bio",
                    entity.entity_type.as_str(),
                    &normalise(&entity.name),
                    ProviderId::Wikidata.as_str(),
                ]);
                let _ = cache.put_positive(&key, payload.clone(), "wikidata");
                let enhancement =
                    enhancement_hint_for_bio(catalogue, ProviderId::Wikidata);
                return Ok(CascadeResponse {
                    v: 1,
                    status: CascadeStatus::Ok,
                    provider_id: Some(
                        ProviderId::Wikidata.as_str().to_string(),
                    ),
                    privacy_class: Some(
                        PrivacyClass::Anonymous.as_str().to_string(),
                    ),
                    payload: Some(payload),
                    detail: None,
                    attribution: Some(Attribution {
                        source_name: "Wikidata".into(),
                        source_url: Some(entity_hit.entity_url),
                        license: "CC0".into(),
                    }),
                    enhancement,
                });
            }
        }
    }

    // Identity-bearing enhancement: Last.fm — richer editorial
    // bio when the operator has enabled it AND provided a key.
    // Only fired for artist-type entities; Last.fm has poor
    // classical (composer / work / performer) coverage.
    if want_lastfm && matches!(entity.entity_type, EntityType::Artist) {
        if let Some(lastfm) = catalogue.lastfm.as_ref() {
            last_provider = Some(ProviderId::Lastfm);
            match lastfm
                .get_artist_bio(&entity.name, entity.mbid.as_deref())
                .await
            {
                Ok(Some(h)) => {
                    let payload = serde_json::json!({
                        "summary": h.summary,
                        "content": h.content,
                        "source_url": h.source_url,
                    });
                    let key = EnrichmentCache::key_for(&[
                        "entity_bio",
                        entity.entity_type.as_str(),
                        &normalise(&entity.name),
                        ProviderId::Lastfm.as_str(),
                    ]);
                    let _ = cache.put_positive(&key, payload.clone(), "lastfm");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Lastfm.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::IdentityBearing.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Last.fm".into(),
                            source_url: h.source_url.clone(),
                            license: "Last.fm terms of use".into(),
                        }),
                        enhancement: None,
                    });
                }
                Ok(None) => {}
                Err(LastfmError::Application { code, message })
                    if evo_online_providers::lastfm_is_notfound_code(code) =>
                {
                    tracing::debug!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "lastfm",
                        code,
                        message,
                        "Last.fm clean miss"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "lastfm",
                        entity = %entity.name,
                        error = %e,
                        "Last.fm transient; skipping"
                    );
                }
            }
        }
    }

    // All enabled providers structurally missed. Return
    // not_found with an enhancement hint pointing at the
    // best identity-bearing provider the operator could enable
    // next.
    let detail = format!(
        "no bio found for {}={} across enabled providers",
        entity.entity_type.as_str(),
        entity.name
    );
    let enhancement = enhancement_hint_for_bio_missing(catalogue);
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        enhancement,
    })
}

fn cascade_ok_from_cache(
    payload: serde_json::Value,
    provider: ProviderId,
    _origin_provider_id: Option<String>,
) -> CascadeResponse {
    CascadeResponse {
        v: 1,
        status: CascadeStatus::Ok,
        provider_id: Some(provider.as_str().to_string()),
        privacy_class: Some(provider.privacy_class().as_str().to_string()),
        payload: Some(payload),
        detail: None,
        attribution: Some(match provider {
            ProviderId::Wikipedia => Attribution {
                source_name: "Wikipedia".into(),
                source_url: None,
                license: "CC BY-SA".into(),
            },
            ProviderId::Wikidata => Attribution {
                source_name: "Wikidata".into(),
                source_url: None,
                license: "CC0".into(),
            },
            ProviderId::MusicBrainz => Attribution {
                source_name: "MusicBrainz".into(),
                source_url: None,
                license: "CC0".into(),
            },
            ProviderId::Lrclib => Attribution {
                source_name: "LRCLIB".into(),
                source_url: None,
                license: "Public domain".into(),
            },
            ProviderId::Lastfm => Attribution {
                source_name: "Last.fm".into(),
                source_url: None,
                license: "Last.fm terms of use".into(),
            },
            ProviderId::Discogs => Attribution {
                source_name: "Discogs".into(),
                source_url: None,
                license: "Discogs terms of use".into(),
            },
            ProviderId::Genius => Attribution {
                source_name: "Genius".into(),
                source_url: None,
                license: "Genius terms of use".into(),
            },
        }),
        enhancement: None,
    }
}

fn enhancement_hint_for_bio(
    catalogue: &ProviderCatalogue,
    won: ProviderId,
) -> Option<EnhancementHint> {
    // Suggest Last.fm as bio enhancement only for artist requests
    // where the anonymous baseline won and Last.fm is either
    // disabled or unavailable.
    if won == ProviderId::Lastfm {
        return None;
    }
    if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key for richer editorial bios".into(),
        })
    } else if !catalogue.config.is_effectively_enabled(ProviderId::Lastfm) {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources \
                     for richer editorial bios"
                .into(),
        })
    } else {
        None
    }
}

fn enhancement_hint_for_bio_missing(
    catalogue: &ProviderCatalogue,
) -> Option<EnhancementHint> {
    // On a full miss with Last.fm available but disabled,
    // suggest enabling it. Otherwise silent.
    if catalogue.lastfm.is_some()
        && !catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
    {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources \
                     to try one more source"
                .into(),
        })
    } else if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key to try one more source".into(),
        })
    } else {
        None
    }
}
