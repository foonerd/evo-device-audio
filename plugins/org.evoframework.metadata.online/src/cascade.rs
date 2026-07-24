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
    TheAudioDb,
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
            ProviderId::TheAudioDb => "theaudiodb",
            ProviderId::Lastfm => "lastfm",
            ProviderId::Discogs => "discogs",
            ProviderId::Genius => "genius",
        }
    }

    /// Inverse of `as_str`. Used when a wire payload or store
    /// entry carries a provider id as a string and the cascade
    /// needs the typed variant. Returns `None` on an unknown id
    /// so the store never crashes the cascade if a stale row
    /// names a retired provider.
    pub(crate) fn from_wire(id: &str) -> Option<Self> {
        match id {
            "musicbrainz" => Some(ProviderId::MusicBrainz),
            "wikipedia" => Some(ProviderId::Wikipedia),
            "wikidata" => Some(ProviderId::Wikidata),
            "lrclib" => Some(ProviderId::Lrclib),
            "theaudiodb" => Some(ProviderId::TheAudioDb),
            "lastfm" => Some(ProviderId::Lastfm),
            "discogs" => Some(ProviderId::Discogs),
            "genius" => Some(ProviderId::Genius),
            _ => None,
        }
    }

    pub(crate) fn privacy_class(self) -> PrivacyClass {
        match self {
            ProviderId::MusicBrainz
            | ProviderId::Wikipedia
            | ProviderId::Wikidata
            | ProviderId::Lrclib
            | ProviderId::TheAudioDb => PrivacyClass::Anonymous,
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

/// One provider's contribution to a cascade response.
///
/// Every provider that returned non-empty content for a query is
/// represented as one `SourceEntry` in the response's `sources`
/// array. The UI renders the operator-selected entry's payload +
/// attribution; the operator switches between entries via the
/// per-source selection surface on the UI side.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct SourceEntry {
    /// Provider id string (`"wikipedia"`, `"lastfm"`,
    /// `"theaudiodb"`, etc.).
    pub(crate) provider_id: String,
    /// Provider's privacy class at the time of the fetch.
    pub(crate) privacy_class: String,
    /// The provider's content payload. Shape is verb-specific;
    /// each verb's contract documents its own payload shape.
    pub(crate) payload: serde_json::Value,
    /// Attribution the operator UI MUST render alongside this
    /// entry's payload.
    pub(crate) attribution: Attribution,
}

/// The cascade's per-verb response shape.
///
/// Every provider that returned non-empty content is represented
/// in `sources`, ordered by the operator's per-source priority
/// (highest priority first). Top-level `provider_id` / `payload`
/// / `attribution` mirror `sources[0]` for back-compat with UIs
/// that render a single default. The operator's per-source
/// selection surface reads `sources` directly and lets the
/// operator switch between contributing entries.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CascadeResponse {
    pub(crate) v: u8,
    /// `"ok"` when at least one provider hit; `"not_found"` when
    /// every enabled provider structurally-missed;
    /// `"not_configured"` only when NO providers are enabled
    /// (or the verb has no anonymous provider); `"bad_request"`
    /// on caller-input errors.
    pub(crate) status: CascadeStatus,
    /// Mirrors `sources[0].provider_id` when `sources` is
    /// non-empty. On non-`ok` statuses, the last-tried provider
    /// so the UI can label an honest "we asked X and nothing was
    /// there" surface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_id: Option<String>,
    /// Mirrors `sources[0].privacy_class` when `sources` is
    /// non-empty. Absent when the status is `"not_configured"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) privacy_class: Option<String>,
    /// Mirrors `sources[0].payload` when `sources` is non-empty.
    /// Kept for back-compat; new consumers read `sources`
    /// directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload: Option<serde_json::Value>,
    /// Operator-readable explanation on any non-`ok` status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<String>,
    /// Mirrors `sources[0].attribution` when `sources` is
    /// non-empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) attribution: Option<Attribution>,
    /// Optional hint pointing at a provider the operator could
    /// enable / configure to enrich the answer further. Retires
    /// once per-source enable/disable ships on the UI side and
    /// `sources` becomes the sole surface consumers walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enhancement: Option<EnhancementHint>,
    /// Every provider that returned non-empty content, ordered by
    /// the operator's per-source priority (highest first). Empty
    /// when `status` is not `"ok"`. This is the source of truth
    /// for the response's content; the top-level `provider_id` /
    /// `payload` / `attribution` mirror `sources[0]` for
    /// back-compat.
    #[serde(default)]
    pub(crate) sources: Vec<SourceEntry>,
}

impl CascadeResponse {
    /// Build a `CascadeResponse` from an ordered vector of
    /// `SourceEntry` (highest-priority first).
    ///
    /// The top-level `payload` is the **field-level first-non-empty
    /// merge** across every source in priority order: for each
    /// top-level JSON key, the value from the highest-priority
    /// source that has a non-empty value wins. Fields unique to a
    /// single source come through as-is. This matches the
    /// Jellyfin / beets displayed-default pattern — the top-level
    /// object stays useful even when a single source misses a
    /// field a lower-priority peer covers, while the operator-
    /// selectable per-source view reads `sources` directly.
    ///
    /// Top-level `provider_id` / `privacy_class` / `attribution`
    /// mirror `sources[0]` — the primary source. UIs that render
    /// per-field attribution walk `sources`.
    ///
    /// `status` is `Ok` iff `sources` is non-empty. Callers that
    /// need `not_found` / `not_configured` / `bad_request` use
    /// the existing `bad_request` / `not_configured` constructors.
    pub(crate) fn from_sources(
        sources: Vec<SourceEntry>,
        enhancement: Option<EnhancementHint>,
    ) -> Self {
        let (status, provider_id, privacy_class, payload, attribution) =
            if let Some(primary) = sources.first() {
                (
                    CascadeStatus::Ok,
                    Some(primary.provider_id.clone()),
                    Some(primary.privacy_class.clone()),
                    Some(merge_sources_field_level(&sources)),
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
            enhancement,
            sources,
        }
    }
}

/// Field-level first-non-empty merge across an ordered slice of
/// sources (highest-priority first). Walks the sources in order;
/// for each top-level key encountered, the first source with a
/// non-empty value at that key wins.
///
/// Non-empty is defined as:
/// - JSON `null` — absent
/// - string — non-empty and non-whitespace
/// - array — non-empty
/// - object — non-empty
/// - number / bool — always present
///
/// Returns a JSON object even when the input is empty (the
/// caller of `from_sources` guarantees non-empty input for the
/// merged branch, but the helper is defensive).
pub(crate) fn merge_sources_field_level(
    sources: &[SourceEntry],
) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    for src in sources {
        let obj = match src.payload.as_object() {
            Some(o) => o,
            None => continue,
        };
        for (k, v) in obj {
            if merged.contains_key(k) {
                continue;
            }
            if is_json_value_present(v) {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(merged)
}

fn is_json_value_present(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
    }
}

/// Sort a mutable `sources` vector in place by the operator's
/// per-provider priority (ascending — lower number wins first).
/// Entries whose `provider_id` does not resolve to a known
/// `ProviderId` sink to the tail. Ties preserve input order
/// (stable sort).
pub(crate) fn sort_sources_by_priority(
    sources: &mut [SourceEntry],
    config: &ProviderConfig,
) {
    sources.sort_by_key(|s| {
        ProviderId::from_wire(&s.provider_id)
            .map(|p| config.flags(p).priority)
            .unwrap_or(u32::MAX)
    });
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
            enhancement: None,
            sources: Vec::new(),
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
    pub(crate) theaudiodb:
        Option<Arc<evo_online_providers::theaudiodb::TheAudioDbClient>>,
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
    pub(crate) theaudiodb: ProviderFlags,
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
            theaudiodb: ProviderFlags {
                enabled: true,
                priority: 45,
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
            ProviderId::TheAudioDb => &self.theaudiodb,
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
            ProviderId::TheAudioDb => self.theaudiodb = flags,
            ProviderId::Lastfm => self.lastfm = flags,
            ProviderId::Discogs => self.discogs = flags,
            ProviderId::Genius => self.genius = flags,
        }
    }

    /// Merge a runtime operator override on top of the current
    /// per-provider flag block. Config parsing calls this once
    /// per operator override; the operator's per-source
    /// enable/priority store calls it once per store entry at
    /// request time. Priority-only overrides preserve the existing
    /// enabled bit; enabled-only overrides preserve the existing
    /// priority. When both are supplied both are applied.
    pub(crate) fn merge_override(
        &mut self,
        provider: ProviderId,
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
            ProviderId::TheAudioDb,
            ProviderId::Lastfm,
            ProviderId::Discogs,
            ProviderId::Genius,
        ] {
            assert!(!cfg.is_effectively_enabled(p), "{p:?} must be disabled");
        }
    }

    #[test]
    fn provider_id_wire_round_trip() {
        for p in [
            ProviderId::MusicBrainz,
            ProviderId::Wikipedia,
            ProviderId::Wikidata,
            ProviderId::Lrclib,
            ProviderId::TheAudioDb,
            ProviderId::Lastfm,
            ProviderId::Discogs,
            ProviderId::Genius,
        ] {
            assert_eq!(
                ProviderId::from_wire(p.as_str()),
                Some(p),
                "wire round-trip for {p:?}"
            );
        }
        assert_eq!(ProviderId::from_wire("no-such-provider"), None);
    }

    #[test]
    fn merge_override_preserves_untouched_fields() {
        let mut cfg = ProviderConfig::defaults();
        let baseline_priority = cfg.flags(ProviderId::Lastfm).priority;
        cfg.merge_override(ProviderId::Lastfm, Some(false), None);
        assert!(!cfg.flags(ProviderId::Lastfm).enabled);
        assert_eq!(cfg.flags(ProviderId::Lastfm).priority, baseline_priority);
        cfg.merge_override(ProviderId::Lastfm, None, Some(5));
        assert!(!cfg.flags(ProviderId::Lastfm).enabled);
        assert_eq!(cfg.flags(ProviderId::Lastfm).priority, 5);
    }

    fn source_of(provider_id: &str, payload: serde_json::Value) -> SourceEntry {
        SourceEntry {
            provider_id: provider_id.to_string(),
            privacy_class: PrivacyClass::Anonymous.as_str().to_string(),
            payload,
            attribution: Attribution {
                source_name: provider_id.to_string(),
                source_url: None,
                license: "test".into(),
            },
        }
    }

    #[test]
    fn merge_sources_field_level_takes_first_non_empty() {
        let sources = vec![
            source_of(
                "wikipedia",
                serde_json::json!({"summary": "wp prose", "language": "en"}),
            ),
            source_of(
                "lastfm",
                serde_json::json!({
                    "summary": "lastfm prose",
                    "listeners": 12345,
                    "language": "",
                }),
            ),
            source_of(
                "theaudiodb",
                serde_json::json!({
                    "summary": "tadb prose",
                    "genre": "rock",
                }),
            ),
        ];
        let merged = merge_sources_field_level(&sources);
        let obj = merged.as_object().unwrap();
        assert_eq!(obj.get("summary").unwrap(), "wp prose");
        assert_eq!(obj.get("language").unwrap(), "en");
        assert_eq!(obj.get("listeners").unwrap().as_u64(), Some(12345));
        assert_eq!(obj.get("genre").unwrap(), "rock");
    }

    #[test]
    fn merge_sources_field_level_skips_empty_and_null_and_whitespace() {
        let sources = vec![
            source_of(
                "wikipedia",
                serde_json::json!({
                    "summary": "",
                    "list": [],
                    "obj": {},
                    "null_field": serde_json::Value::Null,
                    "ws": "   \n\t",
                }),
            ),
            source_of(
                "lastfm",
                serde_json::json!({
                    "summary": "lastfm prose",
                    "list": ["a"],
                    "obj": {"k": "v"},
                    "null_field": "from lastfm",
                    "ws": "content",
                }),
            ),
        ];
        let merged = merge_sources_field_level(&sources);
        let obj = merged.as_object().unwrap();
        assert_eq!(obj.get("summary").unwrap(), "lastfm prose");
        assert_eq!(obj.get("list").unwrap().as_array().unwrap().len(), 1);
        assert_eq!(obj.get("null_field").unwrap(), "from lastfm");
        assert_eq!(obj.get("ws").unwrap(), "content");
    }

    #[test]
    fn sort_sources_by_priority_lower_wins_first() {
        let cfg = ProviderConfig::defaults();
        let mut sources = vec![
            source_of("lastfm", serde_json::json!({})),
            source_of("musicbrainz", serde_json::json!({})),
            source_of("wikipedia", serde_json::json!({})),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        assert_eq!(sources[0].provider_id, "musicbrainz");
        assert_eq!(sources[1].provider_id, "wikipedia");
        assert_eq!(sources[2].provider_id, "lastfm");
    }

    #[test]
    fn sort_sources_by_priority_sinks_unknown() {
        let cfg = ProviderConfig::defaults();
        let mut sources = vec![
            source_of("unknown_provider", serde_json::json!({})),
            source_of("wikipedia", serde_json::json!({})),
        ];
        sort_sources_by_priority(&mut sources, &cfg);
        assert_eq!(sources[0].provider_id, "wikipedia");
        assert_eq!(sources[1].provider_id, "unknown_provider");
    }

    #[test]
    fn from_sources_ok_when_non_empty_notfound_when_empty() {
        let ok = CascadeResponse::from_sources(
            vec![source_of("wikipedia", serde_json::json!({"a": "b"}))],
            None,
        );
        assert!(matches!(ok.status, CascadeStatus::Ok));
        let empty = CascadeResponse::from_sources(vec![], None);
        assert!(matches!(empty.status, CascadeStatus::NotFound));
    }

    #[test]
    fn from_sources_top_level_is_merged_view() {
        let sources = vec![
            source_of("wikipedia", serde_json::json!({"summary": "wp"})),
            source_of(
                "lastfm",
                serde_json::json!({"summary": "lfm", "listeners": 7}),
            ),
        ];
        let resp = CascadeResponse::from_sources(sources, None);
        let payload = resp.payload.unwrap();
        assert_eq!(payload.get("summary").unwrap(), "wp");
        assert_eq!(payload.get("listeners").unwrap().as_u64(), Some(7));
        assert_eq!(resp.provider_id.as_deref(), Some("wikipedia"));
        assert_eq!(resp.sources.len(), 2);
    }
}
