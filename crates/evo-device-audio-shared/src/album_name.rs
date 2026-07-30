// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Album identity: canonical display + grouping key for
//! browse-by-album.
//!
//! MPD's raw `album` tag carries three failure classes that
//! break browse-by-album on ordinary libraries the same way
//! Roon, Plex, Jellyfin, Picard, and beets model around:
//!
//! - **Multi-disc.** Two tags (`Album (Disc 1)`, `Album (Disc
//!   2)`) surface as two tiles for a single release. Enterprise
//!   music software folds them into one and preserves disc-
//!   then-track order on drill.
//! - **Decoration.** `(Deluxe Edition)`, `(Remastered
//!   Version)`, `[Explicit]`, `(Bonus Track Version)` clutter
//!   the display; the tile should show the plain title.
//! - **Degenerate tag.** The ALBUM tag holds the artist credit,
//!   not the album title. The real title exists only in the
//!   folder basename. Standard Plex / Jellyfin fallback: use
//!   the folder.
//!
//! [`album_identity`] resolves the display title and the
//! grouping key from raw MPD tag data plus the file's parent
//! folder basename. Applied once at the browse-by-album call
//! site; downstream consumers (cover-url synthesis, drill by
//! album) key off the same helper.

use unicode_normalization::UnicodeNormalization;

/// Canonical album identity emitted at the browse-by-album
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumIdentity {
    /// Display title for the tile. Multi-disc suffix and
    /// edition decoration removed. NFC-normalised so composed
    /// and decomposed inputs render identically.
    pub display: String,
    /// Grouping discriminator. Two raw tag forms with the same
    /// grouping key belong to the same tile. Case- and
    /// diacritic-insensitive; multi-disc variants fold; edition
    /// qualifiers stay part of the key so a deluxe / standard
    /// release with genuinely different track counts stay
    /// separate tiles.
    pub grouping_key: String,
}

/// Resolve an album's canonical identity from its raw MPD
/// `album` tag, the raw artist credit (used to detect a
/// mistagged album), and the file's parent folder basename.
///
/// Rules, applied in order:
///
/// 1. Degenerate-tag fallback (Plex/Jellyfin folder model):
///    when `raw_album` is empty OR every token in it appears
///    in `raw_artist_credit` (the strong mistag signal — the
///    album tag repeats the artist names), use `parent_folder`
///    as the effective album name.
/// 2. Trailing disc/medium suffix removed (`(Disc 1)`,
///    `(CD 2 of 3)`, `[Vol 3]`, `(Part 1)`). Applied to BOTH
///    display and grouping key so multi-disc folds.
/// 3. Trailing edition/qualifier suffix removed from the
///    DISPLAY (`(Deluxe Edition)`, `[Explicit]`,
///    `(Remastered)`, `(NNth Anniversary Edition)`, …). Kept
///    on the grouping key so `Album (Deluxe Edition)` and
///    `Album` stay separate tiles when the release genuinely
///    differs.
/// 4. Display is NFC-normalised.
///
/// If every step produces an empty string, returns the trimmed
/// raw album (or the trimmed folder) so the tile always has a
/// label.
pub fn album_identity(
    raw_album: &str,
    raw_artist_credit: &str,
    parent_folder: &str,
) -> AlbumIdentity {
    let effective_raw = if is_degenerate_album_tag(raw_album, raw_artist_credit)
    {
        parent_folder.trim()
    } else {
        raw_album.trim()
    };

    let base_without_disc = strip_trailing_disc_suffix(effective_raw);
    let display_clean = strip_trailing_edition_qualifier(base_without_disc);
    let display_source = if display_clean.trim().is_empty() {
        effective_raw
    } else {
        display_clean
    };
    let display: String = display_source.nfc().collect();

    let grouping_key = fold_alnum(base_without_disc);

    AlbumIdentity {
        display,
        grouping_key,
    }
}

/// True when the raw `album` tag is missing or holds the
/// artist credit rather than the album title — the browse
/// facet caller should fetch a parent folder basename before
/// calling [`album_identity`], otherwise the tile falls back
/// to the raw (dirty) album tag.
///
/// Called at the browse-by-album call site so an MPD
/// `find album X` roundtrip only fires for the degenerate
/// rows.
pub fn album_needs_folder_fallback(
    raw_album: &str,
    raw_artist_credit: &str,
) -> bool {
    is_degenerate_album_tag(raw_album, raw_artist_credit)
}

/// True when the raw `album` tag is missing or holds the
/// artist credit rather than the album title.
///
/// - Empty album is trivially degenerate.
/// - Non-empty is degenerate when every token in the album
///   tag appears as a token in the artist credit — the
///   strong mistag signal enterprise libraries use to trigger
///   a folder fallback.
///
/// Comparison is on fold-normalised alphanumeric tokens so
/// diacritic form and delimiter choice do not matter.
fn is_degenerate_album_tag(album: &str, artist_credit: &str) -> bool {
    let album_trim = album.trim();
    if album_trim.is_empty() {
        return true;
    }
    let credit_trim = artist_credit.trim();
    if credit_trim.is_empty() {
        return false;
    }
    let album_tokens = token_bag(album_trim);
    if album_tokens.is_empty() {
        return true;
    }
    let credit_tokens = token_bag(credit_trim);
    if credit_tokens.is_empty() {
        return false;
    }
    album_tokens.iter().all(|t| credit_tokens.contains(t))
}

fn token_bag(s: &str) -> std::collections::BTreeSet<String> {
    fold_alnum(s)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Strip a trailing disc/medium suffix in any of the three
/// forms real-world tag data uses:
///
/// - **Parenthesised** — `Album (Disc 1)`, `Album [CD 2]`,
///   `Album (Disc 1 of 3)`, `Album (Vol 1)`, `Album (Part 1)`.
/// - **Dash-separated** — `Album - Disc 1`, `Album - CD 1`,
///   `Album - Vol 1`, `Album — CD 2` (em-dash).
/// - **Glued** — `Album vol2`, `Album disc1`, `Album cd1` —
///   the label + number is a single whitespace-delimited
///   token.
///
/// All matched forms strip to `Album`, unifying multi-disc
/// variants into one tile. Non-matching parentheticals
/// (`(Live at Wembley)`, `(Deluxe Edition)`) are left for the
/// edition-qualifier stripper.
fn strip_trailing_disc_suffix(s: &str) -> &str {
    let trimmed = s.trim_end();
    if trimmed.len() < 3 {
        return trimmed;
    }
    if let Some(stripped) = strip_paren_disc_suffix(trimmed) {
        return stripped;
    }
    if let Some(stripped) = strip_dash_disc_suffix(trimmed) {
        return stripped;
    }
    if let Some(stripped) = strip_glued_disc_suffix(trimmed) {
        return stripped;
    }
    trimmed
}

fn strip_paren_disc_suffix(trimmed: &str) -> Option<&str> {
    let bytes = trimmed.as_bytes();
    let close = *bytes.last()?;
    let open = match close {
        b')' => b'(',
        b']' => b'[',
        _ => return None,
    };
    let open_idx = bytes.iter().rposition(|&b| b == open)?;
    let inner = trimmed[open_idx + 1..trimmed.len() - 1].trim();
    if parse_disc_group(inner).is_some() {
        Some(trimmed[..open_idx].trim_end())
    } else {
        None
    }
}

fn strip_dash_disc_suffix(trimmed: &str) -> Option<&str> {
    // Find the last dash separator surrounded by spaces:
    // ` - `, ` – ` (en-dash U+2013), ` — ` (em-dash U+2014).
    // Whitespace-flanked because a hyphen inside a title
    // (`Wham! - Last Christmas`) is not a disc marker.
    for delim in [" - ", " \u{2013} ", " \u{2014} "] {
        if let Some(idx) = trimmed.rfind(delim) {
            let tail = trimmed[idx + delim.len()..].trim();
            if parse_disc_group(tail).is_some() {
                return Some(trimmed[..idx].trim_end());
            }
        }
    }
    None
}

fn strip_glued_disc_suffix(trimmed: &str) -> Option<&str> {
    // Last whitespace-delimited token; if it parses as
    // `<label><digits>` (`vol2`, `disc1`, `cd3`), strip it
    // and the whitespace before it. Guards against tokens
    // that happen to end in digits (`Album 2` — album track
    // number two) by requiring the alpha prefix be a known
    // disc-label vocabulary.
    let last_space = trimmed.rfind(char::is_whitespace)?;
    let tail = &trimmed[last_space + 1..];
    if tail.is_empty() {
        return None;
    }
    let lower = tail.to_ascii_lowercase();
    let digit_start = lower.find(|c: char| c.is_ascii_digit())?;
    if digit_start == 0 {
        return None;
    }
    let label = &lower[..digit_start];
    let digits = &lower[digit_start..];
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match label {
        "disc" | "disk" | "cd" | "vol" | "volume" | "part" => {
            Some(trimmed[..last_space].trim_end())
        }
        _ => None,
    }
}

fn parse_disc_group(inner: &str) -> Option<u32> {
    let lower: String = inner.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let (label_raw, rest) = words.split_first()?;
    let label = label_raw.trim_end_matches('.');
    match label {
        "disc" | "disk" | "cd" | "vol" | "volume" | "part" => {}
        _ => return None,
    }
    let n = rest.first()?.parse::<u32>().ok()?;
    match rest.len() {
        1 => Some(n),
        3 if rest[1] == "of" && rest[2].parse::<u32>().is_ok() => Some(n),
        _ => None,
    }
}

/// Strip a trailing edition/qualifier parenthetical from the
/// display title. Fires only for a known qualifier vocabulary;
/// anything else stays — a group like `(Live at Wembley)`
/// remains part of the title.
///
/// Repeats so stacked decorations (`(Deluxe Edition) (Deluxe)`)
/// collapse in one pass.
fn strip_trailing_edition_qualifier(s: &str) -> &str {
    let mut out = s.trim_end();
    loop {
        let Some(stripped) = strip_one_trailing_edition(out) else {
            return out;
        };
        out = stripped;
    }
}

fn strip_one_trailing_edition(s: &str) -> Option<&str> {
    let trimmed = s.trim_end();
    if trimmed.len() < 3 {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let close = bytes[bytes.len() - 1];
    let open = match close {
        b')' => b'(',
        b']' => b'[',
        _ => return None,
    };
    let open_idx = bytes.iter().rposition(|&b| b == open)?;
    let inner = trimmed[open_idx + 1..trimmed.len() - 1].trim();
    if is_edition_qualifier(inner) {
        Some(trimmed[..open_idx].trim_end())
    } else {
        None
    }
}

fn is_edition_qualifier(inner: &str) -> bool {
    let lc = inner.to_ascii_lowercase();
    let lc = lc.trim();
    matches!(
        lc,
        "deluxe"
            | "deluxe edition"
            | "deluxe version"
            | "expanded edition"
            | "expanded version"
            | "extended edition"
            | "extended version"
            | "explicit"
            | "clean"
            | "clean version"
            | "remastered"
            | "remastered version"
            | "remaster"
            | "anniversary edition"
            | "special edition"
            | "collector's edition"
            | "collectors edition"
            | "bonus track version"
            | "bonus track edition"
            | "bonus tracks"
            | "radio edit"
            | "single edit"
            | "explicit version"
    ) || is_anniversary_form(lc)
}

fn is_anniversary_form(lc: &str) -> bool {
    let head = if let Some(h) = lc.strip_suffix(" anniversary edition") {
        h
    } else if let Some(h) = lc.strip_suffix(" anniversary") {
        h
    } else {
        return false;
    };
    if head.is_empty() {
        return false;
    }
    head.chars().next().is_some_and(|c| c.is_ascii_digit())
        && head
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_alphabetic())
}

fn fold_alnum(s: &str) -> String {
    let mut folded = String::with_capacity(s.len());
    let mut last_was_space = true;
    for ch in s.nfd() {
        if is_combining_mark(ch) {
            continue;
        }
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                folded.push(lc);
                last_was_space = false;
            }
        } else if !last_was_space {
            folded.push(' ');
            last_was_space = true;
        }
    }
    if folded.ends_with(' ') {
        folded.pop();
    }
    folded
}

fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32, 0x0300..=0x036F)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------
    // Multi-disc merge (Elton "Goodbye Yellow Brick Road" / "The
    // Ultimate Collection" — both tagged (Disc 1)/(Disc 2))
    // ------------------------------------------------------------

    #[test]
    fn multi_disc_variants_share_grouping_key() {
        let a = album_identity(
            "Goodbye Yellow Brick Road (Disc 1)",
            "Elton John",
            "Elton John - Goodbye Yellow Brick Road",
        );
        let b = album_identity(
            "Goodbye Yellow Brick Road (Disc 2)",
            "Elton John",
            "Elton John - Goodbye Yellow Brick Road",
        );
        assert_eq!(a.grouping_key, b.grouping_key);
        assert_eq!(a.display, "Goodbye Yellow Brick Road");
        assert_eq!(b.display, "Goodbye Yellow Brick Road");
    }

    #[test]
    fn multi_disc_variants_display_without_disc_suffix() {
        for raw in [
            "Album X (Disc 1)",
            "Album X (Disk 2)",
            "Album X [CD 3]",
            "Album X (CD 1 of 2)",
            "Album X (Vol 1)",
            "Album X (Volume 2)",
            "Album X (Part 1)",
            "Album X (Disc. 1)",
        ] {
            let id = album_identity(raw, "Artist", "folder");
            assert_eq!(id.display, "Album X", "raw {raw:?}");
        }
    }

    #[test]
    fn non_disc_parenthetical_kept_intact_on_display() {
        // "Live at Wembley" is not a disc marker and not a
        // known edition qualifier; the tile keeps it.
        let id = album_identity(
            "Bad (Live at Wembley)",
            "Michael Jackson",
            "Michael Jackson - Bad (Live at Wembley)",
        );
        assert_eq!(id.display, "Bad (Live at Wembley)");
    }

    #[test]
    fn dash_form_disc_recognised() {
        for raw in [
            "Les Concerts En Chine - CD 1",
            "Les Concerts En Chine - Disc 2",
            "Les Concerts En Chine - Vol 1",
            "Les Concerts En Chine \u{2013} CD 2",
            "Les Concerts En Chine \u{2014} CD 2",
        ] {
            let id = album_identity(raw, "Jean Michel Jarre", "folder");
            assert_eq!(id.display, "Les Concerts En Chine", "raw {raw:?}");
        }
    }

    #[test]
    fn glued_form_disc_recognised() {
        for raw in [
            "Les Concerts en Chine vol2",
            "Les Concerts en Chine disc1",
            "Les Concerts en Chine cd3",
        ] {
            let id = album_identity(raw, "Jean Michel Jarre", "folder");
            assert_eq!(id.display, "Les Concerts en Chine", "raw {raw:?}");
        }
    }

    #[test]
    fn album_ending_in_digits_not_treated_as_disc() {
        // "Album 2" — the trailing token is just digits, no
        // alpha label. Must NOT strip.
        let id = album_identity("Grease 2", "Various", "folder");
        assert_eq!(id.display, "Grease 2");
    }

    #[test]
    fn hyphen_inside_title_not_treated_as_disc_delimiter() {
        // `Wham! - Last Christmas` has ` - ` but the tail is
        // not a disc marker. Must NOT strip.
        let id = album_identity("Wham! - Last Christmas", "Wham!", "folder");
        assert_eq!(id.display, "Wham! - Last Christmas");
    }

    #[test]
    fn dash_form_variants_share_grouping_key() {
        let a = album_identity(
            "Les Concerts En Chine - CD 1",
            "Jean Michel Jarre",
            "folder",
        );
        let b = album_identity(
            "Les Concerts en Chine vol2",
            "Jean Michel Jarre",
            "folder",
        );
        assert_eq!(a.grouping_key, b.grouping_key);
    }

    #[test]
    fn disc_of_m_form_recognised() {
        let id = album_identity(
            "Ultimate Collection (Disc 1 of 2)",
            "Elton John",
            "folder",
        );
        assert_eq!(id.display, "Ultimate Collection");
    }

    // ------------------------------------------------------------
    // Decoration stripping (display-clean; grouping keeps edition
    // so deluxe/standard don't wrongly merge)
    // ------------------------------------------------------------

    #[test]
    fn edition_qualifiers_stripped_from_display() {
        for (raw, want) in [
            ("Unorthodox Jukebox [Explicit]", "Unorthodox Jukebox"),
            ("Blonde on Blonde (Remastered Version)", "Blonde on Blonde"),
            ("Rumours (Deluxe Edition)", "Rumours"),
            ("Thriller (Bonus Track Version)", "Thriller"),
            ("Damaged (25th Anniversary Edition)", "Damaged"),
            ("Damaged (25th Anniversary)", "Damaged"),
            ("Some Album (Radio Edit)", "Some Album"),
            ("Some Album (Extended Edition)", "Some Album"),
            ("Some Album (Special Edition)", "Some Album"),
        ] {
            let id = album_identity(raw, "Some Artist", "folder");
            assert_eq!(id.display, want, "raw {raw:?}");
        }
    }

    #[test]
    fn stacked_edition_qualifiers_collapse() {
        // The observed real-rig case: two stacked qualifiers.
        let id = album_identity(
            "Rumours (Deluxe Edition) (Deluxe)",
            "Fleetwood Mac",
            "folder",
        );
        assert_eq!(id.display, "Rumours");
    }

    #[test]
    fn edition_qualifier_kept_in_grouping_key() {
        // Standard and Deluxe stay separate tiles because
        // enterprise libraries treat them as different
        // releases when the track counts differ.
        let standard = album_identity("Rumours", "Fleetwood Mac", "folder");
        let deluxe = album_identity(
            "Rumours (Deluxe Edition)",
            "Fleetwood Mac",
            "folder",
        );
        assert_ne!(standard.grouping_key, deluxe.grouping_key);
    }

    #[test]
    fn edition_and_disc_stack_display_clean() {
        // Disc suffix stripped for grouping AND display;
        // edition suffix stripped for display only.
        let id = album_identity(
            "Album X (Deluxe Edition) (Disc 1)",
            "Artist",
            "folder",
        );
        assert_eq!(id.display, "Album X");
    }

    // ------------------------------------------------------------
    // Degenerate-tag fallback (Plex / Jellyfin folder model)
    // ------------------------------------------------------------

    #[test]
    fn degenerate_album_falls_back_to_folder() {
        // The ALBUM tag holds the artist credit; folder holds
        // the real album title. Verve/Paco fixture.
        let id = album_identity(
            "Paco De Lucia, Al Di Meola, John McLaughlin",
            "Paco De Lucia, Al Di Meola, John McLaughlin",
            "Friday Night in San Francisco",
        );
        assert_eq!(id.display, "Friday Night in San Francisco");
    }

    #[test]
    fn empty_album_falls_back_to_folder() {
        let id = album_identity("", "Some Artist", "The Real Title");
        assert_eq!(id.display, "The Real Title");
    }

    #[test]
    fn valid_album_ignores_folder_hint() {
        // The ALBUM tag is a real title; folder name should
        // NOT override it.
        let id = album_identity(
            "Kind of Blue",
            "Miles Davis",
            "Miles Davis - Kind of Blue [1959]",
        );
        assert_eq!(id.display, "Kind of Blue");
    }

    #[test]
    fn album_with_extra_tokens_beyond_credit_not_degenerate() {
        // Album name contains the artist name plus additional
        // tokens — not a mistag. Keep the raw album.
        let id = album_identity(
            "The Beatles Anthology",
            "The Beatles",
            "The Beatles - Anthology",
        );
        assert_eq!(id.display, "The Beatles Anthology");
    }

    #[test]
    fn degenerate_across_delimiter_variations() {
        // Different delimiters in album vs credit still match
        // as subset once folded.
        let id = album_identity(
            "Paco De Lucia; Al Di Meola; John McLaughlin",
            "Paco de Lucía, Al Di Meola, John McLaughlin",
            "Friday Night in San Francisco",
        );
        assert_eq!(id.display, "Friday Night in San Francisco");
    }

    // ------------------------------------------------------------
    // Grouping-key invariants
    // ------------------------------------------------------------

    #[test]
    fn grouping_key_case_and_diacritic_insensitive() {
        let a = album_identity("Café Bleu", "Style Council", "folder");
        let b = album_identity("CAFE BLEU", "Style Council", "folder");
        assert_eq!(a.grouping_key, b.grouping_key);
    }

    #[test]
    fn grouping_key_stable_across_disc_variants() {
        let a = album_identity("Album X (Disc 1)", "Artist", "folder");
        let b = album_identity("Album X (Disc 2)", "Artist", "folder");
        let c = album_identity("Album X", "Artist", "folder");
        assert_eq!(a.grouping_key, b.grouping_key);
        assert_eq!(b.grouping_key, c.grouping_key);
    }

    #[test]
    fn empty_everything_returns_empty_identity() {
        let id = album_identity("", "", "");
        assert!(id.display.is_empty());
        assert!(id.grouping_key.is_empty());
    }

    #[test]
    fn display_nfc_normalises_decomposed_input() {
        // "Cafe" + combining acute → NFC "Café".
        let decomposed = "Cafe\u{0301}";
        let composed = "Café";
        let a = album_identity(decomposed, "Artist", "folder");
        let b = album_identity(composed, "Artist", "folder");
        assert_eq!(a.display, b.display);
        assert_eq!(a.display, composed);
    }
}
