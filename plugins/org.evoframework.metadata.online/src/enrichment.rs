// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Bio + album-notes + lyrics verb handlers.
//!
//! All three verbs share the same shape:
//!
//! 1. Validate `v == 1` + required inputs.
//! 2. Consult the [`EnrichmentCache`] for this verb's namespace.
//!    - Positive cache hit → return the cached payload
//!      VERBATIM with the originating source's `provider_id`.
//!      Cache-hit wire shape MUST equal live-hit wire shape.
//!    - Fresh negative → return `not_found` with the
//!      originating source's `provider_id`.
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
    lastfm::LastfmError, DiscogsError, GeniusError, LrclibClient,
};
use serde::{Deserialize, Serialize};

use crate::cascade::{
    self, Attribution, CascadeResponse, CascadeStatus, EntityRef, EntityType,
    PrivacyClass, ProviderCatalogue, ProviderId, SourceEntry,
};
use crate::enrichment_cache::EnrichmentCache;
use crate::locale;

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
    /// LRCLIB (the sole provider on the pre-cascade
    /// `query_lyrics` verb) is disabled by the operator's
    /// per-provider selection or by the `privacy_mode`
    /// preset. Mirrors the cascade verbs' `not_configured`
    /// status so consumers of the composite `track_detail`
    /// endpoint see a consistent shape across every verb
    /// under offline mode.
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

/// Transparent cache-hit response. Returns the stored payload
/// VERBATIM with the originating source's `provider_id` — no
/// `"cache"` label, no `{ cached_from_provider_id, value: {...} }`
/// wrapper. The wire shape of a cache-hit MUST equal the wire
/// shape of the live-fetch it echoes; anything else forces
/// every consumer to branch on `provider_id == "cache"` and
/// decode a different shape, which the UI does not do (and
/// cannot reasonably be expected to).
///
/// Prior shape (defective): `provider_id="cache"`,
/// `payload={cached_from_provider_id, value: {plain_lyrics,...}}`.
/// The lyrics UI reads `payload.plain_lyrics` at the top level;
/// on cache-hit the lookup landed on the wrapper, `plain_lyrics`
/// was undefined, and the pane rendered empty. Because
/// `put_positive` writes an indefinite entry, the first
/// successful fetch warmed the cache and every subsequent play
/// returned the empty-shape hit — the "lyrics vanish on
/// refresh" symptom.
///
/// The stored payload is already the flat live shape (see
/// `query_lyrics` around line 471); no cache migration or wipe
/// is needed. Existing cache entries repair on deploy.
fn from_cache_ok(
    payload: serde_json::Value,
    provider_id: Option<String>,
) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::Ok,
        // Prefer the cached originating provider id; fall back
        // to the verb's default source when the cache row is
        // missing that field for whatever reason.
        provider_id: Some(provider_id.unwrap_or_else(|| "lrclib".to_string())),
        payload: Some(payload),
        detail: None,
    }
}

fn from_cache_negative(detail: Option<String>) -> EnrichmentResponse {
    EnrichmentResponse {
        v: 1,
        status: ResponseStatus::NotFound,
        // Same shape parity: negative cache-hits carry the
        // originating source id (best-effort; falls back to
        // the verb's default) rather than a bare `"cache"`,
        // so consumers can attribute the miss to the actual
        // source they queried.
        provider_id: Some("lrclib".to_string()),
        payload: None,
        detail,
    }
}

/// Build the cache key for an entity-bio response. Two
/// namespaces:
///
/// - **`mbid/<mbid>`** when the cascade resolved (or the caller
///   supplied) a canonical MusicBrainz MBID for the entity. This
///   is the correct case for every track whose reconciliation
///   completes.
/// - **`name/<normalised-name>`** when no MBID is available. This
///   namespace is deliberately at risk of same-name collisions
///   for artist-type entities (Passenger the musician vs the
///   transport noun) — the bare-name Wikipedia fallback is
///   refused for artist-type entities elsewhere in this cascade
///   so name-only positive entries are only ever written by
///   non-artist entity types.
///
/// The namespaces are segregated: a poisoned `name/passenger`
/// entry cannot leak into a subsequent `mbid/<mbid>` lookup, and
/// vice versa. Two callers with the same name but different
/// resolved MBIDs live in different cache entries.
///
/// Locale-less back-compat wrapper used by the partitioning
/// tests below (they assert namespace segregation invariants
/// that are orthogonal to the locale dimension). Prod callers
/// use [`bio_cache_key_with_locale`] directly.
#[cfg(test)]
fn bio_cache_key(
    entity: &EntityRef,
    resolved_mbid: Option<&str>,
    provider: ProviderId,
) -> String {
    bio_cache_key_with_locale(entity, resolved_mbid, provider, "en")
}

/// Locale-scoped variant: the cache key incorporates the operator
/// locale so entries fetched under one language never shadow a
/// request under another. Without locale in the key, the first
/// caller's language would freeze into the cache and every
/// subsequent operator would see prose in the wrong language
/// regardless of their locale header — an integrity failure the
/// bare `bio_cache_key` above cannot see.
fn bio_cache_key_with_locale(
    entity: &EntityRef,
    resolved_mbid: Option<&str>,
    provider: ProviderId,
    locale: &str,
) -> String {
    match resolved_mbid {
        Some(mbid) => EnrichmentCache::key_for(&[
            "entity_bio",
            entity.entity_type.as_str(),
            "mbid",
            mbid,
            provider.as_str(),
            "loc",
            locale,
        ]),
        None => EnrichmentCache::key_for(&[
            "entity_bio",
            entity.entity_type.as_str(),
            "name",
            &normalise(&entity.name),
            provider.as_str(),
            "loc",
            locale,
        ]),
    }
}

/// Normalise an album title for provider queries by stripping
/// edition suffixes and homogenising punctuation. Applied
/// before the Wikipedia / Last.fm / TheAudioDB album lookups so
/// operator-tagged variants match canonical article titles.
///
/// Handles these cases surfaced by real library tracks:
///
/// - `"Closer - The Best Of Sarah McLachlan (Deluxe Version)"`
///   → `"Closer: The Best of Sarah McLachlan"` — strip the
///   parenthetical edition suffix, replace ` - ` with `: ` so
///   MusicBrainz / Wikipedia article-title conventions match.
///
/// Suffix stripping is case-insensitive and covers the
/// operator-facing edition vocabulary iTunes / Apple Music /
/// Deezer / Tidal emit into tag metadata: `(Deluxe Version)`,
/// `(Deluxe Edition)`, `(Deluxe)`, `(Remastered)`, `(Remaster)`,
/// `(YYYY Remaster)`, `(Expanded Edition)`, `(Special Edition)`,
/// `(Anniversary Edition)`, `(Bonus Track Version)`. Only
/// trailing suffixes are removed — a real album title
/// containing the word "Deluxe" mid-string stays intact.
///
/// The output is not lower-cased — providers do case-sensitive
/// matching on articles like "The" vs "the". Cache keying keeps
/// its own separate `normalise` for lower-case + whitespace
/// folding.
pub(crate) fn normalise_album_query(title: &str) -> String {
    let mut cleaned = title.trim().to_string();
    // Strip trailing parenthetical edition suffixes. Loop so
    // "(Deluxe Version) (Remastered)" gets both stripped, though
    // real tags rarely stack.
    loop {
        let lower = cleaned.to_ascii_lowercase();
        let matched = TRAILING_EDITION_SUFFIXES.iter().find(|s| {
            lower.ends_with(&format!("({s})"))
                || lower.ends_with(&format!("({s} version)"))
                || lower.ends_with(&format!("({s} edition)"))
        });
        match matched {
            Some(_) => {
                if let Some(paren_start) = cleaned.rfind('(') {
                    cleaned = cleaned[..paren_start].trim_end().to_string();
                } else {
                    break;
                }
            }
            None => break,
        }
    }
    // Also strip trailing " - Deluxe Version" / " - Remastered"
    // patterns iTunes emits when the edition isn't in parens.
    let dash_lower = cleaned.to_ascii_lowercase();
    for suffix in TRAILING_EDITION_SUFFIXES {
        let with_dash = format!(" - {suffix}");
        if dash_lower.ends_with(&with_dash) {
            cleaned.truncate(cleaned.len() - with_dash.len());
            break;
        }
        let with_dash_edition = format!(" - {suffix} edition");
        if dash_lower.ends_with(&with_dash_edition) {
            cleaned.truncate(cleaned.len() - with_dash_edition.len());
            break;
        }
        let with_dash_version = format!(" - {suffix} version");
        if dash_lower.ends_with(&with_dash_version) {
            cleaned.truncate(cleaned.len() - with_dash_version.len());
            break;
        }
    }
    // Normalise " - " to ": " — Wikipedia and MusicBrainz
    // article titles use the colon-space form for subtitle
    // separators ("Closer: The Best of Sarah McLachlan"), while
    // iTunes / Apple Music tag titles emit " - ".
    cleaned = cleaned.replace(" - ", ": ");
    // Apply Wikipedia title-case convention: lowercase common
    // connective words (of, the, in, on, at, to, for, and, or,
    // a, an, is, but, by, with, from) unless they appear at
    // the start of the string. Wikipedia's REST summary API is
    // case-sensitive on titles: the tag "The Best Of Sarah
    // McLachlan" (capital Of) returns 404, but the canonical
    // Wikipedia article title uses "the Best of Sarah McLachlan"
    // (lowercase of). Operator tags in the wild use either
    // form; this normalisation converges on Wikipedia's.
    cleaned = lowercase_connective_words(&cleaned);
    cleaned.trim().to_string()
}

/// Lowercase common connective words that Wikipedia's title-
/// casing convention keeps lowercase mid-string. The word at
/// position 0 AND the first word of any subtitle (word
/// following a token that ended with `:`) stay title-cased —
/// Wikipedia capitalises the first word of both the main title
/// and every subtitle regardless of what it is
/// ("Closer: The Best of Sarah McLachlan" — `The` capitalised
/// as subtitle first-word, `of` lowercased as mid-subtitle
/// connective).
fn lowercase_connective_words(title: &str) -> String {
    let words: Vec<&str> = title.split(' ').collect();
    let mut out = String::with_capacity(title.len());
    let mut prev_ended_colon = false;
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        // A word is "subtitle-start" when it follows a token
        // that ended with `:` (main-title / subtitle boundary).
        let is_subtitle_start = prev_ended_colon;
        let w_lower = w.to_ascii_lowercase();
        let lowercase_it = i > 0
            && !is_subtitle_start
            && WIKIPEDIA_CONNECTIVE_WORDS.contains(&w_lower.as_str());
        if lowercase_it {
            out.push_str(&w_lower);
        } else {
            out.push_str(w);
        }
        prev_ended_colon = w.ends_with(':');
    }
    out
}

/// Words Wikipedia's title-casing rule keeps lowercase when they
/// appear mid-title. Not exhaustive — covers the words most
/// likely to appear in album titles that operator tags may have
/// title-cased. Wikipedia's own MoS lists more but these are the
/// ones the real-track probe has hit.
const WIKIPEDIA_CONNECTIVE_WORDS: &[&str] = &[
    "a", "an", "and", "as", "at", "but", "by", "for", "from", "in", "is", "of",
    "on", "or", "the", "to", "with",
];

/// Trailing edition suffixes stripped by
/// [`normalise_album_query`]. Case-insensitive matching happens
/// against the lower-cased title. Order matters only in the
/// dash-form loop above (first match wins); the parenthetical
/// loop reprocesses until stable so order there does not.
const TRAILING_EDITION_SUFFIXES: &[&str] = &[
    "deluxe",
    "deluxe version",
    "deluxe edition",
    "expanded edition",
    "expanded version",
    "special edition",
    "anniversary edition",
    "bonus track version",
    "bonus track edition",
    "bonus tracks version",
    "remastered",
    "remaster",
    "remastered version",
];

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
    /// Operator UI locale (BCP47 short — `"en"`, `"de"`, ...).
    /// Absent =
    /// `"en"` (back-compat with pre-Rule-G callers).
    #[serde(default)]
    #[allow(dead_code)]
    locale: Option<String>,
}

pub(crate) async fn query_lyrics(
    payload: &[u8],
    lrclib: &LrclibClient,
    cache: &EnrichmentCache,
    provider_config: &crate::cascade::ProviderConfig,
) -> Result<EnrichmentResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    // LRCLIB is a network provider — must respect the operator's
    // `privacy_mode = "offline"` preset (and the per-provider
    // `[providers.lrclib] enabled = false` toggle). Without this
    // gate the lyrics path bypasses the privacy guarantee that
    // the offline mode is meant to enforce: the offline
    // attestation script explicitly asserts this.
    if !provider_config
        .is_effectively_enabled(crate::cascade::ProviderId::Lrclib)
    {
        return Ok(EnrichmentResponse {
            v: 1,
            status: ResponseStatus::NotConfigured,
            provider_id: None,
            payload: None,
            detail: Some(
                "LRCLIB is disabled by the operator's privacy \
                 selection; enable it under Settings → Metadata → \
                 Sources"
                    .to_string(),
            ),
        });
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
    // duration_seconds is folded into the cache key so a
    // negative miss keyed on a wrong / imprecise duration
    // doesn't shadow a subsequent lookup with the corrected
    // value for 24h (positive misses are indefinite; negative
    // misses default 24h). LRCLIB's `get_lyrics` treats
    // duration as a match-refinement parameter, so
    // `(artist, track, album, ±1s duration)` may hit vs miss
    // differently — the cache key must reflect that.
    // Rounded to integer seconds because sub-second precision
    // is spurious for lyrics matching (LRCLIB itself matches
    // within a small tolerance).
    let duration_key = req
        .duration_seconds
        .map(|d| format!("{}", d.round() as i64))
        .unwrap_or_default();
    let key = EnrichmentCache::key_for(&[
        "lyrics",
        &normalise(&artist),
        &normalise(&track),
        album_norm.as_deref().unwrap_or(""),
        &duration_key,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_collapses_and_lowers() {
        assert_eq!(normalise("  Radiohead  "), "radiohead");
        assert_eq!(normalise("OK  Computer"), "ok computer");
        assert_eq!(normalise(""), "");
    }

    #[test]
    fn normalise_album_query_strips_parenthetical_editions() {
        assert_eq!(
            normalise_album_query("Songs of Love (Deluxe Version)"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love (Deluxe Edition)"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love (Deluxe)"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love (Remastered)"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love (Special Edition)"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love (Anniversary Edition)"),
            "Songs of Love"
        );
        // Case-insensitive.
        assert_eq!(
            normalise_album_query("Songs of Love (DELUXE VERSION)"),
            "Songs of Love"
        );
    }

    #[test]
    fn normalise_album_query_strips_dash_editions() {
        assert_eq!(
            normalise_album_query("Songs of Love - Deluxe Version"),
            "Songs of Love"
        );
        assert_eq!(
            normalise_album_query("Songs of Love - Remastered"),
            "Songs of Love"
        );
    }

    #[test]
    fn normalise_album_query_folds_dash_subtitle_to_colon_and_wp_case() {
        // Wikipedia + MB use colon-space for subtitle separators;
        // iTunes tags emit " - ". The Sarah McLachlan case that
        // motivated this: "Closer - The Best Of Sarah McLachlan
        // (Deluxe Version)" → "Closer: The Best of Sarah McLachlan"
        // (edition suffix stripped, dash-subtitle normalised, and
        // Wikipedia title-case convention applied — lowercase
        // "of" mid-title because Wikipedia's REST summary API is
        // case-sensitive and the article is at
        // "Closer:_The_Best_of_Sarah_McLachlan").
        assert_eq!(
            normalise_album_query(
                "Closer - The Best Of Sarah McLachlan (Deluxe Version)"
            ),
            "Closer: The Best of Sarah McLachlan"
        );
    }

    #[test]
    fn normalise_album_query_lowercases_connective_words() {
        assert_eq!(
            normalise_album_query("The Dark Side Of The Moon"),
            "The Dark Side of the Moon"
        );
        assert_eq!(normalise_album_query("Band On The Run"), "Band on the Run");
        // First word stays capitalized regardless of what it is.
        assert_eq!(
            normalise_album_query("The Best of Both Worlds"),
            "The Best of Both Worlds"
        );
    }

    #[test]
    fn normalise_album_query_preserves_mid_string_words() {
        // A real album whose title contains "Deluxe" mid-string
        // must not lose it — only trailing edition suffixes are
        // stripped.
        assert_eq!(
            normalise_album_query("Deluxe Recordings"),
            "Deluxe Recordings"
        );
    }

    #[test]
    fn normalise_album_query_no_suffix_is_identity() {
        assert_eq!(normalise_album_query("OK Computer"), "OK Computer");
        assert_eq!(
            normalise_album_query("The Dark Side of the Moon"),
            "The Dark Side of the Moon"
        );
    }

    #[test]
    fn lastfm_disambiguation_stubs_are_detected() {
        // Real Last.fm response for Passenger's singer MBID —
        // routes to the disambiguation page bio.
        let passenger = "There are at least six artists and bands who \
                         have performed with the name \"Passenger\". \
                         The following order is loosely based on the \
                         last.fm statistics.\n1. A Brighton (UK) based \
                         alternative folk band";
        assert!(is_lastfm_disambiguation_stub(passenger));
        // Case-insensitive.
        assert!(is_lastfm_disambiguation_stub(
            "THERE ARE AT LEAST five bands calling themselves Bush."
        ));
        // Variant phrasing Last.fm uses.
        assert!(is_lastfm_disambiguation_stub(
            "There are several artists with this name."
        ));
    }

    #[test]
    fn lastfm_real_bios_not_flagged_as_disambiguation() {
        // A real Passenger-the-singer bio wouldn't contain the
        // stub phrases.
        let real_bio = "Michael David Rosenberg, better known by his \
                        stage name Passenger, is an English indie folk \
                        singer, songwriter and musician.";
        assert!(!is_lastfm_disambiguation_stub(real_bio));
        // A short bio.
        assert!(!is_lastfm_disambiguation_stub(
            "Sarah McLachlan is a \
                                                Canadian singer."
        ));
        // Empty prose.
        assert!(!is_lastfm_disambiguation_stub(""));
    }

    #[test]
    fn bio_cache_key_partitions_mbid_from_name_namespace() {
        // Two entities with the same normalised name but
        // different MBIDs MUST live in different cache entries
        // — the common-noun-trap fix depends on this.
        let ent_passenger_musician = EntityRef {
            entity_type: EntityType::Artist,
            name: "Passenger".to_string(),
            mbid: Some("186e216a-2f8a-41a1-935f-8e30c018a8fe".to_string()),
        };
        let ent_passenger_disambig_only = EntityRef {
            entity_type: EntityType::Artist,
            name: "Passenger".to_string(),
            mbid: None,
        };
        let k_musician = bio_cache_key(
            &ent_passenger_musician,
            ent_passenger_musician.mbid.as_deref(),
            ProviderId::Wikipedia,
        );
        let k_disambig = bio_cache_key(
            &ent_passenger_disambig_only,
            None,
            ProviderId::Wikipedia,
        );
        assert_ne!(
            k_musician, k_disambig,
            "MBID-resolved and name-only cache keys must \
                    be distinct even for the same normalised name"
        );
    }

    #[test]
    fn bio_cache_key_same_mbid_different_names_collide() {
        // If two callers arrive at the same MBID via different
        // display-name spellings, they SHOULD hit the same cache
        // entry. The MBID is the canonical identity.
        let mbid = "186e216a-2f8a-41a1-935f-8e30c018a8fe";
        let a = EntityRef {
            entity_type: EntityType::Artist,
            name: "Passenger".to_string(),
            mbid: Some(mbid.to_string()),
        };
        let b = EntityRef {
            entity_type: EntityType::Artist,
            name: "Mike Rosenberg".to_string(),
            mbid: Some(mbid.to_string()),
        };
        let ka = bio_cache_key(&a, Some(mbid), ProviderId::Wikipedia);
        let kb = bio_cache_key(&b, Some(mbid), ProviderId::Wikipedia);
        assert_eq!(ka, kb, "MBID-keyed cache entries must ignore display name");
    }

    #[test]
    fn bio_cache_key_different_mbids_never_collide() {
        // Passenger the musician vs some other artist that also
        // reconciles to a different MBID: never share cache.
        let a = EntityRef {
            entity_type: EntityType::Artist,
            name: "Passenger".to_string(),
            mbid: Some("186e216a-2f8a-41a1-935f-8e30c018a8fe".to_string()),
        };
        let b = EntityRef {
            entity_type: EntityType::Artist,
            name: "Passenger".to_string(),
            mbid: Some("00000000-0000-0000-0000-000000000000".to_string()),
        };
        let ka = bio_cache_key(&a, a.mbid.as_deref(), ProviderId::Wikipedia);
        let kb = bio_cache_key(&b, b.mbid.as_deref(), ProviderId::Wikipedia);
        assert_ne!(ka, kb, "different MBIDs must never share a cache key");
    }

    #[test]
    fn bio_cache_key_provider_scoped() {
        // Wikipedia and Wikidata payloads for the same entity
        // live in their own cache entries so a disabled provider
        // never returns another provider's cached content.
        let e = EntityRef {
            entity_type: EntityType::Artist,
            name: "Radiohead".to_string(),
            mbid: Some("a74b1b7f-71a5-4011-9441-d0b5e4122711".to_string()),
        };
        let k_wp = bio_cache_key(&e, e.mbid.as_deref(), ProviderId::Wikipedia);
        let k_wd = bio_cache_key(&e, e.mbid.as_deref(), ProviderId::Wikidata);
        assert_ne!(k_wp, k_wd);
    }
}

// Discogs error surfacing helper — shared with the
// release-credits cascade below.

fn discogs_error_message(e: &DiscogsError) -> String {
    match e {
        DiscogsError::Http(err) => format!("discogs http: {err}"),
        DiscogsError::Status { status, body } => {
            format!("discogs status {status}: {body}")
        }
        DiscogsError::Decode(m) => format!("discogs decode: {m}"),
    }
}

// Genius error surfacing helper — shared with the
// track-annotation cascade below.

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
    /// Operator UI locale (BCP47 short — `"en"`, `"de"`, ...).
    /// Absent =
    /// `"en"` (back-compat with pre-Rule-G callers).
    #[serde(default)]
    pub(crate) locale: Option<String>,
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

    // Aggregate cascade: every enabled+available provider
    // fetches in parallel; every non-empty result is folded into
    // `sources: Vec<SourceEntry>`, sorted by operator priority.
    // The top-level payload is the field-level first-non-empty
    // merge across sources — a keyed provider's content is never
    // shadowed by a peer that hit first.
    //
    // MusicBrainz stays SEQUENTIAL because its output routes URLs
    // for Wikipedia + Wikidata; those two only dispatch after MB
    // resolves. MB itself is not a bio content source in this
    // verb — it is identity resolution.
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
    let want_theaudiodb = catalogue
        .config
        .is_effectively_enabled(ProviderId::TheAudioDb)
        && catalogue.theaudiodb.is_some();

    if !(want_mb || want_wp || want_wd || want_lastfm || want_theaudiodb) {
        return Ok(CascadeResponse::not_configured(
            "every bio provider is disabled or unavailable on this device; \
             enable at least one under Settings → Metadata → Sources",
        ));
    }

    // Resolve entity URLs via MusicBrainz artist lookup when
    // enabled. This yields Wikipedia + Wikidata URLs that the
    // downstream providers consume directly — no fuzzy search.
    //
    // resolved_mbid tracks the canonical MBID we ended up with —
    // either caller-supplied or MB-resolved. It's used by the
    // Wikipedia + Wikidata cache-key namespacing below to
    // partition MBID-resolved lookups from name-only ones. A
    // name-only cache entry cannot poison an MBID-keyed lookup
    // and vice versa (the fix for the Passenger/common-noun
    // trap: same name string across two different real artists
    // must not collide in the cache).
    let mut resolved_mbid: Option<String> = entity
        .mbid
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut wikipedia_url: Option<String> = None;
    let mut wikidata_url: Option<String> = None;
    // Canonical entity name from MB — the alias-resolved
    // authoritative form (e.g. tag "Fiona Joy" → MB canonical
    // "Fiona Joy Hawkins"). Downstream helpers prefer this
    // over the tag literal for keyless searches; without it,
    // aliases surface as "not_found" from every source.
    let mut canonical_name: Option<String> = None;
    if want_mb {
        if let Some(mb) = catalogue.musicbrainz.as_ref() {
            if resolved_mbid.is_none() {
                match mb.search_artist(&entity.name).await {
                    Ok(Some(hit)) if hit.confidence_percent >= 85 => {
                        resolved_mbid = Some(hit.artist_mbid);
                        canonical_name = Some(hit.canonical_name);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            entity = %entity.name,
                            error = %e,
                            "MB artist search transient; skipping"
                        );
                    }
                }
            }
            if let Some(mbid) = resolved_mbid.as_ref() {
                match mb.lookup_artist(mbid).await {
                    Ok(al) => {
                        wikipedia_url = al.wikipedia_url;
                        wikidata_url = al.wikidata_url;
                        // Lookup returns the canonical name too;
                        // prefer it over the search result when
                        // caller supplied MBID directly (search
                        // may have been skipped).
                        if canonical_name.is_none() {
                            canonical_name = Some(al.canonical_name);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            mbid = mbid.as_str(),
                            error = %e,
                            "MB artist lookup transient; skipping"
                        );
                    }
                }
            }
        }
    }
    // Effective name for downstream searches — MB canonical
    // when available, tag literal otherwise. `Fiona Joy` (tag)
    // → `Fiona Joy Hawkins` (canonical) → Wikipedia bare-name
    // search hits the correct page instead of returning empty.
    let effective_name =
        canonical_name.as_deref().unwrap_or(entity.name.as_str());

    // Operator locale (the locale-aware fallback). Absent → `"en"` (default).
    let operator_locale = locale::normalise(req.locale.as_deref());

    // Parallel content-fetch across every enabled provider.
    // Each helper returns `Option<SourceEntry>`; `None` on
    // disabled / unavailable / structural miss / cache negative /
    // transient error. `tokio::join!` polls all four
    // concurrently; wall-clock is the slowest single provider.
    let (wp_src, wd_src, lastfm_src, tadb_src) = tokio::join!(
        fetch_wikipedia_bio(
            &entity,
            effective_name,
            resolved_mbid.as_deref(),
            wikipedia_url.as_deref(),
            wikidata_url.as_deref(),
            catalogue,
            cache,
            want_wp,
            want_wd,
            &operator_locale,
        ),
        fetch_wikidata_bio(
            &entity,
            resolved_mbid.as_deref(),
            wikidata_url.as_deref(),
            catalogue,
            cache,
            want_wd,
            &operator_locale,
        ),
        fetch_lastfm_bio(
            &entity,
            effective_name,
            catalogue,
            cache,
            want_lastfm
        ),
        fetch_theaudiodb_bio(
            &entity,
            effective_name,
            resolved_mbid.as_deref(),
            catalogue,
            cache,
            want_theaudiodb,
            &operator_locale,
        ),
    );

    let mut sources: Vec<SourceEntry> = [wp_src, wd_src, lastfm_src, tadb_src]
        .into_iter()
        .flatten()
        .collect();
    cascade::sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(CascadeResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            detail: Some(format!(
                "no bio found for {}={} across enabled providers",
                entity.entity_type.as_str(),
                entity.name
            )),
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        });
    }
    Ok(CascadeResponse::from_sources(sources, None))
}

// ---------------------------------------------------------------
// Per-provider bio-fetch helpers — one `Option<SourceEntry>` per
// provider, folded into the parallel-dispatch `tokio::join!` in
// `query_entity_bio` above. Each helper OWNS its own cache
// check, network fetch, cache-write, and payload construction —
// so a per-provider enable / priority change lands the moment
// the online_provider_config store publishes it, without any
// coordination on the orchestrator's side.
// ---------------------------------------------------------------

// 8 args reflect the parallel-dispatch shape: each helper is a
// self-contained leaf that runs inside `tokio::join!` and must
// carry everything it needs (identity resolution outputs +
// per-provider enable flags + the shared catalogue / cache) as
// direct arguments. Wrapping in a struct would move the args
// without reducing them.
#[allow(clippy::too_many_arguments)]
async fn fetch_wikipedia_bio(
    entity: &EntityRef,
    effective_name: &str,
    resolved_mbid: Option<&str>,
    wikipedia_url: Option<&str>,
    wikidata_url: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    wp_enabled: bool,
    wd_enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !wp_enabled {
        return None;
    }
    let wp = catalogue.wikipedia.as_ref()?;
    // Cache key includes the operator locale so a `de` operator
    // and an `en` operator get distinct entries — otherwise the
    // first fetch (whatever language it happened to land in)
    // would shadow every subsequent request regardless of the
    // caller's locale (a locale-integrity failure).
    let key = bio_cache_key_with_locale(
        entity,
        resolved_mbid,
        ProviderId::Wikipedia,
        operator_locale,
    );
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikipedia_source_from_cached_payload(p));
            }
        }
    }
    // Locale-aware candidate ordering. Every candidate is
    // `(language_hint, fetch_future)`; iterate in order, use the
    // first that returns a non-empty summary. The reported
    // language on the SourceEntry is whichever candidate landed.
    let mb_url_lang = wikipedia_url
        .and_then(|url| {
            evo_online_providers::wikipedia::parse_wikipedia_url(url)
        })
        .map(|(lang, _)| lang);
    // Candidate 1: operator-locale sitelink via Wikidata.
    //   → operator locale first, correct entity via MBID.
    // Candidate 2: MB URL when it happens to be operator locale.
    //   → same, without needing the Wikidata hop.
    // Candidate 3: enwiki sitelink via Wikidata (skipped if
    //   operator locale is already English — candidate 1 covered it).
    //   → English fallback, correct entity via MBID.
    // Candidate 4: MB URL in any other language.
    //   → whatever language MB happens to have; still MBID-first.
    // Candidate 5: bare-name search in operator locale.
    //   → last-resort, gated (artist-type common-noun trap).
    // Candidate 6: bare-name search in English (skipped if
    //   operator locale is already English).
    let mut hit: Option<evo_online_providers::wikipedia::WikipediaSummaryHit> =
        None;

    // Candidate 1: Wikidata `{locale}wiki` sitelink.
    let mut wd_entity_cache: Option<
        evo_online_providers::wikidata::WikidataEntityHit,
    > = None;
    if hit.is_none() {
        if let (Some(wd), Some(wd_url)) =
            (catalogue.wikidata.as_ref(), wikidata_url)
        {
            if wd_enabled {
                match wd.get_entity_from_url(wd_url).await {
                    Ok(Some(wd_entity)) => {
                        wd_entity_cache = Some(wd_entity);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "wikidata",
                            entity = %entity.name,
                            error = %e,
                            "Wikidata sitelink lookup transient; \
                             trying next fallback"
                        );
                    }
                }
            }
        }
    }
    if hit.is_none() {
        if let Some(wd_entity) = wd_entity_cache.as_ref() {
            let locale_site = format!("{operator_locale}wiki");
            if let Some(title) = wd_entity.sitelinks.get(&locale_site) {
                match wp.get_summary(title, operator_locale).await {
                    Ok(Some(s)) => hit = Some(s),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "wikipedia",
                            entity = %entity.name,
                            title = %title,
                            language = operator_locale,
                            error = %e,
                            "operator-locale Wikipedia summary transient; \
                             trying next fallback"
                        );
                    }
                }
            }
        }
    }
    // Candidate 2: MB URL parses to operator-locale edition.
    if hit.is_none()
        && wikipedia_url.is_some()
        && mb_url_lang.as_deref() == Some(operator_locale)
    {
        if let Some(url) = wikipedia_url {
            match wp.get_summary_from_url(url).await {
                Ok(Some(s)) => hit = Some(s),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        entity = %entity.name,
                        url = url,
                        error = %e,
                        "Wikipedia summary from MB-routed URL (locale-match) \
                         transient; trying next fallback"
                    );
                }
            }
        }
    }
    // Candidate 3: Wikidata `enwiki` sitelink (skipped if
    // operator_locale is en — candidate 1 covered it).
    if hit.is_none() && operator_locale != "en" {
        if let Some(wd_entity) = wd_entity_cache.as_ref() {
            if let Some(title) = wd_entity.sitelinks.get("enwiki") {
                match wp.get_summary_en(title).await {
                    Ok(Some(s)) => hit = Some(s),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "wikipedia",
                            entity = %entity.name,
                            enwiki_title = %title,
                            error = %e,
                            "Wikipedia summary via Wikidata enwiki sitelink \
                             transient; trying next fallback"
                        );
                    }
                }
            }
        }
    }
    // Candidate 4: MB URL in any other language.
    if hit.is_none()
        && wikipedia_url.is_some()
        && mb_url_lang.as_deref() != Some(operator_locale)
    {
        if let Some(url) = wikipedia_url {
            match wp.get_summary_from_url(url).await {
                Ok(Some(s)) => hit = Some(s),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        entity = %entity.name,
                        url = url,
                        error = %e,
                        "Wikipedia summary from MB-routed URL transient; \
                         trying next fallback"
                    );
                }
            }
        }
    }
    // Candidates 5-6: bare-name search. Same common-noun-trap
    // gating as before, applied per language attempt.
    if hit.is_none() {
        let mb_gave_useful_canonical =
            resolved_mbid.is_some() && effective_name != entity.name.as_str();
        let should_search = mb_gave_useful_canonical
            || !matches!(entity.entity_type, EntityType::Artist);
        if should_search {
            // Candidate 5: operator-locale bare-name.
            match wp.get_summary(effective_name, operator_locale).await {
                Ok(Some(s)) => hit = Some(s),
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        entity = %entity.name,
                        effective_name,
                        language = operator_locale,
                        error = %e,
                        "operator-locale bare-name Wikipedia summary \
                         transient; trying next fallback"
                    );
                }
            }
            // Candidate 6: English bare-name (skipped if operator_locale is en).
            if hit.is_none() && operator_locale != "en" {
                match wp.get_summary_en(effective_name).await {
                    Ok(Some(s)) => hit = Some(s),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "wikipedia",
                            entity = %entity.name,
                            effective_name,
                            error = %e,
                            "English bare-name Wikipedia summary \
                             transient; skipping"
                        );
                    }
                }
            }
        } else {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                entity = %entity.name,
                "artist-type entity with no MB-routed URL, no Wikidata \
                 sitelink, and no MB canonical-alias resolution; \
                 refusing bare-name Wikipedia fallback (common-noun \
                 disambiguation trap)"
            );
        }
    }
    let summary = hit?;
    let served_language = summary.language.clone();
    let payload = serde_json::json!({
        "title": summary.title,
        "summary": summary.extract,
        "language": served_language,
        "source_url": summary.page_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
    Some(SourceEntry {
        provider_id: ProviderId::Wikipedia.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: Some(served_language),
        payload,
        attribution: Attribution {
            source_name: "Wikipedia".into(),
            source_url: Some(summary.page_url),
            license: "CC BY-SA".into(),
        },
    })
}

async fn fetch_wikidata_bio(
    entity: &EntityRef,
    resolved_mbid: Option<&str>,
    wikidata_url: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let wd = catalogue.wikidata.as_ref()?;
    // Cache key includes locale so a `de` request never
    // returns the cached `en` description under the wrong label.
    let key = bio_cache_key_with_locale(
        entity,
        resolved_mbid,
        ProviderId::Wikidata,
        operator_locale,
    );
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikidata_source_from_cached_payload(p));
            }
        }
    }
    let wd_url = wikidata_url?;
    let entity_hit = match wd.get_entity_from_url(wd_url).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "wikidata",
                entity = %entity.name,
                error = %e,
                "Wikidata facts transient; skipping"
            );
            return None;
        }
    };
    // Pick the operator-locale label + description with
    // the operator → English → any fallback chain. `label_for`
    // and `description_for` return `(text, lang_actually_served)`.
    let label_pick = entity_hit.label_for(operator_locale);
    let description_pick = entity_hit.description_for(operator_locale);
    // Report the label/description language on the SourceEntry —
    // when they diverge, prefer the description's language
    // because that's the prose the operator actually reads.
    let served_language = description_pick
        .as_ref()
        .map(|(_, l)| l.clone())
        .or_else(|| label_pick.as_ref().map(|(_, l)| l.clone()));
    let payload = serde_json::json!({
        "label": label_pick.as_ref().map(|(t, _)| t),
        "description": description_pick.as_ref().map(|(t, _)| t),
        "language": served_language,
        "date_of_birth": entity_hit.date_of_birth,
        "date_of_death": entity_hit.date_of_death,
        "inception": entity_hit.inception,
        "dissolution": entity_hit.dissolution,
        "source_url": entity_hit.entity_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikidata");
    Some(SourceEntry {
        provider_id: ProviderId::Wikidata.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: served_language,
        payload,
        attribution: Attribution {
            source_name: "Wikidata".into(),
            source_url: Some(entity_hit.entity_url),
            license: "CC0".into(),
        },
    })
}

async fn fetch_lastfm_bio(
    entity: &EntityRef,
    effective_name: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    // Last.fm has poor classical (composer / work / performer)
    // coverage; only dispatch for artist-type entities.
    if !matches!(entity.entity_type, EntityType::Artist) {
        return None;
    }
    let lastfm = catalogue.lastfm.as_ref()?;
    // Cache key uses `effective_name` (MB canonical when
    // available) so alias-resolved tags and canonical hits
    // share one entry — the tag "Fiona Joy" and canonical
    // "Fiona Joy Hawkins" both key on the same canonical form
    // once MB has resolved either.
    let key = EnrichmentCache::key_for(&[
        "entity_bio",
        entity.entity_type.as_str(),
        &normalise(effective_name),
        ProviderId::Lastfm.as_str(),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(lastfm_source_from_cached_payload(p));
            }
        }
    }
    match lastfm
        .get_artist_bio(effective_name, entity.mbid.as_deref())
        .await
    {
        Ok(Some(h)) => {
            // Content-correctness: Last.fm serves user-editable
            // "wiki" bios and its own disambiguation stubs. Even
            // an MBID-only query can land the operator on a
            // disambiguation page when Last.fm's internal MBID
            // mapping routes to their disambig page (a data-
            // quality issue on Last.fm's side, not something the
            // API surface exposes). Passenger's singer MBID
            // (186e216a-...) maps to Last.fm's "Passenger"
            // disambiguation page: "There are at least six
            // artists and bands who have performed with the name
            // 'Passenger'". Rendering this under Last.fm's
            // attribution surfaces content-shaped garbage to the
            // operator. Drop the entry as a clean miss so
            // `sources[]` never carries a disambiguation stub.
            let combined_prose = format!(
                "{}\n{}",
                h.summary.as_deref().unwrap_or(""),
                h.content.as_deref().unwrap_or("")
            );
            if is_lastfm_disambiguation_stub(&combined_prose) {
                tracing::info!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "lastfm",
                    entity = %entity.name,
                    "Last.fm returned a disambiguation-stub bio for \
                     this MBID (Last.fm-side MBID mapping issue); \
                     suppressing the entry so the operator never \
                     sees disambiguation content attributed to \
                     Last.fm"
                );
                return None;
            }
            let payload = serde_json::json!({
                "summary": h.summary,
                "content": h.content,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "lastfm");
            Some(SourceEntry {
                provider_id: ProviderId::Lastfm.as_str().to_string(),
                privacy_class: PrivacyClass::IdentityBearing
                    .as_str()
                    .to_string(),
                language: None,
                payload,
                attribution: Attribution {
                    source_name: "Last.fm".into(),
                    source_url: h.source_url.clone(),
                    license: "Last.fm terms of use".into(),
                },
            })
        }
        Ok(None) => None,
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
            None
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "lastfm",
                entity = %entity.name,
                error = %e,
                "Last.fm transient; skipping"
            );
            None
        }
    }
}

/// Detect Last.fm's stable disambiguation-stub bio patterns.
/// Last.fm's user-editable wiki adopts a formulaic shape when
/// multiple artists share a name; the stub is content-shaped
/// (paragraphs, prose) but names no single artist. Rendering it
/// under a specific-artist attribution mislabels the source.
///
/// The pattern is stable and case-insensitive; both English
/// forms observed on real .24 rig data ("There are at least N
/// artists" / "The following order is loosely based on Last.fm
/// statistics") flag the entry as disambiguation.
fn is_lastfm_disambiguation_stub(prose: &str) -> bool {
    let lower = prose.to_ascii_lowercase();
    lower.contains("there are at least")
        || lower.contains("there are several artists")
        || lower.contains("there are multiple artists")
        || lower.contains("the following order is loosely based")
}

async fn fetch_theaudiodb_bio(
    entity: &EntityRef,
    effective_name: &str,
    resolved_mbid: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    // TheAudioDB's bio surface is the strBiography{lang} field
    // family on the artist record — an artist-type surface.
    // Composer / work / performer / conductor / ensemble routes
    // through MB + Wikipedia + Wikidata; TheAudioDB adds nothing
    // there.
    if !matches!(entity.entity_type, EntityType::Artist) {
        return None;
    }
    let tadb = catalogue.theaudiodb.as_ref()?;
    // Locale-aware: locale in the cache key so a `de` request and an
    // `en` request never poison each other. Distinct locales
    // land in distinct cached payloads.
    let key = EnrichmentCache::key_for(&[
        "entity_bio",
        entity.entity_type.as_str(),
        &normalise(effective_name),
        ProviderId::TheAudioDb.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(theaudiodb_source_from_cached_payload(p));
            }
        }
    }
    // MBID-first per enrichment flow: when the caller has
    // resolved a MusicBrainz artist MBID, query TheAudioDB's
    // MBID-indexed endpoint (`artist-mb.php?i=<mbid>`). Name
    // search is the last-resort fallback for entities without a
    // resolved MB identity.
    let hit = match resolved_mbid {
        Some(mbid) => match tadb.fetch_artist_bio_by_mbid(mbid).await {
            Ok(Some(h)) => h,
            Ok(None) => {
                // TheAudioDB does not know this MBID. Fall back
                // to name-search (last-resort).
                match tadb.search_artist_bio(effective_name).await {
                    Ok(Some(h)) => h,
                    Ok(None) => return None,
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "theaudiodb",
                            entity = %entity.name,
                            effective_name,
                            error = %e,
                            "TheAudioDB artist bio name-fallback \
                             transient; skipping"
                        );
                        return None;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    entity = %entity.name,
                    artist_mbid = mbid,
                    error = %e,
                    "TheAudioDB artist bio MBID lookup transient; \
                     skipping"
                );
                return None;
            }
        },
        None => match tadb.search_artist_bio(effective_name).await {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    entity = %entity.name,
                    effective_name,
                    error = %e,
                    "TheAudioDB artist bio transient; skipping"
                );
                return None;
            }
        },
    };
    // Locale-aware pick: operator locale → English → any
    // non-empty. Reports the language actually served on the
    // SourceEntry so the UI can label prose whose language
    // differs from the operator locale.
    let bio_pick = hit.bio_for_locale(operator_locale);
    if bio_pick.is_none() && hit.genre.is_none() && hit.formed_year.is_none() {
        return None;
    }
    let (bio_text, served_language) = match bio_pick.as_ref() {
        Some((t, l)) => (Some(t.clone()), Some(l.clone())),
        None => (None, None),
    };
    let payload = serde_json::json!({
        "summary": bio_text,
        "language": served_language,
        "genre": hit.genre,
        "formed_year": hit.formed_year,
        "source_url": hit.source_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "theaudiodb");
    Some(SourceEntry {
        provider_id: ProviderId::TheAudioDb.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: served_language,
        payload,
        attribution: Attribution {
            source_name: "TheAudioDB".into(),
            source_url: Some(hit.source_url),
            license: "TheAudioDB terms of use".into(),
        },
    })
}

fn source_url_from_payload(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("source_url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Extract the reported language from a cached payload's
/// `language` field. Locale-aware: caches store the language served
/// alongside the prose so a cache-hit `SourceEntry` reports
/// what was originally served, not `None`.
fn language_from_payload(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("language")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
}

fn wikipedia_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    let language = language_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::Wikipedia.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language,
        payload,
        attribution: Attribution {
            source_name: "Wikipedia".into(),
            source_url,
            license: "CC BY-SA".into(),
        },
    }
}

fn wikidata_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    let language = language_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::Wikidata.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language,
        payload,
        attribution: Attribution {
            source_name: "Wikidata".into(),
            source_url,
            license: "CC0".into(),
        },
    }
}

fn lastfm_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    let language = language_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::Lastfm.as_str().to_string(),
        privacy_class: PrivacyClass::IdentityBearing.as_str().to_string(),
        language,
        payload,
        attribution: Attribution {
            source_name: "Last.fm".into(),
            source_url,
            license: "Last.fm terms of use".into(),
        },
    }
}

fn theaudiodb_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    let language = language_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::TheAudioDb.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language,
        payload,
        attribution: Attribution {
            source_name: "TheAudioDB".into(),
            source_url,
            license: "TheAudioDB terms of use".into(),
        },
    }
}

// -----------------------------------------------------------------
// KEYLESS-FIRST CASCADE — release-credits via MusicBrainz
// full-release lookup (anonymous baseline) → Discogs release
// detail (identity-bearing enhancement).
// -----------------------------------------------------------------

/// Cascade request for release-credits. `release_mbid` short-
/// circuits the `search_release` step when the caller already
/// has one; otherwise the cascade reconciles `(artist, album)`
/// against MusicBrainz first.
#[derive(Debug, Deserialize)]
pub(crate) struct ReleaseCreditsCascadeRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    #[serde(default)]
    release_mbid: Option<String>,
    /// Operator UI locale (BCP47 short). the locale-aware fallback. Personnel /
    /// credits are largely language-agnostic (names + roles),
    /// but Discogs' release notes text honours the locale when
    /// present. Absent = `"en"`.
    #[serde(default)]
    #[allow(dead_code)]
    locale: Option<String>,
}

/// MB confidence floor for accepting a `search_release` hit as
/// the release's canonical MBID. Matches the reconciliation
/// module's threshold.
const RELEASE_SEARCH_CONFIDENCE_FLOOR: u32 = 85;

pub(crate) async fn query_release_credits_cascade(
    payload: &[u8],
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
) -> Result<CascadeResponse, String> {
    if payload.is_empty() {
        return Ok(CascadeResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: ReleaseCreditsCascadeRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(CascadeResponse::bad_request(format!(
            "unsupported v: {}",
            req.v
        )));
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
        return Ok(CascadeResponse::bad_request(
            "artist and album are required and must be non-empty",
        ));
    };
    let release_mbid_hint = req
        .release_mbid
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let want_mb = catalogue
        .config
        .is_effectively_enabled(ProviderId::MusicBrainz)
        && catalogue.musicbrainz.is_some();
    let want_discogs =
        catalogue.config.is_effectively_enabled(ProviderId::Discogs)
            && catalogue.discogs.is_some();

    if !(want_mb || want_discogs) {
        return Ok(CascadeResponse::not_configured(
            "every release-credits provider is disabled or unavailable on \
             this device; enable at least one under Settings → Metadata → \
             Sources",
        ));
    }

    // Both providers are self-contained content sources — MB
    // owns its own release-mbid resolution, Discogs looks up
    // by (artist, album) directly. No cross-dependency, so both
    // dispatch in parallel via `tokio::join!`.
    let (mb_src, discogs_src) = tokio::join!(
        fetch_musicbrainz_release_credits(
            &artist,
            &album,
            release_mbid_hint.as_deref(),
            catalogue,
            cache,
            want_mb,
        ),
        fetch_discogs_release_credits(
            &artist,
            &album,
            catalogue,
            cache,
            want_discogs,
        ),
    );

    let mut sources: Vec<SourceEntry> =
        [mb_src, discogs_src].into_iter().flatten().collect();
    cascade::sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(CascadeResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            detail: Some(format!(
                "no release-credits found for {artist} — {album}"
            )),
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        });
    }
    Ok(CascadeResponse::from_sources(sources, None))
}

async fn fetch_musicbrainz_release_credits(
    artist: &str,
    album: &str,
    release_mbid_hint: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let mb = catalogue.musicbrainz.as_ref()?;
    // Normalise album title before MB release search — MB
    // stores releases under their canonical form (no
    // "(Deluxe Version)" suffix); operator tags carrying the
    // edition suffix sink to a clean miss even for well-known
    // releases.
    let normalised_album = normalise_album_query(album);
    let key = EnrichmentCache::key_for(&[
        "release_credits",
        "release",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::MusicBrainz.as_str(),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(mb_release_credits_source_from_cached_payload(p));
            }
        }
    }
    let release_mbid: Option<String> = match release_mbid_hint {
        Some(m) => Some(m.to_string()),
        None => match mb.search_release(artist, &normalised_album).await {
            Ok(Some(hit))
                if hit.confidence_percent
                    >= RELEASE_SEARCH_CONFIDENCE_FLOOR =>
            {
                Some(hit.release_mbid)
            }
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "musicbrainz",
                    artist,
                    album,
                    error = %e,
                    "MB release search transient; skipping"
                );
                None
            }
        },
    };
    let release_mbid = release_mbid?;
    let rc = match mb.lookup_release_full(&release_mbid).await {
        Ok(rc) => rc,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                release_mbid,
                error = %e,
                "MB full-release lookup transient; skipping"
            );
            return None;
        }
    };
    let payload = release_credits_payload(&rc);
    let _ = cache.put_positive(&key, payload.clone(), "musicbrainz");
    Some(SourceEntry {
        provider_id: ProviderId::MusicBrainz.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: None,
        payload,
        attribution: Attribution {
            source_name: "MusicBrainz".into(),
            source_url: Some(format!(
                "https://musicbrainz.org/release/{}",
                rc.release_mbid
            )),
            license: "CC0".into(),
        },
    })
}

async fn fetch_discogs_release_credits(
    artist: &str,
    album: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let discogs = catalogue.discogs.as_ref()?;
    // Normalise album title before Discogs query — Discogs
    // matches on canonical release titles.
    let normalised_album = normalise_album_query(album);
    let key = EnrichmentCache::key_for(&[
        "release_credits",
        "release",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::Discogs.as_str(),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(discogs_source_from_cached_payload(p));
            }
        }
    }
    match discogs.get_release_detail(artist, &normalised_album).await {
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
            Some(SourceEntry {
                provider_id: ProviderId::Discogs.as_str().to_string(),
                privacy_class: PrivacyClass::IdentityBearing
                    .as_str()
                    .to_string(),
                language: None,
                payload,
                attribution: Attribution {
                    source_name: "Discogs".into(),
                    source_url: h.source_url.clone(),
                    license: "Discogs terms of use".into(),
                },
            })
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "discogs",
                artist,
                album,
                error = %discogs_error_message(&e),
                "Discogs release detail transient; skipping"
            );
            None
        }
    }
}

fn mb_release_credits_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::MusicBrainz.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: None,
        payload,
        attribution: Attribution {
            source_name: "MusicBrainz".into(),
            source_url,
            license: "CC0".into(),
        },
    }
}

fn discogs_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::Discogs.as_str().to_string(),
        privacy_class: PrivacyClass::IdentityBearing.as_str().to_string(),
        language: None,
        payload,
        attribution: Attribution {
            source_name: "Discogs".into(),
            source_url,
            license: "Discogs terms of use".into(),
        },
    }
}

fn release_credits_payload(
    rc: &evo_online_providers::musicbrainz::ReleaseCreditsLookup,
) -> serde_json::Value {
    let tracks: Vec<serde_json::Value> = rc
        .tracks
        .iter()
        .map(|t| {
            serde_json::json!({
                "position": t.position,
                "title": t.title,
                "length_ms": t.length_ms,
                "track_artist": t.track_artist,
                "composer": t.composer,
                "conductor": t.conductor,
                "performer": t.performer,
                "work_mbid": t.work_mbid,
            })
        })
        .collect();
    serde_json::json!({
        "release_mbid": rc.release_mbid,
        "first_release_year": rc.first_release_year,
        "recording_type": rc.recording_type,
        "label_name": rc.label_name,
        "catalog_number": rc.catalog_number,
        "album_artist": rc.album_artist,
        "country": rc.country,
        "tracks": tracks,
        "source_url": format!(
            "https://musicbrainz.org/release/{}",
            rc.release_mbid
        ),
    })
}

// cascade_ok_from_cache_generic retired — folded into the sole
// cascade_ok_from_cache helper above, which now takes the same
// source-url-override + enhancement params for every verb. One
// rebuilder means one place to enforce "cache hit == fresh hit"
// on the fields UI renders.

// -----------------------------------------------------------------
// KEYLESS-FIRST CASCADE — track-annotation via Wikipedia song
// page (anonymous baseline, best-effort) → Genius description
// (identity-bearing enhancement).
// -----------------------------------------------------------------
//
// The Genius API does NOT return lyrics text; this verb only
// surfaces its `description` field plus the URL of Genius's web
// page for the song. Consumers render the URL as an outbound
// "View lyrics on Genius" affordance. The plugin never fetches
// Genius's HTML lyrics page.

/// Cascade request for track-annotation. The optional
/// `recording_mbid` short-circuits the Wikipedia title-search
/// step when the caller can supply it (from the release-credits
/// cascade payload's `tracks[].work_mbid` is a companion path
/// but not the recording-mbid; MB does not expose recording url-
/// rels through the current shipped client, so this field is
/// wire-compatible today and consumed once that client method
/// lands).
#[derive(Debug, Deserialize)]
pub(crate) struct TrackAnnotationCascadeRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    track: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    recording_mbid: Option<String>,
    /// Operator UI locale (BCP47 short). the locale-aware fallback. Wikipedia
    /// track annotations honour the locale by editing edition;
    /// Genius text falls through as English. Absent = `"en"`.
    #[serde(default)]
    locale: Option<String>,
}

pub(crate) async fn query_track_annotation_cascade(
    payload: &[u8],
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
) -> Result<CascadeResponse, String> {
    if payload.is_empty() {
        return Ok(CascadeResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: TrackAnnotationCascadeRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(CascadeResponse::bad_request(format!(
            "unsupported v: {}",
            req.v
        )));
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
        return Ok(CascadeResponse::bad_request(
            "artist and track are required and must be non-empty",
        ));
    };

    let want_wp = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikipedia)
        && catalogue.wikipedia.is_some();
    let want_genius =
        catalogue.config.is_effectively_enabled(ProviderId::Genius)
            && catalogue.genius.is_some();

    if !(want_wp || want_genius) {
        return Ok(CascadeResponse::not_configured(
            "every track-annotation provider is disabled or unavailable on \
             this device; enable at least one under Settings → Metadata → \
             Sources",
        ));
    }

    let operator_locale = locale::normalise(req.locale.as_deref());
    let (wp_src, genius_src) = tokio::join!(
        fetch_wikipedia_track_annotation(
            &artist,
            &track,
            catalogue,
            cache,
            want_wp,
            &operator_locale,
        ),
        fetch_genius_track_annotation(
            &artist,
            &track,
            catalogue,
            cache,
            want_genius
        ),
    );

    let mut sources: Vec<SourceEntry> =
        [wp_src, genius_src].into_iter().flatten().collect();
    cascade::sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(CascadeResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            detail: Some(format!(
                "no track annotation found for {artist} — {track} across \
                 enabled providers"
            )),
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        });
    }
    Ok(CascadeResponse::from_sources(sources, None))
}

async fn fetch_wikipedia_track_annotation(
    artist: &str,
    track: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let wp = catalogue.wikipedia.as_ref()?;
    let key = EnrichmentCache::key_for(&[
        "track_annotation",
        "song",
        &normalise(artist),
        &normalise(track),
        ProviderId::Wikipedia.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikipedia_source_from_cached_payload(p));
            }
        }
    }
    // Wikipedia's song-article naming pattern: try the two
    // disambiguated forms first, then bare-title. Songs rarely
    // have their own article, and the disambiguated forms
    // (`"{Track} ({Artist} song)"` / `"{Track} (song)"`) are
    // Wikipedia's own convention. Exhaust all title
    // forms in the operator-locale edition BEFORE falling back
    // to English.
    let candidate_titles = [
        format!("{track} ({artist} song)"),
        format!("{track} (song)"),
        track.to_string(),
    ];
    let mut wp_hit = None;
    for lang in fallback_langs(operator_locale) {
        for candidate in &candidate_titles {
            match wp.get_summary(candidate, lang).await {
                Ok(Some(summary)) => {
                    wp_hit = Some(summary);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        title = %candidate,
                        language = lang,
                        error = %e,
                        "Wikipedia song-title lookup transient / \
                         not-usable; trying next form"
                    );
                    continue;
                }
            }
        }
        if wp_hit.is_some() {
            break;
        }
    }
    let summary = wp_hit?;
    let served_language = summary.language.clone();
    let payload = serde_json::json!({
        "title": summary.title,
        "summary": summary.extract,
        "language": served_language,
        "source_url": summary.page_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
    Some(SourceEntry {
        provider_id: ProviderId::Wikipedia.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: Some(served_language),
        payload,
        attribution: Attribution {
            source_name: "Wikipedia".into(),
            source_url: Some(summary.page_url),
            license: "CC BY-SA".into(),
        },
    })
}

async fn fetch_genius_track_annotation(
    artist: &str,
    track: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let genius = catalogue.genius.as_ref()?;
    let key = EnrichmentCache::key_for(&[
        "track_annotation",
        "song",
        &normalise(artist),
        &normalise(track),
        ProviderId::Genius.as_str(),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(genius_source_from_cached_payload(p));
            }
        }
    }
    match genius.get_track_annotation(artist, track).await {
        Ok(Some(h)) => {
            let payload = serde_json::json!({
                "song_id": h.song_id,
                "description": h.description,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "genius");
            Some(SourceEntry {
                provider_id: ProviderId::Genius.as_str().to_string(),
                privacy_class: PrivacyClass::IdentityBearing
                    .as_str()
                    .to_string(),
                language: None,
                payload,
                attribution: Attribution {
                    source_name: "Genius".into(),
                    source_url: h.source_url.clone(),
                    license: "Genius terms of use".into(),
                },
            })
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "genius",
                artist,
                track,
                error = %genius_error_message(&e),
                "Genius track annotation transient; skipping"
            );
            None
        }
    }
}

fn genius_source_from_cached_payload(
    payload: serde_json::Value,
) -> SourceEntry {
    let source_url = source_url_from_payload(&payload);
    SourceEntry {
        provider_id: ProviderId::Genius.as_str().to_string(),
        privacy_class: PrivacyClass::IdentityBearing.as_str().to_string(),
        language: None,
        payload,
        attribution: Attribution {
            source_name: "Genius".into(),
            source_url,
            license: "Genius terms of use".into(),
        },
    }
}

// -----------------------------------------------------------------
// KEYLESS-FIRST CASCADE — album-notes via Wikipedia album page
// (anonymous baseline, title-search variants) → Last.fm album
// (identity-bearing enhancement).
// -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct AlbumNotesCascadeRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    artist: Option<String>,
    #[serde(default)]
    album: Option<String>,
    /// Optional MB release MBID. When supplied, the Last.fm
    /// enhancement branch uses it to refine the lookup; the
    /// Wikipedia baseline still runs title-search variants
    /// (Wikipedia does not resolve on MBID).
    #[serde(default)]
    release_mbid: Option<String>,
    /// Operator UI locale (BCP47 short). the locale-aware fallback. Wikipedia
    /// resolves against the operator-locale edition;
    /// TheAudioDB selects `strDescription<CC>`; Last.fm
    /// stays English (its API is locale-agnostic). Absent =
    /// `"en"`.
    #[serde(default)]
    locale: Option<String>,
}

pub(crate) async fn query_album_notes_cascade(
    payload: &[u8],
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
) -> Result<CascadeResponse, String> {
    if payload.is_empty() {
        return Ok(CascadeResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: AlbumNotesCascadeRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(CascadeResponse::bad_request(format!(
            "unsupported v: {}",
            req.v
        )));
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
        return Ok(CascadeResponse::bad_request(
            "artist and album are required and must be non-empty",
        ));
    };
    let release_mbid_hint = req
        .release_mbid
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let want_wp = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikipedia)
        && catalogue.wikipedia.is_some();
    let want_lastfm =
        catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
            && catalogue.lastfm.is_some();
    let want_theaudiodb = catalogue
        .config
        .is_effectively_enabled(ProviderId::TheAudioDb)
        && catalogue.theaudiodb.is_some();
    let want_discogs =
        catalogue.config.is_effectively_enabled(ProviderId::Discogs)
            && catalogue.discogs.is_some();

    if !(want_wp || want_lastfm || want_theaudiodb || want_discogs) {
        return Ok(CascadeResponse::not_configured(
            "every album-notes provider is disabled or unavailable on this \
             device; enable at least one under Settings → Metadata → \
             Sources",
        ));
    }

    let operator_locale = locale::normalise(req.locale.as_deref());
    let (wp_src, lastfm_src, tadb_src, discogs_src) = tokio::join!(
        fetch_wikipedia_album_notes(
            &artist,
            &album,
            catalogue,
            cache,
            want_wp,
            &operator_locale,
        ),
        fetch_lastfm_album_notes(
            &artist,
            &album,
            release_mbid_hint.as_deref(),
            catalogue,
            cache,
            want_lastfm,
        ),
        fetch_theaudiodb_album_notes(
            &artist,
            &album,
            catalogue,
            cache,
            want_theaudiodb,
            &operator_locale,
        ),
        fetch_discogs_album_notes(
            &artist,
            &album,
            release_mbid_hint.as_deref(),
            catalogue,
            cache,
            want_discogs,
        ),
    );

    let mut sources: Vec<SourceEntry> =
        [wp_src, lastfm_src, tadb_src, discogs_src]
            .into_iter()
            .flatten()
            .collect();
    cascade::sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(CascadeResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            detail: Some(format!(
                "no album notes found for {artist} — {album} across enabled \
                 providers"
            )),
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        });
    }
    Ok(CascadeResponse::from_sources(sources, None))
}

async fn fetch_wikipedia_album_notes(
    artist: &str,
    album: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let wp = catalogue.wikipedia.as_ref()?;
    // Normalise the album tag to Wikipedia's article-title
    // convention BEFORE cache key + search. Operator tags may
    // carry `(Deluxe Version)` / ` - Deluxe Edition` /
    // ` - Remastered` suffixes iTunes and streaming stores emit;
    // the target Wikipedia article is under the canonical album
    // title, not the edition-tagged form. Also normalises the
    // ` - ` subtitle separator to `: ` — Wikipedia article
    // titles use the colon-space form ("Closer: The Best of
    // Sarah McLachlan" vs the tag's "Closer - The Best Of
    // Sarah McLachlan"). Without this, tag variants sink into
    // clean-miss even when the article plainly exists.
    let normalised_album = normalise_album_query(album);
    // Locale-scoped cache key so each language edition
    // gets its own entry.
    let key = EnrichmentCache::key_for(&[
        "album_notes",
        "album",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::Wikipedia.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikipedia_source_from_cached_payload(p));
            }
        }
    }
    // Wikipedia's album-article naming convention: two
    // disambiguated forms then bare title, on the NORMALISED
    // album title so `(Deluxe Version)` etc. don't send us to
    // an inevitable clean miss.
    let candidate_titles = [
        format!("{normalised_album} ({artist} album)"),
        format!("{normalised_album} (album)"),
        normalised_album.clone(),
    ];
    // Try each title in the operator-locale edition
    // first; only after ALL titles miss in operator locale do
    // we fall back to English. (The inverse — full title
    // sequence in en first — would surface English content
    // even when the operator locale has the article, defeating
    // the locale-aware selector.)
    let mut wp_hit = None;
    for lang in fallback_langs(operator_locale) {
        for candidate in &candidate_titles {
            match wp.get_summary(candidate, lang).await {
                Ok(Some(summary)) => {
                    wp_hit = Some(summary);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        title = %candidate,
                        language = lang,
                        error = %e,
                        "Wikipedia album-title lookup transient / \
                         not-usable; trying next form"
                    );
                    continue;
                }
            }
        }
        if wp_hit.is_some() {
            break;
        }
    }
    let summary = wp_hit?;
    let served_language = summary.language.clone();
    let payload = serde_json::json!({
        "title": summary.title,
        "summary": summary.extract,
        "language": served_language,
        "source_url": summary.page_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
    Some(SourceEntry {
        provider_id: ProviderId::Wikipedia.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: Some(served_language),
        payload,
        attribution: Attribution {
            source_name: "Wikipedia".into(),
            source_url: Some(summary.page_url),
            license: "CC BY-SA".into(),
        },
    })
}

/// Language fallback chain: operator locale, then
/// English (unless operator locale is already English). Used by
/// bare-title-search helpers (Wikipedia album / work / track
/// annotation) that don't have a Wikidata sitelink hop
/// available.
fn fallback_langs(operator_locale: &str) -> Vec<&str> {
    if operator_locale == "en" {
        vec!["en"]
    } else {
        vec![operator_locale, "en"]
    }
}

async fn fetch_lastfm_album_notes(
    artist: &str,
    album: &str,
    release_mbid_hint: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let lastfm = catalogue.lastfm.as_ref()?;
    // Same normalisation as the Wikipedia helper — Last.fm's
    // album.getinfo matches on canonical titles, edition
    // suffixes send the query to a clean miss.
    let normalised_album = normalise_album_query(album);
    let key = EnrichmentCache::key_for(&[
        "album_notes",
        "album",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::Lastfm.as_str(),
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(lastfm_source_from_cached_payload(p));
            }
        }
    }
    match lastfm
        .get_album_notes(artist, &normalised_album, release_mbid_hint)
        .await
    {
        Ok(Some(h)) => {
            let payload = serde_json::json!({
                "summary": h.summary,
                "content": h.content,
                "source_url": h.source_url,
            });
            let _ = cache.put_positive(&key, payload.clone(), "lastfm");
            Some(SourceEntry {
                provider_id: ProviderId::Lastfm.as_str().to_string(),
                privacy_class: PrivacyClass::IdentityBearing
                    .as_str()
                    .to_string(),
                language: None,
                payload,
                attribution: Attribution {
                    source_name: "Last.fm".into(),
                    source_url: h.source_url.clone(),
                    license: "Last.fm terms of use".into(),
                },
            })
        }
        Ok(None) => None,
        Err(LastfmError::Application { code, message })
            if evo_online_providers::lastfm_is_notfound_code(code) =>
        {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "lastfm",
                code,
                message,
                "Last.fm album clean miss"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "lastfm",
                artist,
                album,
                error = %e,
                "Last.fm album transient; skipping"
            );
            None
        }
    }
}

async fn fetch_theaudiodb_album_notes(
    artist: &str,
    album: &str,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let tadb = catalogue.theaudiodb.as_ref()?;
    // Same normalisation as the other album helpers — TheAudioDB's
    // search matches on canonical album titles.
    let normalised_album = normalise_album_query(album);
    // Locale-scoped cache key.
    let key = EnrichmentCache::key_for(&[
        "album_notes",
        "album",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::TheAudioDb.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(theaudiodb_source_from_cached_payload(p));
            }
        }
    }
    let hit = match tadb.search_album_notes(artist, &normalised_album).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "theaudiodb",
                artist,
                album,
                normalised_album,
                error = %e,
                "TheAudioDB album notes transient; skipping"
            );
            return None;
        }
    };
    // Locale-aware pick for the album description.
    let desc_pick = hit.description_for_locale(operator_locale);
    let desc_present = desc_pick.is_some();
    let review_present = hit
        .review
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    // TheAudioDB's album records sometimes carry only a
    // release year + label without any prose. Suppress the
    // entry when there's no renderable text.
    if !desc_present
        && !review_present
        && hit.year.is_none()
        && hit.label.is_none()
    {
        return None;
    }
    let (desc_text, served_language) = match desc_pick.as_ref() {
        Some((t, l)) => (Some(t.clone()), Some(l.clone())),
        None => (None, None),
    };
    let payload = serde_json::json!({
        "summary": desc_text,
        "language": served_language,
        "review": hit.review,
        "year": hit.year,
        "label": hit.label,
        "source_url": hit.source_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "theaudiodb");
    Some(SourceEntry {
        provider_id: ProviderId::TheAudioDb.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: served_language,
        payload,
        attribution: Attribution {
            source_name: "TheAudioDB".into(),
            source_url: Some(hit.source_url),
            license: "TheAudioDB terms of use".into(),
        },
    })
}

/// Discogs album-notes helper. MBID-first per the enrichment flow: the
/// MusicBrainz release lookup surfaces the `discogs` url-rel
/// (when MB carries one), the plugin parses the release id from
/// that URL and hits Discogs's `/releases/{id}` endpoint
/// directly. Name-search is the last-resort fallback for
/// releases MB has not yet linked to Discogs.
///
/// Discogs is THE primary album-notes source for this device's
/// niche / audiophile / SACD / DSD catalogue. Wikipedia +
/// Last.fm + TheAudioDB miss on labels like Blue Coast Records,
/// Tiny Island Music, and countless small-run audiophile
/// pressings. The `release.notes` field on Discogs carries the
/// album's liner text; label / catno / format land alongside so
/// the operator UI can surface all four as one Discogs entry.
async fn fetch_discogs_album_notes(
    artist: &str,
    album: &str,
    release_mbid_hint: Option<&str>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let discogs = catalogue.discogs.as_ref()?;
    let normalised_album = normalise_album_query(album);
    // Cache key mirrors the other album-notes helpers'
    // (artist, normalised_album, provider) partition. Discogs
    // album notes are language-agnostic (the wiki text
    // contributors write in whatever language they picked),
    // so no locale scope is needed here.
    //
    // The trailing "v2" tag partitions entries fetched under the
    // plaintext-annotated Accept dialect from earlier entries
    // that landed pre-fix and cached raw bracketed references
    // (`[l333658]` = label id, `[r=571297]` = release id) as
    // rendered summary text. Positive cache is indefinite, so
    // without this partition the operator would keep seeing
    // "[l333658]" in the summary on every replay of already-cached
    // releases even after the client fix ships. Bumping the tag
    // whenever the extractor's output shape changes at a
    // per-provider level is the standard escape hatch.
    let key = EnrichmentCache::key_for(&[
        "album_notes",
        "album",
        &normalise(artist),
        &normalise(&normalised_album),
        ProviderId::Discogs.as_str(),
        "v2",
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(discogs_source_from_cached_payload(p));
            }
        }
    }

    // Path 1 — MBID-first per the enrichment flow. When the caller passed a
    // release MBID, MB's release lookup surfaces `discogs`
    // url-rel; we parse the id and hit Discogs directly. This
    // sidesteps Discogs's fuzzy `(artist, album)` search
    // entirely for MB-linked releases.
    let mut hit = None;
    if let (Some(mb), Some(mbid)) =
        (catalogue.musicbrainz.as_ref(), release_mbid_hint)
    {
        match mb.lookup_release(mbid).await {
            Ok(release_lookup) => {
                if let Some(discogs_url) = release_lookup.discogs_url.as_deref()
                {
                    match evo_online_providers::discogs::parse_discogs_release_id(
                        discogs_url,
                    ) {
                        Some(release_id) => {
                            match discogs.get_release_by_id(release_id).await {
                                Ok(Some(h)) => hit = Some(h),
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        plugin = crate::PLUGIN_NAME,
                                        provider = "discogs",
                                        release_id,
                                        error = %discogs_error_message(&e),
                                        "Discogs release-by-id transient; \
                                         trying name-search fallback"
                                    );
                                }
                            }
                        }
                        None => {
                            tracing::debug!(
                                plugin = crate::PLUGIN_NAME,
                                provider = "discogs",
                                url = discogs_url,
                                "MB `discogs` url-rel did not parse to a \
                                 release id (may be a master/label link); \
                                 falling back to name search"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "musicbrainz",
                    release_mbid = mbid,
                    error = %e,
                    "MB release lookup transient for Discogs url-rel; \
                     falling back to name search"
                );
            }
        }
    }

    // Path 2 — name-search fallback. Uses the normalised album
    // title so `(Deluxe Version)` etc. don't sink the lookup.
    if hit.is_none() {
        match discogs.get_release_detail(artist, &normalised_album).await {
            Ok(Some(h)) => hit = Some(h),
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "discogs",
                    artist,
                    album,
                    normalised_album,
                    error = %discogs_error_message(&e),
                    "Discogs release-search transient; skipping"
                );
                return None;
            }
        }
    }

    let hit = hit?;
    // Discogs releases sometimes carry only structured facts
    // (label / format / catno) without any prose notes.
    // Suppress the entry when there's no renderable text AND
    // no useful structured facts — a fully-empty entry helps
    // no one.
    let notes = hit
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if notes.is_none()
        && hit.label.is_none()
        && hit.format.is_none()
        && hit.year.is_none()
    {
        return None;
    }

    let source_url = hit.source_url.clone().unwrap_or_else(|| {
        format!("https://www.discogs.com/release/{}", hit.release_id)
    });
    let payload = serde_json::json!({
        "summary": notes,
        "label": hit.label,
        "catalog_number": hit.catalog_number,
        "format": hit.format,
        "year": hit.year,
        "country": hit.country,
        "release_id": hit.release_id,
        "source_url": source_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "discogs");
    Some(SourceEntry {
        provider_id: ProviderId::Discogs.as_str().to_string(),
        privacy_class: PrivacyClass::IdentityBearing.as_str().to_string(),
        language: None,
        payload,
        attribution: Attribution {
            source_name: "Discogs".into(),
            source_url: Some(source_url),
            license: "Discogs terms of use".into(),
        },
    })
}

// -----------------------------------------------------------------
// KEYLESS-FIRST CASCADE — classical work notes via MusicBrainz
// work lookup → url-rels → Wikipedia work-page summary.
// Wikidata offers structured facts as a secondary anonymous
// fallback when the work has a Wikidata entity but no
// Wikipedia article.
//
// No identity-bearing enhancement: Wikipedia + Wikidata are
// the authoritative sources for classical works; no keyed
// provider currently improves on their coverage or fidelity.
// -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct WorkNotesCascadeRequest {
    #[serde(default)]
    v: u8,
    /// Work title (composer's original or MB-canonical form).
    /// The cascade prefers `work_mbid` when supplied and
    /// short-circuits the search step.
    #[serde(default)]
    work_name: Option<String>,
    /// Optional MB work MBID. Companion to the release-credits
    /// cascade's per-track `work_mbid` — a caller with the
    /// release-credits response in hand can pass it verbatim.
    #[serde(default)]
    work_mbid: Option<String>,
    /// Optional composer hint used to disambiguate a `search_work`
    /// candidate list when the title alone is generic (e.g. many
    /// composers wrote pieces titled `"Symphony No. 5"`).
    #[serde(default)]
    composer: Option<String>,
    /// Operator UI locale (BCP47 short). the locale-aware fallback. Wikipedia
    /// work page honours the locale; Wikidata description
    /// selects the operator-locale label. Absent = `"en"`.
    #[serde(default)]
    locale: Option<String>,
}

/// MB confidence floor for a `search_work` hit to be adopted as
/// the work's canonical MBID.
const WORK_SEARCH_CONFIDENCE_FLOOR: u32 = 85;

pub(crate) async fn query_work_notes_cascade(
    payload: &[u8],
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
) -> Result<CascadeResponse, String> {
    if payload.is_empty() {
        return Ok(CascadeResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: WorkNotesCascadeRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(CascadeResponse::bad_request(format!(
            "unsupported v: {}",
            req.v
        )));
    }
    let work_name = req
        .work_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let has_mbid = req
        .work_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some();
    if work_name.is_none() && !has_mbid {
        return Ok(CascadeResponse::bad_request(
            "at least one of `work_name` or `work_mbid` is required",
        ));
    }
    let composer_hint = req
        .composer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let want_mb = catalogue
        .config
        .is_effectively_enabled(ProviderId::MusicBrainz)
        && catalogue.musicbrainz.is_some();
    let want_wp = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikipedia)
        && catalogue.wikipedia.is_some();
    let want_wd = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikidata)
        && catalogue.wikidata.is_some();

    if !(want_mb || want_wp || want_wd) {
        return Ok(CascadeResponse::not_configured(
            "every work-notes provider is disabled or unavailable on this \
             device; enable at least one under Settings → Metadata → Sources",
        ));
    }

    // MB identity-resolve phase — sequential; feeds Wikipedia +
    // Wikidata URLs and the canonical title we key their caches
    // by. Skipped when MB is disabled OR the caller supplied
    // both work_mbid + work_name (nothing MB can add without a
    // lookup, and MB lookups aren't free).
    let mut wikipedia_url: Option<String> = None;
    let mut wikidata_url: Option<String> = None;
    let mut canonical_title: Option<String> = None;
    let mut work_type: Option<String> = None;
    let mut resolved_work_mbid: Option<String> = req
        .work_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if want_mb {
        if let Some(mb) = catalogue.musicbrainz.as_ref() {
            if resolved_work_mbid.is_none() {
                if let Some(name) = work_name.as_deref() {
                    match mb.search_work(name, composer_hint.as_deref()).await {
                        Ok(Some(hit))
                            if hit.confidence_percent
                                >= WORK_SEARCH_CONFIDENCE_FLOOR =>
                        {
                            resolved_work_mbid = Some(hit.work_mbid);
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                plugin = crate::PLUGIN_NAME,
                                provider = "musicbrainz",
                                work = %name,
                                error = %e,
                                "MB work search transient; skipping"
                            );
                        }
                    }
                }
            }
            if let Some(work_mbid) = resolved_work_mbid.as_ref() {
                match mb.lookup_work(work_mbid).await {
                    Ok(wl) => {
                        wikipedia_url = wl.wikipedia_url;
                        wikidata_url = wl.wikidata_url;
                        canonical_title = Some(wl.canonical_title);
                        work_type = wl.work_type;
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            work_mbid,
                            error = %e,
                            "MB work lookup transient; skipping"
                        );
                    }
                }
            }
        }
    }

    let cache_name = canonical_title
        .clone()
        .or_else(|| work_name.clone())
        .unwrap_or_default();
    let operator_locale = locale::normalise(req.locale.as_deref());
    let (wp_src, wd_src) = tokio::join!(
        fetch_wikipedia_work_notes(
            &cache_name,
            work_name.as_deref(),
            canonical_title.as_deref(),
            wikipedia_url.as_deref(),
            resolved_work_mbid.clone(),
            work_type.clone(),
            catalogue,
            cache,
            want_wp,
            &operator_locale,
        ),
        fetch_wikidata_work_notes(
            &cache_name,
            wikidata_url.as_deref(),
            resolved_work_mbid.clone(),
            work_type.clone(),
            catalogue,
            cache,
            want_wd,
            &operator_locale,
        ),
    );

    let mut sources: Vec<SourceEntry> =
        [wp_src, wd_src].into_iter().flatten().collect();
    cascade::sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(CascadeResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            detail: Some(format!(
                "no work notes found for {} across enabled providers",
                canonical_title
                    .clone()
                    .or_else(|| work_name.clone())
                    .unwrap_or_else(|| "the requested work".to_string())
            )),
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        });
    }
    Ok(CascadeResponse::from_sources(sources, None))
}

// 10 args reflect the parallel-dispatch shape — the work-notes
// verb's MB identity-resolve phase produces four downstream
// signals (wikipedia_url, work_mbid, canonical_title,
// work_type) that both leaf helpers consume; grouping them
// into a struct would move the args without reducing them.
#[allow(clippy::too_many_arguments)]
async fn fetch_wikipedia_work_notes(
    cache_name: &str,
    work_name: Option<&str>,
    canonical_title: Option<&str>,
    wikipedia_url: Option<&str>,
    resolved_work_mbid: Option<String>,
    work_type: Option<String>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let wp = catalogue.wikipedia.as_ref()?;
    let key = EnrichmentCache::key_for(&[
        "work_notes",
        "work",
        &normalise(cache_name),
        ProviderId::Wikipedia.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikipedia_source_from_cached_payload(p));
            }
        }
    }
    // If MB gave us a URL, honour it (the URL is MBID-
    // authoritative for THIS work; its language is what MB
    // knows about). Otherwise search canonical_title, then
    // work_name, each in operator locale → English.
    let hit = match wikipedia_url {
        Some(url) => wp.get_summary_from_url(url).await,
        None => {
            let mut fetched = Ok(None);
            'outer: for lang in fallback_langs(operator_locale) {
                for candidate in canonical_title.into_iter().chain(work_name) {
                    match wp.get_summary(candidate, lang).await {
                        Ok(Some(s)) => {
                            fetched = Ok(Some(s));
                            break 'outer;
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            fetched = Err(e);
                            // Try next language / candidate rather
                            // than aborting — a transient on one
                            // form still lets the fallback land.
                        }
                    }
                }
            }
            fetched
        }
    };
    let summary = match hit {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "wikipedia",
                work = %cache_name,
                error = %e,
                "Wikipedia work-summary transient / not-usable; skipping"
            );
            return None;
        }
    };
    let served_language = summary.language.clone();
    let payload = serde_json::json!({
        "title": summary.title,
        "summary": summary.extract,
        "language": served_language,
        "work_mbid": resolved_work_mbid,
        "work_type": work_type,
        "source_url": summary.page_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
    Some(SourceEntry {
        provider_id: ProviderId::Wikipedia.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: Some(served_language),
        payload,
        attribution: Attribution {
            source_name: "Wikipedia".into(),
            source_url: Some(summary.page_url),
            license: "CC BY-SA".into(),
        },
    })
}

#[allow(clippy::too_many_arguments)]
async fn fetch_wikidata_work_notes(
    cache_name: &str,
    wikidata_url: Option<&str>,
    resolved_work_mbid: Option<String>,
    work_type: Option<String>,
    catalogue: &ProviderCatalogue,
    cache: &EnrichmentCache,
    enabled: bool,
    operator_locale: &str,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let wd = catalogue.wikidata.as_ref()?;
    let wd_url = wikidata_url?;
    let key = EnrichmentCache::key_for(&[
        "work_notes",
        "work",
        &normalise(cache_name),
        ProviderId::Wikidata.as_str(),
        "loc",
        operator_locale,
    ]);
    if let Some(entry) = cache.get(&key) {
        if entry.status == "ok" {
            if let Some(p) = entry.payload {
                return Some(wikidata_source_from_cached_payload(p));
            }
        }
    }
    let entity_hit = match wd.get_entity_from_url(wd_url).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "wikidata",
                work = %cache_name,
                error = %e,
                "Wikidata work-facts transient; skipping"
            );
            return None;
        }
    };
    let label_pick = entity_hit.label_for(operator_locale);
    let description_pick = entity_hit.description_for(operator_locale);
    let served_language = description_pick
        .as_ref()
        .map(|(_, l)| l.clone())
        .or_else(|| label_pick.as_ref().map(|(_, l)| l.clone()));
    let payload = serde_json::json!({
        "label": label_pick.as_ref().map(|(t, _)| t),
        "description": description_pick.as_ref().map(|(t, _)| t),
        "language": served_language,
        "inception": entity_hit.inception,
        "genre_ids": entity_hit.genre_ids,
        "work_mbid": resolved_work_mbid,
        "work_type": work_type,
        "source_url": entity_hit.entity_url,
    });
    let _ = cache.put_positive(&key, payload.clone(), "wikidata");
    Some(SourceEntry {
        provider_id: ProviderId::Wikidata.as_str().to_string(),
        privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
        language: served_language,
        payload,
        attribution: Attribution {
            source_name: "Wikidata".into(),
            source_url: Some(entity_hit.entity_url),
            license: "CC0".into(),
        },
    })
}
