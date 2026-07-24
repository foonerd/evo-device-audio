// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

// Cascade module ships the full ProviderId / PrivacyMode /
// ProviderCatalogue / ProviderConfig taxonomy that every verb
// cascade consumes. The module-level allow accommodates
// variants whose consumer verb is not yet on the cascade
// while the migration proceeds; the check re-tightens
// naturally as the remaining verbs land.
#![allow(dead_code)]

//! Keyless-first metadata enrichment cascade.
//!
//! Every text-enrichment verb resolves through an ordered
//! provider cascade: anonymous providers first, identity-bearing
//! providers as opt-in enhancement. A missing key never yields
//! a terminal empty for any verb that has an enabled anonymous
//! provider.
//!
//! ## Provider taxonomy
//!
//! - **Anonymous** — no account required, nothing beyond the
//!   query itself leaves the device: MusicBrainz, Wikipedia,
//!   Wikidata, Cover Art Archive, iTunes, LRCLIB.
//! - **Identity-bearing** — an API key ties the query to a
//!   registered account: Last.fm, Discogs, Genius, fanart.tv.
//!
//! ## Entity taxonomy
//!
//! Enrichment is entity-typed, not single-artist. A bio /
//! notes / credits request names an entity `{ type, name, mbid? }`
//! where `type` is one of `artist | composer | work | performer |
//! conductor | ensemble`. The driver is the local classical
//! projection already on the wire: when a track carries
//! composer / work / performer tags, the cascade enriches
//! those entities; otherwise it enriches the single artist.
//! Every entity resolves through the same anonymous-first
//! cascade — MusicBrainz relationship / lookup + Wikipedia /
//! Wikidata are the authoritative keyless classical sources.
//!
//! ## Per-provider selection
//!
//! Every provider is independently enable/disable-able and
//! orderable by the operator. The cascade walks only ENABLED
//! providers, in the operator's priority order. Rules:
//!
//! - Anonymous providers default enabled.
//! - Identity-bearing providers are `not_configured` until their
//!   credential is in the vault; once present, they become
//!   enable-able ("operate without, extend with").
//! - Disabled providers are never dispatched, regardless of key
//!   presence or privacy mode.
//! - Cache entries are keyed by `(verb, entity_type, entity,
//!   provider_id)` so a disabled or missed provider never
//!   suppresses a different provider's result.
//! - Transient errors are never cached (mirrors the artwork
//!   cascade's transient-not-cached fix).
//!
//! ## Response shape
//!
//! The cascade response carries the winning provider's
//! `provider_id`, `privacy_class`, payload, plus attribution
//! (source_name / source_url / license) and an optional
//! `enhancement` hint pointing at providers the operator could
//! enable to enrich the answer further.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The canonical entity taxonomy for the cascade.
///
/// Classical audio needs distinct enrichment for composer /
/// work / performer / conductor / ensemble; pop / rock is
/// covered by the single `Artist` case. All six values
/// resolve through the same cascade shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntityType {
    Artist,
    Composer,
    Work,
    Performer,
    Conductor,
    Ensemble,
}

impl EntityType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EntityType::Artist => "artist",
            EntityType::Composer => "composer",
            EntityType::Work => "work",
            EntityType::Performer => "performer",
            EntityType::Conductor => "conductor",
            EntityType::Ensemble => "ensemble",
        }
    }
}

/// A named entity to enrich. `mbid` is optional — the cascade
/// resolves it via MusicBrainz search when absent.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EntityRef {
    #[serde(rename = "type")]
    pub(crate) entity_type: EntityType,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) mbid: Option<String>,
}

/// Stable provider identifier. Every provider the cascade knows
/// about is enumerated here so the enable/priority config and
/// the response `provider_id` field share one authoritative list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProviderId {
    // Anonymous — no key required.
    MusicBrainz,
    Wikipedia,
    Wikidata,
    Lrclib,
    // Identity-bearing — API key required.
    Lastfm,
    Discogs,
    Genius,
}

impl ProviderId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProviderId::MusicBrainz => "musicbrainz",
            ProviderId::Wikipedia => "wikipedia",
            ProviderId::Wikidata => "wikidata",
            ProviderId::Lrclib => "lrclib",
            ProviderId::Lastfm => "lastfm",
            ProviderId::Discogs => "discogs",
            ProviderId::Genius => "genius",
        }
    }

    pub(crate) fn privacy_class(self) -> PrivacyClass {
        match self {
            ProviderId::MusicBrainz
            | ProviderId::Wikipedia
            | ProviderId::Wikidata
            | ProviderId::Lrclib => PrivacyClass::Anonymous,
            ProviderId::Lastfm | ProviderId::Discogs | ProviderId::Genius => {
                PrivacyClass::IdentityBearing
            }
        }
    }
}

/// Whether a provider requires operator credentials to query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrivacyClass {
    Anonymous,
    IdentityBearing,
}

impl PrivacyClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PrivacyClass::Anonymous => "anonymous",
            PrivacyClass::IdentityBearing => "identity_bearing",
        }
    }
}

/// Provenance a cascade response carries so the operator UI can
/// render the required attribution alongside any payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct Attribution {
    /// Human-readable provider name for the UI attribution line
    /// (`"MusicBrainz"`, `"Wikipedia"`, `"Wikidata"`, `"Last.fm"`,
    /// etc.).
    pub(crate) source_name: String,
    /// Canonical URL back to the provider's page for the resolved
    /// entity. The UI renders this as a "View on `<source>`"
    /// affordance.
    pub(crate) source_url: Option<String>,
    /// License string. `"CC BY-SA"` for Wikipedia, `"CC0"` for
    /// MusicBrainz + Wikidata, provider-terms strings for
    /// identity-bearing providers.
    pub(crate) license: String,
}

/// Hint the cascade attaches to a response when an additional
/// provider is available if the operator enables it (either
/// flipping the `enabled` flag or supplying a credential).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct EnhancementHint {
    /// Provider id string (e.g. `"lastfm"`).
    pub(crate) provider: String,
    /// True when enabling this provider requires the operator to
    /// supply a credential first.
    pub(crate) requires_key: bool,
    /// Human-readable one-line explanation for the UI's
    /// "Enable Last.fm for richer bios" affordance.
    pub(crate) reason: String,
}

/// The cascade's per-verb response shape. Replaces the older
/// `EnrichmentResponse` for verbs that flow through the cascade.
///
/// `not_configured` is no longer a terminal status for verbs
/// with an enabled anonymous provider — the anonymous baseline
/// answer sits in `payload` + `provider_id` + `attribution`
/// while any relevant identity-bearing provider that could
/// enrich the answer surfaces as an `enhancement` hint.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CascadeResponse {
    pub(crate) v: u8,
    /// `"ok"` when at least one provider hit; `"not_found"` when
    /// every enabled provider structurally-missed;
    /// `"not_configured"` only when NO providers are enabled
    /// (or the verb has no anonymous provider); `"bad_request"`
    /// on caller-input errors.
    pub(crate) status: CascadeStatus,
    /// Winning provider id when the status is `"ok"`. Otherwise
    /// the last-tried provider so the UI can label an honest
    /// "we asked X and nothing was there" surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_id: Option<String>,
    /// Winning provider's privacy class. Absent when the status
    /// is `"not_configured"` (no provider was tried).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) privacy_class: Option<String>,
    /// The resolved content payload. Shape per verb.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<serde_json::Value>,
    /// Operator-readable explanation on any non-`ok` status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    /// Attribution the operator UI MUST render alongside a
    /// non-empty payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attribution: Option<Attribution>,
    /// Optional hint pointing at a provider the operator could
    /// enable / configure to enrich the answer further.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enhancement: Option<EnhancementHint>,
}

/// Wire-serialised status for the cascade response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CascadeStatus {
    Ok,
    NotFound,
    NotConfigured,
    BadRequest,
}

impl CascadeResponse {
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
            enhancement: None,
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
            enhancement: None,
        }
    }
}

/// The catalogue of provider handles + per-provider enable/order
/// config the cascade orchestrator walks.
///
/// The plugin constructs one at load time (from the config +
/// vault-fetched credentials) and passes a reference to every
/// verb handler.
pub(crate) struct ProviderCatalogue {
    pub(crate) musicbrainz:
        Option<Arc<evo_online_providers::musicbrainz::MusicBrainzClient>>,
    pub(crate) wikipedia:
        Option<Arc<evo_online_providers::wikipedia::WikipediaClient>>,
    pub(crate) wikidata:
        Option<Arc<evo_online_providers::wikidata::WikidataClient>>,
    pub(crate) lrclib: Option<Arc<evo_online_providers::lrclib::LrclibClient>>,
    pub(crate) lastfm: Option<Arc<evo_online_providers::lastfm::LastfmClient>>,
    pub(crate) discogs:
        Option<Arc<evo_online_providers::discogs::DiscogsClient>>,
    pub(crate) genius: Option<Arc<evo_online_providers::genius::GeniusClient>>,
    /// Operator-controlled per-provider enable/order.
    pub(crate) config: ProviderConfig,
}

/// Per-provider enable + priority map — one entry per provider
/// the plugin knows about. Populated from the plugin's operator
/// config (`plugins.d/org.evoframework.metadata.online.toml`
/// `[providers.<provider_id>]` sections) at load time.
///
/// Defaults: anonymous providers enabled; identity-bearing
/// providers enabled iff their credential is present at load
/// (identity-bearing without a credential is treated as
/// disabled — the cascade skips it).
#[derive(Debug, Clone)]
pub(crate) struct ProviderConfig {
    pub(crate) musicbrainz: ProviderFlags,
    pub(crate) wikipedia: ProviderFlags,
    pub(crate) wikidata: ProviderFlags,
    pub(crate) lrclib: ProviderFlags,
    pub(crate) lastfm: ProviderFlags,
    pub(crate) discogs: ProviderFlags,
    pub(crate) genius: ProviderFlags,
    /// Privacy-mode preset layered on top of the per-provider
    /// flags. `anonymous_only` disables every identity-bearing
    /// provider en masse while it is selected; `offline`
    /// disables every network provider; `enhanced` is the
    /// no-op default.
    pub(crate) privacy_mode: PrivacyMode,
}

impl ProviderConfig {
    /// Framework defaults: anonymous providers enabled, identity-
    /// bearing providers enabled (they self-skip when their
    /// credential is absent), privacy_mode = enhanced.
    pub(crate) fn defaults() -> Self {
        Self {
            musicbrainz: ProviderFlags {
                enabled: true,
                priority: 10,
            },
            wikipedia: ProviderFlags {
                enabled: true,
                priority: 20,
            },
            wikidata: ProviderFlags {
                enabled: true,
                priority: 30,
            },
            lrclib: ProviderFlags {
                enabled: true,
                priority: 40,
            },
            lastfm: ProviderFlags {
                enabled: true,
                priority: 50,
            },
            discogs: ProviderFlags {
                enabled: true,
                priority: 60,
            },
            genius: ProviderFlags {
                enabled: true,
                priority: 70,
            },
            privacy_mode: PrivacyMode::Enhanced,
        }
    }

    /// True when the (provider, privacy_mode) combination
    /// permits the cascade to dispatch to the provider. Combines
    /// the per-provider `enabled` flag with the privacy-mode
    /// preset; the privacy-mode layer is non-bypassable and
    /// always wins over the per-provider flag.
    pub(crate) fn is_effectively_enabled(&self, provider: ProviderId) -> bool {
        if matches!(self.privacy_mode, PrivacyMode::Offline) {
            return false;
        }
        if matches!(self.privacy_mode, PrivacyMode::AnonymousOnly)
            && matches!(provider.privacy_class(), PrivacyClass::IdentityBearing)
        {
            return false;
        }
        self.flags(provider).enabled
    }

    pub(crate) fn flags(&self, provider: ProviderId) -> ProviderFlags {
        *self.flags_ref(provider)
    }

    /// Read-only reference to a provider's flags. Config parsing
    /// uses this to fetch the framework default before applying
    /// operator overrides.
    pub(crate) fn flags_ref(&self, provider: ProviderId) -> &ProviderFlags {
        match provider {
            ProviderId::MusicBrainz => &self.musicbrainz,
            ProviderId::Wikipedia => &self.wikipedia,
            ProviderId::Wikidata => &self.wikidata,
            ProviderId::Lrclib => &self.lrclib,
            ProviderId::Lastfm => &self.lastfm,
            ProviderId::Discogs => &self.discogs,
            ProviderId::Genius => &self.genius,
        }
    }

    /// Overwrite a provider's flags block. Config parsing uses
    /// this to apply operator overrides.
    pub(crate) fn set_flags(
        &mut self,
        provider: ProviderId,
        flags: ProviderFlags,
    ) {
        match provider {
            ProviderId::MusicBrainz => self.musicbrainz = flags,
            ProviderId::Wikipedia => self.wikipedia = flags,
            ProviderId::Wikidata => self.wikidata = flags,
            ProviderId::Lrclib => self.lrclib = flags,
            ProviderId::Lastfm => self.lastfm = flags,
            ProviderId::Discogs => self.discogs = flags,
            ProviderId::Genius => self.genius = flags,
        }
    }
}

/// Per-provider enable + priority pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderFlags {
    pub(crate) enabled: bool,
    /// Cascade priority. Lower wins first. Providers with equal
    /// priority walk in the fixed order defined by `ProviderId`.
    pub(crate) priority: u32,
}

/// The `metadata.privacy_mode` preset layered on top of the
/// per-provider flags. Non-bypassable: `AnonymousOnly`
/// disables every identity-bearing provider en masse;
/// `Offline` disables every provider en masse; `Enhanced`
/// leaves the per-provider selection intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivacyMode {
    /// The operator's per-provider selection stands as-is —
    /// framework default.
    Enhanced,
    /// Non-bypassable: every identity-bearing provider is
    /// treated as disabled while this mode is selected.
    /// Individual identity-bearing providers can still be
    /// re-enabled by the operator by switching back to
    /// `enhanced` — the preset does not delete their per-
    /// provider flag, only overrides at dispatch time.
    AnonymousOnly,
    /// Non-bypassable: every network provider is treated as
    /// disabled. Local file tags only.
    Offline,
}

impl PrivacyMode {
    pub(crate) fn parse_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "enhanced" => Some(PrivacyMode::Enhanced),
            "anonymous_only" => Some(PrivacyMode::AnonymousOnly),
            "offline" => Some(PrivacyMode::Offline),
            _ => None,
        }
    }
}

/// Order a list of providers by cascade priority (lower wins
/// first). Providers with equal priority preserve their input
/// order, so the caller's default sequence stays stable.
pub(crate) fn ordered_by_priority(
    providers: &[ProviderId],
    config: &ProviderConfig,
) -> Vec<ProviderId> {
    let mut with_prio: Vec<(usize, ProviderId, u32)> = providers
        .iter()
        .enumerate()
        .map(|(i, p)| (i, *p, config.flags(*p).priority))
        .collect();
    with_prio.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    with_prio.into_iter().map(|(_, p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_by_priority_walks_lowest_first() {
        let mut cfg = ProviderConfig::defaults();
        cfg.wikipedia.priority = 5;
        cfg.musicbrainz.priority = 10;
        let ordered = ordered_by_priority(
            &[ProviderId::MusicBrainz, ProviderId::Wikipedia],
            &cfg,
        );
        assert_eq!(
            ordered,
            vec![ProviderId::Wikipedia, ProviderId::MusicBrainz]
        );
    }

    #[test]
    fn privacy_mode_anonymous_only_disables_identity_bearing() {
        let mut cfg = ProviderConfig::defaults();
        cfg.privacy_mode = PrivacyMode::AnonymousOnly;
        assert!(!cfg.is_effectively_enabled(ProviderId::Lastfm));
        assert!(!cfg.is_effectively_enabled(ProviderId::Discogs));
        assert!(cfg.is_effectively_enabled(ProviderId::MusicBrainz));
        assert!(cfg.is_effectively_enabled(ProviderId::Wikipedia));
    }

    #[test]
    fn privacy_mode_offline_disables_everything() {
        let mut cfg = ProviderConfig::defaults();
        cfg.privacy_mode = PrivacyMode::Offline;
        for p in [
            ProviderId::MusicBrainz,
            ProviderId::Wikipedia,
            ProviderId::Wikidata,
            ProviderId::Lrclib,
            ProviderId::Lastfm,
            ProviderId::Discogs,
            ProviderId::Genius,
        ] {
            assert!(!cfg.is_effectively_enabled(p), "{p:?} must be disabled");
        }
    }
}
