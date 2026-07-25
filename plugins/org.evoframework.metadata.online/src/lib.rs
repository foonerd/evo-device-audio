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
mod cascade;
mod config;
mod enrichment;
mod enrichment_cache;
mod reconcile;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use evo_online_providers::{
    build_http_client,
    theaudiodb::{TheAudioDbClient, THEAUDIODB_KEYLESS_API_KEY},
    DiscogsClient, GeniusClient, LastfmClient, LrclibClient, MusicBrainzClient,
    RateLimiter, WikidataClient, WikipediaClient,
};
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

/// Piece 6 enrichment verbs. LRCLIB lyrics is keyless. Bio +
/// album-notes provider is Last.fm; when the operator has not
/// supplied an API key the verbs return structured
/// `not_configured` responses rather than fabricated data.
const REQUEST_METADATA_QUERY_LYRICS: &str = "metadata.query_lyrics";
const REQUEST_METADATA_QUERY_ARTIST_BIO: &str = "metadata.query_artist_bio";
const REQUEST_METADATA_QUERY_ALBUM_NOTES: &str = "metadata.query_album_notes";
/// Discogs release-detail (label / catalog# / year / country /
/// format / notes) enrichment verb.
const REQUEST_METADATA_QUERY_RELEASE_CREDITS: &str =
    "metadata.query_release_credits";
/// Genius track-annotation (song description + lyrics URL)
/// enrichment verb.
const REQUEST_METADATA_QUERY_TRACK_ANNOTATION: &str =
    "metadata.query_track_annotation";
/// Classical work notes — MusicBrainz work lookup → url-rels →
/// Wikipedia summary of the work. Anonymous-only; Wikipedia is
/// the authoritative source for classical works and no
/// identity-bearing provider currently improves on it.
const REQUEST_METADATA_QUERY_WORK_NOTES: &str = "metadata.query_work_notes";

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
    /// Last.fm / Discogs / Genius credential-bearing clients are
    /// wrapped in `Arc<RwLock<Option<Client>>>` so the reactor
    /// task spawned at load time can swap them in place when the
    /// framework publishes a `CredentialSetChanged` event —
    /// without a plugin restart. `handle_request` clones the
    /// current value out under a read guard once per dispatch;
    /// the reactor takes a write guard only when a credential
    /// mutation lands, so reads never contend with themselves.
    lastfm_client: Arc<RwLock<Option<LastfmClient>>>,
    lrclib_client: Option<LrclibClient>,
    discogs_client: Arc<RwLock<Option<DiscogsClient>>>,
    genius_client: Arc<RwLock<Option<GeniusClient>>>,
    wikipedia_client: Option<WikipediaClient>,
    wikidata_client: Option<WikidataClient>,
    /// TheAudioDB keyless client (bio + album review text). No
    /// vault key today; the client wraps the community test key
    /// documented on TheAudioDB's site. If the operator later
    /// obtains a paid key, this slot upgrades to the same
    /// `Arc<RwLock<Option<..>>>` pattern as Last.fm / Discogs /
    /// Genius without touching the cascade.
    theaudiodb_client: Option<TheAudioDbClient>,
    /// Per-provider enable + priority for the text-verb cascade.
    /// Wrapped in `Arc<RwLock<..>>` so the store-change reactor
    /// spawned at load time can swap it in place when the
    /// framework's `online_provider_config` bus publishes a
    /// change — without a plugin restart. `handle_request`
    /// clones the current value out under a read guard once per
    /// dispatch; the reactor takes a write guard only when an
    /// operator gesture lands, so reads never contend with
    /// themselves.
    provider_config: Arc<RwLock<cascade::ProviderConfig>>,
    reconcile_cache: Option<cache::ReconcileCache>,
    lyrics_cache: Option<enrichment_cache::EnrichmentCache>,
    bio_cache: Option<enrichment_cache::EnrichmentCache>,
    notes_cache: Option<enrichment_cache::EnrichmentCache>,
    credits_cache: Option<enrichment_cache::EnrichmentCache>,
    annotation_cache: Option<enrichment_cache::EnrichmentCache>,
    work_notes_cache: Option<enrichment_cache::EnrichmentCache>,
    /// Cached credential-vault handle from the load-time
    /// `LoadContext`. `handle_request` consults it to populate
    /// `ProviderCatalogue.stored_key_hashes` for hint
    /// suppression. `None` when the steward booted without a
    /// vault (test harnesses, degraded boot).
    credential_vault: Option<
        Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    >,
    /// Background reactors spawned at load time. Each subscribes
    /// to a framework change bus (credential vault + online-
    /// provider config store) and re-resolves affected state in
    /// place — no plugin restart. All handles abort on unload so
    /// a re-load cycle spawns fresh reactors rather than leaking
    /// old subscriptions.
    reactor_tasks: Vec<JoinHandle<()>>,
    requests_handled: std::sync::atomic::AtomicU64,
}

impl MetadataOnlinePlugin {
    /// New plugin, not yet loaded.
    pub fn new() -> Self {
        Self {
            loaded: false,
            config: PluginConfig::defaults(),
            mb_client: None,
            lastfm_client: Arc::new(RwLock::new(None)),
            lrclib_client: None,
            discogs_client: Arc::new(RwLock::new(None)),
            genius_client: Arc::new(RwLock::new(None)),
            wikipedia_client: None,
            wikidata_client: None,
            theaudiodb_client: None,
            provider_config: Arc::new(RwLock::new(
                cascade::ProviderConfig::defaults(),
            )),
            reconcile_cache: None,
            lyrics_cache: None,
            bio_cache: None,
            notes_cache: None,
            credits_cache: None,
            annotation_cache: None,
            work_notes_cache: None,
            credential_vault: None,
            reactor_tasks: Vec::new(),
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
                        REQUEST_METADATA_QUERY_LYRICS.to_string(),
                        REQUEST_METADATA_QUERY_ARTIST_BIO.to_string(),
                        REQUEST_METADATA_QUERY_ALBUM_NOTES.to_string(),
                        REQUEST_METADATA_QUERY_RELEASE_CREDITS.to_string(),
                        REQUEST_METADATA_QUERY_TRACK_ANNOTATION.to_string(),
                        REQUEST_METADATA_QUERY_WORK_NOTES.to_string(),
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
            // Apply the operator's provider selection + privacy
            // preset from the config. The cascade orchestrator
            // reads through self.provider_config on every verb
            // dispatch; a `provider_config.privacy_mode = Offline`
            // config makes every network provider a structural
            // miss without any wire-op sequencing.
            //
            // Load-time layering (lowest → highest precedence):
            //   1. `ProviderConfig::defaults()` (framework baseline)
            //   2. plugin config-file `[providers.<id>]` blocks
            //   3. runtime store rows (operator gestures via wire
            //      op `online_providers_set_enabled` /
            //      `online_providers_set_priority`)
            //
            // The reactor task spawned below extends layer 3 into
            // the live-run: each new store event applies the same
            // `merge_override` on top of the current config, so
            // `set_enabled(false)` removes the source on the next
            // dispatch with no restart. Unknown provider ids
            // (from a store row that names a provider this plugin
            // does not implement) are skipped with a debug log —
            // the store is framework-wide and other plugins
            // register their own ids there.
            {
                let mut cfg = self.provider_config.write().await;
                *cfg = self.config.provider_config.clone();
                if let Some(store) = ctx.online_provider_config.as_ref() {
                    match store.list_all().await {
                        Ok(rows) => {
                            for row in rows {
                                let Some(pid) = cascade::ProviderId::from_wire(
                                    &row.provider_id,
                                ) else {
                                    tracing::debug!(
                                        plugin = PLUGIN_NAME,
                                        provider_id = %row.provider_id,
                                        "online_provider_config store row \
                                         names a provider this plugin does \
                                         not implement; skipping"
                                    );
                                    continue;
                                };
                                // Sentinel semantics (migration 042):
                                // priority < 0 means "operator has NOT
                                // explicitly set a priority for this
                                // provider". Keep the plugin's cascade
                                // default; still apply enabled — the
                                // operator's toggle is unambiguous even
                                // when they haven't touched priority.
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
                                 failed; cascade uses plugin config-file \
                                 defaults"
                            );
                        }
                    }
                }
            }
            // Shared HTTPS client — single connection pool +
            // DNS cache across every online provider in this
            // plugin.
            let http = build_http_client(self.config.request_timeout);
            let mb_rate = Arc::new(RateLimiter::new(
                self.config.musicbrainz_min_interval,
            ));
            self.mb_client = Some(MusicBrainzClient::new(
                http.clone(),
                mb_rate,
                self.config.musicbrainz_user_agent.clone(),
            ));
            let lrclib_rate =
                Arc::new(RateLimiter::new(self.config.lrclib_min_interval));
            self.lrclib_client = Some(LrclibClient::new(
                http.clone(),
                lrclib_rate,
                self.config.musicbrainz_user_agent.clone(),
            ));
            // Wikipedia + Wikidata anonymous baseline for the
            // keyless-first metadata enrichment cascade. Both
            // share the MusicBrainz rate limiter's 1-req/sec
            // cadence — Wikimedia asks for the same discipline
            // (descriptive UA, no burst).
            let wikimedia_rate = Arc::new(RateLimiter::new(
                self.config.musicbrainz_min_interval,
            ));
            self.wikipedia_client = Some(WikipediaClient::new(
                http.clone(),
                Arc::clone(&wikimedia_rate),
                self.config.musicbrainz_user_agent.clone(),
            ));
            self.wikidata_client = Some(WikidataClient::new(
                http.clone(),
                wikimedia_rate,
                self.config.musicbrainz_user_agent.clone(),
            ));
            // TheAudioDB anonymous baseline (keyless test key).
            // Shares the MusicBrainz rate limiter's 1-req/sec
            // cadence — TheAudioDB is a small-team database on a
            // best-effort SLA; hammering it is antisocial and
            // the test key is a shared community resource.
            let theaudiodb_rate = Arc::new(RateLimiter::new(
                self.config.musicbrainz_min_interval,
            ));
            self.theaudiodb_client = Some(TheAudioDbClient::new(
                http.clone(),
                theaudiodb_rate,
                self.config.musicbrainz_user_agent.clone(),
                THEAUDIODB_KEYLESS_API_KEY,
            ));
            // Resolve the Last.fm API key. Precedence:
            //   1. Framework credential vault (single-substrate
            //      source — operator UI writes here via the
            //      credential_put wire op).
            //   2. Legacy config file (lastfm.api_key_path /
            //      lastfm.api_key) with one-shot migration into
            //      the vault. The migration keeps the config-file
            //      path working across the substrate transition;
            //      subsequent boots find no config-file key and
            //      read from the vault.
            let lastfm_key = resolve_lastfm_api_key(
                ctx.credential_vault.as_ref(),
                self.config.lastfm_api_key.clone(),
            )
            .await;
            *self.lastfm_client.write().await = lastfm_key
                .map(|key| build_lastfm_client(&http, &self.config, key));
            self.reconcile_cache = Some(cache::ReconcileCache::new(
                ctx.state_dir.join("reconcile_cache"),
                self.config.negative_ttl,
            ));
            self.lyrics_cache = Some(enrichment_cache::EnrichmentCache::new(
                ctx.state_dir.join("lyrics_cache"),
                self.config.negative_ttl,
            ));
            self.bio_cache = Some(enrichment_cache::EnrichmentCache::new(
                ctx.state_dir.join("bio_cache"),
                self.config.negative_ttl,
            ));
            self.notes_cache = Some(enrichment_cache::EnrichmentCache::new(
                ctx.state_dir.join("album_notes_cache"),
                self.config.negative_ttl,
            ));
            self.credits_cache = Some(enrichment_cache::EnrichmentCache::new(
                ctx.state_dir.join("release_credits_cache"),
                self.config.negative_ttl,
            ));
            self.annotation_cache =
                Some(enrichment_cache::EnrichmentCache::new(
                    ctx.state_dir.join("track_annotation_cache"),
                    self.config.negative_ttl,
                ));
            self.work_notes_cache =
                Some(enrichment_cache::EnrichmentCache::new(
                    ctx.state_dir.join("work_notes_cache"),
                    self.config.negative_ttl,
                ));
            // Discogs client — resolves via credential vault under
            // stable key `discogs_personal_access_token`.
            let discogs_token = resolve_credential_from_vault(
                ctx.credential_vault.as_ref(),
                DISCOGS_VAULT_KEY,
            )
            .await;
            *self.discogs_client.write().await =
                discogs_token.and_then(|token| {
                    build_discogs_client(&http, &self.config, token)
                });
            // Genius client — resolves via credential vault under
            // stable key `genius_client_access_token`.
            let genius_token = resolve_credential_from_vault(
                ctx.credential_vault.as_ref(),
                GENIUS_VAULT_KEY,
            )
            .await;
            *self.genius_client.write().await =
                genius_token.and_then(|token| {
                    build_genius_client(&http, &self.config, token)
                });
            let lastfm_configured = self.lastfm_client.read().await.is_some();
            tracing::info!(
                plugin = PLUGIN_NAME,
                cache_wired = self.reconcile_cache.is_some(),
                musicbrainz_ua = %self.config.musicbrainz_user_agent,
                mb_min_interval_ms = self.config.musicbrainz_min_interval.as_millis() as u64,
                lastfm_configured = lastfm_configured,
                lrclib_wired = self.lrclib_client.is_some(),
                "load complete"
            );
            // Stash the credential-vault handle so `handle_request`
            // can consult `list_keys` at request start and hint
            // builders can suppress "add a key" for any provider
            // whose key is already stored.
            self.credential_vault = ctx.credential_vault.clone();
            // Spawn the credential-change reactor. On every
            // `CredentialSetChanged` event that touches one of
            // this plugin's vault keys (`lastfm_api_key` /
            // `discogs_personal_access_token` /
            // `genius_client_access_token`), re-fetch the value
            // and atomically swap the affected provider client
            // in place — no plugin restart, no lifecycle
            // teardown. Aborted on unload so a subsequent
            // re-load spawns a fresh reactor rather than
            // leaking the old subscription.
            if let Some(vault) = ctx.credential_vault.as_ref() {
                let rx = vault.subscribe_changes();
                let vault_for_task = Arc::clone(vault);
                let lastfm_slot = Arc::clone(&self.lastfm_client);
                let discogs_slot = Arc::clone(&self.discogs_client);
                let genius_slot = Arc::clone(&self.genius_client);
                let http_for_task = http.clone();
                let config_for_task = self.config.clone();
                self.reactor_tasks.push(tokio::spawn(credential_reactor(
                    rx,
                    vault_for_task,
                    lastfm_slot,
                    discogs_slot,
                    genius_slot,
                    http_for_task,
                    config_for_task,
                )));
            }
            // Spawn the online-provider-config reactor. On every
            // operator gesture published by the framework's
            // `online_provider_config` bus, apply the mutation to
            // the plugin's local ProviderConfig in place so the
            // next verb dispatch sees the new enable / priority
            // without a plugin restart. Aborted on unload.
            //
            // This is the live-apply pipe the T3 gate acceptance
            // depends on: `set_enabled(false)` removes the source
            // from the cascade's next query, not on the next boot.
            if let Some(store) = ctx.online_provider_config.as_ref() {
                let rx = store.subscribe_changes();
                let config_slot = Arc::clone(&self.provider_config);
                self.reactor_tasks.push(tokio::spawn(
                    online_provider_config_reactor(rx, config_slot),
                ));
            }
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
            // Abort every background reactor (credential vault +
            // online-provider config) so a subsequent re-load
            // spawns fresh subscriptions. Tasks may already be
            // exiting via `RecvError::Closed` if the framework
            // tore down the relevant change bus.
            for task in self.reactor_tasks.drain(..) {
                task.abort();
            }
            self.credential_vault = None;
            *self.lastfm_client.write().await = None;
            self.lrclib_client = None;
            *self.discogs_client.write().await = None;
            *self.genius_client.write().await = None;
            self.wikipedia_client = None;
            self.wikidata_client = None;
            self.theaudiodb_client = None;
            *self.provider_config.write().await =
                cascade::ProviderConfig::defaults();
            self.reconcile_cache = None;
            self.lyrics_cache = None;
            self.bio_cache = None;
            self.notes_cache = None;
            self.credits_cache = None;
            self.annotation_cache = None;
            self.work_notes_cache = None;
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
                REQUEST_METADATA_QUERY_LYRICS,
                REQUEST_METADATA_QUERY_ARTIST_BIO,
                REQUEST_METADATA_QUERY_ALBUM_NOTES,
                REQUEST_METADATA_QUERY_RELEASE_CREDITS,
                REQUEST_METADATA_QUERY_TRACK_ANNOTATION,
                REQUEST_METADATA_QUERY_WORK_NOTES,
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
            let lrclib = self
                .lrclib_client
                .clone()
                .expect("lrclib client present after load");
            let lastfm = self.lastfm_client.read().await.clone();
            let discogs = self.discogs_client.read().await.clone();
            let genius = self.genius_client.read().await.clone();
            let wikipedia = self.wikipedia_client.clone();
            let wikidata = self.wikidata_client.clone();
            let theaudiodb = self.theaudiodb_client.clone();
            let provider_config = self.provider_config.read().await.clone();
            let reconcile_cache = self.reconcile_cache.clone();
            let lyrics_cache = self.lyrics_cache.clone();
            let bio_cache = self.bio_cache.clone();
            let notes_cache = self.notes_cache.clone();
            let credits_cache = self.credits_cache.clone();
            let annotation_cache = self.annotation_cache.clone();
            let work_notes_cache = self.work_notes_cache.clone();
            let payload = req.payload.clone();
            let body = match req.request_type.as_str() {
                REQUEST_METADATA_RECONCILE_RELEASE => {
                    let response = reconcile::reconcile(
                        &payload,
                        &mb,
                        reconcile_cache.as_ref(),
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.reconcile_release response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_LIBRARY_BROWSE_BY_RECORDING_TYPE => {
                    let cache_ref =
                        reconcile_cache.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                            "reconcile cache not wired at load — cannot serve \
                             library.browse_by_recording_type"
                                .to_string(),
                        )
                        })?;
                    let response =
                        browse_recording_type::browse_by_recording_type(
                            &payload, &mb, cache_ref,
                        )
                        .await
                        .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "library.browse_by_recording_type response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_LYRICS => {
                    let cache_ref = lyrics_cache.as_ref().ok_or_else(|| {
                        PluginError::Permanent(
                            "lyrics cache not wired at load".to_string(),
                        )
                    })?;
                    let response = enrichment::query_lyrics(
                        &payload,
                        &lrclib,
                        cache_ref,
                        &provider_config,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_lyrics response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_ARTIST_BIO => {
                    let cache_ref = bio_cache.as_ref().ok_or_else(|| {
                        PluginError::Permanent(
                            "bio cache not wired at load".to_string(),
                        )
                    })?;
                    let catalogue = cascade::ProviderCatalogue {
                        musicbrainz: Some(Arc::new(mb.clone())),
                        wikipedia: wikipedia.clone().map(Arc::new),
                        wikidata: wikidata.clone().map(Arc::new),
                        lrclib: Some(Arc::new(lrclib.clone())),
                        theaudiodb: theaudiodb.clone().map(Arc::new),
                        lastfm: lastfm.clone().map(Arc::new),
                        discogs: discogs.clone().map(Arc::new),
                        genius: genius.clone().map(Arc::new),
                        config: provider_config.clone(),
                    };
                    let response = enrichment::query_entity_bio(
                        &payload, &catalogue, cache_ref,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_artist_bio response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_ALBUM_NOTES => {
                    let cache_ref = notes_cache.as_ref().ok_or_else(|| {
                        PluginError::Permanent(
                            "notes cache not wired at load".to_string(),
                        )
                    })?;
                    let catalogue = cascade::ProviderCatalogue {
                        musicbrainz: Some(Arc::new(mb.clone())),
                        wikipedia: wikipedia.clone().map(Arc::new),
                        wikidata: wikidata.clone().map(Arc::new),
                        lrclib: Some(Arc::new(lrclib.clone())),
                        theaudiodb: theaudiodb.clone().map(Arc::new),
                        lastfm: lastfm.clone().map(Arc::new),
                        discogs: discogs.clone().map(Arc::new),
                        genius: genius.clone().map(Arc::new),
                        config: provider_config.clone(),
                    };
                    let response = enrichment::query_album_notes_cascade(
                        &payload, &catalogue, cache_ref,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_album_notes response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_RELEASE_CREDITS => {
                    let cache_ref =
                        credits_cache.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "credits cache not wired at load".to_string(),
                            )
                        })?;
                    let catalogue = cascade::ProviderCatalogue {
                        musicbrainz: Some(Arc::new(mb.clone())),
                        wikipedia: wikipedia.clone().map(Arc::new),
                        wikidata: wikidata.clone().map(Arc::new),
                        lrclib: Some(Arc::new(lrclib.clone())),
                        theaudiodb: theaudiodb.clone().map(Arc::new),
                        lastfm: lastfm.clone().map(Arc::new),
                        discogs: discogs.clone().map(Arc::new),
                        genius: genius.clone().map(Arc::new),
                        config: provider_config.clone(),
                    };
                    let response = enrichment::query_release_credits_cascade(
                        &payload, &catalogue, cache_ref,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_release_credits response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_TRACK_ANNOTATION => {
                    let cache_ref =
                        annotation_cache.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "annotation cache not wired at load"
                                    .to_string(),
                            )
                        })?;
                    let catalogue = cascade::ProviderCatalogue {
                        musicbrainz: Some(Arc::new(mb.clone())),
                        wikipedia: wikipedia.clone().map(Arc::new),
                        wikidata: wikidata.clone().map(Arc::new),
                        lrclib: Some(Arc::new(lrclib.clone())),
                        theaudiodb: theaudiodb.clone().map(Arc::new),
                        lastfm: lastfm.clone().map(Arc::new),
                        discogs: discogs.clone().map(Arc::new),
                        genius: genius.clone().map(Arc::new),
                        config: provider_config.clone(),
                    };
                    let response = enrichment::query_track_annotation_cascade(
                        &payload, &catalogue, cache_ref,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_track_annotation response JSON: {e}"
                        ))
                    })?
                }
                REQUEST_METADATA_QUERY_WORK_NOTES => {
                    let cache_ref =
                        work_notes_cache.as_ref().ok_or_else(|| {
                            PluginError::Permanent(
                                "work-notes cache not wired at load"
                                    .to_string(),
                            )
                        })?;
                    let catalogue = cascade::ProviderCatalogue {
                        musicbrainz: Some(Arc::new(mb.clone())),
                        wikipedia: wikipedia.clone().map(Arc::new),
                        wikidata: wikidata.clone().map(Arc::new),
                        lrclib: Some(Arc::new(lrclib.clone())),
                        theaudiodb: theaudiodb.clone().map(Arc::new),
                        lastfm: lastfm.clone().map(Arc::new),
                        discogs: discogs.clone().map(Arc::new),
                        genius: genius.clone().map(Arc::new),
                        config: provider_config.clone(),
                    };
                    let response = enrichment::query_work_notes_cascade(
                        &payload, &catalogue, cache_ref,
                    )
                    .await
                    .map_err(PluginError::Permanent)?;
                    response.json_bytes().map_err(|e| {
                        PluginError::Permanent(format!(
                            "metadata.query_work_notes response JSON: {e}"
                        ))
                    })?
                }
                other => {
                    return Err(PluginError::Permanent(format!(
                        "unknown request type (defensive): {other}"
                    )));
                }
            };
            Ok(Response::for_request(req, body))
        }
    }
}

/// Stable credential-vault key for the Last.fm API key.
const LASTFM_VAULT_KEY: &str = "lastfm_api_key";
/// Stable credential-vault key for the Discogs Personal Access
/// Token.
const DISCOGS_VAULT_KEY: &str = "discogs_personal_access_token";
/// Stable credential-vault key for the Genius client access token.
const GENIUS_VAULT_KEY: &str = "genius_client_access_token";

/// Fetch an operator-supplied credential from the framework vault
/// under `key`. Returns `None` when the vault is not wired, when
/// no row exists, or when the stored bytes are not valid UTF-8.
async fn resolve_credential_from_vault(
    vault: Option<
        &Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    >,
    key: &str,
) -> Option<String> {
    let handle = vault?;
    match handle.fetch(key.to_string()).await {
        Ok(Some(bytes)) => String::from_utf8(bytes).ok(),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                plugin = PLUGIN_NAME,
                key = key,
                error = %e,
                "credential vault fetch failed"
            );
            None
        }
    }
}

/// Fetch the Last.fm API key. Precedence:
///   1. Framework credential vault under `LASTFM_VAULT_KEY`.
///   2. Legacy config file key (pre-substrate `lastfm.api_key_path`
///      / `lastfm.api_key`). When the legacy value is present and
///      the vault is populated, upsert the legacy value into the
///      vault under `LASTFM_VAULT_KEY` and return it — one-shot
///      migration.
///   3. Absent — Last.fm-gated verbs return `not_configured`.
async fn resolve_lastfm_api_key(
    vault: Option<
        &Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    >,
    legacy_config_key: Option<String>,
) -> Option<String> {
    if let Some(handle) = vault {
        match handle.fetch(LASTFM_VAULT_KEY.to_string()).await {
            Ok(Some(bytes)) => {
                if let Ok(s) = String::from_utf8(bytes) {
                    return Some(s);
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "credential vault lastfm fetch failed"
                );
            }
        }
    }
    if let Some(legacy) = legacy_config_key {
        if let Some(handle) = vault {
            let metadata =
                evo_plugin_sdk::contract::context::CredentialMetadata {
                    display_name: Some(
                        "Last.fm API key (migrated from plugin config)"
                            .to_string(),
                    ),
                    expires_at_ms: None,
                    uninstall_policy:
                        evo_plugin_sdk::contract::context::UninstallPolicy::PreserveForReinstall,
                };
            if let Err(e) = handle
                .store(
                    LASTFM_VAULT_KEY.to_string(),
                    legacy.as_bytes().to_vec(),
                    metadata,
                )
                .await
            {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    error = %e,
                    "credential vault lastfm migration store failed"
                );
            }
        }
        return Some(legacy);
    }
    None
}

// -----------------------------------------------------------------
// Provider-client builders. Shared between load-time construction
// and the credential-change reactor so both paths produce clients
// with matching rate-limit + user-agent + HTTPS settings.
// -----------------------------------------------------------------

/// Construct a fresh Last.fm client from an operator-supplied key.
fn build_lastfm_client(
    http: &evo_online_providers::HttpClient,
    config: &PluginConfig,
    key: String,
) -> LastfmClient {
    let rate = Arc::new(RateLimiter::new(config.lastfm_min_interval));
    LastfmClient::new(
        http.clone(),
        rate,
        config.musicbrainz_user_agent.clone(),
        key,
    )
}

/// Construct a fresh Discogs client from an operator-supplied
/// personal access token. Returns `None` when the token is
/// rejected by the client constructor (empty / malformed).
fn build_discogs_client(
    http: &evo_online_providers::HttpClient,
    config: &PluginConfig,
    token: String,
) -> Option<DiscogsClient> {
    let rate = Arc::new(RateLimiter::new(Duration::from_millis(1000)));
    DiscogsClient::new(
        http.clone(),
        rate,
        config.musicbrainz_user_agent.clone(),
        token,
    )
}

/// Construct a fresh Genius client from an operator-supplied
/// client access token. Returns `None` when the token is
/// rejected by the client constructor.
fn build_genius_client(
    http: &evo_online_providers::HttpClient,
    config: &PluginConfig,
    token: String,
) -> Option<GeniusClient> {
    let rate = Arc::new(RateLimiter::new(Duration::from_millis(250)));
    GeniusClient::new(
        http.clone(),
        rate,
        config.musicbrainz_user_agent.clone(),
        token,
    )
}

// -----------------------------------------------------------------
// Credential-change reactor.
// -----------------------------------------------------------------

/// Background task spawned at load time. Awaits
/// `CredentialSetChanged` events on the framework's central
/// per-plugin change bus (via the SDK's `subscribe_changes`
/// receiver) and re-resolves affected provider clients in place
/// — no plugin restart. Terminates on:
///
/// - `RecvError::Closed` — the framework's sender dropped
///   (steward teardown or plugin uninstall).
/// - `JoinHandle::abort` from the plugin's `unload` path.
///
/// `RecvError::Lagged` never terminates: the reactor logs the
/// dropped-event count and continues so a burst of operator
/// gestures does not stall the substrate.
#[allow(clippy::too_many_arguments)]
async fn credential_reactor(
    mut rx: tokio::sync::broadcast::Receiver<
        evo_plugin_sdk::contract::context::CredentialChangeEvent,
    >,
    vault: Arc<dyn evo_plugin_sdk::contract::context::CredentialVaultHandle>,
    lastfm_slot: Arc<RwLock<Option<LastfmClient>>>,
    discogs_slot: Arc<RwLock<Option<DiscogsClient>>>,
    genius_slot: Arc<RwLock<Option<GeniusClient>>>,
    http: evo_online_providers::HttpClient,
    config: PluginConfig,
) {
    use evo_plugin_sdk::contract::context::CredentialChangeKind;
    loop {
        match rx.recv().await {
            Ok(event) => {
                for key in &event.changed_keys {
                    match key.as_str() {
                        LASTFM_VAULT_KEY => {
                            let new_client =
                                match event.kind {
                                    CredentialChangeKind::Delete => None,
                                    CredentialChangeKind::Put => {
                                        resolve_credential_from_vault(
                                            Some(&vault),
                                            LASTFM_VAULT_KEY,
                                        )
                                        .await
                                        .map(|k| {
                                            build_lastfm_client(
                                                &http, &config, k,
                                            )
                                        })
                                    }
                                };
                            *lastfm_slot.write().await = new_client;
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                key = LASTFM_VAULT_KEY,
                                kind = ?event.kind,
                                "reactor: re-resolved Last.fm client"
                            );
                        }
                        DISCOGS_VAULT_KEY => {
                            let new_client = match event.kind {
                                CredentialChangeKind::Delete => None,
                                CredentialChangeKind::Put => {
                                    resolve_credential_from_vault(
                                        Some(&vault),
                                        DISCOGS_VAULT_KEY,
                                    )
                                    .await
                                    .and_then(|t| {
                                        build_discogs_client(&http, &config, t)
                                    })
                                }
                            };
                            *discogs_slot.write().await = new_client;
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                key = DISCOGS_VAULT_KEY,
                                kind = ?event.kind,
                                "reactor: re-resolved Discogs client"
                            );
                        }
                        GENIUS_VAULT_KEY => {
                            let new_client = match event.kind {
                                CredentialChangeKind::Delete => None,
                                CredentialChangeKind::Put => {
                                    resolve_credential_from_vault(
                                        Some(&vault),
                                        GENIUS_VAULT_KEY,
                                    )
                                    .await
                                    .and_then(|t| {
                                        build_genius_client(&http, &config, t)
                                    })
                                }
                            };
                            *genius_slot.write().await = new_client;
                            tracing::info!(
                                plugin = PLUGIN_NAME,
                                key = GENIUS_VAULT_KEY,
                                kind = ?event.kind,
                                "reactor: re-resolved Genius client"
                            );
                        }
                        _other => {
                            // Event touched a key this plugin does
                            // not consume (future-provider slot).
                            // Silently ignore.
                        }
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    plugin = PLUGIN_NAME,
                    "credential-change reactor: sender closed, exiting"
                );
                return;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    skipped = n,
                    "credential-change reactor lagged; dropped events"
                );
            }
        }
    }
}

// -----------------------------------------------------------------
// Online-provider-config reactor.
// -----------------------------------------------------------------

/// Background task spawned at load time. Awaits change events on
/// the framework's `online_provider_config` bus and applies each
/// operator gesture to the plugin's local `provider_config` in
/// place — no plugin restart. The next verb dispatch sees the
/// new enable / priority and behaves accordingly (a disabled
/// source stops appearing in `sources[]`; a re-ordered source
/// changes which entry mirrors as the top-level default).
///
/// Terminates on:
/// - `RecvError::Closed` — the framework's sender dropped
///   (steward teardown or plugin uninstall).
/// - `JoinHandle::abort` from the plugin's `unload` path.
///
/// `RecvError::Lagged` never terminates: the reactor logs the
/// dropped-event count and continues so a burst of operator
/// gestures does not stall the substrate.
async fn online_provider_config_reactor(
    mut rx: tokio::sync::broadcast::Receiver<
        evo_plugin_sdk::contract::context::OnlineProviderConfigChangeEvent,
    >,
    config_slot: Arc<RwLock<cascade::ProviderConfig>>,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Some(pid) =
                    cascade::ProviderId::from_wire(&event.provider_id)
                else {
                    tracing::debug!(
                        plugin = PLUGIN_NAME,
                        provider_id = %event.provider_id,
                        "reactor: config change names a provider this plugin \
                         does not implement; skipping"
                    );
                    continue;
                };
                // Sentinel: priority < 0 means "operator has not
                // explicitly set a priority for this provider"
                // (migration 042). Keep the plugin's cascade
                // default; still apply enabled.
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
                    "reactor: applied online_provider_config change"
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
