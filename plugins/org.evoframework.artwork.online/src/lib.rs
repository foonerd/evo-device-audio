// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! # org-evoframework-artwork-online
//!
//! Online artwork respondent. Co-occupies the `artwork.providers`
//! shelf with [`org.evoframework.artwork.local`]; the two plugins
//! partition the shelf's verb set per the Stocking primitive
//! (this plugin owns `artwork.resolve_online`, the local plugin
//! owns `artwork.resolve`). Operator UI or higher-level
//! orchestration invokes whichever cascade tier matches the
//! resolution policy — typically local-first with online
//! fallback.
//!
//! ## Cascade order
//!
//! 1. Cover Art Archive (MusicBrainz) — free; requires a
//!    properly-identifying User-Agent per MusicBrainz TOS.
//! 2. Last.fm `album.getinfo` — operator-supplied API key.
//! 3. iTunes Search API — no key; rewrites 100×100 thumbnail
//!    URLs to 600×600 via documented URL-pattern.
//! 4. Volumio meta proxy — no key; takes a `variant` parameter
//!    selecting Volumio's community vs commercial paths.
//!
//! Each provider is enable/disable + per-key configurable via
//! `/etc/evo/plugins.d/org.evoframework.artwork.online.toml`.
//! Providers whose required config is missing silently disable
//! and the cascade moves on; only the final cascade miss
//! surfaces to the caller as `not_found`.
//!
//! ## Wire shape
//!
//! Request (`v=1`):
//!
//! ```json
//! {
//!   "v": 1,
//!   "target": { "scheme": "mpd-album", "value": "{artist}|{album}" },
//!   "size": "tiny" | "medium" | "large" | "original"
//! }
//! ```
//!
//! Response mirrors `artwork.local`'s shape so UI consumers
//! use one decoder across both cascade tiers, plus the
//! `provider_id` field naming which upstream resolved the
//! artwork:
//!
//! ```json
//! {
//!   "v": 1,
//!   "status": "ok" | "not_found" | "unsupported" | "bad_request",
//!   "content_hash": "<sha256 hex>" | null,
//!   "mime": "image/webp" | "image/jpeg" | null,
//!   "size": "tiny" | "medium" | "large" | "original" | null,
//!   "provider_id": "cover_art_archive" | "lastfm" | "itunes" | "volumio_meta" | null,
//!   "detail": "<operator-readable detail>" | null
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod config;
mod providers;
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
pub const PLUGIN_NAME: &str = "org.evoframework.artwork.online";

/// Sole request type handled by this plugin.
const REQUEST_ARTWORK_RESOLVE_ONLINE: &str = "artwork.resolve_online";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-artwork-online: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Online artwork respondent.
pub struct ArtworkOnlinePlugin {
    loaded: bool,
    config: PluginConfig,
    asset_cache:
        Option<std::sync::Arc<dyn evo_plugin_sdk::contract::AssetCache>>,
    /// Shared HTTP client. Built once at load; provides
    /// connection pooling + DNS cache reuse across the cascade.
    http_client: Option<reqwest::Client>,
    requests_handled: std::sync::atomic::AtomicU64,
}

impl ArtworkOnlinePlugin {
    /// New plugin, not yet loaded.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            asset_cache: None,
            http_client: None,
            requests_handled: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for ArtworkOnlinePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ArtworkOnlinePlugin {
    fn describe(&self) -> impl Future<Output = PluginDescription> + Send + '_ {
        async move {
            PluginDescription {
                identity: PluginIdentity {
                    name: PLUGIN_NAME.to_string(),
                    version: plugin_crate_version(),
                    contract: 1,
                },
                runtime_capabilities: RuntimeCapabilities {
                    request_types: vec![
                        REQUEST_ARTWORK_RESOLVE_ONLINE.to_string()
                    ],
                    accepts_custody: false,
                    flags: Default::default(),
                    course_correct_verbs: Vec::new(),
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
                "artwork online plugin load"
            );
            self.config =
                PluginConfig::from_toml_table(&ctx.config).map_err(|e| {
                    PluginError::Permanent(format!(
                        "invalid plugin config: {e}"
                    ))
                })?;
            self.http_client =
                Some(providers::build_http_client(self.config.request_timeout));
            self.asset_cache = ctx.asset_cache.clone();
            tracing::info!(
                plugin = PLUGIN_NAME,
                asset_cache_wired = self.asset_cache.is_some(),
                musicbrainz_ua_set =
                    self.config.musicbrainz_user_agent.is_some(),
                lastfm_enabled = self.config.providers.lastfm.api_key.is_some(),
                "load complete"
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
            self.asset_cache = None;
            self.http_client = None;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("artwork online plugin not loaded")
            }
        }
    }
}

impl Respondent for ArtworkOnlinePlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "artwork online plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            if req.request_type != REQUEST_ARTWORK_RESOLVE_ONLINE {
                self.requests_handled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type,
                    [REQUEST_ARTWORK_RESOLVE_ONLINE]
                )));
            }
            self.requests_handled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "artwork.resolve_online"
            );

            let client = self
                .http_client
                .clone()
                .expect("http client present after load");
            let config = self.config.clone();
            let payload = req.payload.clone();
            let resolve_output = match resolve::resolve_artwork(
                &payload, &config, &client,
            )
            .await
            {
                Ok(r) => r,
                Err(e) => return Err(PluginError::Permanent(e)),
            };
            if let Some((content_hash, bytes)) = resolve_output.cache_payload {
                if let Some(cache) = &self.asset_cache {
                    if let Err(e) = cache.put(&content_hash, bytes).await {
                        tracing::warn!(
                            plugin = PLUGIN_NAME,
                            content_hash = %content_hash,
                            error = %e,
                            "asset cache put failed; response still carries content_hash"
                        );
                    }
                } else {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        content_hash = %content_hash,
                        "no asset cache wired; content_hash returned for path-only consumers"
                    );
                }
            }
            let body = resolve_output.response.json_bytes().map_err(|e| {
                PluginError::Permanent(format!(
                    "artwork.resolve_online response JSON: {e}"
                ))
            })?;
            Ok(Response::for_request(req, body))
        }
    }
}
