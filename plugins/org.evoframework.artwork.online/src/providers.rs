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
//! Each provider returns a [`ProviderOutcome`] classifying the
//! result as Hit / Miss / Unavailable. Only a clean Miss (the
//! provider's canonical "no such release" answer) counts toward
//! a cascade-level `GenuinelyEmpty` outcome; any Unavailable
//! (network error, HTTP 5xx, 429, timeout, transcode failure)
//! propagates to the cascade as [`CascadeResult::Unavailable`]
//! so upstream callers can distinguish "definitively absent" —
//! cacheable — from "we could not reach the provider right now"
//! — never cacheable.
//!
//! This distinction is load-bearing: framework-side negative
//! memoisation must not remember a transient upstream failure
//! as if it were a permanent absence, or an operator who adds
//! a cover waits until the memo expires.

use reqwest::{Client, StatusCode};

use crate::config::PluginConfig;

/// Per-provider outcome after one fetch attempt.
///
/// `Miss` means the provider answered cleanly with "no such
/// release" (empty search result set, 404 on canonical
/// endpoint). `Unavailable` means we could not obtain that
/// answer — transport error, rate-limit, 5xx, timeout,
/// unsupported provider protocol behaviour.
pub(crate) enum ProviderOutcome {
    /// Provider returned artwork bytes.
    Hit(ProviderHit),
    /// Provider was reachable and answered cleanly with no
    /// artwork for the requested target.
    Miss,
    /// Provider was NOT reachable / returned a transient
    /// failure. Carries a short human-readable reason for
    /// telemetry + wire-response `detail`.
    Unavailable(String),
}

/// Aggregate outcome across the full provider cascade.
///
/// `GenuinelyEmpty` requires that EVERY attempted (enabled +
/// configured) provider returned [`ProviderOutcome::Miss`].
/// If any provider returned [`ProviderOutcome::Unavailable`]
/// and no provider Hit, the aggregate is `Unavailable` — the
/// caller must not cache this outcome negatively.
pub(crate) enum CascadeResult {
    /// The first provider to succeed.
    Hit(ProviderHit),
    /// Every enabled provider was reached and each answered
    /// with a clean Miss. Safe to cache as not_found.
    GenuinelyEmpty,
    /// At least one provider was Unavailable and no provider
    /// Hit. The aggregate detail lists which providers were
    /// unavailable and why.
    Unavailable(String),
}

/// Classify a reqwest error as Unavailable — every transport
/// / decode / IO error is transient by construction.
fn classify_reqwest_error(
    provider: &str,
    err: &anyhow::Error,
) -> ProviderOutcome {
    ProviderOutcome::Unavailable(format!("{provider}: {err}"))
}

/// Classify an HTTP response status.
///
/// 2xx → success (caller consumes body).
/// 404 / 410 → clean Miss (release is definitively absent from
///   this provider's catalogue).
/// 401 / 403 → Unavailable (auth/permission issue; not a
///   catalogue answer — the operator would fix it).
/// 429 / 5xx / 408 → Unavailable (rate-limit or upstream
///   fault).
/// Other 4xx → Unavailable (request-shape defect at the
///   provider; treat conservatively as transient rather than
///   burning a false negative).
pub(crate) enum StatusClass {
    Success,
    Miss,
    Unavailable(String),
}

pub(crate) fn classify_status(
    provider: &str,
    status: StatusCode,
) -> StatusClass {
    if status.is_success() {
        StatusClass::Success
    } else if status == StatusCode::NOT_FOUND || status == StatusCode::GONE {
        StatusClass::Miss
    } else {
        StatusClass::Unavailable(format!(
            "{provider}: HTTP {}",
            status.as_u16()
        ))
    }
}

// Reuse the shared distribution client factory. artwork.online +
// metadata.online share connection pool + DNS cache + TLS
// posture. Local `build_http_client` re-exported so existing
// call sites in this crate compile without churn — but the
// implementation is the shared crate's.
pub(crate) use evo_online_providers::build_http_client;

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

/// Cascade walker.
///
/// Invokes enabled providers in priority order. Returns:
/// - [`CascadeResult::Hit`] on the first provider hit;
/// - [`CascadeResult::GenuinelyEmpty`] when every attempted
///   provider returned [`ProviderOutcome::Miss`] — the album
///   is definitively absent from every catalogue we consulted;
/// - [`CascadeResult::Unavailable`] when any provider was
///   [`ProviderOutcome::Unavailable`] and no provider Hit —
///   we could not obtain complete negative evidence and the
///   caller MUST NOT cache this outcome as absence.
pub(crate) async fn run_cascade(
    artist: &str,
    album: &str,
    client: &Client,
    config: &PluginConfig,
) -> CascadeResult {
    let mut unavailable_reasons: Vec<String> = Vec::new();

    let attempted = |outcome: Option<ProviderOutcome>,
                     name: &'static str,
                     unavailable: &mut Vec<String>|
     -> Option<ProviderOutcome> {
        match outcome {
            None => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    "disabled"
                );
                None
            }
            Some(ProviderOutcome::Hit(hit)) => Some(ProviderOutcome::Hit(hit)),
            Some(ProviderOutcome::Miss) => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    "clean miss"
                );
                Some(ProviderOutcome::Miss)
            }
            Some(ProviderOutcome::Unavailable(reason)) => {
                tracing::warn!(
                    plugin = crate::PLUGIN_NAME,
                    provider = name,
                    reason = %reason,
                    "provider unavailable; cascading (result will not be cached negatively)"
                );
                unavailable.push(reason.clone());
                Some(ProviderOutcome::Unavailable(reason))
            }
        }
    };

    let mut any_miss = false;
    macro_rules! step {
        ($outcome:expr, $name:literal) => {
            if let Some(result) =
                attempted($outcome, $name, &mut unavailable_reasons)
            {
                match result {
                    ProviderOutcome::Hit(hit) => {
                        return CascadeResult::Hit(hit);
                    }
                    ProviderOutcome::Miss => any_miss = true,
                    ProviderOutcome::Unavailable(_) => {}
                }
            }
        };
    }

    if config.providers.cover_art_archive.enabled {
        step!(
            cover_art_archive::fetch(artist, album, client, config).await,
            "cover_art_archive"
        );
    }
    if config.providers.lastfm.enabled {
        step!(lastfm::fetch(artist, album, client, config).await, "lastfm");
    }
    if config.providers.itunes.enabled {
        step!(Some(itunes::fetch(artist, album, client).await), "itunes");
    }
    if config.providers.volumio_meta.enabled {
        step!(
            Some(volumio_meta::fetch(artist, album, client, config).await),
            "volumio_meta"
        );
    }

    if unavailable_reasons.is_empty() {
        // Every attempted provider gave a clean Miss (or no
        // providers were attempted at all — vacuous empty).
        // Safe to cache as not_found.
        let _ = any_miss;
        CascadeResult::GenuinelyEmpty
    } else {
        // At least one provider was Unavailable. We do not
        // have complete negative evidence — return Unavailable
        // so the framework's memoisation skips this outcome.
        CascadeResult::Unavailable(unavailable_reasons.join("; "))
    }
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

    /// Fetch attempt against the CAA cascade.
    ///
    /// Returns `None` ONLY when the provider is disabled by
    /// configuration (missing/empty musicbrainz_user_agent) —
    /// disabled providers do not vote toward the aggregate
    /// cascade result. When the provider is enabled, returns
    /// [`ProviderOutcome`] carrying Hit / Miss / Unavailable
    /// per the classification rules on this module.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Option<ProviderOutcome> {
        let ua = match config.musicbrainz_user_agent.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => {
                tracing::debug!(
                    plugin = crate::PLUGIN_NAME,
                    provider = "cover_art_archive",
                    "skipped: musicbrainz_user_agent not configured"
                );
                return None;
            }
        };
        // Step 1: MusicBrainz release search.
        let mbids = match mb_release_search(artist, album, client, ua).await {
            Ok(SearchOutcome::Mbids(m)) => m,
            Ok(SearchOutcome::CleanMiss) => return Some(ProviderOutcome::Miss),
            Err(e) => {
                return Some(classify_reqwest_error(
                    "cover_art_archive (mb release search)",
                    &e,
                ));
            }
        };
        if mbids.is_empty() {
            // MB returned 200 with an empty release array — the
            // release is definitively not indexed. Clean miss.
            return Some(ProviderOutcome::Miss);
        }
        // Step 2: CAA front-cover fetch per MBID. First hit
        // wins. If EVERY MBID cleanly returned "no front cover"
        // (404 on the /front endpoint), that is a clean Miss:
        // the album exists but has no front art in CAA. If any
        // MBID returned Unavailable, propagate.
        let mut any_unavailable: Option<String> = None;
        for mbid in mbids.into_iter().take(MAX_MBIDS_TRIED) {
            match fetch_front_cover(client, &mbid, ua).await {
                Ok(FrontCoverOutcome::Hit(bytes)) => {
                    return Some(ProviderOutcome::Hit(ProviderHit {
                        bytes,
                        mime: Some("image/jpeg".to_string()),
                        provider_id: "cover_art_archive",
                    }));
                }
                Ok(FrontCoverOutcome::Miss) => continue,
                Err(e) => {
                    any_unavailable = Some(format!(
                        "cover_art_archive (front mbid={mbid}): {e}"
                    ));
                }
            }
        }
        Some(match any_unavailable {
            Some(reason) => ProviderOutcome::Unavailable(reason),
            None => ProviderOutcome::Miss,
        })
    }

    enum SearchOutcome {
        Mbids(Vec<String>),
        CleanMiss,
    }

    enum FrontCoverOutcome {
        Hit(Vec<u8>),
        Miss,
    }

    async fn mb_release_search(
        artist: &str,
        album: &str,
        client: &Client,
        ua: &str,
    ) -> Result<SearchOutcome> {
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
        match classify_status("mb release search", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Ok(SearchOutcome::CleanMiss),
            StatusClass::Unavailable(reason) => {
                anyhow::bail!(reason);
            }
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
        Ok(SearchOutcome::Mbids(mbids))
    }

    async fn fetch_front_cover(
        client: &Client,
        mbid: &str,
        ua: &str,
    ) -> Result<FrontCoverOutcome> {
        let url = format!("{}/release/{}/front", COVER_ART_BASE, mbid);
        let resp = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, ua)
            .send()
            .await
            .context("cover-art-archive front fetch failed")?;
        match classify_status("caa front", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Ok(FrontCoverOutcome::Miss),
            StatusClass::Unavailable(reason) => anyhow::bail!(reason),
        }
        let bytes = resp.bytes().await.context("cover bytes read")?;
        Ok(FrontCoverOutcome::Hit(bytes.to_vec()))
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
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const LASTFM_BASE: &str = "https://ws.audioscrobbler.com/2.0/";

    /// Fetch attempt against Last.fm album.getinfo.
    ///
    /// Returns `None` when the provider is disabled (no API key
    /// configured). Otherwise returns [`ProviderOutcome`].
    /// Last.fm's API returns a 200 OK JSON body with an
    /// `error` field on catalogue misses; we treat that as a
    /// clean Miss. HTTP-level 5xx / 429 / transport errors are
    /// Unavailable.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> Option<ProviderOutcome> {
        let api_key = match config.providers.lastfm.api_key.as_deref() {
            Some(k) if !k.is_empty() => k,
            _ => return None,
        };
        let url = format!(
            "{}?method=album.getinfo&api_key={}&artist={}&album={}&format=json",
            LASTFM_BASE,
            urlencode(api_key),
            urlencode(artist),
            urlencode(album),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm: {e}"
                )));
            }
        };
        match classify_status("lastfm", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Some(ProviderOutcome::Miss),
            StatusClass::Unavailable(r) => {
                return Some(ProviderOutcome::Unavailable(r));
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm: json decode: {e}"
                )));
            }
        };
        // Last.fm signals catalogue misses as `{ error: N,
        // message: "..." }` at 200 OK. Treat as clean Miss.
        if json.get("error").is_some() {
            return Some(ProviderOutcome::Miss);
        }
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
            return Some(ProviderOutcome::Miss);
        };
        let img_resp = match client.get(&image_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm image: {e}"
                )));
            }
        };
        match classify_status("lastfm image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return Some(ProviderOutcome::Miss),
            StatusClass::Unavailable(r) => {
                return Some(ProviderOutcome::Unavailable(r));
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Some(ProviderOutcome::Unavailable(format!(
                    "lastfm image bytes: {e}"
                )));
            }
        };
        Some(ProviderOutcome::Hit(ProviderHit {
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
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const ITUNES_BASE: &str = "https://itunes.apple.com/search";

    /// Fetch attempt against iTunes Search API.
    ///
    /// Always attempted (no key). Miss = 200 OK with empty
    /// `results` or missing `artworkUrl100`. Unavailable =
    /// transport / 5xx / 429 / decode errors.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
    ) -> ProviderOutcome {
        let term = format!("{artist} {album}");
        let url = format!(
            "{}?term={}&entity=album&limit=1",
            ITUNES_BASE,
            urlencode(&term),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!("itunes: {e}"));
            }
        };
        match classify_status("itunes", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes json: {e}"
                ));
            }
        };
        let thumb_url = json
            .get("results")
            .and_then(serde_json::Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|hit| hit.get("artworkUrl100"))
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        let Some(thumb_url) = thumb_url else {
            return ProviderOutcome::Miss;
        };
        // Upscale the thumbnail URL to 600x600 via the
        // documented URL-pattern trick.
        let upscaled = thumb_url.replace("/100x100bb.", "/600x600bb.");
        let img_resp = match client.get(&upscaled).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes image: {e}"
                ));
            }
        };
        match classify_status("itunes image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "itunes image bytes: {e}"
                ));
            }
        };
        ProviderOutcome::Hit(ProviderHit {
            bytes,
            mime: None,
            provider_id: "itunes",
        })
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
    // anyhow no longer used since fetch returns ProviderOutcome directly.

    const VOLUMIO_META_BASE: &str =
        "https://meta.volumio.org/metas/v1/getDatas";

    /// Fetch attempt against Volumio meta-proxy.
    ///
    /// Miss = 200 OK with empty `data`. Unavailable = transport
    /// / 5xx / 429 / decode errors.
    pub(crate) async fn fetch(
        artist: &str,
        album: &str,
        client: &Client,
        config: &PluginConfig,
    ) -> ProviderOutcome {
        let variant = &config.providers.volumio_meta.variant;
        let url = format!(
            "{}?mode=albumArt&artist={}&album={}&variant={}",
            VOLUMIO_META_BASE,
            urlencode(artist),
            urlencode(album),
            urlencode(variant),
        );
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta: {e}"
                ));
            }
        };
        match classify_status("volumio_meta", resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta json: {e}"
                ));
            }
        };
        let image_url = json
            .get("data")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let Some(image_url) = image_url else {
            return ProviderOutcome::Miss;
        };
        let img_resp = match client.get(&image_url).send().await {
            Ok(r) => r,
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta image: {e}"
                ));
            }
        };
        match classify_status("volumio_meta image", img_resp.status()) {
            StatusClass::Success => {}
            StatusClass::Miss => return ProviderOutcome::Miss,
            StatusClass::Unavailable(r) => {
                return ProviderOutcome::Unavailable(r)
            }
        }
        let bytes = match img_resp.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return ProviderOutcome::Unavailable(format!(
                    "volumio_meta image bytes: {e}"
                ));
            }
        };
        ProviderOutcome::Hit(ProviderHit {
            bytes,
            mime: None,
            provider_id: "volumio_meta",
        })
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
