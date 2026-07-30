// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Album-title cleaning primitives.
//!
//! Pure string helpers used by [`folder_album`] when it derives
//! an album's display title from either its raw MPD `album` tag
//! (majority-vote across tracks) or its parent folder basename
//! (fallback when the tag is absent or degenerate).
//!
//! Album IDENTITY (grouping + cover key) is folder-anchored —
//! see [`folder_album`]. This module carries only the
//! decoration-stripping vocabulary shared between tag-derived
//! titles and folder-derived titles:
//!
//! - **Multi-disc suffix strip.** `Album (Disc 1)` /
//!   `Album [CD 2]` / `Album - CD 1` / `Album vol2` / `Album
//!   (Disc 1 of 3)` all reduce to `Album`. Applied to both tag
//!   titles (so multi-disc raw tags render as one) and folder
//!   basenames (so a `Disc 1` sibling folder collapses to its
//!   parent-canonical form).
//! - **Edition-qualifier strip.** `(Deluxe Edition)`,
//!   `(Remastered Version)`, `[Explicit]`, `(NNth Anniversary
//!   Edition)`, `(Bonus Track Version)`, `(Radio Edit)`, and
//!   the stacked forms (`(Deluxe Edition) (Deluxe)`) come off
//!   the DISPLAY. Prior art: Roon / Plex / Jellyfin / Picard
//!   / beets.
//! - **Degenerate-tag detection.** `is_degenerate_album_tag`
//!   answers "does this album tag look like the artist credit
//!   or a label rather than a real title?" — the signal that
//!   folder-anchored resolution should prefer the folder
//!   basename over the tag.
//!
//! [`folder_album`]: crate::folder_album

use unicode_normalization::UnicodeNormalization;

/// Clean an album title for display: strip trailing multi-disc
/// suffix AND trailing edition qualifier, then NFC-normalise.
///
/// Idempotent: `clean_album_title("Album (Deluxe Edition) (Disc
/// 1)")` → `"Album"`. Returns the trimmed original when both
/// strippers produce empty (so the tile never renders blank).
pub fn clean_album_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_disc = strip_trailing_disc_suffix(trimmed);
    let without_edition = strip_trailing_edition_qualifier(without_disc);
    let source = if without_edition.trim().is_empty() {
        trimmed
    } else {
        without_edition
    };
    source.nfc().collect()
}

/// True when the raw `album` tag looks like the artist credit
/// (Verve-style mistag: every token appears in `artist_credit`)
/// or is empty. Signal to prefer the folder basename over the
/// tag.
pub fn is_degenerate_album_tag(album: &str, artist_credit: &str) -> bool {
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
/// variants into one canonical form. Non-matching
/// parentheticals (`(Live at Wembley)`, `(Deluxe Edition)`)
/// are left for the edition-qualifier stripper.
pub fn strip_trailing_disc_suffix(s: &str) -> &str {
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

/// True when `s` is a bare disc marker (`Disc 1`, `CD 2`,
/// `Volume 3`, `Part 1`), case-insensitive. Used by the
/// folder-rollup rule: a folder whose ENTIRE basename is a
/// disc marker is a subfolder of the real album folder — the
/// grandparent is the canonical album folder.
pub fn is_disc_only_basename(s: &str) -> bool {
    parse_disc_group(s.trim()).is_some()
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
/// remains part of the title. Repeats so stacked decorations
/// (`(Deluxe Edition) (Deluxe)`) collapse in one pass.
pub fn strip_trailing_edition_qualifier(s: &str) -> &str {
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
    // Multi-disc suffix strip — three input shapes fold to one
    // canonical form.
    // ------------------------------------------------------------

    #[test]
    fn strip_disc_paren_forms() {
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
            assert_eq!(
                strip_trailing_disc_suffix(raw),
                "Album X",
                "raw {raw:?}"
            );
        }
    }

    #[test]
    fn strip_disc_dash_forms() {
        for raw in [
            "Les Concerts En Chine - CD 1",
            "Les Concerts En Chine - Disc 2",
            "Les Concerts En Chine - Vol 1",
            "Les Concerts En Chine \u{2013} CD 2",
            "Les Concerts En Chine \u{2014} CD 2",
        ] {
            assert_eq!(
                strip_trailing_disc_suffix(raw),
                "Les Concerts En Chine",
                "raw {raw:?}"
            );
        }
    }

    #[test]
    fn strip_disc_glued_forms() {
        for raw in [
            "Les Concerts en Chine vol2",
            "Les Concerts en Chine disc1",
            "Les Concerts en Chine cd3",
        ] {
            assert_eq!(
                strip_trailing_disc_suffix(raw),
                "Les Concerts en Chine",
                "raw {raw:?}"
            );
        }
    }

    #[test]
    fn strip_disc_leaves_non_disc_paren_alone() {
        // `Live at Wembley` is not a disc marker; the edition
        // stripper decides if `(Deluxe Edition)` stays.
        assert_eq!(
            strip_trailing_disc_suffix("Bad (Live at Wembley)"),
            "Bad (Live at Wembley)"
        );
    }

    #[test]
    fn album_ending_in_digits_not_treated_as_disc() {
        assert_eq!(strip_trailing_disc_suffix("Grease 2"), "Grease 2");
    }

    #[test]
    fn hyphen_inside_title_not_treated_as_disc_delimiter() {
        assert_eq!(
            strip_trailing_disc_suffix("Wham! - Last Christmas"),
            "Wham! - Last Christmas"
        );
    }

    // ------------------------------------------------------------
    // is_disc_only_basename — folder-rollup signal.
    // ------------------------------------------------------------

    #[test]
    fn disc_only_basename_recognised() {
        for name in [
            "Disc 1", "Disk 2", "CD 3", "Vol 1", "Volume 2", "Part 1",
            "Disc. 1", "cd 4",
        ] {
            assert!(is_disc_only_basename(name), "name {name:?}");
        }
    }

    #[test]
    fn non_disc_only_basename_rejected() {
        for name in [
            "Album",
            "Disc 1 Bonus Tracks",
            "The Ultimate Collection (Disc 1)",
            "",
            "CD",
            "Disc",
        ] {
            assert!(!is_disc_only_basename(name), "name {name:?}");
        }
    }

    // ------------------------------------------------------------
    // Edition-qualifier strip.
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
            assert_eq!(
                strip_trailing_edition_qualifier(raw),
                want,
                "raw {raw:?}"
            );
        }
    }

    #[test]
    fn stacked_edition_qualifiers_collapse() {
        assert_eq!(
            strip_trailing_edition_qualifier(
                "Rumours (Deluxe Edition) (Deluxe)"
            ),
            "Rumours"
        );
    }

    // ------------------------------------------------------------
    // clean_album_title — both strippers together.
    // ------------------------------------------------------------

    #[test]
    fn clean_album_title_strips_disc_then_edition() {
        assert_eq!(
            clean_album_title("Album X (Deluxe Edition) (Disc 1)"),
            "Album X"
        );
    }

    #[test]
    fn clean_album_title_nfc_normalises() {
        let decomposed = "Cafe\u{0301}";
        let composed = "Café";
        assert_eq!(clean_album_title(decomposed), clean_album_title(composed));
    }

    #[test]
    fn clean_album_title_empty_returns_empty() {
        assert!(clean_album_title("").is_empty());
        assert!(clean_album_title("   ").is_empty());
    }

    // ------------------------------------------------------------
    // Degenerate-tag detection.
    // ------------------------------------------------------------

    #[test]
    fn degenerate_when_album_matches_artist_credit_tokens() {
        assert!(is_degenerate_album_tag(
            "Paco De Lucia, Al Di Meola, John McLaughlin",
            "Paco de Lucía, Al Di Meola & John McLaughlin"
        ));
    }

    #[test]
    fn degenerate_when_album_empty() {
        assert!(is_degenerate_album_tag("", "Any Artist"));
    }

    #[test]
    fn non_degenerate_when_album_has_own_tokens() {
        assert!(!is_degenerate_album_tag("Kind of Blue", "Miles Davis"));
        assert!(!is_degenerate_album_tag(
            "The Beatles Anthology",
            "The Beatles"
        ));
    }
}
