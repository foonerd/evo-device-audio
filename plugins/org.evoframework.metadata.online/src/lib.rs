// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! # org-evoframework-metadata-online
//!
//! Online metadata respondent. Co-occupies the
//! `metadata.providers` shelf with `org.evoframework.metadata.local`;
//! the two plugins partition the shelf's verb set per the
//! Stocking primitive (this plugin owns
//! `metadata.reconcile_release`, the local plugin owns
//! `metadata.query`).
//!
//! ## Sole verb: `metadata.reconcile_release`
//!
//! Reconcile a local `(artist, album)` pair against the
//! MusicBrainz release catalogue and return the canonical
//! identity — MBID, release-group MBID, artist MBID, canonical
//! artist/album strings, first-release year,
//! `recording_type` (Studio / Live / Compilation / Soundtrack),
//! and track count. Downstream verbs depend on this identity:
//!
//! - `library.browse_by_recording_type` (piece 5) filters on
//!   `recording_type`.
//! - Artwork resolution via CAA MBID (rather than fuzzy
//!   `(artist, album)` search) improves cover accuracy.
//! - Composite track/album detail (piece 7) surfaces the
//!   canonical labels rather than whatever the file tags say.
//!
//! ## Provider chain
//!
//! Single provider this cycle: MusicBrainz JSON API
//! (`musicbrainz.org/ws/2/`). Every outbound call flows through
//! the shared token bucket in `evo-online-providers` so the 1
//! req/sec API policy is honoured across plugins.
//!
//! ## Cache
//!
//! Two-level file-based cache in `state_dir/reconcile_cache/`:
//!
//! - Positive hits — indefinite. A MusicBrainz release identity
//!   does not churn (MB assigns MBIDs at creation and does not
//!   reassign). Persisted across steward restarts.
//! - Negative hits (no MB match) — 24 h. Mirrors the artwork
//!   endpoint's negative-cache TTL so a browse burst re-visiting
//!   the same unmatched target does not re-hammer MB.
//!
//! Cache key: `sha256(normalise(artist) + "|" + normalise(album))`.
//! Normalisation is lower-case + Unicode NFC + strip leading /
//! trailing whitespace — matches the mpd-album parser's shape
//! so the same (artist, album) hits regardless of tag drift.
//!
//! ## Wire shape
//!
//! Request (`v=1`):
//!
//! ```json
//! { "v": 1, "artist": "Radiohead", "album": "OK Computer" }
//! ```
//!
//! Response:
//!
//! ```json
//! {
//!   "v": 1,
//!   "status": "ok" | "not_found" | "bad_request",
//!   "canonical": {
//!     "artist": "Radiohead",
//!     "album": "OK Computer",
//!     "release_mbid": "b1392450-e666-3926-a536-22c65f834433",
//!     "release_group_mbid": "5b11f4ce-a62d-471e-81fc-a69a8278c7da",
//!     "artist_mbid": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
//!     "first_release_year": 1997,
//!     "recording_type": "Studio",
//!     "track_count": 12
//!   } | null,
//!   "provider_id": "musicbrainz" | "cache" | null,
//!   "confidence_percent": 100 | null,
//!   "detail": "<operator-readable detail>" | null
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod browse_recording_type;
mod cache;
mod config;
mod reconcile;

use std::future::Future;
use std::sync::Arc;

use evo_online_providers::{build_http_client, MusicBrainzClient, RateLimiter};
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
pub const PLUGIN_NAME: &str = "org.evoframework.metadata.online";

/// The reconciliation verb — cornerstone of piece 3.
const REQUEST_METADATA_RECONCILE_RELEASE: &str = "metadata.reconcile_release";

/// The recording-type facet browse verb — piece 5. Composes
/// MPD-side enumeration with the reconciliation identity to
/// filter albums by their canonical recording type.
const REQUEST_LIBRARY_BROWSE_BY_RECORDING_TYPE: &str =
    "library.browse_by_recording_type";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML).expect(
        "org-evoframework-metadata-online: embedded manifest must parse",
    )
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Online metadata respondent.
pub struct MetadataOnlinePlugin {
    loaded: bool,
    config: PluginConfig,
    mb_client: Option<MusicBrainzClient>,
    reconcile_cache: Option<cache::ReconcileCache>,
    requests_handled: std::sync::atomic::AtomicU64,
}

impl MetadataOnlinePlugin {
    /// New plugin, not yet loaded.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            mb_client: None,
            reconcile_cache: None,
            requests_handled: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Cumulative `handle_request` invocations.
    pub fn requests_handled(&self) -> u64 {
        self.requests_handled
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for MetadataOnlinePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for MetadataOnlinePlugin {
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
                        REQUEST_METADATA_RECONCILE_RELEASE.to_string(),
                        REQUEST_LIBRARY_BROWSE_BY_RECORDING_TYPE.to_string(),
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
                "metadata online plugin load"
            );
            self.config =
                PluginConfig::from_toml_table(&ctx.config).map_err(|e| {
                    PluginError::Permanent(format!(
                        "invalid plugin config: {e}"
                    ))
                })?;
            let http = build_http_client(self.config.request_timeout);
            let rate = Arc::new(RateLimiter::new(
                self.config.musicbrainz_min_interval,
            ));
            self.mb_client = Some(MusicBrainzClient::new(
                http,
                rate,
                self.config.musicbrainz_user_agent.clone(),
            ));
            self.reconcile_cache = Some(cache::ReconcileCache::new(
                ctx.state_dir.join("reconcile_cache"),
                self.config.negative_ttl,
            ));
            tracing::info!(
                plugin = PLUGIN_NAME,
                cache_wired = self.reconcile_cache.is_some(),
                musicbrainz_ua = %self.config.musicbrainz_user_agent,
                mb_min_interval_ms = self.config.musicbrainz_min_interval.as_millis() as u64,
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
            self.mb_client = None;
            self.reconcile_cache = None;
            Ok(())
        }
    }

    fn health_check(&self) -> impl Future<Output = HealthReport> + Send + '_ {
        async move {
            if self.loaded {
                HealthReport::healthy()
            } else {
                HealthReport::unhealthy("metadata online plugin not loaded")
            }
        }
    }
}

impl Respondent for MetadataOnlinePlugin {
    fn handle_request<'a>(
        &'a self,
        req: &'a Request,
    ) -> impl Future<Output = Result<Response, PluginError>> + Send + 'a {
        async move {
            if !self.loaded {
                return Err(PluginError::Permanent(
                    "metadata online plugin not loaded".to_string(),
                ));
            }
            if req.is_past_deadline() {
                return Err(PluginError::Transient(
                    "request deadline already expired".to_string(),
                ));
            }
            let known = [
                REQUEST_METADATA_RECONCILE_RELEASE,
                REQUEST_LIBRARY_BROWSE_BY_RECORDING_TYPE,
            ];
            if !known.contains(&req.request_type.as_str()) {
                self.requests_handled
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return Err(PluginError::Permanent(format!(
                    "unknown request type: {:?} (not one of: {:?})",
                    req.request_type, known
                )));
            }
            self.requests_handled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "handling request"
            );

            let mb = self
                .mb_client
                .clone()
                .expect("mb client present after load");
            let cache = self.reconcile_cache.clone();
            let payload = req.payload.clone();
            let body = if req.request_type == REQUEST_METADATA_RECONCILE_RELEASE
            {
                let response =
                    reconcile::reconcile(&payload, &mb, cache.as_ref())
                        .await
                        .map_err(PluginError::Permanent)?;
                response.json_bytes().map_err(|e| {
                    PluginError::Permanent(format!(
                        "metadata.reconcile_release response JSON: {e}"
                    ))
                })?
            } else {
                let cache_ref = cache.as_ref().ok_or_else(|| {
                    PluginError::Permanent(
                        "reconcile cache not wired at load — cannot serve \
                         library.browse_by_recording_type"
                            .to_string(),
                    )
                })?;
                let response = browse_recording_type::browse_by_recording_type(
                    &payload, &mb, cache_ref,
                )
                .await
                .map_err(PluginError::Permanent)?;
                response.json_bytes().map_err(|e| {
                    PluginError::Permanent(format!(
                        "library.browse_by_recording_type response JSON: {e}"
                    ))
                })?
            };
            Ok(Response::for_request(req, body))
        }
    }
}
