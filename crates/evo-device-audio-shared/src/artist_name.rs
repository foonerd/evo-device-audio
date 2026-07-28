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
//!   raw form. Strips tag-editor watermarks, trailing album
//!   parentheticals, self-duplicating `A/A` slash forms,
//!   normalises Various Artists, reverses `Last, First` sort
//!   form when the head is a single token, and NFC-normalises
//!   so composed and decomposed forms of the same string render
//!   identically. Used by the browse facet at render time so
//!   the tile label is never the raw dirty tag.
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
/// 2. Strip trailing `| <suffix>` (tag-editor watermarks).
/// 3. Strip trailing `(…)` / `[…]` parentheticals (album titles
///    stuffed into the artist tag, e.g. `Paul McCartney (Off
///    The Ground)`).
/// 4. Collapse self-duplicating slash forms (`Passenger/Passenger`
///    → `Passenger`) when both sides fold-equal after a light
///    normalise. Distinct collab sides (`A/B`) stay intact.
/// 5. Split on `,`; if the head is a plausible last name
///    (single token) and the tail is a plausible first-name
///    run, reorder to `<tail> <head>`.
/// 6. NFD-decompose, drop combining marks (U+0300..=U+036F),
///    lowercase, replace every non-alnum run with a single
///    space, trim.
/// 7. Canonicalise Various Artists aliases (`va`, `various`,
///    `variousartists`, `various artists`) to one fold key.
pub fn artist_fold_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    // Strip watermark + parens + self-slash before delimiter
    // sniffing, so `Nick Cave, Kylie Minogue (Duet)` still
    // detects a collab.
    let without_watermark = match trimmed.split_once('|') {
        Some((head, _)) => head.trim(),
        None => trimmed,
    };
    let without_parens = strip_trailing_parentheticals(without_watermark);
    let without_self_slash = collapse_self_slash(&without_parens);
    // Collab-first: order-invariant fold when a delimiter is
    // present. `Nick Cave, Kylie Minogue`, `Kylie Minogue &
    // Nick Cave`, `Nick Cave and Kylie Minogue`, and
    // `Nick Cave; Kylie Minogue` all fold to one key.
    if let Some(collab_key) = collab_fold_key(&without_self_slash) {
        return collab_key;
    }
    let reordered = reorder_last_first(&without_self_slash);
    if reordered.is_empty() {
        return String::new();
    }
    let folded = fold_alnum(&reordered);
    canonicalize_various_artists_fold(folded)
}

/// Clean a raw artist tag into a display form.
///
/// The rules mirror the fold-key's structural steps so that a
/// tile's label matches how the fold-key groups it — a group
/// whose winning raw form is `Bruno Mars | www.RNBxBeatz.com`
/// renders as `Bruno Mars`; `Passenger/Passenger` renders as
/// `Passenger`; `Paul McCartney (Off The Ground)` renders as
/// `Paul McCartney`; Various Artists aliases render as
/// `Various Artists`; `Cohen, Leonard` renders as `Leonard
/// Cohen`; a diacritic composed one way renders NFC.
///
/// If every cleaning step produces an empty string, returns
/// the trimmed original so the tile always has a label. Never
/// returns an empty string when the input has content.
pub fn artist_display_form(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let pre = preprocess_artist(raw);
    if pre.is_empty() {
        return trimmed.nfc().collect();
    }
    let display = if is_various_artists_alias(&pre) {
        "Various Artists".to_string()
    } else {
        pre
    };
    display.nfc().collect()
}

/// Shared structural cleaning used by both fold-key and display.
/// Order matters: watermark → parentheticals → self-slash →
/// collab-credit split → `Last, First` reorder. The collab
/// step runs BEFORE the sort-form reorder so `Nick Cave,
/// Kylie Minogue` (multi-token head) is treated as a two-
/// artist credit rather than mis-reordered.
fn preprocess_artist(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let without_watermark = match trimmed.split_once('|') {
        Some((head, _)) => head.trim(),
        None => trimmed,
    };
    let without_parens = strip_trailing_parentheticals(without_watermark);
    let without_self_slash = collapse_self_slash(&without_parens);
    // Collab-credit collapse: multi-artist credits with any of
    // `;` / ` & ` / ` and ` (or multi-token comma) delimiters
    // fold to the same key regardless of order or delimiter,
    // and render with a canonical `, ` join. Returns the raw
    // input unchanged when no collab delimiter is present.
    let after_collab = collapse_collab(&without_self_slash);
    if after_collab.was_collab {
        return after_collab.display;
    }
    reorder_last_first(&without_self_slash)
}

/// Result of the collab-credit collapse.
struct CollabOutcome {
    /// The display string — parts joined with `, ` in the
    /// order they appeared in the input. Same across
    /// `;` / ` & ` / ` and ` / multi-token-comma
    /// delimiters when the parts are the same.
    display: String,
    /// True iff a collab delimiter was found. When false the
    /// caller should apply its normal single-artist path
    /// (`Last, First` reorder). Without this flag the caller
    /// would clobber a genuine sort-form.
    was_collab: bool,
}

/// Split a raw string into artist credits when a collab
/// delimiter is present. Delimiters (checked in order):
///
/// 1. `;` — unambiguous multi-credit separator.
/// 2. ` & ` (space-ampersand-space) — mainstream multi-credit
///    separator. Sub-string `&` in `AT&T` stays intact
///    because there's no surrounding whitespace.
/// 3. ` and ` (space-and-space, case-insensitive at the
///    ASCII boundary). Sub-string `and` in `Bandanas` stays
///    intact for the same reason. Only splits when the
///    surrounding whitespace shows it is a joining word,
///    not the middle of a token.
/// 4. `,` — split ONLY when the head has multiple
///    whitespace-separated tokens. This preserves the sort-
///    form heuristic (single-token head = `Last, First`)
///    while catching duet credits like `Nick Cave, Kylie
///    Minogue`.
///
/// Returns the parts trimmed but otherwise unmodified. When
/// no delimiter matches, returns `None` so the caller can
/// apply the single-artist path.
fn split_collab(s: &str) -> Option<Vec<&str>> {
    if let Some(parts) = split_on(s, ";") {
        return Some(parts);
    }
    if let Some(parts) = split_on(s, " & ") {
        return Some(parts);
    }
    if let Some(parts) = split_on_ci(s, " and ") {
        return Some(parts);
    }
    // Multi-token-comma: only when the head has multiple
    // tokens. Preserves `Cohen, Leonard` as sort-form.
    if let Some((head, _)) = s.split_once(',') {
        if head.trim().contains(char::is_whitespace) {
            return Some(
                s.split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .collect(),
            );
        }
    }
    None
}

fn split_on<'a>(s: &'a str, delim: &str) -> Option<Vec<&'a str>> {
    if !s.contains(delim) {
        return None;
    }
    Some(
        s.split(delim)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect(),
    )
}

fn split_on_ci<'a>(s: &'a str, delim_lc: &str) -> Option<Vec<&'a str>> {
    // Case-insensitive locate of ASCII delimiter. Works on the
    // small ASCII delimiter shape we use (` and `); the case-
    // sensitive fast-path handles everything else.
    let lower = s.to_ascii_lowercase();
    if !lower.contains(delim_lc) {
        return None;
    }
    // Walk byte-boundaries in the lowercase copy and slice the
    // original at the same byte positions. This is safe
    // because ASCII lower-case does not change byte width for
    // any character.
    let mut parts = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(delim_lc) {
        let end = cursor + rel;
        let part = s[cursor..end].trim();
        if !part.is_empty() {
            parts.push(part);
        }
        cursor = end + delim_lc.len();
    }
    let tail = s[cursor..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    if parts.len() < 2 {
        None
    } else {
        Some(parts)
    }
}

fn collapse_collab(s: &str) -> CollabOutcome {
    let Some(parts) = split_collab(s) else {
        return CollabOutcome {
            display: s.to_string(),
            was_collab: false,
        };
    };
    // Each part may itself carry parenthetical / self-slash
    // noise; recurse on `preprocess_artist` so the same rules
    // apply to every credit in the collab. To avoid infinite
    // recursion when the recursion re-enters collab detection
    // for a part that (somehow) still carries a delimiter, we
    // cap here: run the sibling cleaners inline.
    let clean_parts: Vec<String> = parts
        .into_iter()
        .map(|p| {
            let no_parens = strip_trailing_parentheticals(p);
            let no_slash = collapse_self_slash(&no_parens);
            reorder_last_first(&no_slash).trim().to_string()
        })
        .filter(|p| !p.is_empty())
        .collect();
    // Dedupe on fold — same fold-key parts collapse to one so
    // `A, A` or `A and a` become a single-credit tile.
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    for p in clean_parts {
        let key = fold_alnum(&p);
        if key.is_empty() {
            continue;
        }
        if seen.insert(key) {
            unique.push(p);
        }
    }
    if unique.is_empty() {
        return CollabOutcome {
            display: s.to_string(),
            was_collab: false,
        };
    }
    if unique.len() == 1 {
        return CollabOutcome {
            display: unique.into_iter().next().unwrap(),
            was_collab: true,
        };
    }
    // Fold-key MUST be order-invariant so `A and B` and
    // `B & A` produce the same fold. Sort a *copy* of the
    // clean parts by their fold-key; join with " & " as the
    // canonical fold-delimiter. The DISPLAY keeps the
    // original order (as-presented) with a `, ` join.
    let display = unique.join(", ");
    CollabOutcome {
        display,
        was_collab: true,
    }
}

/// Fold-key form of a collab. When the input is a collab
/// credit, returns `Some(canonical_fold)` — the parts sorted
/// by their individual fold, joined with ` & `. Otherwise
/// `None`, and the caller falls back to the single-artist
/// fold path.
fn collab_fold_key(s: &str) -> Option<String> {
    let parts = split_collab(s)?;
    let mut folded_parts: Vec<String> = parts
        .into_iter()
        .map(|p| {
            let no_parens = strip_trailing_parentheticals(p);
            let no_slash = collapse_self_slash(&no_parens);
            let reordered = reorder_last_first(&no_slash);
            fold_alnum(&reordered)
        })
        .filter(|p| !p.is_empty())
        .collect();
    if folded_parts.is_empty() {
        return None;
    }
    folded_parts.sort();
    folded_parts.dedup();
    Some(folded_parts.join(" & "))
}

/// Strip trailing `(…)` / `[…]` groups (optionally preceded by
/// whitespace). Repeats so stacked junk falls away. Nested
/// brackets inside the trailing group are left alone (bail).
fn strip_trailing_parentheticals(s: &str) -> String {
    let mut out = s.trim().to_string();
    loop {
        let trimmed = out.trim_end();
        let bytes = trimmed.as_bytes();
        if bytes.len() < 3 {
            return trimmed.to_string();
        }
        let close = bytes[bytes.len() - 1];
        let open = match close {
            b')' => b'(',
            b']' => b'[',
            _ => return trimmed.to_string(),
        };
        let Some(open_idx) =
            trimmed.as_bytes().iter().rposition(|&b| b == open)
        else {
            return trimmed.to_string();
        };
        let inner = &trimmed[open_idx + 1..trimmed.len() - 1];
        if inner.as_bytes().contains(&open) || inner.as_bytes().contains(&close)
        {
            return trimmed.to_string();
        }
        out = trimmed[..open_idx].trim_end().to_string();
    }
}

/// `A/A` (and `A / A`) → `A` when both sides are the same under
/// a light fold. Distinct sides (`A/B`) are left unchanged.
fn collapse_self_slash(s: &str) -> String {
    let Some((left, right)) = s.split_once('/') else {
        return s.to_string();
    };
    if right.contains('/') {
        return s.to_string();
    }
    let l = left.trim();
    let r = right.trim();
    if l.is_empty() || r.is_empty() {
        return s.to_string();
    }
    if fold_alnum(l) == fold_alnum(r) {
        l.to_string()
    } else {
        s.to_string()
    }
}

fn reorder_last_first(s: &str) -> String {
    if let Some((head, tail)) = s.split_once(',') {
        let head_trim = head.trim();
        let tail_trim = tail.trim();
        let head_is_single_token = !head_trim.contains(char::is_whitespace);
        let tail_has_no_comma = !tail_trim.contains(',');
        if head_is_single_token
            && tail_has_no_comma
            && !head_trim.is_empty()
            && !tail_trim.is_empty()
        {
            return format!("{tail_trim} {head_trim}");
        }
    }
    s.to_string()
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

fn is_various_artists_alias(s: &str) -> bool {
    matches!(
        fold_alnum(s).as_str(),
        "va" | "various" | "variousartists" | "various artists"
    )
}

fn canonicalize_various_artists_fold(folded: String) -> String {
    match folded.as_str() {
        "va" | "various" | "variousartists" | "various artists" => {
            "various artists".to_string()
        }
        _ => folded,
    }
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

    #[test]
    fn fold_key_collapses_self_slash() {
        assert_eq!(
            artist_fold_key("Passenger/Passenger"),
            artist_fold_key("Passenger")
        );
        assert_eq!(
            artist_fold_key("Passenger / Passenger"),
            artist_fold_key("Passenger")
        );
    }

    #[test]
    fn fold_key_leaves_distinct_slash_collab() {
        assert_ne!(
            artist_fold_key("Artist A/Artist B"),
            artist_fold_key("Artist A")
        );
    }

    #[test]
    fn fold_key_strips_trailing_parenthetical() {
        assert_eq!(
            artist_fold_key("Paul McCartney (Off The Ground)"),
            artist_fold_key("Paul McCartney")
        );
    }

    #[test]
    fn fold_key_collapses_various_artists_aliases() {
        assert_eq!(artist_fold_key("Various Artists"), "various artists");
        assert_eq!(artist_fold_key("VariousArtists"), "various artists");
        assert_eq!(artist_fold_key("Various"), "various artists");
        assert_eq!(artist_fold_key("VA"), "various artists");
        assert_eq!(artist_fold_key("Various Artists"), artist_fold_key("VA"));
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
    fn display_collapses_self_slash() {
        assert_eq!(artist_display_form("Passenger/Passenger"), "Passenger");
        assert_eq!(artist_display_form("Passenger / Passenger"), "Passenger");
    }

    #[test]
    fn display_leaves_distinct_slash_collab() {
        assert_eq!(
            artist_display_form("Artist A/Artist B"),
            "Artist A/Artist B"
        );
    }

    #[test]
    fn display_strips_trailing_parenthetical() {
        assert_eq!(
            artist_display_form("Paul McCartney (Off The Ground)"),
            "Paul McCartney"
        );
        assert_eq!(
            artist_display_form("Pink Floyd [Remastered]"),
            "Pink Floyd"
        );
    }

    #[test]
    fn display_normalises_various_artists() {
        assert_eq!(artist_display_form("VariousArtists"), "Various Artists");
        assert_eq!(artist_display_form("Various"), "Various Artists");
        assert_eq!(artist_display_form("VA"), "Various Artists");
        assert_eq!(artist_display_form("Various Artists"), "Various Artists");
    }

    #[test]
    fn fold_key_matches_between_raw_and_display() {
        // A tile whose display value has been cleaned still
        // groups under the same fold-key as its raw form —
        // caches keyed on fold(raw) also key on fold(display).
        for raw in [
            "Cohen, Leonard",
            "Bruno Mars | www.RNBxBeatz.com",
            "Passenger/Passenger",
            "Paul McCartney (Off The Ground)",
            "VariousArtists",
            "VA",
        ] {
            let display = artist_display_form(raw);
            assert_eq!(
                artist_fold_key(raw),
                artist_fold_key(&display),
                "fold mismatch for {raw:?} → {display:?}"
            );
        }
    }

    // ------------------------------------------------------------
    // Collab-credit folding (Part 1.6 of the artist-facet contract)
    // ------------------------------------------------------------

    #[test]
    fn fold_key_collapses_collab_across_delimiters_and_order() {
        // The three glass symptoms from the screenshot: Al Di
        // Meola / John McLaughlin / Paco de Lucía in `and`-,
        // `&`-, and `;`-delimited forms all fold to one key
        // regardless of order.
        let variants = [
            "Al Di Meola and John McLaughlin and Paco de Lucía",
            "Al Di Meola & John McLaughlin & Paco de Lucía",
            "Al Di Meola; John McLaughlin; Paco de Lucía",
            "Paco de Lucía & Al Di Meola & John McLaughlin",
            "John McLaughlin, Al Di Meola, Paco de Lucía",
        ];
        let first = artist_fold_key(variants[0]);
        for v in &variants[1..] {
            assert_eq!(
                artist_fold_key(v),
                first,
                "collab fold-key differs for {v:?}",
            );
        }
    }

    #[test]
    fn fold_key_collapses_two_artist_collab_across_delimiters() {
        let a = artist_fold_key("Nick Cave, Kylie Minogue");
        let b = artist_fold_key("Kylie Minogue & Nick Cave");
        let c = artist_fold_key("Nick Cave and Kylie Minogue");
        let d = artist_fold_key("Kylie Minogue; Nick Cave");
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
    }

    #[test]
    fn fold_key_keeps_last_first_sort_form_distinct_from_collab_key() {
        // Comma with single-token head stays sort-form
        // (`Cohen, Leonard` → `Leonard Cohen`) — the collab
        // detector honours the multi-token-head discriminator.
        // Distinct fold-keys with the pattern verifies the
        // rule holds both directions.
        let sort_form = artist_fold_key("Cohen, Leonard");
        let collab = artist_fold_key("Cohen & Leonard");
        assert_ne!(sort_form, collab);
    }

    #[test]
    fn display_form_collab_renders_canonical_comma_join() {
        // Display keeps the input order — no alphabetical
        // sort — but always joins with ", " so the tile
        // renders the same regardless of the original
        // delimiter.
        assert_eq!(
            artist_display_form(
                "Al Di Meola and John McLaughlin and Paco de Lucía"
            ),
            "Al Di Meola, John McLaughlin, Paco de Lucía"
        );
        assert_eq!(
            artist_display_form("Kylie Minogue; Nick Cave"),
            "Kylie Minogue, Nick Cave"
        );
    }

    #[test]
    fn display_form_collab_dedupes_within_credit() {
        // `A, A` inside a comma-collab folds to one credit.
        // `A and A` likewise.
        assert_eq!(artist_display_form("Artist A, Artist A"), "Artist A");
        assert_eq!(artist_display_form("Artist A and Artist A"), "Artist A");
    }

    #[test]
    fn fold_key_matches_display_form_for_collabs() {
        // fold(raw) == fold(display(raw)) invariant holds for
        // collab shapes too — critical for the reconcile
        // cache to hit whether the cache was populated from
        // the raw MPD tag or the cleaned display value.
        for raw in [
            "Al Di Meola and John McLaughlin and Paco de Lucía",
            "Paco de Lucía & Al Di Meola & John McLaughlin",
            "Nick Cave, Kylie Minogue",
            "Kylie Minogue; Nick Cave",
        ] {
            let display = artist_display_form(raw);
            assert_eq!(
                artist_fold_key(raw),
                artist_fold_key(&display),
                "collab fold mismatch for {raw:?} → {display:?}"
            );
        }
    }
}
