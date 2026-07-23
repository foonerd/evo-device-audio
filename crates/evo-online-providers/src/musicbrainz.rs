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

/// Artist / composer / performer / conductor / ensemble search
/// hit. In MusicBrainz's schema every one of these entity classes
/// is an `artist` — the taxonomy distinguishes them via the
/// `type` field (e.g. `"Person"` for individuals, `"Group"` for
/// ensembles / orchestras). Cascade callers select by entity
/// type at their layer, not at MB.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistSearchHit {
    /// Artist MBID (opaque UUID string).
    pub artist_mbid: String,
    /// Canonical name from MB.
    pub canonical_name: String,
    /// MB entity type (`"Person"` / `"Group"` / `"Orchestra"` /
    /// `"Choir"` / `"Character"` / etc.). `None` when MB omits
    /// the field.
    pub artist_type: Option<String>,
    /// MB search score (0..100). Cascade callers typically
    /// require ≥85 for named-entity lookups.
    pub confidence_percent: u32,
}

/// Artist lookup with URL relationships. Cascade callers use the
/// Wikipedia / Wikidata URL entries to drive the anonymous
/// enrichment chain — MusicBrainz relation → Wikipedia summary
/// (bio prose) → Wikidata entity (biographical facts).
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistLookup {
    /// Artist MBID (echoes the input).
    pub artist_mbid: String,
    /// Canonical name from MB.
    pub canonical_name: String,
    /// MB entity type (`"Person"` etc.), if MB provided one.
    pub artist_type: Option<String>,
    /// Formation year (for groups) or birth year (for persons).
    pub life_span_begin: Option<String>,
    /// Dissolution year (for groups) or death year (for persons).
    pub life_span_end: Option<String>,
    /// Country of origin, when MB provides one.
    pub country: Option<String>,
    /// Wikipedia page URL from the artist's `wikipedia` URL-rel
    /// (when present). The cascade uses this to jump to Wikipedia's
    /// summary API — no keyword search needed.
    pub wikipedia_url: Option<String>,
    /// Wikidata entity URL (Q-id) from the `wikidata` URL-rel.
    /// The cascade uses this for structured biographical facts
    /// (place of birth, genres, activity dates).
    pub wikidata_url: Option<String>,
    /// Official homepage URL if the artist has one linked.
    pub official_homepage_url: Option<String>,
}

/// Work search hit (classical composition / opera / etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkSearchHit {
    /// Work MBID.
    pub work_mbid: String,
    /// Canonical title.
    pub canonical_title: String,
    /// MB work type (e.g. `"Symphony"`, `"Sonata"`, `"Opera"`,
    /// `"Song"`).
    pub work_type: Option<String>,
    /// MB search score (0..100).
    pub confidence_percent: u32,
}

/// Work lookup with URL relationships.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkLookup {
    /// Work MBID.
    pub work_mbid: String,
    /// Canonical title.
    pub canonical_title: String,
    /// MB work type.
    pub work_type: Option<String>,
    /// Wikipedia page URL from `wikipedia` URL-rel.
    pub wikipedia_url: Option<String>,
    /// Wikidata entity URL from `wikidata` URL-rel.
    pub wikidata_url: Option<String>,
    /// IMSLP score URL from `imslp` URL-rel, when present. The
    /// cascade does not fetch IMSLP directly; the URL surfaces
    /// to the operator UI as a "score available" link.
    pub imslp_url: Option<String>,
}

/// Fuller release lookup for keyless release-credits. Extends
/// [`ReleaseLookup`] with per-recording artist / performer /
/// conductor / composer credits so classical personnel land
/// without keyed providers.
#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseCreditsLookup {
    /// Release MBID.
    pub release_mbid: String,
    /// First release year at the release-group level (same as
    /// [`ReleaseLookup::first_release_year`]).
    pub first_release_year: Option<u16>,
    /// Recording type (same taxonomy as
    /// [`ReleaseLookup::recording_type`]).
    pub recording_type: String,
    /// First label + its catalogue number (when the release
    /// carries at least one label). `None` on releases without
    /// label info.
    pub label_name: Option<String>,
    /// First catalogue number from the label list.
    pub catalog_number: Option<String>,
    /// Release-group's overall artist credit (album-level).
    pub album_artist: Option<String>,
    /// Country of release, when MB has one.
    pub country: Option<String>,
    /// Track-level credits. Every element is one recording on the
    /// release, in track order across media. Length equals the
    /// release's total track count.
    pub tracks: Vec<TrackCredits>,
}

/// Per-track credit block extracted from a fuller release
/// lookup's `media[].tracks[].recording` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackCredits {
    /// Track position on the release, 1-indexed across all media.
    pub position: u32,
    /// Track title from the recording.
    pub title: String,
    /// Recording length in milliseconds (from MB's `length` field).
    pub length_ms: Option<u32>,
    /// Track-level artist credit (may differ from album artist on
    /// compilations and classical releases).
    pub track_artist: Option<String>,
    /// Composer name(s) extracted from the recording's `composer`
    /// / `writer` artist-rels. Multiple composers joined with
    /// `", "`.
    pub composer: Option<String>,
    /// Conductor name(s) extracted from the recording's
    /// `conductor` artist-rels.
    pub conductor: Option<String>,
    /// Performer name(s) extracted from the recording's
    /// `performer` artist-rels (soloist + ensemble members).
    pub performer: Option<String>,
    /// Work MBID reached via the recording's `performance`
    /// work-rel, when present. Classical driver for the work
    /// enrichment cascade.
    pub work_mbid: Option<String>,
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

    /// Search MB for an entity (artist / composer / performer /
    /// conductor / ensemble; all share the `artist` entity type in
    /// MB's schema) by name. Returns the top hit's MBID and
    /// canonical name.
    pub async fn search_artist(
        &self,
        name: &str,
    ) -> Result<Option<ArtistSearchHit>, MusicBrainzError> {
        self.rate.acquire().await;
        let query = format!("artist:\"{}\"", escape_lucene(name));
        let url = format!("{MB_API_BASE}/artist");
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
        let body: ArtistSearchResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(body.artists.into_iter().next().map(|a| ArtistSearchHit {
            artist_mbid: a.id,
            canonical_name: a.name,
            artist_type: a.artist_type,
            confidence_percent: a.score,
        }))
    }

    /// Look up an entity by MBID with URL relationships included
    /// (`inc=url-rels`). Callers extract the Wikipedia / Wikidata
    /// / official-site URLs from the returned `relations` list to
    /// drive the keyless Wikipedia / Wikidata enrichment cascade.
    pub async fn lookup_artist(
        &self,
        artist_mbid: &str,
    ) -> Result<ArtistLookup, MusicBrainzError> {
        self.rate.acquire().await;
        let url = format!("{MB_API_BASE}/artist/{artist_mbid}");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("inc", "url-rels"), ("fmt", "json")])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MusicBrainzError::Status { status, body });
        }
        let body: ArtistLookupResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(artist_lookup_from_response(body))
    }

    /// Search MB for a work (classical / opera / composition) by
    /// title, optionally scoped by composer name. Returns the top
    /// hit.
    pub async fn search_work(
        &self,
        title: &str,
        composer: Option<&str>,
    ) -> Result<Option<WorkSearchHit>, MusicBrainzError> {
        self.rate.acquire().await;
        let query = match composer {
            Some(c) => format!(
                "work:\"{}\" AND artist:\"{}\"",
                escape_lucene(title),
                escape_lucene(c),
            ),
            None => format!("work:\"{}\"", escape_lucene(title)),
        };
        let url = format!("{MB_API_BASE}/work");
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
        let body: WorkSearchResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(body.works.into_iter().next().map(|w| WorkSearchHit {
            work_mbid: w.id,
            canonical_title: w.title,
            work_type: w.work_type,
            confidence_percent: w.score,
        }))
    }

    /// Look up a work by MBID with URL relationships included.
    /// Callers extract Wikipedia / Wikidata / IMSLP links from
    /// `relations`.
    pub async fn lookup_work(
        &self,
        work_mbid: &str,
    ) -> Result<WorkLookup, MusicBrainzError> {
        self.rate.acquire().await;
        let url = format!("{MB_API_BASE}/work/{work_mbid}");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[("inc", "url-rels"), ("fmt", "json")])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MusicBrainzError::Status { status, body });
        }
        let body: WorkLookupResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(work_lookup_from_response(body))
    }

    /// Fuller release lookup with per-track credits and labels.
    /// Covers the keyless-credits + classical personnel path:
    /// `inc=artist-credits+labels+recordings+work-rels`. Every
    /// recording carries its own artist credits so classical
    /// personnel (soloist / conductor / ensemble) surface without
    /// keyed providers.
    pub async fn lookup_release_full(
        &self,
        release_mbid: &str,
    ) -> Result<ReleaseCreditsLookup, MusicBrainzError> {
        self.rate.acquire().await;
        let url = format!("{MB_API_BASE}/release/{release_mbid}");
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .query(&[
                (
                    "inc",
                    "artist-credits+labels+recordings+work-rels+recording-level-rels+artist-rels",
                ),
                ("fmt", "json"),
            ])
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(MusicBrainzError::Status { status, body });
        }
        let body: ReleaseCreditsResponse = resp
            .json()
            .await
            .map_err(|e| MusicBrainzError::Decode(e.to_string()))?;
        Ok(release_credits_from_response(body))
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

// --------------------------------------------------------------
// Artist / work / fuller-release response types + parsers.
// --------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ArtistSearchResponse {
    #[serde(default)]
    artists: Vec<ArtistSearchEntry>,
}

#[derive(Debug, Deserialize)]
struct ArtistSearchEntry {
    id: String,
    name: String,
    #[serde(default)]
    score: u32,
    #[serde(rename = "type", default)]
    artist_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistLookupResponse {
    id: String,
    name: String,
    #[serde(rename = "type", default)]
    artist_type: Option<String>,
    #[serde(rename = "life-span", default)]
    life_span: Option<LifeSpan>,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    relations: Vec<UrlRelation>,
}

#[derive(Debug, Deserialize)]
struct LifeSpan {
    #[serde(default)]
    begin: Option<String>,
    #[serde(default)]
    end: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UrlRelation {
    #[serde(rename = "type", default)]
    rel_type: String,
    #[serde(default)]
    url: Option<UrlObject>,
}

#[derive(Debug, Deserialize)]
struct UrlObject {
    #[serde(default)]
    resource: Option<String>,
}

fn extract_url_for_type(
    relations: &[UrlRelation],
    rel_type: &str,
) -> Option<String> {
    relations
        .iter()
        .find(|r| r.rel_type.eq_ignore_ascii_case(rel_type))
        .and_then(|r| r.url.as_ref())
        .and_then(|u| u.resource.clone())
}

fn artist_lookup_from_response(r: ArtistLookupResponse) -> ArtistLookup {
    let (life_span_begin, life_span_end) = match r.life_span {
        Some(l) => (l.begin, l.end),
        None => (None, None),
    };
    ArtistLookup {
        artist_mbid: r.id,
        canonical_name: r.name,
        artist_type: r.artist_type,
        life_span_begin,
        life_span_end,
        country: r.country,
        wikipedia_url: extract_url_for_type(&r.relations, "wikipedia"),
        wikidata_url: extract_url_for_type(&r.relations, "wikidata"),
        official_homepage_url: extract_url_for_type(
            &r.relations,
            "official homepage",
        ),
    }
}

#[derive(Debug, Deserialize)]
struct WorkSearchResponse {
    #[serde(default)]
    works: Vec<WorkSearchEntry>,
}

#[derive(Debug, Deserialize)]
struct WorkSearchEntry {
    id: String,
    title: String,
    #[serde(default)]
    score: u32,
    #[serde(rename = "type", default)]
    work_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WorkLookupResponse {
    id: String,
    title: String,
    #[serde(rename = "type", default)]
    work_type: Option<String>,
    #[serde(default)]
    relations: Vec<UrlRelation>,
}

fn work_lookup_from_response(r: WorkLookupResponse) -> WorkLookup {
    WorkLookup {
        work_mbid: r.id,
        canonical_title: r.title,
        work_type: r.work_type,
        wikipedia_url: extract_url_for_type(&r.relations, "wikipedia"),
        wikidata_url: extract_url_for_type(&r.relations, "wikidata"),
        imslp_url: extract_url_for_type(&r.relations, "imslp"),
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseCreditsResponse {
    id: String,
    #[serde(rename = "release-group", default)]
    release_group: Option<LookupReleaseGroup>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(rename = "label-info", default)]
    label_info: Vec<LabelInfo>,
    #[serde(default)]
    media: Vec<FullMedium>,
}

#[derive(Debug, Deserialize)]
struct LabelInfo {
    #[serde(rename = "catalog-number", default)]
    catalog_number: Option<String>,
    #[serde(default)]
    label: Option<LabelEntry>,
}

#[derive(Debug, Deserialize)]
struct LabelEntry {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FullMedium {
    #[serde(default)]
    tracks: Vec<FullTrack>,
}

#[derive(Debug, Deserialize)]
struct FullTrack {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u32>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Vec<ArtistCredit>,
    #[serde(default)]
    recording: Option<TrackRecording>,
}

#[derive(Debug, Deserialize)]
struct TrackRecording {
    #[serde(default)]
    length: Option<u32>,
    #[serde(default)]
    relations: Vec<RecordingRelation>,
}

#[derive(Debug, Deserialize)]
struct RecordingRelation {
    #[serde(rename = "type", default)]
    rel_type: String,
    #[serde(default)]
    artist: Option<RelArtist>,
    #[serde(default)]
    work: Option<RelWork>,
}

#[derive(Debug, Deserialize)]
struct RelArtist {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RelWork {
    id: String,
}

fn release_credits_from_response(
    r: ReleaseCreditsResponse,
) -> ReleaseCreditsLookup {
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
    let album_artist = r
        .artist_credit
        .iter()
        .filter_map(|c| {
            c.name
                .clone()
                .or_else(|| c.artist.as_ref().map(|a| a.name.clone()))
        })
        .next();
    let (label_name, catalog_number) = r
        .label_info
        .into_iter()
        .next()
        .map(|li| (li.label.and_then(|l| l.name), li.catalog_number))
        .unwrap_or((None, None));
    let mut tracks: Vec<TrackCredits> = Vec::new();
    let mut running_position: u32 = 0;
    for medium in r.media {
        for t in medium.tracks {
            running_position += 1;
            let position = t.position.unwrap_or(running_position);
            let title = t.title.clone().unwrap_or_else(|| {
                t.recording
                    .as_ref()
                    .map(|_| String::new())
                    .unwrap_or_default()
            });
            let length_ms = t
                .length
                .or_else(|| t.recording.as_ref().and_then(|r| r.length));
            let track_artist = t
                .artist_credit
                .iter()
                .filter_map(|c| {
                    c.name
                        .clone()
                        .or_else(|| c.artist.as_ref().map(|a| a.name.clone()))
                })
                .next();
            let (composer, conductor, performer, work_mbid) = match &t.recording
            {
                Some(rec) => (
                    collect_relation_names(
                        &rec.relations,
                        &["composer", "writer"],
                    ),
                    collect_relation_names(&rec.relations, &["conductor"]),
                    collect_relation_names(
                        &rec.relations,
                        &["performer", "instrument", "vocal"],
                    ),
                    first_work_mbid(&rec.relations),
                ),
                None => (None, None, None, None),
            };
            tracks.push(TrackCredits {
                position,
                title,
                length_ms,
                track_artist,
                composer,
                conductor,
                performer,
                work_mbid,
            });
        }
    }
    ReleaseCreditsLookup {
        release_mbid: r.id,
        first_release_year,
        recording_type,
        label_name,
        catalog_number,
        album_artist,
        country: r.country,
        tracks,
    }
}

fn collect_relation_names(
    relations: &[RecordingRelation],
    match_types: &[&str],
) -> Option<String> {
    let names: Vec<String> = relations
        .iter()
        .filter(|r| {
            match_types
                .iter()
                .any(|m| r.rel_type.eq_ignore_ascii_case(m))
        })
        .filter_map(|r| r.artist.as_ref().and_then(|a| a.name.clone()))
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn first_work_mbid(relations: &[RecordingRelation]) -> Option<String> {
    relations
        .iter()
        .filter(|r| r.rel_type.eq_ignore_ascii_case("performance"))
        .filter_map(|r| r.work.as_ref().map(|w| w.id.clone()))
        .next()
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
