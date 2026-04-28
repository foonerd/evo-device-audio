//! # org-evoframework-metadata-local
//!
//! **`metadata.providers`** singleton respondent. Answers `metadata.query` with
//! v1 JSON by reading embedded tags (via [`lofty`]) from local audio files,
//! using the same `mpd-path` / `mpd-album` addressing and `[library] roots` as
//! `org.evoframework.artwork.local` and `org.evoframework.playback.mpd`, and optional
//! `[metadata] profile` (`standard` default, `extended` for full tags / technicals; see
//! `docs/METADATA_QUERY_V1.md`).
//!
//! # `metadata.query` (JSON, UTF-8)
//!
//! Request (v1), aligned with `artwork.resolve` target shape:
//! ```json
//! {"v":1,"target":{"scheme":"mpd-path","value":"Artist/Album/01.flac"}}
//! ```
//!
//! Response: `"v":1`, `status` (`ok` / `not_found` / `unsupported` /
//! `bad_request`), and when `ok` a rich, Picard- and classical-friendly shape:
//! flat `title` / `artist` / `album` / `year` / `disc` / `duration_ms`, plus
//! optional nested `credits` (composer, conductor, performers, label, …),
//! `classical` (work, movement, movement index), `sort` (TXXX sort keys),
//! `original`, `dates` (recording / release strings), `identifiers` (ISRC,
//! MusicBrainz UUIDs, catalog), `replay_gain`, `file` (sample rate, bit
//! depth, bitrates, channel mask), and `extras` (unmapped / vendor `ItemKey::Unknown`
//! frames). **Field catalogue:** `docs/METADATA_QUERY_V1.md` in this plugin tree.
//!
//! # Version alignment
//!
//! [`PluginIdentity::version`], the embedded `manifest.toml` `[plugin]`
//! section, and this crate’s `CARGO_PKG_VERSION` must match.
//!
//! # See also
//!
//! [`evo_plugin_sdk::contract::Respondent`], `docs/engineering/SCHEMAS.md`
//! (example `metadata.query`).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod config;
mod query;

use std::future::Future;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

use crate::config::PluginConfig;

/// Embedded manifest source.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin name (reverse-DNS); same as manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.metadata.local";

const REQUEST_METADATA_QUERY: &str = "metadata.query";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-metadata-local: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Local file-tag metadata respondent: [`PluginConfig::library_roots`] and
/// `metadata.query` for `mpd-path` values.
pub struct MetadataLocalPlugin {
    loaded: bool,
    config: PluginConfig,
    requests_handled: u64,
}

impl MetadataLocalPlugin {
    /// New instance; call [`Plugin::load`] before handling requests.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            requests_handled: 0,
        }
    }

    /// Count of [`Respondent::handle_request`] invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    #[cfg(test)]
    fn set_loaded_with_config(&mut self, config: PluginConfig) {
        self.loaded = true;
        self.config = config;
    }
}

impl Default for MetadataLocalPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MetadataLocalPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![REQUEST_METADATA_QUERY.to_string()],
                    accepts_custody: false,
                    flags: Default::default(),
                },
                build_info: BuildInfo {
                    plugin_build: env!("CARGO_PKG_VERSION").to_string(),
                    sdk_version: evo_plugin_sdk::VERSION.to_string(),
                    rustc_version: None,
                    built_at: None,
                },
            }
        }
    }

    fn load<'a>(
        &'a mut self,
        ctx: &'a LoadContext,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + 'a {
        async move {
            tracing::info!(
                plugin = PLUGIN_NAME,
                config_keys = ctx.config.len(),
                "metadata local plugin load"
            );
            self.config =
                PluginConfig::from_toml_table(&ctx.config).map_err(|e| {
                    PluginError::Permanent(format!(
                        "invalid plugin config: {e}"
                    ))
                })?;
            if !self.config.library_roots.is_empty() {
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    n = self.config.library_roots.len(),
                    "library search roots configured"
                );
            }
            tracing::info!(
                plugin = PLUGIN_NAME,
                profile = self.config.metadata_profile.as_wire(),
                "metadata response profile (metadata.query)"
            );
            self.loaded = true;
            Ok(())
        }
    }

    fn unload(
        &mut self,
    ) -> impl Future<Output = Result<(), PluginError>> + Send + '_ {
        async move {
            self.loaded = false;
            self.config = PluginConfig::defaults();
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("metadata local plugin not loaded")
            }
        }
    }
}

impl Respondent for MetadataLocalPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "metadata local plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            if req.request_type != REQUEST_METADATA_QUERY {
                self.requests_handled += 1;
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [REQUEST_METADATA_QUERY]
                )));
            }

            self.requests_handled += 1;
            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "metadata.query"
            );

            let out = match query::query_metadata(
                self.config.metadata_profile,
                &self.config.library_roots,
                &req.payload,
            ) {
                Ok(r) => r,
                Err(e) => {
                    return Err(PluginError::Permanent(e));
                }
            };
            let body = out.json_bytes().map_err(|e| {
                PluginError::Permanent(format!("metadata response JSON: {e}"))
            })?;
            Ok(Response::for_request(req, body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evo_plugin_sdk::contract::HealthStatus;
    use evo_plugin_sdk::manifest::InteractionShape;
    use serde_json::Value;

    fn sample_mpd_path_payload(value: &str) -> Vec<u8> {
        format!(
            r#"{{"v":1,"target":{{"scheme":"{}","value":{}}}}}"#,
            query::SCHEME_MPD_PATH,
            serde_json::to_string(value).unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.target.shelf, "metadata.providers");
        let cap = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must have respondent capabilities");
        assert!(cap
            .request_types
            .iter()
            .any(|s| s == REQUEST_METADATA_QUERY));
        assert_eq!(m.kind.interaction, InteractionShape::Respondent);
    }

    #[tokio::test]
    async fn describe_matches_embedded_manifest() {
        let p = MetadataLocalPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(d.identity.version, m.plugin.version);
        assert!(!d.runtime_capabilities.accepts_custody);
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec![REQUEST_METADATA_QUERY]
        );
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = MetadataLocalPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn handle_rejects_before_load() {
        let mut p = MetadataLocalPlugin::new();
        let r = Request {
            request_type: REQUEST_METADATA_QUERY.to_string(),
            payload: vec![],
            correlation_id: 1,
            deadline: None,
            instance_id: None,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.requests_handled(), 0);
    }

    #[tokio::test]
    async fn handle_bad_json() {
        let mut p = MetadataLocalPlugin::new();
        p.set_loaded_with_config(PluginConfig::defaults());
        let r = Request {
            request_type: REQUEST_METADATA_QUERY.to_string(),
            payload: b"not-json".to_vec(),
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "bad_request");
    }

    #[tokio::test]
    async fn handle_not_found_missing_file() {
        let mut p = MetadataLocalPlugin::new();
        p.set_loaded_with_config(PluginConfig::defaults());
        let r = Request {
            request_type: REQUEST_METADATA_QUERY.to_string(),
            payload: sample_mpd_path_payload("/nope/no.flac"),
            correlation_id: 3,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "not_found");
    }
}
