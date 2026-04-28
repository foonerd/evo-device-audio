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

/// MPD warden: missing artist in `mpd-album` is encoded as this literal.
pub const UNKNOWN_ARTIST: &str = "unknown";

/// At most this many local audio files are read for tag match per request.
pub const MAX_MPD_ALBUM_SCAN_CANDIDATES: u32 = 100_000;

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
pub fn first_matching_audio_path(
    library_roots: &[PathBuf],
    want_artist: &str,
    want_album: &str,
) -> Result<Option<PathBuf>, MatchError> {
    let want_artist = want_artist.trim();
    let want_album = want_album.trim();
    let mut examined: u32 = 0;
    for root in library_roots {
        if let Some(p) =
            scan(root.as_path(), want_artist, want_album, &mut examined)?
        {
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
) -> Result<Option<PathBuf>, MatchError> {
    if path.is_file() {
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
    if !path.is_dir() {
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
        if p.is_dir() {
            if let Some(f) = scan(&p, want_artist, want_album, examined)? {
                return Ok(Some(f));
            }
        } else if is_probable_audio_file(&p) {
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
}
