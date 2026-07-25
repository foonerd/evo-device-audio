// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Discogs JSON API client for release credits and artist bio.
//!
//! Two endpoints used:
//!
//! - `/database/search` — resolve `(artist, album)` to a Discogs
//!   release id via `type=release&artist=...&release_title=...`.
//! - `/releases/:id` — release detail (label / catno / year /
//!   country / format / credits).
//! - `/artists/:id` — artist profile (`profile` field).
//!
//! ## API key
//!
//! Discogs requires an operator-supplied Personal Access Token
//! (free registration at `https://www.discogs.com/settings/developers`).
//! The `metadata.online` plugin resolves the token via the
//! framework credential vault under the stable key
//! `discogs_personal_access_token`; when absent, Discogs-gated
//! verbs return structured `not_configured` responses.
//!
//! ## Rate limiting
//!
//! Discogs's terms of use call out 60 requests per minute per
//! authenticated identity. Default `min_interval` is 1 second;
//! multiple concurrent callers share the same [`RateLimiter`] so
//! the ceiling is honoured across parallel release + artist
//! requests.
//!
//! ## Content licensing
//!
//! Discogs contributor data is under a permissive licence.
//! Artwork images stay as URL references rather than proxy-
//! cached bytes — the licensing shape wants operator-fetch,
//! not server-proxy.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

const DISCOGS_API_BASE: &str = "https://api.discogs.com";

/// Errors from the Discogs client.
#[derive(Debug, thiserror::Error)]
pub enum DiscogsError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Discogs returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("Discogs JSON decode failed: {0}")]
    Decode(String),
}

/// Release-detail hit.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseDetailHit {
    /// Discogs release id.
    pub release_id: u64,
    /// Optional label name (first label if multiple).
    pub label: Option<String>,
    /// Optional catalog number (first if multiple).
    pub catalog_number: Option<String>,
    /// Optional release year.
    pub year: Option<u32>,
    /// Optional country of release.
    pub country: Option<String>,
    /// Optional format description (first if multiple; e.g.
    /// `"12\" LP"`, `"CD"`, `"SACD, Hybrid"`).
    pub format: Option<String>,
    /// Optional release notes prose from Discogs' `notes` field.
    pub notes: Option<String>,
    /// URL the operator UI links to for full detail.
    pub source_url: Option<String>,
}

/// Artist-profile hit (Discogs profile text, useful as a
/// second-source artist bio when Last.fm returns not_found).
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistProfileHit {
    pub profile: Option<String>,
    pub source_url: Option<String>,
}

/// Discogs JSON client.
#[derive(Clone)]
pub struct DiscogsClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
    personal_access_token: String,
}

impl DiscogsClient {
    /// Construct a Discogs client. Refuses an empty token so a
    /// caller with a `DiscogsClient` in hand always knows the
    /// provider is armed.
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: String,
        personal_access_token: String,
    ) -> Option<Self> {
        if personal_access_token.trim().is_empty() {
            return None;
        }
        Some(Self {
            http,
            rate,
            user_agent,
            personal_access_token,
        })
    }

    fn auth_header(&self) -> String {
        format!("Discogs token={}", self.personal_access_token)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: String,
    ) -> Result<T, DiscogsError> {
        self.rate.acquire().await;
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, self.user_agent.clone())
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DiscogsError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| DiscogsError::Decode(format!("{e}")))
    }

    /// Fetch release detail for `(artist, album)`. Returns `None`
    /// when Discogs's search resolves no releases matching the
    /// pair. Returns `Err` on transport / decode failure or when
    /// the fetch succeeds but the detail JSON is missing every
    /// interesting field.
    ///
    /// **Name-last fallback.** Callers with a MusicBrainz release
    /// MBID should first walk MB's `discogs` url-rel and call
    /// [`Self::get_release_by_id`] on the resulting id — that's
    /// the MBID-first path. This method is the last-resort
    /// surface for releases without a MB→Discogs link.
    pub async fn get_release_detail(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Option<ReleaseDetailHit>, DiscogsError> {
        let search_url = format!(
            "{DISCOGS_API_BASE}/database/search?type=release&artist={}&release_title={}&per_page=1",
            urlencode(artist),
            urlencode(album),
        );
        let search: SearchResponse = self.get_json(search_url).await?;
        let Some(first) = search.results.into_iter().next() else {
            return Ok(None);
        };
        self.get_release_by_id(first.id).await
    }

    /// Fetch release detail by Discogs release id — the MBID-
    /// first path. Callers resolve the id from a
    /// MusicBrainz release's `discogs` url-rel
    /// (parseable with [`parse_discogs_release_id`]) and then
    /// hit this endpoint directly, bypassing Discogs's fuzzy
    /// `(artist, album)` search.
    pub async fn get_release_by_id(
        &self,
        release_id: u64,
    ) -> Result<Option<ReleaseDetailHit>, DiscogsError> {
        let detail_url = format!("{DISCOGS_API_BASE}/releases/{release_id}");
        let detail: ReleaseDetail = self.get_json(detail_url).await?;
        let label = detail
            .labels
            .as_ref()
            .and_then(|v| v.first())
            .and_then(|l| l.name.clone());
        let catalog_number = detail
            .labels
            .as_ref()
            .and_then(|v| v.first())
            .and_then(|l| l.catno.clone());
        let format = detail
            .formats
            .as_ref()
            .and_then(|v| v.first())
            .and_then(|f| f.name.clone());
        Ok(Some(ReleaseDetailHit {
            release_id,
            label,
            catalog_number,
            year: detail.year,
            country: detail.country,
            format,
            notes: detail.notes,
            source_url: Some(format!(
                "https://www.discogs.com/release/{release_id}"
            )),
        }))
    }

    /// Fetch artist profile for `artist`. Returns `None` when
    /// Discogs' search resolves no artists.
    pub async fn get_artist_profile(
        &self,
        artist: &str,
    ) -> Result<Option<ArtistProfileHit>, DiscogsError> {
        let search_url = format!(
            "{DISCOGS_API_BASE}/database/search?type=artist&q={}&per_page=1",
            urlencode(artist),
        );
        let search: ArtistSearchResponse = self.get_json(search_url).await?;
        let Some(first) = search.results.into_iter().next() else {
            return Ok(None);
        };
        let detail_url = format!("{DISCOGS_API_BASE}/artists/{}", first.id);
        let detail: ArtistDetail = self.get_json(detail_url).await?;
        Ok(Some(ArtistProfileHit {
            profile: detail.profile.filter(|s| !s.trim().is_empty()),
            source_url: Some(format!(
                "https://www.discogs.com/artist/{}",
                first.id
            )),
        }))
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    id: u64,
}

#[derive(Debug, Deserialize)]
struct ReleaseDetail {
    #[serde(default)]
    year: Option<u32>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    labels: Option<Vec<LabelEntry>>,
    #[serde(default)]
    formats: Option<Vec<FormatEntry>>,
}

#[derive(Debug, Deserialize)]
struct LabelEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    catno: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FormatEntry {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistSearchResponse {
    #[serde(default)]
    results: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct ArtistDetail {
    #[serde(default)]
    profile: Option<String>,
}

/// Extract the numeric Discogs release id from a MusicBrainz
/// `discogs` url-rel resource string.
///
/// MB stores discogs URLs in one of these shapes:
///
///   `https://www.discogs.com/release/17279182`
///   `https://www.discogs.com/release/17279182-Fiona-Joy-Signature-Solo`
///   `http://www.discogs.com/release/17279182-...`
///
/// Returns `None` for URLs that don't match — a Discogs
/// URL pointing at a MASTER (`/master/<id>`) rather than a
/// release, or a non-Discogs URL. Callers treat the None as a
/// clean miss and fall through to the name-search path.
pub fn parse_discogs_release_id(url: &str) -> Option<u64> {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = after_scheme.split_once('/')?;
    if host != "www.discogs.com" && host != "discogs.com" {
        return None;
    }
    let after_release = path.strip_prefix("release/")?;
    // The id is the leading digits; everything after `-` /
    // `?` / `#` is slug or query.
    let id_str: String = after_release
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if id_str.is_empty() {
        None
    } else {
        id_str.parse::<u64>().ok()
    }
}

fn urlencode(s: &str) -> String {
    // Minimal percent-encoder. Only alphanumerics + `-_.~` pass
    // through; everything else escapes to `%XX`.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
