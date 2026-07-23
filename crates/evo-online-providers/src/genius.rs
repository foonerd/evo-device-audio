// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Genius JSON API client for song annotation, artist description,
//! and lyrics-URL fallback.
//!
//! Three endpoints used:
//!
//! - `/search?q=<artist track>` — resolve a track to a Genius
//!   song id via the top-hit.
//! - `/songs/:id` — song metadata including `description` prose
//!   (annotation) and `url` (Genius web page carrying the
//!   lyrics).
//! - `/artists/:id` — artist metadata including `description`
//!   prose.
//!
//! ## Contract-clean use of the Genius API
//!
//! The Genius API surfaces the annotation / description text
//! fields it hosts, plus the URL of the lyrics web page. The
//! API DOES NOT return lyrics text — it returns the URL only.
//!
//! This client consumes ONLY what the API returns as text (the
//! `description` fields). It NEVER scrapes the linked web page's
//! HTML to extract lyrics; that would violate the Genius terms
//! of use. The lyrics URL is surfaced to the operator UI as an
//! outbound link — the operator's browser fetches the page
//! directly from Genius, honouring their page-view analytics.
//!
//! ## API key
//!
//! Genius requires an operator-supplied client access token
//! (free registration at `https://genius.com/api-clients`). The
//! `metadata.online` plugin resolves the token via the framework
//! credential vault under the stable key
//! `genius_client_access_token`; when absent, Genius-gated verbs
//! return structured `not_configured` responses.
//!
//! ## Rate limiting
//!
//! Genius does not publicly document a rate limit but community
//! observation is ~5 requests per second. Default `min_interval`
//! is 250 ms; multiple concurrent callers share the same
//! [`RateLimiter`].
//!
//! ## Attribution
//!
//! Displayed API-sourced content from Genius requires
//! attribution ("Powered by Genius" or similar) on the operator
//! UI's rendering surface. Client responses carry a `source_url`
//! field so the UI can link back.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

const GENIUS_API_BASE: &str = "https://api.genius.com";

/// Errors from the Genius client.
#[derive(Debug, thiserror::Error)]
pub enum GeniusError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Genius returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("Genius JSON decode failed: {0}")]
    Decode(String),
}

/// Track-annotation hit.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackAnnotationHit {
    /// The song's Genius id.
    pub song_id: u64,
    /// Optional annotation prose (`description`).
    pub description: Option<String>,
    /// URL of the Genius web page for this song. When
    /// [`LrclibClient`] returns `not_found`, the operator UI can
    /// render this as an outbound "View lyrics on Genius" link.
    /// Never fetch this URL server-side to extract lyrics text.
    pub source_url: Option<String>,
}

/// Artist-description hit.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistDescriptionHit {
    /// The artist's Genius id.
    pub artist_id: u64,
    /// Optional description prose.
    pub description: Option<String>,
    /// URL of the Genius web page for the artist.
    pub source_url: Option<String>,
}

/// Genius JSON client.
#[derive(Clone)]
pub struct GeniusClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
    client_access_token: String,
}

impl GeniusClient {
    /// Construct a Genius client. Refuses an empty token so a
    /// caller with a `GeniusClient` in hand always knows the
    /// provider is armed.
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: String,
        client_access_token: String,
    ) -> Option<Self> {
        if client_access_token.trim().is_empty() {
            return None;
        }
        Some(Self {
            http,
            rate,
            user_agent,
            client_access_token,
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: String,
    ) -> Result<T, GeniusError> {
        self.rate.acquire().await;
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, self.user_agent.clone())
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.client_access_token),
            )
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(GeniusError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| GeniusError::Decode(format!("{e}")))
    }

    /// Fetch the annotation for `(artist, track)`. Returns `None`
    /// when Genius's search resolves no top-hit for the pair.
    pub async fn get_track_annotation(
        &self,
        artist: &str,
        track: &str,
    ) -> Result<Option<TrackAnnotationHit>, GeniusError> {
        let search_url = format!(
            "{GENIUS_API_BASE}/search?q={}",
            urlencode(&format!("{artist} {track}")),
        );
        let search: SearchResponse = self.get_json(search_url).await?;
        let Some(top) = search.response.hits.into_iter().next() else {
            return Ok(None);
        };
        let song_id = top.result.id;
        let detail_url = format!("{GENIUS_API_BASE}/songs/{song_id}");
        let detail: SongDetailResponse = self.get_json(detail_url).await?;
        let description = detail
            .response
            .song
            .description_annotation
            .and_then(|d| d.annotation.body.plain)
            .filter(|s| !s.trim().is_empty());
        Ok(Some(TrackAnnotationHit {
            song_id,
            description,
            source_url: Some(top.result.url),
        }))
    }

    /// Fetch the description for `artist`. Returns `None` when
    /// Genius's search resolves no hits for the query. Best-fit
    /// artist is derived from the top-hit's `primary_artist` id.
    pub async fn get_artist_description(
        &self,
        artist: &str,
    ) -> Result<Option<ArtistDescriptionHit>, GeniusError> {
        let search_url =
            format!("{GENIUS_API_BASE}/search?q={}", urlencode(artist));
        let search: SearchResponse = self.get_json(search_url).await?;
        let Some(top) = search.response.hits.into_iter().next() else {
            return Ok(None);
        };
        let artist_id = top.result.primary_artist.id;
        let detail_url = format!("{GENIUS_API_BASE}/artists/{artist_id}");
        let detail: ArtistDetailResponse = self.get_json(detail_url).await?;
        let description = detail
            .response
            .artist
            .description
            .and_then(|d| d.plain)
            .filter(|s| !s.trim().is_empty());
        Ok(Some(ArtistDescriptionHit {
            artist_id,
            description,
            source_url: Some(top.result.primary_artist.url),
        }))
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    response: SearchResponseInner,
}

#[derive(Debug, Deserialize)]
struct SearchResponseInner {
    #[serde(default)]
    hits: Vec<SearchHit>,
}

#[derive(Debug, Deserialize)]
struct SearchHit {
    result: SearchResult,
}

#[derive(Debug, Deserialize)]
struct SearchResult {
    id: u64,
    url: String,
    primary_artist: PrimaryArtist,
}

#[derive(Debug, Deserialize)]
struct PrimaryArtist {
    id: u64,
    url: String,
}

#[derive(Debug, Deserialize)]
struct SongDetailResponse {
    response: SongDetailResponseInner,
}

#[derive(Debug, Deserialize)]
struct SongDetailResponseInner {
    song: SongDetail,
}

#[derive(Debug, Deserialize)]
struct SongDetail {
    #[serde(default)]
    description_annotation: Option<DescriptionAnnotation>,
}

#[derive(Debug, Deserialize)]
struct DescriptionAnnotation {
    annotation: Annotation,
}

#[derive(Debug, Deserialize)]
struct Annotation {
    body: AnnotationBody,
}

#[derive(Debug, Deserialize)]
struct AnnotationBody {
    #[serde(default)]
    plain: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistDetailResponse {
    response: ArtistDetailResponseInner,
}

#[derive(Debug, Deserialize)]
struct ArtistDetailResponseInner {
    artist: ArtistDetail,
}

#[derive(Debug, Deserialize)]
struct ArtistDetail {
    #[serde(default)]
    description: Option<ArtistDescriptionBody>,
}

#[derive(Debug, Deserialize)]
struct ArtistDescriptionBody {
    #[serde(default)]
    plain: Option<String>,
}

fn urlencode(s: &str) -> String {
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
