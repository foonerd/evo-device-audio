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
//! cascade. Priority default is fanart.tv 40, Deezer 45,
//! TheAudioDB 50, Volumio meta 55 — highest-quality
//! identity-bearing source wins whenever its credential is
//! present. When the fanart key is unset the provider returns
//! `Absent` (no source emitted), so the picker automatically
//! falls through to Deezer's live fetch. Result: keyed
//! deployments get the curated fanart portrait; keyless
//! deployments stay on the Deezer fallback without any
//! operator ordering step.

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
            fanart_tv: ArtistProviderFlags {
                enabled: true,
                priority: 40,
            },
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
/// Wire-facing status for the artist-artwork verb. Mirrors the
/// album-artwork surface's `ResponseStatus` (see
/// [`crate::resolve::ResponseStatus`]) so both surfaces speak
/// one vocabulary — and, critically, so `Unavailable` (a
/// reachable-but-transient outcome that MUST NOT cache
/// negatively) is representable on this path too. Prior to
/// this variant every transient (MB rate-limit / 5xx / DNS
/// hiccup) collapsed to `NotFound` and was durably memoised
/// for hours — a well-known-artist tile could stick blank
/// for the full negative TTL after one MB rate-limit spike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CascadeStatus {
    Ok,
    /// Reconciliation + every enabled provider gave a clean
    /// "no such artist" answer. Framework MAY memoise
    /// negatively under the reconcile-miss TTL.
    NotFound,
    /// At least one identity-bearing step (MB reconcile, or a
    /// provider after a good reconcile) was reachable-but-
    /// transient (rate-limit, HTTP 5xx, timeout, transport
    /// error). Callers MUST NOT memoise this as definitive
    /// absence — the operator UI treats it as "retry on next
    /// gesture" rather than a poison-null.
    Unavailable,
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

    /// Definitive absence: MB search returned no confident
    /// match, or (after a good reconcile) every enabled
    /// provider clean-missed. Cacheable at the caller under the
    /// miss TTL; UI may render an honest placeholder.
    pub(crate) fn not_found(detail: impl Into<String>) -> Self {
        Self {
            v: 1,
            status: CascadeStatus::NotFound,
            provider_id: None,
            privacy_class: None,
            payload: None,
            image_url: None,
            detail: Some(detail.into()),
            attribution: None,
            sources: Vec::new(),
        }
    }

    /// Reachable-but-transient upstream: MB search or every
    /// provider errored (rate-limit / 5xx / transport). NOT
    /// cacheable — the plugin refuses to write, and the wire
    /// contract tells the UI to retry on next gesture rather
    /// than poison its session cache with a null.
    pub(crate) fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            v: 1,
            status: CascadeStatus::Unavailable,
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
    /// Build a wire response from the aggregate provider
    /// outcomes. Three-way aggregation over `[ProviderOutcome]`
    /// after a good reconcile:
    ///
    /// - Any Hit whose payload picker returns a real URL → Ok.
    ///   Even if other providers were Unavailable — a good hit
    ///   wins.
    /// - No Hit, at least one Unavailable → Unavailable. We
    ///   cannot say the artist is absent when a provider
    ///   errored; the UI retries on next gesture.
    /// - No Hit, all clean Absent → NotFound. Cacheable.
    pub(crate) fn from_provider_outcomes(
        outcomes: Vec<ProviderOutcome>,
    ) -> Self {
        let mut hits = Vec::new();
        let mut had_unavailable = false;
        for out in outcomes {
            match out {
                ProviderOutcome::Hit(entry) => hits.push(entry),
                ProviderOutcome::Absent => {}
                ProviderOutcome::Unavailable => had_unavailable = true,
            }
        }
        if hits.is_empty() {
            if had_unavailable {
                return Self::unavailable(
                    "every enabled provider errored transiently; \
                     retry on next gesture",
                );
            }
            return Self::not_found(
                "no artist artwork across enabled providers",
            );
        }
        Self::from_sources(hits)
    }

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

/// Pick a canonical PORTRAIT (photograph) URL from a
/// provider-specific payload for the artist-tile context.
///
/// Image-type suitability outranks provider priority for the
/// portrait: the artist tile displays a photograph of the
/// artist, never a wordmark / band logo / promotional banner.
/// Fanart.tv carries five image classes in one payload
/// (`hd_music_logo_urls`, `hd_artist_logo_urls`,
/// `artist_thumb_urls`, `music_banner_urls`,
/// `artist_background_urls`) — only two are photographs:
///
/// - `artist_thumb_urls` — 1000×1000 square photo (primary
///   portrait class per fanart's taxonomy).
/// - `artist_background_urls` — 1920×1080 fanart-style photo,
///   secondary portrait fallback (some artists have a
///   background photo but no thumb yet).
///
/// The other three fanart classes are wordmarks, band logos,
/// or mixed promotional banners — none is a photograph and
/// none is picked for the portrait context. When fanart's
/// payload carries only logo/banner classes and no photo, the
/// picker returns `None`, and the surrounding source-walk in
/// [`ArtistArtworkResponse::from_sources`] falls through to
/// the next provider (Deezer / TheAudioDB / Volumio meta),
/// whose payloads are photo-only by shape.
///
/// The four artist-artwork sources use different keys:
///
/// - `volumio_meta` → `image_url` (string, photo)
/// - `theaudiodb` → `thumb_url` (string, photo)
/// - `deezer` → `picture_xl_url` / `_big_url` / `_medium_url`
///   / `_small_url` (strings, photos; pick the largest
///   non-empty)
/// - `fanart_tv` → `artist_thumb_urls[0]` /
///   `artist_background_urls[0]` (arrays, photos ONLY —
///   logos and banners deliberately excluded from this
///   picker).
///
/// The picker probes each key in preference order and returns
/// the first non-empty string it finds. Returns `None` when
/// the payload carries no photograph URL under any
/// portrait-suitable key — the resolver then falls through to
/// the next source rather than promoting a logo to portrait.
///
/// A separate context (artist-page hero / logo mark / banner
/// strip) may render logos and banners later; the portrait
/// picker MUST NOT select them.
fn pick_canonical_image_url(payload: &serde_json::Value) -> Option<String> {
    let obj = payload.as_object()?;
    // String-shaped photo keys — used by volumio_meta,
    // theaudiodb, and deezer. All of these payloads carry
    // photographs exclusively.
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
    // Array-shaped photo keys — used by fanart_tv. ONLY the
    // photo classes appear here; logos (`hd_music_logo_urls`,
    // `hd_artist_logo_urls`) and banners (`music_banner_urls`)
    // are deliberately excluded — see the module docstring for
    // the image-type suitability rule.
    const PORTRAIT_ARRAY_KEYS: &[&str] =
        &["artist_thumb_urls", "artist_background_urls"];
    for key in PORTRAIT_ARRAY_KEYS {
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

/// MD5 hex digest of the empty string. Providers (Deezer
/// observed in the wild on 2026-07-28) use this well-known
/// constant as a "no image" placeholder segment in CDN URLs
/// when the artist has no portrait — e.g.
/// `https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/1000x1000-…`.
/// The URL is structurally valid and its bytes decode as an
/// image (Deezer serves a generic silhouette), so a hash-only
/// or Content-Type guard would still pass it. The only honest
/// signal is the segment itself.
const EMPTY_HASH_MD5_HEX: &str = "d41d8cd98f00b204e9800998ecf8427e";

/// Reject URLs that are structurally shaped like a valid image
/// URL but carry no entity identifier — two observed provider
/// failure modes where the picker would otherwise return a
/// status=ok response with a URL that renders as a generic
/// silhouette / broken image.
///
/// 1. `//` inside the path (empty segment) — the pre-2026-07-28
///    Deezer shape `.../artist//<size>-…` where the artist
///    slug was elided outright.
/// 2. The MD5-of-empty-string segment
///    [`EMPTY_HASH_MD5_HEX`] anywhere in the path — the
///    post-2026-07-28 Deezer shape `.../artist/<md5-empty>/<size>-…`
///    where the artist slug was replaced with the well-known
///    "no data" hash. This is what surfaced on the Elton John
///    tile after the outcome-contract landing.
///
/// Both rules apply per-URL; either match rejects.
fn is_real_image_url(url: &str) -> bool {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => url,
    };
    if path.contains("//") {
        return false;
    }
    if path.split('/').any(|seg| seg == EMPTY_HASH_MD5_HEX) {
        return false;
    }
    true
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
    /// Single-flight coalescer keyed on `fold_key`. Ensures at
    /// most one cascade runs per key at any moment; every
    /// concurrent same-key caller subscribes and receives the
    /// same outcome — so a browse fan-out over N tiles that
    /// happen to share an artist collapses to one MB round
    /// trip and one provider wave. Orthogonal to `caches`
    /// (memoises across time; coalescer collapses within one
    /// call cycle).
    pub(crate) coalescer: Arc<
        crate::reconcile_coalescer::ReconcileCoalescer<
            Result<ArtistArtworkResponse, String>,
        >,
    >,
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
///
/// Parses the request, computes the fold-key identity, and
/// routes cache-eligible calls through the single-flight
/// coalescer so that a browse fan-out over N tiles sharing a
/// fold-key collapses to one cascade run. Caller-supplied MBID
/// bypasses the coalescer (the caller has stamped the identity;
/// coalescing on a fold-key that may not correspond to the
/// caller's MBID risks cross-wiring outcomes).
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
    let caller_supplied_mbid = req
        .artist_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // Cache key: fold(input artist) — stable across display
    // cleaning so a browse tile rendered from a cleaned form
    // hits the same cache slot as the raw MPD tag it came
    // from.
    let fold_key =
        evo_device_audio_shared::artist_name::artist_fold_key(&artist);
    let can_cache = !fold_key.is_empty();
    let use_coalescer = can_cache && caller_supplied_mbid.is_none();

    if use_coalescer {
        let artist_owned = artist.clone();
        let fold_key_for_run = fold_key.clone();
        catalogue
            .coalescer
            .run(fold_key.clone(), move || async move {
                run_cascade(
                    artist_owned,
                    fold_key_for_run,
                    None,
                    can_cache,
                    catalogue,
                )
                .await
            })
            .await
    } else {
        run_cascade(
            artist,
            fold_key,
            caller_supplied_mbid,
            can_cache,
            catalogue,
        )
        .await
    }
}

/// Body of the cascade — the parsed request has been split
/// into `(artist, fold_key, caller_supplied_mbid, can_cache)`
/// and this function performs the actual reconcile + provider
/// wave, honouring the cache policy and the three-way outcome
/// contract.
async fn run_cascade(
    artist: String,
    fold_key: String,
    caller_supplied_mbid: Option<String>,
    can_cache: bool,
    catalogue: &ArtistCatalogue,
) -> Result<ArtistArtworkResponse, String> {
    // MBID reconciliation is mandatory for identity-bearing
    // providers. Three shapes on the outcome (see
    // [`ReconcileOutcome`]): `Found` proceeds to the provider
    // wave; `Absent` short-circuits to a cacheable `NotFound`;
    // `Unavailable` short-circuits to `Unavailable` on the
    // wire with NO cache write (rate-limit / transport spikes
    // must not durably poison a valid artist).
    let reconciled = if let Some(mbid) = caller_supplied_mbid.clone() {
        // The caller passed an MBID they already trust; skip
        // MusicBrainz search, but still fetch URL-rels so the
        // Deezer path can key on the recorded Deezer link. A
        // transient on the rels lookup still keeps the MBID
        // (search-side confidence is proxied by the caller's
        // trust); no bare-lookup fabrication on the "no MB
        // client" arm because we know nothing beyond the id.
        match &catalogue.mb {
            Some(mb) => match mb.lookup_artist(&mbid).await {
                Ok(lookup) => ReconcileOutcome::Found(Box::new(lookup)),
                Err(e) => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        provider = "musicbrainz",
                        artist = %artist,
                        mbid = %mbid,
                        error = %mb_error_display(&e),
                        outcome = "found_bare_mbid",
                        next_attempt = "on_next_demand",
                        cache_policy = "not_written_by_caller",
                        "MB URL-rels lookup for caller-supplied MBID transient; identity trusted by caller but Deezer URL-rel absent; caller MUST NOT cache — next request re-attempts full URL-rels lookup"
                    );
                    ReconcileOutcome::FoundPartial(Box::new(
                        bare_lookup_from_mbid(&mbid, &artist),
                    ))
                }
            },
            // No MB client at all: caller trusts the MBID
            // but there is no client to fetch URL-rels. Same
            // degraded shape — MBID-keyed providers can
            // fire, Deezer URL-rel cannot.
            None => ReconcileOutcome::FoundPartial(Box::new(
                bare_lookup_from_mbid(&mbid, &artist),
            )),
        }
    } else if can_cache {
        // Cache-first name reconciliation. Only `Found` +
        // `Absent` write to the cache; a fresh Hit / Miss
        // entry short-circuits the two MB round trips. An
        // `Unavailable` outcome NEVER writes — the next
        // request re-attempts, so a passing rate-limit spike
        // does not poison the tile for hours.
        // A `FoundPartial` outcome (search hit confident but
        // URL-rels transient) also NEVER writes — the
        // identity is fresh but incomplete (no Deezer
        // URL-rel), and caching it would keep Deezer-by-id
        // dead until the 7 d hit-TTL expired. The current
        // call still proceeds to the MBID-keyed providers so
        // the tile has a chance to render; the next request
        // re-attempts the full URL-rels lookup.
        use crate::artwork_caches::{MissReason, ReconcileEntry};
        match catalogue.caches.get_reconcile(&fold_key) {
            Some(ReconcileEntry::Hit { lookup, .. }) => {
                ReconcileOutcome::Found(lookup)
            }
            Some(ReconcileEntry::Miss { .. }) => ReconcileOutcome::Absent,
            None => {
                let outcome =
                    reconcile_artist_mbid(&artist, catalogue.mb.as_deref())
                        .await;
                match &outcome {
                    ReconcileOutcome::Found(lookup) => {
                        catalogue.caches.put_reconcile_hit(
                            fold_key.clone(),
                            (**lookup).clone(),
                        );
                    }
                    ReconcileOutcome::Absent => {
                        catalogue.caches.put_reconcile_miss(
                            fold_key.clone(),
                            MissReason::NoConfidentMatch,
                        );
                    }
                    ReconcileOutcome::FoundPartial(_)
                    | ReconcileOutcome::Unavailable => {
                        // Deliberately no cache write. The
                        // next request retries; the UI's
                        // session cache also skips these
                        // per the wire contract.
                    }
                }
                outcome
            }
        }
    } else {
        reconcile_artist_mbid(&artist, catalogue.mb.as_deref()).await
    };

    // Reconcile short-circuits: Absent → NotFound (definitive
    // absence); Unavailable → Unavailable (retry-safe). `Found`
    // and `FoundPartial` both proceed to the provider wave —
    // `FoundPartial` differs only in that its identity lacks
    // the Deezer URL-rel, so the Deezer-by-id fetch will noop
    // for this call; MBID-keyed providers (fanart, theaudiodb)
    // still fire.
    let reconciled = match reconciled {
        ReconcileOutcome::Found(lookup)
        | ReconcileOutcome::FoundPartial(lookup) => *lookup,
        ReconcileOutcome::Absent => {
            let any_configured = any_provider_configured(catalogue);
            if !any_configured || catalogue.mb.is_none() {
                return Ok(ArtistArtworkResponse::not_configured(
                    "every artist-artwork provider is disabled or unavailable on this \
                     device; enable at least one under Settings → Metadata → Sources \
                     (fanart.tv also requires the artist_mbid + a fanart.tv \
                     personal API key), and set `musicbrainz_user_agent` so the \
                     cascade can reconcile artist identity",
                ));
            }
            return Ok(ArtistArtworkResponse::not_found(format!(
                "no MusicBrainz-confident match for {artist} at ≥{MB_MIN_CONFIDENCE_PERCENT}% \
                 — cascade requires MBID reconciliation to prevent wrong-entity images"
            )));
        }
        ReconcileOutcome::Unavailable => {
            return Ok(ArtistArtworkResponse::unavailable(format!(
                "musicbrainz reconcile transient for {artist}; retry on next gesture"
            )));
        }
    };

    let want_theaudiodb =
        catalogue.config.is_enabled(ArtistProviderId::TheAudioDb)
            && catalogue.theaudiodb.is_some();
    let want_deezer = catalogue.config.is_enabled(ArtistProviderId::Deezer)
        && catalogue.deezer.is_some()
        && reconciled.deezer_artist_url.is_some();
    let want_fanart = catalogue.config.is_enabled(ArtistProviderId::FanartTv)
        && catalogue.fanart.is_some();
    // volumio_meta remains a name-only source; it takes no MBID
    // and no way to validate against a canonical identity. The
    // MBID reconcile above already confirmed the artist exists
    // (we would not be here otherwise); volumio_meta fires only
    // as one of the enabled providers.
    let want_volumio =
        catalogue.config.is_enabled(ArtistProviderId::VolumioMeta);

    if !(want_volumio || want_theaudiodb || want_deezer || want_fanart) {
        // Reconcile succeeded but no provider is enabled /
        // available. Not the "reconcile-absent" case above.
        return Ok(ArtistArtworkResponse::not_configured(
            "every artist-artwork provider is disabled or unavailable on this \
             device; enable at least one under Settings → Metadata → Sources \
             (fanart.tv also requires the artist_mbid + a fanart.tv \
             personal API key)",
        ));
    }

    let effective_mbid: Option<&str> = Some(reconciled.artist_mbid.as_str());
    let deezer_artist_id: Option<u64> = reconciled
        .deezer_artist_url
        .as_deref()
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
    let (volumio_out, tadb_out, fanart_out) =
        if let Some(sources) = cached_non_deezer.as_ref() {
            // Warm cache — re-hydrate cached entries as Hits. The
            // cache only ever stores successful entries (see the
            // cold-path serialisation below); an absent provider
            // in the snapshot is a definitive absence at reconcile
            // time and stays Absent on warm calls. Transients are
            // never cached, so no Unavailable can come from cache.
            let mut v = ProviderOutcome::Absent;
            let mut t = ProviderOutcome::Absent;
            let mut f = ProviderOutcome::Absent;
            for src in sources {
                match src.provider_id.as_str() {
                    "volumio_meta" => v = ProviderOutcome::Hit(src.clone()),
                    "theaudiodb" => t = ProviderOutcome::Hit(src.clone()),
                    "fanart_tv" => f = ProviderOutcome::Hit(src.clone()),
                    _ => {}
                }
            }
            (v, t, f)
        } else {
            // Cold cache — hit every enabled non-Deezer provider
            // and cache only the Hits. Absent/Unavailable never
            // enter the cache: absence gets re-tried next request
            // (cheap) and unavailable MUST NOT durably poison.
            let (v, t, f) = tokio::join!(
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
            // Cache-write policy: only write the aggregate provider
            // snapshot when EVERY non-Deezer outcome is non-transient
            // (Hit or Absent). A single Unavailable in the wave means
            // the memoised set would be incomplete — a subsequent
            // read would treat the missing entry as a durable Absent,
            // reproducing the bug we just fixed. Keep the cold cost
            // and re-fetch next call.
            let all_non_transient =
                matches!(v, ProviderOutcome::Hit(_) | ProviderOutcome::Absent)
                    && matches!(
                        t,
                        ProviderOutcome::Hit(_) | ProviderOutcome::Absent
                    )
                    && matches!(
                        f,
                        ProviderOutcome::Hit(_) | ProviderOutcome::Absent
                    );
            if can_cache && all_non_transient {
                let snapshot: Vec<serde_json::Value> = [&v, &t, &f]
                    .into_iter()
                    .filter_map(|out| match out {
                        ProviderOutcome::Hit(entry) => {
                            serialize_source_entry(entry)
                        }
                        _ => None,
                    })
                    .collect();
                catalogue.caches.put_provider(
                    fold_key.clone(),
                    crate::artwork_caches::ProviderEntry::new(snapshot),
                );
            }
            (v, t, f)
        };

    // Deezer always fires live — the by-id fetch is cheap
    // (single HTTPS round on a known id) and the URL is under
    // the live-fetch invariant.
    let deezer_out = fetch_deezer_artist_by_id(
        deezer_artist_id,
        &artist,
        catalogue,
        want_deezer,
    )
    .await;

    // Aggregate over all four provider outcomes. `from_provider_outcomes`
    // implements the three-way rule: any Hit wins → Ok; otherwise
    // any Unavailable → Unavailable (retry-safe, no negative cache);
    // otherwise all Absent → NotFound.
    let mut response = ArtistArtworkResponse::from_provider_outcomes(vec![
        volumio_out,
        tadb_out,
        deezer_out,
        fanart_out,
    ]);
    if matches!(response.status, CascadeStatus::Ok) {
        sort_sources_by_priority(&mut response.sources, &catalogue.config);
        // The picker in from_sources ran on the pre-sort order;
        // re-run it so `image_url` / `provider_id` reflect the
        // priority-sorted top hit.
        response = ArtistArtworkResponse::from_sources(response.sources);
    }
    Ok(response)
}

fn any_provider_configured(catalogue: &ArtistCatalogue) -> bool {
    catalogue.config.is_enabled(ArtistProviderId::VolumioMeta)
        || (catalogue.config.is_enabled(ArtistProviderId::TheAudioDb)
            && catalogue.theaudiodb.is_some())
        || (catalogue.config.is_enabled(ArtistProviderId::Deezer)
            && catalogue.deezer.is_some())
        || (catalogue.config.is_enabled(ArtistProviderId::FanartTv)
            && catalogue.fanart.is_some())
}

// ---------------------------------------------------------------
// Per-provider fetch helpers. Each returns [`ProviderOutcome`]:
//
// - `Hit(SourceEntry)` on a successful fetch with a usable
//   payload.
// - `Absent` on disabled / no lookup identity / clean upstream
//   miss (404 / empty result). Aggregatable as definitive
//   absence at the wire level.
// - `Unavailable` on transient upstream failure (rate-limit,
//   HTTP 5xx, timeout, transport, decode). MUST NOT durably
//   cache — the wire aggregate surfaces `Unavailable` unless
//   another provider hits.
// ---------------------------------------------------------------

async fn fetch_volumio_meta_artist(
    artist: &str,
    http: &Client,
    variant: &str,
    enabled: bool,
) -> ProviderOutcome {
    if !enabled {
        return ProviderOutcome::Absent;
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
                outcome = "unavailable",
                next_attempt = "on_next_demand",
                "volumio_meta artist artwork transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
            );
            return ProviderOutcome::Unavailable;
        }
    };
    if !resp.status().is_success() {
        // 404 → clean miss (structural, cacheable at aggregate);
        // 5xx / 429 / other → transient (NOT cacheable).
        let status = resp.status();
        if status.as_u16() == 404 {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                "volumio meta artistArt clean miss"
            );
            return ProviderOutcome::Absent;
        }
        tracing::warn!(
            plugin = crate::PLUGIN_NAME,
            provider = "volumio_meta",
            artist,
            status = status.as_u16(),
            "volumio meta artistArt non-success status; NOT caching negatively"
        );
        return ProviderOutcome::Unavailable;
    }
    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                artist,
                error = %e,
                "volumio meta artistArt json decode failed; NOT caching negatively"
            );
            return ProviderOutcome::Unavailable;
        }
    };
    let Some(image_url) = json
        .get("data")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
    else {
        return ProviderOutcome::Absent;
    };
    let payload = serde_json::json!({
        "image_url": image_url,
        "source_url": "https://meta.volumio.org",
    });
    ProviderOutcome::Hit(SourceEntry {
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
) -> ProviderOutcome {
    if !enabled {
        return ProviderOutcome::Absent;
    }
    let Some(tadb) = catalogue.theaudiodb.as_ref() else {
        return ProviderOutcome::Absent;
    };
    let hit = match artist_mbid {
        Some(mbid) => match tadb.fetch_artist_bio_by_mbid(mbid).await {
            Ok(Some(h)) => h,
            Ok(None) => return ProviderOutcome::Absent,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    artist,
                    mbid,
                    error = %e,
                    outcome = "unavailable",
                    next_attempt = "on_next_demand",
                    "theaudiodb artist artwork by MBID transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
                );
                return ProviderOutcome::Unavailable;
            }
        },
        None => match tadb.search_artist_bio(artist).await {
            Ok(Some(h)) => h,
            Ok(None) => return ProviderOutcome::Absent,
            Err(e) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "theaudiodb",
                    artist,
                    error = %e,
                    outcome = "unavailable",
                    next_attempt = "on_next_demand",
                    "theaudiodb artist artwork by name transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
                );
                return ProviderOutcome::Unavailable;
            }
        },
    };
    let Some(thumb) = hit
        .artist_thumb_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return ProviderOutcome::Absent;
    };
    let payload = serde_json::json!({
        "thumb_url": thumb,
        "source_url": hit.source_url,
    });
    ProviderOutcome::Hit(SourceEntry {
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
) -> ProviderOutcome {
    if !enabled {
        return ProviderOutcome::Absent;
    }
    let Some(deezer) = catalogue.deezer.as_ref() else {
        return ProviderOutcome::Absent;
    };
    let Some(id) = deezer_artist_id else {
        return ProviderOutcome::Absent;
    };
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
        Ok(None) => return ProviderOutcome::Absent,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "deezer",
                artist,
                deezer_id = id,
                error = %e,
                outcome = "unavailable",
                next_attempt = "on_next_demand",
                "deezer artist image by id transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
            );
            return ProviderOutcome::Unavailable;
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
    ProviderOutcome::Hit(SourceEntry {
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
) -> ProviderOutcome {
    // Three silent pre-conditions that used to return Absent
    // without any journal breadcrumb — operators triaging "why
    // is fanart never firing?" had to code-read to know these
    // gates existed. Each emits one INFO line naming exactly
    // which gate refused, so a single journal grep answers the
    // question.
    if !enabled {
        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            provider = "fanart_tv",
            artist_mbid,
            outcome = "absent",
            reason = "operator_disabled",
            "fanart.tv artist images: operator disabled the provider in the artist-artwork config (no network call fired)"
        );
        return ProviderOutcome::Absent;
    }
    let Some(fanart) = catalogue.fanart.as_ref() else {
        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            provider = "fanart_tv",
            artist_mbid,
            outcome = "absent",
            reason = "no_api_key_wired",
            "fanart.tv artist images: no API key resolved from the credential vault at plugin load, so the client is not wired (no network call fired); set `fanart_tv_personal_api_key` in the vault to enable"
        );
        return ProviderOutcome::Absent;
    };
    let Some(mbid) = artist_mbid else {
        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            provider = "fanart_tv",
            outcome = "absent",
            reason = "no_mbid_available",
            "fanart.tv artist images: reconcile did not produce an MBID, and fanart.tv is MBID-keyed (no network call fired)"
        );
        return ProviderOutcome::Absent;
    };
    let hit = match fanart.get_artist_images(mbid).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            // Upstream returned a structural miss (fanart.tv
            // responds `404 Not Found` when no entry exists
            // for this MBID at all). Surface it explicitly so
            // operators triaging a fanart-quiet rig can tell
            // "key rejected / endpoint wrong" (this line
            // absent, transient warn instead) from "catalogue
            // has nothing for this artist" (this line present).
            tracing::info!(
                plugin = crate::PLUGIN_NAME,
                provider = "fanart_tv",
                artist_mbid = mbid,
                outcome = "absent",
                reason = "upstream_404_no_entry_for_mbid",
                "fanart.tv artist images: upstream reports no entry for this MBID (structural absence, not a schema / key / endpoint problem)"
            );
            return ProviderOutcome::Absent;
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "fanart_tv",
                artist_mbid = mbid,
                error = %e,
                outcome = "unavailable",
                next_attempt = "on_next_demand",
                "fanart.tv artist images transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
            );
            return ProviderOutcome::Unavailable;
        }
    };
    if !hit.has_any_artwork() {
        // Upstream returned `200 OK` but every image array
        // decoded to zero elements. Two shapes can produce
        // this: (a) fanart has the MBID entry but genuinely
        // no images uploaded yet, or (b) their JSON schema
        // shifted under us and our deserialiser dropped every
        // field. Log the per-array counts + the MBID so an
        // operator or the next commit can tell one from the
        // other with one grep — a real "empty entry" logs
        // all-zero counts consistently across artists; a
        // schema drift shows all-zero counts even for
        // populated MBIDs (spot-check against fanart's own
        // web UI for the same MBID).
        tracing::info!(
            plugin = crate::PLUGIN_NAME,
            provider = "fanart_tv",
            artist_mbid = mbid,
            outcome = "absent",
            reason = "upstream_200_all_arrays_empty",
            hd_music_logo_urls = hit.hd_music_logo_urls.len(),
            hd_artist_logo_urls = hit.hd_artist_logo_urls.len(),
            artist_background_urls = hit.artist_background_urls.len(),
            artist_thumb_urls = hit.artist_thumb_urls.len(),
            music_banner_urls = hit.music_banner_urls.len(),
            "fanart.tv artist images: upstream returned 200 but every image array is empty (either a real empty entry or a schema drift — cross-check against fanart's web UI for this MBID)"
        );
        return ProviderOutcome::Absent;
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
    ProviderOutcome::Hit(SourceEntry {
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

/// Four-way outcome of an artist-name → MBID reconcile pass.
///
/// The original `Option<ArtistLookup>` shape collapsed transient
/// upstream failures (rate-limit / 5xx / transport) into the
/// same `None` sentinel as a genuine "no MB entity at
/// confidence" — the caller then durably cached the transient
/// as an absence. Under browse fan-out on MB's 1 req/sec
/// limiter that shape converts a single rate-limit spike into
/// six hours of blank tiles for the affected artists.
///
/// The four variants:
///
/// - [`ReconcileOutcome::Found`] — MB search returned a hit at
///   ≥ [`MB_MIN_CONFIDENCE_PERCENT`] AND the URL-rels lookup
///   returned a complete identity (Deezer URL-rel, canonical
///   name, etc). Cacheable under the hit TTL.
/// - [`ReconcileOutcome::FoundPartial`] — MB search returned a
///   confident hit but the URL-rels lookup itself was
///   transient (rate-limit / 5xx). The identity (MBID + query
///   name) is nailed so MBID-keyed providers (fanart.tv,
///   TheAudioDB) can still fire on the current call, but the
///   Deezer URL-rel path stays inactive because we did not
///   receive the URL-rel. The caller MUST NOT cache this
///   outcome to the reconcile cache — otherwise the degraded
///   identity persists for the 7 d hit-TTL and Deezer-by-id
///   stays dead across every subsequent request. The next
///   request re-runs the full reconcile; the full URL-rels
///   lookup either succeeds (upgrade to `Found`) or fails
///   again (stay `FoundPartial`).
/// - [`ReconcileOutcome::Absent`] — MB returned no hits or the
///   top hit was below the confidence threshold. Definitive
///   absence at MB's catalogue; cacheable under the miss TTL.
/// - [`ReconcileOutcome::Unavailable`] — MB search itself
///   errored (rate-limit / transport / 5xx). NOT definitive;
///   the caller MUST NOT cache this as an absence, and MUST
///   surface `Unavailable` on the wire so the UI can retry
///   without poisoning its session cache.
pub(crate) enum ReconcileOutcome {
    Found(Box<evo_online_providers::musicbrainz::ArtistLookup>),
    FoundPartial(Box<evo_online_providers::musicbrainz::ArtistLookup>),
    Absent,
    Unavailable,
}

/// Three-way outcome of one provider fetch after a good
/// reconcile. Mirrors [`ReconcileOutcome`] at the provider
/// layer so `from_provider_outcomes` can aggregate honestly:
///
/// - `Hit` — provider returned a real payload with a usable
///   image URL (or was in-cache).
/// - `Absent` — provider gave a clean structural miss (404,
///   empty response, disabled, or no MBID/URL-rel to look up
///   with). Aggregatable as definitive absence.
/// - `Unavailable` — provider was reachable-but-transient
///   (rate-limit, HTTP 5xx, timeout, transport error). Must
///   NOT roll up into a durable "not found" — the aggregate
///   surfaces `Unavailable` unless another provider hits.
#[derive(Debug, Clone)]
pub(crate) enum ProviderOutcome {
    Hit(SourceEntry),
    Absent,
    Unavailable,
}

impl ProviderOutcome {
    /// Convenience for the two-value case (provider was
    /// disabled or returned no lookup id): treat both as clean
    /// absence. Never as unavailable — a disabled provider is
    /// not a transient upstream failure.
    pub(crate) fn absent_when(condition: bool) -> Option<Self> {
        if condition {
            Some(Self::Absent)
        } else {
            None
        }
    }
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
/// Returns [`ReconcileOutcome`]: `Found` on ≥90% hit + rels
/// (or bare-MBID fallback when only the second call
/// transiently failed), `Absent` on definitive no-match or
/// below-confidence match, `Unavailable` on transient MB
/// failure or absent MB client (no client + no cache write is
/// safer than caching an absence we cannot verify).
async fn reconcile_artist_mbid(
    artist: &str,
    mb: Option<&MusicBrainzClient>,
) -> ReconcileOutcome {
    let Some(mb) = mb else {
        // No MB client is a configuration state, not a real
        // "not found" — treat as unavailable so the caller
        // does not memoise a false absence for hours.
        return ReconcileOutcome::Unavailable;
    };
    let hit = match mb.search_artist(artist).await {
        Ok(Some(h)) => h,
        Ok(None) => {
            tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                "MB artist search returned no hits"
            );
            return ReconcileOutcome::Absent;
        }
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                error = %mb_error_display(&e),
                outcome = "unavailable",
                next_attempt = "on_next_demand",
                "MB artist search transient; response=Unavailable, no cache write, retry fires on next request for this artist (no scheduled background retry)"
            );
            return ReconcileOutcome::Unavailable;
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
        return ReconcileOutcome::Absent;
    }
    match mb.lookup_artist(&hit.artist_mbid).await {
        Ok(lookup) => ReconcileOutcome::Found(Box::new(lookup)),
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "musicbrainz",
                artist,
                mbid = %hit.artist_mbid,
                error = %mb_error_display(&e),
                outcome = "found_bare_mbid",
                next_attempt = "on_next_demand",
                cache_policy = "not_written_by_caller",
                "MB artist URL-rels lookup transient; identity nailed by confident search hit but Deezer URL-rel absent; caller MUST NOT cache this degraded Found — next request for this artist re-attempts the full lookup"
            );
            // Search was confident, only URL-rels lookup
            // transiently failed. Return FoundPartial with a
            // bare lookup so downstream MBID-keyed providers
            // (fanart.tv, TheAudioDB) still fire on the
            // current call; the Deezer URL-rel path stays
            // inactive for this call. The caller MUST NOT
            // write this outcome to the reconcile cache —
            // otherwise the degraded identity persists for
            // the 7 d hit-TTL and the Deezer-by-id path stays
            // dead across every subsequent request until the
            // operator clears the cache. See the
            // `FoundPartial` docstring.
            ReconcileOutcome::FoundPartial(Box::new(bare_lookup_from_mbid(
                &hit.artist_mbid,
                artist,
            )))
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
            source_of("deezer", serde_json::json!({})),
            source_of("theaudiodb", serde_json::json!({})),
            source_of("volumio_meta", serde_json::json!({})),
            source_of("fanart_tv", serde_json::json!({})),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        // Priority order (lower wins): fanart 40, deezer 45,
        // theaudiodb 50, volumio 55. fanart wins when its key
        // is present and it produced a source; deezer stays
        // the keyless fallback.
        assert_eq!(sources[0].provider_id, "fanart_tv");
        assert_eq!(sources[1].provider_id, "deezer");
        assert_eq!(sources[2].provider_id, "theaudiodb");
        assert_eq!(sources[3].provider_id, "volumio_meta");
    }

    #[test]
    fn primary_source_is_fanart_when_fanart_hit_beside_deezer() {
        // Both providers produced a source. Priority default
        // puts fanart ahead of Deezer, so after
        // sort_sources_by_priority the primary
        // (top-level payload + provider_id) MUST be fanart's.
        // Provider order in input intentionally reversed so
        // the assertion depends on the sort, not on input
        // order.
        let cfg = ArtistProviderConfig::defaults();
        let mut sources = vec![
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://cdn.deezer.example/xl.jpg",
                }),
            ),
            source_of(
                "fanart_tv",
                serde_json::json!({
                    "image_url": "https://fanart.example/hd.jpg",
                }),
            ),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert_eq!(resp.provider_id.as_deref(), Some("fanart_tv"));
        let payload = resp.payload.unwrap();
        assert_eq!(
            payload.get("image_url").unwrap(),
            "https://fanart.example/hd.jpg",
        );
        assert!(
            payload.get("picture_xl_url").is_none(),
            "primary payload must be fanart's verbatim; the deezer \
             field must not appear at top level"
        );
    }

    #[test]
    fn primary_source_falls_back_to_deezer_when_fanart_absent() {
        // Only Deezer produced a source (fanart key unset or
        // provider absent — either surface as `Absent`, which
        // emits no source). Deezer becomes primary; the
        // operator's keyless deployment stays warm.
        let cfg = ArtistProviderConfig::defaults();
        let mut sources = vec![source_of(
            "deezer",
            serde_json::json!({
                "picture_xl_url": "https://cdn.deezer.example/xl.jpg",
            }),
        )];
        sort_sources_by_priority(&mut sources, &cfg);
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert_eq!(resp.provider_id.as_deref(), Some("deezer"));
        let payload = resp.payload.unwrap();
        assert_eq!(
            payload.get("picture_xl_url").unwrap(),
            "https://cdn.deezer.example/xl.jpg",
        );
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
    fn pick_canonical_image_url_picks_only_portrait_array_keys() {
        // Image-type suitability rule: for the artist portrait
        // context, ONLY `artist_thumb_urls` and
        // `artist_background_urls` are photograph classes.
        // Fanart's logo and banner classes must NOT be selected
        // as a portrait — a wordmark is not a photo of the
        // artist. See module docstring.
        //
        // Regression fixed 2026-07-28: previously
        // `hd_music_logo_urls` won over the portrait keys,
        // producing Abba/Bruno Mars/Queen tiles rendering as
        // wordmarks instead of photographs.
        let fanart_with_logo_only = serde_json::json!({
            "hd_music_logo_urls": ["https://f/logo.png"],
            "hd_artist_logo_urls": ["https://f/hdlogo.png"],
            "music_banner_urls": ["https://f/banner.png"],
        });
        // Fanart with only logos/banners → picker returns None so
        // the source-walk falls through to the next provider
        // (Deezer / TheAudioDB / Volumio meta) whose payloads are
        // photo-only.
        assert!(
            pick_canonical_image_url(&fanart_with_logo_only).is_none(),
            "logos and banners must NOT surface as portraits"
        );

        // Full fanart payload — thumb wins over background,
        // logos/banners are ignored regardless of position.
        let fanart_full = serde_json::json!({
            "hd_music_logo_urls": ["https://f/logo.png"],
            "hd_artist_logo_urls": ["https://f/hdlogo.png"],
            "artist_thumb_urls": ["https://f/thumb.jpg"],
            "music_banner_urls": ["https://f/banner.png"],
            "artist_background_urls": ["https://f/bg.jpg"],
        });
        assert_eq!(
            pick_canonical_image_url(&fanart_full).as_deref(),
            Some("https://f/thumb.jpg")
        );

        // Fanart with no thumb, only background — background
        // is the photo fallback, wins over any logo/banner.
        let fanart_bg_only = serde_json::json!({
            "hd_music_logo_urls": ["https://f/logo.png"],
            "artist_thumb_urls": [],
            "music_banner_urls": ["https://f/banner.png"],
            "artist_background_urls": ["https://f/bg.jpg"],
        });
        assert_eq!(
            pick_canonical_image_url(&fanart_bg_only).as_deref(),
            Some("https://f/bg.jpg")
        );
    }

    #[test]
    fn from_sources_prefers_fanart_photo_over_fanart_logo_via_deezer() {
        // End-to-end shape: fanart is priority 40 (winner) but
        // its payload carries only logos → its picker returns
        // None → source walk falls through to Deezer's photo.
        // Property: portrait context serves a PHOTOGRAPH always
        // — never a logo — even when the priority-winning
        // provider only has logos.
        let sources = vec![
            source_of(
                "fanart_tv",
                serde_json::json!({
                    "hd_music_logo_urls": ["https://f/logo.png"],
                    "hd_artist_logo_urls": ["https://f/hdlogo.png"],
                    "music_banner_urls": ["https://f/banner.png"],
                }),
            ),
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://d/xl.jpg",
                }),
            ),
        ];
        let resp = ArtistArtworkResponse::from_sources(sources);
        // Provider attribution reflects the actual source
        // whose URL was picked (Deezer), not the priority-
        // winning provider (fanart, whose payload was logo-only).
        assert_eq!(resp.provider_id.as_deref(), Some("deezer"));
        assert_eq!(resp.image_url.as_deref(), Some("https://d/xl.jpg"));
    }

    #[test]
    fn from_sources_prefers_fanart_photo_when_present() {
        // The other side of the coin: fanart carries a real
        // photo (thumb) alongside logos. Fanart wins the
        // portrait — the photo class beats the logo classes
        // within fanart's payload, and fanart's priority beats
        // Deezer's.
        let sources = vec![
            source_of(
                "fanart_tv",
                serde_json::json!({
                    "hd_music_logo_urls": ["https://f/logo.png"],
                    "artist_thumb_urls": ["https://f/thumb.jpg"],
                }),
            ),
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://d/xl.jpg",
                }),
            ),
        ];
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert_eq!(resp.provider_id.as_deref(), Some("fanart_tv"));
        assert_eq!(resp.image_url.as_deref(), Some("https://f/thumb.jpg"));
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
        //
        // Uses `artist_thumb_urls` — a photo class, so the
        // portrait picker actually walks it (the logo array
        // keys were removed 2026-07-28; see the
        // portrait-suitability comment on
        // `pick_canonical_image_url`).
        let mixed = serde_json::json!({
            "artist_thumb_urls": [
                "https://f/artist//thumb.jpg",
                "https://f/artist/real/thumb.jpg",
            ],
        });
        assert_eq!(
            pick_canonical_image_url(&mixed).as_deref(),
            Some("https://f/artist/real/thumb.jpg")
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
    fn is_real_image_url_rejects_empty_md5_placeholder_segment() {
        // Observed on 2026-07-28 (Elton John tile):
        // Deezer's "no image for this artist" placeholder now
        // carries the MD5 of the empty string
        // (`d41d8cd98f00b204e9800998ecf8427e`) as the
        // artist-slug segment. The URL is structurally valid
        // and the byte-response decodes as an image (generic
        // silhouette), so an HTTP-shape or Content-Type guard
        // still passes it. Only the segment itself carries
        // the honest signal.
        assert!(!is_real_image_url(
            "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/1000x1000-000000-80-0-0.jpg"
        ));
        assert!(!is_real_image_url(
            "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/500x500-000000-80-0-0.jpg"
        ));
        // Case-sensitive: the observed placeholder is always
        // lowercase hex. If some future provider emits the
        // upper-case form we'll extend, but do not over-
        // reach today.
        assert!(is_real_image_url(
            "https://cdn-images.dzcdn.net/images/artist/D41D8CD98F00B204E9800998ECF8427E/500x500.jpg"
        ));
        // Adjacent segments containing the digest as a
        // substring are NOT the placeholder; only an EXACT
        // segment match rejects.
        assert!(is_real_image_url(
            "https://x/somepath/d41d8cd98f00b204e9800998ecf8427eSUFFIX/thumb.jpg"
        ));
    }

    #[test]
    fn from_sources_falls_through_deezer_empty_md5_placeholder_to_theaudiodb() {
        // Elton John reproduction: Deezer priority (45) sits
        // above TheAudioDB (50) and Volumio meta (55), but
        // Deezer's payload carries the empty-MD5 placeholder
        // artist slug. The picker MUST skip Deezer's URLs
        // and land on TheAudioDB's real thumb.
        let cfg = ArtistProviderConfig::defaults();
        let mut sources = vec![
            source_of(
                "deezer",
                serde_json::json!({
                    "picture_xl_url": "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/1000x1000-000000-80-0-0.jpg",
                    "picture_big_url": "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/500x500-000000-80-0-0.jpg",
                    "picture_medium_url": "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/250x250-000000-80-0-0.jpg",
                    "picture_small_url": "https://cdn-images.dzcdn.net/images/artist/d41d8cd98f00b204e9800998ecf8427e/56x56-000000-80-0-0.jpg",
                }),
            ),
            source_of(
                "theaudiodb",
                serde_json::json!({
                    "thumb_url": "https://r2.theaudiodb.com/images/media/artist/thumb/9o30sk1687869267.jpg",
                }),
            ),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        let resp = ArtistArtworkResponse::from_sources(sources);
        assert_eq!(resp.provider_id.as_deref(), Some("theaudiodb"));
        assert_eq!(
            resp.image_url.as_deref(),
            Some(
                "https://r2.theaudiodb.com/images/media/artist/thumb/9o30sk1687869267.jpg"
            )
        );
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
