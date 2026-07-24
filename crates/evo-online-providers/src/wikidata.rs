// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Wikidata entity client for the audio distribution's keyless
//! online-metadata cascade.
//!
//! Anonymous provider — no account, no key. The keyless-first
//! metadata enrichment cascade uses Wikidata for structured
//! biographical facts: dates of birth / death / formation,
//! country of origin, activity periods, genres, occupations.
//! These fill the ontology facets on the Track Info panels that
//! Wikipedia's plain-text summary does not carry as structured
//! data.
//!
//! ## Endpoint
//!
//! The `Special:EntityData/{Q...}.json` endpoint returns a
//! Wikibase entity object. The client extracts only the small
//! subset of statements the operator UI renders — everything
//! else stays untouched so a Wikidata schema addition never
//! breaks a downstream parser.
//!
//! ## User-Agent policy
//!
//! Same Wikimedia rule as the Wikipedia client: a descriptive
//! UA identifying tool + contact is mandatory. The client
//! refuses to send without one.
//!
//! ## License
//!
//! Wikidata is CC0. No attribution obligation, but the operator
//! UI should still surface `source_name = "Wikidata"` +
//! `source_url = <entity page URL>` alongside the extracted
//! facts so operators can trace where a fact came from.
//!
//! ## Facts extracted
//!
//! The client extracts only what the cascade actually needs:
//!
//! - P569 — date of birth (person)
//! - P570 — date of death (person)
//! - P571 — inception / formation date (group / ensemble)
//! - P576 — dissolution date (group / ensemble)
//! - P19  — place of birth
//! - P27  — country of citizenship
//! - P495 — country of origin (group / ensemble)
//! - P106 — occupation(s)
//! - P136 — genre(s)
//!
//! Additional statements are ignored. A schema addition never
//! breaks the parser; a schema removal surfaces as a missing
//! optional field on the returned struct.

use std::sync::Arc;

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::rate_limit::RateLimiter;

/// Errors from the Wikidata client.
#[derive(Debug, thiserror::Error)]
pub enum WikidataError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Wikidata returned status {status} with body: {body}")]
    Status { status: u16, body: String },
    #[error("Wikidata JSON decode failed: {0}")]
    Decode(String),
    #[error("Wikidata URL missing entity id: {url}")]
    BadUrl { url: String },
}

/// Extracted facts from one Wikidata entity.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WikidataEntityHit {
    /// Wikidata Q-id (e.g. `"Q7346"`).
    pub entity_id: String,
    /// English label for the entity, when present.
    pub label_en: Option<String>,
    /// English short description, when present. Wikidata's
    /// description is typically one line ("English rock band" /
    /// "German composer") — good for a subtitle under the bio.
    pub description_en: Option<String>,
    /// Date of birth (P569) as an ISO date string (`YYYY-MM-DD`).
    /// Nullable — groups and ensembles do not have a P569.
    pub date_of_birth: Option<String>,
    /// Date of death (P570). Person only.
    pub date_of_death: Option<String>,
    /// Inception / formation date (P571). Groups + ensembles.
    pub inception: Option<String>,
    /// Dissolution date (P576). Groups + ensembles.
    pub dissolution: Option<String>,
    /// Place of birth (P19) as the linked Q-id — the caller
    /// can round-trip for a label if it wants the name.
    pub place_of_birth_id: Option<String>,
    /// Country of citizenship (P27) as a Q-id. Person only.
    pub country_of_citizenship_id: Option<String>,
    /// Country of origin (P495) as a Q-id. Groups + ensembles.
    pub country_of_origin_id: Option<String>,
    /// Occupations (P106) as Q-ids. May be many; the caller
    /// picks the ones it wants to surface.
    pub occupation_ids: Vec<String>,
    /// Genres (P136) as Q-ids.
    pub genre_ids: Vec<String>,
    /// English-Wikipedia article title from the entity's
    /// `sitelinks.enwiki.site` block, when present. Cascade
    /// callers use this to fetch actual Wikipedia prose via
    /// `WikipediaClient::get_summary_en` and attribute the
    /// result to Wikipedia (CC BY-SA) rather than falling back
    /// to Wikidata's one-line description as bio content.
    pub enwiki_title: Option<String>,
    /// Canonical Wikidata entity page URL (attribution).
    pub entity_url: String,
}

/// Wikidata entity client. Anonymous, rate-limited via the
/// shared [`RateLimiter`].
#[derive(Clone)]
pub struct WikidataClient {
    http: Client,
    rate: Arc<RateLimiter>,
    user_agent: String,
}

impl WikidataClient {
    /// Construct a client. Same UA rules as the Wikipedia client.
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

    /// Fetch an entity by Q-id (e.g. `"Q7346"`).
    pub async fn get_entity(
        &self,
        entity_id: &str,
    ) -> Result<Option<WikidataEntityHit>, WikidataError> {
        self.rate.acquire().await;
        let url = format!(
            "https://www.wikidata.org/wiki/Special:EntityData/{entity_id}.json"
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
            return Err(WikidataError::Status {
                status: status.as_u16(),
                body,
            });
        }
        let body: EntityResponse = resp
            .json()
            .await
            .map_err(|e| WikidataError::Decode(e.to_string()))?;
        let Some(entity) = body.entities.get(entity_id) else {
            return Ok(None);
        };
        Ok(Some(entity_hit_from_response(entity_id, entity)))
    }

    /// Fetch an entity from a Wikidata URL. Convenience wrapper
    /// for cascades that receive a MusicBrainz `wikidata` URL-rel
    /// (`https://www.wikidata.org/wiki/Q7346` or
    /// `https://www.wikidata.org/entity/Q7346`).
    pub async fn get_entity_from_url(
        &self,
        url: &str,
    ) -> Result<Option<WikidataEntityHit>, WikidataError> {
        let Some(entity_id) = parse_wikidata_url(url) else {
            return Err(WikidataError::BadUrl {
                url: url.to_string(),
            });
        };
        self.get_entity(&entity_id).await
    }
}

/// Extract the Q-id from a Wikidata URL. Handles
/// `https://www.wikidata.org/wiki/Q<n>` and
/// `https://www.wikidata.org/entity/Q<n>`.
pub fn parse_wikidata_url(url: &str) -> Option<String> {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let (host, path) = after_scheme.split_once('/')?;
    if host != "www.wikidata.org" && host != "wikidata.org" {
        return None;
    }
    let rest = path
        .strip_prefix("wiki/")
        .or_else(|| path.strip_prefix("entity/"))?;
    let id = rest.split_once('#').map(|(t, _)| t).unwrap_or(rest);
    let id = id.split_once('?').map(|(t, _)| t).unwrap_or(id);
    if id.starts_with('Q') && id[1..].chars().all(|c| c.is_ascii_digit()) {
        Some(id.to_string())
    } else {
        None
    }
}

#[derive(Debug, Deserialize)]
struct EntityResponse {
    #[serde(default)]
    entities: std::collections::HashMap<String, EntityBody>,
}

#[derive(Debug, Deserialize)]
struct EntityBody {
    #[serde(default)]
    labels: std::collections::HashMap<String, LangValue>,
    #[serde(default)]
    descriptions: std::collections::HashMap<String, LangValue>,
    #[serde(default)]
    claims: std::collections::HashMap<String, Vec<Statement>>,
    #[serde(default)]
    sitelinks: std::collections::HashMap<String, SitelinkEntry>,
}

/// One sitelinks entry — the Wikidata API returns
/// `{ site: "enwiki", title: "…" , badges: […], url?: "…" }`
/// keyed by the site id. The cascade only consumes
/// `enwiki` currently; other language editions land in the
/// same shape when the callers need them.
#[derive(Debug, Deserialize)]
struct SitelinkEntry {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LangValue {
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Statement {
    #[serde(default)]
    mainsnak: Option<MainSnak>,
}

#[derive(Debug, Deserialize)]
struct MainSnak {
    #[serde(default)]
    datavalue: Option<DataValue>,
}

#[derive(Debug, Deserialize)]
struct DataValue {
    #[serde(rename = "type", default)]
    value_type: Option<String>,
    #[serde(default)]
    value: Value,
}

fn entity_hit_from_response(
    entity_id: &str,
    body: &EntityBody,
) -> WikidataEntityHit {
    let label_en = body.labels.get("en").and_then(|l| l.value.clone());
    let description_en =
        body.descriptions.get("en").and_then(|d| d.value.clone());
    let date_of_birth = first_time_value(&body.claims, "P569");
    let date_of_death = first_time_value(&body.claims, "P570");
    let inception = first_time_value(&body.claims, "P571");
    let dissolution = first_time_value(&body.claims, "P576");
    let place_of_birth_id = first_entity_id(&body.claims, "P19");
    let country_of_citizenship_id = first_entity_id(&body.claims, "P27");
    let country_of_origin_id = first_entity_id(&body.claims, "P495");
    let occupation_ids = all_entity_ids(&body.claims, "P106");
    let genre_ids = all_entity_ids(&body.claims, "P136");
    let enwiki_title = body
        .sitelinks
        .get("enwiki")
        .and_then(|entry| entry.title.clone())
        .filter(|s| !s.trim().is_empty());
    WikidataEntityHit {
        entity_id: entity_id.to_string(),
        label_en,
        description_en,
        date_of_birth,
        date_of_death,
        inception,
        dissolution,
        place_of_birth_id,
        country_of_citizenship_id,
        country_of_origin_id,
        occupation_ids,
        genre_ids,
        enwiki_title,
        entity_url: format!("https://www.wikidata.org/wiki/{entity_id}"),
    }
}

fn first_time_value(
    claims: &std::collections::HashMap<String, Vec<Statement>>,
    property: &str,
) -> Option<String> {
    claims
        .get(property)?
        .iter()
        .find_map(|s| s.mainsnak.as_ref())
        .and_then(|snak| snak.datavalue.as_ref())
        .and_then(|dv| {
            if dv.value_type.as_deref() == Some("time") {
                dv.value.get("time").and_then(Value::as_str).map(|s| {
                    // Wikidata time values look like
                    // "+YYYY-MM-DDT00:00:00Z". Strip the sign and
                    // the time-of-day suffix to yield a bare
                    // "YYYY-MM-DD".
                    let trimmed = s.trim_start_matches('+');
                    trimmed
                        .split_once('T')
                        .map(|(d, _)| d)
                        .unwrap_or(trimmed)
                        .to_string()
                })
            } else {
                None
            }
        })
}

fn first_entity_id(
    claims: &std::collections::HashMap<String, Vec<Statement>>,
    property: &str,
) -> Option<String> {
    claims
        .get(property)?
        .iter()
        .find_map(|s| s.mainsnak.as_ref())
        .and_then(|snak| snak.datavalue.as_ref())
        .and_then(|dv| {
            if dv.value_type.as_deref() == Some("wikibase-entityid") {
                dv.value.get("id").and_then(Value::as_str).map(String::from)
            } else {
                None
            }
        })
}

fn all_entity_ids(
    claims: &std::collections::HashMap<String, Vec<Statement>>,
    property: &str,
) -> Vec<String> {
    match claims.get(property) {
        Some(entries) => entries
            .iter()
            .filter_map(|s| {
                s.mainsnak
                    .as_ref()?
                    .datavalue
                    .as_ref()?
                    .value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wikidata_wiki_url() {
        assert_eq!(
            parse_wikidata_url("https://www.wikidata.org/wiki/Q7346"),
            Some("Q7346".to_string())
        );
    }

    #[test]
    fn parses_wikidata_entity_url() {
        assert_eq!(
            parse_wikidata_url("https://www.wikidata.org/entity/Q7346"),
            Some("Q7346".to_string())
        );
    }

    #[test]
    fn strips_fragment_and_query() {
        assert_eq!(
            parse_wikidata_url(
                "https://www.wikidata.org/wiki/Q7346#Statements"
            ),
            Some("Q7346".to_string())
        );
    }

    #[test]
    fn rejects_non_wikidata_urls() {
        assert!(parse_wikidata_url("https://example.com/wiki/Q7346").is_none());
        assert!(parse_wikidata_url("https://www.wikidata.org/wiki/NotAQid")
            .is_none());
        assert!(parse_wikidata_url("not a url").is_none());
    }
}
