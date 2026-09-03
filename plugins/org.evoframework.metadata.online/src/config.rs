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

use crate::cascade::{ProviderConfig, ProviderId};

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
        // `privacy_mode` is NO LONGER read from this plugin's
        // TOML. It is a device-level setting owned by the
        // framework and read through the provider-config handle
        // at request time, so every cascade — text and artwork —
        // sees one posture.
        //
        // A local copy here is precisely what produced the
        // divergence this retirement fixes: this plugin honoured
        // its own copy while the artwork plugin, having none,
        // enforced nothing. Re-introducing the key would
        // re-introduce the divergence.
        //
        // A stale `privacy_mode` left in an operator's TOML is
        // refused rather than ignored: silently accepting a file
        // that no longer does anything would let an operator
        // believe they had set a posture they had not.
        if cfg.privacy_mode.is_some() {
            return Err(
                "privacy_mode is no longer a plugin setting — it is a \
                 device-level setting owned by the framework. Remove it from \
                 this file and set it with the `online_providers_set_privacy_mode` \
                 operator gesture, which applies to every cascade rather than \
                 this plugin alone."
                    .to_string(),
            );
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
    fn privacy_mode_is_no_longer_a_plugin_setting() {
        // The key is REFUSED, not ignored. Privacy mode is a
        // device-level setting owned by the framework and read at
        // request time; a local copy here is exactly what let this
        // plugin enforce a posture the artwork cascade knew
        // nothing about.
        //
        // Silently ignoring a stale key would be worse than
        // refusing it: an operator would keep a privacy_mode line
        // in their file and believe a posture was in force that
        // this plugin was no longer reading.
        for value in ["enhanced", "anonymous_only", "offline", "silent"] {
            let raw: toml::value::Table =
                toml::from_str(&format!("privacy_mode = {value:?}")).unwrap();
            let err = PluginConfig::from_toml_table(&raw)
                .expect_err("privacy_mode in plugin TOML must be refused");
            assert!(
                err.contains("device-level"),
                "refusal must tell the operator where the setting moved to,                  got: {err}"
            );
        }
    }

    #[test]
    fn config_without_privacy_mode_defaults_to_enhanced() {
        // A file that does not mention it parses, and the posture
        // starts at the framework default. The real value is
        // overwritten from the framework on every request.
        let raw: toml::value::Table = toml::from_str("").unwrap();
        let cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert_eq!(
            cfg.provider_config.privacy_mode,
            crate::cascade::PrivacyMode::Enhanced
        );
    }

    #[test]
    fn offline_posture_disables_every_provider() {
        // Behaviour preserved from the retired TOML-driven test;
        // the posture is now set directly, as the framework sets
        // it at request time.
        let mut pc = ProviderConfig::defaults();
        pc.privacy_mode = crate::cascade::PrivacyMode::Offline;
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
                !pc.is_effectively_enabled(p),
                "provider {p:?} must be effectively disabled under offline"
            );
        }
    }

    #[test]
    fn anonymous_only_posture_disables_identity_bearing_only() {
        let mut pc = ProviderConfig::defaults();
        pc.privacy_mode = crate::cascade::PrivacyMode::AnonymousOnly;
        for p in [
            ProviderId::MusicBrainz,
            ProviderId::Wikipedia,
            ProviderId::Wikidata,
            ProviderId::Lrclib,
        ] {
            assert!(
                pc.is_effectively_enabled(p),
                "anonymous provider {p:?} must remain effectively enabled"
            );
        }
        for p in [ProviderId::Lastfm, ProviderId::Discogs, ProviderId::Genius] {
            assert!(
                !pc.is_effectively_enabled(p),
                "identity-bearing provider {p:?} must be effectively \
                 disabled under anonymous_only"
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
        // The operator explicitly enables Last.fm in their file…
        let raw: toml::value::Table = toml::from_str(
            r#"
            [providers.lastfm]
            enabled = true
            "#,
        )
        .unwrap();
        let mut cfg = PluginConfig::from_toml_table(&raw).unwrap();
        assert!(cfg.provider_config.flags(ProviderId::Lastfm).enabled);

        // …and the device posture, set by the framework at request
        // time, overrides it anyway. Non-bypassable means the
        // per-provider enable cannot buy its way past the posture;
        // otherwise `anonymous_only` would mean "anonymous unless
        // something was configured", which is not a guarantee.
        cfg.provider_config.privacy_mode =
            crate::cascade::PrivacyMode::AnonymousOnly;
        assert!(!cfg
            .provider_config
            .is_effectively_enabled(ProviderId::Lastfm));

        // And the operator's raw intent survives underneath, so
        // leaving the posture restores their choice rather than
        // silently losing it.
        assert!(cfg.provider_config.flags(ProviderId::Lastfm).enabled);
    }
}
