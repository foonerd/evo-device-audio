// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Sidecar cover-art primitives shared by the artwork-resolving
//! plugin (`artwork.local`) and the browse-tile emitter
//! (`playback.mpd`). One filesystem walk, one priority list,
//! consumed by both — so the resolver and the emitter can never
//! diverge on what counts as folder art.
//!
//! The library facet's per-directory cover cascade layers on top:
//!
//! 1. Direct sidecar in the browsed directory itself.
//! 2. Representative child cover — the first stable-sorted child
//!    subdirectory whose top level carries a sidecar.
//!
//! `find_cover_in_directory` powers both steps verbatim.
//! `find_artist_images_in_directory` is the artist-portrait
//! counterpart: an operator's `artist*.*` file names a person,
//! where a cover file names a record.

use std::path::{Path, PathBuf};

/// Priority-ordered cover-art filenames in **lowercase**. The
/// directory walk lowercases each entry against this list, so
/// `Cover.JPG`, `FOLDER.jpg`, `CoVeR.JpG`, and Unicode-cased
/// variants all resolve without listing every permutation.
///
/// Ordering follows the established convention (cover > folder >
/// front > coverart > albumart > artist variants > scan > album)
/// and matches volumio-evo's reference list. `.webp` entries are
/// included so libraries already adopting modern formats are
/// served without operator action.
pub const COVER_FILE_NAMES: &[&str] = &[
    "cover.jpg",
    "folder.jpg",
    "cover.png",
    "folder.png",
    "coverart.jpg",
    "albumart.jpg",
    "coverart.png",
    "albumart.png",
    "artists.jpg",
    "artist.jpg",
    "artists.png",
    "artist.png",
    "front.jpg",
    "front.png",
    "album.jpg",
    "scan.jpg",
    "cover.webp",
    "folder.webp",
    "front.webp",
    "artists.webp",
];

/// Image file extensions accepted as last-resort cover candidates
/// when the priority list above misses. Matches volumio-evo's
/// fallback: any image in the directory is treated as cover art
/// unless it exceeds [`MAX_COVER_BYTES`].
pub const FALLBACK_IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// Maximum sidecar file size accepted as cover art. Above this
/// the file is rejected as cover-art-too-large — embedded
/// extraction or online providers take over downstream.
/// Matches volumio-evo's 5 MB ceiling.
pub const MAX_COVER_BYTES: u64 = 5_000_000;

/// Walk `dir`'s top level (NOT subdirectories) for a sidecar
/// cover file. Priority pass against [`COVER_FILE_NAMES`] first,
/// then a fallback pass over [`FALLBACK_IMAGE_EXTENSIONS`]. Both
/// passes skip files above [`MAX_COVER_BYTES`] so an accidental
/// PSD or scan PDF never wins over a smaller image elsewhere in
/// the directory.
///
/// Returns `None` on I/O errors (unreadable directory, no image
/// matches). Consumers treat `None` as "no cover here" — never
/// as an error to propagate; the cascade continues with the next
/// tier.
pub fn find_cover_in_directory(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
        Ok(it) => it.filter_map(Result::ok).collect(),
        Err(_) => return None,
    };
    let mut by_lower: std::collections::HashMap<String, &std::fs::DirEntry> =
        std::collections::HashMap::with_capacity(entries.len());
    for e in &entries {
        if let Some(name) = e.file_name().to_str() {
            by_lower.insert(name.to_lowercase(), e);
        }
    }
    for priority_name in COVER_FILE_NAMES {
        if let Some(entry) = by_lower.get(*priority_name) {
            let path = entry.path();
            if cover_size_ok(&path) {
                return Some(path);
            }
        }
    }
    for e in &entries {
        let path = e.path();
        let Some(ext) = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_lowercase)
        else {
            continue;
        };
        if !FALLBACK_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }
        if cover_size_ok(&path) {
            return Some(path);
        }
    }
    None
}

/// Filename stem that marks a file as an artist portrait rather
/// than a record's cover. Matched case-insensitively as a
/// PREFIX, so `artist.jpg`, `Artist.png`, `ARTIST2.jpeg` and
/// `artist-live.webp` all qualify.
pub const ARTIST_IMAGE_PREFIX: &str = "artist";

/// Every `artist*.*` image in `dir`'s top level, ordered by
/// filename ascending.
///
/// Separate from [`find_cover_in_directory`] because the two
/// answer different questions. A cover file names a *record*; an
/// `artist*.*` file names a *person*. Serving one where the
/// other was asked for is the wrong-subject failure that no
/// image inspection can detect — the picture is real, it is
/// simply of the wrong thing.
///
/// Returns every match rather than the first so a subject with
/// several portraits can be presented as a slideshow. Callers
/// that need a single representative image take the first: the
/// ordering is by lowercased filename, so it is stable across
/// case differences and across filesystems that enumerate in
/// arbitrary order.
///
/// Empty vector on I/O failure or no matches — never an error to
/// propagate; the caller falls through to its next source.
pub fn find_artist_images_in_directory(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut matches: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_lowercase();
            let path = e.path();
            let ext = path
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_lowercase)?;
            if !name.starts_with(ARTIST_IMAGE_PREFIX) {
                return None;
            }
            if !FALLBACK_IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                return None;
            }
            if !cover_size_ok(&path) {
                return None;
            }
            Some((name, path))
        })
        .collect();
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    matches.into_iter().map(|(_, p)| p).collect()
}

/// True when the candidate file exists and is below
/// [`MAX_COVER_BYTES`]. A metadata error (transient I/O,
/// permission denied) skips the candidate without aborting
/// the walk — the next priority entry gets a fair attempt.
fn cover_size_ok(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && m.len() <= MAX_COVER_BYTES,
        Err(_) => false,
    }
}

/// Stable-sorted child subdirectory basenames of `dir` (top level
/// only, non-recursive). Consumed by the browse-tile emitter's
/// Tier 2 pass — the first stable-sorted child whose top level
/// carries a sidecar wins.
///
/// Returns an empty vector on I/O failure so the emitter falls
/// through to Tier 3 (artist-name portrait) or the honest glyph.
/// Skips symlinks so a symlinked directory inside a library
/// can't sneak a child cover in from outside the tree.
pub fn stable_sorted_child_dir_names(dir: &Path) -> Vec<String> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let ft = entry.file_type().ok()?;
            if ft.is_symlink() || !ft.is_dir() {
                return None;
            }
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            Some(name)
        })
        .collect();
    names.sort();
    names
}

/// Stable-sorted basename of the first audio track at `dir`'s
/// top level (non-recursive), or `None` when the directory
/// carries no lofty-recognisable audio file. Powers the
/// browse-tile cascade's embedded-art tier: an album folder
/// whose art lives INSIDE the tracks (the common case — Adele /
/// Phil Collins / most operator libraries) has no sidecar and
/// no child dirs, but every track carries the same cover in its
/// tags. The tile emitter routes to
/// `artwork?scheme=mpd-path&value=<dir>/<first-track>`,
/// re-using `artwork.local`'s existing per-track extractor
/// which already delivers the embedded picture. Stable sort so
/// repeat browses land on the same track and the browse cache
/// serves an identical URL.
///
/// Skips symlinks and hidden files. Returns `None` on I/O
/// failure so the caller cleanly falls through to the next
/// cascade tier.
pub fn first_audio_file_name_in_directory(dir: &Path) -> Option<String> {
    let read = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let ft = entry.file_type().ok()?;
            if ft.is_symlink() || !ft.is_file() {
                return None;
            }
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') {
                return None;
            }
            if !crate::is_probable_audio_file(&entry.path()) {
                return None;
            }
            Some(name)
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

/// True when `dir` has at least one non-symlink, non-hidden
/// child subdirectory at its top level. The artist-container
/// shape gate — an artist container looks like `Artist/Album1/`,
/// `Artist/Album2/` and always has child directories. A file-
/// leaf directory (`Artist/Album/track.flac` with no
/// sub-albums) has none, and correctly falls through to the
/// honest glyph rather than firing an artist-name lookup on
/// an album basename.
pub fn directory_has_child_dirs(dir: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn priority_pass_prefers_cover_over_folder() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("folder.jpg"), b"folder").unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"cover").unwrap();
        let hit = find_cover_in_directory(dir.path()).unwrap();
        assert_eq!(hit.file_name().unwrap(), "cover.jpg");
    }

    #[test]
    fn priority_pass_is_case_insensitive() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("CoVeR.JpG"), b"cover").unwrap();
        let hit = find_cover_in_directory(dir.path()).unwrap();
        assert_eq!(hit.file_name().unwrap(), "CoVeR.JpG");
    }

    #[test]
    fn fallback_pass_picks_any_image_when_no_priority_name() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Symphony No. 5.jpg"), b"art").unwrap();
        let hit = find_cover_in_directory(dir.path()).unwrap();
        assert_eq!(hit.file_name().unwrap(), "Symphony No. 5.jpg");
    }

    #[test]
    fn oversized_priority_file_is_skipped() {
        let dir = tempdir().unwrap();
        let big = vec![0u8; (MAX_COVER_BYTES as usize) + 1];
        std::fs::write(dir.path().join("cover.jpg"), &big).unwrap();
        std::fs::write(dir.path().join("folder.jpg"), b"folder").unwrap();
        let hit = find_cover_in_directory(dir.path()).unwrap();
        assert_eq!(hit.file_name().unwrap(), "folder.jpg");
    }

    #[test]
    fn no_cover_in_empty_dir() {
        let dir = tempdir().unwrap();
        assert!(find_cover_in_directory(dir.path()).is_none());
    }

    #[test]
    fn no_cover_when_missing_dir() {
        let missing = std::path::PathBuf::from("/tmp/does-not-exist-4b2c9");
        assert!(find_cover_in_directory(&missing).is_none());
    }

    #[test]
    fn child_dirs_stable_sort() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Z Album")).unwrap();
        std::fs::create_dir(dir.path().join("A Album")).unwrap();
        std::fs::create_dir(dir.path().join("M Album")).unwrap();
        std::fs::write(dir.path().join("not-a-dir.mp3"), b"").unwrap();
        let children = stable_sorted_child_dir_names(dir.path());
        assert_eq!(children, vec!["A Album", "M Album", "Z Album"]);
    }

    #[test]
    fn child_dirs_skips_hidden_and_files() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::create_dir(dir.path().join("visible")).unwrap();
        std::fs::write(dir.path().join("file.flac"), b"").unwrap();
        let children = stable_sorted_child_dir_names(dir.path());
        assert_eq!(children, vec!["visible"]);
    }

    #[test]
    fn has_child_dirs_true_when_subdir_present() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Album")).unwrap();
        assert!(directory_has_child_dirs(dir.path()));
    }

    #[test]
    fn has_child_dirs_false_when_only_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("t1.flac"), b"").unwrap();
        std::fs::write(dir.path().join("t2.flac"), b"").unwrap();
        assert!(!directory_has_child_dirs(dir.path()));
    }

    #[test]
    fn has_child_dirs_false_when_dir_missing() {
        let missing = std::path::PathBuf::from("/tmp/does-not-exist-4b2c9-x");
        assert!(!directory_has_child_dirs(&missing));
    }

    #[cfg(unix)]
    #[test]
    fn has_child_dirs_ignores_symlinked_subdir() {
        let dir = tempdir().unwrap();
        let real = tempdir().unwrap();
        std::os::unix::fs::symlink(real.path(), dir.path().join("link"))
            .unwrap();
        assert!(!directory_has_child_dirs(dir.path()));
    }

    #[test]
    fn first_audio_stable_sorted_by_name() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("02 - Second.flac"), b"").unwrap();
        std::fs::write(dir.path().join("01 - First.mp3"), b"").unwrap();
        std::fs::write(dir.path().join("03 - Third.m4a"), b"").unwrap();
        assert_eq!(
            first_audio_file_name_in_directory(dir.path()).as_deref(),
            Some("01 - First.mp3")
        );
    }

    #[test]
    fn first_audio_skips_non_audio_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"").unwrap();
        std::fs::write(dir.path().join("liner_notes.txt"), b"").unwrap();
        std::fs::write(dir.path().join("track.flac"), b"").unwrap();
        assert_eq!(
            first_audio_file_name_in_directory(dir.path()).as_deref(),
            Some("track.flac")
        );
    }

    #[test]
    fn first_audio_none_when_no_audio() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("cover.jpg"), b"").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"").unwrap();
        assert!(first_audio_file_name_in_directory(dir.path()).is_none());
    }

    #[test]
    fn first_audio_none_when_only_subdirs() {
        // Artist-container shape: children are album directories,
        // no tracks at this level. Embedded-art tier must not
        // fire — the cascade continues to the child-cover / artist-
        // name tiers instead.
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Album A")).unwrap();
        std::fs::create_dir(dir.path().join("Album B")).unwrap();
        assert!(first_audio_file_name_in_directory(dir.path()).is_none());
    }

    #[test]
    fn first_audio_none_when_missing_dir() {
        let missing = std::path::PathBuf::from("/tmp/does-not-exist-4b2c9-y");
        assert!(first_audio_file_name_in_directory(&missing).is_none());
    }

    #[test]
    fn first_audio_skips_hidden_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden.flac"), b"").unwrap();
        std::fs::write(dir.path().join("visible.flac"), b"").unwrap();
        assert_eq!(
            first_audio_file_name_in_directory(dir.path()).as_deref(),
            Some("visible.flac")
        );
    }

    #[cfg(unix)]
    #[test]
    fn first_audio_skips_symlinked_audio() {
        // Symlinks bypass the library-confinement discipline;
        // never pick a symlinked track as the representative for
        // embedded-art extraction.
        let dir = tempdir().unwrap();
        let external = tempdir().unwrap();
        let external_flac = external.path().join("outside.flac");
        std::fs::write(&external_flac, b"").unwrap();
        std::os::unix::fs::symlink(&external_flac, dir.path().join("aa.flac"))
            .unwrap();
        std::fs::write(dir.path().join("zz.flac"), b"").unwrap();
        assert_eq!(
            first_audio_file_name_in_directory(dir.path()).as_deref(),
            Some("zz.flac"),
            "the alphabetically-first REAL file wins; symlinked audio is skipped"
        );
    }
}

#[cfg(test)]
mod artist_image_tests {
    use super::*;

    fn touch(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    #[test]
    fn matches_artist_prefix_case_insensitively() {
        let d = tempfile::tempdir().unwrap();
        for n in ["artist.jpg", "Artist2.PNG", "ARTIST-live.webp"] {
            touch(d.path(), n);
        }
        let got = find_artist_images_in_directory(d.path());
        assert_eq!(got.len(), 3, "got {got:?}");
    }

    /// A cover file names a record, an artist file names a
    /// person. Mixing them is the wrong-subject failure that no
    /// image inspection can catch.
    #[test]
    fn does_not_match_cover_files() {
        let d = tempfile::tempdir().unwrap();
        for n in ["cover.jpg", "folder.png", "front.jpg", "back.jpg"] {
            touch(d.path(), n);
        }
        assert!(find_artist_images_in_directory(d.path()).is_empty());
    }

    /// Order must be stable across case and across filesystems
    /// that enumerate arbitrarily, because the first is the
    /// representative image a grid tile shows.
    #[test]
    fn orders_by_lowercased_filename() {
        let d = tempfile::tempdir().unwrap();
        for n in ["artist3.jpg", "Artist1.jpg", "ARTIST2.jpg"] {
            touch(d.path(), n);
        }
        let got: Vec<String> = find_artist_images_in_directory(d.path())
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_lowercase())
            .collect();
        assert_eq!(got, vec!["artist1.jpg", "artist2.jpg", "artist3.jpg"]);
    }

    #[test]
    fn ignores_non_image_extensions_and_oversized_files() {
        let d = tempfile::tempdir().unwrap();
        touch(d.path(), "artist.txt");
        touch(d.path(), "artist.pdf");
        std::fs::write(
            d.path().join("artist-huge.jpg"),
            vec![0u8; (MAX_COVER_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(find_artist_images_in_directory(d.path()).is_empty());
    }

    #[test]
    fn missing_directory_is_empty_not_an_error() {
        assert!(
            find_artist_images_in_directory(Path::new("/nonexistent/xyz"))
                .is_empty()
        );
    }
}
