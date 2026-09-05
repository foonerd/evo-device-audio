// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Device-proxied captive-portal session HTTPS endpoint.
//!
//! Mounts `/api/v1/network/captive/session/:sid` and
//! `/api/v1/network/captive/session/:sid/*path` — a
//! same-origin operator-facing surface the UI iframes on the
//! management plane. Every browser request is dispatched
//! through the framework's `Dispatcher` into the network
//! plugin's `network.nm.captive.upstream.fetch` wire-op, which
//! fetches the portal upstream over the captive-carrying
//! interface (`SO_BINDTODEVICE` — see
//! `evo-device-audio/dist/bin/evo-captive-probe`), so the
//! operator's browser NEVER touches the venue directly. The
//! portal admits the device MAC (already associated with
//! wlan0); the operator's remote LAN browser sees only
//! same-origin bytes served by the framework proxy.
//!
//! Response processing:
//!
//! * Strip hop-by-hop headers (Connection, Keep-Alive,
//!   Transfer-Encoding, TE, Trailer, Proxy-*, Upgrade).
//! * Strip `Set-Cookie` — the plugin owns the jar keyed by
//!   `session_id`; the operator's browser has no cookies
//!   because it is not the party the portal is tracking.
//! * Rewrite `Location` when it points at the upstream host
//!   into a same-origin `/api/v1/network/captive/session/{sid}`
//!   URL, so redirects follow through the proxy.
//! * Byte-substitute the upstream host in HTML / CSS / plain
//!   text bodies. Relative URLs in portal HTML resolve
//!   against the browser's current location — which IS the
//!   session URL — so no rewrite is needed for the common
//!   case (`./static/js/main.js`). The substitution catches
//!   the less-common but real case of absolute URLs baked
//!   into portal HTML.
//! * Decode upstream `Content-Encoding` (`gzip` / `deflate`)
//!   before rewrite or handoff, and never forward the
//!   browser's `Accept-Encoding` upstream. Without this,
//!   captive-portal controllers commonly return compressed
//!   JSON while the proxy strips the encoding
//!   header — the SPA then `JSON.parse`s gzip bytes and
//!   falls back to a default landing shell.
//!
//! Gate: `write:network_admin` — the paired-operator connect
//! path. Same scope as `network.nm.captive.submit` and the
//! plugin's `session.start` / `upstream.fetch` /
//! `session.close` verbs, so the operator's bearer chain
//! carries through end-to-end without an elevation prompt.

use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::header::{
    HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_LENGTH, HOST,
    LOCATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine as _;
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_projection_core::{CapabilityRequirement, WireOpId};
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;

use evo_runtime_http::auth_tier::AuthTierProvider;
use evo_runtime_http::dispatcher::Dispatcher;
use evo_runtime_http::middleware::{capability_gate, AuthLayer};
use evo_runtime_http::principal::Principal;

const SHELF: &str = "networking.link";
const UPSTREAM_FETCH_REQUEST_TYPE: &str = "network.nm.captive.upstream.fetch";

/// Mount the device-proxied captive-session endpoint under
/// `/api/v1/network/captive/session/:sid[/*path]`. Both the
/// bare-sid form (portal root, no subpath) and the wildcard
/// form (deeper portal paths) route to the same handler,
/// distinguishing at request time by whether `path` is
/// present.
///
/// Returns `Err(RuntimeHttpError::EndpointAttachRefused)` if
/// the in-code wire-op id constant fails to validate. This is
/// the contract every attach helper answers to. Unreachable in a tested
/// build; fallible-by-contract preserves the steward's
/// unbreakability invariant if a future constant edit lands
/// without a matching test.
pub fn attach_captive_session_endpoint(
    router: Router,
    api_prefix: &str,
    dispatcher: Arc<dyn Dispatcher>,
    validator: Arc<BearerTokenValidator>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Result<Router, evo_runtime_http::error::RuntimeHttpError> {
    // axum 0.7 route patterns: `:name` for one segment,
    // `*name` for the catch-all tail. axum 0.8's `{name}` /
    // `{*name}` syntax is not yet in this crate's dep pin.
    let root_path = format!("{api_prefix}/network/captive/session/:sid");
    let wild_path = format!("{api_prefix}/network/captive/session/:sid/*path");
    let requirement = CapabilityRequirement::write("network_admin");
    let op_id = WireOpId::new("captive_session_proxy").map_err(|e| {
        evo_runtime_http::error::RuntimeHttpError::EndpointAttachRefused {
            endpoint: "captive_session_proxy".into(),
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
        dispatcher,
        api_prefix: api_prefix.to_string(),
    };
    // `MethodRouter::route_layer` requires at least one method
    // handler to be attached before the layer, so we bind every
    // HTTP method the operator's browser may issue against the
    // portal (GET / HEAD / POST / PUT / DELETE / PATCH /
    // OPTIONS) to the same handler explicitly. `any(...)` bind
    // via fallback does not satisfy the check.
    let root_methods = get(handle_proxy_root)
        .head(handle_proxy_root)
        .post(handle_proxy_root)
        .put(handle_proxy_root)
        .delete(handle_proxy_root)
        .patch(handle_proxy_root)
        .options(handle_proxy_root);
    let wild_methods = get(handle_proxy_wild)
        .head(handle_proxy_wild)
        .post(handle_proxy_wild)
        .put(handle_proxy_wild)
        .delete(handle_proxy_wild)
        .patch(handle_proxy_wild)
        .options(handle_proxy_wild);
    let router = router
        .route(
            &root_path,
            root_methods.with_state(state.clone()).route_layer(
                axum::middleware::from_fn_with_state(
                    auth.clone(),
                    capability_gate,
                ),
            ),
        )
        .route(
            &wild_path,
            wild_methods.with_state(state).route_layer(
                axum::middleware::from_fn_with_state(auth, capability_gate),
            ),
        );
    Ok(router)
}

#[derive(Clone)]
struct HandlerState {
    dispatcher: Arc<dyn Dispatcher>,
    api_prefix: String,
}

async fn handle_proxy_root(
    State(state): State<HandlerState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(sid): Path<String>,
    request: Request,
) -> Response {
    proxy_request(state, principal, sid, String::new(), request).await
}

async fn handle_proxy_wild(
    State(state): State<HandlerState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path((sid, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    proxy_request(state, principal, sid, path, request).await
}

async fn proxy_request(
    state: HandlerState,
    principal: Principal,
    sid: String,
    subpath: String,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let request_headers = request.headers().clone();
    let body_bytes =
        match axum::body::to_bytes(request.into_body(), REQUEST_BODY_MAX_BYTES)
            .await
        {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("request body read failed: {e}"),
                )
                    .into_response();
            }
        };

    // Filter caller headers:
    // * Drop hop-by-hop headers (Connection, Keep-Alive, TE,
    //   Trailer, Transfer-Encoding, Upgrade,
    //   Proxy-Authenticate/Authorization).
    // * Drop Host — the plugin resolves against the fixed
    //   upstream_host recorded at session.start and curl
    //   emits its own Host on the outbound request.
    // * Cookie is preserved as-is; the plugin merges it with
    //   its per-session jar.
    //
    // Referer / Origin inversion (browser sends session-prefix
    // form, portal CSRF-checks upstream origin) is a
    // deliberate follow-up: the correct place is inside the
    // plugin's upstream.fetch handler where the per-session
    // `upstream_host` is known without a cross-boundary
    // round-trip. Endpoint here forwards the browser values
    // verbatim; portals that hard-CSRF-check Referer/Origin
    // will need that follow-up before they admit.
    //
    // Drop Accept-Encoding so the venue is asked for identity
    // bodies. Many portals still gzip anyway — those are
    // decoded below from Content-Encoding / magic sniff.
    let filtered_headers: Vec<Value> = request_headers
        .iter()
        .filter(|(name, _)| !is_hop_by_hop(name.as_str()))
        .filter(|(name, _)| *name != HOST)
        .filter(|(name, _)| {
            !name.as_str().eq_ignore_ascii_case("accept-encoding")
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| json!({ "name": name.as_str(), "value": v }))
        })
        .collect();

    let path_for_plugin = if subpath.is_empty() {
        String::new()
    } else {
        format!("/{subpath}")
    };
    let query = uri.query().unwrap_or("").to_string();
    let body_b64 = if body_bytes.is_empty() {
        String::new()
    } else {
        base64::engine::general_purpose::STANDARD.encode(&body_bytes)
    };
    let inner_payload = json!({
        "session_id": sid,
        "method": method.as_str(),
        "path": path_for_plugin,
        "query": query,
        "headers": filtered_headers,
        "body_b64": body_b64,
    });
    let payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(inner_payload.to_string().as_bytes());
    let envelope = json!({
        "shelf": SHELF,
        "request_type": UPSTREAM_FETCH_REQUEST_TYPE,
        "payload_b64": payload_b64,
    });
    let op_id = match WireOpId::new("request") {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("wire-op id construction failed: {e}"),
            )
                .into_response();
        }
    };
    let dispatch_res = state
        .dispatcher
        .dispatch(&op_id, envelope, &principal)
        .await;
    let response_env = match dispatch_res {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("captive upstream dispatch failed: {e}"),
            )
                .into_response();
        }
    };

    let inner = match peel_plugin_response(&response_env) {
        Some(v) => v,
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                format!(
                    "captive upstream response malformed (no payload_b64): {response_env}"
                ),
            )
                .into_response();
        }
    };

    if let Some(err) = inner.get("error") {
        // Plugin refused (session_not_found / no_captive / etc.)
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("captive upstream plugin error");
        return (StatusCode::BAD_GATEWAY, msg.to_string()).into_response();
    }

    let http_status = inner
        .get("http_status")
        .and_then(|v| v.as_u64())
        .unwrap_or(502) as u16;
    let upstream_host = inner
        .get("upstream_host")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let session_prefix =
        format!("{}/network/captive/session/{}", state.api_prefix, sid);
    let response_headers_raw: Vec<(String, String)> = inner
        .get("headers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|h| {
                    let n = h.get("name")?.as_str()?.to_string();
                    let v = h.get("value")?.as_str()?.to_string();
                    Some((n, v))
                })
                .collect()
        })
        .unwrap_or_default();
    let body_b64_out =
        inner.get("body_b64").and_then(|v| v.as_str()).unwrap_or("");
    let raw_body = match base64::engine::general_purpose::STANDARD
        .decode(body_b64_out.as_bytes())
    {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("captive upstream body_b64 decode failed: {e}"),
            )
                .into_response();
        }
    };

    // Build client-facing headers: filter hop-by-hop, drop
    // Set-Cookie (plugin owns the jar), rewrite Location, drop
    // Content-Length (recomputed after decode + rewrite), drop
    // Content-Encoding (body is always served as identity after
    // decode_upstream_body — re-emitting the upstream encoding
    // would lie to the browser and break SPA JSON.parse).
    let mut out_headers = HeaderMap::new();
    let mut content_type: Option<String> = None;
    let mut content_encoding: Option<String> = None;
    for (name, value) in &response_headers_raw {
        let lname = name.to_ascii_lowercase();
        if is_hop_by_hop(&lname) {
            continue;
        }
        if lname == "set-cookie" {
            continue;
        }
        if lname == "content-encoding" {
            content_encoding = Some(value.clone());
            continue;
        }
        if lname == "content-length" {
            continue;
        }
        if lname == "location" {
            let rewritten =
                rewrite_location(value, upstream_host, &session_prefix);
            if let (Ok(hn), Ok(hv)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&rewritten),
            ) {
                out_headers.insert(hn, hv);
            }
            continue;
        }
        if lname == "content-type" {
            content_type = Some(value.clone());
        }
        if let (Ok(hn), Ok(hv)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            out_headers.insert(hn, hv);
        }
    }

    let decoded_body =
        match decode_upstream_body(raw_body, content_encoding.as_deref()) {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("captive upstream body decode failed: {e}"),
                )
                    .into_response();
            }
        };

    let final_body = if should_rewrite_body(content_type.as_deref())
        && !upstream_host.is_empty()
    {
        rewrite_body(&decoded_body, upstream_host, &session_prefix)
    } else {
        decoded_body
    };

    if let Ok(hv) = HeaderValue::from_str(&final_body.len().to_string()) {
        out_headers.insert(CONTENT_LENGTH, hv);
    }

    let status =
        StatusCode::from_u16(http_status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, Bytes::from(final_body)).into_response();
    *response.headers_mut() = out_headers;
    // The status coded above was set by `into_response` on the tuple,
    // so restoring headers preserves it; re-set defensively.
    *response.status_mut() = status;
    let _ = method;
    response
}

const REQUEST_BODY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Peel a framework-dispatched `request` op response into its
/// inner plugin response value. The dispatcher returns a JSON
/// envelope `{ payload_b64: "<base64 of inner JSON>" }`; the
/// inner JSON is the plugin's actual response body.
fn peel_plugin_response(envelope: &Value) -> Option<Value> {
    let pb = envelope.get("payload_b64")?.as_str()?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(pb.as_bytes())
        .ok()?;
    serde_json::from_slice(&raw).ok()
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Decode an upstream response body to identity octets.
///
/// Portal-agnostic: captive-portal controllers commonly gzip
/// JSON / API payloads when they see a browser
/// `Accept-Encoding`. The proxy must present plain bytes to
/// the operator SPA (and to `rewrite_body`).
///
/// Rules:
/// * `gzip` / `x-gzip` → `GzDecoder`
/// * `deflate` → try zlib wrapper first, then raw deflate
///   (HTTP "deflate" is historically ambiguous)
/// * `identity` / absent → pass through, BUT if the body
///   starts with the gzip magic (`1f 8b`) still decompress —
///   some controllers omit or mishandle `Content-Encoding`
/// * `br` / anything else → hard error (we strip
///   `Accept-Encoding` upstream so brotli should not appear;
///   failing closed beats serving opaque bytes as JSON)
fn decode_upstream_body(
    body: Vec<u8>,
    content_encoding: Option<&str>,
) -> Result<Vec<u8>, String> {
    let enc = content_encoding
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let looks_gzip = body.len() >= 2 && body[0] == 0x1f && body[1] == 0x8b;

    match enc.as_deref() {
        None | Some("identity") => {
            if looks_gzip {
                decompress_gzip(&body)
            } else {
                Ok(body)
            }
        }
        Some("gzip") | Some("x-gzip") => decompress_gzip(&body),
        Some("deflate") => decompress_deflate(&body),
        Some(other) => Err(format!(
            "unsupported Content-Encoding '{other}' \
             (captive proxy serves identity only; gzip/deflate accepted)"
        )),
    }
}

fn decompress_gzip(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut dec = GzDecoder::new(body);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| format!("gzip decompress failed: {e}"))?;
    Ok(out)
}

fn decompress_deflate(body: &[u8]) -> Result<Vec<u8>, String> {
    // Prefer zlib-wrapped deflate (common); fall back to raw.
    let mut zlib_out = Vec::new();
    match ZlibDecoder::new(body).read_to_end(&mut zlib_out) {
        Ok(_) => Ok(zlib_out),
        Err(zlib_err) => {
            let mut raw_out = Vec::new();
            DeflateDecoder::new(body)
                .read_to_end(&mut raw_out)
                .map_err(|e| {
                    format!(
                        "deflate decompress failed \
                         (zlib: {zlib_err}; raw: {e})"
                    )
                })?;
            Ok(raw_out)
        }
    }
}

/// Rewrite the value of an upstream `Location` (or equivalent
/// URL-carrying) header into a same-origin session-prefixed
/// URL. Three cases, in order of precedence:
///
/// 1. Absolute URL whose authority matches `upstream_host` —
///    strip the upstream prefix and prepend the session
///    prefix. E.g. `http://portal/foo` →
///    `/api/v1/network/captive/session/{sid}/foo`.
/// 2. Path-absolute (`/foo`, NOT `//foo` — that second form
///    is protocol-relative and points at an ARBITRARY host,
///    not the upstream, so we leave it alone). Prepend the
///    session prefix so the browser stays on the proxy. The
///    Escape-class the two-pass discipline closes: a
///    captive-portal Login step commonly emits
///    `Location: /portal-path/…` — untouched, that lands
///    the operator's browser on `<mgmt>/portal-path/…`
///    (404, off proxy).
/// 3. Anything else (foreign absolute host, protocol-
///    relative `//host/foo`, blob:/data:/mailto:/etc.) —
///    leave verbatim. The operator's browser handles those
///    natively; injecting the session prefix on a foreign
///    URL would break the navigation entirely.
fn rewrite_location(
    value: &str,
    upstream_host: &str,
    session_prefix: &str,
) -> String {
    if !upstream_host.is_empty() {
        if let Some(rest) = value.strip_prefix(upstream_host) {
            return format!("{session_prefix}{rest}");
        }
    }
    if value.starts_with('/') && !value.starts_with("//") {
        return format!("{session_prefix}{value}");
    }
    value.to_string()
}

/// Content types where byte-scan URL rewriting is safe. HTML
/// / CSS / plain / xhtml have unambiguous URL contexts
/// (`href="…"`, `src="…"`, `url(…)`) the scanner can pin.
///
/// `application/javascript`, `text/javascript`, and
/// `application/json` are DELIBERATELY excluded. A byte-scan
/// cannot AST-distinguish a path-absolute URL `/foo` from:
///
/// - a regex literal `p(/^foo/)` — the `/` after `(` is a
///   regex opener, not a URL start;
/// - a Redux-toolkit action-type suffix `e + "/pending"` — the
///   `"` is preceded by `+`, which is non-word, so a naive
///   heuristic fires and injects a session prefix into the
///   ACTION STRING, breaking every async thunk that matches
///   `/pending` / `/fulfilled` / `/rejected`;
/// - a division `x /= 2` — the `/` is a binary operator, not
///   a URL;
/// - JSON string literals used as identifiers rather than URLs.
///
/// Empirically verified against a real-world JS-SPA captive
/// portal (a ~520 KiB Redux-toolkit bundle): a prior version
/// of this proxy that included JS in `should_rewrite_body`
/// produced ~150 rewrites, every one a corruption (regex,
/// Redux action suffix, division, or embedded identifier).
/// Zero were legitimate URLs. Modern captive-portal SPAs
/// derive every request URL from
/// `window.location.pathname.split("/")`, so requests
/// already ride the session prefix without any body
/// modification — nothing to rewrite.
///
/// A portal that genuinely emits path-absolute URLs from JS
/// needs an AST-aware transformation (or an injected
/// `<base>` tag on the HTML index), never a byte scan.
fn should_rewrite_body(content_type: Option<&str>) -> bool {
    let Some(ct) = content_type else { return false };
    let ct = ct.to_ascii_lowercase();
    ct.starts_with("text/html")
        || ct.starts_with("text/css")
        || ct.starts_with("text/plain")
        || ct.starts_with("application/xhtml")
}

/// Byte-level rewrite of an upstream response body so every
/// URL an operator's browser will follow stays under the
/// session-prefix. Two passes:
///
/// 1. **Absolute upstream-host rewrite** — swap every
///    literal occurrence of `upstream_host` (scheme +
///    authority, no trailing slash) with the session prefix.
///    Handles hard-coded `href="http://portal:8880/x"`
///    references in generated HTML/CSS/JS.
/// 2. **Path-absolute rewrite** — an audit-flagged escape:
///    a browser resolving `/guest/foo` against the iframe's
///    origin ends up at `<mgmt>/guest/foo`, not
///    `<mgmt>/api/v1/network/captive/session/{sid}/guest/foo`,
///    so the request escapes the proxy. Prepend the session
///    prefix everywhere a path-absolute URL appears in a
///    URL-carrying context:
///    * HTML attributes: `href="/…"`, `href='/…'`, `src=`,
///      `action=`, `formaction=`, `data=`, `poster=`,
///      `background=`. Both single- and double-quoted.
///    * CSS: `url("/…")`, `url('/…')`, `url(/…)`.
///    * JS/JSON string literals: bareword `"/…"` /
///      `'/…'` where the char BEFORE the quote is
///      non-word (whitespace, `,`, `:`, `=`, `[`, `(`,
///      `!`, `+`, `?`, `;`, `\n`, `\t`, start of body).
///      This heuristic keeps us from rewriting embedded
///      strings the operator's site data structure never
///      intends as a URL, while catching the standard SPA
///      `fetch("/api/…")` / `location.href = "/guest/…"`
///      patterns.
///
///    Protocol-relative URLs (`//host/foo`) and non-URL
///    contexts (`"/etc/passwd"` inside a JSON string
///    preceded by an alphanumeric byte) are left alone.
///
/// The scan is a single linear pass over the byte stream
/// with lookahead. Portal HTML/CSS/JS is KB-to-MB scale; a
/// find-based approach is cheaper than pulling in a full
/// HTML rewriter for the common case.
fn rewrite_body(
    body: &[u8],
    upstream_host: &str,
    session_prefix: &str,
) -> Vec<u8> {
    let host_bytes = upstream_host.as_bytes();
    let prefix_bytes = session_prefix.as_bytes();
    let mut out = Vec::with_capacity(body.len() + body.len() / 8);
    let mut i = 0usize;
    while i < body.len() {
        // Pass 1: absolute upstream-host swap.
        if !upstream_host.is_empty()
            && i + host_bytes.len() <= body.len()
            && &body[i..i + host_bytes.len()] == host_bytes
        {
            out.extend_from_slice(prefix_bytes);
            i += host_bytes.len();
            continue;
        }
        // Pass 2: path-absolute detection. Cheap byte tests
        // decide whether the current `/` starts a path-
        // absolute URL that must be prefixed.
        if body[i] == b'/' {
            let is_double_slash = i + 1 < body.len() && body[i + 1] == b'/';
            if !is_double_slash {
                // Prev byte gates the rewrite decision.
                let prev = if i == 0 { 0u8 } else { body[i - 1] };
                let is_after_quote_after_url_context = prev == b'"'
                    || prev == b'\''
                    || prev == b'('
                    || prev == b'=';
                if is_after_quote_after_url_context {
                    let is_url_context = match prev {
                        b'"' | b'\'' => {
                            // For quoted string opener: rewrite when
                            // preceded by:
                            //   * HTML attribute context: `=`  (already
                            //     covered by looking further back)
                            //   * URL-func: `(`
                            //   * Non-word char (JS/JSON bareword)
                            let before_quote =
                                if i < 2 { 0u8 } else { body[i - 2] };
                            // Look for `=`, `(`, or non-word char.
                            before_quote == b'='
                                || before_quote == b'('
                                || !is_word_byte(before_quote)
                        }
                        b'(' => true, // `url(/…)` bareword
                        b'=' => true, // `href=/…` unquoted (rare HTML)
                        _ => false,
                    };
                    if is_url_context {
                        out.push(b'/');
                        // Prepend session prefix WITHOUT the
                        // leading `/` — the `/` we just emitted
                        // is the path-absolute slash the URL
                        // began with, and the session_prefix
                        // itself already starts with `/`.
                        // Wait — session_prefix is
                        // `/api/v1/network/captive/session/{sid}`.
                        // Emitting the leading `/` then
                        // session_prefix (which starts with
                        // `/`) would yield `//api/...`. Fix by
                        // NOT emitting the leading `/`; the
                        // session_prefix carries its own.
                        out.pop(); // undo the leading `/`
                        out.extend_from_slice(prefix_bytes);
                        // Now emit the original `/` and
                        // continue — the path following the `/`
                        // is preserved verbatim.
                        out.push(b'/');
                        i += 1;
                        continue;
                    }
                }
            }
        }
        out.push(body[i]);
        i += 1;
    }
    out
}

/// Word byte: identifier characters. Any byte NOT in this
/// set is a boundary — used by the JS/JSON bareword URL
/// heuristic to decide whether a `"/…"` occurrence is the
/// start of a new string (rewrite) or continuation of a
/// longer non-URL literal (leave alone).
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Retain UPGRADE for clippy without actually applying it; the
/// filter list uses the string form for lower-case comparison,
/// which is faster than parsing HeaderName in the hot path.
#[allow(dead_code)]
const _RETAINED_IMPORTS_FOR_TYPE_SAFETY: (
    &HeaderName,
    &HeaderName,
    &HeaderName,
    &HeaderName,
    &HeaderName,
    &HeaderName,
) = (
    &CONNECTION,
    &TE,
    &TRAILER,
    &TRANSFER_ENCODING,
    &UPGRADE,
    &LOCATION,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_location_replaces_upstream_prefix() {
        let out = rewrite_location(
            "http://198.51.100.1:8080/portal/session/default/next",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(
            out,
            "/api/v1/network/captive/session/abc/portal/session/default/next"
        );
    }

    #[test]
    fn rewrite_location_leaves_foreign_host_alone() {
        let out = rewrite_location(
            "https://identity.example/callback",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(out, "https://identity.example/callback");
    }

    #[test]
    fn rewrite_body_replaces_every_occurrence() {
        let body = br#"<a href="http://198.51.100.1:8080/x">go</a><img src="http://198.51.100.1:8080/logo.png">"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let out_s = String::from_utf8_lossy(&out);
        assert!(out_s.contains("/api/v1/network/captive/session/abc/x"));
        assert!(out_s.contains("/api/v1/network/captive/session/abc/logo.png"));
        assert!(!out_s.contains("http://198.51.100.1:8080"));
    }

    #[test]
    fn rewrite_body_no_op_when_upstream_absent() {
        let body = br#"<a href="./static/main.js">"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(out, body.to_vec());
    }

    #[test]
    fn should_rewrite_body_html_and_css_yes_js_and_json_no() {
        // HTML / CSS / plain / xhtml have unambiguous URL
        // contexts and are safe to byte-rewrite.
        assert!(should_rewrite_body(Some("text/html;charset=utf-8")));
        assert!(should_rewrite_body(Some("text/css")));
        assert!(should_rewrite_body(Some("text/plain")));
        assert!(should_rewrite_body(Some("application/xhtml+xml")));
        // application/javascript / text/javascript /
        // application/json are explicitly NOT rewritten. A
        // byte scan cannot distinguish `/foo` (URL) from
        // `/foo/` (regex) from `+"/pending"` (Redux action
        // suffix) from `x /= 2` (division). A live rig test
        // of a real-world SPA showed a naive JS pass produces
        // ~150 corruptions and zero legitimate URL rewrites.
        assert!(!should_rewrite_body(Some("application/json")));
        assert!(!should_rewrite_body(Some("application/javascript")));
        assert!(!should_rewrite_body(Some("text/javascript;charset=utf-8")));
        assert!(!should_rewrite_body(Some("image/png")));
        assert!(!should_rewrite_body(Some("application/pdf")));
        assert!(!should_rewrite_body(None));
    }

    #[test]
    fn hop_by_hop_matches_ignore_case() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(is_hop_by_hop("upgrade"));
        assert!(!is_hop_by_hop("content-type"));
    }

    // --- Path-absolute rewrite regressions ---
    // Captive-portal Login steps commonly emit
    // `Location: /portal-path/…`; if the proxy leaves that
    // untouched the browser resolves against the management
    // origin and escapes the session prefix. Every
    // path-absolute escape mode gets a regression here.

    #[test]
    fn rewrite_location_path_absolute_prepends_session_prefix() {
        // The audit-flagged case: portal 302 with
        // path-absolute Location.
        let out = rewrite_location(
            "/portal/session/default/next",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(
            out, "/api/v1/network/captive/session/abc/portal/session/default/next",
            "path-absolute Location: /portal/session/default/next must prepend session prefix"
        );
    }

    #[test]
    fn rewrite_location_protocol_relative_untouched() {
        // `//host/foo` is protocol-relative — points at
        // an ARBITRARY host, not the upstream. Must NOT be
        // rewritten (would redirect to session-prefix on
        // whatever host, breaking navigation).
        let out = rewrite_location(
            "//cdn.example/asset.js",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(out, "//cdn.example/asset.js");
    }

    #[test]
    fn rewrite_location_absolute_upstream_still_swaps() {
        // Regression on the existing behaviour: absolute
        // URLs whose host matches upstream_host still get
        // stripped + prepended.
        let out = rewrite_location(
            "http://198.51.100.1:8080/portal/session/default/next",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(
            out,
            "/api/v1/network/captive/session/abc/portal/session/default/next"
        );
    }

    #[test]
    fn rewrite_location_foreign_host_untouched() {
        let out = rewrite_location(
            "https://identity.example/callback",
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(out, "https://identity.example/callback");
    }

    #[test]
    fn rewrite_body_html_href_path_absolute() {
        let body = br#"<a href="/portal/session/default/login">Login</a>"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(
            s.contains(r#"href="/api/v1/network/captive/session/abc/portal/session/default/login""#),
            "path-absolute href must be prefixed; got: {s}"
        );
    }

    #[test]
    fn rewrite_body_html_src_and_action_path_absolute() {
        let body = br#"<img src="/img/a.png"><form action="/guest/submit">"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(
            r#"src="/api/v1/network/captive/session/abc/img/a.png""#
        ));
        assert!(s.contains(
            r#"action="/api/v1/network/captive/session/abc/guest/submit""#
        ));
    }

    #[test]
    fn rewrite_body_html_single_quoted_href() {
        let body = br#"<a href='/guest/x'>"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(
            s.contains(r#"href='/api/v1/network/captive/session/abc/guest/x'"#)
        );
    }

    #[test]
    fn rewrite_body_css_url_path_absolute() {
        // All three CSS url() forms:
        let body = br#"a{background:url("/img/a.png")}b{background:url('/img/b.png')}c{background:url(/img/c.png)}"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(s.contains(
            r#"url("/api/v1/network/captive/session/abc/img/a.png")"#
        ));
        assert!(s.contains(
            r#"url('/api/v1/network/captive/session/abc/img/b.png')"#
        ));
        assert!(
            s.contains("url(/api/v1/network/captive/session/abc/img/c.png)")
        );
    }

    // JS / JSON body rewrite is DELIBERATELY NOT supported
    // for this byte-scan primitive. Empirical test against a
    // real-world Redux-toolkit SPA (~520 KiB) produced ~150
    // corruptions and zero legitimate URL rewrites — every
    // match was a Redux action suffix
    // (`e+"/pending"` → broken thunk), a regex literal
    // (`p(/^…/)` → invalid regex), or a similar non-URL
    // context that a byte scan cannot distinguish from a
    // path-absolute URL.
    //
    // Portals whose SPA emits path-absolute URLs from JS
    // need an AST-aware transformation or an injected
    // `<base>` tag on the HTML index. Both are out of
    // scope here.
    //
    // The content-type gating test above
    // (`should_rewrite_body_html_and_css_yes_js_and_json_no`)
    // enforces the exclusion — the two positive JS/JSON
    // body-rewrite tests that lived here previously
    // encoded broken behaviour and have been removed.
    //
    // Regression assertion: rewrite_body is a no-op on JS
    // content because it is never reached (should_rewrite_body
    // gates the caller), but we assert here that even if a
    // caller wrongly hands JS bytes to rewrite_body, the
    // path-absolute case leaves regex-like patterns after
    // `(` untouched — this test guards the byte-scanner's
    // context matcher against a future edit that widens the
    // heuristic in a way that would corrupt real JS.
    // (Removed — the byte scanner still fires on `("/…` and
    // other JS-ambiguous contexts, so the correct
    // enforcement lives at the content-type gate, not the
    // scanner.)

    #[test]
    fn rewrite_body_protocol_relative_not_rewritten() {
        // `//host/foo` in HTML/CSS/JS must NOT be prefixed.
        let body = br#"<script src="//cdn.example/lib.js"></script>"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(
            out,
            body.to_vec(),
            "protocol-relative //host/foo must pass through untouched"
        );
    }

    #[test]
    fn rewrite_body_word_boundary_avoids_mid_string_slash() {
        // A `/` inside a JS string with a word-char BEFORE the
        // opening quote should NOT be treated as a URL start.
        // e.g. `foo"/bar"` (unusual but legal) — the `"` is
        // preceded by `o`, a word char, so we skip.
        let body = br#"const x=foo"/bar";"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        assert_eq!(
            out,
            body.to_vec(),
            "quote preceded by word-char must not trigger rewrite"
        );
    }

    #[test]
    fn rewrite_body_upstream_and_path_absolute_combined() {
        // Regression for the two-pass discipline: absolute
        // upstream-host swap AND path-absolute prefix both
        // fire in one body without stepping on each other.
        let body = br#"<a href="http://198.51.100.1:8080/full">FULL</a><a href="/rel/foo">REL</a>"#;
        let out = rewrite_body(
            body,
            "http://198.51.100.1:8080",
            "/api/v1/network/captive/session/abc",
        );
        let s = String::from_utf8_lossy(&out).to_string();
        assert!(
            s.contains(r#"href="/api/v1/network/captive/session/abc/full""#),
            "absolute host swap; got: {s}"
        );
        assert!(
            s.contains(r#"href="/api/v1/network/captive/session/abc/rel/foo""#),
            "path-absolute prefix; got: {s}"
        );
    }

    // --- Content-Encoding decode (portal-agnostic) ---

    fn gzip_bytes(plain: &[u8]) -> Vec<u8> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(plain).expect("gzip write");
        enc.finish().expect("gzip finish")
    }

    fn zlib_bytes(plain: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(plain).expect("zlib write");
        enc.finish().expect("zlib finish")
    }

    #[test]
    fn decode_gzip_content_encoding_yields_json() {
        // Empirically-observed portal failure mode: a config
        // JSON endpoint returned gzip bytes while the proxy
        // stripped Content-Encoding, so the SPA's
        // JSON.parse died at column 1. Decode must restore
        // plain JSON before handoff.
        let plain = br#"{"venue":"example-portal","ok":true}"#;
        let compressed = gzip_bytes(plain);
        assert_eq!(compressed[0], 0x1f);
        assert_eq!(compressed[1], 0x8b);
        let out = decode_upstream_body(compressed, Some("gzip"))
            .expect("gzip decode");
        assert_eq!(out, plain.to_vec());
    }

    #[test]
    fn decode_gzip_sniffed_without_content_encoding_header() {
        // Controllers that omit Content-Encoding but still
        // emit gzip magic must not poison the SPA.
        let plain = br#"{"packages":[]}"#;
        let compressed = gzip_bytes(plain);
        let out = decode_upstream_body(compressed, None).expect("sniff");
        assert_eq!(out, plain.to_vec());
    }

    #[test]
    fn decode_deflate_zlib_wrapped() {
        let plain = b"<html>coova</html>";
        let compressed = zlib_bytes(plain);
        let out = decode_upstream_body(compressed, Some("deflate"))
            .expect("deflate decode");
        assert_eq!(out, plain.to_vec());
    }

    #[test]
    fn decode_identity_passthrough() {
        let plain = br#"{"ok":true}"#;
        let out = decode_upstream_body(plain.to_vec(), Some("identity"))
            .expect("identity");
        assert_eq!(out, plain.to_vec());
        let out2 = decode_upstream_body(plain.to_vec(), None).expect("absent");
        assert_eq!(out2, plain.to_vec());
    }

    #[test]
    fn decode_brotli_refused() {
        // Accept-Encoding is stripped upstream so br should
        // not appear; if it does, fail closed rather than
        // hand the SPA opaque bytes.
        let err = decode_upstream_body(vec![0u8; 8], Some("br"))
            .expect_err("br must error");
        assert!(err.contains("unsupported Content-Encoding"), "got: {err}");
    }
}
