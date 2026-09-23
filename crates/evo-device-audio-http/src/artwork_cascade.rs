// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Shared artwork resolve cascade.
//!
//! Single implementation of the local → identity-synthesised-
//! online cascade, consumed by both the standalone artwork
//! resolve endpoint (`GET /api/v1/audio/artwork`) and the
//! composite track-detail endpoint's artwork sub-source. Both
//! surfaces MUST return the same content_hash for the same
//! target — the guardrail is one-canonical-path.
//!
//! The service holds every stateful piece of the pipeline:
//! the negative memo, the coalescer, and the admission gate.
//! `resolve()` runs the full cascade with all three composed
//! in the right order — negative memo pre-check → coalescer
//! (which absorbs concurrent same-key callers into one plugin
//! dispatch) → admission gate INSIDE the fetcher (same-key
//! waiters share the fetcher's permit) → local tier → local
//! NotFound branch synthesises an mpd-album target for the
//! online tier when identity is present → returns
//! [`CascadeOutcome`] in every terminal case.
//!
//! Callers (endpoints) translate `CascadeOutcome` into their
//! transport-appropriate response shape:
//!
//! - artwork_resolve_endpoint → 302 / 404 / 503 with
//!   `X-Artwork-Provider` header.
//! - track_detail_endpoint → SubSource {ok, not_found, error}
//!   with `provider_id` inline.

use std::sync::Arc;

use evo_plugin_sdk::contract::AssetCache;
use serde_json::{json, Value};

use crate::artwork_admission::{Admission, AdmissionError};
use crate::artwork_negative_cache::NegativeCache;
use crate::artwork_resolve_coalescer::{
    ArtworkResolveCoalescer, CoalesceError,
};
use evo_projection_core::WireOpId;
use evo_runtime_http::dispatcher::Dispatcher;
use evo_runtime_http::principal::Principal;

/// Terminal outcome of `resolve()`. Every variant is
/// exhaustive with a clear operator-readable detail so the
/// caller (endpoint) can render an accurate response.
#[derive(Debug, Clone)]
pub enum CascadeOutcome {
    /// A provider (local or online) or a positive coalescer
    /// memo produced content.
    Resolved {
        /// Content-addressed hash the caller redirects to for
        /// bytes (immutable-cacheable at the hash endpoint).
        content_hash: String,
        /// Which provider produced the content
        /// (`local_sidecar`, `local_embedded`, `cover_art_archive`,
        /// `lastfm`, `itunes`, `volumio_meta`). Absent on
        /// pre-provenance plugin builds.
        provider_id: Option<String>,
    },
    /// Both provider tiers structured-NotFound — the caller
    /// UI's placeholder floor applies. `provider_id` carries
    /// the last-tier id from the fresh cascade OR from a
    /// negative-memo hit (so the caller can label "we tried
    /// and gave up at online/itunes" vs "at local").
    NotFound {
        /// Last-tier provider id from the fresh cascade or from
        /// the negative-memo hit. Absent on cold all-tier
        /// misses (the endpoint substitutes a boundary default).
        provider_id: Option<String>,
        /// Human-readable failure message ("no sidecar; no
        /// embedded picture", "cover_art_archive: 404").
        detail: String,
    },
    /// Caller-input error — bad_request from a plugin, bad
    /// scheme/value/size at the endpoint boundary. Not
    /// cached negatively (the client can fix and retry).
    BadRequest {
        /// Boundary or plugin-emitted refusal message.
        detail: String,
    },
    /// Admission bucket saturated; caller should retry after
    /// backoff. Signals structured backpressure to the UI.
    AdmissionDeadline {
        /// Which bucket + how long the caller waited before
        /// the admission gate refused.
        detail: String,
    },
    /// Coalescer's inflight sleeper deadline elapsed — the
    /// in-flight fetcher is taking longer than the framework
    /// tolerates. Caller should retry.
    CoalescerDeadline {
        /// Key + waited_ms — the framework's retry hint.
        detail: String,
    },
    /// Dispatch / envelope / wire-op failure — not a tier
    /// fault; the operator sees the underlying error.
    Transient {
        /// Underlying wire / envelope / dispatch error.
        detail: String,
    },
}

/// Provenance identifier for negative-cache hits that did
/// not memoise a specific tier id — the operator UI can
/// distinguish memoised negatives from live cascade misses.
pub const PROVENANCE_NEGATIVE_CACHE: &str = "negative_cache";

/// Every size the `?refresh=1` fan-out evicts for a target.
/// Must mirror the size taxonomy the endpoint accepts so
/// stale-cached siblings can't be served after a refresh
/// gesture. `tiny` (a backward-compat alias for `small`) is
/// canonicalised at endpoint entry, so it doesn't need its
/// own row here — the caller's original alias is still evicted
/// separately by the outer `forget` if it's not in this list.
const FORGET_SIZE_FANOUT: &[&str] = &["small", "medium", "large", "original"];

/// Provenance identifier surfaced on positive-hit responses
/// served straight from the persistent resolve-index without
/// re-running the plugin cascade. Operator diagnostic surfaces
/// use this to distinguish "we shortcut the browse from the
/// index" from "we ran the full local → online chain" — the
/// gap in cost between the two is orders of magnitude on
/// large libraries.
pub const PROVENANCE_RESOLVE_INDEX: &str = "resolve_index";

/// Version of the artwork selection contract.
///
/// Stamped on every resolve-index row and checked on every read.
/// A row written under a different value is refused and the
/// cascade re-runs once, so a corrected picker reaches the glass
/// without an operator gesture.
///
/// The index is otherwise a permanent positive — no TTL, no
/// revalidation — which is exactly what it must be for browse
/// cost: a TTL would re-hit providers on a timer. Versioning
/// invalidates on *cause* instead of on time: nothing expires
/// while the rules are unchanged, and everything is reconsidered
/// the moment they change.
///
/// BUMP THIS when any of the following changes anywhere in the
/// artwork path, framework or plugin:
///   - which source wins (priority order, tie-breaking, the
///     payload picker)
///   - what is rejected (placeholder / non-photograph rules,
///     URL-shape filters, host refusals)
///   - the set of providers dispatched by default
///
/// A missed bump is not a crash; it is a silently stale tile,
/// which is the failure this constant exists to end. When in
/// doubt, bump it — the cost is one re-resolve per artist.
///
/// History:
///   1 — pixel-based non-photograph rejection replaces the dead
///       content-hash placeholder guard; provider priority
///       sentinel restores the intended cascade order.
pub const CASCADE_LOGIC_VERSION: u32 = 1;

/// What an operator clear gesture actually removed.
///
/// Counts are `None` for the targeted scope, where the unit of
/// work is one target rather than a sweep, and `Some` for the
/// all-scope sweep. `plugin_cleared` reports the fan-out
/// separately so a partial outcome is legible instead of being
/// flattened into a success the operator cannot trust.
#[derive(Debug, Clone)]
pub struct ArtworkForgetOutcome {
    /// `"targeted"` or `"all"`.
    pub scope: &'static str,
    /// Resolve-index entries removed (all-scope only).
    pub index_entries_removed: Option<usize>,
    /// Asset-cache blobs deleted (all-scope only).
    pub assets_deleted: Option<usize>,
    /// Whether the plugin fan-out succeeded.
    pub plugin_cleared: bool,
    /// Why the fan-out failed, when it did.
    pub plugin_detail: Option<String>,
}

/// Told when a resolve lands, so a surface already on screen
/// can repaint itself.
///
/// Artwork resolution is asynchronous with respect to the paint
/// that asked for it: a tile requests a target, the cascade may
/// have to reach a rate-limited provider, and by the time bytes
/// exist the tile has long since drawn its glyph. Without a
/// signal the operator's only recourse is to navigate away and
/// back, or press Refresh — for artwork that arrived seconds
/// later through no fault of theirs.
///
/// Implemented in the framework crate over the happenings bus.
/// It is a trait here because the dependency runs the other way:
/// `evo` depends on this crate, so the cascade cannot reach the
/// bus directly and is handed an emitter instead — the same
/// shape as [`Dispatcher`].
pub trait ArtworkResolvedNotifier: Send + Sync {
    /// A `(scheme, value, size)` target now resolves to
    /// `content_hash`. Called once per landed resolve, never on
    /// a warm index hit.
    fn artwork_resolved(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        content_hash: &str,
    );
}

/// The shared cascade service. Constructed once at boot;
/// shared via `Arc` by every endpoint that needs artwork
/// resolution.
pub struct ArtworkCascade {
    dispatcher: Arc<dyn Dispatcher>,
    coalescer: Arc<ArtworkResolveCoalescer>,
    admission: Arc<Admission>,
    negative_cache: Arc<NegativeCache>,
    /// Persistent (scheme, value, size) → content_hash index.
    /// The FAST PATH: every resolve consults this before the
    /// coalescer memo and the plugin dispatch. On hit the
    /// endpoint 302-redirects to `/api/v1/audio/artwork/<hash>`
    /// without touching the plugin — turning browse artwork
    /// resolution from an O(library) per-tile tag walk into
    /// an O(1) lookup that survives restart, memo expiry, and
    /// coalescer eviction. Populated on every successful
    /// resolve (see `remember_positive`) and evicted by the
    /// operator's `?refresh=1` gesture (see `forget`).
    resolve_index:
        Option<Arc<crate::artwork_resolve_index::ArtworkResolveIndex>>,
    /// AssetCache handle used by the `?refresh=1` gesture to
    /// evict the positive-hash entry when the operator asks for
    /// a fresh resolve. `None` when the steward has no asset
    /// cache wired — in that case the cascade still works but
    /// the refresh gesture can only evict the negative memo.
    asset_cache: Option<Arc<dyn AssetCache>>,
    /// Optional sink told when a resolve lands. See
    /// [`ArtworkResolvedNotifier`].
    resolved_notifier: Option<Arc<dyn ArtworkResolvedNotifier>>,
}

impl ArtworkCascade {
    /// Construct a cascade with the caller-supplied dispatcher
    /// and default coalescer / admission / negative-cache
    /// tunables. The framework router builds one at boot and
    /// clones its `Arc<Self>` into every endpoint that needs
    /// artwork resolution.
    ///
    /// `asset_cache` is optional: `Some` wires the
    /// positive-index eviction path so `?refresh=1` drops the
    /// content-hash bytes; `None` leaves the cascade with
    /// negative-memo-only eviction.
    ///
    /// `resolve_index` is optional: `Some` wires the O(1)
    /// persistent positive index so browse resolves become
    /// index-lookup-fast; `None` leaves the cascade with
    /// coalescer-memo-only positive memoisation (30 s TTL,
    /// in-memory, lost on restart — the pre-2026-07-29
    /// behaviour).
    pub fn new(
        dispatcher: Arc<dyn Dispatcher>,
        asset_cache: Option<Arc<dyn AssetCache>>,
        resolve_index: Option<
            Arc<crate::artwork_resolve_index::ArtworkResolveIndex>,
        >,
    ) -> Arc<Self> {
        Arc::new(Self {
            dispatcher,
            coalescer: Arc::new(ArtworkResolveCoalescer::new()),
            admission: Arc::new(Admission::new()),
            negative_cache: Arc::new(NegativeCache::new()),
            resolve_index,
            asset_cache,
            resolved_notifier: None,
        })
    }

    /// Wire the sink told when a resolve lands.
    ///
    /// Separate from [`Self::new`] so every existing caller and
    /// test keeps working unchanged, and so a deployment that
    /// wants no signal simply never calls this.
    pub fn with_resolved_notifier(
        self: Arc<Self>,
        notifier: Arc<dyn ArtworkResolvedNotifier>,
    ) -> Arc<Self> {
        Arc::new(Self {
            dispatcher: Arc::clone(&self.dispatcher),
            coalescer: Arc::clone(&self.coalescer),
            admission: Arc::clone(&self.admission),
            negative_cache: Arc::clone(&self.negative_cache),
            resolve_index: self.resolve_index.clone(),
            asset_cache: self.asset_cache.clone(),
            resolved_notifier: Some(notifier),
        })
    }

    /// Evict any negative memo AND any positive-hash entry for
    /// `(scheme, value, size)`.
    ///
    /// The operator escape hatch: `?refresh=1` on the resolve
    /// endpoint calls this before dispatching, so a target
    /// whose memo is stuck on a pre-fix / pre-retag miss can
    /// be cleared without waiting for TTL. When a positive
    /// hash is memoised for the same target, this call also
    /// evicts the AssetCache bytes at that hash — completing
    /// the "clear + re-resolve" gesture the operator UI
    /// invokes when it wants a fresh serve regardless of
    /// whether the last outcome was positive or negative.
    /// Returns `(index_entries_removed, assets_deleted)` so the
    /// operator gesture can report what it actually removed.
    pub async fn forget(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> (usize, usize) {
        // Evict every stored size for the (scheme, value), not
        // just the caller's requested size. The caller-supplied
        // `size` is the paint's exact request; but the
        // resolve/size-fallback path (see `resolve_with_origin`)
        // will happily serve a `medium` bytes-hash for a `small`
        // request when the exact key misses. Evicting only the
        // requested size would leave sibling-size entries intact,
        // and the very next resolve would fall back through
        // `get_any_size` to those stale bytes — the operator's
        // `?refresh=1` gesture would appear to no-op.
        //
        // The correct semantic: `?refresh=1` clears everything
        // for the target, then the next resolve genuinely re-
        // runs the cascade.
        let mut index_entries = 0usize;
        let mut assets = 0usize;
        for candidate in FORGET_SIZE_FANOUT {
            let (i, a) = self.forget_one_size(scheme, value, candidate).await;
            index_entries += i;
            assets += a;
        }
        // Also evict the caller-supplied size in case it's an
        // alias not in the fan-out list (e.g. "tiny").
        if !FORGET_SIZE_FANOUT.contains(&size) {
            let (i, a) = self.forget_one_size(scheme, value, size).await;
            index_entries += i;
            assets += a;
        }
        (index_entries, assets)
    }

    /// The operator's "clear this artist's artwork" gesture.
    ///
    /// Evicts the framework's stored bytes and index entries for
    /// one target, then fans the same gesture out to the artwork
    /// provider plugins so their own memos drop in the same
    /// breath.
    ///
    /// The fan-out is not optional bookkeeping. The framework
    /// owns the `(scheme, value, size) → hash` index and the
    /// bytes; the plugins own the provider results that produced
    /// them. Clearing only the framework half means the next
    /// resolve re-runs the cascade, gets the identical provider
    /// URL back from a plugin memo, and repaints the identical
    /// image — the operator sees a clear that changed nothing.
    /// Clearing only the plugin half leaves the index short-
    /// circuiting to the old bytes and never consulting a
    /// provider at all. Only both together mean "forget".
    pub async fn forget_target(
        &self,
        scheme: &str,
        value: &str,
        principal: &Principal,
    ) -> ArtworkForgetOutcome {
        let (index_entries, assets) = self.forget(scheme, value, "large").await;
        let plugin = self
            .clear_plugin_caches(
                json!({
                    "v": 1,
                    "target": { "scheme": scheme, "value": value },
                }),
                principal,
            )
            .await;
        ArtworkForgetOutcome {
            scope: "targeted",
            index_entries_removed: Some(index_entries),
            assets_deleted: Some(assets),
            plugin_cleared: plugin.is_ok(),
            plugin_detail: plugin.err(),
        }
    }

    /// The operator's "clear all artwork" gesture.
    ///
    /// Walks the resolve index, evicts every byte blob it points
    /// at, drops every entry, and fans out to the plugins.
    ///
    /// Reported counts are what was actually removed, not what
    /// was attempted — an operator who is told "cleared" while
    /// bytes survived is exactly the failure this gesture exists
    /// to end.
    pub async fn forget_everything(
        &self,
        principal: &Principal,
    ) -> ArtworkForgetOutcome {
        let mut index_entries = 0usize;
        let mut assets_deleted = 0usize;
        if let Some(index) = &self.resolve_index {
            match index.drop_all().await {
                Ok(hashes) => {
                    index_entries = hashes.len();
                    if let Some(cache) = &self.asset_cache {
                        for hash in hashes {
                            match cache.delete(&hash).await {
                                Ok(true) => assets_deleted += 1,
                                Ok(false) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        content_hash = %hash,
                                        error = %e,
                                        "artwork clear-all: asset delete failed"
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "artwork clear-all: resolve-index sweep failed"
                    );
                }
            }
        }
        // In-memory memos last: a concurrent resolve that raced
        // the sweep above may have re-populated one, and this
        // catches it. They are cheap to rebuild and carry no
        // bytes, so clearing them unconditionally is safe.
        self.negative_cache.clear();
        self.coalescer.clear();
        let plugin =
            self.clear_plugin_caches(json!({ "v": 1 }), principal).await;
        tracing::info!(
            index_entries_removed = index_entries,
            assets_deleted = assets_deleted,
            plugin_cleared = plugin.is_ok(),
            "artwork clear-all completed"
        );
        ArtworkForgetOutcome {
            scope: "all",
            index_entries_removed: Some(index_entries),
            assets_deleted: Some(assets_deleted),
            plugin_cleared: plugin.is_ok(),
            plugin_detail: plugin.err(),
        }
    }

    /// Fan a clear gesture out to the artwork provider shelf.
    ///
    /// A plugin that refuses or is absent is reported, never
    /// fatal: the framework half of the eviction has already
    /// happened and is the half that owns the served bytes. The
    /// operator is told what did and did not clear rather than
    /// being handed a blanket success.
    async fn clear_plugin_caches(
        &self,
        payload: Value,
        principal: &Principal,
    ) -> Result<(), String> {
        use base64::Engine;
        let payload_b64 = base64::engine::general_purpose::STANDARD
            .encode(payload.to_string().as_bytes());
        let envelope = json!({
            "shelf": "artwork.providers",
            "request_type": "artwork.online.clear_cache",
            "payload_b64": payload_b64,
        });
        let op_id = WireOpId::new("request")
            .map_err(|e| format!("wire-op id construction failed: {e}"))?;
        // Transport success is not gesture success. A plugin
        // that refuses returns a perfectly good Response whose
        // payload says `bad_request`; reporting that as cleared
        // is how an operator gets told their cache was emptied
        // while it was not. Peel the payload and read the
        // plugin's own status, the same way the resolve path
        // does.
        let envelope_out = self
            .dispatcher
            .dispatch(&op_id, envelope, principal)
            .await
            .map_err(|e| {
                format!("artwork.online.clear_cache dispatch failed: {e}")
            })?;
        let Some(response) = peel_plugin_response(&envelope_out) else {
            return Err(format!(
                "artwork.online.clear_cache returned a malformed envelope \
                 (no payload_b64 or non-JSON inner payload): {envelope_out}"
            ));
        };
        match response.get("status").and_then(Value::as_str) {
            Some("ok") => Ok(()),
            Some(other) => {
                let detail = response
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or("no detail");
                Err(format!(
                    "artwork.online.clear_cache refused: status={other} \
                     ({detail})"
                ))
            }
            None => Err(format!(
                "artwork.online.clear_cache response carried no status: \
                 {response}"
            )),
        }
    }

    /// Evict one size, reporting `(index_entries, assets)`
    /// actually removed so callers can tell the operator what
    /// happened rather than asserting success.
    async fn forget_one_size(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> (usize, usize) {
        let neg_key = (scheme.to_string(), value.to_string(), size.to_string());
        self.negative_cache.forget(&neg_key);
        // Evict the coalescer's memo too — without this, the
        // next resolve for the same target re-hydrates the
        // cached outcome (content_hash + provider_id) from the
        // memo and skips the plugin dispatch. That would race
        // with the AssetCache eviction below: the endpoint
        // would 302 to the same hash whose bytes we just
        // deleted, and the hash endpoint would 404 until the
        // memo TTL elapsed.
        self.coalescer.forget(scheme, value, size);
        // Look up and remove the persistent resolve-index
        // entry before touching the AssetCache. Order matters:
        // if the AssetCache delete succeeds and the index
        // remove fails, the index would point at bytes that no
        // longer exist — a persistent inconsistency. Removing
        // the index first means a subsequent AssetCache
        // failure only wastes bytes (which the next successful
        // resolve overwrites).
        let mut index_entries = 0usize;
        let mut assets = 0usize;
        let hash = if let Some(index) = &self.resolve_index {
            let h = index.get(scheme, value, size).await.map(|hit| hit.hash);
            match index.forget(scheme, value, size).await {
                Ok(true) => index_entries += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        scheme = %scheme,
                        value = %value,
                        size = %size,
                        error = %e,
                        "artwork cascade: resolve-index forget failed"
                    );
                }
            }
            h
        } else {
            None
        };
        if let Some(hash) = hash {
            if let Some(cache) = &self.asset_cache {
                match cache.delete(&hash).await {
                    Ok(existed) => {
                        if existed {
                            assets += 1;
                        }
                        tracing::info!(
                            scheme = %scheme,
                            value = %value,
                            size = %size,
                            content_hash = %hash,
                            existed = existed,
                            "artwork cascade: positive-index refresh evicted asset-cache entry"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            scheme = %scheme,
                            value = %value,
                            size = %size,
                            content_hash = %hash,
                            error = %e,
                            "artwork cascade: positive-index refresh could not evict asset-cache entry"
                        );
                    }
                }
            }
        }
        (index_entries, assets)
    }

    /// Check that a resolve-index short-circuit's claimed
    /// hash still has bytes in the AssetCache. Returns
    /// `true` when the bytes are reachable, `false` when the
    /// mapping has drifted (index outlived cache eviction /
    /// operator delete / disk corruption) — and in that case
    /// evicts the drifted (scheme, value, size) entry so the
    /// next resolve re-runs the cascade instead of returning
    /// the same dead hash forever.
    ///
    /// Design note: the extra `has` costs one filesystem
    /// `stat` per short-circuit hit — the trait's default
    /// impl reads the bytes, but the framework's
    /// `FilesystemAssetCache` overrides `has` to a metadata
    /// probe so this stays cheap enough for library-scale
    /// browses. The stat cost is the price of self-healing
    /// drift; without it, once a mapping goes stale the
    /// operator UI paints a permanent broken-image icon and
    /// the only remediation is an out-of-band `?refresh=1`.
    ///
    /// Absence of an `asset_cache` is treated as "cannot
    /// verify" and the mapping is returned unchanged — the
    /// downstream hash endpoint will surface its own 404
    /// verbatim, which is the pre-existing (worse) behaviour;
    /// this shim only sharpens what happens when the cache
    /// IS wired.
    async fn index_entry_bytes_present(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        hash: &str,
    ) -> bool {
        let Some(cache) = &self.asset_cache else {
            return true;
        };
        match cache.has(hash).await {
            Ok(true) => true,
            Ok(false) => {
                tracing::info!(
                    scheme = %scheme,
                    value = %value,
                    size = %size,
                    content_hash = %hash,
                    "artwork cascade: resolve-index entry pointed at hash \
                     whose bytes are absent from asset cache; evicting \
                     stale entry and falling through to full cascade"
                );
                if let Some(index) = &self.resolve_index {
                    if let Err(e) = index.forget(scheme, value, size).await {
                        tracing::warn!(
                            scheme = %scheme,
                            value = %value,
                            size = %size,
                            error = %e,
                            "artwork cascade: forget of drifted resolve-index \
                             entry failed; next resolve may short-circuit \
                             to same dead hash"
                        );
                    }
                }
                false
            }
            Err(e) => {
                // Probe itself faulted (I/O error). Treat as
                // "cannot verify" and return the mapping —
                // the alternative is a permanent 404 driven
                // by transient disk trouble, which is worse
                // than a possibly-stale hit whose downstream
                // 404 is at least the same as pre-shim
                // behaviour.
                tracing::warn!(
                    scheme = %scheme,
                    value = %value,
                    size = %size,
                    content_hash = %hash,
                    error = %e,
                    "artwork cascade: asset-cache has() probe faulted; \
                     returning resolve-index mapping unchecked"
                );
                true
            }
        }
    }

    /// Record a successful resolve so a later `?refresh=1`
    /// gesture can find the content hash to evict AND — more
    /// importantly — so the next resolve for this target
    /// short-circuits to an O(1) index lookup (fast path in
    /// `resolve`). Called from the cascade's Ok path.
    async fn remember_positive(
        &self,
        scheme: &str,
        value: &str,
        size: &str,
        content_hash: &str,
    ) {
        if let Some(index) = &self.resolve_index {
            if let Err(e) = index
                .put(scheme, value, size, content_hash, CASCADE_LOGIC_VERSION)
                .await
            {
                tracing::warn!(
                    scheme = %scheme,
                    value = %value,
                    size = %size,
                    content_hash = %content_hash,
                    error = %e,
                    "artwork cascade: resolve-index put failed; \
                     browse fast-path will miss until next successful put"
                );
            }
        }
        // Tell anyone already looking at this target that it now
        // has bytes.
        //
        // Emitted after the index write, so a surface that reacts
        // by re-requesting the same URL finds the fast path
        // populated rather than racing the cascade it just came
        // out of. Emitted regardless of whether the index write
        // succeeded: the bytes exist either way, and a repaint is
        // still the right thing — a failed index put costs a
        // slower second resolve, not a wrong picture.
        //
        // This is the only site that fires, and it sits on the
        // cascade's completion path. A warm index hit returns
        // earlier and never reaches here, so the signal tracks
        // resolves that LANDED rather than paints that happened.
        if let Some(notifier) = &self.resolved_notifier {
            notifier.artwork_resolved(scheme, value, size, content_hash);
        }
    }

    /// Run the cascade. `scheme` + `value` + `size` are the
    /// canonical inputs (size pre-canonicalised by the caller
    /// — e.g., `tiny` → `small`). Returns [`CascadeOutcome`]
    /// with a fully classified terminal state.
    pub async fn resolve(
        &self,
        principal: &Principal,
        scheme: &str,
        value: &str,
        size: &str,
    ) -> CascadeOutcome {
        let negative_key =
            (scheme.to_string(), value.to_string(), size.to_string());

        // Long-TTL negative memo pre-check.
        if let Some(entry) = self.negative_cache.get(&negative_key) {
            let provider_id = entry
                .provider_id
                .or_else(|| Some(PROVENANCE_NEGATIVE_CACHE.to_string()));
            let detail = format!(
                "artwork not resolved (memoised {}): status={} detail={}",
                PROVENANCE_NEGATIVE_CACHE, entry.status, entry.detail
            );
            // The memo records WHY, and the reply must preserve
            // it. This store holds two different things: a
            // provider-confirmed absence, and a short-lived note
            // that an upstream was failing (see
            // `memoise_transient_if_needed`). Collapsing both to
            // NotFound turned "MusicBrainz answered 503 nine
            // seconds ago" into "this album has no artwork" —
            // and a caller cannot retry what it is told is
            // settled. A local album whose provider tier merely
            // wobbled then stayed a glyph for the memo's whole
            // life.
            //
            // Replaying the transient class keeps the burst
            // protection this memo exists for — no re-dispatch,
            // the failing upstream is left alone — while still
            // answering something the caller can come back from.
            return match entry.status.as_str() {
                "unavailable" => CascadeOutcome::Transient { detail },
                "admission_deadline" => {
                    CascadeOutcome::AdmissionDeadline { detail }
                }
                "coalescer_deadline" => {
                    CascadeOutcome::CoalescerDeadline { detail }
                }
                // A provider-confirmed absence. This is the only
                // memo class that is a verdict.
                _ => CascadeOutcome::NotFound {
                    provider_id,
                    detail,
                },
            };
        }

        // Persistent positive-index FAST PATH. Consulted BEFORE
        // the coalescer memo and the plugin dispatch so a
        // library-scale browse over resolved albums is O(1)
        // per tile — the index survives restart, memo expiry,
        // and the coalescer's 30 s TTL. The index is only a
        // claim about which hash to serve; if the AssetCache
        // bytes were evicted (LRU quota, operator delete), the
        // hash endpoint will surface the 404 verbatim and the
        // operator UI can retry with `?refresh=1`. On the
        // common case (index populated, bytes present) this
        // reduces browse cost from O(library) tag-walk per tile
        // to one file read + one memory hash lookup at the
        // endpoint layer.
        if let Some(index) = &self.resolve_index {
            // First: exact-size lookup preserves the historical
            // O(1) hot path for callers whose exact key was
            // previously populated.
            if let Some(hit) = index.get(scheme, value, size).await {
                let hash = hit.hash;
                // Verify the mapping is still live before
                // returning it. The index survives asset-cache
                // eviction (LRU quota, operator delete, disk
                // hygiene): without this check, a stale
                // mapping short-circuits every subsequent
                // resolve to a hash whose bytes are gone, and
                // the operator UI paints a permanent broken-
                // image icon until `?refresh=1` is issued out
                // of band. Bytes-present verification here
                // makes the drift self-healing: the stale
                // entry is dropped, the cascade re-runs, and
                // the next request produces a fresh winner.
                // Stale-rules check comes first: bytes being
                // present says nothing about whether today's
                // cascade would still choose them. A row written
                // under superseded selection rules is dropped
                // (with its bytes) so the cascade re-runs once.
                if hit.version != CASCADE_LOGIC_VERSION {
                    tracing::info!(
                        scheme = %scheme,
                        value = %value,
                        size = %size,
                        content_hash = %hash,
                        row_version = hit.version,
                        current_version = CASCADE_LOGIC_VERSION,
                        "artwork cascade: resolve-index entry predates the \
                         current selection rules; evicting and re-resolving"
                    );
                    self.forget_one_size(scheme, value, size).await;
                } else if self
                    .index_entry_bytes_present(scheme, value, size, &hash)
                    .await
                {
                    return CascadeOutcome::Resolved {
                        content_hash: hash,
                        provider_id: Some(PROVENANCE_RESOLVE_INDEX.to_string()),
                    };
                }
                // Fall through to any-size fallback and
                // ultimately full cascade — the stale entry
                // has already been evicted inside
                // `index_entry_bytes_present`.
            }
            // Then: any-size fallback. If the resolve_index has
            // any content hash for (scheme, value) at ANY size,
            // serve those bytes. The bytes are the same content;
            // the browser downscales fine. Rationale: the
            // operator's browse view frequently paints one size
            // (small tile) while a prior action resolved bytes
            // at a different size (medium detail view, or an
            // enrichment stored `original`). Exact-only lookup
            // would re-storm the provider cascade every paint;
            // the fallback returns "art we have" instead of
            // "not found, hit the network again."
            if let Some((hit, served_size)) =
                index.get_any_size(scheme, value, size).await
            {
                let hash = hit.hash;
                if hit.version != CASCADE_LOGIC_VERSION {
                    tracing::info!(
                        scheme = %scheme,
                        value = %value,
                        size = %served_size,
                        content_hash = %hash,
                        row_version = hit.version,
                        current_version = CASCADE_LOGIC_VERSION,
                        "artwork cascade: any-size resolve-index entry \
                         predates the current selection rules; evicting \
                         and re-resolving"
                    );
                    self.forget_one_size(scheme, value, &served_size).await;
                    // Fall through to the full cascade below.
                } else if self
                    // Same drift check as the exact-size arm: the
                    // any-size mapping may point at a hash whose
                    // bytes the LRU has since dropped.
                    .index_entry_bytes_present(
                        scheme,
                        value,
                        &served_size,
                        &hash,
                    )
                    .await
                {
                    let provenance = if served_size == size {
                        PROVENANCE_RESOLVE_INDEX.to_string()
                    } else {
                        format!(
                            "{PROVENANCE_RESOLVE_INDEX}:size_fallback:{served_size}"
                        )
                    };
                    return CascadeOutcome::Resolved {
                        content_hash: hash,
                        provider_id: Some(provenance),
                    };
                }
            }
        }

        // Coalescer + fetcher closure. Captures the dispatch
        // machinery, the admission gate, and the negative-
        // cache write side.
        let dispatcher = Arc::clone(&self.dispatcher);
        let admission = Arc::clone(&self.admission);
        let negative_cache_for_fetcher = Arc::clone(&self.negative_cache);
        let negative_key_for_fetcher = negative_key.clone();
        let scheme_owned = scheme.to_string();
        let value_owned = value.to_string();
        let size_owned = size.to_string();
        let principal_clone = principal.clone();

        let outcome = self
            .coalescer
            .resolve_or_coalesce(scheme, value, size, move || async move {
                run_cascade(
                    dispatcher,
                    admission,
                    principal_clone,
                    scheme_owned,
                    value_owned,
                    size_owned,
                    negative_cache_for_fetcher,
                    negative_key_for_fetcher,
                )
                .await
            })
            .await;

        match outcome {
            Ok((content_hash, provider_id)) => {
                // Persistent positive-index population. On the
                // next resolve for the same target the fast
                // path above short-circuits to O(1) without
                // touching the plugin. Enables the `?refresh=1`
                // gesture to reach the asset-cache bytes too.
                self.remember_positive(scheme, value, size, &content_hash)
                    .await;
                CascadeOutcome::Resolved {
                    content_hash,
                    provider_id,
                }
            }
            Err(CoalesceError::FetcherError { reason, .. }) => {
                let outcome = classify_fetcher_error(reason);
                self.memoise_transient_if_needed(&outcome, scheme, value, size);
                outcome
            }
            Err(CoalesceError::WaitDeadlineElapsed {
                scheme: sch,
                value: val,
                size: sz,
                waited_ms,
            }) => {
                let outcome = CascadeOutcome::CoalescerDeadline {
                    detail: format!(
                        "artwork resolve coalescer wait deadline elapsed \
                         for (scheme={sch}, value={val}, size={sz}) after \
                         {waited_ms} ms — upstream provider is slow; retry"
                    ),
                };
                self.memoise_transient_if_needed(&outcome, scheme, value, size);
                outcome
            }
        }
    }

    /// Transient outcomes (upstream unavailable / admission
    /// deadline / coalescer wait deadline / dispatch error) do
    /// not populate `resolve_index` and previously wrote nothing
    /// to `NegativeCache` either — so a browse paint that
    /// re-visits the same key seconds after a transient failure
    /// re-dispatched the full provider cascade every time. Live
    /// rig evidence pre-fix: a 50-tile artist-browse view whose
    /// CoverArtArchive returns HTTP 503 ran the full cascade on
    /// every visit, forever.
    ///
    /// Fix: on any transient outcome, write a short-TTL
    /// NegativeCache entry (`TRANSIENT_MEMO_TTL`, 5 min) so
    /// a re-visit inside the window returns 404-fast (with the
    /// transient's status carried on the entry so the operator
    /// UI can distinguish "no art anywhere" from "temporarily
    /// unavailable"). Structured-NotFound writes remain the
    /// existing day-scale entries via `run_cascade` — the two
    /// TTLs coexist under the same NegativeCache.
    fn memoise_transient_if_needed(
        &self,
        outcome: &CascadeOutcome,
        scheme: &str,
        value: &str,
        size: &str,
    ) {
        let (status, detail, provider_id) = match outcome {
            CascadeOutcome::Transient { detail } => {
                ("unavailable", detail.clone(), None)
            }
            CascadeOutcome::AdmissionDeadline { detail } => {
                ("admission_deadline", detail.clone(), None)
            }
            CascadeOutcome::CoalescerDeadline { detail } => {
                ("coalescer_deadline", detail.clone(), None)
            }
            // Resolved + NotFound + BadRequest do not populate
            // the transient memo. NotFound is populated by
            // run_cascade with the long-TTL default (24 h).
            _ => return,
        };
        let key = (scheme.to_string(), value.to_string(), size.to_string());
        self.negative_cache.put_with_ttl(
            key,
            status,
            detail,
            provider_id,
            TRANSIENT_MEMO_TTL,
        );
    }
}

/// TTL for transient-outcome negative memo entries — 5 minutes.
/// Long enough that a burst of re-visits inside an operator's
/// browse session does not re-storm the failing upstream;
/// short enough that once the upstream recovers, the next
/// browse a few minutes later re-probes rather than staying
/// stuck on a stale "unavailable" for hours. Distinct from the
/// day-scale default TTL used for structured NotFound.
const TRANSIENT_MEMO_TTL: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);

// -----------------------------------------------------------
// Internal helpers
// -----------------------------------------------------------

/// Take an admission permit for an ONLINE provider dispatch.
///
/// Admission exists to bound concurrent load on third-party
/// providers — rate-limited, latency-unbounded, occasionally
/// unreachable. It does not exist to bound reading a file.
///
/// The permit used to wrap the whole cascade, local tier
/// included, so a sidecar read queued behind resolves that were
/// asleep on MusicBrainz. On a browse of an all-local library
/// that is pure self-inflicted latency: artwork already on disk
/// waits for network work it never needed, and when the bucket
/// saturates the local tile is refused outright despite its
/// bytes being one `stat` away.
///
/// Held across the online dispatch and dropped the moment it
/// returns, so the bucket measures what it is meant to measure.
async fn admit_online(
    admission: &Admission,
    scheme: &str,
) -> Result<crate::artwork_admission::AdmissionGuard, String> {
    match admission.admit(scheme).await {
        Ok(guard) => Ok(guard),
        Err(AdmissionError::DeadlineElapsed {
            scheme,
            bucket,
            waited_ms,
        }) => Err(format!(
            "artwork resolve admission deadline elapsed on {bucket} \
             bucket for scheme={scheme} after {waited_ms} ms"
        )),
        Err(AdmissionError::ClosedRuntime) => {
            Err("artwork resolve admission runtime closed".into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_cascade(
    dispatcher: Arc<dyn Dispatcher>,
    admission: Arc<Admission>,
    principal: Principal,
    scheme_owned: String,
    value_owned: String,
    size_owned: String,
    negative_cache: Arc<NegativeCache>,
    negative_key: (String, String, String),
) -> Result<(String, Option<String>), String> {
    // Admission gates ONLINE dispatch only — see `admit_online`.
    // The local tier below runs ungated.
    use base64::Engine;

    // Scheme-branch: artist-mbid still routes DIRECTLY to the
    // online plugin — the local tier has no MBID→file index and
    // MPD queries key on tag values, not MBIDs.
    //
    // artist-name USED to also route direct to online; that
    // short-circuit is now GONE. The local artwork plugin now
    // accepts `scheme=artist-name` (walks up the artist's
    // directory chain looking for `artist.jpg` / `folder.jpg` /
    // `cover.jpg`) so the operator's on-disk portrait convention
    // resolves without a network round-trip. `artist-name` falls
    // through to the normal Tier 1 (local) → Tier 2 (online)
    // path below.
    if scheme_owned == "artist-mbid" {
        let artist_payload = json!({
            "v": 1,
            "target": {"scheme": &scheme_owned, "value": &value_owned},
            "size": &size_owned,
        });
        let artist_payload_b64 = base64::engine::general_purpose::STANDARD
            .encode(artist_payload.to_string().as_bytes());
        let artist_envelope = json!({
            "shelf": "artwork.providers",
            "request_type": "artwork.resolve_artist_online",
            "payload_b64": &artist_payload_b64,
        });
        let op_id = WireOpId::new("request")
            .map_err(|e| format!("wire-op id construction failed: {e}"))?;
        let permit = admit_online(&admission, &scheme_owned).await?;
        let artist_dispatch = dispatcher
            .dispatch(&op_id, artist_envelope, &principal)
            .await;
        drop(permit);
        let artist_env = match artist_dispatch {
            Ok(e) => e,
            Err(e) => {
                return Err(format!(
                    "artwork resolve dispatch failed \
                     (artwork.resolve_artist_online): {e}"
                ));
            }
        };
        let artist_response =
            peel_plugin_response(&artist_env).ok_or_else(|| {
                format!(
                    "artwork resolve: malformed dispatch envelope (no \
                     payload_b64 or non-JSON inner payload) for \
                     artwork.resolve_artist_online: {artist_env}"
                )
            })?;
        let artist_provider_id = extract_provider_id(&artist_response);
        if let Some(hash) = extract_content_hash(&artist_response) {
            return Ok((hash, artist_provider_id));
        }
        let artist_status = extract_status(&artist_response);
        let artist_detail = extract_detail(&artist_response);
        if artist_status == "not_found" {
            negative_cache.put(
                negative_key.clone(),
                artist_status.clone(),
                artist_detail.clone(),
                artist_provider_id.clone(),
            );
            return Err(format!(
                "artwork not resolved: status={artist_status} \
                 detail={artist_detail}"
            ));
        } else if artist_status == "unavailable" {
            return Err(format!(
                "artwork upstream unavailable: status={artist_status} \
                 detail={artist_detail}"
            ));
        } else {
            return Err(format!(
                "artwork not resolved: status={artist_status} \
                 detail={artist_detail}"
            ));
        }
    }

    // Tier 1: local dispatch with the caller's original target.
    let local_payload = json!({
        "v": 1,
        "target": {"scheme": &scheme_owned, "value": &value_owned},
        "size": &size_owned,
    });
    let local_payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(local_payload.to_string().as_bytes());
    let local_envelope = json!({
        "shelf": "artwork.providers",
        "request_type": "artwork.resolve",
        "payload_b64": &local_payload_b64,
    });
    let op_id = WireOpId::new("request")
        .map_err(|e| format!("wire-op id construction failed: {e}"))?;
    let local_dispatch = dispatcher
        .dispatch(&op_id, local_envelope, &principal)
        .await;
    let local_env = match local_dispatch {
        Ok(e) => e,
        Err(e) => {
            return Err(format!(
                "artwork resolve dispatch failed (artwork.resolve): {e}"
            ));
        }
    };
    let local_response = peel_plugin_response(&local_env).ok_or_else(|| {
        format!(
            "artwork resolve: malformed dispatch envelope (no \
             payload_b64 or non-JSON inner payload) for \
             artwork.resolve: {local_env}"
        )
    })?;
    let local_provider_id = extract_provider_id(&local_response);
    if let Some(hash) = extract_content_hash(&local_response) {
        return Ok((hash, local_provider_id));
    }
    let local_status = extract_status(&local_response);
    let local_detail = extract_detail(&local_response);
    if local_status == "bad_request" {
        // Caller-input error at the local tier; do not cache
        // negatively — the caller can fix and retry.
        return Err(format!(
            "artwork not resolved: status={local_status} \
             detail={local_detail}"
        ));
    }

    // Local NotFound. Two possible Tier 2 targets:
    //
    // - Album-scoped schemes (mpd-path, mpd-album, mpd-directory)
    //   synthesise an mpd-album target from the local response's
    //   identity (artist, album) tags and dispatch to the album-
    //   scoped online verb `artwork.resolve_online`.
    //
    // - Artist-name scheme dispatches DIRECTLY to the artist-
    //   scoped online verb `artwork.resolve_artist_online` with
    //   the original artist-name value. No identity synthesis
    //   needed — the caller already carries the artist name and
    //   the online plugin's artist verb is byte-cached.
    let (online_target_scheme, online_target_value, online_request_type) =
        if scheme_owned == "artist-name" {
            (
                "artist-name".to_string(),
                value_owned.clone(),
                "artwork.resolve_artist_online",
            )
        } else {
            let Some((artist, album)) = extract_identity(&local_response)
            else {
                // No identity available — skip online, memoise
                // negatively so a browse burst re-visiting this
                // target does not re-invoke the local tag-read.
                negative_cache.put(
                    negative_key.clone(),
                    local_status.clone(),
                    local_detail.clone(),
                    local_provider_id.clone(),
                );
                return Err(format!(
                    "artwork not resolved: status={local_status} \
                     detail={local_detail}"
                ));
            };
            (
                "mpd-album".to_string(),
                format!("{artist}|{album}"),
                "artwork.resolve_online",
            )
        };

    let online_payload = json!({
        "v": 1,
        "target": {"scheme": online_target_scheme, "value": &online_target_value},
        "size": &size_owned,
    });
    let online_payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(online_payload.to_string().as_bytes());
    let online_envelope = json!({
        "shelf": "artwork.providers",
        "request_type": online_request_type,
        "payload_b64": &online_payload_b64,
    });
    let op_id = WireOpId::new("request")
        .map_err(|e| format!("wire-op id construction failed: {e}"))?;
    let permit = admit_online(&admission, &scheme_owned).await?;
    let online_dispatch = dispatcher
        .dispatch(&op_id, online_envelope, &principal)
        .await;
    drop(permit);
    let online_env = match online_dispatch {
        Ok(e) => e,
        Err(e) => {
            return Err(format!(
                "artwork resolve dispatch failed \
                 (artwork.resolve_online): {e}"
            ));
        }
    };
    let online_response =
        peel_plugin_response(&online_env).ok_or_else(|| {
            format!(
                "artwork resolve: malformed dispatch envelope (no \
                 payload_b64 or non-JSON inner payload) for \
                 artwork.resolve_online: {online_env}"
            )
        })?;
    let online_provider_id = extract_provider_id(&online_response);
    if let Some(hash) = extract_content_hash(&online_response) {
        return Ok((hash, online_provider_id));
    }
    let online_status = extract_status(&online_response);
    let online_detail = extract_detail(&online_response);
    // Only a genuine, provider-confirmed "not_found" is
    // eligible for the negative memo. `unavailable`,
    // `bad_request`, or anything else the plugin might emit
    // is transient / caller-fixable and must not be cached —
    // otherwise a transient upstream failure would masquerade
    // as definitive absence until the memo expires.
    if online_status == "not_found" {
        negative_cache.put(
            negative_key.clone(),
            online_status.clone(),
            online_detail.clone(),
            online_provider_id.clone(),
        );
        Err(format!(
            "artwork not resolved: status={online_status} \
             detail={online_detail}"
        ))
    } else if online_status == "unavailable" {
        // Distinct prefix so classify_fetcher_error routes this
        // to CascadeOutcome::Transient (not NotFound).
        Err(format!(
            "artwork upstream unavailable: status={online_status} \
             detail={online_detail}"
        ))
    } else {
        Err(format!(
            "artwork not resolved: status={online_status} \
             detail={online_detail}"
        ))
    }
}

fn classify_fetcher_error(reason: String) -> CascadeOutcome {
    if reason.starts_with("artwork resolve admission deadline") {
        CascadeOutcome::AdmissionDeadline { detail: reason }
    } else if reason.starts_with("artwork upstream unavailable:") {
        // Upstream (online tier providers) was reachable-but-
        // transient. Distinct from NotFound so the caller can
        // retry and the endpoint can surface a 502 rather than
        // a 404 (a "we could not reach anyone" outcome, not "we
        // reached everyone and confirmed absence").
        CascadeOutcome::Transient { detail: reason }
    } else if reason.starts_with("artwork not resolved:") {
        CascadeOutcome::NotFound {
            provider_id: None,
            detail: reason,
        }
    } else {
        // dispatch / envelope / wire-op failures land here.
        CascadeOutcome::Transient { detail: reason }
    }
}

// -----------------------------------------------------------
// Peel + extract helpers (canonical for both endpoints)
// -----------------------------------------------------------

fn peel_plugin_response(envelope: &Value) -> Option<Value> {
    use base64::Engine;
    let payload_b64 = envelope.as_object()?.get("payload_b64")?.as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn extract_content_hash(plugin_response: &Value) -> Option<String> {
    let raw = plugin_response
        .as_object()
        .and_then(|m| m.get("content_hash"))
        .and_then(Value::as_str)?;
    if is_known_placeholder_hash(raw) {
        // A provider returned a known-placeholder hash — the
        // most common shape is the empty-string MD5
        // (`d41d8cd98f00b204e9800998ecf8427e`), which Deezer
        // uses as its "no picture available" sentinel and
        // which some artwork providers (volumio_meta) forward
        // as if it were real bytes. Refusing at the framework
        // layer means the cascade falls through to the next
        // tier instead of caching a placeholder under an
        // operator-facing key — the operator's UI sees a
        // proper NotFound + placeholder floor OR the next
        // tier's genuine content, never a blank Deezer thumb
        // masquerading as an artist portrait.
        //
        // Provider-agnostic: any tier / any provider returning
        // a placeholder hash is refused. Adding new placeholder
        // hashes is O(1) here without touching every provider.
        return None;
    }
    Some(raw.to_string())
}

/// Known-placeholder content hashes rejected at the cascade
/// layer. Extend this list when a new upstream is observed to
/// forward a "no content available" sentinel as if it were
/// real bytes.
///
/// The empty-string MD5 sentinel (`d41d8cd98f00b204e9800998ecf8427e`)
/// appears in Deezer artwork URLs as a "no picture" marker and
/// has been observed being accepted by the `volumio_meta`
/// provider and forwarded as a Resolved outcome. Rejecting it
/// here keeps the cascade honest regardless of which provider
/// forwards it.
///
/// Comparison is case-insensitive to defend against providers
/// that upper-case the hex.
fn is_known_placeholder_hash(hash: &str) -> bool {
    const KNOWN_PLACEHOLDERS: &[&str] = &[
        // Empty-string MD5 (Deezer "no picture" sentinel).
        "d41d8cd98f00b204e9800998ecf8427e",
    ];
    KNOWN_PLACEHOLDERS
        .iter()
        .any(|p| p.eq_ignore_ascii_case(hash))
}

fn extract_status(plugin_response: &Value) -> String {
    plugin_response
        .as_object()
        .and_then(|m| m.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn extract_detail(plugin_response: &Value) -> String {
    plugin_response
        .as_object()
        .and_then(|m| m.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn extract_provider_id(plugin_response: &Value) -> Option<String> {
    plugin_response
        .as_object()
        .and_then(|m| m.get("provider_id"))
        .and_then(Value::as_str)
        .map(String::from)
}

fn extract_identity(plugin_response: &Value) -> Option<(String, String)> {
    let identity = plugin_response.as_object()?.get("identity")?.as_object()?;
    let artist = identity.get("artist")?.as_str()?.trim();
    let album = identity.get("album")?.as_str()?.trim();
    if artist.is_empty() || album.is_empty() {
        return None;
    }
    Some((artist.to_string(), album.to_string()))
}

#[cfg(test)]
mod resolved_notifier_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<(String, String, String, String)>>,
    }
    impl ArtworkResolvedNotifier for Recorder {
        fn artwork_resolved(
            &self,
            scheme: &str,
            value: &str,
            size: &str,
            content_hash: &str,
        ) {
            self.seen.lock().unwrap().push((
                scheme.to_string(),
                value.to_string(),
                size.to_string(),
                content_hash.to_string(),
            ));
        }
    }

    struct NullDispatcher;
    #[async_trait::async_trait]
    impl Dispatcher for NullDispatcher {
        async fn dispatch(
            &self,
            _op_id: &WireOpId,
            _payload: Value,
            _principal: &Principal,
        ) -> Result<Value, evo_runtime_http::dispatcher::DispatchError>
        {
            unreachable!("these tests never reach dispatch")
        }
    }

    const HASH: &str =
        "3333333333333333333333333333333333333333333333333333333333333333";

    /// A landed resolve tells whoever is listening, with the key
    /// they painted and the hash that now serves it.
    ///
    /// Artwork resolution is asynchronous with respect to the
    /// paint that asked for it: by the time bytes exist the tile
    /// has drawn its glyph. Without this signal a surface already
    /// on screen has no way to learn the picture arrived, and the
    /// operator's only recourse is to navigate away and back.
    #[tokio::test]
    async fn a_landed_resolve_is_announced() {
        let rec = Arc::new(Recorder::default());
        let dir = tempfile::TempDir::new().unwrap();
        let index =
            Arc::new(crate::artwork_resolve_index::ArtworkResolveIndex::new(
                dir.path().to_path_buf(),
            ));
        let c =
            ArtworkCascade::new(Arc::new(NullDispatcher), None, Some(index))
                .with_resolved_notifier(rec.clone());
        c.remember_positive("artist-name", "Someone", "large", HASH)
            .await;
        let seen = rec.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "exactly one announcement per landing");
        assert_eq!(
            seen[0],
            (
                "artist-name".to_string(),
                "Someone".to_string(),
                "large".to_string(),
                HASH.to_string()
            )
        );
    }

    /// The announcement must survive an index write failure. The
    /// bytes exist either way, so a repaint is still correct — a
    /// failed put costs a slower second resolve, not a wrong
    /// picture.
    #[tokio::test]
    async fn an_announcement_does_not_depend_on_the_index_write() {
        let rec = Arc::new(Recorder::default());
        // No index wired at all: the put path is skipped entirely.
        let c = ArtworkCascade::new(Arc::new(NullDispatcher), None, None)
            .with_resolved_notifier(rec.clone());
        c.remember_positive("mpd-path", "a/b.flac", "small", HASH)
            .await;
        assert_eq!(rec.seen.lock().unwrap().len(), 1);
    }

    /// Silence is a valid configuration: a cascade with no
    /// notifier must not panic or change behaviour.
    #[tokio::test]
    async fn a_cascade_without_a_notifier_stays_silent() {
        let c = ArtworkCascade::new(Arc::new(NullDispatcher), None, None);
        c.remember_positive("artist-name", "Someone", "large", HASH)
            .await;
    }
}
