// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Audio-distribution HTTP surfaces.
//!
//! Artwork resolve and serve, track-detail aggregation, and the
//! cascade that composes local and online providers into one
//! answer. This is product: it knows about albums, artists,
//! cover files and named online services, and it belongs to the
//! distribution that ships those things.
//!
//! It lived in the framework's HTTP crate until now. Plugins may
//! not call one another, so composing local-then-online needs a
//! composer, and the framework was the only place with a router
//! to mount on. That is no longer true: a distribution supplies
//! an HTTPS hookup, receives the router the framework finished
//! building, and mounts its own surfaces on it. This crate is
//! what that hookup mounts.
//!
//! The framework keeps the substrate underneath — content-
//! addressed blob storage, plugin dispatch, auth, TLS — and
//! knows nothing about what is being served over it.
//!
//! ## One home
//!
//! Both HTTP surfaces share a single [`ArtworkCascade`]. Two
//! surfaces resolving the same target through different code
//! would eventually disagree, and did: the same album once
//! returned art from the standalone endpoint and nothing from
//! the composite one. [`mount`] constructs the cascade once and
//! clones the handle into everything that needs it.

pub mod artwork_admission;
pub mod artwork_cascade;
pub mod artwork_endpoint;
pub mod artwork_negative_cache;
pub mod artwork_resolve_coalescer;
pub mod artwork_resolve_endpoint;
pub mod artwork_resolve_index;
pub mod captive_session_endpoint;
pub mod track_detail_endpoint;

use std::sync::Arc;

pub use artwork_cascade::{ArtworkCascade, ArtworkResolvedNotifier};
pub use artwork_resolve_index::ArtworkResolveIndex;

/// Everything [`mount`] needs. Assembled by the distribution from
/// the context its HTTPS hookup receives.
pub struct MountConfig {
    /// Path prefix the framework mounted its own routes under.
    pub api_prefix: String,
    /// Plugin dispatch. The cascade composes provider plugins
    /// through this; the plugins never call each other.
    pub dispatcher: Arc<dyn evo_runtime_http::Dispatcher>,
    /// Content-addressed blob store for resolved bytes.
    pub asset_cache:
        Option<Arc<dyn evo_plugin_sdk::contract::asset_cache::AssetCache>>,
    /// Bearer validator, shared with framework routes.
    pub validator: Arc<evo_auth_bearer::BearerTokenValidator>,
    /// Auth-tier provider, shared with framework routes.
    pub tier_provider: Arc<dyn evo_runtime_http::AuthTierProvider>,
    /// Capabilities granted to LAN-trusted callers.
    pub lan_trust_caps: evo_auth_bearer::CapabilitySet,
    /// Directory the persistent resolve index lives under.
    pub state_dir: std::path::PathBuf,
    /// Told when a resolve lands, so a surface already on screen
    /// can repaint. The distribution supplies one that announces
    /// on the happenings bus under its own claimant name.
    pub resolved_notifier: Option<Arc<dyn ArtworkResolvedNotifier>>,
}

/// Mount the distribution's audio HTTP surfaces onto the router
/// the framework handed over.
///
/// The single attach site for this product. Nothing in the
/// framework's own router construction knows these routes exist.
pub fn mount(
    router: evo_runtime_http::Router,
    cfg: MountConfig,
) -> Result<evo_runtime_http::Router, evo_runtime_http::RuntimeHttpError> {
    let resolve_index =
        Some(Arc::new(ArtworkResolveIndex::new(cfg.state_dir.clone())));

    // One cascade, cloned into every surface that needs it.
    let cascade = ArtworkCascade::new(
        Arc::clone(&cfg.dispatcher),
        cfg.asset_cache.clone(),
        resolve_index,
    );
    let cascade = match cfg.resolved_notifier {
        Some(notifier) => cascade.with_resolved_notifier(notifier),
        None => cascade,
    };

    let mut router = router;

    // Content-addressed byte serving, when a blob store is wired.
    if let Some(asset_cache) = cfg.asset_cache.clone() {
        router = artwork_endpoint::attach_artwork_endpoint(
            router,
            &cfg.api_prefix,
            asset_cache,
            Arc::clone(&cfg.validator),
            Arc::clone(&cfg.tier_provider),
            cfg.lan_trust_caps.clone(),
        )?;
    }

    router = artwork_resolve_endpoint::attach_artwork_resolve_endpoint(
        router,
        &cfg.api_prefix,
        Arc::clone(&cascade),
        Arc::clone(&cfg.validator),
        Arc::clone(&cfg.tier_provider),
        cfg.lan_trust_caps.clone(),
    )?;

    router = track_detail_endpoint::attach_track_detail_endpoint(
        router,
        &cfg.api_prefix,
        Arc::clone(&cfg.dispatcher),
        Arc::clone(&cascade),
        Arc::clone(&cfg.validator),
        Arc::clone(&cfg.tier_provider),
        cfg.lan_trust_caps.clone(),
    )?;

    // Device-proxied captive-portal session surface. Product for
    // the same reason the artwork presenters are: it exists
    // because a venue portal has to be fetched over the interface
    // carrying it, which is a fact about this distribution's
    // networking, not about serving HTTP. The route and its gate
    // are unchanged by the move — only who mounts it.
    router = captive_session_endpoint::attach_captive_session_endpoint(
        router,
        &cfg.api_prefix,
        Arc::clone(&cfg.dispatcher),
        Arc::clone(&cfg.validator),
        Arc::clone(&cfg.tier_provider),
        cfg.lan_trust_caps,
    )?;

    Ok(router)
}
