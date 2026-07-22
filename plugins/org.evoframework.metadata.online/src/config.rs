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

/// Effective, in-memory plugin configuration.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    pub(crate) request_timeout: Duration,
    pub(crate) musicbrainz_user_agent: String,
    pub(crate) musicbrainz_min_interval: Duration,
    pub(crate) negative_ttl: Duration,
}

impl PluginConfig {
    pub(crate) fn defaults() -> Self {
        Self {
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            musicbrainz_user_agent: DEFAULT_MUSICBRAINZ_USER_AGENT.to_string(),
            musicbrainz_min_interval: DEFAULT_MUSICBRAINZ_MIN_INTERVAL,
            negative_ttl: DEFAULT_NEGATIVE_TTL,
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
        Ok(out)
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
}
