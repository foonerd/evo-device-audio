// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Artwork-fetch HTTPS endpoint.
//!
//! Mounts `GET /api/v1/audio/artwork/:content_hash` against the
//! framework's content-addressed asset cache. The endpoint
//! serves cached bytes when present, returns 404 when absent.
//! Used by multi-room receivers fetching artwork from the
//! group leader; future consumers (browse-tree art, podcast
//! cover art, lyrics) reach the same endpoint with the same
//! addressing model.
//!
//! ## Cache identity
//!
//! The content hash on the URL path is the SHA-256 of the
//! asset bytes (64 lowercase-hex chars). The endpoint refuses
//! ill-formed paths with `400 Bad Request` rather than
//! returning 404 — distinguishes "the operator's URL is
//! malformed" from "the asset is not cached locally".
//!
//! ## Response headers
//!
//! Responses carry an `ETag: "<sha256>"` matching the path
//! param + `Cache-Control: max-age=31536000, immutable`.
//! Browser + downstream caches treat the bytes as
//! permanent-by-content-hash; identity changes mean a different
//! URL, so the long max-age is safe.
//!
//! The endpoint refuses requests without a valid bearer token
//! (delegated to the same `AuthLayer` middleware the
//! schema-driven routes use). The required capability is
//! `read:audio`; consumers fetching artwork already hold this
//! scope via the bootstrap operator token + every minted
//! session token.

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_plugin_sdk::contract::asset_cache::AssetCache;
use evo_projection_core::{CapabilityRequirement, WireOpId};
use std::sync::Arc;

use evo_runtime_http::auth_tier::AuthTierProvider;
use evo_runtime_http::middleware::{capability_gate, AuthLayer};
use evo_runtime_http::principal::Principal;

/// Mount the artwork endpoint onto the supplied router under
/// the canonical `/api/v1/audio/artwork/:content_hash` path.
/// Wraps the route in the bearer-auth middleware so the
/// endpoint refuses unauthenticated requests at the wire layer.
///
/// Returns `Err(RuntimeHttpError::EndpointAttachRefused)` when
/// the in-code wire-op id constant fails to validate. This
/// MUST NOT be reachable in a tested build (the regression
/// test exercises construction); it is fallible by contract so
/// the framework's unbreakability invariant is preserved if a
/// future code change to the constant happens past a missing
/// test. The caller surfaces the error to the operator log
/// rather than letting the steward boot panic.
pub fn attach_artwork_endpoint(
    router: Router,
    api_prefix: &str,
    asset_cache: Arc<dyn AssetCache>,
    validator: Arc<BearerTokenValidator>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Result<Router, evo_runtime_http::error::RuntimeHttpError> {
    let path = format!("{api_prefix}/audio/artwork/:content_hash");
    let requirement = CapabilityRequirement::read("audio");
    let op_id = WireOpId::new("audio_artwork_fetch").map_err(|e| {
        evo_runtime_http::error::RuntimeHttpError::EndpointAttachRefused {
            endpoint: "audio_artwork_fetch".into(),
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
    Ok(router.route(
        &path,
        get(handle_fetch).with_state(asset_cache).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        ),
    ))
}

async fn handle_fetch(
    State(cache): State<Arc<dyn AssetCache>>,
    axum::Extension(_principal): axum::Extension<Principal>,
    Path(content_hash): Path<String>,
) -> Response {
    if !is_valid_hash(&content_hash) {
        return (
            StatusCode::BAD_REQUEST,
            "content_hash must be 64 lowercase-hex characters",
        )
            .into_response();
    }
    match cache.get(&content_hash).await {
        Ok(Some(bytes)) => {
            // Sniff the MIME from the leading bytes BEFORE the
            // bytes move into into_response(). Browsers sniff
            // <img> on their own, but proxies / CDN edge caches
            // / image-processing libraries respect declared
            // Content-Type and would otherwise see
            // `application/octet-stream` for every image the
            // cache serves.
            let mime = sniff_image_mime(&bytes);
            let mut response = bytes.into_response();
            let headers = response.headers_mut();
            // Permanent-by-content-hash; identity changes mean a
            // different URL, so the long max-age is safe.
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("max-age=31536000, immutable"),
            );
            // ETag matches the path param; downstream caches
            // (browser, reverse proxies, member nodes) can do
            // conditional fetches even though the bytes are
            // already immutable by construction.
            if let Ok(etag_value) =
                HeaderValue::from_str(&format!("\"{content_hash}\""))
            {
                headers.insert(header::ETAG, etag_value);
            }
            headers
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
            response
        }
        Ok(None) => (StatusCode::NOT_FOUND, "asset not cached").into_response(),
        Err(e) => {
            tracing::warn!(
                content_hash = %content_hash,
                error = %e,
                "artwork endpoint: asset cache get failed"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "asset cache lookup failed",
            )
                .into_response()
        }
    }
}

fn is_valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Sniff the MIME type from the leading bytes of an asset.
///
/// The cache stores opaque bytes addressed by SHA-256; this
/// function recognises the image formats the audio reference
/// distribution writes (JPEG, PNG, WebP, GIF) by their well-
/// known magic numbers. Unknown formats fall back to
/// `application/octet-stream` so the browser can still sniff
/// per its own rules without the framework asserting an
/// incorrect type.
fn sniff_image_mime(bytes: &[u8]) -> &'static str {
    // JPEG: FF D8 FF
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    // PNG: 89 50 4E 47 0D 0A 1A 0A
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        return "image/png";
    }
    // WebP: RIFF...WEBP — byte layout is 'R' 'I' 'F' 'F' [4 size
    // bytes] 'W' 'E' 'B' 'P'. Validate the RIFF marker AND the
    // WEBP form marker; the chunk size between them is variable.
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
    {
        return "image/webp";
    }
    // GIF: GIF87a or GIF89a
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return "image/gif";
    }
    // Unknown — let the browser sniff its own.
    "application/octet-stream"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_hash_accepted() {
        let h = "a".repeat(64);
        assert!(is_valid_hash(&h));
    }

    #[test]
    fn uppercase_hash_refused() {
        let h = "A".repeat(64);
        assert!(!is_valid_hash(&h));
    }

    #[test]
    fn sniff_jpeg_from_magic_bytes() {
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert_eq!(sniff_image_mime(&bytes), "image/jpeg");
    }

    #[test]
    fn sniff_png_from_magic_bytes() {
        let bytes = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        assert_eq!(sniff_image_mime(&bytes), "image/png");
    }

    #[test]
    fn sniff_webp_from_riff_webp_marker() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]); // size
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(b"VP8 "); // chunk type
        assert_eq!(sniff_image_mime(&bytes), "image/webp");
    }

    #[test]
    fn sniff_gif_from_magic_bytes() {
        assert_eq!(sniff_image_mime(b"GIF87a..."), "image/gif");
        assert_eq!(sniff_image_mime(b"GIF89a..."), "image/gif");
    }

    #[test]
    fn sniff_unknown_falls_back_to_octet_stream() {
        assert_eq!(
            sniff_image_mime(b"random bytes"),
            "application/octet-stream"
        );
        assert_eq!(sniff_image_mime(&[]), "application/octet-stream");
    }

    #[test]
    fn wrong_length_hash_refused() {
        let h = "a".repeat(63);
        assert!(!is_valid_hash(&h));
        let h = "a".repeat(65);
        assert!(!is_valid_hash(&h));
    }

    #[test]
    fn non_hex_refused() {
        let mut h = "a".repeat(63);
        h.push('z');
        assert!(!is_valid_hash(&h));
    }

    /// Regression: the wire-op id used by the auth layer must be
    /// a valid snake-case identifier. A period-separated id like
    /// `audio.artwork.fetch` is refused at construction time and
    /// the `.expect(...)` in `attach_artwork_endpoint` panics on
    /// every steward boot. This test exercises the attach path
    /// end-to-end so the regression is caught at unit-test time.
    #[test]
    fn attach_artwork_endpoint_does_not_panic_on_construct() {
        use evo_auth_bearer::BearerTokenIssuer;
        use evo_auth_bearer::RevocationList;
        use std::pin::Pin;

        struct StubCache;
        impl AssetCache for StubCache {
            fn get<'a>(
                &'a self,
                _content_hash: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                Option<Vec<u8>>,
                                evo_plugin_sdk::contract::asset_cache::AssetCacheError,
                            >,
                        > + Send
                        + 'a,
                >,
            >{
                Box::pin(async { Ok(None) })
            }
            fn put<'a>(
                &'a self,
                _content_hash: &'a str,
                _bytes: Vec<u8>,
            ) -> Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                (),
                                evo_plugin_sdk::contract::asset_cache::AssetCacheError,
                            >,
                        > + Send
                        + 'a,
                >,
            >{
                Box::pin(async { Ok(()) })
            }
            fn get_or_fetch<'a>(
                &'a self,
                _content_hash: &'a str,
                _fetch_fn: Box<
                    dyn evo_plugin_sdk::contract::asset_cache::AssetFetcher
                        + Send
                        + 'static,
                >,
            ) -> Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                Vec<u8>,
                                evo_plugin_sdk::contract::asset_cache::AssetCacheError,
                            >,
                        > + Send
                        + 'a,
                >,
            >{
                Box::pin(async { Ok(Vec::new()) })
            }
            fn delete<'a>(
                &'a self,
                _content_hash: &'a str,
            ) -> Pin<
                Box<
                    dyn std::future::Future<
                            Output = Result<
                                bool,
                                evo_plugin_sdk::contract::asset_cache::AssetCacheError,
                            >,
                        > + Send
                        + 'a,
                >,
            >{
                Box::pin(async { Ok(false) })
            }
        }

        let signing_key = BearerTokenIssuer::generate_signing_key();
        let validator = Arc::new(evo_auth_bearer::BearerTokenValidator::new(
            signing_key.verifying_key(),
            Arc::new(RevocationList::new()),
        ));
        let cache: Arc<dyn AssetCache> = Arc::new(StubCache);
        let tier_provider: Arc<
            dyn evo_runtime_http::auth_tier::AuthTierProvider,
        > = Arc::new(evo_runtime_http::auth_tier::StaticAuthTier::new(
            evo_runtime_http::auth_tier::AuthTier::Open,
        ));
        let lan_trust_caps =
            CapabilitySet::new(vec![evo_auth_bearer::Capability::read(
                "audio",
            )]);
        // Attaching MUST succeed against the canonical
        // wire-op id. The attach helper is fallible so a
        // future regression with an ill-formed id surfaces as
        // a structured error rather than a panic; this test
        // also confirms the canonical-id call path returns Ok.
        let router = attach_artwork_endpoint(
            Router::new(),
            "/api/v1",
            cache,
            validator,
            tier_provider,
            lan_trust_caps,
        )
        .expect("attach must succeed against the canonical wire-op id");
        let _: Router = router;
    }
}
