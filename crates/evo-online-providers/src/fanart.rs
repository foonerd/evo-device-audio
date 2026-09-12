// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! fanart.tv v3 JSON client — keyed artist artwork
//! (backgrounds / HD logos / banners) for the metadata cascade.
//!
//! ## Provider posture
//!
//! fanart.tv requires a personal API key (free after account
//! registration on the project's site). Operators supply the key
//! via the credential vault under
//! `fanart_tv_personal_api_key`; the plugin passes it to this
//! client at construction. No content flows without a key —
//! fanart.tv is `privacy_class: identity_bearing`.
//!
//! ## Endpoint
//!
//! - `GET https://webservice.fanart.tv/v3/music/{artist_mbid}?
//!   api_key={key}` — returns the full image manifest for an
//!   artist keyed by MusicBrainz artist MBID. The manifest is
//!   grouped by artwork type (`hdmusiclogo`, `hdartistlogo`,
//!   `artistbackground`, `artistthumb`, `musicbanner`, …); each
//!   type is an array of entries carrying an image URL.
//!
//! ## MusicBrainz linkage
//!
//! fanart.tv indexes artists by MBID; the caller MUST supply an
//! MBID (from the plugin's existing MB reconciliation step).
//! There is no name-search fallback in this client — the
//! reconciliation contract is that MB assigns the MBID first,
//! then downstream identity-bearing providers key on it.
//!
//! ## Attribution
//!
//! The operator UI MUST render attribution beside any rendered
//! fanart.tv artwork, carrying `source_name = "fanart.tv"`,
//! `source_url = https://fanart.tv/artist/{artist_mbid}`, and
//! `license = "fanart.tv terms of use"`. fanart.tv community
//! uploads are contributor-licensed under the project's terms;
//! the marker signals the UI to render attribution accordingly.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

/// Errors from the fanart.tv client.
#[derive(Debug, thiserror::Error)]
pub enum FanartError {
    /// Underlying HTTP transport failure. Cascade callers treat
    /// this as a transient provider skip.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// fanart.tv returned a non-2xx HTTP status. 404 is a
    /// legitimate miss (no images for the MBID) and surfaces
    /// as `Ok(None)` rather than an error.
    #[error("fanart.tv returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    /// Response body did not decode as the expected JSON shape.
    #[error("fanart.tv JSON decode failed: {0}")]
    Decode(String),
    /// The caller passed an empty or otherwise invalid required
    /// argument. Contract violation.
    #[error("fanart.tv invalid argument: {0}")]
    Invalid(String),
}

/// Artist-artwork manifest surfaced by
/// [`FanartClient::get_artist_images`]. Every image-type field
/// is `Vec<String>` because fanart.tv returns arrays; a caller
/// that only wants the first entry uses `.first()` on the
/// relevant field.
///
/// URLs point at fanart.tv's CDN and are stable per artist MBID.
/// Long-lived links, so unlike Deezer's the URL itself is worth
/// memoising under the plugin's per-provider cache layer, with
/// the same license/attribution shape the other keyed providers
/// use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistImagesHit {
    /// The MusicBrainz artist MBID the manifest is keyed on
    /// (echoes the caller's argument).
    pub artist_mbid: String,
    /// The artist's display name from fanart.tv (`name` field),
    /// when present.
    pub artist_name: Option<String>,
    /// HD music logo URLs (`hdmusiclogo` / `musiclogo`
    /// endpoints).
    pub hd_music_logo_urls: Vec<String>,
    /// HD artist logo URLs (`hdartistlogo` endpoints).
    pub hd_artist_logo_urls: Vec<String>,
    /// Full-bleed artist background URLs (`artistbackground`).
    pub artist_background_urls: Vec<String>,
    /// Artist thumb URLs (`artistthumb`).
    pub artist_thumb_urls: Vec<String>,
    /// Music banner URLs (`musicbanner`).
    pub music_banner_urls: Vec<String>,
    /// Canonical URL back to the artist's fanart.tv page.
    pub source_url: String,
}

impl ArtistImagesHit {
    /// True when the hit has at least one usable URL of any
    /// artwork type. A response with only the artist name and
    /// no images is not usable for the operator UI.
    pub fn has_any_artwork(&self) -> bool {
        !self.hd_music_logo_urls.is_empty()
            || !self.hd_artist_logo_urls.is_empty()
            || !self.artist_background_urls.is_empty()
            || !self.artist_thumb_urls.is_empty()
            || !self.music_banner_urls.is_empty()
    }
}

/// fanart.tv v3 JSON client.
#[derive(Clone)]
pub struct FanartClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
    api_key: String,
}

impl std::fmt::Debug for FanartClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FanartClient")
            .field("user_agent", &self.user_agent)
            .field("has_api_key", &!self.api_key.is_empty())
            .finish_non_exhaustive()
    }
}

impl FanartClient {
    /// Construct a client bound to the operator's fanart.tv
    /// personal API key. Returns `None` when the key argument
    /// is empty — the caller decides whether to fall back to
    /// keyless providers or degrade the cascade for this
    /// artwork tier. The key is not logged; only its presence
    /// surfaces in [`Debug`].
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Option<Self> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            http,
            rate,
            user_agent: user_agent.into(),
            api_key,
        })
    }

    /// Fetch the artist's fanart.tv artwork manifest keyed by
    /// MusicBrainz artist MBID. Returns `Ok(None)` on 404
    /// (fanart.tv has no images for the MBID) — the cascade
    /// treats this as a clean miss.
    pub async fn get_artist_images(
        &self,
        artist_mbid: &str,
    ) -> Result<Option<ArtistImagesHit>, FanartError> {
        let mbid = artist_mbid.trim();
        if mbid.is_empty() {
            return Err(FanartError::Invalid("artist MBID is empty".into()));
        }
        if self.user_agent.trim().is_empty() {
            return Err(FanartError::Invalid(
                "user-agent must be non-empty".into(),
            ));
        }
        self.rate.acquire().await;
        let url = format!("https://webservice.fanart.tv/v3/music/{mbid}");
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(FanartError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let body: ArtistBody = serde_json::from_slice(&bytes)
            .map_err(|e| FanartError::Decode(e.to_string()))?;
        let source_url = format!(
            "https://fanart.tv/artist/{}",
            body.mbid_id.as_deref().unwrap_or(mbid)
        );
        Ok(Some(ArtistImagesHit {
            artist_mbid: body.mbid_id.unwrap_or_else(|| mbid.to_string()),
            artist_name: body.name,
            hd_music_logo_urls: extract_urls(&body.hdmusiclogo),
            hd_artist_logo_urls: extract_urls(&body.hdartistlogo),
            artist_background_urls: extract_urls(&body.artistbackground),
            artist_thumb_urls: extract_urls(&body.artistthumb),
            music_banner_urls: extract_urls(&body.musicbanner),
            source_url,
        }))
    }
}

fn extract_urls(entries: &Option<Vec<ImageEntry>>) -> Vec<String> {
    entries
        .as_ref()
        .map(|list| {
            list.iter()
                .filter_map(|e| e.url.clone())
                .filter(|u| !u.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

// -----------------------------------------------------------------
// JSON shape — fanart.tv v3 music endpoint.
// -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ArtistBody {
    #[serde(rename = "mbid_id")]
    mbid_id: Option<String>,
    name: Option<String>,
    #[serde(default)]
    hdmusiclogo: Option<Vec<ImageEntry>>,
    #[serde(default)]
    hdartistlogo: Option<Vec<ImageEntry>>,
    #[serde(default)]
    artistbackground: Option<Vec<ImageEntry>>,
    #[serde(default)]
    artistthumb: Option<Vec<ImageEntry>>,
    #[serde(default)]
    musicbanner: Option<Vec<ImageEntry>>,
}

#[derive(Debug, Deserialize)]
struct ImageEntry {
    #[serde(default)]
    url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_with_key(key: &str) -> Option<FanartClient> {
        let http = reqwest::Client::builder().build().unwrap();
        let rate =
            Arc::new(RateLimiter::new(std::time::Duration::from_millis(500)));
        FanartClient::new(http, rate, "evo/1.0 test", key)
    }

    #[test]
    fn new_returns_none_on_empty_key() {
        assert!(client_with_key("").is_none());
        assert!(client_with_key("   ").is_none());
    }

    #[test]
    fn new_returns_some_on_present_key() {
        assert!(client_with_key("OPERATOR_PERSONAL_KEY").is_some());
    }

    #[test]
    fn debug_hides_the_api_key() {
        let c = client_with_key("SUPER_SECRET_KEY").unwrap();
        let s = format!("{c:?}");
        assert!(s.contains("FanartClient"));
        assert!(s.contains("has_api_key: true"));
        assert!(!s.contains("SUPER_SECRET_KEY"));
    }

    #[tokio::test]
    async fn empty_mbid_returns_invalid() {
        let c = client_with_key("KEY").unwrap();
        let r = c.get_artist_images("").await;
        assert!(matches!(r, Err(FanartError::Invalid(_))));
        let r = c.get_artist_images("   ").await;
        assert!(matches!(r, Err(FanartError::Invalid(_))));
    }

    #[test]
    fn artist_body_decodes_populated_manifest() {
        let body = r#"{
            "mbid_id": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
            "name": "Radiohead",
            "hdmusiclogo": [
                {"url": "https://cdn.fanart.tv/logo1.png"},
                {"url": "https://cdn.fanart.tv/logo2.png"}
            ],
            "artistbackground": [
                {"url": "https://cdn.fanart.tv/bg1.jpg"}
            ],
            "artistthumb": [{"url": ""}]
        }"#;
        let ab: ArtistBody = serde_json::from_str(body).unwrap();
        assert_eq!(
            ab.mbid_id.as_deref(),
            Some("a74b1b7f-71a5-4011-9441-d0b5e4122711")
        );
        assert_eq!(ab.name.as_deref(), Some("Radiohead"));

        let logos = extract_urls(&ab.hdmusiclogo);
        assert_eq!(logos.len(), 2);
        assert_eq!(logos[0], "https://cdn.fanart.tv/logo1.png");

        let bgs = extract_urls(&ab.artistbackground);
        assert_eq!(bgs.len(), 1);

        // Empty URLs are filtered out — a `{"url": ""}` entry
        // must not produce a rendered CDN link.
        let thumbs = extract_urls(&ab.artistthumb);
        assert!(thumbs.is_empty());
    }

    #[test]
    fn artist_body_decodes_manifest_with_no_arrays() {
        // fanart.tv omits image-type arrays entirely when the
        // artist has no images of that type. Deserializer must
        // treat them as absent, not error.
        let body = r#"{
            "mbid_id": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
            "name": "Some Artist"
        }"#;
        let ab: ArtistBody = serde_json::from_str(body).unwrap();
        assert!(ab.hdmusiclogo.is_none());
        assert!(ab.artistbackground.is_none());
        assert!(extract_urls(&ab.hdmusiclogo).is_empty());
    }

    #[test]
    fn has_any_artwork_reflects_populated_state() {
        let empty = ArtistImagesHit {
            artist_mbid: "x".into(),
            artist_name: None,
            hd_music_logo_urls: Vec::new(),
            hd_artist_logo_urls: Vec::new(),
            artist_background_urls: Vec::new(),
            artist_thumb_urls: Vec::new(),
            music_banner_urls: Vec::new(),
            source_url: "https://fanart.tv/artist/x".into(),
        };
        assert!(!empty.has_any_artwork());

        let populated = ArtistImagesHit {
            hd_music_logo_urls: vec!["https://cdn/logo.png".into()],
            ..empty
        };
        assert!(populated.has_any_artwork());
    }
}
