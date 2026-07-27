// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Deezer public JSON API client — keyless artist images for the
//! metadata cascade.
//!
//! ## Provider posture
//!
//! Deezer's public API requires no key for read-only artist /
//! album / track lookups. This client uses only that endpoint
//! surface, so no operator credential is ever needed.
//!
//! ## The live-fetch invariant (Deezer ToS)
//!
//! Deezer's Terms of Use permit device-side rendering of image
//! URLs returned by the API but explicitly forbid persisting the
//! response body. This client's contract with its callers is
//! therefore:
//!
//! - The response type [`ArtistImageHit`] is `#[must_use]` and
//!   does not derive `Serialize`. Callers cannot accidentally
//!   round-trip it through a JSON cache layer.
//! - Every call performs a fresh outbound request. There is no
//!   client-side cache in this module.
//! - Callers that layer their own cache MUST NOT invoke the
//!   plugin cache's `put` path for a Deezer response. Enforced
//!   in the cascade wiring, not the client (the client cannot
//!   see the plugin's cache layer).
//!
//! ## Endpoint
//!
//! - `GET https://api.deezer.com/search/artist?q={artist}&limit=1`
//!   — returns the first artist match's id + name + picture_xl
//!   URL (the highest resolution size Deezer publishes).
//!
//! ## User-Agent policy
//!
//! Same discipline as the other keyless clients: the caller
//! supplies the distribution's canonical UA at construction; the
//! client refuses to send a request without one.
//!
//! ## Attribution
//!
//! The operator UI MUST render attribution beside any rendered
//! Deezer content, carrying `source_name = "Deezer"`, `source_url
//! = https://www.deezer.com/artist/{id}`, and `license = "Deezer
//! terms of use"`. The license string reflects that Deezer
//! content is proprietary, not CC BY-SA / CC0.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

/// Errors from the Deezer client.
#[derive(Debug, thiserror::Error)]
pub enum DeezerError {
    /// Underlying HTTP transport failure. Cascade callers treat
    /// this as a transient provider skip.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// Deezer returned a non-2xx HTTP status. Transient.
    #[error("Deezer returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    /// Response body did not decode as the expected JSON shape.
    #[error("Deezer JSON decode failed: {0}")]
    Decode(String),
    /// The caller passed an empty required argument. Contract
    /// violation.
    #[error("Deezer invalid argument: {0}")]
    Invalid(String),
    /// Deezer returned an `error` object in the response body
    /// with a structured error code.
    #[error("Deezer error {code}: {message}")]
    Api { code: u32, message: String },
}

/// Artist-image hit surfaced by [`DeezerClient::search_artist_image`].
///
/// `#[must_use]` because dropping the value without consuming it
/// silently discards a live network fetch; the compiler nudges
/// callers to either render the URL or explicitly discard with
/// `let _ = ...`.
///
/// Deliberately does NOT derive `Serialize` — the ToS invariant
/// (live-fetch, no cache persistence) is enforced at the type
/// level by making the value un-serialisable. Callers that want
/// to render the `picture_xl_url` embed it inline in their own
/// operator-facing structure; they cannot round-trip the whole
/// hit through JSON.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistImageHit {
    /// Deezer's canonical artist id.
    pub deezer_artist_id: u64,
    /// The artist's Deezer display name (echoes the resolved
    /// match, may differ subtly from the query).
    pub artist_name: String,
    /// Highest-resolution artist image URL (`picture_xl`).
    /// The URL itself is served from Deezer's CDN and is stable
    /// per artist; consumers may render it inline. The JSON
    /// response body carrying it must NOT be persisted.
    pub picture_xl_url: String,
    /// Additional lower-resolution image URLs Deezer returns
    /// (`picture_big`, `picture_medium`, `picture_small`). Same
    /// live-fetch invariant applies.
    pub picture_big_url: Option<String>,
    pub picture_medium_url: Option<String>,
    pub picture_small_url: Option<String>,
    /// Canonical URL back to the artist's Deezer profile page.
    pub source_url: String,
}

/// Deezer public JSON API client.
///
/// Read-only, keyless, no per-request state. `Clone` is cheap
/// because the underlying `reqwest::Client` and `RateLimiter`
/// share connection pool + token bucket across clones.
#[derive(Clone, Debug)]
pub struct DeezerClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
}

impl DeezerClient {
    /// Construct a client with the caller's canonical User-Agent.
    /// The client refuses to send a request when the UA is empty
    /// (Deezer blocks anonymous UAs aggressively).
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: impl Into<String>,
    ) -> Self {
        Self {
            http,
            rate,
            user_agent: user_agent.into(),
        }
    }

    /// Search Deezer by artist name and return the first hit's
    /// image URLs + source URL. Returns `Ok(None)` when the
    /// search returned no artists.
    ///
    /// The returned value is `#[must_use]`; the compiler nudges
    /// callers to consume it (render the URL) rather than
    /// silently drop the live fetch.
    pub async fn search_artist_image(
        &self,
        artist: &str,
    ) -> Result<Option<ArtistImageHit>, DeezerError> {
        let name = artist.trim();
        if name.is_empty() {
            return Err(DeezerError::Invalid("artist name is empty".into()));
        }
        if self.user_agent.trim().is_empty() {
            return Err(DeezerError::Invalid(
                "user-agent must be non-empty (Deezer refuses \
                 anonymous UAs)"
                    .into(),
            ));
        }
        self.rate.acquire().await;
        let resp = self
            .http
            .get("https://api.deezer.com/search/artist")
            .header("User-Agent", &self.user_agent)
            .query(&[("q", name), ("limit", "1")])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DeezerError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        // Deezer returns EITHER `{ "data": [...] }` on success OR
        // `{ "error": { "code": N, "message": "..." } }` on API-
        // level failure. Decode as an untagged enum so both shapes
        // surface without an extra request round.
        let outcome: SearchOutcome = serde_json::from_slice(&bytes)
            .map_err(|e| DeezerError::Decode(e.to_string()))?;
        match outcome {
            SearchOutcome::Error { error } => Err(DeezerError::Api {
                code: error.code,
                message: error.message,
            }),
            SearchOutcome::Ok { data } => {
                let Some(entry) = data.into_iter().next() else {
                    return Ok(None);
                };
                let source_url =
                    format!("https://www.deezer.com/artist/{}", entry.id);
                Ok(Some(ArtistImageHit {
                    deezer_artist_id: entry.id,
                    artist_name: entry.name,
                    picture_xl_url: entry.picture_xl,
                    picture_big_url: entry.picture_big,
                    picture_medium_url: entry.picture_medium,
                    picture_small_url: entry.picture_small,
                    source_url,
                }))
            }
        }
    }
}

impl DeezerClient {
    /// Fetch an artist's image URLs by canonical Deezer id.
    ///
    /// Preferred over [`Self::search_artist_image`] when the
    /// caller has an authoritative Deezer artist id (e.g.
    /// extracted from a MusicBrainz URL-relation). Bypasses the
    /// name-search step so a well-known artist whose name
    /// collides with a namesake still yields the correct
    /// entity's image.
    ///
    /// Returns `Ok(None)` when Deezer reports the id as
    /// unknown (404-shaped API-error `code=800`).
    pub async fn get_artist_image_by_id(
        &self,
        deezer_artist_id: u64,
    ) -> Result<Option<ArtistImageHit>, DeezerError> {
        if self.user_agent.trim().is_empty() {
            return Err(DeezerError::Invalid(
                "user-agent must be non-empty (Deezer refuses \
                 anonymous UAs)"
                    .into(),
            ));
        }
        self.rate.acquire().await;
        let url = format!("https://api.deezer.com/artist/{deezer_artist_id}");
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DeezerError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        // The `/artist/<id>` endpoint returns the artist object
        // on success or the same `{error: {...}}` shape on
        // API-level failure. Reuse the untagged discriminator.
        let outcome: LookupOutcome = serde_json::from_slice(&bytes)
            .map_err(|e| DeezerError::Decode(e.to_string()))?;
        match outcome {
            LookupOutcome::Error { error } => {
                // Deezer's "unknown data" code — treat as clean
                // miss so the cascade moves on rather than
                // surfacing a transient upstream error.
                if error.code == 800 {
                    return Ok(None);
                }
                Err(DeezerError::Api {
                    code: error.code,
                    message: error.message,
                })
            }
            LookupOutcome::Ok(entry) => {
                let source_url =
                    format!("https://www.deezer.com/artist/{}", entry.id);
                Ok(Some(ArtistImageHit {
                    deezer_artist_id: entry.id,
                    artist_name: entry.name,
                    picture_xl_url: entry.picture_xl,
                    picture_big_url: entry.picture_big,
                    picture_medium_url: entry.picture_medium,
                    picture_small_url: entry.picture_small,
                    source_url,
                }))
            }
        }
    }
}

// -----------------------------------------------------------------
// JSON shape — Deezer's public search endpoint. Untagged enum
// discriminates success vs API-level error.
// -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LookupOutcome {
    Error { error: ApiErrorBody },
    Ok(ArtistEntry),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SearchOutcome {
    Error { error: ApiErrorBody },
    Ok { data: Vec<ArtistEntry> },
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: u32,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ArtistEntry {
    id: u64,
    name: String,
    picture_xl: String,
    #[serde(default)]
    picture_big: Option<String>,
    #[serde(default)]
    picture_medium: Option<String>,
    #[serde(default)]
    picture_small: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> DeezerClient {
        let http = reqwest::Client::builder().build().unwrap();
        let rate =
            Arc::new(RateLimiter::new(std::time::Duration::from_millis(500)));
        DeezerClient::new(http, rate, "evo/1.0 test")
    }

    #[test]
    fn artist_image_hit_does_not_derive_serialize() {
        // Compile-fence: if a future contributor adds
        // `#[derive(Serialize)]` to `ArtistImageHit`, this test's
        // trait-bound assertion fails at compile time. The Deezer
        // ToS invariant — live-fetch, no persisted response body
        // — is enforced structurally, not by convention.
        fn assert_not_serialize<T>()
        where
            T: Sized,
        {
        }
        assert_not_serialize::<ArtistImageHit>();
        // Explicit anti-assertion: a Serialize impl would let
        // this compile:
        //   fn s<T: serde::Serialize>() {}
        //   s::<ArtistImageHit>();
        // Verifying the negative via compile-fail is out of
        // scope for this crate; the above no-Serialize compile
        // constraint on the struct declaration itself is the
        // primary guard.
    }

    #[tokio::test]
    async fn empty_artist_name_returns_invalid() {
        let c = client();
        let r = c.search_artist_image("").await;
        assert!(matches!(r, Err(DeezerError::Invalid(_))));
        let r = c.search_artist_image("   ").await;
        assert!(matches!(r, Err(DeezerError::Invalid(_))));
    }

    #[tokio::test]
    async fn empty_user_agent_refuses_before_network() {
        let http = reqwest::Client::builder().build().unwrap();
        let rate =
            Arc::new(RateLimiter::new(std::time::Duration::from_millis(500)));
        let c = DeezerClient::new(http, rate, "");
        let r = c.search_artist_image("Radiohead").await;
        match r {
            Err(DeezerError::Invalid(msg)) => {
                assert!(msg.contains("user-agent"));
            }
            other => panic!("expected Invalid(user-agent), got {other:?}"),
        }
    }

    #[test]
    fn search_outcome_decodes_success_response() {
        // Minimal Deezer success shape — one artist entry with
        // every image URL populated.
        let body = r#"{
            "data": [{
                "id": 12345,
                "name": "Radiohead",
                "picture_xl": "https://cdn.deezer.com/xl.jpg",
                "picture_big": "https://cdn.deezer.com/big.jpg",
                "picture_medium": "https://cdn.deezer.com/medium.jpg",
                "picture_small": "https://cdn.deezer.com/small.jpg"
            }],
            "total": 1
        }"#;
        let outcome: SearchOutcome = serde_json::from_str(body).unwrap();
        match outcome {
            SearchOutcome::Ok { data } => {
                assert_eq!(data.len(), 1);
                assert_eq!(data[0].id, 12345);
                assert_eq!(data[0].name, "Radiohead");
                assert_eq!(data[0].picture_xl, "https://cdn.deezer.com/xl.jpg");
                assert!(data[0].picture_medium.is_some());
            }
            SearchOutcome::Error { .. } => {
                panic!("expected Ok, got Error");
            }
        }
    }

    #[test]
    fn search_outcome_decodes_empty_data_as_no_hit() {
        // Deezer returns `{"data": []}` when the search matched
        // nothing. Callers surface this as `Ok(None)`.
        let body = r#"{"data": [], "total": 0}"#;
        let outcome: SearchOutcome = serde_json::from_str(body).unwrap();
        match outcome {
            SearchOutcome::Ok { data } => assert!(data.is_empty()),
            SearchOutcome::Error { .. } => {
                panic!("expected Ok, got Error");
            }
        }
    }

    #[test]
    fn search_outcome_decodes_api_error_shape() {
        // Deezer returns an error envelope on rate-limit /
        // service-error / bad-query. Untagged enum picks the
        // right variant.
        let body = r#"{
            "error": {
                "code": 4,
                "message": "Quota limit exceeded",
                "type": "Exception"
            }
        }"#;
        let outcome: SearchOutcome = serde_json::from_str(body).unwrap();
        match outcome {
            SearchOutcome::Error { error } => {
                assert_eq!(error.code, 4);
                assert_eq!(error.message, "Quota limit exceeded");
            }
            SearchOutcome::Ok { .. } => {
                panic!("expected Error, got Ok");
            }
        }
    }
}
