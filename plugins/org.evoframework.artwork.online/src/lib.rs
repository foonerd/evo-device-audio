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
//!   "size": "small" | "medium" | "large" | "original" (`tiny` accepted as alias for `small`)
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
//!   "size": "small" | "medium" | "large" | "original" (`tiny` accepted as alias for `small`) | null,
//!   "provider_id": "cover_art_archive" | "lastfm" | "itunes" | "volumio_meta" | null,
//!   "detail": "<operator-readable detail>" | null
//! }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::manual_async_fn)]

mod artist_cascade;
mod artwork_caches;
mod config;
mod providers;
mod reconcile_coalescer;
mod resolve;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use evo_online_providers::{
    deezer::DeezerClient,
    fanart::FanartClient,
    musicbrainz::MusicBrainzClient,
    rate_limit::RateLimiter,
    theaudiodb::{TheAudioDbClient, THEAUDIODB_KEYLESS_API_KEY},
};
use evo_plugin_sdk::contract::{
    BuildInfo, HealthReport, LoadContext, Plugin, PluginDescription,
    PluginError, PluginIdentity, Request, Respondent, Response,
    RuntimeCapabilities,
};
use evo_plugin_sdk::Manifest;

use crate::config::PluginConfig;

/// Vault key the operator's fanart.tv personal API key lives
/// under. The framework credential vault owns the storage; this
/// plugin fetches the value at load and passes it to the fanart
/// client constructor.
const FANART_VAULT_KEY: &str = "fanart_tv_personal_api_key";

/// Embedded manifest.
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// Plugin reverse-DNS name; shared with the manifest and tests.
pub const PLUGIN_NAME: &str = "org.evoframework.artwork.online";

/// Album-artwork verb (existing, single-answer).
const REQUEST_ARTWORK_RESOLVE_ONLINE: &str = "artwork.resolve_online";

/// Artist-artwork verb (new). Runs the parallel-dispatch
/// aggregate cascade in [`artist_cascade`] and returns the
/// `sources: Vec<SourceEntry>` envelope shared with the
/// text-verb cascade in `org.evoframework.metadata.online`. The
/// operator UI's per-source selection surface reads `sources`
/// directly and renders text + image sources through one
/// code path.
const REQUEST_ARTWORK_RESOLVE_ARTIST_ARTWORK: &str =
    "artwork.resolve_artist_artwork";

/// Request type: drop this plugin's in-memory caches.
///
/// Wipes the artist-artwork reconcile cache (MB name → MBID
/// memo, positive + negative under TTL) and the non-Deezer
/// provider result cache. Deezer entries were never cached
/// (ToS live-fetch); a clear is a no-op for that provider.
/// Idempotent. Returns a small JSON envelope describing the
/// cleared counts.
const REQUEST_ARTWORK_ONLINE_CLEAR_CACHE: &str = "artwork.online.clear_cache";

/// Parse the embedded [`Manifest`].
pub fn manifest() -> Manifest {
    Manifest::from_toml(MANIFEST_TOML)
        .expect("org-evoframework-artwork-online: embedded manifest must parse")
}

fn plugin_crate_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION is valid semver")
}

/// Fabricate a MusicBrainz-TOS-compliant User-Agent when the
/// operator has not configured one. MB TOS says a client MUST
/// send a UA that identifies "software, version, and contact
/// info"; the default identifies this plugin's crate name +
/// version and points at the plugin repository as the
/// operator-visible contact for any provider-facing question
/// about the traffic pattern.
fn default_musicbrainz_user_agent() -> String {
    format!(
        "{PLUGIN_NAME}/{} (+https://github.com/foonerd/evo-device-audio)",
        env!("CARGO_PKG_VERSION")
    )
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
    /// TheAudioDB keyless client for the artist-artwork cascade
    /// (surfaces `strArtistThumb`). Built at load; cleared on
    /// unload.
    theaudiodb_client: Option<Arc<TheAudioDbClient>>,
    /// Deezer keyless client. Provides four resolutions of
    /// artist portrait via public API. ToS-mandated live-fetch
    /// only — enforced structurally by `ArtistImageHit`'s
    /// deliberate absence of `Serialize`.
    deezer_client: Option<Arc<DeezerClient>>,
    /// fanart.tv client. Requires an operator-supplied personal
    /// API key from the framework credential vault (key
    /// `fanart_tv_personal_api_key`). `None` when the vault has
    /// no key or the framework has no vault; the artist-artwork
    /// cascade treats the fanart source as disabled in that
    /// case.
    fanart_client: Option<Arc<FanartClient>>,
    /// MusicBrainz client used by the artist-artwork cascade to
    /// resolve a canonical MBID from an artist name (`ws/2/artist
    /// ?query=`) plus URL relationships (`ws/2/artist/<mbid>
    /// ?inc=url-rels`) that carry the artist's Deezer link.
    /// Always constructed once the plugin has loaded: uses the
    /// operator-supplied `musicbrainz_user_agent` when set, or a
    /// spec-compliant default identifying this plugin's crate
    /// version + a public contact link. `Option` remains to
    /// keep the pre-load state (`None`) representable.
    mb_client: Option<Arc<MusicBrainzClient>>,
    /// Per-provider enable + priority for the artist-artwork
    /// cascade. Framework defaults, layered with runtime
    /// overrides from the `online_provider_config` store at
    /// load time. Wrapped in `Arc<RwLock<..>>` so the store-
    /// change reactor can apply operator gestures in place —
    /// the next verb dispatch sees the new enable / priority
    /// without a plugin restart.
    artist_provider_config:
        Arc<tokio::sync::RwLock<artist_cascade::ArtistProviderConfig>>,
    /// Volumio meta variant string (`community` / `commercial`)
    /// piggy-backed from the album-artwork provider config.
    /// Artist-artwork uses the same base endpoint under
    /// `mode=artistArt`.
    volumio_meta_variant: String,
    /// Background reactor task spawned at load. Subscribes to
    /// the framework's `online_provider_config` bus and applies
    /// operator gestures to `artist_provider_config` in place.
    /// Aborted on unload so a re-load cycle spawns a fresh
    /// subscription rather than leaking the old one.
    reactor_tasks: Vec<tokio::task::JoinHandle<()>>,
    /// Per-plugin caches for the artist-artwork cascade —
    /// memoises the MB reconcile outcome and the non-Deezer
    /// provider results so repeat browse of the same artist
    /// set does not re-hammer upstream. LRU-capped, TTL-bound,
    /// dropped on unload. Deezer results never enter these
    /// caches (live-fetch invariant enforced by
    /// `ArtistImageHit`'s missing `Serialize`).
    artwork_caches: Arc<artwork_caches::ArtworkCaches>,
    /// Single-flight coalescer for the artist-artwork cascade.
    /// Keyed on fold-key so a browse fan-out that surfaces the
    /// same artist under multiple tiles (or the same tile
    /// twice from separate WS calls within one browse) runs
    /// the cascade at most once — the concurrent waiters
    /// subscribe to the in-flight future and share its
    /// outcome. Orthogonal to `artwork_caches`: the coalescer
    /// collapses within one call cycle, the caches memoise
    /// across time.
    reconcile_coalescer: Arc<
        reconcile_coalescer::ReconcileCoalescer<
            Result<artist_cascade::ArtistArtworkResponse, String>,
        >,
    >,
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
            theaudiodb_client: None,
            deezer_client: None,
            fanart_client: None,
            mb_client: None,
            artist_provider_config: Arc::new(tokio::sync::RwLock::new(
                artist_cascade::ArtistProviderConfig::defaults(),
            )),
            volumio_meta_variant: "community".to_string(),
            reactor_tasks: Vec::new(),
            artwork_caches: Arc::new(artwork_caches::ArtworkCaches::new()),
            reconcile_coalescer: Arc::new(
                reconcile_coalescer::ReconcileCoalescer::new(),
            ),
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
                        REQUEST_ARTWORK_RESOLVE_ONLINE.to_string(),
                        REQUEST_ARTWORK_RESOLVE_ARTIST_ARTWORK.to_string(),
                        REQUEST_ARTWORK_ONLINE_CLEAR_CACHE.to_string(),
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
            let http =
                providers::build_http_client(self.config.request_timeout);
            self.http_client = Some(http.clone());
            self.asset_cache = ctx.asset_cache.clone();
            self.volumio_meta_variant =
                self.config.providers.volumio_meta.variant.clone();
            // ------------------------------------------------------
            // Artist-artwork cascade wiring.
            //
            // Three new clients — TheAudioDB (keyless), Deezer
            // (keyless), fanart.tv (keyed via vault). Every client
            // shares the plugin's HTTPS client for connection-pool
            // + DNS-cache reuse. Rate limiters are one-per-
            // provider at 1 req/sec — Deezer + TheAudioDB are
            // community-scale services and fanart.tv's SLA is
            // best-effort, so social discipline matters even
            // when the API doesn't enforce it.
            let ua = self.config.musicbrainz_user_agent.clone().unwrap_or_else(
                || "evo-device-audio (artist-artwork cascade)".to_string(),
            );
            let one_req_per_sec =
                || Arc::new(RateLimiter::new(Duration::from_secs(1)));
            self.theaudiodb_client = Some(Arc::new(TheAudioDbClient::new(
                http.clone(),
                one_req_per_sec(),
                ua.clone(),
                THEAUDIODB_KEYLESS_API_KEY,
            )));
            self.deezer_client = Some(Arc::new(DeezerClient::new(
                http.clone(),
                one_req_per_sec(),
                ua.clone(),
            )));
            // MusicBrainz client — used by the artist-artwork
            // cascade to reconcile artist name → MBID → URL-rels
            // (Deezer artist id / other databases). MB's TOS
            // requires 1 req/sec and an identifying UA; the
            // shared `one_req_per_sec()` limiter honours the
            // former, and the UA is always set — operator
            // override under `musicbrainz_user_agent` when
            // present, or a spec-compliant default identifying
            // this plugin + crate version + a public contact
            // link. That way a fresh install has a working
            // artist-artwork cascade without an operator
            // configuration step, and MB's TOS is honoured
            // either way (a distribution-scoped UA is more
            // informative than a naked `curl` one).
            let mb_ua = self
                .config
                .musicbrainz_user_agent
                .clone()
                .unwrap_or_else(default_musicbrainz_user_agent);
            self.mb_client = Some(Arc::new(MusicBrainzClient::new(
                http.clone(),
                one_req_per_sec(),
                mb_ua,
            )));
            // fanart.tv: fetch personal API key from the framework
            // credential vault. Absent key → client is None →
            // cascade treats fanart source as disabled (returns
            // None on fetch; other providers still dispatch).
            let fanart_key = match ctx.credential_vault.as_ref() {
                Some(vault) => {
                    match vault.fetch(FANART_VAULT_KEY.to_string()).await {
                        Ok(Some(bytes)) => match String::from_utf8(bytes) {
                            Ok(s) if !s.trim().is_empty() => Some(s),
                            Ok(_) => None,
                            Err(e) => {
                                tracing::warn!(
                                    plugin = PLUGIN_NAME,
                                    key = FANART_VAULT_KEY,
                                    error = %e,
                                    "credential vault value is not valid \
                                     UTF-8; fanart source stays disabled"
                                );
                                None
                            }
                        },
                        Ok(None) => None,
                        Err(e) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                key = FANART_VAULT_KEY,
                                error = %e,
                                "credential vault fetch failed; fanart \
                                 source stays disabled"
                            );
                            None
                        }
                    }
                }
                None => None,
            };
            self.fanart_client = fanart_key.and_then(|key| {
                FanartClient::new(http.clone(), one_req_per_sec(), ua, key)
                    .map(Arc::new)
            });
            // Runtime-store overlay for the artist-artwork
            // cascade's per-provider enable / priority. Same
            // shape as metadata.online's overlay: framework
            // defaults, then the store rows layered on top.
            // Unknown provider ids skip with a debug log. The
            // reactor spawned below extends this into the
            // live-run so `set_enabled(false)` removes the
            // source on the next query, no restart.
            {
                let mut cfg = self.artist_provider_config.write().await;
                if let Some(store) = ctx.online_provider_config.as_ref() {
                    match store.list_all().await {
                        Ok(rows) => {
                            for row in rows {
                                let Some(pid) =
                                    artist_cascade::ArtistProviderId::from_wire(
                                        &row.provider_id,
                                    )
                                else {
                                    tracing::debug!(
                                        plugin = PLUGIN_NAME,
                                        provider_id = %row.provider_id,
                                        "online_provider_config store row \
                                         names a provider this plugin's \
                                         artist-artwork cascade does not \
                                         implement; skipping"
                                    );
                                    continue;
                                };
                                // Sentinel semantics (migration 042):
                                // priority < 0 means "operator has NOT
                                // explicitly set a priority for this
                                // provider". Keep the plugin's cascade
                                // default; still apply enabled.
                                let priority_override = if row.priority < 0 {
                                    None
                                } else {
                                    Some(row.priority as u32)
                                };
                                cfg.merge_override(
                                    pid,
                                    Some(row.enabled),
                                    priority_override,
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                plugin = PLUGIN_NAME,
                                error = %e,
                                "online_provider_config store list_all \
                                 failed; artist-artwork cascade uses \
                                 framework defaults"
                            );
                        }
                    }
                }
            }
            // Spawn the store-change reactor so live operator
            // gestures re-resolve the cascade in place.
            if let Some(store) = ctx.online_provider_config.as_ref() {
                let rx = store.subscribe_changes();
                let config_slot = Arc::clone(&self.artist_provider_config);
                self.reactor_tasks.push(tokio::spawn(
                    online_provider_config_reactor(rx, config_slot),
                ));
            }
            tracing::info!(
                plugin = PLUGIN_NAME,
                asset_cache_wired = self.asset_cache.is_some(),
                musicbrainz_ua_set =
                    self.config.musicbrainz_user_agent.is_some(),
                lastfm_enabled = self.config.providers.lastfm.api_key.is_some(),
                theaudiodb_wired = self.theaudiodb_client.is_some(),
                deezer_wired = self.deezer_client.is_some(),
                fanart_wired = self.fanart_client.is_some(),
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
            self.theaudiodb_client = None;
            self.deezer_client = None;
            self.fanart_client = None;
            self.mb_client = None;
            // Drop the cache state and replace with a fresh
            // empty pair so a subsequent load() starts with
            // no memoised reconcile / provider entries. Same
            // for the coalescer's in-flight map (should be
            // empty at unload, but reset defensively).
            self.artwork_caches =
                Arc::new(artwork_caches::ArtworkCaches::new());
            self.reconcile_coalescer =
                Arc::new(reconcile_coalescer::ReconcileCoalescer::new());
            // Abort every background reactor so a subsequent
            // re-load spawns fresh subscriptions.
            for task in self.reactor_tasks.drain(..) {
                task.abort();
            }
            *self.artist_provider_config.write().await =
                artist_cascade::ArtistProviderConfig::defaults();
            self.volumio_meta_variant = "community".to_string();
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
            let known = [
                REQUEST_ARTWORK_RESOLVE_ONLINE,
                REQUEST_ARTWORK_RESOLVE_ARTIST_ARTWORK,
                REQUEST_ARTWORK_ONLINE_CLEAR_CACHE,
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

            if req.request_type == REQUEST_ARTWORK_ONLINE_CLEAR_CACHE {
                let (reconcile_dropped, provider_dropped) =
                    self.artwork_caches.drop_all();
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    reconcile_entries_dropped = reconcile_dropped,
                    provider_entries_dropped = provider_dropped,
                    "artwork.online.clear_cache: in-mem LRUs cleared"
                );
                let body = serde_json::to_vec(&serde_json::json!({
                    "v": 1,
                    "status": "ok",
                    "reconcile_entries_dropped": reconcile_dropped,
                    "provider_entries_dropped": provider_dropped,
                }))
                .map_err(|e| {
                    PluginError::Permanent(format!(
                        "artwork.online.clear_cache response JSON: {e}"
                    ))
                })?;
                return Ok(Response::for_request(req, body));
            }

            tracing::debug!(
                plugin = PLUGIN_NAME,
                request_type = %req.request_type,
                cid = req.correlation_id,
                payload_len = req.payload.len(),
                "handling request"
            );

            match req.request_type.as_str() {
                REQUEST_ARTWORK_RESOLVE_ONLINE => {
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
                    if let Some((content_hash, bytes)) =
                        resolve_output.cache_payload
                    {
                        if let Some(cache) = &self.asset_cache {
                            if let Err(e) =
                                cache.put(&content_hash, bytes).await
                            {
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
                    let body =
                        resolve_output.response.json_bytes().map_err(|e| {
                            PluginError::Permanent(format!(
                                "artwork.resolve_online response JSON: {e}"
                            ))
                        })?;
                    Ok(Response::for_request(req, body))
                }
                REQUEST_ARTWORK_RESOLVE_ARTIST_ARTWORK => {
                    let http = self
                        .http_client
                        .clone()
                        .expect("http client present after load");
                    let config_snapshot =
                        self.artist_provider_config.read().await.clone();
                    let catalogue = artist_cascade::ArtistCatalogue {
                        volumio_meta_http: Arc::new(http),
                        volumio_meta_variant: self.volumio_meta_variant.clone(),
                        theaudiodb: self.theaudiodb_client.clone(),
                        deezer: self.deezer_client.clone(),
                        fanart: self.fanart_client.clone(),
                        mb: self.mb_client.clone(),
                        caches: Arc::clone(&self.artwork_caches),
                        coalescer: Arc::clone(&self.reconcile_coalescer),
                        config: config_snapshot,
                    };
                    let response = artist_cascade::query_artist_artwork(
                        &req.payload,
                        &catalogue,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    let body = response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "artwork.resolve_artist_artwork response JSON: {e}"
                        ))
                    })?;
                    Ok(Response::for_request(req, body))
                }
                other => Err(PluginError::Permanent(format!(
                    "unknown request type (defensive): {other}"
                ))),
            }
        }
    }
}

// -----------------------------------------------------------------
// Online-provider-config reactor.
// -----------------------------------------------------------------

/// Background task spawned at load time. Awaits change events
/// on the framework's `online_provider_config` bus and applies
/// each operator gesture to `artist_provider_config` in place
/// — no plugin restart. The next verb dispatch sees the new
/// enable / priority and behaves accordingly.
///
/// Terminates on `RecvError::Closed` (framework sender dropped)
/// or `JoinHandle::abort` (unload). `RecvError::Lagged` never
/// terminates: the reactor logs the dropped-event count and
/// continues so a burst of operator gestures does not stall
/// the substrate.
async fn online_provider_config_reactor(
    mut rx: tokio::sync::broadcast::Receiver<
        evo_plugin_sdk::contract::context::OnlineProviderConfigChangeEvent,
    >,
    config_slot: Arc<tokio::sync::RwLock<artist_cascade::ArtistProviderConfig>>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Some(pid) = artist_cascade::ArtistProviderId::from_wire(
                    &event.provider_id,
                ) else {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        provider_id = %event.provider_id,
                        "reactor: config change names a provider this \
                         plugin's artist-artwork cascade does not implement; \
                         skipping"
                    );
                    continue;
                };
                // Sentinel: priority < 0 means "operator has not
                // explicitly set a priority" (migration 042).
                // Keep the plugin's cascade default; still apply
                // enabled.
                let priority_override = if event.priority < 0 {
                    None
                } else {
                    Some(event.priority as u32)
                };
                {
                    let mut cfg = config_slot.write().await;
                    cfg.merge_override(
                        pid,
                        Some(event.enabled),
                        priority_override,
                    );
                }
                tracing::info!(
                    plugin = PLUGIN_NAME,
                    provider_id = %event.provider_id,
                    enabled = event.enabled,
                    priority = event.priority,
                    "reactor: applied online_provider_config change to \
                     artist-artwork cascade"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    dropped = n,
                    "reactor: online_provider_config bus lagged; \
                     continuing (next event will re-sync)"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "reactor: online_provider_config bus closed; exiting"
                );
                return;
            }
        }
    }
}
