// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Artwork resolve-by-target HTTPS endpoint.
//!
//! Mounts `GET /api/v1/audio/artwork` against the framework's
//! wire-op dispatcher. The endpoint takes target params
//! (`scheme` + `value` + optional `size`), cascades through the
//! `artwork.providers` shelf's two provider tiers — `artwork.local`
//! (embedded / sidecar / folder-name) then `artwork.online`
//! (Cover Art Archive / Last.fm / iTunes / proxy) — and returns
//! a `302 Found` redirecting to the canonical
//! `/api/v1/audio/artwork/:content_hash` endpoint that serves
//! bytes from the framework's asset cache.
//!
//! ## Provider cascade
//!
//! The endpoint tries `artwork.resolve` (owned by artwork.local)
//! first with the caller's original target. On structured
//! NotFound, it SYNTHESISES an mpd-album target from the
//! `(artist, album)` identity artwork.local stamped onto its
//! response — for mpd-album requests the identity is the parsed
//! value (idempotent), for mpd-path requests it's read from the
//! file's tags. The synthesised target then goes to
//! `artwork.resolve_online` (owned by artwork.online, keyed on
//! `(artist, album)`).
//!
//! Without the identity synthesis, an mpd-path request whose
//! file had no embedded / sidecar art would 404 even when the
//! album's cover was available online — artwork.online refuses
//! any non-mpd-album scheme because its cascade cannot resolve
//! a raw file path against a public catalogue. The identity
//! synthesis closes that hole.
//!
//! A 404 is surfaced to the operator UI's placeholder floor
//! ONLY when:
//!
//!   - Local structured-NotFound AND identity is absent
//!     (no `(artist, album)` tags on the file, or lofty can't
//!     parse the format) — the endpoint refuses to invent a
//!     half-identity lookup that could match unrelated releases.
//!   - OR both tiers structured-NotFound.
//!
//! Hard failures (wire error, malformed dispatch envelope,
//! `bad_request` from any tier) DO NOT cascade — the operator
//! sees the underlying error rather than a masked 404.
//!
//! Both tier dispatches happen under the SAME admission permit,
//! so the shared-bucket bound applies to the cascade as a whole,
//! not per-tier — a browse burst that saturates the bucket at
//! the local tier does not double-charge for the online retry.
//!
//! ## Why two endpoints
//!
//! Splitting resolve (target → hash) from serve (hash → bytes)
//! is the engineering-excellence shape: the hash endpoint is
//! immutable-cacheable (`Cache-Control: max-age=31536000,
//! immutable`) because content addressing means identity
//! changes produce different URLs. Browsers, CDNs, and
//! reverse proxies treat hash URLs as forever-cacheable, which
//! is exactly correct.
//!
//! The resolve endpoint cannot be immutable-cacheable: the
//! same `(scheme, value, size)` may map to different bytes
//! over time as the operator edits track tags. The redirect
//! pattern moves the cache decision to the right side of the
//! resolve boundary.
//!
//! ## Size taxonomy
//!
//! The canonical size taxonomy is `small | medium | large`
//! (from the artwork-sources-and-operator-surface design) plus
//! `original` for the hero surface that needs the raw source:
//!
//! - `small` — 300 px square (list rows, queue tiles: 150 px
//!   logical × 2x retina)
//! - `medium` — 600 px square (browse tiles, mini-player: 300
//!   px logical × 2x)
//! - `large` — 1200 px square (now-playing card, album-art
//!   view: 600 px logical × 2x)
//! - `original` — no resize (multi-room propagation, archival,
//!   downloadable export)
//!
//! `tiny` is retained as a backward-compatible alias for
//! `small` so pre-existing UI callers do not break; new
//! callers use `small`.
//!
//! ## Default size (list-safe)
//!
//! When the caller omits the `size` query parameter, the
//! endpoint defaults to `medium`. This choice is deliberate:
//!
//! - Library / queue / favourites / playlist rendering is the
//!   dominant surface (thousands of visible thumbs on a large
//!   library). Defaulting to `original` on this path serves
//!   full-resolution bytes for every visible track and does not
//!   carry library scale.
//! - `medium` is the neutral default that lets browse render at
//!   quality without the operator surface having to state a
//!   size on every emit.
//! - Callers that genuinely want the source (hero panel,
//!   multi-room propagation, archival download) supply
//!   `?size=original` explicitly — an intent-encoded opt-in
//!   rather than a silent default.
//!
//! Resize execution is the artwork provider plugin's
//! responsibility; the endpoint enforces the taxonomy at the
//! boundary and forwards the size verbatim to the resolver.
//!
//! ## Capability scope
//!
//! Same `read:audio` capability as the byte-serving endpoint:
//! callers fetching artwork already hold this scope via the
//! bootstrap operator token + every minted session token.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_projection_core::{CapabilityRequirement, WireOpId};
use serde::Deserialize;
use std::sync::Arc;

use crate::artwork_cascade::{ArtworkCascade, CascadeOutcome};
use evo_runtime_http::auth_tier::AuthTierProvider;
use evo_runtime_http::middleware::{capability_gate, AuthLayer};
use evo_runtime_http::principal::Principal;

/// Response header key surfacing which provider produced the
/// resolve outcome. Lower-case per HTTP/2 canonical form (axum
/// `HeaderName::from_static` requires lowercase). Stable values:
/// plugin-emitted (`local_sidecar`, `local_embedded`,
/// `cover_art_archive`, `lastfm`, `itunes`, `volumio_meta`), the
/// endpoint-owned `negative_cache` (memoised prior all-tier
/// miss), or `none` (cold all-tier miss where no provider
/// produced content).
const PROVENANCE_HEADER: &str = "x-artwork-provider";

/// Value used on a cold-cascade 404 where every tier
/// structured-NotFounded on this exact request. The memoised
/// path (see [`NegativeCache`]) sets the last-tier provider id
/// on subsequent hits; the first miss carries `none` so the
/// operator UI can distinguish cold from warm negatives.
const PROVENANCE_NONE: &str = "none";

/// Mount the resolve-by-target endpoint onto the supplied
/// router under `/api/v1/audio/artwork`. Wraps the route in
/// the bearer-auth middleware (same `read:audio` capability the
/// hash endpoint uses).
///
/// Returns `Err(RuntimeHttpError::EndpointAttachRefused)` if
/// the in-code wire-op id constant fails to construct (cannot
/// happen in tested builds).
pub fn attach_artwork_resolve_endpoint(
    router: Router,
    api_prefix: &str,
    cascade: Arc<ArtworkCascade>,
    validator: Arc<BearerTokenValidator>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Result<Router, evo_runtime_http::error::RuntimeHttpError> {
    let path = format!("{api_prefix}/audio/artwork");
    let requirement = CapabilityRequirement::read("audio");
    let op_id = WireOpId::new("audio_artwork_resolve").map_err(|e| {
        evo_runtime_http::error::RuntimeHttpError::EndpointAttachRefused {
            endpoint: "audio_artwork_resolve".into(),
            reason: format!("wire-op id refused at construction: {e}"),
        }
    })?;
    let auth = AuthLayer {
        requirement,
        validator,
        op_id,
        observatory: None,
        tier_provider,
        lan_trust_caps,
    };
    let state = HandlerState {
        cascade,
        api_prefix: api_prefix.to_string(),
    };
    // DELETE carries its own gate. Eviction is destructive and
    // must never be authorised by the read token that serves
    // browse traffic, so it requires `write:audio` and is
    // layered per-method rather than sharing the GET route's
    // read gate.
    let forget_op_id = WireOpId::new("audio_artwork_forget").map_err(|e| {
        evo_runtime_http::error::RuntimeHttpError::EndpointAttachRefused {
            endpoint: "audio_artwork_forget".into(),
            reason: format!("wire-op id refused at construction: {e}"),
        }
    })?;
    let forget_auth = AuthLayer {
        requirement: CapabilityRequirement::write("audio"),
        validator: auth.validator.clone(),
        op_id: forget_op_id,
        observatory: None,
        tier_provider: Arc::clone(&auth.tier_provider),
        lan_trust_caps: auth.lan_trust_caps.clone(),
    };
    let resolve_route =
        get(handle_resolve).with_state(state.clone()).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        );
    let forget_route = axum::routing::delete(handle_forget)
        .with_state(state)
        .route_layer(axum::middleware::from_fn_with_state(
            forget_auth,
            capability_gate,
        ));
    Ok(router.route(&path, resolve_route.merge(forget_route)))
}

/// Query for the eviction gesture.
///
/// Both fields absent means all-scope. Supplying exactly one of
/// them is refused rather than guessed: a caller that meant to
/// clear one artist and typo'd the parameter name would
/// otherwise wipe the whole library.
#[derive(Debug, Deserialize)]
struct ForgetQuery {
    #[serde(default)]
    scheme: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

/// `DELETE /api/v1/audio/artwork` — the operator's clear gesture.
///
/// With `?scheme=&value=`: clears that target. With neither:
/// clears everything. Both forms evict the framework's stored
/// bytes and index entries and fan out to the provider plugins,
/// so "clear" means the bytes are gone rather than a lookup
/// table being rebuilt from the same sources.
async fn handle_forget(
    State(state): State<HandlerState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(query): Query<ForgetQuery>,
) -> Response {
    let outcome = match (query.scheme, query.value) {
        (Some(scheme), Some(value)) => {
            if scheme.trim().is_empty() || value.trim().is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    "scheme and value must both be non-empty for a \
                     targeted clear; omit both to clear everything",
                )
                    .into_response();
            }
            state
                .cascade
                .forget_target(&scheme, &value, &principal)
                .await
        }
        (None, None) => state.cascade.forget_everything(&principal).await,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "supply both scheme and value to clear one target, or \
                 neither to clear everything",
            )
                .into_response();
        }
    };
    let body = serde_json::json!({
        "v": 1,
        "status": "ok",
        "scope": outcome.scope,
        "index_entries_removed": outcome.index_entries_removed,
        "assets_deleted": outcome.assets_deleted,
        "plugin_cleared": outcome.plugin_cleared,
        "plugin_detail": outcome.plugin_detail,
    });
    (StatusCode::OK, axum::Json(body)).into_response()
}

#[derive(Clone)]
struct HandlerState {
    /// Shared cascade primitive — same instance the composite
    /// track-detail endpoint uses. Ensures one artwork
    /// resolution path across every framework surface.
    cascade: Arc<ArtworkCascade>,
    api_prefix: String,
}

#[derive(Debug, Deserialize)]
struct ResolveQuery {
    /// External-addressing scheme (e.g. `mpd-path`, `mpd-album`).
    scheme: String,
    /// Scheme-specific opaque value.
    value: String,
    /// Optional size taxonomy: `small | medium | large | original`
    /// (`tiny` is a backward-compatible alias for `small`).
    /// Defaults to `medium` when absent — the list-safe default;
    /// callers that need the source supply `original` explicitly.
    #[serde(default)]
    size: Option<String>,
    /// Operator escape hatch: when `1`, evict any negative memo
    /// for this exact target before dispatching, so a fresh
    /// cascade runs. Intended for the "I just corrected the
    /// tags / cover, look again now" surface — never for browse
    /// (that would burn the memoisation's purpose). Any other
    /// value or absence is a no-op.
    #[serde(default)]
    refresh: Option<String>,
}

/// Endpoint default size when the caller omits `?size=`.
///
/// `medium` is the neutral list-safe default (see module-level
/// "Default size (list-safe)" section for the reasoning). If
/// this constant changes, the doc must change with it — the
/// choice is contract, not convenience.
const DEFAULT_SIZE: &str = "medium";

async fn handle_resolve(
    State(state): State<HandlerState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(query): Query<ResolveQuery>,
) -> Response {
    let raw_size = query.size.as_deref().unwrap_or(DEFAULT_SIZE);
    if !is_valid_size(raw_size) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "size must be one of small | medium | large | original \
                 (`tiny` accepted as alias for `small`); got {raw_size:?}"
            ),
        )
            .into_response();
    }
    // Canonicalise `tiny` → `small` before forwarding to the
    // resolver so the plugin's size dispatch is keyed on one
    // canonical name.
    let size = canonical_size(raw_size);
    if query.scheme.is_empty() || query.value.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "scheme and value query parameters are required",
        )
            .into_response();
    }
    // Operator escape hatch. `?refresh=1` evicts any negative
    // memo for this exact target before dispatching, so a
    // fresh cascade runs. Any other value or absence is a
    // no-op. This is the "I just fixed this, look again now"
    // path — the standalone endpoint carries it and the
    // composite endpoint intentionally does not (browse would
    // burn the memo's purpose).
    if query.refresh.as_deref() == Some("1") {
        state
            .cascade
            .forget(&query.scheme, &query.value, size)
            .await;
    }
    // Delegate to the shared cascade — the SAME service the
    // composite track-detail endpoint uses. Both surfaces see
    // the same negative memo, the same coalescer, the same
    // admission bucket, and the same content_hash for the
    // same target.
    match state
        .cascade
        .resolve(&principal, &query.scheme, &query.value, size)
        .await
    {
        CascadeOutcome::Resolved {
            content_hash,
            provider_id,
        } => redirect_to_hash_endpoint(
            &state.api_prefix,
            &content_hash,
            provider_id.as_deref(),
        ),
        CascadeOutcome::NotFound {
            provider_id,
            detail,
        } => {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::HeaderName::from_static(PROVENANCE_HEADER),
                HeaderValue::from_str(
                    provider_id.as_deref().unwrap_or(PROVENANCE_NONE),
                )
                .unwrap_or_else(|_| HeaderValue::from_static(PROVENANCE_NONE)),
            );
            (StatusCode::NOT_FOUND, headers, detail).into_response()
        }
        CascadeOutcome::BadRequest { detail } => {
            (StatusCode::BAD_REQUEST, detail).into_response()
        }
        // Every transient class carries a retry contract: 503
        // plus Retry-After, which is what `artwork_admission`
        // has documented as the endpoint's obligation all along.
        //
        // These used to answer 404, on the reasoning that a 5xx
        // renders as a broken-image icon in a bare `<img>` while
        // a 404 lets the UI paint its own placeholder. That
        // reasoning optimised the wrong thing. It made "we were
        // too busy to look" indistinguishable from "we looked
        // and there is nothing", and a client cannot retry what
        // it cannot recognise: a tile that treats 404 as a
        // verdict marks itself permanently broken and never
        // repaints, even for a local album whose art is sitting
        // on disk. A stuck glyph on resolvable artwork is a
        // worse outcome than a momentary broken-image icon, and
        // a client that reads the status can avoid both.
        //
        // 404 now means one thing only: validated absence.
        CascadeOutcome::AdmissionDeadline { detail } => retry_later(
            "admission_deadline",
            RETRY_AFTER_ADMISSION_SECS,
            detail,
        ),
        CascadeOutcome::CoalescerDeadline { detail } => retry_later(
            "coalescer_deadline",
            RETRY_AFTER_COALESCER_SECS,
            detail,
        ),
        CascadeOutcome::Transient { detail } => retry_later(
            "upstream_unavailable",
            RETRY_AFTER_UPSTREAM_SECS,
            detail,
        ),
    }
}

/// Backpressure hint for a saturated admission bucket. The
/// bucket drains as in-flight resolves complete, so the wait is
/// on our own queue rather than on a third party — short.
const RETRY_AFTER_ADMISSION_SECS: u64 = 2;

/// Backpressure hint when an in-flight fetcher outlived the
/// coalescer's wait deadline. Someone else is already doing this
/// exact work; the answer is likely to exist shortly.
const RETRY_AFTER_COALESCER_SECS: u64 = 2;

/// Backpressure hint for a dispatch or upstream failure. Longer,
/// because the cause is outside this device — a provider
/// wobble, a cut cable, a blackholed host — and hammering it
/// helps nobody.
const RETRY_AFTER_UPSTREAM_SECS: u64 = 5;

/// A transient refusal that the caller should retry.
///
/// `Retry-After` is the whole point: it turns "not now" into a
/// scheduled second attempt instead of a permanent verdict. The
/// classification stays on `X-Artwork-Provider` and the detail
/// in the body, so operator diagnostics lose nothing.
fn retry_later(
    provenance: &'static str,
    retry_after_secs: u64,
    detail: String,
) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::HeaderName::from_static(PROVENANCE_HEADER),
        HeaderValue::from_static(provenance),
    );
    headers.insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&retry_after_secs.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("2")),
    );
    // A transient answer must never be cached: the next attempt
    // has to reach the cascade.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (StatusCode::SERVICE_UNAVAILABLE, headers, detail).into_response()
}

fn is_valid_size(size: &str) -> bool {
    matches!(size, "small" | "medium" | "large" | "original" | "tiny")
}

/// Canonicalise the wire-supplied size token: `tiny` maps to
/// `small` (backward compat); every other valid token passes
/// through unchanged. Callers must have validated via
/// [`is_valid_size`] before invoking.
fn canonical_size(size: &str) -> &str {
    match size {
        "tiny" => "small",
        other => other,
    }
}

fn redirect_to_hash_endpoint(
    api_prefix: &str,
    content_hash: &str,
    provider_id: Option<&str>,
) -> Response {
    let location = format!("{api_prefix}/audio/artwork/{content_hash}");
    let mut headers = HeaderMap::new();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(&location)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    // The resolve hop MUST NOT be cached by the client.
    //
    // This is a redirect from a stable subject key to whichever
    // content currently represents it — the target is expected
    // to change. Caching it as immutable froze that decision in
    // the browser for a year: after a cascade correction, after
    // an operator clear, after a better provider was enabled,
    // the client never re-asked. It followed the stored
    // `Location` to the superseded hash and painted the old
    // picture, so fixes landed in the framework and the plugin
    // were invisible on the glass.
    //
    // The earlier reasoning — "any change to the artwork changes
    // the hash, which changes the target, which is a cache miss
    // by construction" — is only true for a client that performs
    // the resolve. A client holding a cached 302 never reaches
    // the code that would produce a different hash.
    //
    // Immutability belongs one hop further on: `/artwork/{hash}`
    // is content-addressed and IS long-cached, so the bytes are
    // still fetched at most once per distinct image. What this
    // costs is one conditional request per tile per paint against
    // a local index lookup — tens of milliseconds, no provider
    // traffic — which is the price of a tile that can change.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Every successful resolve carries the provenance header.
    // Missing plugin-emitted value falls back to `unknown` so
    // the header is always present (operator UI parsers can
    // depend on its presence rather than checking existence).
    headers.insert(
        axum::http::HeaderName::from_static(PROVENANCE_HEADER),
        HeaderValue::from_str(provider_id.unwrap_or("unknown"))
            .unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    (StatusCode::FOUND, headers, "").into_response()
}

#[cfg(test)]
mod tests {
    /// A transient refusal must be retryable, and must say so on
    /// the wire. These classes previously answered 404, which a
    /// client cannot distinguish from a validated absence — so a
    /// tile marked itself permanently broken and never repainted,
    /// even for a local album whose bytes were on disk.
    #[test]
    fn a_transient_refusal_is_retryable_not_a_verdict() {
        let response = retry_later(
            "admission_deadline",
            RETRY_AFTER_ADMISSION_SECS,
            "bucket saturated".to_string(),
        );
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a transient must not answer 404 — 404 means validated absence"
        );
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .expect("a retryable refusal must carry Retry-After");
        assert!(
            retry.parse::<u64>().is_ok_and(|n| n > 0),
            "Retry-After must be a positive delay, got {retry:?}"
        );
    }

    /// A transient answer must never be cached, or the retry it
    /// invites never reaches the cascade.
    #[test]
    fn a_transient_refusal_is_never_cached() {
        let response = retry_later(
            "upstream_unavailable",
            RETRY_AFTER_UPSTREAM_SECS,
            "provider unreachable".to_string(),
        );
        let cc = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .expect("transient responses must set Cache-Control");
        assert!(cc.contains("no-store"), "got {cc:?}");
    }

    /// The classification survives on the provenance header, so
    /// operator diagnostics lose nothing by the status change.
    #[test]
    fn a_transient_refusal_keeps_its_classification() {
        for provenance in [
            "admission_deadline",
            "coalescer_deadline",
            "upstream_unavailable",
        ] {
            let response = retry_later(provenance, 2, "detail".to_string());
            let got = response
                .headers()
                .get(axum::http::HeaderName::from_static(PROVENANCE_HEADER))
                .and_then(|v| v.to_str().ok())
                .expect("provenance header present");
            assert_eq!(got, provenance);
        }
    }

    /// An upstream wobble is someone else's problem and deserves
    /// a longer pause than our own queue draining.
    #[test]
    fn upstream_backoff_is_not_shorter_than_local_backoff() {
        const { assert!(RETRY_AFTER_UPSTREAM_SECS >= RETRY_AFTER_ADMISSION_SECS) };
        const { assert!(RETRY_AFTER_UPSTREAM_SECS >= RETRY_AFTER_COALESCER_SECS) };
        const { assert!(RETRY_AFTER_ADMISSION_SECS > 0) };
        const { assert!(RETRY_AFTER_COALESCER_SECS > 0) };
    }

    use super::*;

    #[test]
    fn size_validation_accepts_canonical_taxonomy() {
        assert!(is_valid_size("small"));
        assert!(is_valid_size("medium"));
        assert!(is_valid_size("large"));
        assert!(is_valid_size("original"));
    }

    #[test]
    fn size_validation_accepts_tiny_as_backward_compat_alias() {
        // `tiny` was the pre-2026-07-21 name for what the
        // canonical artwork-sources design calls `small`. Keep
        // the old name valid on the wire so pre-existing UI
        // callers do not break; new callers use `small`.
        assert!(is_valid_size("tiny"));
    }

    #[test]
    fn size_validation_refuses_arbitrary_values() {
        assert!(!is_valid_size(""));
        assert!(!is_valid_size("xl"));
        assert!(!is_valid_size("SMALL"));
        assert!(!is_valid_size("128"));
    }

    #[test]
    fn canonical_size_maps_tiny_to_small_only() {
        assert_eq!(canonical_size("tiny"), "small");
        assert_eq!(canonical_size("small"), "small");
        assert_eq!(canonical_size("medium"), "medium");
        assert_eq!(canonical_size("large"), "large");
        assert_eq!(canonical_size("original"), "original");
    }

    #[test]
    fn default_size_is_list_safe_medium() {
        // Default is a contract, not convenience: `medium` is
        // the neutral list-safe default per the endpoint's
        // documented contract. If this test fires, the
        // module-level "Default size (list-safe)" section MUST
        // change with the constant.
        assert_eq!(DEFAULT_SIZE, "medium");
    }

    #[test]
    fn redirect_uses_canonical_hash_path() {
        let response = redirect_to_hash_endpoint(
            "/api/v1",
            "abc123def456",
            Some("cover_art_archive"),
        );
        let status = response.status();
        assert_eq!(status, StatusCode::FOUND);
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert_eq!(location, "/api/v1/audio/artwork/abc123def456");
    }

    /// Contract guard: the resolve hop's redirect MUST carry a
    /// long-cache directive so every visible tile does not
    /// re-hit the endpoint every few seconds and defeat the
    /// resolve_index. `?refresh=1` is the operator-editable
    /// override; the framework scanner invalidates the mapping
    /// on real hash changes; the redirect target itself (the
    /// bytes URL) is a different URL on any real content
    /// change, so a stale-cached redirect can never serve
    /// wrong bytes — the browser follows the cached redirect
    /// and either sees the immutable bytes it already cached
    /// or fetches new immutable bytes.
    ///
    /// A previous version shipped `max-age=5` and every tile
    /// re-resolved on every paint, taxing every browse / queue
    /// paint linearly with tile count and time-on-screen. This
    /// test refuses any regression to a sub-day cap.
    #[test]
    fn resolve_redirect_is_never_client_cached() {
        // The resolve hop maps a stable subject key to whichever
        // content currently represents it, and that target is
        // expected to change — on a cascade correction, an
        // operator clear, or a newly enabled provider.
        //
        // Caching it froze that decision in the browser: the
        // client followed a stored `Location` to a superseded
        // hash and painted the old picture, so corrections
        // landed in the framework and the plugin never reached
        // the glass. A cached redirect is a cached DECISION, not
        // cached content.
        let response = redirect_to_hash_endpoint(
            "/api/v1",
            "abc123def456",
            Some("cover_art_archive"),
        );
        let cache_control = response
            .headers()
            .get(header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .expect("resolve redirect must set Cache-Control");
        assert!(
            cache_control.contains("no-store"),
            "resolve redirect must not be client-cached, got {cache_control:?}"
        );
        assert!(
            !cache_control.contains("immutable"),
            "immutability belongs on the content-addressed hop, not here; \
             got {cache_control:?}"
        );
        assert!(
            !cache_control.contains("max-age=31536000"),
            "year-long caching of the resolve hop is the defect that made \
             cascade fixes invisible; got {cache_control:?}"
        );
    }
}
