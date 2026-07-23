// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Plugin configuration.
//!
//! Loaded from
//! `/etc/evo/plugins.d/org.evoframework.metadata.online.toml`
//! by the framework, passed through the `LoadContext.config`
//! at load time. Every field has a sensible default so an
//! operator with no config file gets working reconciliation
//! against MusicBrainz's public API.

use std::time::Duration;

use serde::Deserialize;

use crate::cascade::{PrivacyMode, ProviderConfig, ProviderId};

/// Canonical User-Agent used against MusicBrainz. Includes
/// product name, version, and contact URL per MB's Terms of
/// Use. Operators may override via `musicbrainz.user_agent`.
const DEFAULT_MUSICBRAINZ_USER_AGENT: &str =
    "evo-device-audio/0.1.13 ( +https://github.com/foonerd/evo-device-audio )";

/// MB API policy: 1 request per second per client.
const DEFAULT_MUSICBRAINZ_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Bounded outbound HTTPS timeout. MB's search endpoint
/// occasionally exceeds 5 s under load; 12 s covers the tail
/// without letting the framework's coalescer deadline expire.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

/// Long-TTL negative cache. Mirrors the artwork endpoint's
/// negative-cache posture: a browse burst re-visiting the same
/// unmatched target should not re-hammer MB across days.
const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Last.fm API policy: 5 requests per second per key.
const DEFAULT_LASTFM_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// LRCLIB — no documented per-second policy but explicit
/// respectful use called out. Default 200 ms (5 req/sec) matches
/// Last.fm's cadence.
const DEFAULT_LRCLIB_MIN_INTERVAL: Duration = Duration::from_millis(200);

/// Effective, in-memory plugin configuration.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    pub(crate) request_timeout: Duration,
    pub(crate) musicbrainz_user_agent: String,
    pub(crate) musicbrainz_min_interval: Duration,
    pub(crate) negative_ttl: Duration,
    /// Last.fm API key. `None` disables the Last.fm provider —
    /// the bio + album-notes verbs return structured
    /// `not_configured` rather than fabricating a result. This
    /// is the pin the operator specified: "honest-empty on
    /// tracks with no lyrics/bio rather than a fabricated
    /// result." When the operator drops the key at the path
    /// pointed to by `lastfm.api_key_path`, the plugin arms
    /// the provider on next restart.
    pub(crate) lastfm_api_key: Option<String>,
    pub(crate) lastfm_min_interval: Duration,
    /// LRCLIB rate-limit cadence. Overridable per operator
    /// policy; default 200 ms (5 req/sec) matches Last.fm's.
    pub(crate) lrclib_min_interval: Duration,
    /// Provider cascade configuration — per-provider enable +
    /// priority pairs plus the `privacy_mode` preset. The
    /// operator's `[providers.<id>]` and `privacy_mode`
    /// entries in the plugin config file feed this. The plugin
    /// copies it into its `self.provider_config` at load; every
    /// verb dispatch reads through it via the ProviderCatalogue.
    pub(crate) provider_config: ProviderConfig,
}

impl PluginConfig {
    pub(crate) fn defaults() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            musicbrainz_user_agent: DEFAULT_MUSICBRAINZ_USER_AGENT.to_string(),
            musicbrainz_min_interval: DEFAULT_MUSICBRAINZ_MIN_INTERVAL,
            negative_ttl: DEFAULT_NEGATIVE_TTL,
            lastfm_api_key: None,
            lastfm_min_interval: DEFAULT_LASTFM_MIN_INTERVAL,
            lrclib_min_interval: DEFAULT_LRCLIB_MIN_INTERVAL,
            provider_config: ProviderConfig::defaults(),
        }
    }

    /// Load overrides from the plugin's `LoadContext.config`
    /// TOML table. Missing keys keep their defaults; malformed
    /// keys surface as `Err(String)` for the caller to reject
    /// with `PluginError::Permanent`.
    pub(crate) fn from_toml_table(
        raw: &toml::value::Table,
    ) -> Result<Self, String> {
        let cfg: RawConfig = raw
            .clone()
            .try_into()
            .map_err(|e| format!("invalid plugin config: {e}"))?;
        let mut out = Self::defaults();
        if let Some(secs) = cfg.request_timeout_seconds {
            if !(1..=120).contains(&secs) {
                return Err(format!(
                    "request_timeout_seconds must be in 1..=120; got {secs}"
                ));
            }
            out.request_timeout = Duration::from_secs(secs);
        }
        if let Some(mb) = cfg.musicbrainz {
            if let Some(ua) = mb.user_agent {
                if ua.trim().is_empty() {
                    return Err(
                        "musicbrainz.user_agent must be non-empty when set"
                            .to_string(),
                    );
                }
                out.musicbrainz_user_agent = ua;
            }
            if let Some(millis) = mb.min_interval_ms {
                if !(0..=60_000).contains(&millis) {
                    return Err(format!(
                        "musicbrainz.min_interval_ms must be 0..=60000; got {millis}"
                    ));
                }
                out.musicbrainz_min_interval = Duration::from_millis(millis);
            }
        }
        if let Some(hours) = cfg.negative_ttl_hours {
            if hours == 0 {
                return Err("negative_ttl_hours must be > 0".to_string());
            }
            out.negative_ttl = Duration::from_secs(hours * 60 * 60);
        }
        if let Some(lastfm) = cfg.lastfm {
            if let Some(path) = lastfm.api_key_path {
                let trimmed = path.trim();
                if !trimmed.is_empty() {
                    match std::fs::read_to_string(trimmed) {
                        Ok(contents) => {
                            let key = contents.trim().to_string();
                            if !key.is_empty() {
                                out.lastfm_api_key = Some(key);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                path = %trimmed,
                                error = %e,
                                "lastfm.api_key_path unreadable; provider stays disabled"
                            );
                        }
                    }
                }
            }
            if let Some(key) = lastfm.api_key {
                let trimmed = key.trim();
                if !trimmed.is_empty() {
                    out.lastfm_api_key = Some(trimmed.to_string());
                }
            }
            if let Some(millis) = lastfm.min_interval_ms {
                if !(0..=60_000).contains(&millis) {
                    return Err(format!(
                        "lastfm.min_interval_ms must be 0..=60000; got {millis}"
                    ));
                }
                out.lastfm_min_interval = Duration::from_millis(millis);
            }
        }
        if let Some(lrclib) = cfg.lrclib {
            if let Some(millis) = lrclib.min_interval_ms {
                if !(0..=60_000).contains(&millis) {
                    return Err(format!(
                        "lrclib.min_interval_ms must be 0..=60000; got {millis}"
                    ));
                }
                out.lrclib_min_interval = Duration::from_millis(millis);
            }
        }
        if let Some(mode) = cfg.privacy_mode {
            out.provider_config.privacy_mode =
                match mode.to_ascii_lowercase().as_str() {
                    "enhanced" => PrivacyMode::Enhanced,
                    "anonymous_only" => PrivacyMode::AnonymousOnly,
                    "offline" => PrivacyMode::Offline,
                    other => {
                        return Err(format!(
                            "privacy_mode must be one of \
                         \"enhanced\" | \"anonymous_only\" | \"offline\"; \
                         got {other:?}"
                        ));
                    }
                };
        }
        if let Some(providers) = cfg.providers {
            for (id_raw, flags_raw) in providers {
                let id = parse_provider_id(&id_raw)?;
                let mut flags = *out.provider_config.flags_ref(id);
                if let Some(enabled) = flags_raw.enabled {
                    flags.enabled = enabled;
                }
                if let Some(priority) = flags_raw.priority {
                    flags.priority = priority;
                }
                out.provider_config.set_flags(id, flags);
            }
        }
        Ok(out)
    }
}

fn parse_provider_id(raw: &str) -> Result<ProviderId, String> {
    match raw {
        "musicbrainz" => Ok(ProviderId::MusicBrainz),
        "wikipedia" => Ok(ProviderId::Wikipedia),
        "wikidata" => Ok(ProviderId::Wikidata),
        "lrclib" => Ok(ProviderId::Lrclib),
        "lastfm" => Ok(ProviderId::Lastfm),
        "discogs" => Ok(ProviderId::Discogs),
        "genius" => Ok(ProviderId::Genius),
        other => Err(format!(
            "unknown provider id under [providers]: {other:?}; \
             expected one of musicbrainz / wikipedia / wikidata / \
             lrclib / lastfm / discogs / genius"
        )),
    }
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    request_timeout_seconds: Option<u64>,
    #[serde(default)]
    musicbrainz: Option<RawMusicBrainz>,
    #[serde(default)]
    negative_ttl_hours: Option<u64>,
    #[serde(default)]
    lastfm: Option<RawLastfm>,
    #[serde(default)]
    lrclib: Option<RawLrclib>,
    /// Optional privacy-mode preset — layered non-bypassably on
    /// top of the per-provider flags. Accepted values:
    /// `"enhanced"` (default, per-provider selection stands),
    /// `"anonymous_only"` (every identity-bearing provider is
    /// disabled en masse), `"offline"` (every network provider
    /// disabled).
    #[serde(default)]
    privacy_mode: Option<String>,
    /// Optional per-provider `[providers.<id>]` sub-tables
    /// letting the operator flip `enabled` and re-order
    /// `priority` per provider. Providers omitted keep their
    /// framework defaults.
    #[serde(default)]
    providers: Option<std::collections::BTreeMap<String, RawProviderFlags>>,
}

#[derive(Debug, Deserialize)]
struct RawProviderFlags {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    priority: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RawLastfm {
    /// Path to a file whose contents are the API key. Preferred
    /// over inline `api_key` so the key doesn't sit in plaintext
    /// in a shared config file.
    #[serde(default)]
    api_key_path: Option<String>,
    /// Inline API key. Overrides `api_key_path` when both are
    /// set. Present so tests / dev setups can supply a key
    /// without a filesystem indirection.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    min_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawLrclib {
    #[serde(default)]
    min_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RawMusicBrainz {
    #[serde(default)]
    user_agent: Option<String>,
    #[serde(default)]
    min_interval_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_reasonable() {
        let cfg = PluginConfig::defaults();
        assert!(cfg.musicbrainz_user_agent.starts_with("evo-device-audio"));
        assert_eq!(cfg.musicbrainz_min_interval, Duration::from_secs(1));
        assert_eq!(cfg.request_timeout, Duration::from_secs(12));
        assert_eq!(cfg.negative_ttl.as_secs(), 24 * 3600);
    }

    #[test]
    fn ua_override_wins() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [musicbrainz]
            user_agent = "MyCustomApp/1.0 ( +https://example.com )"
            "#,
        )
        .unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert!(cfg.musicbrainz_user_agent.starts_with("MyCustomApp"));
    }

    #[test]
    fn empty_ua_refused() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [musicbrainz]
            user_agent = "   "
            "#,
        )
        .unwrap();
        assert!(PluginConfig::from_toml_table(&raw).is_err());
    }

    #[test]
    fn interval_override_clamped() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [musicbrainz]
            min_interval_ms = 500
            "#,
        )
        .unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(cfg.musicbrainz_min_interval, Duration::from_millis(500));
    }

    #[test]
    fn privacy_mode_defaults_to_enhanced() {
        let raw: toml::value::Table = toml::from_str("").unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(cfg.provider_config.privacy_mode, PrivacyMode::Enhanced);
    }

    #[test]
    fn privacy_mode_anonymous_only_parses() {
        let raw: toml::value::Table =
            toml::from_str(r#"privacy_mode = "anonymous_only""#).unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(
            cfg.provider_config.privacy_mode,
            PrivacyMode::AnonymousOnly
        );
    }

    #[test]
    fn privacy_mode_offline_parses() {
        let raw: toml::value::Table =
            toml::from_str(r#"privacy_mode = "offline""#).unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(cfg.provider_config.privacy_mode, PrivacyMode::Offline);
    }

    #[test]
    fn privacy_mode_unknown_rejected() {
        let raw: toml::value::Table =
            toml::from_str(r#"privacy_mode = "silent""#).unwrap();
        assert!(PluginConfig::from_toml_table(&raw).is_err());
    }

    #[test]
    fn privacy_mode_offline_disables_every_provider() {
        let raw: toml::value::Table =
            toml::from_str(r#"privacy_mode = "offline""#).unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        for p in [
            ProviderId::MusicBrainz,
            ProviderId::Wikipedia,
            ProviderId::Wikidata,
            ProviderId::Lrclib,
            ProviderId::Lastfm,
            ProviderId::Discogs,
            ProviderId::Genius,
        ] {
            assert!(
                !cfg.provider_config.is_effectively_enabled(p),
                "provider {:?} must be effectively disabled under offline",
                p
            );
        }
    }

    #[test]
    fn privacy_mode_anonymous_only_disables_identity_bearing() {
        let raw: toml::value::Table =
            toml::from_str(r#"privacy_mode = "anonymous_only""#).unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        // Anonymous providers remain enabled.
        for p in [
            ProviderId::MusicBrainz,
            ProviderId::Wikipedia,
            ProviderId::Wikidata,
            ProviderId::Lrclib,
        ] {
            assert!(
                cfg.provider_config.is_effectively_enabled(p),
                "anonymous provider {:?} must remain effectively enabled",
                p
            );
        }
        // Identity-bearing providers are disabled en masse.
        for p in [ProviderId::Lastfm, ProviderId::Discogs, ProviderId::Genius] {
            assert!(
                !cfg.provider_config.is_effectively_enabled(p),
                "identity-bearing provider {:?} must be effectively \
                 disabled under anonymous_only",
                p
            );
        }
    }

    #[test]
    fn per_provider_disable_applies() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [providers.wikipedia]
            enabled = false
            "#,
        )
        .unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert!(!cfg
            .provider_config
            .is_effectively_enabled(ProviderId::Wikipedia));
        assert!(cfg
            .provider_config
            .is_effectively_enabled(ProviderId::MusicBrainz));
    }

    #[test]
    fn per_provider_priority_override_applies() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [providers.lastfm]
            priority = 5
            "#,
        )
        .unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(cfg.provider_config.flags(ProviderId::Lastfm).priority, 5);
    }

    #[test]
    fn unknown_provider_id_rejected() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            [providers.spotify]
            enabled = true
            "#,
        )
        .unwrap();
        assert!(PluginConfig::from_toml_table(&raw).is_err());
    }

    #[test]
    fn privacy_mode_wins_over_per_provider_enable_for_identity_bearing() {
        let raw: toml::value::Table = toml::from_str(
            r#"
            privacy_mode = "anonymous_only"

            [providers.lastfm]
            enabled = true
            "#,
        )
        .unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        // Even though the operator explicitly enabled Last.fm,
        // anonymous_only overrides it — the preset is
        // non-bypassable.
        assert!(!cfg
            .provider_config
            .is_effectively_enabled(ProviderId::Lastfm));
    }
}
