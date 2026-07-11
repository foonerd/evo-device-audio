// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Operator configuration for the artwork.online plugin.
//!
//! Per-provider enable/disable + API keys + HTTP user-agent +
//! per-request timeout. The configuration is the single
//! operator-tunable surface; the resolution cascade order is
//! fixed by code (CAA → Last.fm → iTunes → Volumio meta).
//!
//! `/etc/evo/plugins.d/org.evoframework.artwork.online.toml` shape:
//!
//! ```toml
//! # MusicBrainz/Cover Art Archive UA — REQUIRED by their TOS.
//! # Identify the application + version + contact channel.
//! musicbrainz_user_agent = "evo-device-audio/0.1.13 (https://github.com/foonerd)"
//!
//! # Per-provider switches. Default is enabled = true if the
//! # provider has its config; disabled when required config is
//! # missing.
//! [providers.cover_art_archive]
//! enabled = true
//!
//! [providers.lastfm]
//! enabled = true
//! api_key = "..."
//!
//! [providers.itunes]
//! enabled = true
//!
//! [providers.volumio_meta]
//! enabled = true
//! variant = "community"   # or "commercial"
//!
//! # HTTP request timeout (seconds). The framework's per-verb
//! # response budget bounds this further at 15s; the per-provider
//! # timeout is the upper bound for a single upstream call.
//! request_timeout_secs = 8
//! ```

use std::time::Duration;

/// Parsed plugin configuration.
#[derive(Debug, Clone)]
pub(crate) struct PluginConfig {
    /// User-Agent string sent to MusicBrainz / Cover Art Archive.
    /// MusicBrainz refuses requests without an identifying UA; the
    /// `None` case still admits the plugin but the CAA provider
    /// silently disables.
    pub(crate) musicbrainz_user_agent: Option<String>,
    /// Per-provider config.
    pub(crate) providers: ProvidersConfig,
    /// Per-request HTTP timeout. Caps a single upstream call;
    /// the framework's per-verb response budget caps the full
    /// cascade.
    pub(crate) request_timeout: Duration,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ProvidersConfig {
    pub(crate) cover_art_archive: CoverArtArchiveConfig,
    pub(crate) lastfm: LastFmConfig,
    pub(crate) itunes: ITunesConfig,
    pub(crate) volumio_meta: VolumioMetaConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct CoverArtArchiveConfig {
    pub(crate) enabled: bool,
}

impl Default for CoverArtArchiveConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LastFmConfig {
    pub(crate) enabled: bool,
    pub(crate) api_key: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ITunesConfig {
    pub(crate) enabled: bool,
}

impl Default for ITunesConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct VolumioMetaConfig {
    pub(crate) enabled: bool,
    pub(crate) variant: String,
}

impl Default for VolumioMetaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            variant: "community".to_string(),
        }
    }
}

impl PluginConfig {
    /// Default config: every provider enabled by default,
    /// timeouts at 8 seconds, MusicBrainz UA unset (CAA disabled
    /// in effect until the operator sets it).
    pub(crate) fn defaults() -> Self {
        Self {
            musicbrainz_user_agent: None,
            providers: ProvidersConfig::default(),
            request_timeout: Duration::from_secs(8),
        }
    }

    /// Merge operator table. Unknown keys at the top level are
    /// ignored with a warning so a typo in one key does not
    /// abandon the rest of the config.
    pub(crate) fn from_toml_table(
        table: &toml::Table,
    ) -> Result<Self, ConfigError> {
        let mut cfg = Self::defaults();
        for key in table.keys() {
            match key.as_str() {
                "musicbrainz_user_agent"
                | "providers"
                | "request_timeout_secs" => {}
                other => {
                    tracing::warn!(
                        plugin = crate::PLUGIN_NAME,
                        key = other,
                        "unknown top-level config key; ignored"
                    );
                }
            }
        }
        if let Some(toml::Value::String(ua)) =
            table.get("musicbrainz_user_agent")
        {
            cfg.musicbrainz_user_agent = Some(ua.clone());
        }
        if let Some(toml::Value::Integer(secs)) =
            table.get("request_timeout_secs")
        {
            if *secs <= 0 {
                return Err(ConfigError {
                    key: "request_timeout_secs".into(),
                    message: "must be positive".into(),
                });
            }
            cfg.request_timeout = Duration::from_secs(*secs as u64);
        }
        if let Some(toml::Value::Table(providers)) = table.get("providers") {
            parse_providers(providers, &mut cfg.providers)?;
        }
        Ok(cfg)
    }
}

fn parse_providers(
    table: &toml::Table,
    out: &mut ProvidersConfig,
) -> Result<(), ConfigError> {
    if let Some(toml::Value::Table(t)) = table.get("cover_art_archive") {
        if let Some(toml::Value::Boolean(b)) = t.get("enabled") {
            out.cover_art_archive.enabled = *b;
        }
    }
    if let Some(toml::Value::Table(t)) = table.get("lastfm") {
        if let Some(toml::Value::Boolean(b)) = t.get("enabled") {
            out.lastfm.enabled = *b;
        }
        if let Some(toml::Value::String(s)) = t.get("api_key") {
            out.lastfm.api_key = Some(s.clone());
        }
    }
    if let Some(toml::Value::Table(t)) = table.get("itunes") {
        if let Some(toml::Value::Boolean(b)) = t.get("enabled") {
            out.itunes.enabled = *b;
        }
    }
    if let Some(toml::Value::Table(t)) = table.get("volumio_meta") {
        if let Some(toml::Value::Boolean(b)) = t.get("enabled") {
            out.volumio_meta.enabled = *b;
        }
        if let Some(toml::Value::String(s)) = t.get("variant") {
            out.volumio_meta.variant = s.clone();
        }
    }
    Ok(())
}

/// Invalid operator configuration.
#[derive(Debug, thiserror::Error)]
pub(crate) struct ConfigError {
    pub(crate) key: String,
    pub(crate) message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_defaults() {
        let t: toml::Table = "".parse().unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert!(c.musicbrainz_user_agent.is_none());
        assert_eq!(c.request_timeout, Duration::from_secs(8));
        assert!(c.providers.cover_art_archive.enabled);
        assert!(!c.providers.lastfm.enabled); // disabled until api_key set
        assert!(c.providers.itunes.enabled);
        assert!(c.providers.volumio_meta.enabled);
    }

    #[test]
    fn parses_musicbrainz_user_agent() {
        let t: toml::Table =
            r#"musicbrainz_user_agent = "evo/0.1 (https://example.com)""#
                .parse()
                .unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert_eq!(
            c.musicbrainz_user_agent.as_deref(),
            Some("evo/0.1 (https://example.com)")
        );
    }

    #[test]
    fn parses_lastfm_api_key() {
        let t: toml::Table = r#"
            [providers.lastfm]
            enabled = true
            api_key = "abc123"
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert!(c.providers.lastfm.enabled);
        assert_eq!(c.providers.lastfm.api_key.as_deref(), Some("abc123"));
    }

    #[test]
    fn refuses_zero_or_negative_timeout() {
        let t: toml::Table = "request_timeout_secs = 0".parse().unwrap();
        assert!(PluginConfig::from_toml_table(&t).is_err());

        let t: toml::Table = "request_timeout_secs = -5".parse().unwrap();
        assert!(PluginConfig::from_toml_table(&t).is_err());
    }

    #[test]
    fn per_provider_enable_override() {
        let t: toml::Table = r#"
            [providers.cover_art_archive]
            enabled = false
            [providers.itunes]
            enabled = false
        "#
        .parse()
        .unwrap();
        let c = PluginConfig::from_toml_table(&t).unwrap();
        assert!(!c.providers.cover_art_archive.enabled);
        assert!(!c.providers.itunes.enabled);
        assert!(c.providers.volumio_meta.enabled);
    }
}
