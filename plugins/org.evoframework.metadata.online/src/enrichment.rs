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

use evo_online_providers::{
    lastfm::LastfmError, DiscogsError, GeniusError, LrclibClient,
};
use serde::{Deserialize, Serialize};

use crate::cascade::{
    Attribution, CascadeResponse, CascadeStatus, EnhancementHint, EntityRef,
    EntityType, PrivacyClass, ProviderCatalogue, ProviderId,
};
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
fn bio_cache_key(
    entity: &EntityRef,
    resolved_mbid: Option<&str>,
    provider: ProviderId,
) -> String {
    match resolved_mbid {
        Some(mbid) => EnrichmentCache::key_for(&[
            "entity_bio",
            entity.entity_type.as_str(),
            "mbid",
            mbid,
            provider.as_str(),
        ]),
        None => EnrichmentCache::key_for(&[
            "entity_bio",
            entity.entity_type.as_str(),
            "name",
            &normalise(&entity.name),
            provider.as_str(),
        ]),
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

    // Anonymous baseline: try MusicBrainz artist search + url-rels
    // to discover Wikipedia / Wikidata URLs, then Wikipedia
    // summary, then Wikidata facts. Every step is gated on
    // per-provider enable + privacy-mode.
    let mut last_provider: Option<ProviderId> = None;

    // Positive cache pre-check per provider so an operator
    // disabling a provider does not suppress cached positives
    // from a still-enabled provider.
    // Anonymous first: Wikipedia (bio prose is our primary
    // enrichment surface).
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

    if !(want_mb || want_wp || want_wd || want_lastfm) {
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
    // either caller-supplied or MB-resolved. It's hoisted to outer
    // scope so the Wikipedia + Wikidata cache-key namespacing below
    // can partition MBID-resolved lookups from name-only ones. A
    // name-only cache entry cannot poison an MBID-keyed lookup and
    // vice versa (the fix for the Passenger/common-noun trap: same
    // name string across two different real artists must not
    // collide in the cache).
    let mut resolved_mbid: Option<String> = entity
        .mbid
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut wikipedia_url: Option<String> = None;
    let mut wikidata_url: Option<String> = None;
    if want_mb {
        if let Some(mb) = catalogue.musicbrainz.as_ref() {
            last_provider = Some(ProviderId::MusicBrainz);
            if resolved_mbid.is_none() {
                match mb.search_artist(&entity.name).await {
                    Ok(Some(hit)) if hit.confidence_percent >= 85 => {
                        resolved_mbid = Some(hit.artist_mbid);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        // MB transient — do not cache; skip MB.
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

    // Wikipedia summary — the primary anonymous bio content.
    if want_wp {
        if let Some(wp) = catalogue.wikipedia.as_ref() {
            last_provider = Some(ProviderId::Wikipedia);
            // Cache key is namespaced by (mbid | name): MBID-
            // resolved bios live in a separate namespace from
            // name-only bios so a name collision (Passenger the
            // musician vs the transport noun) cannot leak between
            // them. This is the load-bearing invariant against
            // the common-noun trap.
            let key = bio_cache_key(
                &entity,
                resolved_mbid.as_deref(),
                ProviderId::Wikipedia,
            );
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikipedia,
                            None,
                            enhancement_hint_for_bio(
                                catalogue,
                                ProviderId::Wikipedia,
                            ),
                        ));
                    }
                }
            }
            let hit = match &wikipedia_url {
                Some(url) => wp.get_summary_from_url(url).await,
                None => {
                    // Artist-type entities MUST NOT fall back to
                    // a bare-name Wikipedia title search when MB
                    // did not route a `wikipedia` url-rel — the
                    // plain title search hits common-noun articles
                    // for artists whose names collide with English
                    // words (Passenger the musician vs the
                    // transport noun, Bush the band vs the shrub,
                    // Cake / Air / Live / Yes / Blur / …). Prefer
                    // honest empty over a confidently-wrong
                    // common-noun article. Non-artist entity
                    // types (composer / work / performer /
                    // conductor / ensemble) retain the bare-name
                    // fallback pending an equivalent collision
                    // audit; work_notes has its own cascade path.
                    if matches!(entity.entity_type, EntityType::Artist) {
                        tracing::debug!(
                            plugin = crate::PLUGIN_NAME,
                            entity = %entity.name,
                            "artist-type entity with no MB-routed \
                             Wikipedia URL; refusing bare-name \
                             Wikipedia fallback (common-noun \
                             disambiguation trap)"
                        );
                        Ok(None)
                    } else {
                        wp.get_summary_en(&entity.name).await
                    }
                }
            };
            match hit {
                Ok(Some(summary)) => {
                    // Include source_url in the cached payload so
                    // the cache-hit rebuilder can recover Wikipedia's
                    // canonical page URL. CC BY-SA requires the
                    // attribution link on every rendered payload,
                    // including cached ones — dropping the URL is
                    // a licence violation, not a cosmetic gap.
                    let payload = serde_json::json!({
                        "title": summary.title,
                        "summary": summary.extract,
                        "language": summary.language,
                        "source_url": summary.page_url,
                    });
                    let _ =
                        cache.put_positive(&key, payload.clone(), "wikipedia");
                    let enhancement = enhancement_hint_for_bio(
                        catalogue,
                        ProviderId::Wikipedia,
                    );
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Wikipedia.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::Anonymous.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Wikipedia".into(),
                            source_url: Some(summary.page_url),
                            license: "CC BY-SA".into(),
                        }),
                        enhancement,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        entity = %entity.name,
                        error = %e,
                        "Wikipedia summary transient / not-usable; skipping"
                    );
                }
            }
        }
    }

    // Wikidata — structured biographical facts. Fallback bio
    // surface when Wikipedia has no summary.
    if want_wd {
        if let Some(wd) = catalogue.wikidata.as_ref() {
            last_provider = Some(ProviderId::Wikidata);
            let key = bio_cache_key(
                &entity,
                resolved_mbid.as_deref(),
                ProviderId::Wikidata,
            );
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikidata,
                            None,
                            enhancement_hint_for_bio(
                                catalogue,
                                ProviderId::Wikidata,
                            ),
                        ));
                    }
                }
            }
            let hit = match &wikidata_url {
                Some(url) => wd.get_entity_from_url(url).await,
                None => Ok(None),
            };
            if let Ok(Some(entity_hit)) = hit {
                // Prefer Wikipedia prose via the entity's
                // enwiki sitelink over Wikidata's one-line
                // description. Wikidata's `description` is
                // designed to disambiguate on the entity list
                // ("English rock band" / "German composer") and
                // is not a bio — the UI needs actual prose.
                // The enwiki sitelink is the canonical way to
                // walk from a Wikidata entity to its Wikipedia
                // article regardless of whether MB routed a
                // `wikipedia` url-rel (which is the case that
                // pushed us into this Wikidata branch in the
                // first place).
                if let (Some(title), Some(wp)) = (
                    entity_hit.enwiki_title.as_ref(),
                    catalogue.wikipedia.as_ref(),
                ) {
                    if catalogue
                        .config
                        .is_effectively_enabled(ProviderId::Wikipedia)
                    {
                        match wp.get_summary_en(title).await {
                            Ok(Some(summary)) => {
                                let payload = serde_json::json!({
                                    "title": summary.title,
                                    "summary": summary.extract,
                                    "language": summary.language,
                                    "source_url": summary.page_url,
                                });
                                // Cache under the Wikipedia
                                // namespace so a repeat request
                                // whose Wikipedia branch runs
                                // first finds this on cache.
                                let wp_key = bio_cache_key(
                                    &entity,
                                    resolved_mbid.as_deref(),
                                    ProviderId::Wikipedia,
                                );
                                let _ = cache.put_positive(
                                    &wp_key,
                                    payload.clone(),
                                    "wikipedia",
                                );
                                let enhancement = enhancement_hint_for_bio(
                                    catalogue,
                                    ProviderId::Wikipedia,
                                );
                                return Ok(CascadeResponse {
                                    v: 1,
                                    status: CascadeStatus::Ok,
                                    provider_id: Some(
                                        ProviderId::Wikipedia
                                            .as_str()
                                            .to_string(),
                                    ),
                                    privacy_class: Some(
                                        PrivacyClass::Anonymous
                                            .as_str()
                                            .to_string(),
                                    ),
                                    payload: Some(payload),
                                    detail: None,
                                    attribution: Some(Attribution {
                                        source_name: "Wikipedia".into(),
                                        source_url: Some(summary.page_url),
                                        license: "CC BY-SA".into(),
                                    }),
                                    enhancement,
                                });
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(
                                    plugin = crate::PLUGIN_NAME,
                                    provider = "wikipedia",
                                    entity = %entity.name,
                                    enwiki_title = %title,
                                    error = %e,
                                    "Wikipedia summary via Wikidata \
                                     enwiki sitelink transient / \
                                     not-usable; falling back to \
                                     Wikidata description"
                                );
                            }
                        }
                    }
                }
                // Fallback: no enwiki sitelink, Wikipedia is
                // disabled, or the Wikipedia fetch failed —
                // return Wikidata's one-line description as
                // honest what-we-have content with CC0
                // attribution.
                let payload = serde_json::json!({
                    "label": entity_hit.label_en,
                    "description": entity_hit.description_en,
                    "date_of_birth": entity_hit.date_of_birth,
                    "date_of_death": entity_hit.date_of_death,
                    "inception": entity_hit.inception,
                    "dissolution": entity_hit.dissolution,
                    "source_url": entity_hit.entity_url,
                });
                let _ = cache.put_positive(&key, payload.clone(), "wikidata");
                let enhancement =
                    enhancement_hint_for_bio(catalogue, ProviderId::Wikidata);
                return Ok(CascadeResponse {
                    v: 1,
                    status: CascadeStatus::Ok,
                    provider_id: Some(
                        ProviderId::Wikidata.as_str().to_string(),
                    ),
                    privacy_class: Some(
                        PrivacyClass::Anonymous.as_str().to_string(),
                    ),
                    payload: Some(payload),
                    detail: None,
                    attribution: Some(Attribution {
                        source_name: "Wikidata".into(),
                        source_url: Some(entity_hit.entity_url),
                        license: "CC0".into(),
                    }),
                    enhancement,
                });
            }
        }
    }

    // Identity-bearing enhancement: Last.fm — richer editorial
    // bio when the operator has enabled it AND provided a key.
    // Only fired for artist-type entities; Last.fm has poor
    // classical (composer / work / performer) coverage.
    if want_lastfm && matches!(entity.entity_type, EntityType::Artist) {
        if let Some(lastfm) = catalogue.lastfm.as_ref() {
            last_provider = Some(ProviderId::Lastfm);
            match lastfm
                .get_artist_bio(&entity.name, entity.mbid.as_deref())
                .await
            {
                Ok(Some(h)) => {
                    let payload = serde_json::json!({
                        "summary": h.summary,
                        "content": h.content,
                        "source_url": h.source_url,
                    });
                    let key = EnrichmentCache::key_for(&[
                        "entity_bio",
                        entity.entity_type.as_str(),
                        &normalise(&entity.name),
                        ProviderId::Lastfm.as_str(),
                    ]);
                    let _ = cache.put_positive(&key, payload.clone(), "lastfm");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Lastfm.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::IdentityBearing.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Last.fm".into(),
                            source_url: h.source_url.clone(),
                            license: "Last.fm terms of use".into(),
                        }),
                        enhancement: None,
                    });
                }
                Ok(None) => {}
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
                }
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "lastfm",
                        entity = %entity.name,
                        error = %e,
                        "Last.fm transient; skipping"
                    );
                }
            }
        }
    }

    // All enabled providers structurally missed. Return
    // not_found with an enhancement hint pointing at the
    // best identity-bearing provider the operator could enable
    // next.
    let detail = format!(
        "no bio found for {}={} across enabled providers",
        entity.entity_type.as_str(),
        entity.name
    );
    let enhancement = enhancement_hint_for_bio_missing(catalogue);
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        enhancement,
    })
}

/// Rebuild a cascade response from a cached payload. Cache-hit
/// responses MUST be indistinguishable from fresh-hit responses on
/// every field UI renders — provider_id, privacy_class,
/// attribution (including source_url), and enhancement. Two
/// invariants this helper enforces:
///
/// 1. **source_url is recovered from the cached payload**, not
///    hardcoded `None`. CC BY-SA requires the attribution link on
///    every rendered payload; the fresh path persists it into the
///    cached payload, and this rebuilder pulls it back. The
///    caller may override via `source_url_override` when the
///    canonical URL is derivable from other cached fields (e.g.
///    MB release URL from the cached `release_mbid`).
/// 2. **enhancement is recomputed via the verb-specific hint
///    closure**, not dropped. The hint depends on the current
///    catalogue state (which providers are enabled, which have
///    credentials in the vault), so it MUST be computed at
///    dispatch time, not baked into the cached bytes.
fn cascade_ok_from_cache(
    payload: serde_json::Value,
    provider: ProviderId,
    source_url_override: Option<String>,
    enhancement: Option<EnhancementHint>,
) -> CascadeResponse {
    let source_url = source_url_override.or_else(|| {
        payload
            .get("source_url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    });
    let (source_name, license) = match provider {
        ProviderId::MusicBrainz => ("MusicBrainz", "CC0"),
        ProviderId::Wikipedia => ("Wikipedia", "CC BY-SA"),
        ProviderId::Wikidata => ("Wikidata", "CC0"),
        ProviderId::Lrclib => ("LRCLIB", "Public domain"),
        ProviderId::Lastfm => ("Last.fm", "Last.fm terms of use"),
        ProviderId::Discogs => ("Discogs", "Discogs terms of use"),
        ProviderId::Genius => ("Genius", "Genius terms of use"),
    };
    CascadeResponse {
        v: 1,
        status: CascadeStatus::Ok,
        provider_id: Some(provider.as_str().to_string()),
        privacy_class: Some(provider.privacy_class().as_str().to_string()),
        payload: Some(payload),
        detail: None,
        attribution: Some(Attribution {
            source_name: source_name.into(),
            source_url,
            license: license.into(),
        }),
        enhancement,
    }
}

fn enhancement_hint_for_bio(
    catalogue: &ProviderCatalogue,
    won: ProviderId,
) -> Option<EnhancementHint> {
    // Suggest Last.fm as bio enhancement only for artist requests
    // where the anonymous baseline won and Last.fm is either
    // disabled or unavailable.
    if won == ProviderId::Lastfm {
        return None;
    }
    if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key for richer editorial bios".into(),
        })
    } else if !catalogue.config.is_effectively_enabled(ProviderId::Lastfm) {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources \
                     for richer editorial bios"
                .into(),
        })
    } else {
        None
    }
}

fn enhancement_hint_for_bio_missing(
    catalogue: &ProviderCatalogue,
) -> Option<EnhancementHint> {
    // On a full miss with Last.fm available but disabled,
    // suggest enabling it. Otherwise silent.
    if catalogue.lastfm.is_some()
        && !catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
    {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources \
                     to try one more source"
                .into(),
        })
    } else if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key to try one more source".into(),
        })
    } else {
        None
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

    let mut last_provider: Option<ProviderId> = None;

    // Anonymous baseline: MusicBrainz. Resolves the release MBID
    // (via caller-supplied hint or `search_release`), then a full
    // release lookup delivers artist credits, labels + catalog #,
    // recording-level performer / conductor / composer relations,
    // and per-track work MBIDs. This is the canonical shape that
    // populates classical personnel — a Discogs-only response
    // cannot match this fidelity.
    if want_mb {
        if let Some(mb) = catalogue.musicbrainz.as_ref() {
            last_provider = Some(ProviderId::MusicBrainz);
            let key = EnrichmentCache::key_for(&[
                "release_credits",
                "release",
                &normalise(&artist),
                &normalise(&album),
                ProviderId::MusicBrainz.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        // Cached payload already carries the
                        // canonical `source_url` set on the fresh
                        // path; cascade_ok_from_cache recovers it.
                        // Enhancement hint is recomputed via the
                        // catalogue so a cached MB hit still
                        // surfaces the Discogs uplift.
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::MusicBrainz,
                            None,
                            enhancement_hint_for_release_credits(
                                catalogue,
                                ProviderId::MusicBrainz,
                            ),
                        ));
                    }
                }
            }
            let release_mbid: Option<String> = match &req.release_mbid {
                Some(m) if !m.trim().is_empty() => Some(m.trim().to_string()),
                _ => match mb.search_release(&artist, &album).await {
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
            if let Some(release_mbid) = release_mbid {
                match mb.lookup_release_full(&release_mbid).await {
                    Ok(rc) => {
                        let payload = release_credits_payload(&rc);
                        let _ = cache.put_positive(
                            &key,
                            payload.clone(),
                            "musicbrainz",
                        );
                        let attribution = Attribution {
                            source_name: "MusicBrainz".into(),
                            source_url: Some(format!(
                                "https://musicbrainz.org/release/{}",
                                rc.release_mbid
                            )),
                            license: "CC0".into(),
                        };
                        let enhancement = enhancement_hint_for_release_credits(
                            catalogue,
                            ProviderId::MusicBrainz,
                        );
                        return Ok(CascadeResponse {
                            v: 1,
                            status: CascadeStatus::Ok,
                            provider_id: Some(
                                ProviderId::MusicBrainz.as_str().to_string(),
                            ),
                            privacy_class: Some(
                                PrivacyClass::Anonymous.as_str().to_string(),
                            ),
                            payload: Some(payload),
                            detail: None,
                            attribution: Some(attribution),
                            enhancement,
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            plugin = crate::PLUGIN_NAME,
                            provider = "musicbrainz",
                            release_mbid,
                            error = %e,
                            "MB full-release lookup transient; skipping"
                        );
                    }
                }
            }
        }
    }

    // Identity-bearing enhancement: Discogs release detail.
    // Delivers pressing + label + catalog + notes at higher
    // fidelity than MB when the operator has enabled Discogs and
    // provided a token. Cache is keyed separately so a disabled
    // Discogs never suppresses a cached MB result and vice versa.
    if want_discogs {
        if let Some(discogs) = catalogue.discogs.as_ref() {
            last_provider = Some(ProviderId::Discogs);
            let key = EnrichmentCache::key_for(&[
                "release_credits",
                "release",
                &normalise(&artist),
                &normalise(&album),
                ProviderId::Discogs.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Discogs,
                            None,
                            enhancement_hint_for_release_credits(
                                catalogue,
                                ProviderId::Discogs,
                            ),
                        ));
                    }
                }
            }
            match discogs.get_release_detail(&artist, &album).await {
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
                    let _ =
                        cache.put_positive(&key, payload.clone(), "discogs");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Discogs.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::IdentityBearing.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Discogs".into(),
                            source_url: h.source_url.clone(),
                            license: "Discogs terms of use".into(),
                        }),
                        enhancement: enhancement_hint_for_release_credits(
                            catalogue,
                            ProviderId::Discogs,
                        ),
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    // Transient — never cache; skip Discogs.
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "discogs",
                        artist,
                        album,
                        error = %discogs_error_message(&e),
                        "Discogs release detail transient; skipping"
                    );
                }
            }
        }
    }

    // Every enabled provider structurally missed.
    let detail = format!("no release-credits found for {artist} — {album}");
    let enhancement = enhancement_hint_for_release_credits_missing(catalogue);
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        enhancement,
    })
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

fn enhancement_hint_for_release_credits(
    catalogue: &ProviderCatalogue,
    won: ProviderId,
) -> Option<EnhancementHint> {
    if won == ProviderId::Discogs {
        return None;
    }
    // MB won — suggest Discogs when it could enrich further.
    if catalogue.discogs.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Discogs.as_str().to_string(),
            requires_key: true,
            reason: "Add a Discogs Personal Access Token for pressing + \
                     personnel depth"
                .into(),
        })
    } else if !catalogue.config.is_effectively_enabled(ProviderId::Discogs) {
        Some(EnhancementHint {
            provider: ProviderId::Discogs.as_str().to_string(),
            requires_key: false,
            reason: "Enable Discogs under Settings → Metadata → Sources for \
                     pressing + personnel depth"
                .into(),
        })
    } else {
        None
    }
}

fn enhancement_hint_for_release_credits_missing(
    catalogue: &ProviderCatalogue,
) -> Option<EnhancementHint> {
    if catalogue.discogs.is_some()
        && !catalogue.config.is_effectively_enabled(ProviderId::Discogs)
    {
        Some(EnhancementHint {
            provider: ProviderId::Discogs.as_str().to_string(),
            requires_key: false,
            reason: "Enable Discogs under Settings → Metadata → Sources to \
                     try one more source"
                .into(),
        })
    } else if catalogue.discogs.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Discogs.as_str().to_string(),
            requires_key: true,
            reason: "Add a Discogs Personal Access Token to try one more \
                     source"
                .into(),
        })
    } else {
        None
    }
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

    let mut last_provider: Option<ProviderId> = None;

    // Anonymous baseline: Wikipedia song page. Songs rarely
    // carry their own Wikipedia article — the try-set below
    // exhausts the disambiguated forms `"{Track} ({Artist} song)"`
    // and `"{Track} (song)"` before falling back to the bare
    // title (which lands on the correct page for songs whose
    // title is unambiguous in Wikipedia's namespace).
    if want_wp {
        if let Some(wp) = catalogue.wikipedia.as_ref() {
            last_provider = Some(ProviderId::Wikipedia);
            let key = EnrichmentCache::key_for(&[
                "track_annotation",
                "song",
                &normalise(&artist),
                &normalise(&track),
                ProviderId::Wikipedia.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikipedia,
                            None,
                            enhancement_hint_for_track_annotation(
                                catalogue,
                                ProviderId::Wikipedia,
                            ),
                        ));
                    }
                }
            }
            let candidate_titles = [
                format!("{track} ({artist} song)"),
                format!("{track} (song)"),
                track.clone(),
            ];
            let mut wp_hit = None;
            for candidate in &candidate_titles {
                match wp.get_summary_en(candidate).await {
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
                            error = %e,
                            "Wikipedia song-title lookup transient / \
                             not-usable; trying next form"
                        );
                        continue;
                    }
                }
            }
            if let Some(summary) = wp_hit {
                let payload = serde_json::json!({
                    "title": summary.title,
                    "summary": summary.extract,
                    "language": summary.language,
                    "source_url": summary.page_url,
                });
                let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
                let enhancement = enhancement_hint_for_track_annotation(
                    catalogue,
                    ProviderId::Wikipedia,
                );
                return Ok(CascadeResponse {
                    v: 1,
                    status: CascadeStatus::Ok,
                    provider_id: Some(
                        ProviderId::Wikipedia.as_str().to_string(),
                    ),
                    privacy_class: Some(
                        PrivacyClass::Anonymous.as_str().to_string(),
                    ),
                    payload: Some(payload),
                    detail: None,
                    attribution: Some(Attribution {
                        source_name: "Wikipedia".into(),
                        source_url: Some(summary.page_url),
                        license: "CC BY-SA".into(),
                    }),
                    enhancement,
                });
            }
        }
    }

    // Identity-bearing enhancement: Genius description.
    if want_genius {
        if let Some(genius) = catalogue.genius.as_ref() {
            last_provider = Some(ProviderId::Genius);
            let key = EnrichmentCache::key_for(&[
                "track_annotation",
                "song",
                &normalise(&artist),
                &normalise(&track),
                ProviderId::Genius.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Genius,
                            None,
                            enhancement_hint_for_track_annotation(
                                catalogue,
                                ProviderId::Genius,
                            ),
                        ));
                    }
                }
            }
            match genius.get_track_annotation(&artist, &track).await {
                Ok(Some(h)) => {
                    let payload = serde_json::json!({
                        "song_id": h.song_id,
                        "description": h.description,
                        "source_url": h.source_url,
                    });
                    let _ = cache.put_positive(&key, payload.clone(), "genius");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Genius.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::IdentityBearing.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Genius".into(),
                            source_url: h.source_url.clone(),
                            license: "Genius terms of use".into(),
                        }),
                        enhancement: enhancement_hint_for_track_annotation(
                            catalogue,
                            ProviderId::Genius,
                        ),
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "genius",
                        artist,
                        track,
                        error = %genius_error_message(&e),
                        "Genius track annotation transient; skipping"
                    );
                }
            }
        }
    }

    let detail = format!(
        "no track annotation found for {artist} — {track} across \
         enabled providers"
    );
    let enhancement = enhancement_hint_for_track_annotation_missing(catalogue);
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        enhancement,
    })
}

fn enhancement_hint_for_track_annotation(
    catalogue: &ProviderCatalogue,
    won: ProviderId,
) -> Option<EnhancementHint> {
    if won == ProviderId::Genius {
        return None;
    }
    if catalogue.genius.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Genius.as_str().to_string(),
            requires_key: true,
            reason: "Add a Genius API access token for song annotations".into(),
        })
    } else if !catalogue.config.is_effectively_enabled(ProviderId::Genius) {
        Some(EnhancementHint {
            provider: ProviderId::Genius.as_str().to_string(),
            requires_key: false,
            reason: "Enable Genius under Settings → Metadata → Sources for \
                     song annotations"
                .into(),
        })
    } else {
        None
    }
}

fn enhancement_hint_for_track_annotation_missing(
    catalogue: &ProviderCatalogue,
) -> Option<EnhancementHint> {
    if catalogue.genius.is_some()
        && !catalogue.config.is_effectively_enabled(ProviderId::Genius)
    {
        Some(EnhancementHint {
            provider: ProviderId::Genius.as_str().to_string(),
            requires_key: false,
            reason: "Enable Genius under Settings → Metadata → Sources to \
                     try one more source"
                .into(),
        })
    } else if catalogue.genius.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Genius.as_str().to_string(),
            requires_key: true,
            reason: "Add a Genius API access token to try one more source"
                .into(),
        })
    } else {
        None
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

    let want_wp = catalogue
        .config
        .is_effectively_enabled(ProviderId::Wikipedia)
        && catalogue.wikipedia.is_some();
    let want_lastfm =
        catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
            && catalogue.lastfm.is_some();

    if !(want_wp || want_lastfm) {
        return Ok(CascadeResponse::not_configured(
            "every album-notes provider is disabled or unavailable on this \
             device; enable at least one under Settings → Metadata → \
             Sources",
        ));
    }

    let mut last_provider: Option<ProviderId> = None;

    // Anonymous baseline: Wikipedia album page via title-search
    // variants. Wikipedia's page naming convention for album
    // articles is `"{Album} ({Artist} album)"` or
    // `"{Album} (album)"` for disambiguation, then the bare
    // title for unambiguous titles. Exhaust all three in order.
    if want_wp {
        if let Some(wp) = catalogue.wikipedia.as_ref() {
            last_provider = Some(ProviderId::Wikipedia);
            let key = EnrichmentCache::key_for(&[
                "album_notes",
                "album",
                &normalise(&artist),
                &normalise(&album),
                ProviderId::Wikipedia.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikipedia,
                            None,
                            enhancement_hint_for_album_notes(
                                catalogue,
                                ProviderId::Wikipedia,
                            ),
                        ));
                    }
                }
            }
            let candidate_titles = [
                format!("{album} ({artist} album)"),
                format!("{album} (album)"),
                album.clone(),
            ];
            let mut wp_hit = None;
            for candidate in &candidate_titles {
                match wp.get_summary_en(candidate).await {
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
                            error = %e,
                            "Wikipedia album-title lookup transient / \
                             not-usable; trying next form"
                        );
                        continue;
                    }
                }
            }
            if let Some(summary) = wp_hit {
                let payload = serde_json::json!({
                    "title": summary.title,
                    "summary": summary.extract,
                    "language": summary.language,
                    "source_url": summary.page_url,
                });
                let _ = cache.put_positive(&key, payload.clone(), "wikipedia");
                let enhancement = enhancement_hint_for_album_notes(
                    catalogue,
                    ProviderId::Wikipedia,
                );
                return Ok(CascadeResponse {
                    v: 1,
                    status: CascadeStatus::Ok,
                    provider_id: Some(
                        ProviderId::Wikipedia.as_str().to_string(),
                    ),
                    privacy_class: Some(
                        PrivacyClass::Anonymous.as_str().to_string(),
                    ),
                    payload: Some(payload),
                    detail: None,
                    attribution: Some(Attribution {
                        source_name: "Wikipedia".into(),
                        source_url: Some(summary.page_url),
                        license: "CC BY-SA".into(),
                    }),
                    enhancement,
                });
            }
        }
    }

    // Identity-bearing enhancement: Last.fm album.getinfo wiki.
    if want_lastfm {
        if let Some(lastfm) = catalogue.lastfm.as_ref() {
            last_provider = Some(ProviderId::Lastfm);
            let key = EnrichmentCache::key_for(&[
                "album_notes",
                "album",
                &normalise(&artist),
                &normalise(&album),
                ProviderId::Lastfm.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Lastfm,
                            None,
                            enhancement_hint_for_album_notes(
                                catalogue,
                                ProviderId::Lastfm,
                            ),
                        ));
                    }
                }
            }
            match lastfm
                .get_album_notes(&artist, &album, req.release_mbid.as_deref())
                .await
            {
                Ok(Some(h)) => {
                    let payload = serde_json::json!({
                        "summary": h.summary,
                        "content": h.content,
                        "source_url": h.source_url,
                    });
                    let _ = cache.put_positive(&key, payload.clone(), "lastfm");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Lastfm.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::IdentityBearing.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Last.fm".into(),
                            source_url: h.source_url.clone(),
                            license: "Last.fm terms of use".into(),
                        }),
                        enhancement: enhancement_hint_for_album_notes(
                            catalogue,
                            ProviderId::Lastfm,
                        ),
                    });
                }
                Ok(None) => {}
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
                }
            }
        }
    }

    let detail = format!(
        "no album notes found for {artist} — {album} across enabled providers"
    );
    let enhancement = enhancement_hint_for_album_notes_missing(catalogue);
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        enhancement,
    })
}

fn enhancement_hint_for_album_notes(
    catalogue: &ProviderCatalogue,
    won: ProviderId,
) -> Option<EnhancementHint> {
    if won == ProviderId::Lastfm {
        return None;
    }
    if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key for richer album notes".into(),
        })
    } else if !catalogue.config.is_effectively_enabled(ProviderId::Lastfm) {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources for \
                     richer album notes"
                .into(),
        })
    } else {
        None
    }
}

fn enhancement_hint_for_album_notes_missing(
    catalogue: &ProviderCatalogue,
) -> Option<EnhancementHint> {
    if catalogue.lastfm.is_some()
        && !catalogue.config.is_effectively_enabled(ProviderId::Lastfm)
    {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: false,
            reason: "Enable Last.fm under Settings → Metadata → Sources to \
                     try one more source"
                .into(),
        })
    } else if catalogue.lastfm.is_none() {
        Some(EnhancementHint {
            provider: ProviderId::Lastfm.as_str().to_string(),
            requires_key: true,
            reason: "Add a Last.fm API key to try one more source".into(),
        })
    } else {
        None
    }
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

    let mut last_provider: Option<ProviderId> = None;

    // Anonymous baseline: MusicBrainz work lookup. Yields the
    // Wikipedia + Wikidata URLs the downstream providers consume
    // directly. When the caller supplied `work_mbid` the search
    // step is skipped.
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
            last_provider = Some(ProviderId::MusicBrainz);
            if resolved_work_mbid.is_none() {
                if let Some(name) = work_name.as_deref() {
                    let composer_hint = req
                        .composer
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty());
                    match mb.search_work(name, composer_hint).await {
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

    // Wikipedia work-page summary — primary anonymous content.
    if want_wp {
        if let Some(wp) = catalogue.wikipedia.as_ref() {
            last_provider = Some(ProviderId::Wikipedia);
            let cache_name = canonical_title
                .clone()
                .or_else(|| work_name.clone())
                .unwrap_or_default();
            let key = EnrichmentCache::key_for(&[
                "work_notes",
                "work",
                &normalise(&cache_name),
                ProviderId::Wikipedia.as_str(),
            ]);
            if let Some(entry) = cache.get(&key) {
                if entry.status == "ok" {
                    if let Some(p) = entry.payload {
                        // work_notes has no identity-bearing
                        // enhancement — Wikipedia is authoritative
                        // for classical works. Explicit `None` so
                        // the intent is on the wire.
                        return Ok(cascade_ok_from_cache(
                            p,
                            ProviderId::Wikipedia,
                            None,
                            None,
                        ));
                    }
                }
            }
            let hit = match &wikipedia_url {
                Some(url) => wp.get_summary_from_url(url).await,
                None => match &canonical_title {
                    Some(title) => wp.get_summary_en(title).await,
                    None => match &work_name {
                        Some(name) => wp.get_summary_en(name).await,
                        None => Ok(None),
                    },
                },
            };
            match hit {
                Ok(Some(summary)) => {
                    let payload = serde_json::json!({
                        "title": summary.title,
                        "summary": summary.extract,
                        "language": summary.language,
                        "work_mbid": resolved_work_mbid,
                        "work_type": work_type,
                        "source_url": summary.page_url,
                    });
                    let _ =
                        cache.put_positive(&key, payload.clone(), "wikipedia");
                    return Ok(CascadeResponse {
                        v: 1,
                        status: CascadeStatus::Ok,
                        provider_id: Some(
                            ProviderId::Wikipedia.as_str().to_string(),
                        ),
                        privacy_class: Some(
                            PrivacyClass::Anonymous.as_str().to_string(),
                        ),
                        payload: Some(payload),
                        detail: None,
                        attribution: Some(Attribution {
                            source_name: "Wikipedia".into(),
                            source_url: Some(summary.page_url),
                            license: "CC BY-SA".into(),
                        }),
                        enhancement: None,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "wikipedia",
                        work = %cache_name,
                        error = %e,
                        "Wikipedia work-summary transient / not-usable; \
                         skipping"
                    );
                }
            }
        }
    }

    // Wikidata — structured work facts (composer, inception,
    // genre) as a secondary anonymous fallback when Wikipedia
    // has no summary but MB routed a wikidata_url.
    if want_wd {
        if let Some(wd) = catalogue.wikidata.as_ref() {
            last_provider = Some(ProviderId::Wikidata);
            let hit = match &wikidata_url {
                Some(url) => wd.get_entity_from_url(url).await,
                None => Ok(None),
            };
            if let Ok(Some(entity_hit)) = hit {
                let payload = serde_json::json!({
                    "label": entity_hit.label_en,
                    "description": entity_hit.description_en,
                    "inception": entity_hit.inception,
                    "genre_ids": entity_hit.genre_ids,
                    "work_mbid": resolved_work_mbid,
                    "work_type": work_type,
                    "source_url": entity_hit.entity_url,
                });
                let cache_name = canonical_title
                    .clone()
                    .or_else(|| work_name.clone())
                    .unwrap_or_default();
                let key = EnrichmentCache::key_for(&[
                    "work_notes",
                    "work",
                    &normalise(&cache_name),
                    ProviderId::Wikidata.as_str(),
                ]);
                let _ = cache.put_positive(&key, payload.clone(), "wikidata");
                return Ok(CascadeResponse {
                    v: 1,
                    status: CascadeStatus::Ok,
                    provider_id: Some(
                        ProviderId::Wikidata.as_str().to_string(),
                    ),
                    privacy_class: Some(
                        PrivacyClass::Anonymous.as_str().to_string(),
                    ),
                    payload: Some(payload),
                    detail: None,
                    attribution: Some(Attribution {
                        source_name: "Wikidata".into(),
                        source_url: Some(entity_hit.entity_url),
                        license: "CC0".into(),
                    }),
                    enhancement: None,
                });
            }
        }
    }

    let detail = format!(
        "no work notes found for {} across enabled providers",
        canonical_title
            .clone()
            .or_else(|| work_name.clone())
            .unwrap_or_else(|| "the requested work".to_string())
    );
    Ok(CascadeResponse {
        v: 1,
        status: CascadeStatus::NotFound,
        provider_id: last_provider.map(|p| p.as_str().to_string()),
        privacy_class: last_provider
            .map(|p| p.privacy_class().as_str().to_string()),
        payload: None,
        detail: Some(detail),
        attribution: None,
        // No identity-bearing enhancement for work notes.
        enhancement: None,
    })
}
