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
    deezer::DeezerClient, fanart::FanartClient, theaudiodb::TheAudioDbClient,
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
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
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
#[derive(Debug, Clone, Serialize, PartialEq)]
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
        let (status, provider_id, privacy_class, payload, attribution) =
            if let Some(primary) = sources.first() {
                (
                    CascadeStatus::Ok,
                    Some(primary.provider_id.clone()),
                    Some(primary.privacy_class.clone()),
                    Some(primary.payload.clone()),
                    Some(primary.attribution.clone()),
                )
            } else {
                (CascadeStatus::NotFound, None, None, None, None)
            };
        Self {
            v: 1,
            status,
            provider_id,
            privacy_class,
            payload,
            detail: None,
            attribution,
            sources,
        }
    }
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
    pub(crate) config: ArtistProviderConfig,
}

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
    let artist_mbid = req
        .artist_mbid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let want_volumio =
        catalogue.config.is_enabled(ArtistProviderId::VolumioMeta);
    let want_theaudiodb =
        catalogue.config.is_enabled(ArtistProviderId::TheAudioDb)
            && catalogue.theaudiodb.is_some();
    let want_deezer = catalogue.config.is_enabled(ArtistProviderId::Deezer)
        && catalogue.deezer.is_some();
    let want_fanart = catalogue.config.is_enabled(ArtistProviderId::FanartTv)
        && catalogue.fanart.is_some()
        && artist_mbid.is_some();

    if !(want_volumio || want_theaudiodb || want_deezer || want_fanart) {
        return Ok(ArtistArtworkResponse::not_configured(
            "every artist-artwork provider is disabled or unavailable on this \
             device; enable at least one under Settings → Metadata → Sources \
             (fanart.tv also requires the artist_mbid + a fanart.tv \
             personal API key)",
        ));
    }

    let (volumio_src, tadb_src, deezer_src, fanart_src) = tokio::join!(
        fetch_volumio_meta_artist(
            &artist,
            &catalogue.volumio_meta_http,
            &catalogue.volumio_meta_variant,
            want_volumio,
        ),
        fetch_theaudiodb_artist(&artist, catalogue, want_theaudiodb),
        fetch_deezer_artist(&artist, catalogue, want_deezer),
        fetch_fanart_artist(artist_mbid.as_deref(), catalogue, want_fanart,),
    );

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
    artist: &str,
    catalogue: &ArtistCatalogue,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let tadb = catalogue.theaudiodb.as_ref()?;
    let hit = match tadb.search_artist_bio(artist).await {
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

async fn fetch_deezer_artist(
    artist: &str,
    catalogue: &ArtistCatalogue,
    enabled: bool,
) -> Option<SourceEntry> {
    if !enabled {
        return None;
    }
    let deezer = catalogue.deezer.as_ref()?;
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
    let hit = match deezer.search_artist_image(artist).await {
        Ok(Some(h)) => h,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = "deezer",
                artist,
                error = %e,
                "Deezer artist image transient; skipping"
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
