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

/// Artist-image hit — the URL of a photograph Discogs holds for
/// the artist, plus the artist page to attribute it to.
///
/// Distinct from [`ArtistProfileHit`] on purpose: the bio surface
/// and the artwork surface live in different plugins, and a
/// caller that wants a picture should not pay for a profile-text
/// round trip (or vice versa).
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistImageHit {
    /// Full-size image URL. Callers fetch and transcode this
    /// themselves; the client returns a reference, never bytes,
    /// matching how every other artwork provider behaves here.
    pub image_url: String,
    /// Artist page on Discogs, for operator-facing attribution.
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

    /// Wait for a rate-limit slot, then dispatch.
    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: String,
    ) -> Result<T, DiscogsError> {
        self.rate.acquire().await;
        self.get_json_dispatched(url).await
    }

    /// Dispatch immediately, assuming the caller has already
    /// taken a rate-limit slot. Never consults the limiter, so
    /// calling it without a slot in hand overruns the budget.
    async fn get_json_dispatched<T: for<'de> Deserialize<'de>>(
        &self,
        url: String,
    ) -> Result<T, DiscogsError> {
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, self.user_agent.clone())
            .header(reqwest::header::AUTHORIZATION, self.auth_header())
            // Ask Discogs for the plaintext-annotated dialect so the
            // response carries `notes_plaintext` / `profile_plaintext`
            // fields alongside the raw `notes` / `profile`. Without
            // this the default v2 dialect returns bracketed reference
            // codes verbatim (`[l333658]` = label id, `[r=571297]` =
            // release id) that no downstream consumer resolves,
            // rendering "All songs owned by [l333658]" to the
            // operator. With this dialect Discogs resolves the codes
            // server-side into human-readable text and hands us the
            // resolved form on the `_plaintext` twin. No extra
            // per-id round trips; the resolution counts against the
            // same rate-limit bucket as the original request.
            .header(
                reqwest::header::ACCEPT,
                "application/vnd.discogs.v2.plaintext+json",
            )
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
        // Prefer the plaintext-annotated twin; fall back to the raw
        // `notes` only when the plaintext field is absent (Discogs
        // API versioning / edge cases). Empty strings collapse to
        // `None` at the same time so a blank plaintext field never
        // shadows a populated raw fallback.
        let notes = plaintext_or_raw(detail.notes_plaintext, detail.notes);
        Ok(Some(ReleaseDetailHit {
            release_id,
            label,
            catalog_number,
            year: detail.year,
            country: detail.country,
            format,
            notes,
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
        // Same plaintext-preference as release notes above: the
        // artist profile field also carries `[a=NNN]`-style
        // reference codes on the raw form, resolved by Discogs on
        // the plaintext twin.
        let profile =
            plaintext_or_raw(detail.profile_plaintext, detail.profile);
        Ok(Some(ArtistProfileHit {
            profile,
            source_url: Some(format!(
                "https://www.discogs.com/artist/{}",
                first.id
            )),
        }))
    }

    /// Fetch an artist photograph URL for `artist`.
    ///
    /// Returns `Ok(None)` when the search resolves no artist, or
    /// when the resolved artist carries no imagery — both are
    /// ordinary misses that let a cascade walk on to the next
    /// provider, not errors. Returns `Err` on transport or decode
    /// failure.
    ///
    /// Costs two rate-limited requests on a hit (search, then
    /// detail), the same shape as [`Self::get_artist_profile`].
    /// Both share the client's single limiter, so the ceiling
    /// holds across the artwork and text surfaces together.
    ///
    /// Prefers the image Discogs marks `primary`; falls back to
    /// the first entry carrying a URL when no primary is flagged,
    /// because an artist with only `secondary` images still has a
    /// usable photograph and refusing it would leave the tile
    /// blank for no reason.
    pub async fn get_artist_image(
        &self,
        artist: &str,
    ) -> Result<Option<ArtistImageHit>, DiscogsError> {
        match self.artist_image_impl(artist, true).await? {
            ArtistImageAttempt::Completed(hit) => Ok(hit),
            // Unreachable: the blocking path waits for a slot
            // rather than reporting one unavailable.
            ArtistImageAttempt::RateLimited => Ok(None),
        }
    }

    /// Look up an artist image without ever waiting on the shared
    /// 1 req/s budget.
    ///
    /// Yields [`ArtistImageAttempt::RateLimited`] — having
    /// dispatched nothing — when no slot is free, so a caller on
    /// a latency-sensitive path can record a transient miss and
    /// move on instead of holding its whole provider wave open.
    /// A browse grid resolving many artists at once would
    /// otherwise pay the budget serially and inherit Discogs'
    /// ceiling as its own page latency.
    ///
    /// The caller is expected to treat `RateLimited` as transient
    /// (retry on a later pass), never as an absence.
    pub async fn try_get_artist_image(
        &self,
        artist: &str,
    ) -> Result<ArtistImageAttempt, DiscogsError> {
        self.artist_image_impl(artist, false).await
    }

    /// Shared body for the blocking and non-blocking lookups.
    /// `blocking` selects how each of the two rate-limited
    /// requests (search, then detail) takes its slot.
    async fn artist_image_impl(
        &self,
        artist: &str,
        blocking: bool,
    ) -> Result<ArtistImageAttempt, DiscogsError> {
        // Only the ENTRY to the lookup is non-blocking. This is
        // a two-request shape (search, then detail), and the two
        // are milliseconds apart — far inside the 1 req/s budget.
        // Applying the non-blocking rule to both would refuse the
        // detail slot on essentially every call, so the lookup
        // would spend a search request and then abandon it, and
        // the provider could never return an image at all.
        //
        // Gating the entry is what protects a browse: only an
        // artist that wins a free slot starts a lookup, so a grid
        // does not queue tile behind tile. Once a search has been
        // spent the lookup is committed, and the detail call waits
        // for its slot — a bounded wait of at most one interval,
        // paid only by a caller already past the gate.
        let search_url = format!(
            "{DISCOGS_API_BASE}/database/search?type=artist&q={}&per_page=1",
            urlencode(artist),
        );
        if blocking {
            self.rate.acquire().await;
        } else if !self.rate.try_acquire().await {
            return Ok(ArtistImageAttempt::RateLimited);
        }
        let search: ArtistSearchResponse =
            self.get_json_dispatched(search_url).await?;
        let Some(first) = search.results.into_iter().next() else {
            return Ok(ArtistImageAttempt::Completed(None));
        };
        let detail_url = format!("{DISCOGS_API_BASE}/artists/{}", first.id);
        self.rate.acquire().await;
        let detail: ArtistDetail = self.get_json_dispatched(detail_url).await?;
        let images = detail.images.unwrap_or_default();
        let pick = images
            .iter()
            .find(|i| {
                i.kind.as_deref() == Some("primary")
                    && i.uri.as_deref().is_some_and(|u| !u.trim().is_empty())
            })
            .or_else(|| {
                images.iter().find(|i| {
                    i.uri.as_deref().is_some_and(|u| !u.trim().is_empty())
                })
            });
        let Some(entry) = pick else {
            return Ok(ArtistImageAttempt::Completed(None));
        };
        let Some(image_url) = entry.uri.as_ref() else {
            return Ok(ArtistImageAttempt::Completed(None));
        };
        Ok(ArtistImageAttempt::Completed(Some(ArtistImageHit {
            image_url: image_url.clone(),
            source_url: Some(format!(
                "https://www.discogs.com/artist/{}",
                first.id
            )),
        })))
    }
}

/// Outcome of a non-blocking artist-image lookup.
///
/// Separates "we asked and this is the answer" from "we never
/// asked, because the rate-limit budget was spent". Collapsing
/// the two into `Option` would let a caller memoise a
/// rate-limited pass as a durable absence.
#[derive(Debug, Clone, PartialEq)]
pub enum ArtistImageAttempt {
    /// The lookup ran to completion: `Some` on a hit, `None` on
    /// an ordinary miss (no search match, or no usable imagery).
    Completed(Option<ArtistImageHit>),
    /// No rate-limit slot was free. Nothing was dispatched and
    /// nothing is known about this artist; retry on a later pass.
    RateLimited,
}

/// Prefer the plaintext-annotated field, fall back to the raw
/// field, and treat empty / whitespace-only strings as absent so
/// a blank plaintext value doesn't shadow a populated raw one.
fn plaintext_or_raw(
    plaintext: Option<String>,
    raw: Option<String>,
) -> Option<String> {
    let clean = |s: String| {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    plaintext.and_then(clean).or_else(|| raw.and_then(clean))
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
    /// The raw notes field — carries Discogs' bracketed reference
    /// codes verbatim (`[l333658]`, `[r=571297]`). Kept as a
    /// fallback for the pathological case where the plaintext
    /// dialect returns the raw form on `notes` and no
    /// `notes_plaintext` twin.
    #[serde(default)]
    notes: Option<String>,
    /// The plaintext-annotated dialect's resolved twin of `notes`,
    /// populated when the request carried
    /// `Accept: application/vnd.discogs.v2.plaintext+json`.
    /// Discogs resolves `[l...]` / `[r=...]` reference codes into
    /// human text server-side and surfaces the resolved form here;
    /// callers prefer this over `notes`.
    #[serde(default)]
    notes_plaintext: Option<String>,
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
    /// Raw profile field — same bracketed-reference caveat as
    /// `ReleaseDetail::notes`; kept as a fallback.
    #[serde(default)]
    profile: Option<String>,
    /// Plaintext-annotated twin of `profile`, populated when the
    /// request carried the plaintext-dialect Accept header.
    /// Callers prefer this over `profile`.
    #[serde(default)]
    profile_plaintext: Option<String>,
    /// Artist photographs. Absent on artists Discogs holds no
    /// imagery for, and absent entirely on responses to tokens
    /// without image permission — both surface as "no image"
    /// rather than an error.
    #[serde(default)]
    images: Option<Vec<ArtistImageEntry>>,
}

#[derive(Debug, Deserialize)]
struct ArtistImageEntry {
    /// `"primary"` for the artist's main photograph,
    /// `"secondary"` for the rest. Discogs does not guarantee a
    /// primary exists.
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// Full-size image URL.
    #[serde(default)]
    uri: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn contains_bracket_reference(text: &str) -> bool {
        // Matches Discogs' bracketed reference codes:
        //   [lNNN]  — label / company id
        //   [r=NNN] — release id
        //   [a=NNN] — artist id
        //   [m=NNN] — master id
        // Any of these leaked into rendered text is the exact
        // "All songs owned by [l333658]" symptom this fixture
        // gates against.
        let text = text.to_ascii_lowercase();
        for prefix in ["[l", "[r=", "[a=", "[m="] {
            if let Some(idx) = text.find(prefix) {
                let after = &text[idx + prefix.len()..];
                if after.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn contains_bracket_reference_detects_all_variants() {
        // Sanity check on the matcher used by the release-notes /
        // profile fixtures below.
        assert!(contains_bracket_reference("All songs owned by [l333658]"));
        assert!(contains_bracket_reference("See also [r=571297]"));
        assert!(contains_bracket_reference("Member of [a=1234]"));
        assert!(contains_bracket_reference("From [m=999]"));
        assert!(!contains_bracket_reference(
            "Older release; catalogue notes clean."
        ));
    }

    #[test]
    fn release_detail_prefers_notes_plaintext_over_raw_notes() {
        // Fixture mirrors the shape Discogs returns under
        // `Accept: application/vnd.discogs.v2.plaintext+json`:
        // BOTH the raw and plaintext fields are present, and the
        // plaintext form has already resolved `[l...]` / `[r=...]`
        // reference codes. The extractor MUST prefer the
        // plaintext form so no bracket markup reaches the
        // operator UI.
        let body = r#"{
            "year": 2015,
            "country": "US",
            "notes": "All songs owned by [l333658]. See also [r=571297].",
            "notes_plaintext": "All songs owned by Blue Coast Records. See also Older.",
            "labels": [{"name": "Blue Coast Records", "catno": "BCR-001"}],
            "formats": [{"name": "SACD, Hybrid"}]
        }"#;
        let detail: ReleaseDetail = serde_json::from_str(body).unwrap();
        let notes = plaintext_or_raw(
            detail.notes_plaintext.clone(),
            detail.notes.clone(),
        )
        .expect("notes present on both fields");
        assert!(
            !contains_bracket_reference(&notes),
            "extracted notes must not carry bracket markup; got {notes:?}"
        );
        assert!(notes.contains("Blue Coast Records"));
        assert!(notes.contains("Older"));
    }

    #[test]
    fn release_detail_falls_back_to_raw_notes_when_plaintext_absent() {
        // Defence-in-depth: if a future Discogs response omits the
        // plaintext twin entirely (older release detail, API
        // regression), the extractor still returns the raw notes
        // rather than dropping the field.
        let body = r#"{
            "year": 2015,
            "notes": "All songs owned by [l333658]."
        }"#;
        let detail: ReleaseDetail = serde_json::from_str(body).unwrap();
        let notes = plaintext_or_raw(detail.notes_plaintext, detail.notes)
            .expect("raw notes present, plaintext absent");
        // The raw form still carries brackets here — the extractor
        // does not strip; it only prefers plaintext when available.
        // The UI-side defence-in-depth strip is a separate follow-on.
        assert!(notes.contains("[l333658]"));
    }

    #[test]
    fn release_detail_treats_empty_plaintext_as_absent() {
        // Discogs occasionally returns an empty-string plaintext
        // twin. The extractor must treat that as absent and fall
        // through to the raw form rather than emitting empty text.
        let body = r#"{
            "notes": "All songs owned by [l333658].",
            "notes_plaintext": "   "
        }"#;
        let detail: ReleaseDetail = serde_json::from_str(body).unwrap();
        let notes = plaintext_or_raw(detail.notes_plaintext, detail.notes)
            .expect("raw fallback when plaintext is whitespace only");
        assert!(notes.contains("[l333658]"));
    }

    #[test]
    fn artist_detail_prefers_profile_plaintext_over_raw_profile() {
        // Same pair-of-fields shape on `/artists/{id}` responses;
        // extractor must prefer the plaintext twin.
        let body = r#"{
            "profile": "Founded [a=1234] in 1962.",
            "profile_plaintext": "Founded The Rolling Stones in 1962."
        }"#;
        let detail: ArtistDetail = serde_json::from_str(body).unwrap();
        let profile = plaintext_or_raw(
            detail.profile_plaintext.clone(),
            detail.profile.clone(),
        )
        .expect("profile present on both fields");
        assert!(
            !contains_bracket_reference(&profile),
            "extracted profile must not carry bracket markup; got {profile:?}"
        );
        assert!(profile.contains("The Rolling Stones"));
    }
}
