// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Last.fm JSON API client for artist bio and album notes.
//!
//! Two endpoints used:
//!
//! - `artist.getInfo` — bio (`.artist.bio.content`) + summary
//!   (`.artist.bio.summary`) + published URL (`.artist.url`).
//! - `album.getInfo` — album notes (`.album.wiki.content`) +
//!   summary (`.album.wiki.summary`) + published URL
//!   (`.album.url`).
//!
//! ## API key
//!
//! Last.fm requires an operator-supplied API key (free
//! registration at `last.fm/api/account/create`). The
//! `metadata.online` plugin config carries the key path; when
//! it's absent the plugin marks the provider as disabled and
//! returns structured `not_configured` responses to bio /
//! notes verbs. This client itself refuses to construct
//! without a key so a caller with a `LastfmClient` in hand
//! always knows the provider is armed.
//!
//! ## Rate limiting
//!
//! Last.fm's terms of use call out 5 requests per second per
//! API key as the ceiling. Default `min_interval` in the audio
//! distribution is 200 ms; multiple concurrent callers share
//! the same [`RateLimiter`] so the ceiling is honoured across
//! parallel bio + notes requests.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

const LASTFM_API_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

/// Errors from the Last.fm client.
#[derive(Debug, thiserror::Error)]
pub enum LastfmError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Last.fm returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("Last.fm JSON decode failed: {0}")]
    Decode(String),
    #[error("Last.fm application error {code}: {message}")]
    Application { code: u32, message: String },
}

/// Artist-bio hit.
#[derive(Debug, Clone, PartialEq)]
pub struct BioHit {
    pub summary: Option<String>,
    pub content: Option<String>,
    pub source_url: Option<String>,
}

/// Album-notes hit.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumNotesHit {
    pub summary: Option<String>,
    pub content: Option<String>,
    pub source_url: Option<String>,
}

/// Last.fm JSON client.
#[derive(Clone)]
pub struct LastfmClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
    api_key: String,
}

impl LastfmClient {
    /// Construct a client bound to a specific API key. The key
    /// is not logged; only its presence is asserted at
    /// construction.
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            rate,
            user_agent: user_agent.into(),
            api_key: api_key.into(),
        }
    }

    /// `artist.getInfo` — fetch the artist bio.
    ///
    /// **MBID-first, exclusive.** When `artist_mbid` is supplied,
    /// this method sends the `mbid=` parameter ONLY and omits the
    /// `artist=` parameter entirely. Last.fm's `artist.getInfo`
    /// returns disambiguation stubs ("There are at least six
    /// artists and bands…") when it receives both `mbid` + `artist`
    /// and the two don't align on Last.fm's internal index — even
    /// though the MBID uniquely identifies the artist. Sending
    /// MBID-alone forces Last.fm to look up the artist by MBID
    /// directly, which is the whole point of the MBID being on
    /// the wire. Matches Picard / beets / Roon posture.
    ///
    /// Without an MBID this falls back to `artist=` — same shape
    /// operators without reconciliation get.
    pub async fn get_artist_bio(
        &self,
        artist_name: &str,
        artist_mbid: Option<&str>,
    ) -> Result<Option<BioHit>, LastfmError> {
        self.rate.acquire().await;
        let mut params = vec![
            ("method", "artist.getinfo".to_string()),
            ("format", "json".to_string()),
            ("api_key", self.api_key.clone()),
            ("autocorrect", "1".to_string()),
        ];
        match artist_mbid.map(str::trim).filter(|s| !s.is_empty()) {
            Some(mbid) => {
                // MBID-only. Do NOT send artist=; that reintroduces
                // the disambiguation-stub failure mode.
                params.push(("mbid", mbid.to_string()));
            }
            None => {
                params.push(("artist", artist_name.to_string()));
            }
        }
        let value: serde_json::Value = self.get_json(&params).await?;
        Ok(parse_artist_bio(&value))
    }

    /// `album.getInfo` — fetch the album wiki (notes).
    ///
    /// **MBID-first, exclusive.** Same rationale as
    /// [`Self::get_artist_bio`]: with `release_mbid` present, send
    /// `mbid=` alone and omit both `artist=` and `album=`, so
    /// Last.fm resolves the release by MBID directly and cannot
    /// fall back to name-based disambiguation.
    pub async fn get_album_notes(
        &self,
        artist_name: &str,
        album_name: &str,
        release_mbid: Option<&str>,
    ) -> Result<Option<AlbumNotesHit>, LastfmError> {
        self.rate.acquire().await;
        let mut params = vec![
            ("method", "album.getinfo".to_string()),
            ("format", "json".to_string()),
            ("api_key", self.api_key.clone()),
            ("autocorrect", "1".to_string()),
        ];
        match release_mbid.map(str::trim).filter(|s| !s.is_empty()) {
            Some(mbid) => {
                params.push(("mbid", mbid.to_string()));
            }
            None => {
                params.push(("artist", artist_name.to_string()));
                params.push(("album", album_name.to_string()));
            }
        }
        let value: serde_json::Value = self.get_json(&params).await?;
        Ok(parse_album_notes(&value))
    }

    async fn get_json(
        &self,
        params: &[(&str, String)],
    ) -> Result<serde_json::Value, LastfmError> {
        let resp = self
            .http
            .get(LASTFM_API_BASE)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(params)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(LastfmError::Status { status, body });
        }
        let value: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| LastfmError::Decode(e.to_string()))?;
        // Last.fm surfaces application errors inline: 200 OK
        // with `{"error": <code>, "message": "..."}`. Common
        // codes: 6 (invalid parameters — artist/track not
        // found), 8 (temporarily unavailable), 26 (suspended
        // API key). Classify so callers distinguish "not
        // found" from "keep-caching-forever transient issue".
        if let Some(code) = value.get("error").and_then(|v| v.as_u64()) {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Err(LastfmError::Application {
                code: code as u32,
                message,
            });
        }
        Ok(value)
    }
}

/// Whether a Last.fm application error code means "no match for
/// this input" (safe to cache negative) vs "keep trying" (do
/// not cache). Code 6 = artist/track not found.
pub fn is_notfound_code(code: u32) -> bool {
    code == 6
}

fn parse_artist_bio(v: &serde_json::Value) -> Option<BioHit> {
    let artist = v.get("artist")?;
    let bio = artist.get("bio")?;
    let summary = bio
        .get("summary")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let content = bio
        .get("content")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let source_url = artist
        .get("url")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    if summary.is_none() && content.is_none() {
        return None;
    }
    Some(BioHit {
        summary,
        content,
        source_url,
    })
}

fn parse_album_notes(v: &serde_json::Value) -> Option<AlbumNotesHit> {
    let album = v.get("album")?;
    let wiki = album.get("wiki")?;
    let summary = wiki
        .get("summary")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let content = wiki
        .get("content")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let source_url = album
        .get("url")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    if summary.is_none() && content.is_none() {
        return None;
    }
    Some(AlbumNotesHit {
        summary,
        content,
        source_url,
    })
}

// Silence unused-import warnings when tests are the only
// caller of Deserialize below.
#[allow(dead_code)]
#[derive(Deserialize)]
struct _KeepDeserializeLive;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_artist_bio_extracts_summary_content_url() {
        let v = serde_json::json!({
            "artist": {
                "name": "Radiohead",
                "url": "https://last.fm/artist/Radiohead",
                "bio": {
                    "summary": "Radiohead are an English rock band...",
                    "content": "Radiohead are an English rock band formed in Abingdon, Oxfordshire, in 1985..."
                }
            }
        });
        let bio = parse_artist_bio(&v).unwrap();
        assert!(bio.summary.is_some());
        assert!(bio.content.is_some());
        assert_eq!(
            bio.source_url.as_deref(),
            Some("https://last.fm/artist/Radiohead")
        );
    }

    #[test]
    fn parse_artist_bio_returns_none_when_bio_empty() {
        let v = serde_json::json!({
            "artist": {
                "name": "Radiohead",
                "url": "https://last.fm/artist/Radiohead",
                "bio": {
                    "summary": "",
                    "content": "   "
                }
            }
        });
        assert!(parse_artist_bio(&v).is_none());
    }

    #[test]
    fn parse_album_notes_extracts_wiki() {
        let v = serde_json::json!({
            "album": {
                "name": "OK Computer",
                "url": "https://last.fm/album/OK+Computer",
                "wiki": {
                    "summary": "OK Computer is the third studio album...",
                    "content": "OK Computer is the third studio album by the English rock band Radiohead..."
                }
            }
        });
        let notes = parse_album_notes(&v).unwrap();
        assert!(notes.summary.is_some());
        assert!(notes.content.is_some());
    }

    #[test]
    fn parse_album_notes_returns_none_on_missing_wiki() {
        let v = serde_json::json!({
            "album": {
                "name": "OK Computer",
                "url": "https://last.fm/album/OK+Computer"
            }
        });
        assert!(parse_album_notes(&v).is_none());
    }

    #[test]
    fn is_notfound_code_classifies_correctly() {
        assert!(is_notfound_code(6));
        assert!(!is_notfound_code(8));
        assert!(!is_notfound_code(26));
    }
}
