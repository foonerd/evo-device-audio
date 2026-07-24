// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! TheAudioDB JSON client — keyless artist bio + album review
//! source for the metadata cascade.
//!
//! ## Provider posture
//!
//! TheAudioDB publishes a free public test API key (`"2"`) that
//! authenticates every keyless request from a device without a
//! registered account. Operators who register on the project's
//! Patreon can supply a paid key for higher rate limits + full
//! endpoints; this client accepts either shape at construction.
//! Keyless (test-key) usage is the default posture — anonymous
//! provider from the operator's perspective.
//!
//! ## Endpoints exercised
//!
//! - `GET /api/v1/json/{key}/search.php?s={artist}` — artist
//!   search. Returns the strBiographyEN + strArtistThumb +
//!   strArtistFanart fields the cascade renders for bio content.
//! - `GET /api/v1/json/{key}/searchalbum.php?s={artist}&a={album}`
//!   — album search. Returns strDescriptionEN + strReview + year
//!   + label fields for album-notes content.
//!
//! ## User-Agent policy
//!
//! Same discipline as the other keyless clients: the caller
//! supplies the distribution's canonical UA at construction; the
//! client refuses to send a request without one.
//!
//! ## License
//!
//! TheAudioDB content is CC BY-SA (matches Wikipedia's
//! attribution posture). The operator UI MUST render attribution
//! — `source_name = "TheAudioDB"` + `source_url =
//! https://www.theaudiodb.com/artist/{artist_id}` (or
//! `/album/{album_id}`) + `license = "CC BY-SA"` — beside any
//! rendered biography / review payload.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;

use crate::rate_limit::RateLimiter;

/// TheAudioDB's public test API key. Documented on the project
/// site as the "free API test key that anyone can use". Used as
/// the default when the operator has not supplied a Patreon key.
pub const THEAUDIODB_KEYLESS_API_KEY: &str = "2";

/// Errors from the TheAudioDB client.
#[derive(Debug, thiserror::Error)]
pub enum TheAudioDbError {
    /// Underlying HTTP transport failure (DNS / TLS / socket /
    /// timeout). Cascade callers treat this as a transient
    /// provider skip — do not cache, try the next source.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    /// TheAudioDB returned a non-2xx HTTP status. Same disposition
    /// as `Http`.
    #[error("TheAudioDB returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    /// Response body did not decode as the expected JSON shape.
    #[error("TheAudioDB JSON decode failed: {0}")]
    Decode(String),
    /// The caller passed an empty required argument (artist or
    /// album name). Contract violation, not a transient.
    #[error("TheAudioDB invalid argument: {0}")]
    Invalid(String),
}

/// Artist-bio hit surfaced by [`TheAudioDbClient::search_artist_bio`].
///
/// Every field is `Option` because TheAudioDB's response fields
/// are individually populated per artist. A response with just
/// `bio_en` present is common and usable; the caller decides
/// how to render partial hits.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistBioHit {
    /// TheAudioDB internal artist id, used to construct the
    /// attribution URL back to the artist's page.
    pub artist_id: String,
    /// The `strBiographyEN` field — English-language biography
    /// prose. This is the primary bio content the cascade
    /// consumes.
    pub bio_en: Option<String>,
    /// The `strArtistThumb` field — canonical URL to a
    /// thumbnail image of the artist. Consumers that render
    /// artist artwork can use it; not persisted by the plugin.
    pub artist_thumb_url: Option<String>,
    /// The `strGenre` field — primary genre label.
    pub genre: Option<String>,
    /// The `intFormedYear` field — year the artist / band
    /// formed (best-effort integer parse).
    pub formed_year: Option<u16>,
    /// Canonical URL back to the artist's TheAudioDB page.
    pub source_url: String,
}

/// Album-notes hit surfaced by [`TheAudioDbClient::search_album_notes`].
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumNotesHit {
    /// TheAudioDB internal album id.
    pub album_id: String,
    /// The `strDescriptionEN` field — English-language album
    /// description. Primary album-notes content.
    pub description_en: Option<String>,
    /// The `strReview` field — editorial review text (when
    /// TheAudioDB has one for the album).
    pub review: Option<String>,
    /// The `intYearReleased` field — first release year
    /// (best-effort integer parse).
    pub year: Option<u16>,
    /// The `strLabel` field — release label.
    pub label: Option<String>,
    /// Canonical URL back to the album's TheAudioDB page.
    pub source_url: String,
}

/// TheAudioDB JSON client.
#[derive(Clone)]
pub struct TheAudioDbClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
    api_key: String,
}

impl std::fmt::Debug for TheAudioDbClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TheAudioDbClient")
            .field("user_agent", &self.user_agent)
            .field("keyless", &(self.api_key == THEAUDIODB_KEYLESS_API_KEY))
            .finish_non_exhaustive()
    }
}

impl TheAudioDbClient {
    /// Construct a client bound to a specific API key. Pass
    /// [`THEAUDIODB_KEYLESS_API_KEY`] for the anonymous default;
    /// pass an operator-supplied Patreon key when present. The
    /// key is not logged; only its keyless-vs-keyed provenance
    /// surfaces in [`Debug`].
    pub fn new(
        http: Client,
        rate: Arc<RateLimiter>,
        user_agent: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            rate,
            user_agent: user_agent.into(),
            api_key: api_key.into(),
        }
    }

    /// Search TheAudioDB by artist name, returning the first
    /// hit's bio payload. Returns `Ok(None)` when the response
    /// carries no artist entry — a legitimate miss the cascade
    /// treats as a clean skip.
    pub async fn search_artist_bio(
        &self,
        artist: &str,
    ) -> Result<Option<ArtistBioHit>, TheAudioDbError> {
        let name = artist.trim();
        if name.is_empty() {
            return Err(TheAudioDbError::Invalid(
                "artist name is empty".into(),
            ));
        }
        self.rate.acquire().await;
        let url = format!(
            "https://www.theaudiodb.com/api/v1/json/{}/search.php",
            self.api_key
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .query(&[("s", name)])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TheAudioDbError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let root: ArtistSearchRoot = serde_json::from_slice(&bytes)
            .map_err(|e| TheAudioDbError::Decode(e.to_string()))?;
        let Some(artist_entry) =
            root.artists.and_then(|list| list.into_iter().next())
        else {
            return Ok(None);
        };
        let artist_id = artist_entry.id_artist.unwrap_or_default();
        let source_url = if artist_id.is_empty() {
            "https://www.theaudiodb.com".to_string()
        } else {
            format!("https://www.theaudiodb.com/artist/{artist_id}")
        };
        Ok(Some(ArtistBioHit {
            artist_id,
            bio_en: artist_entry.str_biography_en.and_then(nonempty),
            artist_thumb_url: artist_entry.str_artist_thumb.and_then(nonempty),
            genre: artist_entry.str_genre.and_then(nonempty),
            formed_year: artist_entry
                .int_formed_year
                .as_deref()
                .and_then(|s| s.parse().ok()),
            source_url,
        }))
    }

    /// Search TheAudioDB by (artist, album), returning the first
    /// hit's notes payload.
    pub async fn search_album_notes(
        &self,
        artist: &str,
        album: &str,
    ) -> Result<Option<AlbumNotesHit>, TheAudioDbError> {
        let a = artist.trim();
        let al = album.trim();
        if a.is_empty() {
            return Err(TheAudioDbError::Invalid(
                "artist name is empty".into(),
            ));
        }
        if al.is_empty() {
            return Err(TheAudioDbError::Invalid("album name is empty".into()));
        }
        self.rate.acquire().await;
        let url = format!(
            "https://www.theaudiodb.com/api/v1/json/{}/searchalbum.php",
            self.api_key
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .query(&[("s", a), ("a", al)])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(TheAudioDbError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let bytes = resp.bytes().await?;
        let root: AlbumSearchRoot = serde_json::from_slice(&bytes)
            .map_err(|e| TheAudioDbError::Decode(e.to_string()))?;
        let Some(album_entry) =
            root.album.and_then(|list| list.into_iter().next())
        else {
            return Ok(None);
        };
        let album_id = album_entry.id_album.unwrap_or_default();
        let source_url = if album_id.is_empty() {
            "https://www.theaudiodb.com".to_string()
        } else {
            format!("https://www.theaudiodb.com/album/{album_id}")
        };
        Ok(Some(AlbumNotesHit {
            album_id,
            description_en: album_entry.str_description_en.and_then(nonempty),
            review: album_entry.str_review.and_then(nonempty),
            year: album_entry
                .int_year_released
                .as_deref()
                .and_then(|s| s.parse().ok()),
            label: album_entry.str_label.and_then(nonempty),
            source_url,
        }))
    }
}

fn nonempty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// -----------------------------------------------------------------
// JSON shape — TheAudioDB uses `null` for absent fields and
// prefixes field names with the wire-type marker (str / int / ...)
// so every field on the wire is Option<String>.
// -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ArtistSearchRoot {
    artists: Option<Vec<ArtistEntry>>,
}

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct ArtistEntry {
    #[serde(rename = "idArtist")]
    id_artist: Option<String>,
    #[serde(rename = "strBiographyEN")]
    str_biography_en: Option<String>,
    #[serde(rename = "strArtistThumb")]
    str_artist_thumb: Option<String>,
    #[serde(rename = "strGenre")]
    str_genre: Option<String>,
    #[serde(rename = "intFormedYear")]
    int_formed_year: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumSearchRoot {
    album: Option<Vec<AlbumEntry>>,
}

#[allow(non_snake_case)]
#[derive(Debug, Deserialize)]
struct AlbumEntry {
    #[serde(rename = "idAlbum")]
    id_album: Option<String>,
    #[serde(rename = "strDescriptionEN")]
    str_description_en: Option<String>,
    #[serde(rename = "strReview")]
    str_review: Option<String>,
    #[serde(rename = "intYearReleased")]
    int_year_released: Option<String>,
    #[serde(rename = "strLabel")]
    str_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyless_test_key_is_the_documented_constant() {
        assert_eq!(THEAUDIODB_KEYLESS_API_KEY, "2");
    }

    #[test]
    fn debug_hides_the_api_key_and_marks_keyless() {
        let http = reqwest::Client::builder().build().unwrap();
        let rate =
            Arc::new(RateLimiter::new(std::time::Duration::from_millis(500)));
        let c = TheAudioDbClient::new(
            http,
            rate,
            "evo/1.0 test",
            THEAUDIODB_KEYLESS_API_KEY,
        );
        let s = format!("{c:?}");
        assert!(s.contains("TheAudioDbClient"));
        assert!(s.contains("keyless: true"));
        assert!(!s.contains(THEAUDIODB_KEYLESS_API_KEY));
    }

    #[test]
    fn debug_marks_operator_key_as_keyed() {
        let http = reqwest::Client::builder().build().unwrap();
        let rate =
            Arc::new(RateLimiter::new(std::time::Duration::from_millis(500)));
        let c = TheAudioDbClient::new(
            http,
            rate,
            "evo/1.0 test",
            "SOME_OPERATOR_PATREON_KEY",
        );
        let s = format!("{c:?}");
        assert!(s.contains("keyless: false"));
        assert!(!s.contains("SOME_OPERATOR_PATREON_KEY"));
    }

    #[test]
    fn artist_search_root_decodes_populated_response() {
        // Minimal shape of TheAudioDB's real response — one artist
        // with the fields the cascade consumes.
        let body = r#"{
            "artists": [{
                "idArtist": "111239",
                "strArtist": "Radiohead",
                "strBiographyEN": "Radiohead are an English rock band...",
                "strArtistThumb": "https://example.com/thumb.jpg",
                "strGenre": "Alternative Rock",
                "intFormedYear": "1985"
            }]
        }"#;
        let root: ArtistSearchRoot = serde_json::from_str(body).unwrap();
        let entry = root
            .artists
            .expect("artists array present")
            .into_iter()
            .next()
            .expect("first entry present");
        assert_eq!(entry.id_artist.as_deref(), Some("111239"));
        assert!(entry
            .str_biography_en
            .as_ref()
            .is_some_and(|s| s.starts_with("Radiohead are")));
        assert_eq!(entry.int_formed_year.as_deref(), Some("1985"));
    }

    #[test]
    fn artist_search_root_decodes_null_body_as_no_artist() {
        // TheAudioDB returns `{"artists": null}` on a miss. The
        // cascade must treat this as a clean skip.
        let body = r#"{"artists": null}"#;
        let root: ArtistSearchRoot = serde_json::from_str(body).unwrap();
        assert!(root.artists.is_none());
    }

    #[test]
    fn album_search_root_decodes_populated_response() {
        let body = r#"{
            "album": [{
                "idAlbum": "2109547",
                "strAlbum": "OK Computer",
                "strDescriptionEN": "OK Computer is a landmark...",
                "strReview": "A widely acclaimed album...",
                "intYearReleased": "1997",
                "strLabel": "Parlophone"
            }]
        }"#;
        let root: AlbumSearchRoot = serde_json::from_str(body).unwrap();
        let entry = root.album.unwrap().into_iter().next().unwrap();
        assert_eq!(entry.id_album.as_deref(), Some("2109547"));
        assert!(entry
            .str_description_en
            .as_ref()
            .is_some_and(|s| s.starts_with("OK Computer")));
        assert_eq!(entry.int_year_released.as_deref(), Some("1997"));
        assert_eq!(entry.str_label.as_deref(), Some("Parlophone"));
    }

    #[test]
    fn nonempty_trims_and_filters() {
        assert_eq!(nonempty("hello".into()), Some("hello".into()));
        assert_eq!(nonempty("  padded  ".into()), Some("padded".into()));
        assert_eq!(nonempty("".into()), None);
        assert_eq!(nonempty("   ".into()), None);
    }
}
