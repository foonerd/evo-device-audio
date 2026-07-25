// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Operator locale normalisation for the enrichment cascade's
//! locale-aware content selection (metadata language follows
//! the operator UI locale).
//!
//! Every enrichment verb accepts an optional BCP47 short tag
//! (`"en"`, `"de"`, `"fr"`, `"de-DE"`, ...). Callers reach for
//! [`normalise`] to fold the operator input into the two-letter
//! primary tag the providers actually key off; when absent or
//! unrecognisable the fallback is `"en"` — the second tier in
//! the fallback chain (operator → English → any non-empty).
//!
//! [`theaudiodb_country_code`] maps the primary tag to the
//! country-coded suffix TheAudioDB uses on its `strBiography` /
//! `strDescription` fields (the API is inconsistent on this: some
//! codes match the language, others match the country most
//! associated with it — the map here is derived from the live
//! response shape, not guessed).

/// Fold an operator-supplied locale hint into the two-letter
/// BCP47 primary tag providers key off. Case-insensitive;
/// trims and strips the region subtag (`de-DE` → `"de"`,
/// `en_US` → `"en"`). Returns `"en"` for `None`, empty input,
/// or an unrecognisable tag — the second tier of the fallback
/// chain.
pub(crate) fn normalise(input: Option<&str>) -> String {
    let raw = match input {
        Some(s) => s.trim(),
        None => return "en".into(),
    };
    if raw.is_empty() {
        return "en".into();
    }
    // Split on `-` or `_` and keep the primary tag only.
    let primary = raw
        .split(['-', '_'])
        .next()
        .unwrap_or(raw)
        .to_ascii_lowercase();
    // BCP47 primary tags are 2-3 lowercase letters. Reject
    // anything else and fall back to English.
    if primary.len() >= 2
        && primary.len() <= 3
        && primary.chars().all(|c| c.is_ascii_alphabetic())
    {
        primary
    } else {
        "en".into()
    }
}

/// Map a normalised BCP47 primary tag to TheAudioDB's field
/// suffix (`strBiography<CC>` / `strDescription<CC>`). The API
/// uses country codes for most non-English variants — the map
/// below is verified against live responses (Sarah McLachlan,
/// Rolling Stones). Returns `None` for `"en"` (the caller uses
/// the base `strBiography` / `strDescription` field) and for
/// tags TheAudioDB does not carry (the caller falls back to
/// English). Currently the mapping happens inside the
/// TheAudioDB client's `collect_locale_map` helper (which
/// walks the flattened response), so this direction is kept
/// available for callers that want to project into the API's
/// naming convention directly.
#[allow(dead_code)]
pub(crate) fn theaudiodb_country_code(primary: &str) -> Option<&'static str> {
    match primary {
        "en" => None, // Base fields hold English.
        "de" => Some("DE"),
        "fr" => Some("FR"),
        "es" => Some("ES"),
        "it" => Some("IT"),
        "ja" => Some("JP"),
        "ru" => Some("RU"),
        "zh" => Some("CN"),
        "pt" => Some("PT"),
        "nl" => Some("NL"),
        "pl" => Some("PL"),
        "hu" => Some("HU"),
        "he" => Some("IL"),
        "sv" => Some("SE"),
        "no" => Some("NO"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_defaults_to_english_when_missing_or_empty() {
        assert_eq!(normalise(None), "en");
        assert_eq!(normalise(Some("")), "en");
        assert_eq!(normalise(Some("   ")), "en");
    }

    #[test]
    fn normalise_strips_region_and_lowercases() {
        assert_eq!(normalise(Some("de-DE")), "de");
        assert_eq!(normalise(Some("en_US")), "en");
        assert_eq!(normalise(Some("FR-fr")), "fr");
    }

    #[test]
    fn normalise_falls_back_on_unrecognisable() {
        assert_eq!(normalise(Some("!!!")), "en");
        assert_eq!(normalise(Some("12")), "en");
        assert_eq!(normalise(Some("englishpls")), "en");
    }

    #[test]
    fn theaudiodb_country_code_maps_known_locales() {
        assert_eq!(theaudiodb_country_code("de"), Some("DE"));
        assert_eq!(theaudiodb_country_code("ja"), Some("JP"));
        assert_eq!(theaudiodb_country_code("zh"), Some("CN"));
        assert_eq!(theaudiodb_country_code("sv"), Some("SE"));
        assert_eq!(theaudiodb_country_code("he"), Some("IL"));
    }

    #[test]
    fn theaudiodb_country_code_returns_none_for_english() {
        // English lives on the base strBiography field, not a
        // suffixed variant — the caller drops straight to the
        // English base rather than requesting strBiographyEN.
        assert_eq!(theaudiodb_country_code("en"), None);
    }

    #[test]
    fn theaudiodb_country_code_returns_none_for_unmapped() {
        assert_eq!(theaudiodb_country_code("eo"), None);
        assert_eq!(theaudiodb_country_code("cy"), None);
    }
}
