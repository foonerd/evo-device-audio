//! `artwork.resolve_online` request handler.
//!
//! Walks the online provider cascade (CAA → Last.fm → iTunes →
//! Volumio meta), retrieves bytes from the first provider hit,
//! transcodes via the shared pipeline, pushes the result to
//! the framework's asset cache, and returns the content_hash on
//! the wire. The response shape mirrors `artwork.resolve` from
//! the local plugin so consumers can use identical decoder
//! logic across the two cascade tiers.

use evo_device_audio_shared::transcode::{
    transcode, ArtworkSize, TranscodedArtwork,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::PluginConfig;
use crate::providers;

/// Request payload (JSON).
#[derive(Debug, Deserialize)]
pub(crate) struct ResolveOnlineRequest {
    /// Schema version. `1` is the only value accepted.
    pub(crate) v: u8,
    /// Subject identifier mirroring `ExternalAddressing`. The
    /// online cascade resolves the `mpd-album` scheme — the
    /// compound `"{artist}|{album}"` value the playback warden
    /// emits. The `mpd-path` scheme is refused with a
    /// structured `unsupported` response: per-track artwork is
    /// the local plugin's domain (file sidecar / embedded
    /// extraction). Online providers index by album, not by
    /// individual file path.
    pub(crate) target: ResolveTarget,
    /// Optional size variant. `tiny | medium | large | original`;
    /// defaults to `original` when absent.
    #[serde(default)]
    pub(crate) size: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResolveTarget {
    pub(crate) scheme: String,
    pub(crate) value: String,
}

/// Response payload (JSON). Mirrors `artwork.local`'s response
/// shape so UI consumers can use one decoder for both cascade
/// tiers; the `provider_id` field is the online-cascade-specific
/// addition surfacing which upstream resolved the artwork.
#[derive(Debug, Serialize)]
pub(crate) struct ResolveOnlineResponse {
    v: u8,
    status: ResponseStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
    /// Which provider in the cascade returned the bytes. Set on
    /// `status = "ok"`. Stable identifiers:
    /// `cover_art_archive`, `lastfm`, `itunes`, `volumio_meta`.
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseStatus {
    Ok,
    NotFound,
    Unsupported,
    BadRequest,
}

impl ResolveOnlineResponse {
    pub(crate) fn json_bytes(self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self)
    }
}

/// Output of [`resolve_artwork`]. Carries the wire response +
/// optional `(content_hash, bytes)` pair to push to the asset
/// cache. The split keeps the synchronous transcode in
/// spawn_blocking while the async cache write runs in the outer
/// async context.
pub(crate) struct ResolveOutput {
    pub(crate) response: ResolveOnlineResponse,
    pub(crate) cache_payload: Option<(String, Vec<u8>)>,
}

/// Entry point.
///
/// Validates the request, parses the mpd-album value, runs the
/// cascade, transcodes the winning bytes, and returns the
/// response + cache payload. Wraps the synchronous transcode
/// step in a separate `spawn_blocking` at the caller's site;
/// the cascade itself is async because of the HTTP fetches.
pub(crate) async fn resolve_artwork(
    payload: &[u8],
    config: &PluginConfig,
    client: &Client,
) -> Result<ResolveOutput, String> {
    if payload.is_empty() {
        return Ok(bad_request("empty payload"));
    }
    let text = match std::str::from_utf8(payload) {
        Ok(t) => t,
        Err(e) => {
            return Ok(bad_request(&format!("payload is not UTF-8: {e}")));
        }
    };
    let req: ResolveOnlineRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => return Ok(bad_request(&format!("invalid JSON: {e}"))),
    };
    if req.v != 1 {
        return Ok(bad_request(&format!("unsupported request v: {}", req.v)));
    }
    let size_str = req.size.as_deref().unwrap_or("original");
    let size = match ArtworkSize::parse(size_str) {
        Some(s) => s,
        None => {
            return Ok(bad_request(&format!(
                "unknown size: {size_str} (expected tiny | medium | large | original)"
            )));
        }
    };
    // Only mpd-album is supported on the online cascade — file
    // paths are the local plugin's domain.
    if req.target.scheme != "mpd-album" {
        return Ok(ResolveOutput {
            response: ResolveOnlineResponse {
                v: 1,
                status: ResponseStatus::Unsupported,
                content_hash: None,
                mime: None,
                size: None,
                provider_id: None,
                detail: Some(format!(
                    "online cascade supports mpd-album only; got {}",
                    req.target.scheme
                )),
            },
            cache_payload: None,
        });
    }
    let (artist, album) =
        match evo_device_audio_shared::parse_mpd_album_value(&req.target.value)
        {
            Ok(p) => p,
            Err(_) => {
                return Ok(bad_request(&format!(
                    "malformed mpd-album value: {}",
                    req.target.value
                )));
            }
        };
    // Run the cascade.
    let hit = providers::run_cascade(&artist, &album, client, config).await;
    let Some(hit) = hit else {
        return Ok(ResolveOutput {
            response: ResolveOnlineResponse {
                v: 1,
                status: ResponseStatus::NotFound,
                content_hash: None,
                mime: None,
                size: None,
                provider_id: None,
                detail: Some(format!(
                    "no provider returned artwork for artist={artist:?} album={album:?}"
                )),
            },
            cache_payload: None,
        });
    };
    let source_mime = hit.mime.clone().unwrap_or_else(|| {
        // Sniff defaults to image/octet-stream which the
        // transcode pipeline only uses when passing original
        // bytes through (the resize path always emits
        // image/webp). For online providers we know the
        // upstream returns a real image, so the sniff is
        // safe — but if a malformed upstream gives us
        // not-an-image, the transcode's decode step refuses
        // honestly.
        "image/jpeg".to_string()
    });
    let TranscodedArtwork {
        bytes,
        content_hash,
        mime,
    } = match transcode(hit.bytes, &source_mime, size) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                plugin = crate::PLUGIN_NAME,
                provider = %hit.provider_id,
                error = %e,
                "transcode failed; degrading to not_found"
            );
            return Ok(ResolveOutput {
                response: ResolveOnlineResponse {
                    v: 1,
                    status: ResponseStatus::NotFound,
                    content_hash: None,
                    mime: None,
                    size: None,
                    provider_id: None,
                    detail: Some(format!(
                        "transcode of {} bytes from {} failed",
                        source_mime, hit.provider_id
                    )),
                },
                cache_payload: None,
            });
        }
    };
    Ok(ResolveOutput {
        response: ResolveOnlineResponse {
            v: 1,
            status: ResponseStatus::Ok,
            content_hash: Some(content_hash.clone()),
            mime: Some(mime),
            size: Some(size.as_str().to_string()),
            provider_id: Some(hit.provider_id.to_string()),
            detail: None,
        },
        cache_payload: Some((content_hash, bytes)),
    })
}

fn bad_request(detail: &str) -> ResolveOutput {
    ResolveOutput {
        response: ResolveOnlineResponse {
            v: 1,
            status: ResponseStatus::BadRequest,
            content_hash: None,
            mime: None,
            size: None,
            provider_id: None,
            detail: Some(detail.to_string()),
        },
        cache_payload: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_config() -> PluginConfig {
        PluginConfig::defaults()
    }

    fn make_client() -> Client {
        providers::build_http_client(std::time::Duration::from_secs(2))
    }

    #[tokio::test]
    async fn empty_payload_yields_bad_request() {
        let out = resolve_artwork(b"", &empty_config(), &make_client())
            .await
            .unwrap();
        assert_eq!(out.response.status, ResponseStatus::BadRequest);
        assert!(out.cache_payload.is_none());
    }

    #[tokio::test]
    async fn mpd_path_scheme_yields_unsupported() {
        let payload =
            br#"{"v":1,"target":{"scheme":"mpd-path","value":"foo"}}"#;
        let out = resolve_artwork(payload, &empty_config(), &make_client())
            .await
            .unwrap();
        assert_eq!(out.response.status, ResponseStatus::Unsupported);
    }

    #[tokio::test]
    async fn unknown_size_yields_bad_request() {
        let payload = br#"{"v":1,"target":{"scheme":"mpd-album","value":"Beatles|Revolver"},"size":"xl"}"#;
        let out = resolve_artwork(payload, &empty_config(), &make_client())
            .await
            .unwrap();
        assert_eq!(out.response.status, ResponseStatus::BadRequest);
        let detail = out.response.detail.unwrap_or_default();
        assert!(
            detail.contains("xl"),
            "detail should name the bad size; got {detail}"
        );
    }

    #[tokio::test]
    async fn malformed_album_value_yields_bad_request() {
        // Missing pipe separator → ParseError::InvalidFormat.
        let payload =
            br#"{"v":1,"target":{"scheme":"mpd-album","value":"no separator"}}"#;
        let out = resolve_artwork(payload, &empty_config(), &make_client())
            .await
            .unwrap();
        assert_eq!(out.response.status, ResponseStatus::BadRequest);
    }

    #[tokio::test]
    async fn unsupported_v_yields_bad_request() {
        let payload =
            br#"{"v":99,"target":{"scheme":"mpd-album","value":"a|b"}}"#;
        let out = resolve_artwork(payload, &empty_config(), &make_client())
            .await
            .unwrap();
        assert_eq!(out.response.status, ResponseStatus::BadRequest);
    }
}
