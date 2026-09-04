// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Composite track-detail HTTPS endpoint.
//!
//! Mounts `GET /api/v1/audio/track_detail` on the framework's
//! wire-op dispatcher. Takes a target (`scheme=mpd-path` +
//! `value=<path>`), dispatches the sub-sources that make a
//! complete track view — local metadata, artwork resolution,
//! MusicBrainz reconciliation, LRCLIB lyrics, Last.fm bio +
//! album notes — and returns one aggregated JSON envelope
//! where each sub-source carries its own `{status,
//! provider_id, payload, detail}`.
//!
//! ## Honest partial results
//!
//! Any sub-source failure surfaces on its own `status` field.
//! A missing Last.fm API key → bio + notes come back
//! `not_configured`. LRCLIB has no lyrics → `not_found`.
//! Reconciliation misses (album not in MB) → `not_found`.
//! Local metadata read error → the sub-source carries the
//! error, but the top-level status still returns 200 with
//! whatever else succeeded. The endpoint never fabricates
//! results and never fails the whole aggregation because
//! one sub-source is dark.
//!
//! ## Composition happens framework-side
//!
//! This endpoint reaches every plugin through the framework's
//! `Dispatcher` — the same primitive `artwork_resolve_endpoint`
//! uses. Plugin-side cross-plugin dispatch across the OOP
//! boundary is not available until the wire protocol propagates
//! `ShelfRequestDispatcher`; framework-side dispatch has no such
//! constraint (the endpoint holds the dispatcher directly).
//!
//! Sub-dispatches run in parallel via `tokio::join!` — the
//! whole composite completes in the slowest single
//! sub-dispatch, not the sum.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use evo_auth_bearer::{BearerTokenValidator, CapabilitySet};
use evo_projection_core::{CapabilityRequirement, WireOpId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::artwork_cascade::{ArtworkCascade, CascadeOutcome};
use evo_runtime_http::auth_tier::AuthTierProvider;
use evo_runtime_http::dispatcher::Dispatcher;
use evo_runtime_http::middleware::{capability_gate, AuthLayer};
use evo_runtime_http::principal::Principal;

/// Mount the composite endpoint under `/api/v1/audio/track_detail`.
#[allow(clippy::too_many_arguments)]
pub fn attach_track_detail_endpoint(
    router: Router,
    api_prefix: &str,
    dispatcher: Arc<dyn Dispatcher>,
    artwork_cascade: Arc<ArtworkCascade>,
    validator: Arc<BearerTokenValidator>,
    tier_provider: Arc<dyn AuthTierProvider>,
    lan_trust_caps: CapabilitySet,
) -> Result<Router, evo_runtime_http::error::RuntimeHttpError> {
    let path = format!("{api_prefix}/audio/track_detail");
    let requirement = CapabilityRequirement::read("audio");
    let op_id = WireOpId::new("audio_track_detail").map_err(|e| {
        evo_runtime_http::error::RuntimeHttpError::EndpointAttachRefused {
            endpoint: "audio_track_detail".into(),
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
        artwork_cascade,
        api_prefix: api_prefix.to_string(),
    };
    Ok(router.route(
        &path,
        get(handle_track_detail).with_state(state).route_layer(
            axum::middleware::from_fn_with_state(auth, capability_gate),
        ),
    ))
}

#[derive(Clone)]
struct HandlerState {
    dispatcher: Arc<dyn Dispatcher>,
    /// Shared cascade primitive — same instance the standalone
    /// resolve endpoint holds. Guarantees the composite and the
    /// standalone return the same content_hash for the same
    /// target: one negative memo, one coalescer, one admission
    /// bucket, one local→identity-synth→online tier chain.
    artwork_cascade: Arc<ArtworkCascade>,
    api_prefix: String,
}

#[derive(Debug, Deserialize)]
struct TrackDetailQuery {
    /// Currently only `mpd-path` is accepted (the operator UI's
    /// primary target shape for now-playing / queue rows /
    /// track detail screens). mpd-album target is a follow-on
    /// (album-level aggregation is piece 7's second half if
    /// scoped in).
    scheme: String,
    value: String,
}

async fn handle_track_detail(
    State(state): State<HandlerState>,
    axum::Extension(principal): axum::Extension<Principal>,
    headers: axum::http::HeaderMap,
    Query(query): Query<TrackDetailQuery>,
) -> Response {
    if query.scheme != "mpd-path" {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported scheme {:?}; only mpd-path is currently accepted",
                query.scheme
            ),
        )
            .into_response();
    }
    if query.value.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "value must be non-empty")
            .into_response();
    }

    // Resolve the operator UI locale from the request's
    // `Accept-Language` header and thread it into every
    // enrichment sub-dispatch. Absent header → `"en"` (default),
    // matching the plugin-side `locale::normalise(None)` default.
    // The composite carries the operator locale as a first-class
    // request property so the plugin never has to guess.
    let operator_locale = accept_language_primary(&headers);

    // Step 1: fetch local metadata for source-format + tags.
    // Every downstream sub-source keys on the artist / album /
    // track / duration this returns.
    let metadata =
        fetch_metadata_local(&state.dispatcher, &principal, &query.value).await;

    // Extract identity for the sub-dispatches. When metadata
    // fails, the composite still returns 200 with metadata's
    // sub-status carrying the failure — every downstream
    // sub-source that needs identity structurally-not-found
    // themselves.
    let (artist, album, track_title, duration_seconds) =
        extract_identity_from_metadata(&metadata.payload);

    // Step 2 — Wave 1: sub-dispatches that do not depend on
    // reconciliation identity. Runs in parallel — no barrier on
    // reconciliation itself: they succeed or structurally-miss
    // independently.
    //
    // Bio + notes are deliberately NOT in this wave. Both benefit
    // from the reconciled MBIDs (artist_mbid for bio, release_mbid
    // for notes) — passing the MBID short-circuits the plugin's
    // fuzzy-search reconciliation on the same pair and, critically,
    // disambiguates entities whose names collide with common
    // English words (Passenger the musician vs the transport noun,
    // Bush the band vs the shrub, etc.). Firing bio/notes without
    // the MBID has the plugin fall back to a bare-name Wikipedia
    // title search on a miss — which lands on the common-noun
    // article. Wave 2 below fixes that by threading the MBIDs
    // through once reconciliation completes.
    let (artwork_sub, reconciliation_sub, lyrics_sub) = tokio::join!(
        resolve_artwork(
            &state.artwork_cascade,
            &principal,
            &state.api_prefix,
            &query.value,
        ),
        reconcile_release(
            &state.dispatcher,
            &principal,
            artist.as_deref(),
            album.as_deref(),
        ),
        query_lyrics(
            &state.dispatcher,
            &principal,
            artist.as_deref(),
            track_title.as_deref(),
            album.as_deref(),
            duration_seconds,
            &operator_locale,
        ),
    );

    // Extract the reconciled MBIDs from Wave 1's reconciliation
    // sub-source. Reconciliation may have missed / errored — in
    // which case both MBIDs are None and Wave 2 dispatches with
    // name-only (the pre-existing fallback path). The plugin-side
    // cascade defends against the common-noun trap by refusing a
    // bare-name Wikipedia fallback on artist-type entities without
    // an MBID.
    let (reconciled_artist_mbid, reconciled_release_mbid) =
        extract_reconciled_mbids(&reconciliation_sub);

    // Step 2 — Wave 2: bio + notes, in parallel, with the
    // reconciled MBIDs threaded through. Wave 1's other sub-sources
    // are already resolved; only these two are outstanding.
    let (bio_sub, notes_sub) = tokio::join!(
        query_bio(
            &state.dispatcher,
            &principal,
            artist.as_deref(),
            reconciled_artist_mbid.as_deref(),
            &operator_locale,
        ),
        query_album_notes(
            &state.dispatcher,
            &principal,
            artist.as_deref(),
            album.as_deref(),
            reconciled_release_mbid.as_deref(),
            &operator_locale,
        ),
    );

    let body = json!({
        "v": 1,
        "status": "ok",
        "target": {
            "scheme": &query.scheme,
            "value": &query.value,
        },
        "sources": {
            "metadata_local": metadata.into_json(),
            "artwork": artwork_sub.into_json(),
            "reconciliation": reconciliation_sub.into_json(),
            "lyrics": lyrics_sub.into_json(),
            "artist_bio": bio_sub.into_json(),
            "album_notes": notes_sub.into_json(),
        },
    });

    (StatusCode::OK, Json(body)).into_response()
}

// -----------------------------------------------------------
// Sub-source shape
// -----------------------------------------------------------

/// Every sub-source returns this shape. The endpoint composes
/// N of these into the top-level response body.
///
/// Cascade-aware sub-sources (bio / notes / release-credits /
/// track-annotation / work-notes) additionally carry
/// `privacy_class`, `attribution`, and `enhancement` — the
/// keyless-first cascade's operator-visible provenance +
/// uplift hints. Non-cascade sub-sources (local metadata,
/// artwork, reconciliation, lyrics) leave those fields
/// `None` and they are omitted from the JSON.
struct SubSource {
    status: &'static str,
    provider_id: Option<String>,
    payload: Option<Value>,
    detail: Option<String>,
    /// Winning provider's privacy class — `"anonymous"` or
    /// `"identity_bearing"`. Cascade sub-sources only.
    privacy_class: Option<String>,
    /// Attribution object `{ source_name, source_url, license }`
    /// the operator UI MUST render alongside a non-empty
    /// payload (Wikipedia is CC BY-SA, MusicBrainz + Wikidata
    /// are CC0, identity-bearing providers carry their terms).
    attribution: Option<Value>,
    /// Enhancement hint `{ provider, requires_key, reason }`
    /// pointing at a provider the operator could enable
    /// (or supply a key for) to enrich the answer further.
    enhancement: Option<Value>,
    /// Language actually served for this sub-source's top-level
    /// payload — BCP47 short (`"en"`, `"de"`, ...).     /// the operator UI can
    /// annotate the pane ("bio served in English — no German
    /// article available") without walking `sources[]`.
    language: Option<String>,
    /// Every provider that returned non-empty content for the
    /// underlying cascade verb, ordered by the operator's
    /// per-source priority (highest first). One entry per
    /// contributing provider — carries the full
    /// `{ provider_id, privacy_class, payload, attribution }`
    /// object the plugin emits. This is the operator UI's
    /// selection surface: the top-level `provider_id` /
    /// `payload` / `attribution` mirror `sources[0]` for the
    /// single-value default view; the per-source selection
    /// affordance reads `sources` directly and lets the
    /// operator switch or reorder without a plugin restart.
    ///
    /// Empty on non-cascade sub-sources and on cascade
    /// sub-sources whose status is not `"ok"`. Passthrough,
    /// not a projection — track_detail carries the full
    /// envelope the cascade emits.
    sources: Vec<Value>,
}

impl SubSource {
    fn ok(provider_id: Option<String>, payload: Option<Value>) -> Self {
        Self {
            status: "ok",
            provider_id,
            payload,
            detail: None,
            privacy_class: None,
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        }
    }
    fn not_found(provider_id: Option<String>, detail: String) -> Self {
        Self {
            status: "not_found",
            provider_id,
            payload: None,
            detail: Some(detail),
            privacy_class: None,
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        }
    }
    fn not_configured(detail: String) -> Self {
        Self {
            status: "not_configured",
            provider_id: None,
            payload: None,
            detail: Some(detail),
            privacy_class: None,
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        }
    }
    fn error(detail: String) -> Self {
        Self {
            status: "error",
            provider_id: None,
            payload: None,
            detail: Some(detail),
            privacy_class: None,
            attribution: None,
            language: None,
            enhancement: None,
            sources: Vec::new(),
        }
    }
    /// Attach the cascade envelope fields (privacy_class /
    /// attribution / enhancement / sources) to this sub-source.
    /// Called by the cascade-aware classifier after extracting
    /// each field from the plugin's response.
    ///
    /// `sources` is the operator UI's per-source selection
    /// surface — passthrough of every content-bearing entry
    /// the cascade emitted. track_detail carries the FULL
    /// cascade envelope, not a lossy projection: the top-level
    /// `provider_id` / `payload` / `attribution` are the
    /// single-value default view (`sources[0]` mirror);
    /// `sources` is the source of truth for the per-source
    /// selection surface.
    fn with_cascade_metadata(
        mut self,
        privacy_class: Option<String>,
        attribution: Option<Value>,
        enhancement: Option<Value>,
        sources: Vec<Value>,
        language: Option<String>,
    ) -> Self {
        self.privacy_class = privacy_class;
        self.attribution = attribution;
        self.enhancement = enhancement;
        self.sources = sources;
        self.language = language;
        self
    }
    fn into_json(self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("status".into(), Value::String(self.status.to_string()));
        if let Some(pid) = self.provider_id {
            m.insert("provider_id".into(), Value::String(pid));
        }
        if let Some(pc) = self.privacy_class {
            m.insert("privacy_class".into(), Value::String(pc));
        }
        if let Some(p) = self.payload {
            m.insert("payload".into(), p);
        }
        if let Some(d) = self.detail {
            m.insert("detail".into(), Value::String(d));
        }
        if let Some(a) = self.attribution {
            m.insert("attribution".into(), a);
        }
        if let Some(e) = self.enhancement {
            m.insert("enhancement".into(), e);
        }
        if let Some(lang) = self.language {
            m.insert("language".into(), Value::String(lang));
        }
        // sources[] is always emitted for cascade sub-sources
        // (even when empty on non-ok statuses) so the UI's
        // per-source selection surface has a stable shape to
        // decode. Non-cascade sub-sources leave the vector
        // empty; the endpoint's field-omission convention for
        // empty collections is preserved via
        // `skip_serializing_if` semantics — an empty vec is
        // omitted, a populated vec is emitted.
        if !self.sources.is_empty() {
            m.insert("sources".into(), Value::Array(self.sources));
        }
        Value::Object(m)
    }
}

// -----------------------------------------------------------
// Locale helper (operator-locale extraction)
// -----------------------------------------------------------

/// Extract the operator's primary UI locale from the request's
/// `Accept-Language` header. Contract: the operator-
/// facing UI's i18n locale IS the metadata locale, and it
/// travels to the enrichment cascade as a first-class request
/// property. The parsed tag is a BCP47 short (`"en"`, `"de"`);
/// region subtags and unrecognisable input fall through to
/// `"en"` so the plugin's `locale::normalise` default matches.
///
/// This is deliberately a light parser — HTTP `Accept-Language`
/// supports q-values and multiple candidates (`de,en;q=0.8`),
/// but for metadata prose the operator's first choice is what
/// matters. Q-value negotiation would only surface if the
/// operator listed two languages and TheAudioDB carried both;
/// the fallback chain in every provider already covers that
/// case operator-locale → English → any.
fn accept_language_primary(headers: &axum::http::HeaderMap) -> String {
    let Some(raw) = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
    else {
        return "en".into();
    };
    let first = raw.split(',').next().unwrap_or("").trim();
    // Strip any q-value trailer just in case the caller sent
    // `de;q=0.9` as the first token.
    let candidate = first.split(';').next().unwrap_or(first).trim();
    if candidate.is_empty() {
        return "en".into();
    }
    // Same normalisation the plugin applies: primary tag only,
    // lowercased.
    let primary = candidate
        .split(['-', '_'])
        .next()
        .unwrap_or(candidate)
        .to_ascii_lowercase();
    if primary.len() >= 2
        && primary.len() <= 3
        && primary.chars().all(|c| c.is_ascii_alphabetic())
    {
        primary
    } else {
        "en".into()
    }
}

// -----------------------------------------------------------
// Dispatch helpers
// -----------------------------------------------------------

/// One shelf dispatch through the framework's wire-op layer.
/// Returns the plugin's JSON response payload on success, or
/// an error string on wire / envelope failure.
async fn dispatch_shelf(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    shelf: &str,
    request_type: &str,
    payload: Value,
) -> Result<Value, String> {
    use base64::Engine;
    let payload_b64 = base64::engine::general_purpose::STANDARD
        .encode(payload.to_string().as_bytes());
    let request_envelope = json!({
        "shelf": shelf,
        "request_type": request_type,
        "payload_b64": payload_b64,
    });
    let op_id = WireOpId::new("request")
        .map_err(|e| format!("wire-op id construction failed: {e}"))?;
    let envelope = dispatcher
        .dispatch(&op_id, request_envelope, principal)
        .await
        .map_err(|e| format!("dispatch failed: {e}"))?;
    // Discriminate the envelope's success shape from its Error
    // shape BEFORE demanding `payload_b64`. Successful plugin
    // responses carry `{"payload_b64": "<b64>"}`; error responses
    // carry `{"error": {...}}` (framework wrap of PluginError).
    // The prior implementation only checked for `payload_b64`
    // and emitted "response missing payload_b64" on every error
    // response — indistinguishable to the caller from an actual
    // half-landed wire codec regression, which the deploy smoke
    // gate treats as blocking. Surface the plugin's error
    // directly so external-service transients (upstream 503s,
    // rate-limits) do not mask as substrate regressions.
    let map = envelope.as_object().ok_or_else(|| {
        format!(
            "response is not a JSON object (shelf={shelf}, \
             request_type={request_type}, value={envelope})"
        )
    })?;
    if let Some(err_value) = map.get("error") {
        return Err(format!("plugin error: {err_value}"));
    }
    let inner_b64 = map
        .get("payload_b64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            format!(
                "response missing payload_b64 field (shelf={shelf}, \
                 request_type={request_type}, envelope={envelope})"
            )
        })?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(inner_b64)
        .map_err(|e| format!("payload_b64 decode: {e}"))?;
    serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| format!("plugin response JSON parse: {e}"))
}

// -----------------------------------------------------------
// metadata.local (metadata.query on metadata.providers)
// -----------------------------------------------------------

async fn fetch_metadata_local(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    mpd_path: &str,
) -> SubSource {
    let payload = json!({
        "v": 1,
        "target": {"scheme": "mpd-path", "value": mpd_path},
    });
    match dispatch_shelf(
        dispatcher,
        principal,
        "metadata.providers",
        "metadata.query",
        payload,
    )
    .await
    {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            // Prefer the plugin's additive `provider_id`
            // (`file_tags` / `mpd_lsinfo` / `mpd_queue_tags`);
            // fall back to the shelf name for older plugins.
            let provider_id = v
                .get("provider_id")
                .and_then(|p| p.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| Some("metadata_local".to_string()));
            match status {
                "ok" => SubSource::ok(provider_id, Some(v)),
                "not_found" => {
                    let detail = v
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .unwrap_or("metadata.local returned not_found")
                        .to_string();
                    SubSource::not_found(provider_id, detail)
                }
                other => SubSource::error(format!(
                    "metadata.local returned status={other:?}: {v}"
                )),
            }
        }
        Err(e) => SubSource::error(format!("metadata.local dispatch: {e}")),
    }
}

/// Extract the identity fields the downstream sub-sources
/// need from the metadata.local response.
fn extract_identity_from_metadata(
    metadata_payload: &Option<Value>,
) -> (Option<String>, Option<String>, Option<String>, Option<f64>) {
    let Some(v) = metadata_payload else {
        return (None, None, None, None);
    };
    let get_str = |k: &str| -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(str::to_string)
    };
    let artist = get_str("artist");
    let album = get_str("album");
    let track_title = get_str("title");
    // Top-level `duration_ms` is always present on ok responses;
    // nested `file.duration_ms` is stripped under the default
    // `standard` profile — prefer top-level, fall back to file.
    let duration_seconds = v
        .get("duration_ms")
        .and_then(|d| d.as_u64())
        .or_else(|| {
            v.get("file")
                .and_then(|f| f.get("duration_ms"))
                .and_then(|d| d.as_u64())
        })
        .map(|ms| ms as f64 / 1000.0);
    (artist, album, track_title, duration_seconds)
}

/// Extract `(artist_mbid, release_mbid)` from a completed
/// reconciliation sub-source so Wave 2 (bio + notes) can thread
/// the canonical MBIDs through to the plugin cascades. Returns
/// `(None, None)` when reconciliation missed, errored, or
/// short-circuited on identity: Wave 2 then dispatches with the
/// name-only fallback path.
///
/// The reconciliation classifier wraps its response as
/// `payload.canonical.{artist_mbid, release_mbid, ...}` — this
/// helper pulls the two MBIDs out of that shape.
fn extract_reconciled_mbids(
    reconciliation_sub: &SubSource,
) -> (Option<String>, Option<String>) {
    let Some(payload) = reconciliation_sub.payload.as_ref() else {
        return (None, None);
    };
    let canonical = payload.get("canonical");
    let get_str = |field: &str| -> Option<String> {
        canonical
            .and_then(|c| c.get(field))
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    };
    (get_str("artist_mbid"), get_str("release_mbid"))
}

// -----------------------------------------------------------
// Artwork sub-dispatch
// -----------------------------------------------------------

/// Delegate to the shared cascade so the composite endpoint
/// runs through the SAME resolution path as the standalone
/// `/api/v1/audio/artwork` endpoint. This is the load-bearing
/// property: both surfaces see the same negative memo, the same
/// coalescer, the same admission bucket, and — critically — the
/// same content_hash for the same target. A track that resolves
/// to online-only artwork (Cover Art Archive / Last.fm / iTunes)
/// on the standalone endpoint MUST resolve identically here.
///
/// Size defaults to `medium` per the composite endpoint's
/// list-safe contract; callers wanting `original` fetch the
/// standalone hash endpoint directly at the URL this returns.
async fn resolve_artwork(
    cascade: &Arc<ArtworkCascade>,
    principal: &Principal,
    api_prefix: &str,
    mpd_path: &str,
) -> SubSource {
    match cascade
        .resolve(principal, "mpd-path", mpd_path, "medium")
        .await
    {
        CascadeOutcome::Resolved {
            content_hash,
            provider_id,
        } => {
            let url = format!("{api_prefix}/audio/artwork/{content_hash}");
            SubSource::ok(
                provider_id,
                Some(json!({
                    "content_hash": content_hash,
                    "url": url,
                    "size": "medium",
                })),
            )
        }
        CascadeOutcome::NotFound {
            provider_id,
            detail,
        } => SubSource::not_found(provider_id, detail),
        CascadeOutcome::BadRequest { detail } => SubSource::error(detail),
        CascadeOutcome::AdmissionDeadline { detail } => {
            SubSource::error(detail)
        }
        CascadeOutcome::CoalescerDeadline { detail } => {
            SubSource::error(detail)
        }
        CascadeOutcome::Transient { detail } => SubSource::error(detail),
    }
}

// -----------------------------------------------------------
// Reconciliation sub-dispatch
// -----------------------------------------------------------

async fn reconcile_release(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    artist: Option<&str>,
    album: Option<&str>,
) -> SubSource {
    let (Some(a), Some(b)) = (artist, album) else {
        return SubSource::not_found(
            None,
            "metadata_local did not supply artist + album; \
             cannot reconcile"
                .to_string(),
        );
    };
    let payload = json!({"v": 1, "artist": a, "album": b});
    match dispatch_shelf(
        dispatcher,
        principal,
        "metadata.providers",
        "metadata.reconcile_release",
        payload,
    )
    .await
    {
        Ok(v) => classify_status_wrapped_response(v, "musicbrainz"),
        Err(e) => SubSource::error(format!("reconcile dispatch: {e}")),
    }
}

// -----------------------------------------------------------
// Lyrics sub-dispatch (LRCLIB)
// -----------------------------------------------------------

async fn query_lyrics(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    artist: Option<&str>,
    track: Option<&str>,
    album: Option<&str>,
    duration_seconds: Option<f64>,
    operator_locale: &str,
) -> SubSource {
    let (Some(a), Some(t)) = (artist, track) else {
        return SubSource::not_found(
            None,
            "metadata_local did not supply artist + track; \
             cannot query lyrics"
                .to_string(),
        );
    };
    let mut payload =
        json!({"v": 1, "artist": a, "track": t, "locale": operator_locale});
    if let Some(alb) = album {
        payload["album"] = Value::String(alb.to_string());
    }
    if let Some(dur) = duration_seconds {
        payload["duration_seconds"] = serde_json::Number::from_f64(dur)
            .map(Value::Number)
            .unwrap_or(Value::Null);
    }
    match dispatch_shelf(
        dispatcher,
        principal,
        "metadata.providers",
        "metadata.query_lyrics",
        payload,
    )
    .await
    {
        Ok(v) => classify_enrichment_response(v, "lrclib"),
        Err(e) => SubSource::error(format!("lyrics dispatch: {e}")),
    }
}

// -----------------------------------------------------------
// Bio sub-dispatch (Last.fm)
// -----------------------------------------------------------

async fn query_bio(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    artist: Option<&str>,
    artist_mbid: Option<&str>,
    operator_locale: &str,
) -> SubSource {
    let Some(a) = artist else {
        return SubSource::not_found(
            None,
            "metadata_local did not supply artist; cannot query bio"
                .to_string(),
        );
    };
    // Thread the reconciled MBID through so the plugin cascade
    // fetches Wikipedia via MB's `wikipedia` url-rel on the
    // canonical entity — the ONLY way to disambiguate entities
    // whose names collide with common English words. Without the
    // MBID, the plugin's fuzzy `search_artist` reconciliation
    // needs ≥85% confidence to route the URL, and its bare-name
    // Wikipedia fallback lands on the common-noun article
    // (Passenger, Bush, Cake, Air, Live, Yes, Blur, …).
    let mut payload = json!({"v": 1, "artist": a, "locale": operator_locale});
    if let Some(mbid) = artist_mbid {
        if !mbid.trim().is_empty() {
            payload["artist_mbid"] = Value::String(mbid.to_string());
        }
    }
    match dispatch_shelf(
        dispatcher,
        principal,
        "metadata.providers",
        "metadata.query_artist_bio",
        payload,
    )
    .await
    {
        // Cascade-shape response: anonymous baseline (MB →
        // Wikipedia → Wikidata) with Last.fm as the identity-
        // bearing enhancement. Default provider on an otherwise
        // provider-less "ok" is Wikipedia since that is the
        // cascade's primary anonymous bio surface.
        Ok(v) => classify_cascade_response(v, "wikipedia"),
        Err(e) => SubSource::error(format!("bio dispatch: {e}")),
    }
}

// -----------------------------------------------------------
// Album notes sub-dispatch (Last.fm)
// -----------------------------------------------------------

async fn query_album_notes(
    dispatcher: &Arc<dyn Dispatcher>,
    principal: &Principal,
    artist: Option<&str>,
    album: Option<&str>,
    release_mbid: Option<&str>,
    operator_locale: &str,
) -> SubSource {
    let (Some(a), Some(b)) = (artist, album) else {
        return SubSource::not_found(
            None,
            "metadata_local did not supply artist + album; \
             cannot query album notes"
                .to_string(),
        );
    };
    // Same MBID-threading rationale as bio: pass the reconciled
    // release_mbid so the plugin's album-notes cascade uses the
    // canonical entity instead of a fuzzy title search.
    let mut payload =
        json!({"v": 1, "artist": a, "album": b, "locale": operator_locale});
    if let Some(mbid) = release_mbid {
        if !mbid.trim().is_empty() {
            payload["release_mbid"] = Value::String(mbid.to_string());
        }
    }
    match dispatch_shelf(
        dispatcher,
        principal,
        "metadata.providers",
        "metadata.query_album_notes",
        payload,
    )
    .await
    {
        // Cascade-shape response: Wikipedia album page as the
        // anonymous baseline, Last.fm as the identity-bearing
        // enhancement. Wikipedia is the default on a provider-
        // less "ok" — it wins for most mainstream albums.
        Ok(v) => classify_cascade_response(v, "wikipedia"),
        Err(e) => SubSource::error(format!("album notes dispatch: {e}")),
    }
}

// -----------------------------------------------------------
// Shared classifiers
// -----------------------------------------------------------

/// Classifier for the pre-cascade enrichment verbs (lyrics —
/// the one remaining verb that has not moved to the cascade
/// shape). Ignores the new `privacy_class` / `attribution` /
/// `enhancement` fields even when present; those are surfaced
/// only by the cascade-aware classifier below.
fn classify_enrichment_response(v: Value, default_provider: &str) -> SubSource {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    match status {
        "ok" => SubSource::ok(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .or_else(|| Some(default_provider.to_string())),
            v.get("payload").cloned(),
        ),
        "not_found" => SubSource::not_found(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string),
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("provider returned not_found")
                .to_string(),
        ),
        "not_configured" => SubSource::not_configured(
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("provider not configured on this device")
                .to_string(),
        ),
        "bad_request" => SubSource::error(
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("bad_request from provider")
                .to_string(),
        ),
        other => {
            SubSource::error(format!("unknown provider status: {other:?}"))
        }
    }
}

/// Classifier for the cascade-shape enrichment verbs
/// (bio / album-notes / release-credits / track-annotation /
/// work-notes). Surfaces the cascade's `privacy_class`,
/// `attribution`, and `enhancement` alongside the base fields
/// so the operator UI can render provenance and uplift
/// affordances.
///
/// `not_configured` still surfaces on genuine
/// no-provider-enabled paths; on those, `privacy_class` is
/// absent (no provider was tried) and the `enhancement` hint
/// carries any uplift the operator could enable.
fn classify_cascade_response(v: Value, default_provider: &str) -> SubSource {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    let privacy_class = v
        .get("privacy_class")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let attribution = v.get("attribution").cloned();
    let enhancement = v.get("enhancement").cloned();
    // The cascade emits `language` at the top level
    // (mirror of `sources[0].language`). Passed through to
    // the UI so it can label the top-level prose without
    // walking sources[].
    let language = v
        .get("language")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    // Passthrough — the cascade emits `sources: Vec<SourceEntry>`
    // (one entry per contributing provider, ordered by operator
    // priority) as the per-source selection surface. track_detail
    // carries the full envelope; the UI's selection affordance
    // reads this vector directly and gets the same shape it
    // would see calling the verb over WS.
    let sources: Vec<Value> = v
        .get("sources")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let base = match status {
        "ok" => SubSource::ok(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .or_else(|| Some(default_provider.to_string())),
            v.get("payload").cloned(),
        ),
        "not_found" => SubSource::not_found(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string),
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or(
                    "cascade returned not_found across enabled providers",
                )
                .to_string(),
        ),
        "not_configured" => SubSource::not_configured(
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("no cascade provider enabled on this device")
                .to_string(),
        ),
        "bad_request" => SubSource::error(
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("bad_request from cascade")
                .to_string(),
        ),
        other => SubSource::error(format!("unknown cascade status: {other:?}")),
    };
    base.with_cascade_metadata(
        privacy_class,
        attribution,
        enhancement,
        sources,
        language,
    )
}

/// Classifier for the reconciliation verb — it puts the
/// canonical object at the top level (not under `payload`),
/// so the extractor is slightly different from the enrichment
/// verbs.
fn classify_status_wrapped_response(
    v: Value,
    default_provider: &str,
) -> SubSource {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
    match status {
        "ok" => SubSource::ok(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
                .or_else(|| Some(default_provider.to_string())),
            Some(json!({
                "canonical": v.get("canonical"),
                "confidence_percent": v.get("confidence_percent"),
            })),
        ),
        "not_found" => SubSource::not_found(
            v.get("provider_id")
                .and_then(|s| s.as_str())
                .map(str::to_string),
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("reconciliation returned not_found")
                .to_string(),
        ),
        "bad_request" => SubSource::error(
            v.get("detail")
                .and_then(|d| d.as_str())
                .unwrap_or("bad_request from reconciliation")
                .to_string(),
        ),
        other => SubSource::error(format!(
            "unknown reconciliation status: {other:?}"
        )),
    }
}

// Silence unused import warnings when the aggregation shape
// changes.
#[allow(dead_code)]
#[derive(Serialize)]
struct _KeepSerializeLive;
