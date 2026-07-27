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

pub mod artist_name;
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

/// At most this many directories are examined per folder-name
/// fallback pass. The fallback is invoked when the tag-walk
/// returns None (formats lofty can't parse — DSD, edge cases —
/// or files without artist/album tags); it walks library_roots
/// looking for a directory whose basename contains the
/// normalised album name, and returns the first file inside a
/// matching directory. Bounded so a pathological deep tree
/// can't stall the request.
pub const MAX_ALBUM_FOLDER_SCAN_DIRECTORIES: u32 = 20_000;

/// Find the first file inside a directory whose basename
/// (normalised: lower-case, alphanumeric-only) matches the
/// normalised `album` value. Used as the folder-name-fallback
/// step in the artwork.local mpd-album resolver's cascade —
/// invoked when the tag-walk returns None (typical for DSD
/// files where lofty can't read tags, or files without an
/// album tag but with a folder-name that names the album).
///
/// Return value: `Some(any_file_path)` inside a matching
/// folder. The caller then cascades the returned path through
/// `resolve_cover_for_audio_file` (or equivalent) which walks
/// the file's parent directory for cover.jpg / folder.jpg /
/// embedded-tag art. Returning ANY file (not just an audio
/// file) is intentional: even if the folder contains only
/// unreadable-tag DSD tracks and a folder.jpg, the sidecar
/// cascade will find the cover.
///
/// Bounded by [`MAX_ALBUM_FOLDER_SCAN_DIRECTORIES`]; returns
/// `Ok(None)` on cap without erroring so the resolver can
/// still emit a structured NotFound.
///
/// Deterministic ranking (smaller wins on every key):
///   1. **Tier**  — `0` when the normalised basename contains
///      BOTH the normalised artist AND the normalised album;
///      `1` when only the album is present. `artist == UNKNOWN_ARTIST`
///      (case-insensitive) or empty artist collapses every
///      match into tier 1 — no false artist bonus from mpd-album's
///      unknown-artist marker.
///   2. **Quality** — `0` exact-match (basename == want_album),
///      `1` starts-with, `2` ends-with, `3` substring. Tightens
///      the fit when two folders both contain the album value.
///   3. **norm_len** — shorter normalised basename wins. Given
///      equal tier+quality, `"Signature Solo"` beats
///      `"Signature Solo Vol 2"` — the extra suffix is a wider,
///      less-specific fit.
///   4. **path** — lexicographic path tie-break so the resolver
///      returns the same folder across calls even when the
///      first three keys are indistinguishable.
///
/// The scoring covers the two named-fallback risks:
///   - two folder names overlap on album substring →
///     tier + quality + length pick the tighter fit; if all
///     three are equal, path lexicography stays deterministic.
///   - artist context is carried but the caller cannot know
///     which of several album-substring folders belongs to the
///     right artist → tier-0 (artist AND album) always beats
///     tier-1 (album only) regardless of quality/length.
pub fn first_file_in_album_named_folder(
    library_roots: &[PathBuf],
    artist: &str,
    album: &str,
) -> Result<Option<PathBuf>, MatchError> {
    let want_album = normalise_for_folder_match(album);
    if want_album.is_empty() {
        return Ok(None);
    }
    // Artist tier only applies when the caller supplied a real
    // artist. mpd-album's warden encodes missing artist as the
    // literal `UNKNOWN_ARTIST`; treat that (and case variants)
    // as artist-absent so the tier system doesn't spuriously
    // reward a folder just because its normalised basename
    // happens to contain the substring `unknown`.
    let want_artist_norm = normalise_for_folder_match(artist);
    let want_artist: Option<&str> = if want_artist_norm.is_empty()
        || artist.eq_ignore_ascii_case(UNKNOWN_ARTIST)
    {
        None
    } else {
        Some(want_artist_norm.as_str())
    };

    let mut examined: u32 = 0;
    let mut visited: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    let mut candidates: Vec<FolderCandidate> = Vec::new();

    for root in library_roots {
        collect_matching_folders(
            root,
            want_artist,
            &want_album,
            &mut examined,
            &mut visited,
            &mut candidates,
        )?;
    }

    candidates.sort_by(|a, b| {
        (a.tier, a.quality, a.norm_len, &a.path)
            .cmp(&(b.tier, b.quality, b.norm_len, &b.path))
    });

    for c in &candidates {
        if let Some(f) = first_file_in_directory(&c.path) {
            return Ok(Some(f));
        }
    }
    Ok(None)
}

struct FolderCandidate {
    tier: u8,
    quality: u8,
    norm_len: usize,
    path: PathBuf,
}

/// Normalise a folder / album name for the folder-name match.
/// Lower-cases and strips everything that isn't an ASCII
/// alphanumeric so `"[DSD64] Fiona Joy - Signature Solo"` and
/// `"Signature - Solo"` (album value from mpd-album) both
/// collapse to something matchable via substring. Non-ASCII
/// characters (`Sigur Rós`, `Ágætis byrjun`) survive by
/// lower-casing + keeping code-points; the substring rule
/// works on the raw char sequence.
fn normalise_for_folder_match(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn collect_matching_folders(
    dir: &Path,
    want_artist: Option<&str>,
    want_album: &str,
    examined: &mut u32,
    visited: &mut std::collections::HashSet<PathBuf>,
    candidates: &mut Vec<FolderCandidate>,
) -> Result<(), MatchError> {
    if *examined >= MAX_ALBUM_FOLDER_SCAN_DIRECTORIES {
        return Ok(());
    }
    let canonical = match dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };
    if !visited.insert(canonical) {
        return Ok(());
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(e) => return Err(MatchError::Io(e.to_string())),
    };
    let mut entries: Vec<PathBuf> =
        read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in &entries {
        *examined += 1;
        if *examined >= MAX_ALBUM_FOLDER_SCAN_DIRECTORIES {
            return Ok(());
        }
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            // Skip symlinks — matches first_matching_audio_path's
            // symlink discipline (never follow, never chase).
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let normalised = normalise_for_folder_match(name);
        if !normalised.is_empty() {
            if let Some(cand) =
                score_folder(path, &normalised, want_artist, want_album)
            {
                candidates.push(cand);
            }
        }
        collect_matching_folders(
            path,
            want_artist,
            want_album,
            examined,
            visited,
            candidates,
        )?;
    }
    Ok(())
}

fn score_folder(
    path: &Path,
    normalised: &str,
    want_artist: Option<&str>,
    want_album: &str,
) -> Option<FolderCandidate> {
    if !normalised.contains(want_album) {
        return None;
    }
    let quality: u8 = if normalised == want_album {
        0
    } else if normalised.starts_with(want_album) {
        1
    } else if normalised.ends_with(want_album) {
        2
    } else {
        3
    };
    let tier: u8 = match want_artist {
        Some(a) if normalised.contains(a) => 0,
        _ => 1,
    };
    Some(FolderCandidate {
        tier,
        quality,
        norm_len: normalised.chars().count(),
        path: path.to_path_buf(),
    })
}

fn first_file_in_directory(dir: &Path) -> Option<PathBuf> {
    let read = std::fs::read_dir(dir).ok()?;
    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files.into_iter().next()
}

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

/// Cover-identity-picking `artwork_target_url` for track rows.
///
/// Picks the resolve scheme based on tag availability so list
/// surfaces (library, queue, favourites, playlists) can collapse
/// N tracks in one album to ONE resolve key at the framework's
/// artwork endpoint:
///
/// - When both `artist` and `album` are present and non-empty
///   (after trim), returns an `mpd-album` URL with the
///   canonical `"{artist}|{album}"` value. Every track in the
///   same album emits the same URL — the framework's resolve
///   coalescer collapses concurrent same-key requests to one
///   upstream dispatch, and the UI can rely on identical URLs
///   sharing browser + service-worker cache entries.
/// - Otherwise falls back to the per-track `mpd-path` URL
///   using the file path. Tracks without album context (loose
///   files, sidecar-only libraries, custom covers per track)
///   keep the per-track fidelity path.
///
/// Empty / whitespace-only artist or album is treated as
/// absent — an `Artist|<empty>` mpd-album would not resolve at
/// the provider, so the fallback to `mpd-path` is honest.
///
/// The now-playing subject emits its own URL directly via
/// `artwork_target_url("mpd-path", ...)` — the hero surface
/// keeps per-track identity regardless of tag state; this
/// helper is for list surfaces where the album is the operator-
/// visible identity anyway.
pub fn artwork_target_url_for_track(
    file_path: &str,
    artist: Option<&str>,
    album: Option<&str>,
) -> String {
    let artist_ok = artist
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let album_ok = album
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    match (artist_ok, album_ok) {
        (Some(a), Some(al)) => {
            let value = format!("{a}|{al}");
            artwork_target_url("mpd-album", &value)
        }
        _ => artwork_target_url("mpd-path", file_path),
    }
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

    #[test]
    fn for_track_with_full_tags_uses_mpd_album_key() {
        // Both artist and album present → mpd-album URL. N
        // tracks in the same album will emit the same URL and
        // collapse to one resolve key at the framework
        // endpoint.
        let a = artwork_target_url_for_track(
            "Beatles/Revolver/01.flac",
            Some("The Beatles"),
            Some("Revolver"),
        );
        let b = artwork_target_url_for_track(
            "Beatles/Revolver/02.flac",
            Some("The Beatles"),
            Some("Revolver"),
        );
        let c = artwork_target_url_for_track(
            "Beatles/Revolver/03.flac",
            Some("The Beatles"),
            Some("Revolver"),
        );
        assert_eq!(a, b, "same album must emit same URL for track 1 vs 2");
        assert_eq!(a, c, "same album must emit same URL for track 1 vs 3");
        assert!(a.contains("scheme=mpd-album"));
        assert!(a.contains("The%20Beatles%7CRevolver"));
    }

    #[test]
    fn for_track_missing_artist_falls_back_to_mpd_path() {
        let url = artwork_target_url_for_track(
            "Loose/01.flac",
            None,
            Some("Revolver"),
        );
        assert!(url.contains("scheme=mpd-path"));
        assert!(url.contains("Loose%2F01.flac"));
    }

    #[test]
    fn for_track_missing_album_falls_back_to_mpd_path() {
        let url = artwork_target_url_for_track(
            "Beatles/Loose/01.flac",
            Some("The Beatles"),
            None,
        );
        assert!(url.contains("scheme=mpd-path"));
        assert!(url.contains("Beatles%2FLoose%2F01.flac"));
    }

    #[test]
    fn for_track_empty_or_whitespace_tags_fall_back_to_mpd_path() {
        // Whitespace-only artist / album are treated as absent
        // — an `Artist|<empty>` mpd-album would not resolve at
        // the provider, so per-track path is the honest choice.
        let url = artwork_target_url_for_track(
            "unknown/01.flac",
            Some("   "),
            Some(""),
        );
        assert!(url.contains("scheme=mpd-path"));
    }

    #[test]
    fn for_track_different_albums_by_same_artist_emit_different_urls() {
        let revolver = artwork_target_url_for_track(
            "Beatles/Revolver/01.flac",
            Some("The Beatles"),
            Some("Revolver"),
        );
        let abbey = artwork_target_url_for_track(
            "Beatles/Abbey Road/01.flac",
            Some("The Beatles"),
            Some("Abbey Road"),
        );
        assert_ne!(
            revolver, abbey,
            "different album names by the same artist must emit \
             distinct URLs so the resolver can pick the right cover"
        );
    }
}

const AUDIO_EXTS: &[&str] = &[
    // Lossy / compressed containers.
    "mp3", "aac", "m4a", "mp4", "m4b", "ogg", "oga", "opus", "wma", "webm",
    "3gp", "aax", "mka", // Lossless PCM containers.
    "flac", "wav", "aif", "aiff", "wv", "ape", "mpc",
    // DSD stream containers — DSF (Sony) + DFF (Philips DSD-IFF).
    // Missing pre-2026-07-22 meant the mpd-album tag-walk (and
    // any other AUDIO_EXTS-gated scan) filtered DSD files out
    // before lofty saw them; DSD albums returned 404 from the
    // mpd-album path even when their folder carried folder.jpg
    // or embedded art. Adding them here lets the scanner enter
    // the file; the artwork.local cascade (tag match → folder
    // sidecar → embedded → online) picks up from there.
    "dsf", "dff",
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
    // Folder-name-fallback tests (mpd-album cascade step 2).
    // These pin the "album by folder name" invariant that
    // covers formats lofty can't tag-parse (DSF/DFF today) or
    // libraries with missing artist/album tags.
    // ---------------------------------------------------------

    #[test]
    fn folder_by_album_name_finds_normalised_substring_match() {
        let dir = tempfile::tempdir().unwrap();
        // Folder name contains the album name plus extra text
        // (case + special chars + brackets), and the folder
        // contains a DSF file lofty can't tag-parse plus a
        // folder.jpg the cascade would use.
        let sub = dir.path().join("[DSD64] Fiona Joy - Signature Solo");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("01. Ceremony.dsf"), b"dsf-fake-bytes")
            .unwrap();
        std::fs::write(sub.join("folder.jpg"), b"fake-jpg-bytes").unwrap();

        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            "Fiona Joy",
            "Signature - Solo",
        )
        .unwrap();
        assert!(
            found.is_some(),
            "folder-name match MUST find the album folder even when its \
             basename has bracketed prefix + case + dashes"
        );
    }

    #[test]
    fn folder_by_album_name_returns_none_when_no_folder_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(
            dir.path().join("Other Artist").join("Other Album"),
        )
        .unwrap();
        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            "Fiona Joy",
            "Signature - Solo",
        )
        .unwrap();
        assert_eq!(
            found, None,
            "no folder whose basename contains 'signaturesolo' → None, \
             not a false positive"
        );
    }

    #[test]
    fn folder_by_album_name_handles_empty_album() {
        let dir = tempfile::tempdir().unwrap();
        // Empty album value must not match every folder — the
        // fallback should refuse to consider a search with no
        // signal.
        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            "Fiona Joy",
            "",
        )
        .unwrap();
        assert_eq!(found, None);
    }

    #[test]
    fn folder_by_album_name_prefers_deterministic_first_file() {
        // Ensure a folder with multiple files returns the same
        // one across calls (sorted order).
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("The Album");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("z_last.txt"), b"z").unwrap();
        std::fs::write(sub.join("a_first.txt"), b"a").unwrap();
        std::fs::write(sub.join("m_middle.txt"), b"m").unwrap();
        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(sub.join("a_first.txt")),
            "sorted-first file wins so subsequent calls return the \
             same path (deterministic)"
        );
    }

    #[test]
    fn folder_by_album_name_normalisation_handles_unicode() {
        let dir = tempfile::tempdir().unwrap();
        // Unicode-cased folder + album value.
        let sub = dir.path().join("Sigur Rós - Ágætis byrjun");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("cover.jpg"), b"jpg").unwrap();
        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            "Sigur Rós",
            "ágætis byrjun",
        )
        .unwrap();
        assert!(
            found.is_some(),
            "unicode folder + unicode album value must match via lowercase-normalise"
        );
    }

    // ---------------------------------------------------------
    // Determinism-hardening regression tests. These pin the
    // scoring contract that keeps the folder-name fallback
    // from serving a wrong-album cover when two folder names
    // overlap on the album substring. Ranking (smaller wins):
    //   1. tier: (artist AND album) < (album only)
    //   2. quality: exact < starts-with < ends-with < substring
    //   3. normalised basename length: shorter wins
    //   4. path lexicography: final tie-break
    // ---------------------------------------------------------

    #[test]
    fn folder_ranking_prefers_artist_and_album_over_album_only() {
        // Two folders both contain the normalised album name.
        // Folder A carries the artist too (tier 0); Folder B is
        // just the album (tier 1). Tier 0 wins even when Folder
        // B has strictly better quality — the whole point of
        // the tier system.
        let dir = tempfile::tempdir().unwrap();
        let with_artist = dir.path().join("[DSD64] Fiona Joy - Signature Solo");
        std::fs::create_dir_all(&with_artist).unwrap();
        std::fs::write(with_artist.join("hit.jpg"), b"hit").unwrap();
        let album_only = dir.path().join("Signature - Solo");
        std::fs::create_dir_all(&album_only).unwrap();
        std::fs::write(album_only.join("miss.jpg"), b"miss").unwrap();

        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            "Fiona Joy",
            "Signature - Solo",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(with_artist.join("hit.jpg")),
            "artist+album tier must beat album-only tier even when the \
             album-only folder is an exact-quality match"
        );
    }

    #[test]
    fn folder_ranking_prefers_exact_match_over_substring_when_no_artist() {
        // Artist is UNKNOWN → every candidate collapses to
        // tier 1. Within tier 1, exact-quality (basename ==
        // normalised album) beats substring quality.
        let dir = tempfile::tempdir().unwrap();
        let exact = dir.path().join("The Album");
        std::fs::create_dir_all(&exact).unwrap();
        std::fs::write(exact.join("hit.jpg"), b"hit").unwrap();
        let extended = dir.path().join("The Album Remastered");
        std::fs::create_dir_all(&extended).unwrap();
        std::fs::write(extended.join("miss.jpg"), b"miss").unwrap();

        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(exact.join("hit.jpg")),
            "exact-quality match must beat starts-with quality within \
             the same tier"
        );
    }

    #[test]
    fn folder_ranking_prefers_shorter_basename_within_same_quality() {
        // Two folders both starts-with the album value and
        // have no artist context. Tighter fit (shorter
        // normalised basename) wins so the wider-scoped
        // folder cannot capture the request.
        let dir = tempfile::tempdir().unwrap();
        let tight = dir.path().join("Signature Solo Live");
        std::fs::create_dir_all(&tight).unwrap();
        std::fs::write(tight.join("hit.jpg"), b"hit").unwrap();
        let wide = dir.path().join("Signature Solo Compilation Anthology");
        std::fs::create_dir_all(&wide).unwrap();
        std::fs::write(wide.join("miss.jpg"), b"miss").unwrap();

        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "Signature Solo",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(tight.join("hit.jpg")),
            "shorter normalised basename wins within the same tier + \
             quality — the tighter fit is the more-specific match"
        );
    }

    #[test]
    fn folder_ranking_alphabetic_tie_break_is_deterministic() {
        // Same tier, same quality (exact-match), same
        // normalised length. Path lexicography must produce
        // a stable result — otherwise the resolver would
        // return different covers across calls.
        let dir = tempfile::tempdir().unwrap();
        // Both folders normalise to "thealbum".
        let ab = dir.path().join("The Album (AA)");
        std::fs::create_dir_all(&ab).unwrap();
        std::fs::write(ab.join("aa.jpg"), b"aa").unwrap();
        let ba = dir.path().join("The Album (BB)");
        std::fs::create_dir_all(&ba).unwrap();
        std::fs::write(ba.join("bb.jpg"), b"bb").unwrap();

        // Both call twice — same result both times.
        let a = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album AA",
        )
        .unwrap();
        let b = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album AA",
        )
        .unwrap();
        assert_eq!(
            a, b,
            "identical inputs must return identical output across calls"
        );
        // And when both would be tier 1 quality 3 (album =
        // "The Album" is starts-with for both after
        // normalisation), the lexicographically-earlier path
        // wins.
        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(ab.join("aa.jpg")),
            "lexicographic path tie-break: `The Album (AA)` sorts \
             before `The Album (BB)`"
        );
    }

    #[test]
    fn folder_ranking_unknown_artist_marker_does_not_reward_folder_named_unknown(
    ) {
        // Regression: `artist == UNKNOWN_ARTIST` must NOT be
        // treated as a real artist substring — otherwise a
        // folder called `Unknown - The Album` would win over
        // `The Album` on tier alone despite carrying no
        // artist context.
        let dir = tempfile::tempdir().unwrap();
        let unknown_named = dir.path().join("Unknown - The Album");
        std::fs::create_dir_all(&unknown_named).unwrap();
        std::fs::write(unknown_named.join("miss.jpg"), b"miss").unwrap();
        let clean = dir.path().join("The Album");
        std::fs::create_dir_all(&clean).unwrap();
        std::fs::write(clean.join("hit.jpg"), b"hit").unwrap();

        let found = first_file_in_album_named_folder(
            &[dir.path().to_path_buf()],
            UNKNOWN_ARTIST,
            "The Album",
        )
        .unwrap();
        assert_eq!(
            found,
            Some(clean.join("hit.jpg")),
            "UNKNOWN_ARTIST must NOT be scored as a real artist substring; \
             tier-collapse means the exact-quality match wins on quality"
        );
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
