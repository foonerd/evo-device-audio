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
//! 3. Artist-name portrait — for container folders shaped like an
//!    artist's discography (child subdirectories present) the
//!    emitter routes to the framework's `artist-name` scheme so
//!    `artwork.online`'s artist cascade delivers a portrait.
//!
//! `find_cover_in_directory` powers steps 1 and 2 verbatim.
//! `directory_has_child_dirs` powers the step-3 shape test.

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

/// True when `dir` has at least one non-symlink, non-hidden
/// child subdirectory at its top level. The Tier 3 shape
/// gate — an artist container looks like `Artist/Album1/`,
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
}
