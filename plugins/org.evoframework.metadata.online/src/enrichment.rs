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

use evo_online_providers::{lastfm::LastfmError, LastfmClient, LrclibClient};
use serde::{Deserialize, Serialize};

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
// Artist bio — Last.fm
// ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct BioRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    artist_mbid: Option<String>,
}

pub(crate) async fn query_artist_bio(
    payload: &[u8],
    lastfm: Option<&LastfmClient>,
    cache: &EnrichmentCache,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: BioRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported v: {}", req.v)));
    }
    let artist = req
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let artist = match artist {
        Some(a) => a.to_string(),
        None => {
            return Ok(bad_request("artist is required and must be non-empty"));
        }
    };
    let Some(lastfm) = lastfm else {
        return Ok(not_configured(
            "Last.fm API key not configured on this device; bio provider disabled",
        ));
    };
    let key = EnrichmentCache::key_for(&["bio", &normalise(&artist)]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Ok(from_cache_ok(p, entry.provider_id));
            }
        }
        return Ok(from_cache_negative(entry.detail));
    }
    let hit_result = lastfm
        .get_artist_bio(&artist, req.artist_mbid.as_deref())
        .await;
    match hit_result {
        Ok(None) => {
            let detail = "Last.fm has no bio for this artist".to_string();
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
            // Not-found application code — cache negatively.
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
        Err(e) => {
            // Transient error — do NOT cache.
            Err(format!("Last.fm error: {e}"))
        }
    }
}

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
