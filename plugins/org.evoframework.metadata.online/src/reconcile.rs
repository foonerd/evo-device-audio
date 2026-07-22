// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `metadata.reconcile_release` verb implementation.
//!
//! Given a `(artist, album)` pair, return the MusicBrainz
//! canonical release identity plus derived fields
//! (`first_release_year`, `recording_type`, `track_count`).
//!
//! Flow:
//!
//! 1. Validate request shape (`v == 1`, non-empty `artist` +
//!    `album`).
//! 2. Normalise `(artist, album)` for cache keying.
//! 3. Consult the persistent cache — a positive hit short-
//!    circuits with `provider_id: "cache"`; a fresh negative hit
//!    returns the memoised not_found without touching MB.
//! 4. Cache miss: MB search → highest-scoring release + confidence.
//!    Empty result → write negative + return not_found.
//! 5. Follow-up: MB release lookup with release-group +
//!    media inclusions to derive recording_type + year +
//!    track_count.
//! 6. Cache write (positive) + return response.
//!
//! Minimum confidence threshold: 90. MB's search score is
//! integer 0..100 with exact matches at 100 and near-matches in
//! the mid-90s; scores below 90 usually indicate a wrong-artist
//! or wrong-album match that a downstream MBID lookup would
//! carry forward as false canonical labels. Below-threshold
//! matches are treated as not_found — the operator UI's fallback
//! (raw tags) shows through rather than silently swapping in
//! bad canonical data.

use evo_online_providers::{MusicBrainzClient, MusicBrainzError};
use serde::{Deserialize, Serialize};

use crate::cache::ReconcileCache;

/// Minimum MB search score accepted as a valid reconciliation.
/// Below this, treat as not_found — do not surface weak matches
/// as authoritative.
const MIN_CONFIDENCE_PERCENT: u32 = 90;

/// Wire request shape.
#[derive(Debug, Deserialize)]
struct ReconcileRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
}

/// Wire response shape.
#[derive(Debug, Serialize)]
pub(crate) struct ReconcileResponse {
    v: u8,
    status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    canonical: Option<CanonicalIdentity>,
    /// Which provider surfaced this response:
    /// - `"musicbrainz"` — fresh reconciliation from MB.
    /// - `"cache"` — served from persistent cache (positive or
    ///   negative).
    /// - `None` on `BadRequest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    /// MB search score (0..100) for the accepted release, or
    /// `None` on `NotFound` / `BadRequest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence_percent: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Ok,
    NotFound,
    BadRequest,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct CanonicalIdentity {
    pub(crate) artist: String,
    pub(crate) album: String,
    pub(crate) release_mbid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) release_group_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) artist_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) first_release_year: Option<u16>,
    pub(crate) recording_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) track_count: Option<u32>,
}

impl ReconcileResponse {
    pub(crate) fn json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// Entry point invoked from the plugin's `handle_request`.
pub(crate) async fn reconcile(
    payload: &[u8],
    mb: &MusicBrainzClient,
    cache: Option<&ReconcileCache>,
) -> Result<ReconcileResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: ReconcileRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported request v: {}", req.v)));
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

    let normalised_artist = normalise(&artist);
    let normalised_album = normalise(&album);
    let key = ReconcileCache::key_for(&normalised_artist, &normalised_album);

    // Cache pre-check.
    if let Some(c) = cache {
        if let Some(entry) = c.get(&key) {
            return Ok(from_cache(entry));
        }
    }

    // MB search.
    let hit = match mb.search_release(&artist, &album).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            let detail =
                "MB search returned zero releases for the (artist, album) pair"
                    .to_string();
            if let Some(c) = cache {
                let _ = c.put_negative(&key, detail.clone());
            }
            return Ok(ReconcileResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                canonical: None,
                provider_id: Some("musicbrainz".to_string()),
                confidence_percent: None,
                detail: Some(detail),
            });
        }
        Err(e) => {
            // Wire / status error — DO NOT cache (transient); surface
            // to the operator so intermittent MB outages don't stick.
            return Err(format!(
                "musicbrainz search failed: {}",
                mb_error_detail(e)
            ));
        }
    };

    if hit.confidence_percent < MIN_CONFIDENCE_PERCENT {
        let detail = format!(
            "MB best match confidence {} < min threshold {} — treating as not_found (raw tags win)",
            hit.confidence_percent, MIN_CONFIDENCE_PERCENT
        );
        if let Some(c) = cache {
            let _ = c.put_negative(&key, detail.clone());
        }
        return Ok(ReconcileResponse {
            v: 1,
            status: ResponseStatus::NotFound,
            canonical: None,
            provider_id: Some("musicbrainz".to_string()),
            confidence_percent: Some(hit.confidence_percent),
            detail: Some(detail),
        });
    }

    // MB release lookup for recording_type + year + track_count.
    let lookup = match mb.lookup_release(&hit.release_mbid).await {
        Ok(l) => l,
        Err(e) => {
            return Err(format!(
                "musicbrainz release lookup failed for {}: {}",
                hit.release_mbid,
                mb_error_detail(e)
            ));
        }
    };

    let canonical = CanonicalIdentity {
        artist: hit.canonical_artist,
        album: hit.canonical_album,
        release_mbid: hit.release_mbid,
        release_group_mbid: hit.release_group_mbid,
        artist_mbid: hit.artist_mbid,
        first_release_year: lookup.first_release_year,
        recording_type: lookup.recording_type,
        track_count: lookup.track_count,
    };

    if let Some(c) = cache {
        let canonical_json =
            serde_json::to_value(&canonical).unwrap_or(serde_json::Value::Null);
        let _ = c.put_positive(
            &key,
            canonical_json,
            "musicbrainz",
            hit.confidence_percent,
        );
    }

    Ok(ReconcileResponse {
        v: 1,
        status: ResponseStatus::Ok,
        canonical: Some(canonical),
        provider_id: Some("musicbrainz".to_string()),
        confidence_percent: Some(hit.confidence_percent),
        detail: None,
    })
}

fn from_cache(entry: crate::cache::CacheEntry) -> ReconcileResponse {
    match entry.status.as_str() {
        "ok" => ReconcileResponse {
            v: 1,
            status: ResponseStatus::Ok,
            canonical: entry.canonical.and_then(|v| {
                serde_json::from_value::<CanonicalIdentity>(v).ok()
            }),
            provider_id: Some("cache".to_string()),
            confidence_percent: entry.confidence_percent,
            detail: entry.detail,
        },
        _ => ReconcileResponse {
            v: 1,
            status: ResponseStatus::NotFound,
            canonical: None,
            provider_id: Some("cache".to_string()),
            confidence_percent: None,
            detail: entry.detail,
        },
    }
}

fn bad_request(detail: &str) -> ReconcileResponse {
    ReconcileResponse {
        v: 1,
        status: ResponseStatus::BadRequest,
        canonical: None,
        provider_id: None,
        confidence_percent: None,
        detail: Some(detail.to_string()),
    }
}

/// Normalise a string for cache keying: lower-case + strip
/// leading / trailing whitespace + collapse internal whitespace
/// to single spaces. Symmetric with the mpd-album parser's
/// normalisation so the same (artist, album) hits the cache
/// regardless of tag drift (double-spaces, casing).
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

fn mb_error_detail(e: MusicBrainzError) -> String {
    match e {
        MusicBrainzError::Http(err) => format!("http: {err}"),
        MusicBrainzError::Status { status, body } => {
            let truncated: String = body.chars().take(200).collect();
            format!("status={status} body={truncated}")
        }
        MusicBrainzError::Decode(msg) => format!("decode: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_collapses_and_lowers() {
        assert_eq!(normalise("  Radiohead  "), "radiohead");
        assert_eq!(normalise("OK  Computer"), "ok computer");
        assert_eq!(normalise("Sigur Rós"), "sigur rós");
        assert_eq!(normalise(""), "");
    }

    #[tokio::test]
    async fn bad_request_on_missing_artist() {
        let payload = br#"{"v":1,"album":"OK Computer"}"#;
        // MB is not touched — use a dummy client with a zero-interval limiter.
        let http = evo_online_providers::build_http_client(
            std::time::Duration::from_secs(5),
        );
        let rate = std::sync::Arc::new(evo_online_providers::RateLimiter::new(
            std::time::Duration::from_nanos(0),
        ));
        let mb = MusicBrainzClient::new(http, rate, "test/1.0");
        let resp = reconcile(payload, &mb, None).await.unwrap();
        assert_eq!(resp.status, ResponseStatus::BadRequest);
    }

    #[tokio::test]
    async fn bad_request_on_bad_json() {
        let payload = br#"not json"#;
        let http = evo_online_providers::build_http_client(
            std::time::Duration::from_secs(5),
        );
        let rate = std::sync::Arc::new(evo_online_providers::RateLimiter::new(
            std::time::Duration::from_nanos(0),
        ));
        let mb = MusicBrainzClient::new(http, rate, "test/1.0");
        assert!(reconcile(payload, &mb, None).await.is_err());
    }

    #[tokio::test]
    async fn cache_hit_short_circuits_mb() {
        // Write a positive entry into the cache; reconcile
        // should return it with provider_id=cache without
        // touching MB (MB call would error if reached — the
        // client points at a bogus base URL).
        let dir = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            dir.path().to_path_buf(),
            std::time::Duration::from_secs(60),
        );
        let key = ReconcileCache::key_for("radiohead", "ok computer");
        let canonical = serde_json::json!({
            "artist": "Radiohead",
            "album": "OK Computer",
            "release_mbid": "b1392450-e666-3926-a536-22c65f834433",
            "release_group_mbid": "5b11f4ce-a62d-471e-81fc-a69a8278c7da",
            "artist_mbid": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
            "first_release_year": 1997,
            "recording_type": "Studio",
            "track_count": 12,
        });
        cache
            .put_positive(&key, canonical, "musicbrainz", 100)
            .unwrap();

        let payload = br#"{"v":1,"artist":"Radiohead","album":"OK Computer"}"#;
        let http = evo_online_providers::build_http_client(
            std::time::Duration::from_millis(10),
        );
        let rate = std::sync::Arc::new(evo_online_providers::RateLimiter::new(
            std::time::Duration::from_nanos(0),
        ));
        let mb = MusicBrainzClient::new(http, rate, "test/1.0");
        let resp = reconcile(payload, &mb, Some(&cache)).await.unwrap();
        assert_eq!(resp.status, ResponseStatus::Ok);
        assert_eq!(resp.provider_id.as_deref(), Some("cache"));
        let c = resp.canonical.unwrap();
        assert_eq!(c.artist, "Radiohead");
        assert_eq!(c.recording_type, "Studio");
    }
}
