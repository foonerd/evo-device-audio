//! # org-evoframework-artwork-local
//!
//! **Milestone 4** — `artwork.providers` singleton respondent. Resolves
//! local sidecar cover art for tracks announced by
//! `org.evoframework.playback.mpd`, using the same `mpd-path` / `mpd-album`
//! addressing scheme strings as [`ExternalAddressing`](
//! evo_plugin_sdk::contract::ExternalAddressing) (see
//! `resolve::SCHEME_MPD_PATH` / `resolve::SCHEME_MPD_ALBUM`).
//!
//! # `artwork.resolve` (JSON, UTF-8)
//!
//! Request (v1 only):
//! ```json
//! {"v":1,"target":{"scheme":"mpd-path","value":"Artist/Album/01.flac"}}
//! ```
//! Response always includes `"v":1` and a `status` field: `ok`,
//! `not_found`, `unsupported`, or `bad_request`, plus optional
//! `path`, `mime`, and `detail` as document in [`resolve::ArtworkResolveResponse`].
//!
//! - **`mpd-path`**: `value` is MPD’s `file` (relative to a configured
//!   [`config::PluginConfig::library_roots`] or absolute on disk). A cover
//!   file next to the resolved audio file is chosen from a fixed name list
//!   (`folder.jpg`, `cover.jpg`, …) in [`resolve::find_cover_beside_audio_file`].
//! - **`mpd-album`**: `value` is `"{artist}|{album}"` as emitted by
//!   `org.evoframework.playback.mpd` for the `album` subject. The respondent scans
//!   files under [library] roots and picks the **first** track (deterministic
//!   walk) whose primary tag artist and album match; it then uses the same
//!   cover logic as `mpd-path` for that file. Large libraries are bounded (see
//!   `evo_device_audio_shared::MAX_MPD_ALBUM_SCAN_CANDIDATES`).
//!
//! # Version alignment
//!
//! [`PluginIdentity::version`], the embedded `manifest.toml` `[plugin]`
//! section, and this crate’s `CARGO_PKG_VERSION` must match; see
//! [`plugin_crate_version`].
//!
//! # Reference
//!
//! [`evo_plugin_sdk::contract::Respondent`] and
//! `docs/engineering/PLUGIN_AUTHORING.md` (singleton respondent).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod config;
mod embedded;
mod resolve;

use std::future::Future;

use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

use crate::config::PluginConfig;

/// Embedded manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin reverse-DNS name; shared with the manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.artwork.local";

/// Request type: resolve cover / visual material for a subject.
const REQUEST_ARTWORK_RESOLVE: &str = "artwork.resolve";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-artwork-local: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Local artwork respondent: optional `[library]` roots in
/// `LoadContext::config`, sidecar files, embedded tag images, and
/// `state_dir` cache for the latter.
pub struct ArtworkLocalPlugin {
    /// `true` after a successful [`Plugin::load`].
    loaded: bool,
    /// Merged from [`PluginConfig::from_toml_table`].
    config: PluginConfig,
    /// `LoadContext::state_dir`; used for embedded cover cache.
    state_dir: Option<std::path::PathBuf>,
    /// Count of `handle_request` invocations.
    requests_handled: u64,
}

impl ArtworkLocalPlugin {
    /// New plugin, not yet [`Plugin::load`]ed.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            state_dir: None,
            requests_handled: 0,
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
    }

    /// For unit tests: simulate load without a real [`LoadContext`].
    #[cfg(test)]
    fn set_loaded_with_config(
        &mut self,
        config: PluginConfig,
        state_dir: std::path::PathBuf,
    ) {
        self.loaded = true;
        self.config = config;
        self.state_dir = Some(state_dir);
    }
}

impl Default for ArtworkLocalPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ArtworkLocalPlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![REQUEST_ARTWORK_RESOLVE.to_string()],
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
                "artwork local plugin load"
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
            self.state_dir = Some(ctx.state_dir.clone());
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
            self.state_dir = None;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("artwork plugin not loaded")
            }
        }
    }
}

impl Respondent for ArtworkLocalPlugin {
    fn handle_request<'a>(
        &'a mut self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "artwork plugin not loaded".to_string(),
                ));
            }

            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }

            if req.request_type != REQUEST_ARTWORK_RESOLVE {
                self.requests_handled += 1;
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [REQUEST_ARTWORK_RESOLVE]
                )));
            }

            self.requests_handled += 1;

            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "artwork.resolve"
            );

            let out = match resolve::resolve_artwork(
                &self.config.library_roots,
                self.state_dir.as_deref(),
                &req.payload,
            ) {
                Ok(r) => r,
                Err(e) => {
                    return Err(PluginError::Permanent(e));
                }
            };
            let body = out.json_bytes().map_err(|e| {
                PluginError::Permanent(format!("artwork response JSON: {e}"))
            })?;
            Ok(Response::for_request(req, body))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use evo_plugin_sdk::contract::HealthStatus;
    use evo_plugin_sdk::manifest::InteractionShape;
    use serde_json::Value;

    fn sample_mpd_path_payload(value: &str) -> Vec<u8> {
        format!(
            r#"{{"v":1,"target":{{"scheme":"{}","value":{}}}}}"#,
            resolve::SCHEME_MPD_PATH,
            serde_json::to_string(value).unwrap()
        )
        .into_bytes()
    }

    #[test]
    fn manifest_parses() {
        let m = manifest();
        assert_eq!(m.plugin.name, PLUGIN_NAME);
        assert_eq!(m.plugin.contract, 1);
        assert_eq!(m.kind.interaction, InteractionShape::Respondent);
        let cap = m
            .capabilities
            .respondent
            .as_ref()
            .expect("manifest must have respondent capabilities");
        assert!(cap
            .request_types
            .iter()
            .any(|s| s == REQUEST_ARTWORK_RESOLVE));
    }

    #[tokio::test]
    async fn describe_matches_embedded_manifest() {
        let p = ArtworkLocalPlugin::new();
        let d = p.describe().await;
        let m = manifest();
        assert_eq!(d.identity.name, m.plugin.name);
        assert_eq!(
            d.identity.version, m.plugin.version,
            "CARGO_PKG_VERSION / describe / manifest [plugin].version must match"
        );
        assert!(!d.runtime_capabilities.accepts_custody);
        assert_eq!(
            d.runtime_capabilities.request_types,
            vec![REQUEST_ARTWORK_RESOLVE]
        );
    }

    #[tokio::test]
    async fn health_unhealthy_before_load() {
        let p = ArtworkLocalPlugin::new();
        assert!(matches!(
            p.health_check().await.status,
            HealthStatus::Unhealthy
        ));
    }

    #[tokio::test]
    async fn handle_rejects_before_load() {
        let mut p = ArtworkLocalPlugin::new();
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
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
    async fn handle_unknown_request_type() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: "metadata.query".to_string(),
            payload: vec![],
            correlation_id: 2,
            deadline: None,
            instance_id: None,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Permanent(_)));
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_bad_request_invalid_json() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: b"{not json".to_vec(),
            correlation_id: 3,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "bad_request");
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_not_found() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: sample_mpd_path_payload("/no/such/absolute.flac"),
            correlation_id: 4,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "not_found");
        assert_eq!(p.requests_handled(), 1);
    }

    #[tokio::test]
    async fn handle_resolve_ok_with_cover() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("A").join("B");
        std::fs::create_dir_all(&sub).unwrap();
        let flac = sub.join("t.flac");
        std::fs::write(&flac, b"x").unwrap();
        std::fs::write(sub.join("folder.jpg"), b"fakejpeg").unwrap();

        let rel = "A/B/t.flac";
        let mut p = ArtworkLocalPlugin::new();
        p.set_loaded_with_config(
            PluginConfig {
                library_roots: vec![dir.path().to_path_buf()],
            },
            dir.path().join("state"),
        );

        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: sample_mpd_path_payload(rel),
            correlation_id: 5,
            deadline: None,
            instance_id: None,
        };
        let out = p.handle_request(&r).await.unwrap();
        let v: Value = serde_json::from_slice(&out.payload).unwrap();
        assert_eq!(v["status"], "ok");
        let pstr = v["path"].as_str().unwrap();
        let pb = PathBuf::from(pstr);
        assert!(pb.ends_with("folder.jpg"), "{pstr}");
    }

    #[tokio::test]
    async fn handle_past_deadline() {
        let mut p = ArtworkLocalPlugin::new();
        let tmp = tempfile::tempdir().unwrap();
        p.set_loaded_with_config(
            PluginConfig::defaults(),
            tmp.path().to_path_buf(),
        );
        let r = Request {
            request_type: REQUEST_ARTWORK_RESOLVE.to_string(),
            payload: vec![],
            correlation_id: 6,
            deadline: Some(
                std::time::Instant::now() - std::time::Duration::from_secs(1),
            ),
            instance_id: None,
        };
        let e = p.handle_request(&r).await.unwrap_err();
        assert!(matches!(e, PluginError::Transient(_)));
        assert_eq!(p.requests_handled(), 0);
    }
}
