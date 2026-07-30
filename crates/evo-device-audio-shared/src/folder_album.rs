// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Folder-anchored album identity.
//!
//! Enterprise music software (Plex, Jellyfin, Roon, Picard,
//! beets) treats the **containing directory** as the unit of an
//! album. Tracks in a folder are one album; disc subfolders and
//! disc-suffix sibling folders roll up to a canonical parent;
//! the folder basename supplies the title when the tag is
//! absent or degenerate.
//!
//! This module carries the shared identity primitive:
//!
//! - [`canonical_album_folder`] — resolves an MPD-relative
//!   track path to the canonical album-folder key (with
//!   subfolder rollup and disc-suffix rollup).
//!
//! Tag-derived identity is deliberately not offered here — the
//! browse-by-album call site derives display title / artist
//! from the aggregated tag population within each folder group,
//! but the GROUPING KEY is folder-anchored and cannot be
//! fragmented by tag drift.
//!
//! Rollup vocabulary (`Disc N`, `CD N`, `Vol N`, `Volume N`,
//! `Part N`, `Disc N of M`, plus paren / bracket / dash / glued
//! forms) is delegated to [`album_name`] so the same
//! vocabulary applies to folder rollup and title cleaning.
//!
//! [`album_name`]: crate::album_name

use crate::album_name::{is_disc_only_basename, strip_trailing_disc_suffix};

/// Resolve an MPD-relative track path to the canonical album-
/// folder key.
///
/// Rules, applied in order:
///
/// 1. **Bare disc subfolder** — when the track's immediate
///    parent folder basename IS a disc marker (`Disc 1`,
///    `CD 2`, `Volume 3`), the album folder is one level up:
///    `Artist/Album/Disc 1/track.mp3` → `Artist/Album`.
/// 2. **Disc-suffix sibling folder** — when the parent
///    basename ends with a disc suffix (`Goodbye Yellow Brick
///    Road (Disc 1)`, `Album - CD 1`, `Album vol2`), strip the
///    suffix and re-key on the sibling group so
///    `Artist/Album (Disc 1)/track.mp3` and
///    `Artist/Album (Disc 2)/track.mp3` share
///    `Artist/Album`.
/// 3. **No rollup** — use the parent directory verbatim.
///
/// Empty / root-relative paths return an empty key so the
/// browse enumeration drops them (a track directly under the
/// music root is not a valid album row — every real music
/// library has at least one artist / album layer).
///
/// Path separator: MPD emits forward slashes on every platform
/// including Windows, so this is a pure byte-position walk. No
/// filesystem access.
pub fn canonical_album_folder(mpd_relative_path: &str) -> String {
    let trimmed = mpd_relative_path.trim().trim_end_matches('/');
    // The track's parent directory.
    let Some((parent, _file)) = split_last_segment(trimmed) else {
        return String::new();
    };
    if parent.is_empty() {
        return String::new();
    }
    let Some((grandparent, basename)) = split_last_segment(parent) else {
        return parent.to_string();
    };
    // Rule 1: bare disc subfolder — roll up to grandparent.
    if is_disc_only_basename(basename) {
        return grandparent.to_string();
    }
    // Rule 2: disc-suffix sibling folder — strip and re-key.
    let stripped = strip_trailing_disc_suffix(basename);
    if stripped.len() != basename.len() {
        if grandparent.is_empty() {
            return stripped.to_string();
        }
        return format!("{grandparent}/{stripped}");
    }
    // Rule 3: no rollup.
    parent.to_string()
}

/// Split a `/`-delimited path into `(prefix, last_segment)`.
/// Returns `None` when `s` is empty. When `s` has no `/`,
/// returns `("", s)`. Trailing empty segments (e.g. a trailing
/// `/`) are ignored — the caller already `trim_end_matches`es.
fn split_last_segment(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    match s.rfind('/') {
        Some(idx) => Some((&s[..idx], &s[idx + 1..])),
        None => Some(("", s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------
    // Rule 1 — bare disc subfolder rolls up to grandparent.
    // ------------------------------------------------------------

    #[test]
    fn bare_disc_subfolder_rolls_up_to_grandparent() {
        for path in [
            "Artist/Album/Disc 1/track.mp3",
            "Artist/Album/Disc 2/track.mp3",
            "Artist/Album/CD 1/track.mp3",
            "Artist/Album/CD 2/track.mp3",
            "Artist/Album/Volume 3/track.mp3",
            "Artist/Album/Vol 1/track.mp3",
        ] {
            assert_eq!(
                canonical_album_folder(path),
                "Artist/Album",
                "path {path:?}",
            );
        }
    }

    #[test]
    fn bare_disc_subfolder_variants_all_share_same_key() {
        let a = canonical_album_folder("Artist/Album/Disc 1/t.mp3");
        let b = canonical_album_folder("Artist/Album/CD 2/t.mp3");
        let c = canonical_album_folder("Artist/Album/Volume 3/t.mp3");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    // ------------------------------------------------------------
    // Rule 2 — disc-suffix sibling folders share canonical key.
    // ------------------------------------------------------------

    #[test]
    fn disc_suffix_sibling_folders_share_canonical_key() {
        for path in [
            "Elton John/Goodbye Yellow Brick Road (Disc 1)/1-01.mp3",
            "Elton John/Goodbye Yellow Brick Road (Disc 2)/2-01.mp3",
        ] {
            assert_eq!(
                canonical_album_folder(path),
                "Elton John/Goodbye Yellow Brick Road",
                "path {path:?}",
            );
        }
    }

    #[test]
    fn dash_form_sibling_folders_share_canonical_key() {
        let a =
            canonical_album_folder("Jarre/Les Concerts En Chine - CD 1/t.mp3");
        let b =
            canonical_album_folder("Jarre/Les Concerts en Chine vol2/t.mp3");
        assert_eq!(a, "Jarre/Les Concerts En Chine");
        assert_eq!(b, "Jarre/Les Concerts en Chine");
        // NOTE: rollup preserves the folder's original text — a
        // library where sibling folders differ in casing keeps
        // them as separate tiles; case-collapse is a downstream
        // fold-key concern for grouping, not for the raw path.
    }

    #[test]
    fn bracketed_disc_suffix_sibling_folders_share_canonical_key() {
        for path in [
            "A/Album [CD 1]/t.mp3",
            "A/Album [CD 2]/t.mp3",
            "A/Album (Disc 1 of 3)/t.mp3",
        ] {
            assert_eq!(
                canonical_album_folder(path),
                "A/Album",
                "path {path:?}",
            );
        }
    }

    // ------------------------------------------------------------
    // Rule 3 — no rollup.
    // ------------------------------------------------------------

    #[test]
    fn regular_folder_stays_as_parent() {
        assert_eq!(
            canonical_album_folder("Bruno Mars/Earth To Mars/track.mp3"),
            "Bruno Mars/Earth To Mars",
        );
    }

    #[test]
    fn non_disc_parenthetical_preserved() {
        // `(Live at Wembley)` is not a disc marker — the
        // folder stays as-is; two "Bad (Live at Wembley)"
        // folders in different artists still separate via
        // their parent, and the tile display is a downstream
        // concern.
        assert_eq!(
            canonical_album_folder("MJ/Bad (Live at Wembley)/t.mp3"),
            "MJ/Bad (Live at Wembley)",
        );
    }

    // ------------------------------------------------------------
    // Root-only + edge shapes.
    // ------------------------------------------------------------

    #[test]
    fn empty_path_returns_empty_key() {
        assert!(canonical_album_folder("").is_empty());
        assert!(canonical_album_folder("   ").is_empty());
    }

    #[test]
    fn root_level_track_returns_empty_key() {
        // A file directly under the music root has no album
        // folder — drop it.
        assert!(canonical_album_folder("stray-track.mp3").is_empty());
    }

    #[test]
    fn trailing_slash_tolerated() {
        assert_eq!(canonical_album_folder("A/Album/track.mp3/"), "A/Album",);
    }

    // ------------------------------------------------------------
    // Regression: standard vs deluxe with different track counts
    // are NOT merged — deluxe/standard are release siblings, not
    // disc-suffix siblings, and their folder basenames differ.
    // ------------------------------------------------------------

    #[test]
    fn deluxe_and_standard_stay_separate_folders() {
        let standard = canonical_album_folder("Artist/Rumours/t.mp3");
        let deluxe =
            canonical_album_folder("Artist/Rumours (Deluxe Edition)/t.mp3");
        assert_ne!(standard, deluxe);
    }

    // ------------------------------------------------------------
    // Regression: real deep hierarchies with disc-only names on
    // the bottom layer roll up to the album folder — not further.
    // ------------------------------------------------------------

    #[test]
    fn rollup_stops_at_album_folder() {
        // `Compilation/2024/Album/Disc 1/t.mp3` — the bare
        // disc rolls up to `Compilation/2024/Album`, not to
        // `Compilation/2024` (the year folder).
        assert_eq!(
            canonical_album_folder("Compilation/2024/Album/Disc 1/t.mp3"),
            "Compilation/2024/Album",
        );
    }
}
