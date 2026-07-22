// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! MusicBrainz JSON API client for the audio distribution's
//! online-metadata plugins.
//!
//! Two operations:
//!
//! - [`MusicBrainzClient::search_release`] — search the MB
//!   catalogue for a `(artist, album)` pair, return the top
//!   match's release MBID + release-group MBID + artist MBID +
//!   confidence score.
//! - [`MusicBrainzClient::lookup_release`] — given a release
//!   MBID, fetch the release object with the release-group
//!   inclusion so callers can determine `recording_type`
//!   (Studio / Live / Compilation / Soundtrack), first-release
//!   year, and track count.
//!
//! Every outbound call passes through the shared [`RateLimiter`]
//! so multiple plugins hitting MusicBrainz from the same device
//! honour the API's 1 request per second policy.
//!
//! ## User-Agent
//!
//! MusicBrainz's Terms of Use require every client to send a
//! User-Agent that identifies the software + version + contact
//! info. The distribution's canonical string is threaded through
//! the client constructor by the calling plugin — this crate
//! does not fabricate a default. A missing UA on construction is
//! a plugin-side configuration error, not a runtime failure.
//!
//! ## Confidence scoring
//!
//! MB's search endpoint returns a `score` field (0..100) with
//! the top hit typically at 100 for exact matches. The client
//! passes it through verbatim on
//! [`SearchHit::confidence_percent`] so the caller can enforce
//! a minimum threshold before accepting a reconciliation.
//! Reconciliation callers typically require ≥90 to avoid weak
//! matches (a soundtrack album whose artist is "Various
//! Artists" often scores in the mid-80s for a wrong-artist
//! query).

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

/// MusicBrainz API base. Version 2 of the schema — has been
/// stable since 2011 and is what every current MB client uses.
const MB_API_BASE: &str = "https://musicbrainz.org/ws/2";

/// Structured errors from the MB client.
#[derive(Debug, thiserror::Error)]
pub enum MusicBrainzError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("MusicBrainz returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("MusicBrainz JSON decode failed: {0}")]
    Decode(String),
}

/// One release hit from the MB search endpoint. Fields the
/// reconciliation caller pulls into its canonical response.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Release MBID (opaque UUID string).
    pub release_mbid: String,
    /// Release-group MBID — reconciliation callers persist this
    /// alongside the release MBID because it is the stable
    /// identity for browse-by-album (different releases of the
    /// same album share a release-group).
    pub release_group_mbid: Option<String>,
    /// Artist MBID — first credited artist on the release.
    /// Reconciliation callers use it to key artist-scoped
    /// enrichment queries downstream.
    pub artist_mbid: Option<String>,
    /// Canonical artist string from MB (post-punctuation
    /// normalisation). Callers surface this on the reconciled
    /// response so operator UI shows the catalogue's
    /// authoritative form.
    pub canonical_artist: String,
    /// Canonical album (release title).
    pub canonical_album: String,
    /// MB's search score (0..100). Reconciliation callers apply
    /// a minimum threshold before accepting the hit.
    pub confidence_percent: u32,
}

/// Full release lookup result (release + inline release-group).
///
/// Callers use this after `search_release` returns a
/// `SearchHit` to determine the recording type + first-release
/// year + track count — data the search endpoint does not
/// include.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseLookup {
    /// First release date at the release-group level — the
    /// year the ALBUM (not any specific pressing) was first
    /// released. Falls back to the release's own date when the
    /// group's date is absent.
    pub first_release_year: Option<u16>,
    /// Recording type derived from release-group primary +
    /// secondary types. Canonical values:
    ///
    /// - `Studio` — primary "Album", no live/compilation/etc
    ///   in secondaries
    /// - `Live` — secondaries include "Live"
    /// - `Compilation` — secondaries include "Compilation"
    /// - `Soundtrack` — secondaries include "Soundtrack"
    /// - `EP` / `Single` / `Broadcast` / `Other` — primary
    ///   type maps directly
    ///
    /// Every value is a stable string the browse-by-recording-
    /// type verb (piece 5) filters on.
    pub recording_type: String,
    /// Track count.
    pub track_count: Option<u32>,
}

/// MusicBrainz JSON client. Wraps a shared HTTPS client + a
/// shared per-service token bucket + a caller-provided
/// User-Agent string.
///
/// Thread-safe (`Clone` produces a lightweight handle sharing
/// the underlying `Arc<RateLimiter>` and reqwest client). Multi-
/// plugin sharing: both artwork.online (for CAA MBID lookups)
/// and metadata.online (for reconciliation) hold a clone of
/// the same client so their outbound cadence is jointly
/// governed by the one rate limiter.
#[derive(Clone)]
pub struct MusicBrainzClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
}

impl MusicBrainzClient {
    /// Construct a client. `user_agent` MUST include product
    /// name + version + contact string per MB's Terms of Use;
    /// callers embed the distribution's canonical form (e.g.
    /// `evo-device-audio/0.1.13 (+https://github.com/foonerd/evo-device-audio)`).
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

    /// Search MB for a `(artist, album)` pair; return the
    /// highest-scoring release. Returns `Ok(None)` on no match
    /// (empty result array from MB).
    ///
    /// Formats a MB Lucene-syntax query with the artist and
    /// release title terms quoted to protect against special-
    /// character interference; requests JSON via the `fmt=json`
    /// parameter (MB's Accept-negotiation is fragile — the
    /// explicit format param wins on every version).
    pub async fn search_release(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Option<SearchHit>, MusicBrainzError> {
        self.rate.acquire().await;
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album),
        );
        let url = format!("{MB_API_BASE}/release");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[
                ("query", query.as_str()),
                ("fmt", "json"),
                ("limit", "3"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MusicBrainzError::Status { status, body });
        }
        let body: SearchResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(body
            .releases
            .into_iter()
            .next()
            .map(hit_from_search_release))
    }

    /// Look up a release by MBID, inlining the release-group so
    /// the caller can determine `recording_type` + first-release
    /// year without a second round trip.
    pub async fn lookup_release(
        &self,
        release_mbid: &str,
    ) -> Result<ReleaseLookup, MusicBrainzError> {
        self.rate.acquire().await;
        let url = format!("{MB_API_BASE}/release/{release_mbid}");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("inc", "release-groups media"), ("fmt", "json")])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MusicBrainzError::Status { status, body });
        }
        let body: LookupResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(lookup_from_release(body))
    }
}

// --------------------------------------------------------------
// MB JSON response shapes — private; the public surface is the
// two `SearchHit` and `ReleaseLookup` types above.
// --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    releases: Vec<SearchRelease>,
}

#[derive(Debug, Deserialize)]
struct SearchRelease {
    id: String,
    #[serde(default)]
    score: u32,
    title: String,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "release-group", default)]
    release_group: Option<ReleaseGroupRef>,
}

#[derive(Debug, Deserialize)]
struct ArtistCredit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    artist: Option<ArtistRef>,
}

#[derive(Debug, Deserialize)]
struct ArtistRef {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseGroupRef {
    id: String,
}

fn hit_from_search_release(r: SearchRelease) -> SearchHit {
    let artist_mbid = r
        .artist_credit
        .first()
        .and_then(|c| c.artist.as_ref())
        .map(|a| a.id.clone());
    let canonical_artist = r
        .artist_credit
        .iter()
        .filter_map(|c| {
            c.name
                .clone()
                .or_else(|| c.artist.as_ref().map(|a| a.name.clone()))
        })
        .next()
        .unwrap_or_default();
    SearchHit {
        release_mbid: r.id,
        release_group_mbid: r.release_group.map(|g| g.id),
        artist_mbid,
        canonical_artist,
        canonical_album: r.title,
        confidence_percent: r.score,
    }
}

#[derive(Debug, Deserialize)]
struct LookupResponse {
    #[serde(rename = "release-group", default)]
    release_group: Option<LookupReleaseGroup>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    media: Vec<LookupMedium>,
}

#[derive(Debug, Deserialize)]
struct LookupReleaseGroup {
    #[serde(rename = "primary-type", default)]
    primary_type: Option<String>,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Vec<String>,
    #[serde(rename = "first-release-date", default)]
    first_release_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LookupMedium {
    #[serde(rename = "track-count", default)]
    track_count: Option<u32>,
}

fn lookup_from_release(r: LookupResponse) -> ReleaseLookup {
    let (recording_type, first_release_year) = match &r.release_group {
        Some(g) => (
            classify_recording_type(
                g.primary_type.as_deref(),
                &g.secondary_types,
            ),
            year_from_date_str(g.first_release_date.as_deref())
                .or_else(|| year_from_date_str(r.date.as_deref())),
        ),
        None => ("Other".to_string(), year_from_date_str(r.date.as_deref())),
    };
    let track_count = r.media.iter().filter_map(|m| m.track_count).sum::<u32>();
    let track_count = if track_count > 0 {
        Some(track_count)
    } else {
        None
    };
    ReleaseLookup {
        first_release_year,
        recording_type,
        track_count,
    }
}

/// Map MB primary + secondary release-group types to the
/// distribution's canonical `recording_type` string.
///
/// MB's taxonomy: primary types are `Album`, `Single`, `EP`,
/// `Broadcast`, `Other`. Secondary types include `Live`,
/// `Compilation`, `Soundtrack`, `Remix`, `Demo`, etc.
///
/// The distribution's canonical values are a small stable set
/// the browse-by-recording-type verb (piece 5) filters on. When
/// a release has multiple secondaries (e.g. Live + Compilation),
/// Live takes priority — operators browsing "Live" want to
/// see everything performed live regardless of whether it also
/// hits a compilation shelf.
pub fn classify_recording_type(
    primary: Option<&str>,
    secondaries: &[String],
) -> String {
    let is_live = secondaries.iter().any(|s| s.eq_ignore_ascii_case("Live"));
    let is_compilation = secondaries
        .iter()
        .any(|s| s.eq_ignore_ascii_case("Compilation"));
    let is_soundtrack = secondaries
        .iter()
        .any(|s| s.eq_ignore_ascii_case("Soundtrack"));
    if is_live {
        return "Live".to_string();
    }
    if is_soundtrack {
        return "Soundtrack".to_string();
    }
    if is_compilation {
        return "Compilation".to_string();
    }
    match primary {
        Some(p) if p.eq_ignore_ascii_case("Album") => "Studio".to_string(),
        Some(p) if p.eq_ignore_ascii_case("Single") => "Single".to_string(),
        Some(p) if p.eq_ignore_ascii_case("EP") => "EP".to_string(),
        Some(p) if p.eq_ignore_ascii_case("Broadcast") => {
            "Broadcast".to_string()
        }
        Some(p) if !p.is_empty() => p.to_string(),
        _ => "Other".to_string(),
    }
}

fn year_from_date_str(s: Option<&str>) -> Option<u16> {
    let s = s?;
    // MB dates are `YYYY`, `YYYY-MM`, or `YYYY-MM-DD`. Take the
    // first four ASCII digits; if there are none, return None.
    let year: String = s.chars().take(4).collect();
    year.parse::<u16>().ok()
}

/// Escape Lucene-syntax metacharacters in a query term so a
/// user-supplied artist / album containing (say) parentheses
/// or a hyphen doesn't misparse into a boolean query fragment.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' | '"' | '+' | '-' | '!' | '(' | ')' | '{' | '}' | '['
            | ']' | '^' | '~' | '*' | '?' | ':' | '/' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recording_type_prefers_live_over_compilation() {
        assert_eq!(
            classify_recording_type(
                Some("Album"),
                &["Live".to_string(), "Compilation".to_string()]
            ),
            "Live"
        );
    }

    #[test]
    fn classify_recording_type_studio_when_no_secondaries() {
        assert_eq!(classify_recording_type(Some("Album"), &[]), "Studio");
    }

    #[test]
    fn classify_recording_type_soundtrack_beats_compilation() {
        assert_eq!(
            classify_recording_type(
                Some("Album"),
                &["Soundtrack".to_string(), "Compilation".to_string()]
            ),
            "Soundtrack"
        );
    }

    #[test]
    fn classify_recording_type_case_insensitive() {
        assert_eq!(
            classify_recording_type(Some("album"), &["live".to_string()]),
            "Live"
        );
        assert_eq!(classify_recording_type(Some("EP"), &[]), "EP");
    }

    #[test]
    fn classify_recording_type_falls_through_to_other() {
        assert_eq!(classify_recording_type(None, &[]), "Other");
        assert_eq!(classify_recording_type(Some(""), &[]), "Other");
    }

    #[test]
    fn year_from_date_handles_yyyy_yyyymm_yyyymmdd() {
        assert_eq!(year_from_date_str(Some("1997")), Some(1997));
        assert_eq!(year_from_date_str(Some("1997-06")), Some(1997));
        assert_eq!(year_from_date_str(Some("1997-06-21")), Some(1997));
        assert_eq!(year_from_date_str(Some("")), None);
        assert_eq!(year_from_date_str(None), None);
    }

    #[test]
    fn escape_lucene_wraps_metacharacters() {
        assert_eq!(escape_lucene("Rock 'n' Roll"), "Rock 'n' Roll");
        assert_eq!(escape_lucene("A:B"), "A\\:B");
        assert_eq!(escape_lucene("(live)"), "\\(live\\)");
        assert_eq!(escape_lucene("A+B"), "A\\+B");
    }

    #[test]
    fn hit_from_search_release_pulls_first_credit() {
        // Synth an MB search response and verify the hit
        // extractor pulls artist MBID + canonical name + album
        // title + score.
        let raw = r#"{
            "id": "release-mbid",
            "score": 100,
            "title": "OK Computer",
            "artist-credit": [
                {"name": "Radiohead", "artist": {"id": "artist-mbid", "name": "Radiohead"}}
            ],
            "release-group": {"id": "release-group-mbid"}
        }"#;
        let release: SearchRelease = serde_json::from_str(raw).unwrap();
        let hit = hit_from_search_release(release);
        assert_eq!(hit.release_mbid, "release-mbid");
        assert_eq!(
            hit.release_group_mbid.as_deref(),
            Some("release-group-mbid")
        );
        assert_eq!(hit.artist_mbid.as_deref(), Some("artist-mbid"));
        assert_eq!(hit.canonical_artist, "Radiohead");
        assert_eq!(hit.canonical_album, "OK Computer");
        assert_eq!(hit.confidence_percent, 100);
    }

    #[test]
    fn lookup_from_release_derives_recording_type_and_year() {
        // Album + Live secondary → Live; group's first-release-
        // date wins over release's; track count sums across
        // media.
        let raw = r#"{
            "release-group": {
                "primary-type": "Album",
                "secondary-types": ["Live"],
                "first-release-date": "1994-05-30"
            },
            "date": "2010-01-01",
            "media": [
                {"track-count": 12},
                {"track-count": 3}
            ]
        }"#;
        let response: LookupResponse = serde_json::from_str(raw).unwrap();
        let lookup = lookup_from_release(response);
        assert_eq!(lookup.recording_type, "Live");
        assert_eq!(lookup.first_release_year, Some(1994));
        assert_eq!(lookup.track_count, Some(15));
    }
}
