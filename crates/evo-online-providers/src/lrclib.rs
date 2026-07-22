// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! LRCLIB JSON API client for lyrics lookup.
//!
//! LRCLIB (`https://lrclib.net`) is a community-driven, keyless
//! lyrics catalogue. Sensible response shape (JSON), permissive
//! terms of use for personal / non-commercial listeners. No API
//! key required — audio distribution consumes it directly.
//!
//! ## Rate limiting
//!
//! LRCLIB has no documented per-second policy but explicit
//! respectful use is called out in their terms. This client uses
//! the shared [`RateLimiter`] with a caller-supplied minimum
//! interval — default 200 ms (5 req/sec) in `metadata.online`.
//! Multiple plugin instances would share the same limiter if
//! they consumed this crate.
//!
//! ## Endpoints used
//!
//! - `GET /api/get?artist_name=<A>&track_name=<T>&album_name=<AL>&duration=<D>`
//!   — exact-match lookup for a specific track. Response
//!   carries `syncedLyrics` (LRC format with `[mm:ss.ss]`
//!   timestamps) and / or `plainLyrics`.
//!
//! Both `album_name` and `duration` are optional but improve
//! match quality; when the caller has them (usually from
//! MPD's current-song tags) we pass them through.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

const LRCLIB_API_BASE: &str = "https://lrclib.net/api";

/// Errors from the LRCLIB client.
#[derive(Debug, thiserror::Error)]
pub enum LrclibError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("LRCLIB returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("LRCLIB JSON decode failed: {0}")]
    Decode(String),
}

/// One lyrics hit.
#[derive(Debug, Clone, PartialEq)]
pub struct LyricsHit {
    /// Synced lyrics in LRC format (`[mm:ss.ss]<line>`). `None`
    /// when the entry has only plain lyrics.
    pub synced_lyrics: Option<String>,
    /// Plain-text lyrics (no timestamps). `None` when the entry
    /// has only synced.
    pub plain_lyrics: Option<String>,
    /// LRCLIB's internal ID for this lyrics entry. Callers can
    /// forward this on the response so operators can look up
    /// the source directly at `https://lrclib.net/lyrics/<id>`.
    pub lrclib_id: Option<u64>,
    /// Was `synced_lyrics` present? Duplicated as a convenience
    /// flag on `SearchHit` for callers that don't want to
    /// inspect both option fields.
    pub is_synced: bool,
}

/// LRCLIB client. Wraps a shared reqwest client + shared rate
/// limiter + caller-provided User-Agent. `Clone` produces a
/// lightweight handle sharing the underlying `Arc<RateLimiter>`
/// and reqwest client.
#[derive(Clone)]
pub struct LrclibClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
}

impl LrclibClient {
    /// Construct a client. `user_agent` should identify the
    /// caller for LRCLIB's operational logs — the audio
    /// distribution passes its canonical string.
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: impl Into<String>,
    ) -> Self {
        Self {
            http,
            rate,
            user_agent: user_agent.into(),
        }
    }

    /// Exact-match lookup for a `(artist, track, [album], [duration])`
    /// tuple. Returns `Ok(None)` on 404 (LRCLIB's shape for "no
    /// match"). Returns `Err(_)` on transport or decoding failure.
    ///
    /// LRCLIB emits `{}` when no match — we treat that shape as
    /// `Ok(None)` in addition to the 404 response for defence
    /// in depth.
    pub async fn get_lyrics(
        &self,
        artist: &str,
        track: &str,
        album: Option<&str>,
        duration_seconds: Option<f64>,
    ) -> Result<Option<LyricsHit>, LrclibError> {
        self.rate.acquire().await;
        let mut params: Vec<(&str, String)> = Vec::with_capacity(4);
        params.push(("artist_name", artist.to_string()));
        params.push(("track_name", track.to_string()));
        if let Some(album) = album {
            if !album.is_empty() {
                params.push(("album_name", album.to_string()));
            }
        }
        if let Some(dur) = duration_seconds {
            // LRCLIB expects integer seconds. Round to nearest
            // to match how mp3/flac tag decoders report duration.
            params.push(("duration", (dur.round() as i64).to_string()));
        }
        let url = format!("{LRCLIB_API_BASE}/get");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&params)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LrclibError::Status { status, body });
        }
        let body: RawGetResponse = resp
            .json()
            .await
            .map_err(|e| LrclibError::Decode(e.to_string()))?;
        Ok(hit_from_raw(body))
    }
}

#[derive(Debug, Deserialize)]
struct RawGetResponse {
    #[serde(default)]
    id: Option<u64>,
    #[serde(rename = "syncedLyrics", default)]
    synced_lyrics: Option<String>,
    #[serde(rename = "plainLyrics", default)]
    plain_lyrics: Option<String>,
}

fn hit_from_raw(raw: RawGetResponse) -> Option<LyricsHit> {
    // Some LRCLIB entries carry empty string fields when the
    // catalogue has the track but no actual lyrics uploaded yet.
    // Treat empty as absent so the caller returns a proper
    // not_found rather than an empty-payload false positive.
    let synced = raw.synced_lyrics.filter(|s| !s.trim().is_empty());
    let plain = raw.plain_lyrics.filter(|s| !s.trim().is_empty());
    if synced.is_none() && plain.is_none() {
        return None;
    }
    Some(LyricsHit {
        is_synced: synced.is_some(),
        synced_lyrics: synced,
        plain_lyrics: plain,
        lrclib_id: raw.id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_from_raw_prefers_present_fields() {
        let raw = RawGetResponse {
            id: Some(12345),
            synced_lyrics: Some("[00:12.34] hello\n".to_string()),
            plain_lyrics: Some("hello\n".to_string()),
        };
        let hit = hit_from_raw(raw).unwrap();
        assert!(hit.is_synced);
        assert!(hit.synced_lyrics.is_some());
        assert!(hit.plain_lyrics.is_some());
        assert_eq!(hit.lrclib_id, Some(12345));
    }

    #[test]
    fn hit_from_raw_empty_fields_treated_as_absent() {
        let raw = RawGetResponse {
            id: Some(0),
            synced_lyrics: Some("".to_string()),
            plain_lyrics: Some("   \n".to_string()),
        };
        assert!(hit_from_raw(raw).is_none());
    }

    #[test]
    fn hit_from_raw_only_plain_marks_is_synced_false() {
        let raw = RawGetResponse {
            id: None,
            synced_lyrics: None,
            plain_lyrics: Some("hello\n".to_string()),
        };
        let hit = hit_from_raw(raw).unwrap();
        assert!(!hit.is_synced);
        assert!(hit.synced_lyrics.is_none());
        assert_eq!(hit.plain_lyrics.as_deref(), Some("hello\n"));
    }
}
