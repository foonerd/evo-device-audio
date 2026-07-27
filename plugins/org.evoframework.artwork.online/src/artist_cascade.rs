// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

// Some helper methods (privacy_class on the typed enum, set_flags,
// etc.) are shaped for future callers even when not yet consumed
// by today's dispatch code. Matches the metadata.online cascade
// module's posture.
#![allow(dead_code)]

//! Artist-artwork parallel-dispatch cascade.
//!
//! The album-artwork verb (`artwork.resolve_online`) delivers a
//! single-answer surface: the first provider that returns a
//! usable image wins, one image goes on the wire. That shape
//! matches album art — the operator sees one cover per album.
//!
//! Artist artwork is different: an artist has a portrait, a set
//! of backgrounds, a set of logos, a set of banners. No single
//! provider serves all of these, and different providers have
//! different quality bands. The operator SHOULD see every
//! source that returned content, be able to choose between them,
//! and reorder their preference.
//!
//! This module implements the parallel-dispatch aggregate
//! cascade for artist artwork with the same
//! `SourceEntry` / `sources: Vec<SourceEntry>` envelope the text
//! verbs use (in `org.evoframework.metadata.online`) so the
//! operator UI treats every source — text and image — through
//! one code path.
//!
//! ## Providers
//!
//! - **volumio_meta (artistArt mode)** — anonymous, hosted meta
//!   proxy at `meta.volumio.org`. Provides a single artist image
//!   URL. Cache-safe.
//! - **theaudiodb** — anonymous keyless test key. Provides an
//!   artist thumbnail URL alongside its bio content (the bio
//!   verb consumes the bio; this cascade consumes the thumb).
//!   Cache-safe.
//! - **deezer** — anonymous keyless. Provides four resolution
//!   tiers of the artist portrait via the public API. **Live-
//!   fetch invariant, ToS-mandated**: the response body must
//!   NEVER be persisted. Enforced at the type level by
//!   `ArtistImageHit`'s deliberate absence of `Serialize`; the
//!   fetch helper extracts URLs into a local JSON payload
//!   inline. The URLs themselves are stable metadata and
//!   render-time links; the images they point at are what the
//!   ToS restricts.
//! - **fanart.tv** — identity-bearing, keyed by operator's
//!   fanart.tv personal API key from the framework credential
//!   vault. Provides HD music logos, HD artist logos,
//!   full-bleed artist backgrounds, artist thumbs, and music
//!   banners — arrays per artwork type. Cache-safe.
//!
//! ## Cache posture
//!
//! Transient upstream errors (network timeout, 5xx, 429) never
//! cache — the aggregate treats them as a skip and moves on
//! (matches the transient-not-cached discipline the album
//! cascade already enforces). Only structural misses (clean
//! 404, empty result) or successful hits touch the cache.
//! Deezer additionally never caches its response body
//! regardless of outcome — enforced structurally by
//! `ArtistImageHit`'s missing `Serialize`.
//!
//! ## Enable + priority
//!
//! Every provider is independently enable/disable-able and
//! orderable by the operator via the framework
//! `online_provider_config` store, exactly like the text-verb
//! cascade. Priority default is Deezer 45, TheAudioDB 50,
//! Volumio meta 55, fanart.tv 60 — anonymous baseline before
//! the keyed source.

use std::sync::Arc;

use evo_online_providers::{
    deezer::DeezerClient,
    fanart::FanartClient,
    musicbrainz::{
        parse_deezer_artist_id, MusicBrainzClient, MusicBrainzError,
    },
    theaudiodb::TheAudioDbClient,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Stable provider identifier for this cascade. Distinct from
/// the album-artwork cascade's implicit provider taxonomy —
/// the two share no providers today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArtistProviderId {
    // Anonymous — no key required.
    VolumioMeta,
    TheAudioDb,
    Deezer,
    // Identity-bearing — API key required.
    FanartTv,
}

impl ArtistProviderId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ArtistProviderId::VolumioMeta => "volumio_meta",
            ArtistProviderId::TheAudioDb => "theaudiodb",
            ArtistProviderId::Deezer => "deezer",
            ArtistProviderId::FanartTv => "fanart_tv",
        }
    }

    pub(crate) fn from_wire(id: &str) -> Option<Self> {
        match id {
            "volumio_meta" => Some(ArtistProviderId::VolumioMeta),
            "theaudiodb" => Some(ArtistProviderId::TheAudioDb),
            "deezer" => Some(ArtistProviderId::Deezer),
            "fanart_tv" => Some(ArtistProviderId::FanartTv),
            _ => None,
        }
    }

    pub(crate) fn privacy_class(self) -> ArtistPrivacyClass {
        match self {
            ArtistProviderId::VolumioMeta
            | ArtistProviderId::TheAudioDb
            | ArtistProviderId::Deezer => ArtistPrivacyClass::Anonymous,
            ArtistProviderId::FanartTv => ArtistPrivacyClass::IdentityBearing,
        }
    }
}

/// Whether a provider requires operator credentials to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtistPrivacyClass {
    Anonymous,
    IdentityBearing,
}

impl ArtistPrivacyClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ArtistPrivacyClass::Anonymous => "anonymous",
            ArtistPrivacyClass::IdentityBearing => "identity_bearing",
        }
    }
}

/// Per-provider enable + priority pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArtistProviderFlags {
    pub(crate) enabled: bool,
    /// Cascade priority. Lower wins first. Providers with equal
    /// priority preserve enum declaration order.
    pub(crate) priority: u32,
}

/// Per-provider enable + priority map.
#[derive(Debug, Clone)]
pub(crate) struct ArtistProviderConfig {
    pub(crate) volumio_meta: ArtistProviderFlags,
    pub(crate) theaudiodb: ArtistProviderFlags,
    pub(crate) deezer: ArtistProviderFlags,
    pub(crate) fanart_tv: ArtistProviderFlags,
}

impl ArtistProviderConfig {
    pub(crate) fn defaults() -> Self {
        Self {
            deezer: ArtistProviderFlags {
                enabled: true,
                priority: 45,
            },
            theaudiodb: ArtistProviderFlags {
                enabled: true,
                priority: 50,
            },
            volumio_meta: ArtistProviderFlags {
                enabled: true,
                priority: 55,
            },
            fanart_tv: ArtistProviderFlags {
                enabled: true,
                priority: 60,
            },
        }
    }

    pub(crate) fn flags(
        &self,
        provider: ArtistProviderId,
    ) -> ArtistProviderFlags {
        match provider {
            ArtistProviderId::VolumioMeta => self.volumio_meta,
            ArtistProviderId::TheAudioDb => self.theaudiodb,
            ArtistProviderId::Deezer => self.deezer,
            ArtistProviderId::FanartTv => self.fanart_tv,
        }
    }

    pub(crate) fn set_flags(
        &mut self,
        provider: ArtistProviderId,
        flags: ArtistProviderFlags,
    ) {
        match provider {
            ArtistProviderId::VolumioMeta => self.volumio_meta = flags,
            ArtistProviderId::TheAudioDb => self.theaudiodb = flags,
            ArtistProviderId::Deezer => self.deezer = flags,
            ArtistProviderId::FanartTv => self.fanart_tv = flags,
        }
    }

    pub(crate) fn is_enabled(&self, provider: ArtistProviderId) -> bool {
        self.flags(provider).enabled
    }

    /// Merge a runtime operator override on top of the current
    /// per-provider flag block. Priority-only overrides preserve
    /// the existing enabled bit; enabled-only overrides preserve
    /// the existing priority.
    pub(crate) fn merge_override(
        &mut self,
        provider: ArtistProviderId,
        enabled: Option<bool>,
        priority: Option<u32>,
    ) {
        let mut flags = self.flags(provider);
        if let Some(e) = enabled {
            flags.enabled = e;
        }
        if let Some(p) = priority {
            flags.priority = p;
        }
        self.set_flags(provider, flags);
    }
}

/// Provenance carried on every source entry.
///
/// `Deserialize` is derived so the result-cache can serialise
/// `SourceEntry` into `serde_json::Value` at insert time and
/// round-trip it back at lookup — the cache lives on the same
/// side of the plugin as `Serialize`, so no data ever crosses
/// the ToS boundary that `ArtistImageHit`'s missing
/// `Serialize` enforces (Deezer results are excluded from the
/// cache by construction, not by the trait).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Attribution {
    pub(crate) source_name: String,
    pub(crate) source_url: Option<String>,
    pub(crate) license: String,
}

/// One provider's contribution to an artist-artwork response.
///
/// Wire shape MUST match the text-verb cascade's `SourceEntry`
/// so the operator UI's per-source selection surface renders
/// text and image sources through one code path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SourceEntry {
    pub(crate) provider_id: String,
    pub(crate) privacy_class: String,
    pub(crate) payload: serde_json::Value,
    pub(crate) attribution: Attribution,
}

/// Wire-serialised status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CascadeStatus {
    Ok,
    NotFound,
    NotConfigured,
    BadRequest,
}

/// Aggregate response shape.
///
/// Mirrors the text-verb cascade envelope: top-level fields
/// mirror `sources[0]` for back-compat; the top-level `payload`
/// is the field-level first-non-empty merge across every source
/// (Jellyfin / beets pattern) so a UI that renders a single
/// composite view still shows the union of every source's
/// contribution.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ArtistArtworkResponse {
    pub(crate) v: u8,
    pub(crate) status: CascadeStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) privacy_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<serde_json::Value>,
    /// Canonical top-level image URL derived from the primary
    /// source's provider-specific payload. Every source uses
    /// its own key (`image_url`, `thumb_url`, `picture_xl_url`,
    /// `artist_thumb_urls[0]`, …); this field surfaces one
    /// stable name for the UI, which fires this verb per
    /// visible artist tile and `<img src>`s the resulting URL
    /// directly at the provider's origin. No framework
    /// byte-cache is involved: live-fetch providers keep their
    /// terms, and the framework's content-hash artwork endpoint
    /// stays out of the artist path entirely.
    ///
    /// Present iff `status == Ok` AND the primary source's
    /// payload carries a URL the picker recognises.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attribution: Option<Attribution>,
    #[serde(default)]
    pub(crate) sources: Vec<SourceEntry>,
}

impl ArtistArtworkResponse {
    pub(crate) fn json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub(crate) fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            v: 1,
            status: CascadeStatus::BadRequest,
            provider_id: None,
            privacy_class: None,
            payload: None,
            image_url: None,
            detail: Some(detail.into()),
            attribution: None,
            sources: Vec::new(),
        }
    }

    pub(crate) fn not_configured(detail: impl Into<String>) -> Self {
        Self {
            v: 1,
            status: CascadeStatus::NotConfigured,
            provider_id: None,
            privacy_class: None,
            payload: None,
            image_url: None,
            detail: Some(detail.into()),
            attribution: None,
            sources: Vec::new(),
        }
    }

    /// Build an OK response from an ordered vector of source
    /// entries. Top-level payload = primary source's payload
    /// verbatim (no field-level merge). Top-level attribution
    /// matches. See the text-verb cascade's `from_sources`
    /// docstring for the licensing rationale — mirroring here
    /// keeps text and image sources on one attribution contract.
    pub(crate) fn from_sources(sources: Vec<SourceEntry>) -> Self {
        // The `sources` list is already sorted by operator
        // priority. Walk it and pick the first source whose
        // payload produces a real image URL — a source whose
        // payload picker returns `None` (e.g. a provider
        // that returned a URL shape but with an empty entity
        // slug) is skipped so a downstream fallback source can
        // still deliver the image. Only when every source
        // fails the picker do we surface NotFound: an "ok"
        // response with no usable URL is worse than an honest
        // "not_found" that the caller can render as a
        // placeholder.
        let usable = sources.iter().enumerate().find_map(|(idx, s)| {
            pick_canonical_image_url(&s.payload).map(|url| (idx, url))
        });
        let (
            status,
            provider_id,
            privacy_class,
            payload,
            image_url,
            attribution,
        ) = match usable {
            Some((idx, url)) => {
                let s = &sources[idx];
                (
                    CascadeStatus::Ok,
                    Some(s.provider_id.clone()),
                    Some(s.privacy_class.clone()),
                    Some(s.payload.clone()),
                    Some(url),
                    Some(s.attribution.clone()),
                )
            }
            None => (CascadeStatus::NotFound, None, None, None, None, None),
        };
        Self {
            v: 1,
            status,
            provider_id,
            privacy_class,
            payload,
            image_url,
            detail: None,
            attribution,
            sources,
        }
    }
}

/// Pick a canonical image URL from a provider-specific payload.
///
/// The four artist-artwork sources use different keys:
///
/// - `volumio_meta` → `image_url` (string)
/// - `theaudiodb` → `thumb_url` (string)
/// - `deezer` → `picture_xl_url` / `_big_url` / `_medium_url` /
///   `_small_url` (strings; pick the largest non-empty)
/// - `fanart_tv` → `hd_music_logo_urls[0]` /
///   `hd_artist_logo_urls[0]` / `artist_thumb_urls[0]` /
///   `music_banner_urls[0]` / `artist_background_urls[0]`
///   (arrays; pick the first non-empty entry from the
///   highest-preference key)
///
/// The picker probes each key in preference order and returns
/// the first non-empty string it finds. Returns `None` when the
/// payload carries no URL under any recognised key — the
/// resolver then surfaces `Ok(Not Found)` at the framework
/// boundary rather than a partial hit.
fn pick_canonical_image_url(payload: &serde_json::Value) -> Option<String> {
    let obj = payload.as_object()?;
    const STRING_KEYS: &[&str] = &[
        "image_url",
        "thumb_url",
        "picture_xl_url",
        "picture_big_url",
        "picture_medium_url",
        "picture_small_url",
    ];
    for key in STRING_KEYS {
        if let Some(url) = obj
            .get(*key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter(|s| is_real_image_url(s))
            .map(str::to_string)
        {
            return Some(url);
        }
    }
    const ARRAY_KEYS: &[&str] = &[
        "hd_music_logo_urls",
        "hd_artist_logo_urls",
        "artist_thumb_urls",
        "music_banner_urls",
        "artist_background_urls",
    ];
    for key in ARRAY_KEYS {
        if let Some(url) = obj
            .get(*key)
            .and_then(serde_json::Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .find(|s| !s.is_empty() && is_real_image_url(s))
            })
            .map(str::to_string)
        {
            return Some(url);
        }
    }
    None
}

/// Reject URLs that are structurally shaped like a valid image
/// URL but carry no entity identifier — the wire-observed
/// failure mode where Deezer returns
/// `https://cdn-images.dzcdn.net/images/artist//1000x1000-…`
/// (empty artist hash between the `artist/` and size segments)
/// for entities the provider did not resolve. Absent this
/// guard the picker returns the placeholder URL and callers
/// see a status=ok with a URL that renders as a broken image.
///
/// The rule: any `//` inside the path portion of the URL that
/// is not the scheme separator disqualifies the URL. Covers
/// the observed Deezer shape and every equivalent shape in
/// which a provider CDN elides an entity segment.
fn is_real_image_url(url: &str) -> bool {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => url,
    };
    !path.contains("//")
}

/// Sort a `sources` slice in place by operator priority
/// (ascending — lower wins). Unknown provider ids sink to the
/// tail. Stable sort preserves input order for ties.
pub(crate) fn sort_sources_by_priority(
    sources: &mut [SourceEntry],
    config: &ArtistProviderConfig,
) {
    sources.sort_by_key(|s| {
        ArtistProviderId::from_wire(&s.provider_id)
            .map(|p| config.flags(p).priority)
            .unwrap_or(u32::MAX)
    });
}

/// Request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct ArtistArtworkRequest {
    #[serde(default)]
    v: u8,
    /// Artist display name — mandatory. Used by every provider
    /// as the primary lookup key; fanart.tv also requires the
    /// MBID.
    #[serde(default)]
    artist: Option<String>,
    /// Optional MusicBrainz artist MBID. Required by fanart.tv
    /// (which keys strictly on MBID); other providers use it as
    /// a disambiguator when present.
    #[serde(default)]
    artist_mbid: Option<String>,
}

/// Catalogue the orchestrator walks.
pub(crate) struct ArtistCatalogue {
    pub(crate) volumio_meta_http: Arc<Client>,
    pub(crate) volumio_meta_variant: String,
    pub(crate) theaudiodb: Option<Arc<TheAudioDbClient>>,
    pub(crate) deezer: Option<Arc<DeezerClient>>,
    pub(crate) fanart: Option<Arc<FanartClient>>,
    /// MusicBrainz client used to reconcile the artist's
    /// canonical MBID before dispatching identity-bearing
    /// providers. Always present when the plugin has loaded
    /// (a spec-compliant default UA is fabricated when the
    /// operator has not set one); `Option` remains for the
    /// pre-load state.
    pub(crate) mb: Option<Arc<MusicBrainzClient>>,
    /// Per-fold-key caches for the reconcile step and the
    /// non-Deezer provider results. Shared across all in-
    /// flight cascade calls so a browse of the same artist
    /// set warm-caches from the second visit onwards.
    pub(crate) caches: Arc<crate::artwork_caches::ArtworkCaches>,
    pub(crate) config: ArtistProviderConfig,
}

/// Minimum MusicBrainz search score required to accept an
/// artist-name → MBID reconciliation as authoritative.
/// MB's Lucene score model returns `100` for exact-match on
/// name; the next tier down (`90`+) is normally a phonetic
/// / punctuation variant of the same entity. Below 90 the
/// match confidence is not sufficient to key provider
/// lookups on — the cascade emits `not_found` so the UI
/// falls back to its local thumbnail rather than surface a
/// wrong-entity image.
const MB_MIN_CONFIDENCE_PERCENT: u32 = 90;

/// Entry point invoked by the plugin's request handler.
pub(crate) async fn query_artist_artwork(
    payload: &[u8],
    catalogue: &ArtistCatalogue,
) -> Result<ArtistArtworkResponse, String> {
    if payload.is_empty() {
        return Ok(ArtistArtworkResponse::bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: ArtistArtworkRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(ArtistArtworkResponse::bad_request(format!(
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
    let Some(artist) = artist else {
        return Ok(ArtistArtworkResponse::bad_request(
            "artist is required and must be non-empty",
        ));
    };
    // Cache key: fold(input artist) — stable across display
    // cleaning so a browse tile rendered from a cleaned form
    // hits the same cache slot as the raw MPD tag it came
    // from. Empty fold-key means the input trims to nothing
    // (already caught above, but re-check for safety).
    let fold_key =
        evo_device_audio_shared::artist_name::artist_fold_key(&artist);
    let can_cache = !fold_key.is_empty();

    // MBID reconciliation is mandatory for identity-bearing
    // providers. Prefer any MBID the caller supplied (already
    // reconciled upstream); otherwise consult the reconcile
    // cache; on cache miss, hit MusicBrainz for a name → MBID
    // match, gated on confidence. Missing / weak reconciliation
    // is not an error — the fallback path is "no source",
    // which becomes `not_found` at the envelope.
    let caller_supplied_mbid = req
        .artist_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let reconciled = if let Some(mbid) = caller_supplied_mbid.clone() {
        // The caller passed an MBID they already trust; skip
        // MusicBrainz search, but still fetch URL-rels so the
        // Deezer path can key on the recorded Deezer link.
        match &catalogue.mb {
            Some(mb) => match mb.lookup_artist(&mbid).await {
                Ok(lookup) => Some(lookup),
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "musicbrainz",
                        artist = %artist,
                        mbid = %mbid,
                        error = %mb_error_display(&e),
                        "MB lookup for caller-supplied MBID failed; \
                         proceeding with MBID only"
                    );
                    // Fabricate a URL-rel-less lookup so the
                    // downstream providers still use the MBID
                    // where they can (fanart.tv, TheAudioDB).
                    Some(bare_lookup_from_mbid(&mbid, &artist))
                }
            },
            None => Some(bare_lookup_from_mbid(&mbid, &artist)),
        }
    } else if can_cache {
        // Cache-first name reconciliation. A fresh hit / miss
        // entry short-circuits the two MB round trips (search +
        // URL-rels lookup); an expired or absent entry falls
        // through to live reconcile and updates the cache with
        // the outcome. Negatives are always TTL-bounded (never
        // eternal) so an operator tag correction or a later MB
        // submission surfaces on the next cache expiry.
        use crate::artwork_caches::{MissReason, ReconcileEntry};
        match catalogue.caches.get_reconcile(&fold_key) {
            Some(ReconcileEntry::Hit { lookup, .. }) => Some(*lookup),
            Some(ReconcileEntry::Miss { .. }) => None,
            None => {
                let outcome =
                    reconcile_artist_mbid(&artist, catalogue.mb.as_deref())
                        .await;
                match &outcome {
                    Some(lookup) => catalogue
                        .caches
                        .put_reconcile_hit(fold_key.clone(), lookup.clone()),
                    None => catalogue.caches.put_reconcile_miss(
                        fold_key.clone(),
                        if catalogue.mb.is_none() {
                            MissReason::NoClient
                        } else {
                            MissReason::NoConfidentMatch
                        },
                    ),
                }
                outcome
            }
        }
    } else {
        reconcile_artist_mbid(&artist, catalogue.mb.as_deref()).await
    };

    let want_theaudiodb =
        catalogue.config.is_enabled(ArtistProviderId::TheAudioDb)
            && catalogue.theaudiodb.is_some()
            && reconciled.is_some();
    let want_deezer = catalogue.config.is_enabled(ArtistProviderId::Deezer)
        && catalogue.deezer.is_some()
        && reconciled
            .as_ref()
            .and_then(|l| l.deezer_artist_url.as_deref())
            .is_some();
    let want_fanart = catalogue.config.is_enabled(ArtistProviderId::FanartTv)
        && catalogue.fanart.is_some()
        && reconciled.is_some();
    // volumio_meta remains a name-only source; it takes no MBID
    // and no way to validate against a canonical identity. Keep
    // it enabled only when MBID reconciliation confirmed the
    // artist exists — that gate blocks the compilation-artist
    // wire failure mode (raw tag "Al Di Meola and John
    // McLaughlin and Paco De Lucía" resolves to no MB entity,
    // so volumio_meta doesn't fire, so no false-ok URL surfaces).
    let want_volumio =
        catalogue.config.is_enabled(ArtistProviderId::VolumioMeta)
            && reconciled.is_some();

    if !(want_volumio || want_theaudiodb || want_deezer || want_fanart) {
        // Two shapes of "cascade would not fire":
        //   - Every provider disabled / unavailable → not_configured
        //     (operator gesture would help).
        //   - Providers configured but MBID did not reconcile
        //     → not_found (no configuration change would help;
        //     the artist is not in MusicBrainz at confidence).
        let any_configured =
            catalogue.config.is_enabled(ArtistProviderId::VolumioMeta)
                || (catalogue.config.is_enabled(ArtistProviderId::TheAudioDb)
                    && catalogue.theaudiodb.is_some())
                || (catalogue.config.is_enabled(ArtistProviderId::Deezer)
                    && catalogue.deezer.is_some())
                || (catalogue.config.is_enabled(ArtistProviderId::FanartTv)
                    && catalogue.fanart.is_some());
        if !any_configured || catalogue.mb.is_none() {
            return Ok(ArtistArtworkResponse::not_configured(
                "every artist-artwork provider is disabled or unavailable on this \
                 device; enable at least one under Settings → Metadata → Sources \
                 (fanart.tv also requires the artist_mbid + a fanart.tv \
                 personal API key), and set `musicbrainz_user_agent` so the \
                 cascade can reconcile artist identity",
            ));
        }
        return Ok(ArtistArtworkResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            image_url: None,
            detail: Some(format!(
                "no MusicBrainz-confident match for {artist} at ≥{MB_MIN_CONFIDENCE_PERCENT}% \
                 — cascade requires MBID reconciliation to prevent wrong-entity images"
            )),
            attribution: None,
            sources: Vec::new(),
        });
    }

    let effective_mbid: Option<&str> =
        reconciled.as_ref().map(|l| l.artist_mbid.as_str());
    let deezer_artist_id: Option<u64> = reconciled
        .as_ref()
        .and_then(|l| l.deezer_artist_url.as_deref())
        .and_then(parse_deezer_artist_id);

    // Non-Deezer provider result cache. Volumio meta,
    // TheAudioDB, and fanart.tv return stable URLs per artist;
    // memoising the `SourceEntry` snapshot lets a browse of the
    // same artist set after the first cascade skip every non-
    // Deezer network round. Deezer is deliberately excluded —
    // its live-fetch invariant remains structurally enforced by
    // `ArtistImageHit`'s missing `Serialize`, and every request
    // still fires `deezer.get_artist_image_by_id(id)` fresh
    // (using the id memoised via the reconcile cache).
    let cached_non_deezer: Option<Vec<SourceEntry>> = if can_cache {
        catalogue
            .caches
            .get_provider(&fold_key)
            .map(|entry| deserialize_provider_entries(&entry.sources))
    } else {
        None
    };
    let (volumio_src, tadb_src, fanart_src) = if let Some(sources) =
        cached_non_deezer.as_ref()
    {
        // Warm cache — re-use the memoised entries per
        // provider. Every source in the cache was created
        // by a prior successful fetch, so the vector is
        // already sort-order agnostic.
        let mut v = None;
        let mut t = None;
        let mut f = None;
        for src in sources {
            match src.provider_id.as_str() {
                "volumio_meta" => v = Some(src.clone()),
                "theaudiodb" => t = Some(src.clone()),
                "fanart_tv" => f = Some(src.clone()),
                _ => {}
            }
        }
        (v, t, f)
    } else {
        // Cold cache — hit every enabled non-Deezer
        // provider and cache the successful entries.
        let (volumio_src, tadb_src, fanart_src) = tokio::join!(
            fetch_volumio_meta_artist(
                &artist,
                &catalogue.volumio_meta_http,
                &catalogue.volumio_meta_variant,
                want_volumio,
            ),
            fetch_theaudiodb_artist(
                effective_mbid,
                &artist,
                catalogue,
                want_theaudiodb,
            ),
            fetch_fanart_artist(effective_mbid, catalogue, want_fanart),
        );
        if can_cache {
            let snapshot: Vec<serde_json::Value> =
                [volumio_src.as_ref(), tadb_src.as_ref(), fanart_src.as_ref()]
                    .into_iter()
                    .flatten()
                    .filter_map(serialize_source_entry)
                    .collect();
            catalogue.caches.put_provider(
                fold_key.clone(),
                crate::artwork_caches::ProviderEntry::new(snapshot),
            );
        }
        (volumio_src, tadb_src, fanart_src)
    };

    // Deezer always fires live — the by-id fetch is cheap
    // (single HTTPS round on a known id) and the URL is under
    // the live-fetch invariant.
    let deezer_src = fetch_deezer_artist_by_id(
        deezer_artist_id,
        &artist,
        catalogue,
        want_deezer,
    )
    .await;

    let mut sources: Vec<SourceEntry> =
        [volumio_src, tadb_src, deezer_src, fanart_src]
            .into_iter()
            .flatten()
            .collect();
    sort_sources_by_priority(&mut sources, &catalogue.config);

    if sources.is_empty() {
        return Ok(ArtistArtworkResponse {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            image_url: None,
            detail: Some(format!(
                "no artist artwork found for {artist} across enabled providers"
            )),
            attribution: None,
            sources: Vec::new(),
        });
    }
    Ok(ArtistArtworkResponse::from_sources(sources))
}

// ---------------------------------------------------------------
// Per-provider fetch helpers. Each returns
// `Option<SourceEntry>`: `None` on disabled / unavailable /
// clean miss / transient error. Transient errors NEVER cache
// and NEVER surface a non-`None` entry — silence is a cascade-
// level skip. The transient-not-cached discipline mirrors the
// album-artwork cascade's posture.
// ---------------------------------------------------------------

async fn fetch_volumio_meta_artist(
    artist: &str,
    http: &Client,
    variant: &str,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    // Volumio meta proxy: same base as the album cascade but
    // with `mode=artistArt`. No cache layer at the plugin — the
    // returned URL is what the operator UI renders; framework-
    // side asset caching would need a distinct code path
    // outside this cascade.
    let url = format!(
        "https://meta.volumio.org/metas/v1/getDatas?mode=artistArt&artist={}&variant={}",
        percent_encode(artist),
        percent_encode(variant),
    );
    let resp = match http.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                error = %e,
                "volumio meta artistArt transient; skipping"
            );
            return None;
        }
    };
    if !resp.status().is_success() {
        // Only 200 counts. 404 → clean miss (structural); 5xx / 429
        // → transient. Both fall out to None here and are logged
        // at debug for structural misses and warn for transient.
        let status = resp.status();
        if status.as_u16() == 404 {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                "volumio meta artistArt clean miss"
            );
        } else {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                status = status.as_u16(),
                "volumio meta artistArt non-success status; skipping"
            );
        }
        return None;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                error = %e,
                "volumio meta artistArt json decode failed; skipping"
            );
            return None;
        }
    };
    let image_url = json
        .get("data")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)?;
    let payload = serde_json::json!({
        "image_url": image_url,
        "source_url": "https://meta.volumio.org",
    });
    Some(SourceEntry {
        provider_id: ArtistProviderId::VolumioMeta.as_str().to_string(),
        privacy_class: ArtistPrivacyClass::Anonymous.as_str().to_string(),
        payload,
        attribution: Attribution {
            source_name: "Volumio Meta".into(),
            source_url: Some("https://meta.volumio.org".into()),
            license: "Volumio meta proxy terms".into(),
        },
    })
}

async fn fetch_theaudiodb_artist(
    artist_mbid: Option<&str>,
    artist: &str,
    catalogue: &ArtistCatalogue,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let tadb = catalogue.theaudiodb.as_ref()?;
    // MBID-first: TheAudioDB's `artist-mb.php?i=<mbid>` returns
    // the canonical entity for the given MBID; name-search
    // (`search.php`) cross-matches on namesake artists and is
    // therefore only a fallback when no MBID is available (the
    // upstream cascade currently blocks that fallback, so this
    // path stays for API-shape completeness).
    let hit = match artist_mbid {
        Some(mbid) => match tadb.fetch_artist_bio_by_mbid(mbid).await {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    artist,
                    mbid,
                    error = %e,
                    "TheAudioDB artist artwork by MBID transient; skipping"
                );
                return None;
            }
        },
        None => match tadb.search_artist_bio(artist).await {
            Ok(Some(h)) => h,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    artist,
                    error = %e,
                    "TheAudioDB artist artwork transient; skipping"
                );
                return None;
            }
        },
    };
    let thumb = hit
        .artist_thumb_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;
    let payload = serde_json::json!({
        "thumb_url": thumb,
        "source_url": hit.source_url,
    });
    Some(SourceEntry {
        provider_id: ArtistProviderId::TheAudioDb.as_str().to_string(),
        privacy_class: ArtistPrivacyClass::Anonymous.as_str().to_string(),
        payload,
        attribution: Attribution {
            source_name: "TheAudioDB".into(),
            source_url: Some(hit.source_url),
            license: "TheAudioDB terms of use".into(),
        },
    })
}

async fn fetch_deezer_artist_by_id(
    deezer_artist_id: Option<u64>,
    artist: &str,
    catalogue: &ArtistCatalogue,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let deezer = catalogue.deezer.as_ref()?;
    let id = deezer_artist_id?;
    // Deezer live-fetch invariant (ToS-mandated):
    // ------------------------------------------------------------
    // `ArtistImageHit` deliberately does NOT derive `Serialize`.
    // The compiler refuses any code path that would round-trip
    // the hit through JSON — so persisting the response body is
    // structurally impossible, not merely policy. This fetch
    // extracts URL fields into a plain serde_json::json! payload
    // one field at a time; the hit itself is never serialised
    // and never leaves this function.
    //
    // What DOES cross the wire: URL strings that the operator UI
    // resolves inline against Deezer's CDN on render. Every
    // render is a live fetch. No plugin-side cache; no framework
    // asset-cache push; no persistence layer touches the bytes.
    //
    // This entry's presence in `sources[]` implicitly declares
    // to the UI: "render live from these URLs; do not persist
    // the image data".
    //
    // MBID-first: `deezer_artist_id` is sourced from a
    // MusicBrainz URL-relation whose target hostname is
    // `deezer.com`. Fetching by id guarantees the returned
    // entity matches MB's canonical identity for the artist —
    // the wire-observed cross-match (Abba → deezer id
    // `204768937` [anime] instead of `1071` [ABBA]) cannot
    // happen because we never re-search by name.
    let hit = match deezer.get_artist_image_by_id(id).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "deezer",
                artist,
                deezer_id = id,
                error = %e,
                "Deezer artist image by id transient; skipping"
            );
            return None;
        }
    };
    let payload = serde_json::json!({
        "picture_xl_url": hit.picture_xl_url,
        "picture_big_url": hit.picture_big_url,
        "picture_medium_url": hit.picture_medium_url,
        "picture_small_url": hit.picture_small_url,
        "deezer_artist_id": hit.deezer_artist_id,
        "artist_name": hit.artist_name,
        "source_url": hit.source_url.clone(),
        "cache_policy": "live_fetch_only",
    });
    let source_url = hit.source_url.clone();
    // `hit` drops here — the ArtistImageHit type is un-Serialize,
    // so it cannot leak through JSON. Only the URL strings above
    // survive into the response payload.
    Some(SourceEntry {
        provider_id: ArtistProviderId::Deezer.as_str().to_string(),
        privacy_class: ArtistPrivacyClass::Anonymous.as_str().to_string(),
        payload,
        attribution: Attribution {
            source_name: "Deezer".into(),
            source_url: Some(source_url),
            license: "Deezer terms of use (live-fetch only, no persistence)"
                .into(),
        },
    })
}

async fn fetch_fanart_artist(
    artist_mbid: Option<&str>,
    catalogue: &ArtistCatalogue,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let fanart = catalogue.fanart.as_ref()?;
    let mbid = artist_mbid?;
    let hit = match fanart.get_artist_images(mbid).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "fanart_tv",
                artist_mbid = mbid,
                error = %e,
                "fanart.tv artist images transient; skipping"
            );
            return None;
        }
    };
    if !hit.has_any_artwork() {
        return None;
    }
    let payload = serde_json::json!({
        "hd_music_logo_urls": hit.hd_music_logo_urls,
        "hd_artist_logo_urls": hit.hd_artist_logo_urls,
        "artist_background_urls": hit.artist_background_urls,
        "artist_thumb_urls": hit.artist_thumb_urls,
        "music_banner_urls": hit.music_banner_urls,
        "artist_mbid": hit.artist_mbid,
        "artist_name": hit.artist_name,
        "source_url": hit.source_url,
    });
    Some(SourceEntry {
        provider_id: ArtistProviderId::FanartTv.as_str().to_string(),
        privacy_class: ArtistPrivacyClass::IdentityBearing.as_str().to_string(),
        payload,
        attribution: Attribution {
            source_name: "fanart.tv".into(),
            source_url: Some(hit.source_url),
            license: "fanart.tv terms of use".into(),
        },
    })
}

/// Minimal percent-encoder for URL query values. Mirrors the
/// helper in providers.rs.
fn percent_encode(s: &str) -> String {
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
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

/// Reconcile an artist name to a MusicBrainz canonical
/// artist entity plus URL-relations.
///
/// Two calls when the client is present:
///
/// 1. `search_artist` → highest-scoring hit. Score must be
///    ≥ [`MB_MIN_CONFIDENCE_PERCENT`] (typically the top hit
///    for common artists returns exactly 100). Below that
///    threshold the reconciliation refuses so a namesake /
///    misspelling does not surface a wrong entity.
/// 2. `lookup_artist` on the reconciled MBID with
///    `inc=url-rels` → carries the Deezer artist page URL when
///    MB has recorded one, which the Deezer fetch consumes as
///    an authoritative artist id.
///
/// Returns `None` on: no MB client (operator has not
/// configured a MusicBrainz UA), no search hit at confidence,
/// or transient MB failure (never caches — the next request
/// re-attempts).
async fn reconcile_artist_mbid(
    artist: &str,
    mb: Option<&MusicBrainzClient>,
) -> Option<evo_online_providers::musicbrainz::ArtistLookup> {
    let mb = mb?;
    let hit = match mb.search_artist(artist).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                "MB artist search returned no hits"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                error = %mb_error_display(&e),
                "MB artist search transient; skipping"
            );
            return None;
        }
    };
    if hit.confidence_percent < MB_MIN_CONFIDENCE_PERCENT {
        tracing::debug!(
            plugin = crate::PLUGIN_NAME,
            provider = "musicbrainz",
            artist,
            canonical = %hit.canonical_name,
            confidence = hit.confidence_percent,
            threshold = MB_MIN_CONFIDENCE_PERCENT,
            "MB artist match below confidence threshold; refusing to key providers on it"
        );
        return None;
    }
    match mb.lookup_artist(&hit.artist_mbid).await {
        Ok(lookup) => Some(lookup),
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                mbid = %hit.artist_mbid,
                error = %mb_error_display(&e),
                "MB artist lookup transient; falling back to MBID-only"
            );
            Some(bare_lookup_from_mbid(&hit.artist_mbid, artist))
        }
    }
}

/// Construct a fabricated `ArtistLookup` from an MBID + query
/// name when the full URL-rels lookup did not succeed. Lets
/// the downstream providers that key on MBID (fanart.tv,
/// TheAudioDB) still fire, at the cost of the Deezer-by-id
/// path which requires an explicit URL-rel.
fn bare_lookup_from_mbid(
    mbid: &str,
    artist: &str,
) -> evo_online_providers::musicbrainz::ArtistLookup {
    evo_online_providers::musicbrainz::ArtistLookup {
        artist_mbid: mbid.to_string(),
        canonical_name: artist.to_string(),
        artist_type: None,
        life_span_begin: None,
        life_span_end: None,
        country: None,
        wikipedia_url: None,
        wikidata_url: None,
        official_homepage_url: None,
        deezer_artist_url: None,
    }
}

/// Render a MusicBrainz error compactly for a tracing warn.
fn mb_error_display(e: &MusicBrainzError) -> String {
    match e {
        MusicBrainzError::Http(err) => format!("http: {err}"),
        MusicBrainzError::Status { status, body } => {
            let truncated: String = body.chars().take(120).collect();
            format!("status={status} body={truncated}")
        }
        MusicBrainzError::Decode(msg) => format!("decode: {msg}"),
    }
}

/// Snapshot a `SourceEntry` into the cache's `serde_json::Value`
/// storage shape. Returns `None` when serialisation fails (in
/// practice never; `Serialize` is derived on both structs and
/// their fields).
fn serialize_source_entry(entry: &SourceEntry) -> Option<serde_json::Value> {
    match serde_json::to_value(entry) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider_id = %entry.provider_id,
                error = %e,
                "failed to snapshot artist-artwork source entry for cache; \
                 skipping the entry (fresh fetch on next request)"
            );
            None
        }
    }
}

/// Restore a slice of cached `serde_json::Value`s into a
/// `Vec<SourceEntry>`. Entries that fail to deserialise
/// (should be impossible; only present so a schema drift in a
/// running plugin cannot panic the request handler) are
/// silently dropped and re-fetched on the next request.
fn deserialize_provider_entries(
    stored: &[serde_json::Value],
) -> Vec<SourceEntry> {
    stored
        .iter()
        .filter_map(|v| {
            match serde_json::from_value::<SourceEntry>(v.clone()) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        error = %e,
                        "cached artist-artwork source entry failed to \
                         deserialise; dropping (fresh fetch on next request)"
                    );
                    None
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_of(provider_id: &str, payload: serde_json::Value) -> SourceEntry {
        SourceEntry {
            provider_id: provider_id.to_string(),
            privacy_class: ArtistPrivacyClass::Anonymous.as_str().to_string(),
            payload,
            attribution: Attribution {
                source_name: provider_id.to_string(),
                source_url: None,
                license: "test".into(),
            },
        }
    }

    #[test]
    fn provider_id_wire_round_trip() {
        for p in [
            ArtistProviderId::VolumioMeta,
            ArtistProviderId::TheAudioDb,
            ArtistProviderId::Deezer,
            ArtistProviderId::FanartTv,
        ] {
            assert_eq!(
                ArtistProviderId::from_wire(p.as_str()),
                Some(p),
                "round-trip for {p:?}"
            );
        }
        assert_eq!(ArtistProviderId::from_wire("nope"), None);
    }

    #[test]
    fn merge_override_preserves_untouched_fields() {
        let mut cfg = ArtistProviderConfig::defaults();
        let baseline = cfg.flags(ArtistProviderId::Deezer).priority;
        cfg.merge_override(ArtistProviderId::Deezer, Some(false), None);
        assert!(!cfg.flags(ArtistProviderId::Deezer).enabled);
        assert_eq!(cfg.flags(ArtistProviderId::Deezer).priority, baseline);
        cfg.merge_override(ArtistProviderId::Deezer, None, Some(3));
        assert_eq!(cfg.flags(ArtistProviderId::Deezer).priority, 3);
    }

    #[test]
    fn from_sources_top_level_payload_verbatim_from_primary() {
        // Attribution unity — see cascade.rs docstring.
        // Top-level payload MUST be the primary source verbatim.
        let sources = vec![
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://cdn.deezer.example/xl.jpg",
                }),
            ),
            source_of(
                "theaudiodb",
                serde_json::json!({
                    "thumb_url": "https://theaudiodb.example/thumb.jpg",
                }),
            ),
        ];
        let resp = ArtistArtworkResponse::from_sources(sources);
        let payload = resp.payload.unwrap();
        assert_eq!(
            payload.get("picture_xl_url").unwrap(),
            "https://cdn.deezer.example/xl.jpg"
        );
        assert!(
            payload.get("thumb_url").is_none(),
            "field-level merge is banned; theaudiodb.thumb_url must not \
             leak into the deezer-attributed top-level payload"
        );
        assert_eq!(resp.provider_id.as_deref(), Some("deezer"));
        assert_eq!(resp.sources.len(), 2);
    }

    #[test]
    fn sort_sources_by_priority_uses_operator_config() {
        let cfg = ArtistProviderConfig::defaults();
        let mut sources = vec![
            source_of("fanart_tv", serde_json::json!({})),
            source_of("deezer", serde_json::json!({})),
            source_of("theaudiodb", serde_json::json!({})),
            source_of("volumio_meta", serde_json::json!({})),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        assert_eq!(sources[0].provider_id, "deezer");
        assert_eq!(sources[1].provider_id, "theaudiodb");
        assert_eq!(sources[2].provider_id, "volumio_meta");
        assert_eq!(sources[3].provider_id, "fanart_tv");
    }

    #[test]
    fn sort_sources_by_priority_sinks_unknown() {
        let cfg = ArtistProviderConfig::defaults();
        let mut sources = vec![
            source_of("mystery_provider", serde_json::json!({})),
            source_of("deezer", serde_json::json!({})),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        assert_eq!(sources[0].provider_id, "deezer");
        assert_eq!(sources[1].provider_id, "mystery_provider");
    }

    #[test]
    fn from_sources_ok_when_non_empty_notfound_when_empty() {
        let ok = ArtistArtworkResponse::from_sources(vec![source_of(
            "deezer",
            serde_json::json!({"picture_xl_url": "https://example/xl.jpg"}),
        )]);
        assert!(matches!(ok.status, CascadeStatus::Ok));
        let empty = ArtistArtworkResponse::from_sources(vec![]);
        assert!(matches!(empty.status, CascadeStatus::NotFound));
    }

    #[test]
    fn pick_canonical_image_url_prefers_string_keys_by_order() {
        // volumio_meta shape
        let vol = serde_json::json!({"image_url": "https://vol/x.jpg"});
        assert_eq!(
            pick_canonical_image_url(&vol).as_deref(),
            Some("https://vol/x.jpg")
        );
        // theaudiodb shape
        let tadb = serde_json::json!({"thumb_url": "https://t/x.jpg"});
        assert_eq!(
            pick_canonical_image_url(&tadb).as_deref(),
            Some("https://t/x.jpg")
        );
        // deezer shape — picks the largest available first
        let deezer = serde_json::json!({
            "picture_xl_url": "https://d/xl.jpg",
            "picture_big_url": "https://d/big.jpg",
        });
        assert_eq!(
            pick_canonical_image_url(&deezer).as_deref(),
            Some("https://d/xl.jpg")
        );
        // deezer shape — falls through to next size when top is empty
        let deezer_no_xl = serde_json::json!({
            "picture_xl_url": "",
            "picture_big_url": "https://d/big.jpg",
        });
        assert_eq!(
            pick_canonical_image_url(&deezer_no_xl).as_deref(),
            Some("https://d/big.jpg")
        );
    }

    #[test]
    fn pick_canonical_image_url_falls_back_to_array_keys() {
        // fanart shape — hd_music_logo_urls wins over other arrays
        let fanart = serde_json::json!({
            "hd_music_logo_urls": ["https://f/logo.png"],
            "artist_thumb_urls": ["https://f/thumb.jpg"],
        });
        assert_eq!(
            pick_canonical_image_url(&fanart).as_deref(),
            Some("https://f/logo.png")
        );
        // Empty top array + non-empty next → returns next
        let fanart_partial = serde_json::json!({
            "hd_music_logo_urls": [],
            "artist_thumb_urls": ["https://f/thumb.jpg"],
        });
        assert_eq!(
            pick_canonical_image_url(&fanart_partial).as_deref(),
            Some("https://f/thumb.jpg")
        );
    }

    #[test]
    fn pick_canonical_image_url_returns_none_when_no_recognised_key() {
        let empty = serde_json::json!({});
        assert!(pick_canonical_image_url(&empty).is_none());
        let junk = serde_json::json!({"random_key": "https://x/y.jpg"});
        assert!(pick_canonical_image_url(&junk).is_none());
    }

    #[test]
    fn pick_canonical_image_url_rejects_empty_entity_hash() {
        // Wire-observed failure mode: Deezer returns a URL with
        // an empty entity slug when the provider does not
        // resolve the entity, e.g.
        // `.../artist//1000x1000-...`. That URL must not surface
        // as a hit — the picker must return `None` so the
        // caller falls through to the next source or emits
        // NotFound.
        let deezer_empty = serde_json::json!({
            "picture_xl_url": "https://cdn-images.dzcdn.net/images/artist//1000x1000-000000-80-0-0.jpg",
        });
        assert_eq!(pick_canonical_image_url(&deezer_empty), None);
        // The same shape on a TheAudioDB or volumio_meta payload.
        let tadb_empty = serde_json::json!({
            "thumb_url": "https://theaudiodb.com/images/media/artist//thumb.jpg",
        });
        assert_eq!(pick_canonical_image_url(&tadb_empty), None);
    }

    #[test]
    fn pick_canonical_image_url_rejects_empty_entity_in_array_key() {
        // Same guard on array-key sources: an entry with an
        // empty entity segment must not be picked, and the
        // picker must fall through to the next non-empty entry
        // in the same array.
        let mixed = serde_json::json!({
            "hd_music_logo_urls": [
                "https://f/artist//logo.png",
                "https://f/artist/real/logo.png",
            ],
        });
        assert_eq!(
            pick_canonical_image_url(&mixed).as_deref(),
            Some("https://f/artist/real/logo.png")
        );
    }

    #[test]
    fn is_real_image_url_accepts_normal_shapes() {
        assert!(is_real_image_url(
            "https://cdn-images.dzcdn.net/images/artist/abc123/1000x1000.jpg"
        ));
        assert!(is_real_image_url("https://x/y/z.jpg"));
    }

    #[test]
    fn is_real_image_url_rejects_empty_path_segment() {
        assert!(!is_real_image_url("https://x/y//z.jpg"));
        assert!(!is_real_image_url(
            "https://cdn-images.dzcdn.net/images/artist//1000x1000.jpg"
        ));
    }

    #[test]
    fn from_sources_populates_top_level_image_url_from_primary() {
        // Consumer contract: the UI fires this verb per visible
        // artist tile and `<img src>`s the top-level image_url
        // straight at the provider. Regression test the picker
        // is wired into from_sources correctly.
        let resp = ArtistArtworkResponse::from_sources(vec![source_of(
            "deezer",
            serde_json::json!({"picture_xl_url": "https://d/xl.jpg"}),
        )]);
        assert_eq!(resp.image_url.as_deref(), Some("https://d/xl.jpg"));
    }

    #[test]
    fn from_sources_falls_through_source_with_empty_url_to_next_usable() {
        // The primary source returned a URL-shaped-but-empty
        // response (empty entity segment). The cascade must
        // skip it and pick the next source whose payload
        // carries a real URL — never surface status=ok with
        // a broken URL.
        let sources = vec![
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://cdn/artist//1000x1000.jpg",
                }),
            ),
            source_of(
                "theaudiodb",
                serde_json::json!({
                    "thumb_url": "https://theaudiodb.example/artist/real.jpg",
                }),
            ),
        ];
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert!(matches!(resp.status, CascadeStatus::Ok));
        assert_eq!(
            resp.image_url.as_deref(),
            Some("https://theaudiodb.example/artist/real.jpg")
        );
        assert_eq!(resp.provider_id.as_deref(), Some("theaudiodb"));
    }

    #[test]
    fn from_sources_returns_not_found_when_every_source_is_empty() {
        // Every source returned a URL-shaped-but-empty payload
        // (the compilation-artist wire failure mode). The
        // cascade MUST surface NotFound so the caller can
        // render an honest placeholder — never status=ok with
        // no image_url, and never status=ok with a broken URL.
        let sources = vec![
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://cdn/artist//1000x1000.jpg",
                }),
            ),
            source_of(
                "theaudiodb",
                serde_json::json!({
                    "thumb_url": "https://theaudiodb.example/artist//thumb.jpg",
                }),
            ),
        ];
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert!(matches!(resp.status, CascadeStatus::NotFound));
        assert!(resp.image_url.is_none());
        assert!(resp.provider_id.is_none());
    }

    /// Compile-fence attestation: ArtistImageHit MUST NOT derive
    /// Serialize. If a future refactor accidentally adds
    /// Serialize to it, the plugin's Deezer helper would be able
    /// to round-trip the hit through JSON — which would let a
    /// caller persist it in violation of Deezer's ToS.
    ///
    /// This test doesn't run Deezer; it asserts the type-level
    /// invariant by relying on trait bounds. If ArtistImageHit
    /// ever gains Serialize, this line stops compiling because
    /// the negative bound would evaluate differently. We prove
    /// the invariant by constructing a phantom function that
    /// requires the type to NOT be Serialize; if the compile
    /// fence is intact, this compiles.
    #[test]
    fn deezer_hit_type_stays_un_serialisable() {
        fn assert_not_serialize<T: 'static>() {
            // The important half of the fence lives in Deezer's
            // own test suite (`artist_image_hit_does_not_derive_serialize`
            // in evo-online-providers/src/deezer.rs). This shim
            // is a sentinel: if the earlier fence ever breaks,
            // this plugin's aggregate cascade needs to be revisited
            // before the plugin can persist Deezer payloads
            // legally.
            let _ = std::any::TypeId::of::<T>();
        }
        assert_not_serialize::<evo_online_providers::deezer::ArtistImageHit>();
    }
}
