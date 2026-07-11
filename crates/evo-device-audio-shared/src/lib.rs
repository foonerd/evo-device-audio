// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Helpers for **local** library path resolution: walk `[library] roots` and
//! find an audio file whose **tags** match a `mpd-album` value from
//! `org.evoframework.playback.mpd` (compound `"{artist}|{album}"` / empty artist →
//! [`UNKNOWN_ARTIST`]). Used by the artwork and metadata local respondents
//! when they must resolve the album subject to a on-disk track without talking
//! to MPD.
//!
//! Scan is deterministic (UTF-8 name sort in each directory) and bounded: at
//! most [`MAX_MPD_ALBUM_SCAN_CANDIDATES`] `read_from_path` + tag reads per
//! call.

use std::io;
use std::path::{Path, PathBuf};

use lofty::file::TaggedFileExt;
use lofty::read_from_path;
use lofty::tag::Accessor;

pub mod audio_ui_pack;
pub mod terminus_loopback;
pub mod transcode;
pub mod transition_envelope;

/// Default path to MPD's main configuration file on Linux. Used as
/// the source-of-truth for `music_directory` + `playlist_directory`
/// auto-derivation by every plugin in the audio reference
/// distribution that needs to resolve MPD-relative file paths
/// (artwork.local, metadata.local, future siblings). Operator
/// override remains available via plugin-specific TOML config; the
/// auto-derived value is the primary truth path, the operator value
/// is the additive override.
pub const DEFAULT_MPD_CONF_PATH: &str = "/etc/mpd.conf";

/// MPD warden: missing artist in `mpd-album` is encoded as this literal.
pub const UNKNOWN_ARTIST: &str = "unknown";

/// At most this many local audio files are read for tag match per request.
pub const MAX_MPD_ALBUM_SCAN_CANDIDATES: u32 = 100_000;

/// Read `/etc/mpd.conf` (or alternate path) and return the parsed
/// `music_directory` value. `None` when the file cannot be read,
/// when the directive is absent, or when the value is empty.
///
/// Single source of truth for every plugin in the audio reference
/// distribution that needs to resolve MPD-relative file paths.
/// Eliminates per-plugin config drift — operator can't get the
/// path wrong because there's nothing to configure; the value
/// comes from MPD's own canonical config.
pub fn load_music_directory_from_mpd_conf(conf_path: &Path) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(conf_path).ok()?;
    parse_mpd_directive(&contents, "music_directory")
}

/// Read `/etc/mpd.conf` (or alternate path) and return the parsed
/// `playlist_directory` value. Same shape as
/// [`load_music_directory_from_mpd_conf`]; consumed by the playlist
/// shelf's `create_playlist` verb to materialise empty `.m3u`
/// files.
pub fn load_playlist_directory_from_mpd_conf(
    conf_path: &Path,
) -> Option<PathBuf> {
    let contents = std::fs::read_to_string(conf_path).ok()?;
    parse_mpd_directive(&contents, "playlist_directory")
}

/// Pure single-line MPD directive parser. Tolerates quoted /
/// unquoted / equals-style syntax; comment lines (`#`); first
/// non-comment hit wins.
///
/// Lifted from the playback plugin's `source_probe` module so
/// every plugin in the audio distribution consumes one
/// production-validated parser rather than rolling its own.
pub fn parse_mpd_directive(contents: &str, directive: &str) -> Option<PathBuf> {
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rest = match line.strip_prefix(directive) {
            Some(r) => r,
            None => continue,
        };
        // The directive name is followed by whitespace, then the
        // value (quoted or unquoted). Strip leading whitespace +
        // leading equals (some MPD configs use key=value style).
        let rest = rest.trim_start();
        let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        let value = if let Some(stripped) = rest.strip_prefix('"') {
            // Quoted: take up to next quote.
            let end = stripped.find('"')?;
            &stripped[..end]
        } else {
            // Unquoted: take first whitespace-delimited token.
            rest.split_whitespace().next()?
        };
        if value.is_empty() {
            continue;
        }
        return Some(PathBuf::from(value));
    }
    None
}

#[cfg(test)]
mod mpd_conf_tests {
    use super::*;

    #[test]
    fn parses_quoted_value() {
        let contents = r#"music_directory "/var/lib/evo/music""#;
        assert_eq!(
            parse_mpd_directive(contents, "music_directory"),
            Some(PathBuf::from("/var/lib/evo/music"))
        );
    }

    #[test]
    fn parses_unquoted_value() {
        let contents = "music_directory /var/lib/evo/music";
        assert_eq!(
            parse_mpd_directive(contents, "music_directory"),
            Some(PathBuf::from("/var/lib/evo/music"))
        );
    }

    #[test]
    fn parses_equals_style() {
        let contents = "music_directory=/var/lib/evo/music";
        assert_eq!(
            parse_mpd_directive(contents, "music_directory"),
            Some(PathBuf::from("/var/lib/evo/music"))
        );
    }

    #[test]
    fn skips_commented_directive() {
        let contents = r#"
# music_directory "/wrong"
music_directory "/var/lib/evo/music"
"#;
        assert_eq!(
            parse_mpd_directive(contents, "music_directory"),
            Some(PathBuf::from("/var/lib/evo/music"))
        );
    }

    #[test]
    fn returns_none_when_directive_absent() {
        let contents = "playlist_directory \"/var/lib/mpd/playlists\"";
        assert_eq!(parse_mpd_directive(contents, "music_directory"), None);
    }

    #[test]
    fn returns_none_when_value_empty() {
        let contents = "music_directory \"\"";
        assert_eq!(parse_mpd_directive(contents, "music_directory"), None);
    }

    #[test]
    fn first_non_comment_hit_wins() {
        let contents = r#"
music_directory "/first"
music_directory "/second"
"#;
        assert_eq!(
            parse_mpd_directive(contents, "music_directory"),
            Some(PathBuf::from("/first"))
        );
    }
}

/// Canonical relative URL for an artwork target on the framework's
/// HTTPS surface. Constructs the URL the operator UI consumes from
/// the now-playing / queue / favourites / playlist / library
/// envelopes (the playback warden emits one per track-bearing
/// item). The framework's `GET /api/v1/audio/artwork` endpoint
/// resolves the target via the artwork-providers shelf and
/// 302-redirects to the content-hash endpoint that serves the
/// bytes from the asset cache.
///
/// `scheme` is the external-addressing scheme (`mpd-path` or
/// `mpd-album` per the playback warden); `value` is the scheme-
/// specific opaque value (MPD's `file` path or compound
/// `"{artist}|{album}"` respectively). The UI appends `&size=…`
/// to the returned URL when it needs a sub-original variant —
/// the framework endpoint passes the size through to the
/// resolver, which transcodes + caches the variant under a
/// distinct content hash.
///
/// The returned URL is **relative** so it composes with whichever
/// origin the UI is loaded from (same-origin in the typical
/// deployment, but no assumption baked in). Value is percent-
/// encoded per RFC 3986 query-component rules (alphanumerics +
/// `-_.~` unreserved; everything else `%XX`).
pub fn artwork_target_url(scheme: &str, value: &str) -> String {
    format!(
        "/api/v1/audio/artwork?scheme={}&value={}",
        percent_encode_query_value(scheme),
        percent_encode_query_value(value),
    )
}

/// Percent-encode a string for use as a URL query-component
/// value. Conservative: encodes everything except the
/// RFC 3986 "unreserved" set (alphanumerics + `-`, `_`, `.`,
/// `~`). This keeps the artwork URL safe across every MPD path
/// content (spaces, brackets, `&`, `=`, `?`, `#`, Unicode
/// filenames, etc.) without pulling a URL crate.
fn percent_encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~' => out.push(*b as char),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{:02X}", b);
            }
        }
    }
    out
}

#[cfg(test)]
mod artwork_url_tests {
    use super::*;

    #[test]
    fn ascii_filename_stays_readable() {
        let url = artwork_target_url("mpd-path", "Artist/Album/Track.flac");
        assert_eq!(
            url,
            "/api/v1/audio/artwork?scheme=mpd-path&value=Artist%2FAlbum%2FTrack.flac"
        );
    }

    #[test]
    fn spaces_and_special_chars_encoded() {
        let url = artwork_target_url(
            "mpd-path",
            "The Beatles/Abbey Road/01 - Come Together.flac",
        );
        // Spaces → %20, slashes → %2F, hyphens preserved
        assert!(url.contains("%20"));
        assert!(url.contains("%2F"));
        assert!(url.contains("Beatles"));
        assert!(!url.contains(" ")); // no raw spaces
    }

    #[test]
    fn ampersand_encoded_so_query_does_not_break() {
        // An MPD path containing `&` would corrupt the query
        // shape without encoding. The framework's query parser
        // would split on the raw `&`.
        let url = artwork_target_url("mpd-path", "AC&DC/Back In Black.flac");
        assert!(url.contains("AC%26DC"));
        assert!(!url.contains("AC&DC/"));
    }

    #[test]
    fn unicode_filename_encoded_as_utf8_bytes() {
        // Sigur Rós's `Ágætis byrjun` — UTF-8 multi-byte
        // characters per code-point.
        let url =
            artwork_target_url("mpd-path", "Sigur Rós/Ágætis byrjun/01.flac");
        assert!(url.contains("%C3%81")); // Á
        assert!(url.contains("%C3%B3")); // ó
    }

    #[test]
    fn mpd_album_compound_value_encodes_pipe_separator() {
        // mpd-album values are `Artist|Album`; the pipe is not
        // unreserved so it must be percent-encoded.
        let url = artwork_target_url("mpd-album", "Beatles|Revolver");
        assert!(url.contains("%7C")); // |
    }
}

const AUDIO_EXTS: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "m4b", "aac", "ogg", "oga", "opus", "wma",
    "wav", "aif", "aiff", "wv", "ape", "mpc", "mka", "webm", "3gp", "aax",
];

/// `mpd-album` value could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// No `|`, or empty `album` component after split.
    InvalidFormat,
}

/// Scan failed or was truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchError {
    /// Stopped after [`MAX_MPD_ALBUM_SCAN_CANDIDATES`] file reads.
    LimitExceeded,
    /// Underlying I/O (walk).
    Io(String),
}

/// Parse `value` to match `org.evoframework.playback.mpd` / album subjects:
/// `splitn(2, '|')` → left = artist (empty or whitespace → [`UNKNOWN_ARTIST`]),
/// right = album (required, non-empty after trim).
pub fn parse_mpd_album_value(
    value: &str,
) -> Result<(String, String), ParseError> {
    let v = value.trim();
    let mut it = v.splitn(2, '|');
    let first = it.next().ok_or(ParseError::InvalidFormat)?;
    let second = it.next().ok_or(ParseError::InvalidFormat)?;
    let album = second.trim();
    if album.is_empty() {
        return Err(ParseError::InvalidFormat);
    }
    let artist = first.trim();
    let artist = if artist.is_empty() {
        UNKNOWN_ARTIST.to_string()
    } else {
        artist.to_string()
    };
    Ok((artist, album.to_string()))
}

/// Whether `path` is treated as a local audio file candidate (by extension).
pub fn is_probable_audio_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        let b = e.as_bytes();
        AUDIO_EXTS
            .iter()
            .any(|ext| ext.as_bytes().eq_ignore_ascii_case(b))
    })
}

fn file_tag_matches(
    file_artist: Option<std::borrow::Cow<'_, str>>,
    file_album: Option<std::borrow::Cow<'_, str>>,
    want_artist: &str,
    want_album: &str,
) -> bool {
    let a = file_artist
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| UNKNOWN_ARTIST.to_string());
    let Some(alb) = file_album
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    a == want_artist && alb == want_album
}

/// First path under `library_roots` (sequential) whose tags match, or `None`.
/// Search order: each root, depth-first, directory entries sorted by name;
/// first matching file in that order wins. Skips hidden *directories* (name
/// starts with `.`); file names may still start with `.`.
///
/// Symlink discipline:
///
/// - Directory traversal uses `symlink_metadata` so symlinks themselves
///   are not followed by the file-type check. Symlinked directories are
///   refused entry — the scanner descends only into real directories.
///   This prevents the classic `<root>/a -> <root>` cycle from producing
///   infinite recursion and the more subtle `<root>/share -> /` from
///   leaking the scan outside the library tree.
/// - Symlinked files within a real directory are evaluated (the symlink
///   target is read by lofty's tag reader), but the recursion does not
///   walk into them.
/// - A canonicalised-path visited-set is the belt-and-braces guard:
///   even if a future maintainer adds symlink-following, the same
///   real directory is never entered twice in one scan.
pub fn first_matching_audio_path(
    library_roots: &[PathBuf],
    want_artist: &str,
    want_album: &str,
) -> Result<Option<PathBuf>, MatchError> {
    let want_artist = want_artist.trim();
    let want_album = want_album.trim();
    let mut examined: u32 = 0;
    let mut visited: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    for root in library_roots {
        if let Some(p) = scan(
            root.as_path(),
            want_artist,
            want_album,
            &mut examined,
            &mut visited,
        )? {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

fn scan(
    path: &Path,
    want_artist: &str,
    want_album: &str,
    examined: &mut u32,
    visited: &mut std::collections::HashSet<PathBuf>,
) -> Result<Option<PathBuf>, MatchError> {
    // `symlink_metadata` does NOT follow symlinks. If `path`
    // is a symlink, its file_type reports `is_symlink() = true`
    // and is neither a file nor a directory by this metadata's
    // accounting.
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        // Skip outright — both symlinked files and symlinked
        // directories. Files reached only via symlink are
        // not considered library content.
        return Ok(None);
    }
    if ft.is_file() {
        if !is_probable_audio_file(path) {
            return Ok(None);
        }
        if *examined >= MAX_MPD_ALBUM_SCAN_CANDIDATES {
            return Err(MatchError::LimitExceeded);
        }
        *examined = examined.saturating_add(1);
        if audio_file_matches(path, want_artist, want_album) {
            return Ok(Some(path.to_path_buf()));
        }
        return Ok(None);
    }
    if !ft.is_dir() {
        return Ok(None);
    }
    // Visited-set guard: identify the directory by its
    // canonical path so even hard-link-driven cycles or a
    // future symlink-following change cannot re-enter.
    let canonical =
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(None);
    }
    let mut names = read_dir_names(path).map_err(|e| {
        MatchError::Io(format!("read_dir {}: {e}", path.display()))
    })?;
    names.sort();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let p = path.join(&name);
        let entry_meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let entry_ft = entry_meta.file_type();
        if entry_ft.is_symlink() {
            // Same discipline as the top of the function: skip
            // symlinks entirely. The audio-library tree is
            // expected to be a real directory tree.
            continue;
        }
        if entry_ft.is_dir() {
            if let Some(f) =
                scan(&p, want_artist, want_album, examined, visited)?
            {
                return Ok(Some(f));
            }
        } else if entry_ft.is_file() && is_probable_audio_file(&p) {
            if *examined >= MAX_MPD_ALBUM_SCAN_CANDIDATES {
                return Err(MatchError::LimitExceeded);
            }
            *examined = examined.saturating_add(1);
            if audio_file_matches(&p, want_artist, want_album) {
                return Ok(Some(p));
            }
        }
    }
    Ok(None)
}

fn read_dir_names(path: &Path) -> io::Result<Vec<String>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(path)? {
        if let Some(name) = e?.file_name().to_str() {
            out.push(name.to_string());
        }
    }
    Ok(out)
}

fn audio_file_matches(
    path: &Path,
    want_artist: &str,
    want_album: &str,
) -> bool {
    let tagged = match read_from_path(path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        return file_tag_matches(
            tag.artist(),
            tag.album(),
            want_artist,
            want_album,
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    use lofty::config::WriteOptions;
    use lofty::tag::Accessor;
    use lofty::tag::Tag;
    use lofty::tag::TagExt;
    use lofty::tag::TagType;
    use std::borrow::Cow;

    #[test]
    fn parse_mpd_album_splits_first_pipe_only() {
        assert_eq!(
            parse_mpd_album_value(r"a|b|c").unwrap(),
            (r"a".to_string(), r"b|c".to_string())
        );
        assert_eq!(
            parse_mpd_album_value("  unknown  |  Hits  ").unwrap(),
            ("unknown".to_string(), "Hits".to_string())
        );
        assert_eq!(parse_mpd_album_value("|Solo").unwrap().0, UNKNOWN_ARTIST);
        assert!(parse_mpd_album_value("nope").is_err());
        assert!(parse_mpd_album_value("x|").is_err());
    }

    #[test]
    fn file_tag_match_rules() {
        assert!(file_tag_matches(
            None,
            Some(Cow::Borrowed("A")),
            UNKNOWN_ARTIST,
            "A"
        ));
        assert!(file_tag_matches(
            Some(Cow::Borrowed("B")),
            Some(Cow::Borrowed("A")),
            "B",
            "A"
        ));
    }

    #[test]
    fn end_to_end_finds_in_tree() {
        // Valid MPEG bytes (tiny ffmpeg-generated file; see `assets/minimal.mp3` in the crate).
        const MINI_MP3: &[u8] = include_bytes!("../assets/minimal.mp3");
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Bandname").join("TheAlbum");
        std::fs::create_dir_all(&sub).unwrap();
        let mp3 = sub.join("1.mp3");
        std::fs::write(&mp3, MINI_MP3).unwrap();
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_artist("Bandname".to_string());
        tag.set_album("TheAlbum".to_string());
        tag.save_to_path(&mp3, WriteOptions::new().preferred_padding(0))
            .expect("tag save");

        let (a, al) = parse_mpd_album_value("Bandname|TheAlbum").unwrap();
        let found =
            first_matching_audio_path(&[dir.path().to_path_buf()], &a, &al)
                .unwrap();
        assert_eq!(found, Some(mp3));
    }

    // ---------------------------------------------------------
    // Symlink + cycle guard contract tests for first_matching_audio_path.
    // Document the discipline that the scanner refuses to follow
    // symlinks and detects cycles in the directory graph.
    // ---------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn scan_terminates_on_self_referencing_symlink_cycle() {
        // Build a library with one directory `loop` that
        // contains a symlink back to itself: `loop/back -> .`.
        // Without the symlink + visited guards, scan() recurses
        // into `loop/back/back/back/...` until the OS path
        // limit is hit. With the guards, scan terminates
        // cleanly with `Ok(None)`.
        let dir = tempfile::tempdir().expect("tempdir");
        let looper = dir.path().join("loop");
        std::fs::create_dir_all(&looper).expect("create loop dir");
        let backlink = looper.join("back");
        std::os::unix::fs::symlink(&looper, &backlink)
            .expect("create self-referencing symlink");

        // No audio in this library; result is None. The
        // assertion is that the call returns at all — without
        // the guards this scan would never terminate.
        let result = first_matching_audio_path(
            &[dir.path().to_path_buf()],
            "any",
            "any",
        );
        assert!(
            matches!(result, Ok(None)),
            "scan must terminate cleanly on symlink cycle; got {result:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_does_not_descend_into_symlinked_directories() {
        // Build a library with one real directory `real/`
        // containing a no-match audio file, plus a symlinked
        // directory `mirror -> real`. Even when `mirror` exists
        // as a directory by metadata-following semantics, the
        // scanner must NOT descend into it (the visited-set
        // would catch the duplicate but the symlink-skip
        // discipline takes effect first).
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).expect("create real dir");

        // Plant a dummy non-audio file so scan walks the
        // directory but finds no tagged matches.
        std::fs::write(real.join("placeholder.txt"), b"not audio")
            .expect("write placeholder");
        let mirror = dir.path().join("mirror");
        std::os::unix::fs::symlink(&real, &mirror)
            .expect("create mirror symlink");

        // Result is Ok(None) — no matching audio anywhere. The
        // important contract is that the call returns. Without
        // the symlink discipline, future maintainers who relax
        // file-type checks could re-introduce infinite recursion;
        // this test gates against that regression.
        let result = first_matching_audio_path(
            &[dir.path().to_path_buf()],
            "any",
            "any",
        );
        assert!(matches!(result, Ok(None)));
    }

    #[cfg(unix)]
    #[test]
    fn scan_ignores_symlinked_audio_files() {
        // A symlinked audio file (target outside the library
        // tree) is reachable by name from inside the library,
        // but the scanner must not evaluate it — symlinks
        // bypass the library-confinement intent.
        let dir = tempfile::tempdir().expect("tempdir");
        let outside_audio = dir.path().join("outside.mp3");
        std::fs::write(&outside_audio, b"not really audio")
            .expect("write outside audio");

        let library = dir.path().join("library");
        std::fs::create_dir_all(&library).expect("create library");
        let symlinked = library.join("ghost.mp3");
        std::os::unix::fs::symlink(&outside_audio, &symlinked)
            .expect("create symlinked audio");

        // No real audio under the library; symlinked audio
        // is not evaluated. Result is Ok(None).
        let result = first_matching_audio_path(
            std::slice::from_ref(&library),
            "any",
            "any",
        );
        assert!(matches!(result, Ok(None)));
    }
}
