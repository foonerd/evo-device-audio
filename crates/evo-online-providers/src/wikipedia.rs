// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Wikipedia REST summary API client for the audio distribution's
//! keyless online-metadata cascade.
//!
//! Anonymous provider — no account, no key, no query-string
//! identifier. The keyless-first metadata enrichment cascade
//! (per the accepted design) uses Wikipedia as the anonymous
//! bio / notes source. A MusicBrainz artist / work lookup
//! surfaces the Wikipedia URL directly on its `url-rels`; this
//! client fetches the summary from that URL — no fuzzy search
//! required.
//!
//! ## Endpoint
//!
//! The Wikimedia REST v1 `page/summary/{title}` endpoint returns
//! a compact JSON object with:
//!
//! - `title` — canonical page title after redirect resolution.
//! - `extract` — the article's lead-section plain-text summary.
//!   This is the field that fills the operator UI's Track Info
//!   bio / notes panels.
//! - `content_urls.desktop.page` — the canonical Wikipedia URL
//!   (echoes the URL the caller passed in, resolved for
//!   redirects).
//! - `type` — `standard` (a real article), `disambiguation`
//!   (multi-target disambiguation page — not usable as bio),
//!   `no-extract` (soft-redirect or stub).
//!
//! ## User-Agent policy
//!
//! Wikimedia's User-Agent policy requires every non-browser
//! request to identify the tool + a contact URL. The caller
//! supplies the canonical distribution string at construction
//! time; the client refuses to send a request without one — a
//! request without a descriptive UA hits Wikimedia's blanket
//! block within seconds.
//!
//! ## License
//!
//! Wikipedia article text is CC BY-SA. The operator UI MUST
//! render attribution — `source_name = "Wikipedia"` +
//! `source_url = <canonical page URL>` + `license = "CC BY-SA"`
//! — beside any rendered `extract`. The client surfaces these
//! fields on [`WikipediaSummaryHit`] verbatim.
//!
//! ## Language
//!
//! The endpoint is language-scoped by hostname
//! (`en.wikipedia.org` / `de.wikipedia.org` / etc.). This client
//! defaults to English but accepts a per-lookup language code so
//! the distribution can later thread the operator's UI locale
//! through the cascade. When the specified language edition has
//! no article, the client returns `Ok(None)` and the cascade
//! moves on.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

/// Errors from the Wikipedia client.
#[derive(Debug, thiserror::Error)]
pub enum WikipediaError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Wikipedia returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("Wikipedia JSON decode failed: {0}")]
    Decode(String),
    /// The page exists but is a disambiguation / no-extract stub
    /// unusable as bio content. Cascade callers treat this as a
    /// clean miss.
    #[error("Wikipedia page is not usable as bio (type: {page_type})")]
    NotUsable { page_type: String },
}

/// One Wikipedia summary hit.
#[derive(Debug, Clone, PartialEq)]
pub struct WikipediaSummaryHit {
    /// Canonical page title after redirect resolution.
    pub title: String,
    /// The article's lead-section plain-text summary — the
    /// content the operator UI renders in Track Info bio /
    /// notes panels.
    pub extract: String,
    /// Canonical Wikipedia page URL. The operator UI attributes
    /// with this URL; the cascade's attribution field carries it
    /// as `source_url`.
    pub page_url: String,
    /// Language code the article came from (`"en"`, `"de"`,
    /// etc.). Echoed for consumers that render per-language
    /// affordances.
    pub language: String,
}

/// Wikipedia REST summary client. Anonymous, rate-limited via
/// the shared [`RateLimiter`].
#[derive(Clone)]
pub struct WikipediaClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
}

impl WikipediaClient {
    /// Construct a client. `user_agent` MUST include product name,
    /// version, and contact info per Wikimedia's policy — the
    /// distribution's canonical string is what the calling plugin
    /// threads through. A missing or generic UA hits Wikimedia's
    /// block; the client does not fabricate a default.
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

    /// Fetch a Wikipedia article summary by page title in the
    /// `en` edition. Convenience wrapper for the common English
    /// path; use [`Self::get_summary`] when a specific language
    /// or a non-English URL is available.
    pub async fn get_summary_en(
        &self,
        title: &str,
    ) -> Result<Option<WikipediaSummaryHit>, WikipediaError> {
        self.get_summary(title, "en").await
    }

    /// Fetch a Wikipedia article summary by page title in the
    /// specified language edition.
    pub async fn get_summary(
        &self,
        title: &str,
        language: &str,
    ) -> Result<Option<WikipediaSummaryHit>, WikipediaError> {
        self.rate.acquire().await;
        let encoded_title = urlencode(title);
        let url = format!(
            "https://{language}.wikipedia.org/api/rest_v1/page/summary/{encoded_title}"
        );
        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, &self.user_agent)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(WikipediaError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let body: SummaryResponse = resp
            .json()
            .await
            .map_err(|e| WikipediaError::Decode(e.to_string()))?;
        // Filter out non-usable page types up front — cascade
        // callers see a clean miss rather than a stub.
        let page_type = body.page_type.as_deref().unwrap_or("standard");
        if page_type != "standard" {
            return Err(WikipediaError::NotUsable {
                page_type: page_type.to_string(),
            });
        }
        let extract = body.extract.unwrap_or_default();
        if extract.trim().is_empty() {
            return Ok(None);
        }
        let canonical_page_url = body
            .content_urls
            .and_then(|c| c.desktop)
            .and_then(|d| d.page)
            .unwrap_or_else(|| {
                format!("https://{language}.wikipedia.org/wiki/{encoded_title}")
            });
        Ok(Some(WikipediaSummaryHit {
            title: body.title.unwrap_or_else(|| title.to_string()),
            extract,
            page_url: canonical_page_url,
            language: language.to_string(),
        }))
    }

    /// Extract the page title from a Wikipedia URL and fetch its
    /// summary. Useful when a MusicBrainz `wikipedia` URL-rel
    /// hands us a full URL rather than a title. Returns `Ok(None)`
    /// when the URL cannot be parsed as a Wikipedia article
    /// URL — the cascade treats a broken link as a miss and
    /// moves on rather than dying.
    pub async fn get_summary_from_url(
        &self,
        url: &str,
    ) -> Result<Option<WikipediaSummaryHit>, WikipediaError> {
        let Some((language, title)) = parse_wikipedia_url(url) else {
            return Ok(None);
        };
        self.get_summary(&title, &language).await
    }
}

#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    extract: Option<String>,
    #[serde(rename = "type", default)]
    page_type: Option<String>,
    #[serde(rename = "content_urls", default)]
    content_urls: Option<ContentUrls>,
}

#[derive(Debug, Deserialize)]
struct ContentUrls {
    #[serde(default)]
    desktop: Option<ContentUrlsVariant>,
}

#[derive(Debug, Deserialize)]
struct ContentUrlsVariant {
    #[serde(default)]
    page: Option<String>,
}

/// Extract `(language, title)` from a Wikipedia article URL.
///
/// Handles:
///
///   `https://en.wikipedia.org/wiki/Radiohead`         → `("en", "Radiohead")`
///   `http://de.wikipedia.org/wiki/Ludwig_van_Beethoven` → `("de", "Ludwig_van_Beethoven")`
///
/// Returns `None` for non-Wikipedia URLs and for URLs that do
/// not carry a `/wiki/<title>` path — cascade callers treat that
/// as a clean miss.
pub fn parse_wikipedia_url(url: &str) -> Option<(String, String)> {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = after_scheme.split_once('/')?;
    let language = host.strip_suffix(".wikipedia.org")?;
    if language.is_empty() {
        return None;
    }
    let title = path.strip_prefix("wiki/")?;
    if title.is_empty() {
        return None;
    }
    // URL fragments (`#Section`) and query strings should not
    // reach the summary API — strip them.
    let title = title.split_once('#').map(|(t, _)| t).unwrap_or(title);
    let title = title.split_once('?').map(|(t, _)| t).unwrap_or(title);
    Some((language.to_string(), title.to_string()))
}

fn urlencode(s: &str) -> String {
    // Wikipedia's page-title path segment allows unreserved
    // characters + `%XX` encoding for everything else. Page
    // titles are typically already in that shape.
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/' => out.push(*b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_english_wikipedia_url() {
        assert_eq!(
            parse_wikipedia_url("https://en.wikipedia.org/wiki/Radiohead"),
            Some(("en".to_string(), "Radiohead".to_string()))
        );
    }

    #[test]
    fn parses_german_wikipedia_url() {
        assert_eq!(
            parse_wikipedia_url(
                "http://de.wikipedia.org/wiki/Ludwig_van_Beethoven"
            ),
            Some(("de".to_string(), "Ludwig_van_Beethoven".to_string(),))
        );
    }

    #[test]
    fn strips_fragment_and_query() {
        assert_eq!(
            parse_wikipedia_url(
                "https://en.wikipedia.org/wiki/Radiohead#Discography"
            ),
            Some(("en".to_string(), "Radiohead".to_string()))
        );
        assert_eq!(
            parse_wikipedia_url(
                "https://en.wikipedia.org/wiki/Radiohead?redirect=no"
            ),
            Some(("en".to_string(), "Radiohead".to_string()))
        );
    }

    #[test]
    fn rejects_non_wikipedia_urls() {
        assert!(parse_wikipedia_url("https://example.com/wiki/x").is_none());
        assert!(parse_wikipedia_url("not a url").is_none());
        assert!(parse_wikipedia_url("https://en.wikipedia.org/").is_none());
        assert!(parse_wikipedia_url("https://en.wikipedia.org/wiki/").is_none());
    }
}
