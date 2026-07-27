// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Artist name folding and display cleaning.
//!
//! MPD's raw `artist` / `albumartist` tag store carries per-file
//! drift that surfaces the same real artist as multiple facet
//! tiles (fold-key gap) or as a tile whose display value still
//! carries tag-editor garbage (display gap). The two functions
//! in this module close both gaps with a single implementation
//! shared across every plugin that consumes artist names in
//! this distribution:
//!
//! - [`artist_fold_key`] — group discriminator. Two raw forms
//!   with the same fold-key belong to the same real artist.
//!   Used by the `playback.mpd` browse facet for dedupe, and by
//!   `artwork.online`'s reconcile cache as the key that stays
//!   stable across display cleaning.
//! - [`artist_display_form`] — human-readable presentation of a
//!   raw form. Strips tag-editor watermarks, reverses `Last,
//!   First` sort form when the head is a single token, and
//!   NFC-normalises so composed and decomposed forms of the
//!   same string render identically. Used by the browse facet
//!   at render time so the tile label is never the raw dirty
//!   tag.
//!
//! Both operate on `&str` and produce owned `String`; both are
//! pure (no allocation of external state, no async, no
//! blocking).

use unicode_normalization::UnicodeNormalization;

/// Fold an artist tag value into a dedupe key.
///
/// Two raw tag forms that produce the same fold-key are treated
/// as the same real artist for grouping (browse facet dedupe;
/// reconcile cache lookup). The key is not human-readable; it is
/// only a group discriminator. Callers keep the raw form (or a
/// cleaned form via [`artist_display_form`]) for display.
///
/// Fold rules, applied in order:
///
/// 1. Trim; empty → empty key (caller drops).
/// 2. Strip trailing `| <suffix>` (tag-editor watermarks). The
///    `|` is exceptionally rare in real artist names; anything
///    after it is more likely a URL / tagger signature than
///    part of the credit.
/// 3. Split on `,`; if the head is a plausible last name
///    (single token) and the tail is a plausible first-name
///    run, reorder to `<tail> <head>`. Handles `Cohen, Leonard`
///    → `Leonard Cohen` but leaves `Nick Cave, Kylie Minogue`
///    (duet credit) unchanged because the head has multiple
///    tokens.
/// 4. NFD-decompose, drop combining marks (U+0300..=U+036F —
///    Latin diacritics), lowercase, replace every non-alnum
///    run with a single space, trim.
pub fn artist_fold_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_watermark = match trimmed.split_once('|') {
        Some((head, _)) => head.trim(),
        None => trimmed,
    };
    let reordered: String =
        if let Some((head, tail)) = without_watermark.split_once(',') {
            let head_trim = head.trim();
            let tail_trim = tail.trim();
            let head_is_single_token = !head_trim.contains(char::is_whitespace);
            let tail_has_no_comma = !tail_trim.contains(',');
            if head_is_single_token
                && tail_has_no_comma
                && !head_trim.is_empty()
                && !tail_trim.is_empty()
            {
                format!("{tail_trim} {head_trim}")
            } else {
                without_watermark.to_string()
            }
        } else {
            without_watermark.to_string()
        };
    let mut folded = String::with_capacity(reordered.len());
    let mut last_was_space = true;
    for ch in reordered.nfd() {
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

/// Clean a raw artist tag into a display form.
///
/// The rules mirror the fold-key's first three steps so that a
/// tile's label matches how the fold-key groups it — a group
/// whose winning raw form is `Bruno Mars | www.RNBxBeatz.com`
/// renders as `Bruno Mars`; a group whose winning raw form is
/// `Cohen, Leonard` renders as `Leonard Cohen`; a diacritic
/// composed one way renders NFC.
///
/// Rules:
///
/// 1. Trim; empty → empty (caller may fall back to the raw
///    string).
/// 2. Strip trailing `| <suffix>` watermark, if present.
/// 3. Reverse `Last, First` → `First Last` when the head is a
///    single token (mirror of the fold-key rule). Multi-token
///    comma heads (duet credits like `Nick Cave, Kylie
///    Minogue`) stay as-is.
/// 4. Trim any residual whitespace.
/// 5. NFC-normalise so `Café` composed and `Cafe\u{0301}`
///    decomposed render identically on the wire.
///
/// If every cleaning step produces an empty string, returns
/// the trimmed original so the tile always has a label. Never
/// returns an empty string when the input has content.
pub fn artist_display_form(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_watermark = match trimmed.split_once('|') {
        Some((head, _)) => head.trim(),
        None => trimmed,
    };
    let reordered: String =
        if let Some((head, tail)) = without_watermark.split_once(',') {
            let head_trim = head.trim();
            let tail_trim = tail.trim();
            let head_is_single_token = !head_trim.contains(char::is_whitespace);
            let tail_has_no_comma = !tail_trim.contains(',');
            if head_is_single_token
                && tail_has_no_comma
                && !head_trim.is_empty()
                && !tail_trim.is_empty()
            {
                format!("{tail_trim} {head_trim}")
            } else {
                without_watermark.to_string()
            }
        } else {
            without_watermark.to_string()
        };
    let cleaned = reordered.trim();
    if cleaned.is_empty() {
        return trimmed.nfc().collect();
    }
    cleaned.nfc().collect()
}

fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32, 0x0300..=0x036F)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------
    // artist_fold_key — group discriminator
    // ------------------------------------------------------------

    #[test]
    fn fold_key_collapses_diacritic_forms() {
        assert_eq!(
            artist_fold_key("Céline Dion"),
            artist_fold_key("Celine Dion")
        );
        assert_eq!(
            artist_fold_key("Sinéad O'Connor"),
            artist_fold_key("Sinead O'Connor")
        );
    }

    #[test]
    fn fold_key_reorders_sort_form() {
        assert_eq!(
            artist_fold_key("Cohen, Leonard"),
            artist_fold_key("Leonard Cohen")
        );
    }

    #[test]
    fn fold_key_strips_pipe_watermark() {
        assert_eq!(
            artist_fold_key("Bruno Mars | www.RNBxBeatz.com"),
            artist_fold_key("Bruno Mars")
        );
    }

    #[test]
    fn fold_key_collapses_hyphen_variants() {
        assert_eq!(
            artist_fold_key("Jean-Michel Jarre"),
            artist_fold_key("Jean Michel Jarre")
        );
    }

    #[test]
    fn fold_key_leaves_multi_artist_credits_alone() {
        let a = artist_fold_key("Nick Cave, Kylie Minogue");
        let b = artist_fold_key("Kylie Minogue Nick Cave");
        assert_ne!(a, b);
    }

    #[test]
    fn fold_key_empty_when_blank() {
        assert!(artist_fold_key("").is_empty());
        assert!(artist_fold_key("   ").is_empty());
        assert!(artist_fold_key("|").is_empty());
    }

    // ------------------------------------------------------------
    // artist_display_form — human-readable presentation
    // ------------------------------------------------------------

    #[test]
    fn display_strips_pipe_watermark() {
        assert_eq!(
            artist_display_form("Bruno Mars | www.RNBxBeatz.com"),
            "Bruno Mars"
        );
        assert_eq!(
            artist_display_form("Bruno Mars|www.RNBxBeatz.com"),
            "Bruno Mars"
        );
    }

    #[test]
    fn display_reverses_last_first_when_head_is_single_token() {
        assert_eq!(artist_display_form("Cohen, Leonard"), "Leonard Cohen");
        assert_eq!(artist_display_form("Collins, Phil"), "Phil Collins");
    }

    #[test]
    fn display_leaves_multi_artist_credits_unchanged() {
        // Head has multiple tokens → a comma is a credit
        // separator, not a sort-form marker.
        assert_eq!(
            artist_display_form("Nick Cave, Kylie Minogue"),
            "Nick Cave, Kylie Minogue"
        );
    }

    #[test]
    fn display_preserves_diacritics() {
        // Display keeps Unicode; only the fold-key strips
        // diacritics for grouping.
        assert_eq!(artist_display_form("Céline Dion"), "Céline Dion");
        assert_eq!(artist_display_form("Sinéad O'Connor"), "Sinéad O'Connor");
    }

    #[test]
    fn display_nfc_normalises_decomposed_input() {
        // "Cafe" + combining acute → NFC composes to "Café".
        let decomposed = "Cafe\u{0301}";
        let composed = "Café";
        assert_eq!(artist_display_form(decomposed), composed);
        assert_eq!(
            artist_display_form(decomposed),
            artist_display_form(composed)
        );
    }

    #[test]
    fn display_falls_back_to_trimmed_input_when_cleaning_would_empty() {
        // Pipe with nothing before it: cleaning produces empty,
        // display returns the trimmed original so the tile is
        // never blank.
        assert_eq!(artist_display_form("|watermark only"), "|watermark only");
    }

    #[test]
    fn display_empty_when_input_is_blank() {
        assert!(artist_display_form("").is_empty());
        assert!(artist_display_form("   ").is_empty());
    }

    #[test]
    fn fold_key_matches_between_raw_and_display() {
        // A tile whose display value has been cleaned still
        // groups under the same fold-key as its raw form —
        // caches keyed on fold(raw) also key on fold(display).
        let raw = "Cohen, Leonard";
        let display = artist_display_form(raw);
        assert_eq!(artist_fold_key(raw), artist_fold_key(&display));
        let raw2 = "Bruno Mars | www.RNBxBeatz.com";
        let display2 = artist_display_form(raw2);
        assert_eq!(artist_fold_key(raw2), artist_fold_key(&display2));
    }
}
