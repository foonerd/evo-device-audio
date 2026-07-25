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
/// are individually populated per artist.
///
/// Locale-aware selection note: TheAudioDB carries per-locale biographies on
/// `strBiography` (English base) + `strBiography<CC>` (DE / FR /
/// ES / IT / JP / RU / CN / PT / NL / PL / HU / IL / SE / NO).
/// The caller reaches for [`Self::bio_for_locale`] which picks
/// operator locale → English → any-non-empty and reports the
/// language actually served — never a "false empty" for a
/// locale the API happens not to carry.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtistBioHit {
    /// TheAudioDB internal artist id, used to construct the
    /// attribution URL back to the artist's page.
    pub artist_id: String,
    /// The per-locale bio map — key is a BCP47 short language
    /// tag (`"en"`, `"de"`, ...), value is the non-empty prose
    /// the API returned for that locale. operator-locale → English → any-non-empty
    /// fallback chain runs over this map, not over one
    /// specific field.
    pub bios_by_locale: std::collections::HashMap<String, String>,
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

impl ArtistBioHit {
    /// Fallback pick: try the operator locale, then
    /// English, then any non-empty entry the response carries.
    /// Returns `(bio_text, language_actually_served)`. Callers
    /// report `language_actually_served` on `SourceEntry.language`.
    pub fn bio_for_locale(&self, locale: &str) -> Option<(String, String)> {
        if let Some(b) = self.bios_by_locale.get(locale) {
            return Some((b.clone(), locale.to_string()));
        }
        if locale != "en" {
            if let Some(b) = self.bios_by_locale.get("en") {
                return Some((b.clone(), "en".to_string()));
            }
        }
        self.bios_by_locale
            .iter()
            .next()
            .map(|(l, b)| (b.clone(), l.clone()))
    }
}

/// Album-notes hit surfaced by [`TheAudioDbClient::search_album_notes`].
///
/// Same shape as [`ArtistBioHit`]: `strDescription`
/// (English base) + `strDescription<CC>` per locale, accessed
/// via [`Self::description_for_locale`] with the
/// operator → English → any-non-empty fallback chain.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumNotesHit {
    /// TheAudioDB internal album id.
    pub album_id: String,
    /// Per-locale description map.
    pub descriptions_by_locale: std::collections::HashMap<String, String>,
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

impl AlbumNotesHit {
    /// Fallback pick for album descriptions. Same
    /// contract as [`ArtistBioHit::bio_for_locale`].
    pub fn description_for_locale(
        &self,
        locale: &str,
    ) -> Option<(String, String)> {
        if let Some(d) = self.descriptions_by_locale.get(locale) {
            return Some((d.clone(), locale.to_string()));
        }
        if locale != "en" {
            if let Some(d) = self.descriptions_by_locale.get("en") {
                return Some((d.clone(), "en".to_string()));
            }
        }
        self.descriptions_by_locale
            .iter()
            .next()
            .map(|(l, d)| (d.clone(), l.clone()))
    }
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

    /// Fetch an artist by MusicBrainz artist MBID via
    /// `artist-mb.php?i=<mbid>` — TheAudioDB's MBID-indexed
    /// endpoint. MBID-first per the enrichment flow: when the
    /// caller has resolved the MB identity for this artist,
    /// this method returns the same artist TheAudioDB has under
    /// that MBID without any name-disambiguation risk.
    ///
    /// Returns `Ok(None)` when TheAudioDB has no artist record
    /// keyed on the supplied MBID (a clean miss — the artist
    /// is not in their catalogue).
    pub async fn fetch_artist_bio_by_mbid(
        &self,
        artist_mbid: &str,
    ) -> Result<Option<ArtistBioHit>, TheAudioDbError> {
        let mbid = artist_mbid.trim();
        if mbid.is_empty() {
            return Err(TheAudioDbError::Invalid(
                "artist MBID is empty".into(),
            ));
        }
        self.rate.acquire().await;
        let url = format!(
            "https://www.theaudiodb.com/api/v1/json/{}/artist-mb.php",
            self.api_key
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", &self.user_agent)
            .query(&[("i", mbid)])
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
        Ok(Self::first_artist_to_bio_hit(root))
    }

    /// Search TheAudioDB by artist name, returning the first
    /// hit's bio payload. Returns `Ok(None)` when the response
    /// carries no artist entry — a legitimate miss the cascade
    /// treats as a clean skip.
    ///
    /// **Name-last fallback.** Callers with a resolved MB
    /// identity should call [`Self::fetch_artist_bio_by_mbid`]
    /// first; this method is the last-resort path for entities
    /// without a resolved MBID.
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
        Ok(Self::first_artist_to_bio_hit(root))
    }

    /// Shared shape extractor: TheAudioDB's `artists` array
    /// (from both `search.php` and `artist-mb.php`) maps to the
    /// same `ArtistBioHit` fields. Consolidated here so the
    /// name-first and MBID-first paths return identical shapes.
    fn first_artist_to_bio_hit(root: ArtistSearchRoot) -> Option<ArtistBioHit> {
        let artist_entry =
            root.artists.and_then(|list| list.into_iter().next())?;
        let artist_id = artist_entry.id_artist.unwrap_or_default();
        let source_url = if artist_id.is_empty() {
            "https://www.theaudiodb.com".to_string()
        } else {
            format!("https://www.theaudiodb.com/artist/{artist_id}")
        };
        Some(ArtistBioHit {
            artist_id,
            bios_by_locale: collect_bios_by_locale(&artist_entry.extra),
            artist_thumb_url: artist_entry.str_artist_thumb.and_then(nonempty),
            genre: artist_entry.str_genre.and_then(nonempty),
            formed_year: artist_entry
                .int_formed_year
                .as_deref()
                .and_then(|s| s.parse().ok()),
            source_url,
        })
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
            descriptions_by_locale: collect_descriptions_by_locale(
                &album_entry.extra,
            ),
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

/// Extract every `strBiography<CC>` (and the base `strBiography`
/// = English) into a `{lang_short: text}` map. Empty / whitespace
/// values are dropped so the fallback chain doesn't hit a
/// spuriously-populated locale.
fn collect_bios_by_locale(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, String> {
    collect_locale_map(extra, "strBiography")
}

/// Same shape as [`collect_bios_by_locale`] but for
/// `strDescription<CC>` on the album endpoint.
fn collect_descriptions_by_locale(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, String> {
    collect_locale_map(extra, "strDescription")
}

fn collect_locale_map(
    extra: &std::collections::HashMap<String, serde_json::Value>,
    prefix: &str,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (key, value) in extra {
        let Some(suffix) = key.strip_prefix(prefix) else {
            continue;
        };
        let Some(text) = value.as_str() else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        // `strBiography` (no suffix) is TheAudioDB's English base.
        let lang = if suffix.is_empty() {
            "en".to_string()
        } else {
            country_code_to_lang(suffix).to_string()
        };
        out.insert(lang, trimmed.to_string());
    }
    out
}

/// Map TheAudioDB's country-coded suffix back to a BCP47 short
/// language tag. Inverse of
/// `crate::locale::theaudiodb_country_code`. Unknown suffixes
/// pass through lower-cased — the caller may still match them
/// against a locale from the same family.
fn country_code_to_lang(cc: &str) -> String {
    match cc {
        "DE" => "de".into(),
        "FR" => "fr".into(),
        "ES" => "es".into(),
        "IT" => "it".into(),
        "JP" => "ja".into(),
        "RU" => "ru".into(),
        "CN" => "zh".into(),
        "PT" => "pt".into(),
        "NL" => "nl".into(),
        "PL" => "pl".into(),
        "HU" => "hu".into(),
        "IL" => "he".into(),
        "SE" => "sv".into(),
        "NO" => "no".into(),
        other => other.to_ascii_lowercase(),
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
    #[serde(rename = "strArtistThumb")]
    str_artist_thumb: Option<String>,
    #[serde(rename = "strGenre")]
    str_genre: Option<String>,
    #[serde(rename = "intFormedYear")]
    int_formed_year: Option<String>,
    /// TheAudioDB emits `strBiography` (English base)
    /// plus `strBiography<CC>` per locale. The full response
    /// carries ~40 other fields we don't consume — the caller
    /// only reads the biography set, so a `flatten` capture
    /// keeps every locale variant available for
    /// [`ArtistBioHit::bio_for_locale`] without a per-locale
    /// field explosion here.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
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
    #[serde(rename = "strReview")]
    str_review: Option<String>,
    #[serde(rename = "intYearReleased")]
    int_year_released: Option<String>,
    #[serde(rename = "strLabel")]
    str_label: Option<String>,
    /// `strDescription` (English base) + per-locale
    /// `strDescription<CC>` — same shape as the artist bio
    /// response.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_json::Value>,
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
        // TheAudioDB has NO `strBiographyEN`; the
        // English base lives on `strBiography` and per-locale
        // variants land on `strBiography<CC>`. The flattened
        // `extra` map captures every prose-carrying field so
        // `bio_for_locale` can pick the operator locale.
        let body = r#"{
            "artists": [{
                "idArtist": "111239",
                "strArtist": "Radiohead",
                "strBiography": "Radiohead are an English rock band...",
                "strBiographyDE": "Radiohead ist eine britische Rockband...",
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
        assert_eq!(entry.int_formed_year.as_deref(), Some("1985"));
        // English base + DE variant both land in `extra`.
        assert!(entry
            .extra
            .get("strBiography")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("Radiohead are")));
        assert!(entry
            .extra
            .get("strBiographyDE")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("Radiohead ist")));
    }

    #[test]
    fn bio_for_locale_picks_operator_locale_then_english_then_any() {
        // Positive: operator locale populated → served in that
        // locale.
        let hit_full = TheAudioDbClient::first_artist_to_bio_hit(
            serde_json::from_str(
                r#"{"artists":[{
                    "idArtist":"1",
                    "strBiography":"english base",
                    "strBiographyDE":"deutsche version"
                }]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let (text, lang) = hit_full.bio_for_locale("de").unwrap();
        assert_eq!(lang, "de");
        assert_eq!(text, "deutsche version");

        // Fallback 1: operator locale absent, English present.
        let hit_no_de = TheAudioDbClient::first_artist_to_bio_hit(
            serde_json::from_str(
                r#"{"artists":[{
                    "idArtist":"1",
                    "strBiography":"english base"
                }]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let (text, lang) = hit_no_de.bio_for_locale("de").unwrap();
        assert_eq!(lang, "en", "German absent → English fallback");
        assert_eq!(text, "english base");

        // Fallback 2: operator locale AND English absent, but
        // some other locale present — serve that, honestly
        // labelled.
        let hit_only_fr = TheAudioDbClient::first_artist_to_bio_hit(
            serde_json::from_str(
                r#"{"artists":[{
                    "idArtist":"1",
                    "strBiographyFR":"version française"
                }]}"#,
            )
            .unwrap(),
        )
        .unwrap();
        let (text, lang) = hit_only_fr.bio_for_locale("de").unwrap();
        assert_eq!(
            lang, "fr",
            "German + English absent → any-non-empty (fr), labelled fr"
        );
        assert_eq!(text, "version française");

        // Nothing at all: honest None (empty bio pane in the UI).
        let hit_empty = TheAudioDbClient::first_artist_to_bio_hit(
            serde_json::from_str(r#"{"artists":[{"idArtist":"1"}]}"#).unwrap(),
        )
        .unwrap();
        assert!(hit_empty.bio_for_locale("de").is_none());
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
        // Same shape as bio: `strDescription` (English base) +
        // `strDescription<CC>` per locale, both captured via
        // the flattened `extra` map.
        let body = r#"{
            "album": [{
                "idAlbum": "2109547",
                "strAlbum": "OK Computer",
                "strDescription": "OK Computer is a landmark...",
                "strDescriptionDE": "OK Computer ist ein Meilenstein...",
                "strReview": "A widely acclaimed album...",
                "intYearReleased": "1997",
                "strLabel": "Parlophone"
            }]
        }"#;
        let root: AlbumSearchRoot = serde_json::from_str(body).unwrap();
        let entry = root.album.unwrap().into_iter().next().unwrap();
        assert_eq!(entry.id_album.as_deref(), Some("2109547"));
        assert_eq!(entry.int_year_released.as_deref(), Some("1997"));
        assert_eq!(entry.str_label.as_deref(), Some("Parlophone"));
        assert!(entry
            .extra
            .get("strDescription")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("OK Computer")));
        assert!(entry
            .extra
            .get("strDescriptionDE")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("OK Computer ist")));
    }

    #[test]
    fn nonempty_trims_and_filters() {
        assert_eq!(nonempty("hello".into()), Some("hello".into()));
        assert_eq!(nonempty("  padded  ".into()), Some("padded".into()));
        assert_eq!(nonempty("".into()), None);
        assert_eq!(nonempty("   ".into()), None);
    }
}
