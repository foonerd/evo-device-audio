// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Online artwork providers.
//!
//! Each provider implements a thin `fetch` async function that
//! takes (artist, album, http client, config) and returns either
//! image bytes on hit or a structured no-result on miss. The
//! cascade walker invokes providers in priority order; the first
//! provider returning Ok-with-bytes wins.
//!
//! ## Provider summary
//!
//! - [`cover_art_archive`] — Cover Art Archive (MusicBrainz).
//!   Free, requires only a properly-identifying User-Agent per
//!   MusicBrainz TOS. Two-step query: search MusicBrainz for
//!   matching release MBIDs, then fetch the front cover from
//!   `coverartarchive.org/release/{mbid}/front`.
//! - [`lastfm`] — Last.fm `album.getinfo`. Requires an operator-
//!   supplied API key. Image URL embedded in the response.
//! - [`itunes`] — Apple iTunes Search API. No key required.
//!   Returns 100x100 thumbnail URLs that we rewrite to 600x600
//!   (a documented URL-pattern trick).
//! - [`volumio_meta`] — Volumio's hosted meta proxy
//!   (`meta.volumio.org`). No key; takes a `variant` parameter
//!   (`community` vs `commercial`).
//!
//! Each provider returns a [`ProviderHit`] on success carrying
//! the bytes + the provider id for telemetry. Misses + per-
//! provider failures are logged at debug and the cascade moves
//! on; only the cascade's final miss (after every provider) is
//! returned to the caller as not-found.

use reqwest::Client;
use std::time::Duration;

use crate::config::PluginConfig;

/// One successful provider hit.
pub(crate) struct ProviderHit {
    /// Image bytes (JPEG / PNG / WebP, per the upstream
    /// provider's encoding).
    pub(crate) bytes: Vec<u8>,
    /// MIME type when the upstream provided it; `None` when the
    /// caller should sniff from leading bytes.
    pub(crate) mime: Option<String>,
    /// Stable provider identifier for telemetry + diagnostics.
    /// Round-trips on the wire-shape response so the operator UI
    /// can show "from MusicBrainz" / "from iTunes" etc.
    pub(crate) provider_id: &'static str,
}

/// Build the shared HTTP client. One per plugin load —
/// connection pooling + DNS cache reuse across cascade calls.
pub(crate) fn build_http_client(timeout: Duration) -> Client {
    Client::builder()
        .timeout(timeout)
        // Generous redirect limit covers CAA's 307 redirect
        // chain to the actual image URL.
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        // Build failures here are framework-level configuration
        // errors (TLS init, threadpool spawn); refusing to admit
        // is the right shape. Construction error surfaces to
        // the plugin's load() return.
        .expect("reqwest client builder")
}

/// Cascade walker.
///
/// Invokes providers in priority order; returns the first
/// successful [`ProviderHit`]. Logs each provider's outcome at
/// debug for diagnostics. Returns `None` only when every
/// provider missed or was disabled.
pub(crate) async fn run_cascade(
    artist: &str,
    album: &str,
    client: &Client,
    config: &PluginConfig,
) -> Option<ProviderHit> {
    if config.providers.cover_art_archive.enabled {
        match cover_art_archive::fetch(artist, album, client, config).await {
            Ok(Some(hit)) => return Some(hit),
            Ok(None) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "cover_art_archive",
                "miss"
            ),
            Err(e) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "cover_art_archive",
                error = %e,
                "provider error; cascading"
            ),
        }
    }
    if config.providers.lastfm.enabled
        && config.providers.lastfm.api_key.is_some()
    {
        match lastfm::fetch(artist, album, client, config).await {
            Ok(Some(hit)) => return Some(hit),
            Ok(None) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "lastfm",
                "miss"
            ),
            Err(e) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "lastfm",
                error = %e,
                "provider error; cascading"
            ),
        }
    }
    if config.providers.itunes.enabled {
        match itunes::fetch(artist, album, client).await {
            Ok(Some(hit)) => return Some(hit),
            Ok(None) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "itunes",
                "miss"
            ),
            Err(e) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "itunes",
                error = %e,
                "provider error; cascading"
            ),
        }
    }
    if config.providers.volumio_meta.enabled {
        match volumio_meta::fetch(artist, album, client, config).await {
            Ok(Some(hit)) => return Some(hit),
            Ok(None) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                "miss"
            ),
            Err(e) => tracing::debug!(
                plugin = crate::PLUGIN_NAME,
                provider = "volumio_meta",
                error = %e,
                "provider error; cascading"
            ),
        }
    }
    None
}

pub(crate) mod cover_art_archive {
    //! Cover Art Archive provider.
    //!
    //! Two-step lookup:
    //! 1. Query MusicBrainz `release` search for the
    //!    artist+album pair; collect the top N release MBIDs.
    //! 2. For each MBID, hit `coverartarchive.org/release/{mbid}/front`.
    //!    The endpoint returns the front cover image bytes (after
    //!    a 307 redirect to the CDN-hosted URL); the first MBID
    //!    that yields bytes wins.
    //!
    //! MusicBrainz refuses requests without an identifying UA
    //! per their TOS; the provider silently disables when the
    //! operator has not configured one.

    use super::*;
    use anyhow::{Context, Result};

    const MUSICBRAINZ_BASE: &str = "https://musicbrainz.org/ws/2";
    const COVER_ART_BASE: &str = "https://coverartarchive.org";
    /// Cap MBIDs tried per album to keep upstream load bounded.
    /// First MBID is most-relevant per MusicBrainz scoring;
    /// retrying a small number handles the case where the top
    /// hit lacks front-cover artwork.
    const MAX_MBIDS_TRIED: usize = 3;

    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Result<Option<ProviderHit>> {
        let ua = match config.musicbrainz_user_agent.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "cover_art_archive",
                    "skipped: musicbrainz_user_agent not configured"
                );
                return Ok(None);
            }
        };
        let mbids = mb_release_search(artist, album, client, ua).await?;
        for mbid in mbids.into_iter().take(MAX_MBIDS_TRIED) {
            if let Some(bytes) = fetch_front_cover(client, &mbid, ua).await? {
                return Ok(Some(ProviderHit {
                    bytes,
                    mime: Some("image/jpeg".to_string()),
                    provider_id: "cover_art_archive",
                }));
            }
        }
        Ok(None)
    }

    async fn mb_release_search(
        artist: &str,
        album: &str,
        client: &Client,
        ua: &str,
    ) -> Result<Vec<String>> {
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album),
        );
        let url = format!(
            "{}/release?query={}&fmt=json&limit=5",
            MUSICBRAINZ_BASE,
            urlencode(&query),
        );
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, ua)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("musicbrainz release search request failed")?;
        if !resp.status().is_success() {
            return Ok(Vec::new());
        }
        let json: serde_json::Value =
            resp.json().await.context("musicbrainz response decode")?;
        let mbids: Vec<String> = json
            .get("releases")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        r.get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(String::from)
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(mbids)
    }

    async fn fetch_front_cover(
        client: &Client,
        mbid: &str,
        ua: &str,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/release/{}/front", COVER_ART_BASE, mbid);
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, ua)
            .send()
            .await
            .context("cover-art-archive front fetch failed")?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let bytes = resp.bytes().await.context("cover bytes read")?;
        Ok(Some(bytes.to_vec()))
    }

    /// Escape Lucene query special characters per
    /// MusicBrainz's query language. Conservative: covers the
    /// characters that would otherwise corrupt the query shape.
    pub(crate) fn escape_lucene(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '\\' | '"' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '^'
                | '~' | '*' | '?' | '+' | '-' | '!' | '&' | '|' => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
        out
    }
}

pub(crate) mod lastfm {
    //! Last.fm `album.getinfo` provider.
    //!
    //! Requires an operator-supplied API key (`api_key` in the
    //! `[providers.lastfm]` table). The API returns a JSON
    //! payload with an `image` array carrying multiple sizes;
    //! we pick the largest (`mega` / `extralarge`) and fetch.

    use super::*;
    use anyhow::{Context, Result};

    const LASTFM_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Result<Option<ProviderHit>> {
        let api_key = match config.providers.lastfm.api_key.as_deref() {
            Some(k) if !k.is_empty() => k,
            _ => return Ok(None),
        };
        let url = format!(
            "{}?method=album.getinfo&api_key={}&artist={}&album={}&format=json",
            LASTFM_BASE,
            urlencode(api_key),
            urlencode(artist),
            urlencode(album),
        );
        let resp = client.get(&url).send().await.context("lastfm request")?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let json: serde_json::Value =
            resp.json().await.context("lastfm json")?;
        // Last.fm's image array carries entries shaped
        // `{ size: "mega" | "extralarge" | ..., "#text": "<url>" }`.
        let url = json
            .get("album")
            .and_then(|a| a.get("image"))
            .and_then(serde_json::Value::as_array)
            .and_then(|imgs| {
                ["mega", "extralarge", "large", "medium", "small"]
                    .iter()
                    .find_map(|want| {
                        imgs.iter().find_map(|img| {
                            let size = img
                                .get("size")
                                .and_then(serde_json::Value::as_str)?;
                            if size == *want {
                                img.get("#text")
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                            } else {
                                None
                            }
                        })
                    })
            });
        let Some(image_url) = url else {
            return Ok(None);
        };
        let img_resp = client
            .get(&image_url)
            .send()
            .await
            .context("lastfm image fetch")?;
        if !img_resp.status().is_success() {
            return Ok(None);
        }
        let bytes = img_resp
            .bytes()
            .await
            .context("lastfm image bytes")?
            .to_vec();
        Ok(Some(ProviderHit {
            bytes,
            mime: None,
            provider_id: "lastfm",
        }))
    }
}

pub(crate) mod itunes {
    //! Apple iTunes Search API provider.
    //!
    //! No API key required. Returns 100x100 thumbnail URLs in
    //! `artworkUrl100`; URL-pattern trick swaps the size segment
    //! to fetch a larger variant (`/100x100bb/` →
    //! `/600x600bb/`).

    use super::*;
    use anyhow::{Context, Result};

    const ITUNES_BASE: &str = "https://itunes.apple.com/search";

    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
    ) -> Result<Option<ProviderHit>> {
        let term = format!("{artist} {album}");
        let url = format!(
            "{}?term={}&entity=album&limit=1",
            ITUNES_BASE,
            urlencode(&term),
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .context("itunes search request")?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let json: serde_json::Value =
            resp.json().await.context("itunes search json")?;
        let thumb_url = json
            .get("results")
            .and_then(serde_json::Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|hit| hit.get("artworkUrl100"))
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        let Some(thumb_url) = thumb_url else {
            return Ok(None);
        };
        // Upscale the thumbnail URL to 600x600 via the
        // documented URL-pattern trick.
        let upscaled = thumb_url.replace("/100x100bb.", "/600x600bb.");
        let img_resp = client
            .get(&upscaled)
            .send()
            .await
            .context("itunes image fetch")?;
        if !img_resp.status().is_success() {
            return Ok(None);
        }
        let bytes = img_resp
            .bytes()
            .await
            .context("itunes image bytes")?
            .to_vec();
        Ok(Some(ProviderHit {
            bytes,
            mime: None,
            provider_id: "itunes",
        }))
    }
}

pub(crate) mod volumio_meta {
    //! Volumio's hosted meta proxy provider.
    //!
    //! Endpoint: `https://meta.volumio.org/metas/v1/getDatas`
    //! with `mode=albumArt`, `artist`, `album`, `variant`. The
    //! `variant` selects between Volumio's community + commercial
    //! distribution paths.

    use super::*;
    use anyhow::{Context, Result};

    const VOLUMIO_META_BASE: &str =
        "https://meta.volumio.org/metas/v1/getDatas";

    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Result<Option<ProviderHit>> {
        let variant = &config.providers.volumio_meta.variant;
        let url = format!(
            "{}?mode=albumArt&artist={}&album={}&variant={}",
            VOLUMIO_META_BASE,
            urlencode(artist),
            urlencode(album),
            urlencode(variant),
        );
        let resp = client
            .get(&url)
            .send()
            .await
            .context("volumio_meta request")?;
        if !resp.status().is_success() {
            return Ok(None);
        }
        let json: serde_json::Value =
            resp.json().await.context("volumio_meta json")?;
        let image_url = json
            .get("data")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let Some(image_url) = image_url else {
            return Ok(None);
        };
        let img_resp = client
            .get(&image_url)
            .send()
            .await
            .context("volumio_meta image fetch")?;
        if !img_resp.status().is_success() {
            return Ok(None);
        }
        let bytes = img_resp
            .bytes()
            .await
            .context("volumio_meta image bytes")?
            .to_vec();
        Ok(Some(ProviderHit {
            bytes,
            mime: None,
            provider_id: "volumio_meta",
        }))
    }
}

/// Minimal percent-encoder for URL query values. Mirrors the
/// helper in the shared crate's artwork_target_url; inlined here
/// to keep the plugin's outbound URL construction self-contained.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_art_archive_lucene_escape() {
        let escaped = cover_art_archive::escape_lucene(
            r#"Sting: A "Test" Album (Deluxe)"#,
        );
        assert!(escaped.contains(r#"\""#));
        assert!(escaped.contains(r#"\:"#));
        assert!(escaped.contains(r#"\("#));
        assert!(escaped.contains(r#"\)"#));
    }

    #[test]
    fn urlencode_unreserved_passthrough() {
        assert_eq!(urlencode("Beatles_Revolver"), "Beatles_Revolver");
        assert_eq!(urlencode("AC&DC"), "AC%26DC");
        assert_eq!(urlencode("Sigur Rós"), "Sigur%20R%C3%B3s");
    }
}
