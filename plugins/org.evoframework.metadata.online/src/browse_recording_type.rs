// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `library.browse_by_recording_type` verb implementation.
//!
//! Composes MPD-side library enumeration with MB-side
//! reconciliation identity to filter albums by canonical
//! `recording_type` (Studio / Live / Compilation / Soundtrack /
//! EP / Single / Broadcast / Other).
//!
//! This is the fifth facet-browse verb — the other four
//! (artist / album / genre / year) live on the shelf's
//! co-tenant playback.mpd and read MPD directly. That plugin
//! doesn't have cross-plugin dispatch across the OOP boundary
//! yet, so this one enumerates MPD with its own minimal
//! client and reconciles via the same code path
//! `metadata.reconcile_release` uses.
//!
//! Flow:
//!
//! 1. Validate request shape (`v == 1`, non-empty
//!    `recording_type`).
//! 2. Enumerate every `(albumartist, album)` pair via
//!    [`MinimalMpd::enumerate_albums`].
//! 3. For each pair, reconcile via [`crate::reconcile::reconcile`]
//!    (which consults the persistent cache first, cold-cache
//!    hits MB at the shared 1 req/sec rate limit).
//! 4. Filter by requested recording_type; paginate the matches.
//! 5. Return the standard envelope with progress counters.

use evo_online_providers::MusicBrainzClient;
use serde::{Deserialize, Serialize};

use crate::cache::ReconcileCache;
use crate::mpd_client::{MinimalMpd, DEFAULT_MPD_ADDR};

/// Wire request shape.
#[derive(Debug, Deserialize)]
struct BrowseByRecordingTypeRequest {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    recording_type: Option<String>,
    #[serde(default)]
    page: Option<usize>,
    #[serde(default)]
    page_size: Option<usize>,
}

/// Wire response shape. Matches the operator-facing envelope
/// pattern (facet + entries + pagination + progress counters).
#[derive(Debug, Serialize)]
pub(crate) struct BrowseByRecordingTypeResponse {
    v: u8,
    status: ResponseStatus,
    facet: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_type: Option<String>,
    entries: Vec<FacetEntry>,
    page: usize,
    page_size: usize,
    total: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_page: Option<usize>,
    library_album_count: usize,
    reconciled_album_count: usize,
    reconcile_errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResponseStatus {
    Ok,
    BadRequest,
}

#[derive(Debug, Serialize)]
struct FacetEntry {
    artist: String,
    album: String,
    recording_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_release_year: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_mbid: Option<String>,
}

impl BrowseByRecordingTypeResponse {
    pub(crate) fn json_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// Server-side hard cap on the response's entry count.
/// Mirrors playback.mpd's [`BROWSE_HARD_CAP`] for consistent
/// operator-UI budgets across every browse verb.
const BROWSE_HARD_CAP: usize = 2_000;

/// Entry point invoked from the plugin's `handle_request`.
pub(crate) async fn browse_by_recording_type(
    payload: &[u8],
    mb: &MusicBrainzClient,
    cache: &ReconcileCache,
) -> Result<BrowseByRecordingTypeResponse, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|e| format!("payload is not UTF-8: {e}"))?;
    let req: BrowseByRecordingTypeRequest =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported request v: {}", req.v)));
    }
    let want_type = req
        .recording_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let want_type = match want_type {
        Some(t) => t.to_string(),
        None => {
            return Ok(bad_request(
                "recording_type is required and must be non-empty",
            ));
        }
    };
    let page = req.page.unwrap_or(0);
    let requested_size = req.page_size.unwrap_or(BROWSE_HARD_CAP);
    let page_size = requested_size.clamp(1, BROWSE_HARD_CAP);

    // Enumerate the library.
    let mut mpd = MinimalMpd::connect(DEFAULT_MPD_ADDR)
        .await
        .map_err(|e| format!("MPD connect failed: {e}"))?;
    let pairs = mpd
        .enumerate_albums()
        .await
        .map_err(|e| format!("MPD album enumeration failed: {e}"))?;
    let library_album_count = pairs.len();

    // Reconcile each pair. Positive + negative results memoise
    // in the persistent cache; cold-cache first call bounded by
    // MB's 1 req/sec × unreconciled count.
    let mut reconciled_count = 0usize;
    let mut reconcile_errors = 0usize;
    let mut matched: Vec<FacetEntry> = Vec::new();
    for (artist, album) in &pairs {
        let reconcile_payload = format!(
            "{{\"v\":1,\"artist\":{},\"album\":{}}}",
            json_str(artist),
            json_str(album),
        );
        let response = match crate::reconcile::reconcile(
            reconcile_payload.as_bytes(),
            mb,
            Some(cache),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                reconcile_errors += 1;
                continue;
            }
        };
        // The reconcile response is a serde-Serialize struct; go
        // through JSON to inspect status + canonical without
        // making its private types public here.
        let serialized = match response.json_bytes() {
            Ok(bytes) => bytes,
            Err(_) => {
                reconcile_errors += 1;
                continue;
            }
        };
        let value: serde_json::Value = match serde_json::from_slice(&serialized)
        {
            Ok(v) => v,
            Err(_) => {
                reconcile_errors += 1;
                continue;
            }
        };
        reconciled_count += 1;
        if value.get("status").and_then(|s| s.as_str()) != Some("ok") {
            continue;
        }
        let canonical = match value.get("canonical") {
            Some(c) if !c.is_null() => c,
            _ => continue,
        };
        let their_type = canonical
            .get("recording_type")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        if !their_type.eq_ignore_ascii_case(&want_type) {
            continue;
        }
        matched.push(FacetEntry {
            artist: canonical
                .get("artist")
                .and_then(|s| s.as_str())
                .unwrap_or(artist.as_str())
                .to_string(),
            album: canonical
                .get("album")
                .and_then(|s| s.as_str())
                .unwrap_or(album.as_str())
                .to_string(),
            recording_type: their_type.to_string(),
            first_release_year: canonical
                .get("first_release_year")
                .and_then(|v| v.as_u64())
                .and_then(|n| u16::try_from(n).ok()),
            release_mbid: canonical
                .get("release_mbid")
                .and_then(|s| s.as_str())
                .map(str::to_string),
        });
    }

    let total = matched.len();
    let range_start = page.saturating_mul(page_size);
    let range_end = range_start.saturating_add(page_size).min(total);
    let entries: Vec<FacetEntry> = if range_start >= total {
        Vec::new()
    } else {
        matched.drain(range_start..range_end).collect()
    };
    let more = range_end < total;
    let next_page = if more { Some(page + 1) } else { None };

    Ok(BrowseByRecordingTypeResponse {
        v: 1,
        status: ResponseStatus::Ok,
        facet: "recording_type",
        recording_type: Some(want_type),
        entries,
        page,
        page_size,
        total,
        truncated: more,
        next_page,
        library_album_count,
        reconciled_album_count: reconciled_count,
        reconcile_errors,
        detail: None,
    })
}

fn bad_request(detail: &str) -> BrowseByRecordingTypeResponse {
    BrowseByRecordingTypeResponse {
        v: 1,
        status: ResponseStatus::BadRequest,
        facet: "recording_type",
        recording_type: None,
        entries: Vec::new(),
        page: 0,
        page_size: 0,
        total: 0,
        truncated: false,
        next_page: None,
        library_album_count: 0,
        reconciled_album_count: 0,
        reconcile_errors: 0,
        detail: Some(detail.to_string()),
    }
}

/// Escape a string for embedding in a hand-written JSON literal.
/// Cheaper than round-tripping through serde_json::to_string for
/// two short strings per reconcile; matches the escaping the
/// reconcile module's own payload parser handles.
fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_request_on_missing_recording_type() {
        let payload = br#"{"v":1}"#;
        let http = evo_online_providers::build_http_client(
            std::time::Duration::from_secs(5),
        );
        let rate = std::sync::Arc::new(evo_online_providers::RateLimiter::new(
            std::time::Duration::from_nanos(0),
        ));
        let mb = MusicBrainzClient::new(http, rate, "test/1.0");
        let tmp = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            tmp.path().to_path_buf(),
            std::time::Duration::from_secs(60),
        );
        let resp = browse_by_recording_type(payload, &mb, &cache)
            .await
            .unwrap();
        assert_eq!(resp.status, ResponseStatus::BadRequest);
    }

    #[tokio::test]
    async fn bad_request_on_wrong_version() {
        let payload = br#"{"v":9,"recording_type":"Studio"}"#;
        let http = evo_online_providers::build_http_client(
            std::time::Duration::from_secs(5),
        );
        let rate = std::sync::Arc::new(evo_online_providers::RateLimiter::new(
            std::time::Duration::from_nanos(0),
        ));
        let mb = MusicBrainzClient::new(http, rate, "test/1.0");
        let tmp = tempfile::tempdir().unwrap();
        let cache = ReconcileCache::new(
            tmp.path().to_path_buf(),
            std::time::Duration::from_secs(60),
        );
        let resp = browse_by_recording_type(payload, &mb, &cache)
            .await
            .unwrap();
        assert_eq!(resp.status, ResponseStatus::BadRequest);
    }

    #[test]
    fn json_str_escapes_quotes_and_backslashes() {
        assert_eq!(json_str("Radiohead"), r#""Radiohead""#);
        assert_eq!(json_str(r#"AC"DC"#), r#""AC\"DC""#);
        assert_eq!(json_str(r#"A\B"#), r#""A\\B""#);
    }
}
