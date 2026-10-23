// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Import a playlist file: `.m3u`, `.pls` or `.cue`.
//!
//! The operator has a playlist file — exported from another
//! player, downloaded, or a cue sheet for a rip — and wants it as
//! a stored playlist here. The file is read, each of its rows is
//! resolved to something MPD can hold, and the rows are written
//! into a stored playlist through MPD's own `playlistadd`, which
//! is the only way this plugin writes a playlist.
//!
//! The three parsers are pure: text in, rows out. They open no
//! file and read no clock, so every row they produce can be
//! asserted from a string.
//!
//! A `.cue` is read, never written. A cue sheet beside its audio
//! file is something MPD already understands, and dropping one
//! there would change the operator's library tree under them.
//! Nothing in this module writes to the music directory; the only
//! filesystem read is the uploaded file, confined to the uploads
//! directory. The cue's tracks are returned in the response so
//! the glass can show what was read; the stored playlist holds the
//! audio file each track belongs to, once per file, because a
//! stored playlist cannot carry a time range.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::mpd::MpdConnection;
use crate::playlist::{
    publish_index, validate_playlist_name, PlaylistContext, VerbError,
    PLAYLIST_PAYLOAD_VERSION,
};

const PLUGIN_NAME: &str = "org.evoframework.playback.mpd";

/// Where uploaded files land on the device. Created by the
/// installer, world-writable, outside the music tree.
pub(crate) const DEFAULT_UPLOADS_DIR: &str = "/var/lib/evo/uploads";

/// The largest playlist file the verb will read. A playlist is
/// text; one megabyte is tens of thousands of rows.
pub(crate) const MAX_IMPORT_BYTES: u64 = 1 << 20;

// ----- payload and report -----

/// The three formats, by name or by the file's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ImportFormat {
    M3u,
    Pls,
    Cue,
}

impl ImportFormat {
    /// From a file name's extension: `.m3u` and `.m3u8` are M3U.
    pub(crate) fn from_file_name(name: &str) -> Option<Self> {
        let ext = name.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "m3u" | "m3u8" => Some(Self::M3u),
            "pls" => Some(Self::Pls),
            "cue" => Some(Self::Cue),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::M3u => "m3u",
            Self::Pls => "pls",
            Self::Cue => "cue",
        }
    }
}

/// `playlist.import_file` request payload.
///
/// Exactly one of `path` and `content` is given: the file as
/// uploaded to the device, named relative to the uploads
/// directory, or the file's text sent inline by a glass that read
/// it itself. `format` may be omitted when `path` carries a known
/// extension.
#[derive(Debug, Deserialize)]
pub(crate) struct ImportFilePayload {
    pub(crate) v: u32,
    /// The stored playlist to create.
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) format: Option<ImportFormat>,
    /// The uploaded file, relative to the uploads directory.
    #[serde(default)]
    pub(crate) path: Option<String>,
    /// The file's text, inline.
    #[serde(default)]
    pub(crate) content: Option<String>,
    /// An MPD-relative directory that relative rows resolve
    /// against — the folder the playlist was written next to.
    #[serde(default)]
    pub(crate) base: Option<String>,
    /// Replace a playlist of the same name instead of refusing.
    #[serde(default)]
    pub(crate) overwrite: bool,
}

/// One row the import did not write, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SkippedRow {
    pub(crate) entry: String,
    pub(crate) reason: String,
}

/// One track of a cue sheet, as read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CueTrack {
    pub(crate) number: u32,
    pub(crate) title: Option<String>,
    pub(crate) performer: Option<String>,
    /// The `FILE` the track is in, as written in the sheet.
    pub(crate) file: String,
    /// `INDEX 01`, in milliseconds from the start of the file.
    pub(crate) start_ms: u64,
}

/// What `playlist.import_file` answers.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportReport {
    pub(crate) v: u32,
    /// `ok` when every row went in; `partial` when some did not.
    pub(crate) status: &'static str,
    pub(crate) name: String,
    pub(crate) format: ImportFormat,
    /// Rows the file held.
    pub(crate) parsed: usize,
    /// Rows now in the playlist.
    pub(crate) imported: usize,
    pub(crate) skipped: Vec<SkippedRow>,
    /// A cue sheet's tracks; empty for the other formats.
    pub(crate) tracks: Vec<CueTrack>,
}

// ----- errors -----

#[derive(Debug, thiserror::Error)]
pub(crate) enum ImportError {
    #[error(transparent)]
    Verb(#[from] VerbError),
    #[error("playlist.import_file: give exactly one of 'path' and 'content'")]
    OneSource,
    #[error("playlist.import_file: 'format' is needed: {reason}")]
    FormatUnknown { reason: String },
    #[error("playlist.import_file: upload {path:?} refused: {reason}")]
    UploadRefused { path: String, reason: String },
    #[error("playlist.import_file: {format} does not parse: {reason}")]
    Unparseable {
        format: &'static str,
        reason: String,
    },
    #[error("playlist.import_file: nothing to import: {reason}")]
    Nothing { reason: String },
}

// ----- the parsers -----

/// One row of a parsed playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedEntry {
    /// The row as written: a path, a URL.
    pub(crate) location: String,
    pub(crate) title: Option<String>,
}

/// What a parser hands back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ParsedPlaylist {
    pub(crate) entries: Vec<ParsedEntry>,
    /// Cue sheets only: the tracks, in sheet order.
    pub(crate) tracks: Vec<CueTrack>,
}

/// Strip a UTF-8 byte-order mark, which editors on one platform
/// put at the front of every text file.
fn without_bom(text: &str) -> &str {
    text.strip_prefix('\u{FEFF}').unwrap_or(text)
}

/// M3U / M3U8: one location per line. `#EXTINF:<secs>,<title>`
/// names the row that follows it; every other `#` line is a
/// comment. Blank lines are nothing.
pub(crate) fn parse_m3u(text: &str) -> ParsedPlaylist {
    let mut out = ParsedPlaylist::default();
    let mut pending_title: Option<String> = None;
    for raw in without_bom(text).lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            // `<secs>,<title>`; the title is everything after the
            // first comma, and may be empty.
            pending_title = rest
                .split_once(',')
                .map(|(_, t)| t.trim())
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        out.entries.push(ParsedEntry {
            location: line.to_string(),
            title: pending_title.take(),
        });
    }
    out
}

/// PLS: an INI file with a `[playlist]` section, `FileN=`,
/// `TitleN=`, `LengthN=` keys. Rows come out in `N` order whatever
/// order the keys were written in.
pub(crate) fn parse_pls(text: &str) -> Result<ParsedPlaylist, ImportError> {
    let text = without_bom(text);
    let mut in_section = false;
    let mut seen_header = false;
    let mut rows: std::collections::BTreeMap<u32, ParsedEntry> =
        std::collections::BTreeMap::new();
    let mut titles: std::collections::BTreeMap<u32, String> =
        std::collections::BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[') {
            let name = section.trim_end_matches(']').trim();
            in_section = name.eq_ignore_ascii_case("playlist");
            seen_header |= in_section;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let lower = key.to_ascii_lowercase();
        if let Some(n) = lower.strip_prefix("file") {
            if let Ok(n) = n.parse::<u32>() {
                if !value.is_empty() {
                    rows.insert(
                        n,
                        ParsedEntry {
                            location: value.to_string(),
                            title: None,
                        },
                    );
                }
            }
        } else if let Some(n) = lower.strip_prefix("title") {
            if let Ok(n) = n.parse::<u32>() {
                if !value.is_empty() {
                    titles.insert(n, value.to_string());
                }
            }
        }
    }
    if !seen_header {
        return Err(ImportError::Unparseable {
            format: "pls",
            reason: "no [playlist] section".to_string(),
        });
    }
    let entries = rows
        .into_iter()
        .map(|(n, mut e)| {
            e.title = titles.remove(&n);
            e
        })
        .collect();
    Ok(ParsedPlaylist {
        entries,
        tracks: Vec::new(),
    })
}

/// The first quoted argument on a cue line, or the bare rest.
fn cue_arg(rest: &str) -> String {
    let rest = rest.trim();
    if let Some(inner) = rest.strip_prefix('"') {
        inner.split('"').next().unwrap_or_default().to_string()
    } else {
        rest.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

/// `mm:ss:ff` at 75 frames a second, to milliseconds.
fn cue_time_ms(s: &str) -> Option<u64> {
    let mut parts = s.trim().split(':');
    let mm: u64 = parts.next()?.parse().ok()?;
    let ss: u64 = parts.next()?.parse().ok()?;
    let ff: u64 = parts.next()?.parse().ok()?;
    if ss >= 60 || ff >= 75 {
        return None;
    }
    Some((mm * 60 + ss) * 1000 + ff * 1000 / 75)
}

/// CUE: `FILE "<name>" <type>` opens a file; `TRACK <n> AUDIO`
/// opens a track in it; `TITLE`, `PERFORMER` name whichever is
/// open; `INDEX 01 mm:ss:ff` is where the track starts. One
/// playlist row per distinct `FILE`, in order; the tracks are
/// returned beside them.
pub(crate) fn parse_cue(text: &str) -> Result<ParsedPlaylist, ImportError> {
    let text = without_bom(text);
    let mut files: Vec<String> = Vec::new();
    let mut tracks: Vec<CueTrack> = Vec::new();
    let mut current_file: Option<String> = None;
    // The track being described, and whether `INDEX 01` has
    // been seen for it.
    let mut open: Option<(CueTrack, bool)> = None;

    let close = |open: &mut Option<(CueTrack, bool)>,
                 tracks: &mut Vec<CueTrack>| {
        if let Some((t, _)) = open.take() {
            tracks.push(t);
        }
    };

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (word, rest) =
            line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match word.to_ascii_uppercase().as_str() {
            "FILE" => {
                close(&mut open, &mut tracks);
                let name = cue_arg(rest);
                if name.is_empty() {
                    return Err(ImportError::Unparseable {
                        format: "cue",
                        reason: "FILE without a name".to_string(),
                    });
                }
                if !files.contains(&name) {
                    files.push(name.clone());
                }
                current_file = Some(name);
            }
            "TRACK" => {
                close(&mut open, &mut tracks);
                let Some(file) = current_file.clone() else {
                    return Err(ImportError::Unparseable {
                        format: "cue",
                        reason: "TRACK before any FILE".to_string(),
                    });
                };
                let number = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
                    .unwrap_or(tracks.len() as u32 + 1);
                open = Some((
                    CueTrack {
                        number,
                        title: None,
                        performer: None,
                        file,
                        start_ms: 0,
                    },
                    false,
                ));
            }
            "TITLE" => {
                if let Some((t, _)) = open.as_mut() {
                    let v = cue_arg(rest);
                    t.title = (!v.is_empty()).then_some(v);
                }
            }
            "PERFORMER" => {
                if let Some((t, _)) = open.as_mut() {
                    let v = cue_arg(rest);
                    t.performer = (!v.is_empty()).then_some(v);
                }
            }
            "INDEX" => {
                if let Some((t, have_01)) = open.as_mut() {
                    let mut parts = rest.split_whitespace();
                    let which = parts.next().unwrap_or_default();
                    let time = parts.next().unwrap_or_default();
                    // INDEX 01 is the start; INDEX 00 is pregap.
                    // Take 01 when it comes; fall back to the
                    // first index seen if 01 never does.
                    if which == "01" || which == "1" || !*have_01 {
                        if let Some(ms) = cue_time_ms(time) {
                            t.start_ms = ms;
                            *have_01 = which == "01" || which == "1";
                        }
                    }
                }
            }
            // REM, CATALOG, FLAGS, ISRC, PREGAP, POSTGAP, SONGWRITER
            // and anything else: not what the playlist needs.
            _ => {}
        }
    }
    close(&mut open, &mut tracks);
    if files.is_empty() {
        return Err(ImportError::Unparseable {
            format: "cue",
            reason: "no FILE line".to_string(),
        });
    }
    Ok(ParsedPlaylist {
        entries: files
            .into_iter()
            .map(|f| ParsedEntry {
                location: f,
                title: None,
            })
            .collect(),
        tracks,
    })
}

/// Parse by format.
pub(crate) fn parse(
    format: ImportFormat,
    text: &str,
) -> Result<ParsedPlaylist, ImportError> {
    match format {
        ImportFormat::M3u => Ok(parse_m3u(text)),
        ImportFormat::Pls => parse_pls(text),
        ImportFormat::Cue => parse_cue(text),
    }
}

// ----- resolving rows to what MPD can hold -----

/// Turn a row as written into a URI MPD's stored playlist can
/// hold, or say why it cannot.
///
/// - `http://` / `https://` stay as they are: streams.
/// - `dlna:` identities stay: they are what the playlist shelf
///   stores for network containers.
/// - `file://` is a path.
/// - An absolute path must sit under `music_directory`; it
///   becomes MPD-relative. One outside the library is skipped —
///   MPD cannot reach it and this plugin must not guess.
/// - A relative path is MPD-relative, under `base` when given.
///   `..` is refused: a playlist does not get to climb.
/// - Backslashes are separators, which is how a Windows-written
///   `.m3u` arrives.
pub(crate) fn resolve_row(
    location: &str,
    music_directory: &Path,
    base: Option<&str>,
) -> Result<String, String> {
    let raw = location.trim();
    if raw.is_empty() {
        return Err("empty row".to_string());
    }
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(raw.to_string());
    }
    if lower.starts_with("dlna:") {
        return Ok(raw.to_string());
    }
    let path_text = match raw.strip_prefix("file://") {
        Some(p) => p.to_string(),
        None => {
            if let Some((scheme, _)) = raw.split_once("://") {
                return Err(format!("unsupported scheme {scheme:?}"));
            }
            raw.to_string()
        }
    };
    let path_text = path_text.replace('\\', "/");
    let path = Path::new(&path_text);

    let relative: PathBuf = if path.is_absolute() {
        match path.strip_prefix(music_directory) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => {
                return Err(format!(
                    "outside the library at {}",
                    music_directory.display()
                ))
            }
        }
    } else {
        let mut rel = PathBuf::new();
        if let Some(b) = base.map(str::trim).filter(|b| !b.is_empty()) {
            rel.push(b.trim_matches('/'));
        }
        rel.push(path);
        rel
    };

    let mut clean = PathBuf::new();
    for c in relative.components() {
        match c {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err("'..' is not allowed".to_string())
            }
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    let s = clean.to_string_lossy().into_owned();
    if s.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(s)
}

// ----- the upload -----

/// Read an uploaded file by its name relative to the uploads
/// directory. Absolute names and `..` are refused: the verb reads
/// uploads and nothing else on the device. Text that is not UTF-8
/// is read lossily — an old `.m3u` in a Windows code page still
/// imports, with its titles mangled rather than refused.
pub(crate) async fn read_upload(
    uploads_dir: &Path,
    rel: &str,
) -> Result<String, ImportError> {
    let refused = |reason: &str| ImportError::UploadRefused {
        path: rel.to_string(),
        reason: reason.to_string(),
    };
    let p = Path::new(rel);
    if rel.trim().is_empty() {
        return Err(refused("empty path"));
    }
    // Plain names only: no root, no drive prefix, no `..`. One
    // check, so there is one thing to prove.
    if p.components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(refused(
            "must be a plain name inside the uploads directory",
        ));
    }
    let full = uploads_dir.join(p);
    let meta = tokio::fs::metadata(&full)
        .await
        .map_err(|e| refused(&format!("{e}")))?;
    if !meta.is_file() {
        return Err(refused("not a file"));
    }
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(refused(&format!(
            "{} bytes is over the {} byte limit",
            meta.len(),
            MAX_IMPORT_BYTES
        )));
    }
    let bytes = tokio::fs::read(&full)
        .await
        .map_err(|e| refused(&format!("{e}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ----- the verb -----

/// `playlist.import_file` — read, resolve, write through MPD.
///
/// Refusals before MPD is touched: a bad name, the favourites
/// playlist, both or neither of `path` / `content`, an unknown
/// format, an unreadable upload, text that does not parse, and a
/// file none of whose rows resolve. The playlist is created by
/// MPD's `playlistadd` row by row, so a row MPD refuses — a path
/// not in its database — is skipped and reported while the rest
/// go in. A playlist of the same name is refused unless
/// `overwrite` is set, in which case it is emptied first.
pub(crate) async fn handle_import_file(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    uploads_dir: &Path,
    payload: ImportFilePayload,
) -> Result<ImportReport, ImportError> {
    if payload.v != PLAYLIST_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: "playlist.import_file".to_string(),
            got: payload.v,
            expected: PLAYLIST_PAYLOAD_VERSION,
        }
        .into());
    }
    validate_playlist_name(&payload.name)?;
    if payload.name == ctx.favourites_name {
        return Err(VerbError::FavouritesProtected {
            name: payload.name.clone(),
        }
        .into());
    }

    // The text, from exactly one place.
    let (text, format) = match (payload.path.as_deref(), payload.content) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(ImportError::OneSource)
        }
        (Some(path), None) => {
            let format = payload
                .format
                .or_else(|| ImportFormat::from_file_name(path))
                .ok_or_else(|| ImportError::FormatUnknown {
                    reason: format!(
                        "{path:?} has no .m3u, .m3u8, .pls or .cue extension"
                    ),
                })?;
            (read_upload(uploads_dir, path).await?, format)
        }
        (None, Some(content)) => {
            let format =
                payload.format.ok_or_else(|| ImportError::FormatUnknown {
                    reason: "inline content carries no file name".to_string(),
                })?;
            (content, format)
        }
    };

    let parsed = parse(format, &text)?;
    if parsed.entries.is_empty() {
        return Err(ImportError::Nothing {
            reason: "the file holds no rows".to_string(),
        });
    }

    // Resolve every row before touching MPD, so a file that
    // cannot be imported leaves nothing behind.
    let mut uris: Vec<String> = Vec::new();
    let mut skipped: Vec<SkippedRow> = Vec::new();
    for entry in &parsed.entries {
        match resolve_row(
            &entry.location,
            &ctx.music_directory,
            payload.base.as_deref(),
        ) {
            Ok(uri) => uris.push(uri),
            Err(reason) => skipped.push(SkippedRow {
                entry: entry.location.clone(),
                reason,
            }),
        }
    }
    if uris.is_empty() {
        return Err(ImportError::Nothing {
            reason: format!(
                "none of the {} rows resolve to something MPD can hold; \
                 first: {:?} ({})",
                parsed.entries.len(),
                skipped[0].entry,
                skipped[0].reason
            ),
        });
    }

    let mpd = |what: &str, e: crate::mpd::MpdError| VerbError::Mpd {
        verb: "import_file".to_string(),
        reason: format!("{what}: {e}"),
    };
    let existing = conn
        .listplaylists()
        .await
        .map_err(|e| mpd("listplaylists", e))?;
    if existing.iter().any(|s| s.name == payload.name) {
        if !payload.overwrite {
            return Err(VerbError::DuplicateName {
                name: payload.name.clone(),
            }
            .into());
        }
        conn.playlistclear(&payload.name)
            .await
            .map_err(|e| mpd("playlistclear", e))?;
    }

    // Row by row, not a command list: a list stops at the first
    // refusal, and one dead row must not keep the rest out.
    let mut imported = 0usize;
    for uri in &uris {
        match conn.playlistadd(&payload.name, uri).await {
            Ok(()) => imported += 1,
            Err(e) => {
                tracing::warn!(
                    plugin = PLUGIN_NAME,
                    name = %payload.name,
                    uri = %uri,
                    error = %e,
                    "import_file: MPD refused a row; skipped"
                );
                skipped.push(SkippedRow {
                    entry: uri.clone(),
                    reason: format!("MPD refused it: {e}"),
                });
            }
        }
    }
    publish_index(ctx, conn).await;
    if imported == 0 {
        return Err(ImportError::Nothing {
            reason: format!(
                "MPD refused every one of the {} rows that resolved",
                uris.len()
            ),
        });
    }
    tracing::info!(
        plugin = PLUGIN_NAME,
        name = %payload.name,
        format = format.as_str(),
        parsed = parsed.entries.len(),
        imported,
        skipped = skipped.len(),
        "playlist imported"
    );
    Ok(ImportReport {
        v: PLAYLIST_PAYLOAD_VERSION,
        status: if skipped.is_empty() { "ok" } else { "partial" },
        name: payload.name,
        format,
        parsed: parsed.entries.len(),
        imported,
        skipped,
        tracks: parsed.tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playlist::DEFAULT_FAVOURITES_PLAYLIST_NAME;
    use crate::source_registry::SourceRegistry;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    // ===== the three parsers =====

    /// M3U: titles ride on the row after their `#EXTINF`,
    /// comments and blank lines are nothing, a BOM and CRLF are
    /// tolerated, and a URL is a row like any other.
    #[test]
    fn m3u_rows_carry_their_titles() {
        let text = "\u{FEFF}#EXTM3U\r\n\
                    #EXTINF:240,Low - Words\r\n\
                    Low/Trust/01 Words.flac\r\n\
                    \r\n\
                    # a comment\r\n\
                    #EXTINF:-1,\r\n\
                    http://stream.example/live\r\n\
                    bare/row.mp3\r\n";
        let p = parse_m3u(text);
        assert_eq!(
            p.entries,
            vec![
                ParsedEntry {
                    location: "Low/Trust/01 Words.flac".into(),
                    title: Some("Low - Words".into()),
                },
                ParsedEntry {
                    location: "http://stream.example/live".into(),
                    title: None,
                },
                ParsedEntry {
                    location: "bare/row.mp3".into(),
                    title: None,
                },
            ]
        );
        assert!(p.tracks.is_empty());
        assert!(parse_m3u("#EXTM3U\n\n# nothing\n").entries.is_empty());
    }

    /// PLS: rows come out in `N` order whatever order the keys
    /// were written, titles attach by `N`, and a file without the
    /// `[playlist]` section is not a PLS.
    #[test]
    fn pls_rows_come_out_in_number_order() {
        let text = "[playlist]\n\
                    NumberOfEntries=3\n\
                    File2=second.flac\n\
                    Title2=Second\n\
                    File1=http://a.example/one\n\
                    File3=third.flac\n\
                    Length3=120\n\
                    Version=2\n";
        let p = parse_pls(text).unwrap();
        assert_eq!(
            p.entries
                .iter()
                .map(|e| e.location.as_str())
                .collect::<Vec<_>>(),
            vec!["http://a.example/one", "second.flac", "third.flac"]
        );
        assert_eq!(p.entries[1].title.as_deref(), Some("Second"));
        assert_eq!(p.entries[0].title, None);

        let no_header = parse_pls("File1=x.flac\n");
        assert!(
            matches!(
                no_header,
                Err(ImportError::Unparseable { format: "pls", .. })
            ),
            "{no_header:?}"
        );
        assert!(parse_pls("[playlist]\n").unwrap().entries.is_empty());
    }

    /// CUE: one row per distinct FILE in order, and every TRACK
    /// read with its title, performer, file and `INDEX 01` start.
    #[test]
    fn cue_files_become_rows_and_tracks_are_read() {
        let text = "REM GENRE Rock\r\n\
                    PERFORMER \"Low\"\r\n\
                    TITLE \"Trust\"\r\n\
                    FILE \"Trust.flac\" WAVE\r\n\
                    \x20 TRACK 01 AUDIO\r\n\
                    \x20   TITLE \"(That's How You Sing) Amazing Grace\"\r\n\
                    \x20   PERFORMER \"Low\"\r\n\
                    \x20   INDEX 01 00:00:00\r\n\
                    \x20 TRACK 02 AUDIO\r\n\
                    \x20   TITLE \"Canada\"\r\n\
                    \x20   INDEX 00 02:30:00\r\n\
                    \x20   INDEX 01 02:30:15\r\n\
                    FILE \"Bonus.flac\" WAVE\r\n\
                    \x20 TRACK 03 AUDIO\r\n\
                    \x20   TITLE \"Extra\"\r\n\
                    \x20   INDEX 01 00:10:00\r\n";
        let p = parse_cue(text).unwrap();
        assert_eq!(
            p.entries
                .iter()
                .map(|e| e.location.as_str())
                .collect::<Vec<_>>(),
            vec!["Trust.flac", "Bonus.flac"],
            "one row per FILE, in order"
        );
        assert_eq!(p.tracks.len(), 3);
        assert_eq!(
            p.tracks[0],
            CueTrack {
                number: 1,
                title: Some("(That's How You Sing) Amazing Grace".into()),
                performer: Some("Low".into()),
                file: "Trust.flac".into(),
                start_ms: 0,
            }
        );
        // 02:30:15 at 75 frames a second is 150 s + 200 ms.
        assert_eq!(p.tracks[1].start_ms, 150_200);
        assert_eq!(p.tracks[1].title.as_deref(), Some("Canada"));
        assert_eq!(p.tracks[1].performer, None, "no track performer");
        assert_eq!(p.tracks[2].file, "Bonus.flac");
        assert_eq!(p.tracks[2].start_ms, 10_000);

        let no_file = parse_cue("TITLE \"x\"\nTRACK 01 AUDIO\n");
        assert!(matches!(
            no_file,
            Err(ImportError::Unparseable { format: "cue", .. })
        ));
    }

    /// A cue that names the same FILE twice still yields one row:
    /// a stored playlist holds the audio file once.
    #[test]
    fn a_cue_names_each_file_once() {
        let text = "FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
                    FILE \"b.flac\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n\
                    FILE \"a.flac\" WAVE\nTRACK 03 AUDIO\nINDEX 01 01:00:00\n";
        let p = parse_cue(text).unwrap();
        assert_eq!(
            p.entries
                .iter()
                .map(|e| e.location.clone())
                .collect::<Vec<_>>(),
            vec!["a.flac", "b.flac"]
        );
        assert_eq!(p.tracks.len(), 3);
        assert_eq!(p.tracks[2].file, "a.flac");
    }

    /// `ImportFormat` from a file name.
    #[test]
    fn the_format_is_read_from_the_extension() {
        assert_eq!(
            ImportFormat::from_file_name("x.M3U"),
            Some(ImportFormat::M3u)
        );
        assert_eq!(
            ImportFormat::from_file_name("x.m3u8"),
            Some(ImportFormat::M3u)
        );
        assert_eq!(
            ImportFormat::from_file_name("a/b.pls"),
            Some(ImportFormat::Pls)
        );
        assert_eq!(
            ImportFormat::from_file_name("rip.cue"),
            Some(ImportFormat::Cue)
        );
        assert_eq!(ImportFormat::from_file_name("notes.txt"), None);
        assert_eq!(ImportFormat::from_file_name("noext"), None);
    }

    // ===== resolving rows =====

    /// Absolute paths under the library become MPD-relative;
    /// outside it they are skipped; `..` is refused; streams and
    /// stored identities pass; backslashes are separators; a base
    /// applies to relative rows.
    #[test]
    fn rows_resolve_to_what_mpd_can_hold() {
        let music = Path::new("/var/lib/evo/music");
        let ok = |row: &str, base: Option<&str>| {
            resolve_row(row, music, base).unwrap()
        };
        let no = |row: &str, base: Option<&str>| {
            resolve_row(row, music, base).unwrap_err()
        };
        assert_eq!(
            ok("/var/lib/evo/music/INTERNAL/a.flac", None),
            "INTERNAL/a.flac"
        );
        assert_eq!(
            ok("file:///var/lib/evo/music/USB/b.flac", None),
            "USB/b.flac"
        );
        assert!(no("/home/andrew/c.flac", None).contains("outside the library"));
        assert!(no("../../etc/passwd", None).contains(".."));
        assert!(no("INTERNAL/../../x.flac", None).contains(".."));
        assert_eq!(ok("http://a.example/s", None), "http://a.example/s");
        assert_eq!(ok("HTTPS://a.example/s", None), "HTTPS://a.example/s");
        assert_eq!(ok("dlna:sid/obj", None), "dlna:sid/obj");
        assert!(no("ftp://a.example/x", None).contains("unsupported scheme"));
        assert_eq!(ok("Low\\Trust\\01.flac", None), "Low/Trust/01.flac");
        assert_eq!(
            ok("./01.flac", Some("INTERNAL/Low/Trust/")),
            "INTERNAL/Low/Trust/01.flac"
        );
        assert_eq!(ok("01.flac", Some("")), "01.flac");
        assert!(no("   ", None).contains("empty"));
    }

    // ===== the upload =====

    /// Only a file inside the uploads directory is read.
    #[tokio::test]
    async fn the_upload_is_confined_to_the_uploads_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("list.m3u"), "a.flac\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(
            read_upload(dir.path(), "list.m3u").await.unwrap(),
            "a.flac\n"
        );
        for bad in ["../list.m3u", "/etc/passwd", "", "sub", "missing.m3u"] {
            assert!(
                matches!(
                    read_upload(dir.path(), bad).await,
                    Err(ImportError::UploadRefused { .. })
                ),
                "{bad:?} must be refused"
            );
        }
        // Not UTF-8 is still read, lossily, rather than refused.
        std::fs::write(dir.path().join("latin.m3u"), b"caf\xe9.mp3\n").unwrap();
        let text = read_upload(dir.path(), "latin.m3u").await.unwrap();
        assert!(text.ends_with(".mp3\n"), "{text:?}");
        // Over the limit is refused.
        let big = vec![b'x'; (MAX_IMPORT_BYTES + 1) as usize];
        std::fs::write(dir.path().join("big.m3u"), big).unwrap();
        assert!(matches!(
            read_upload(dir.path(), "big.m3u").await,
            Err(ImportError::UploadRefused { .. })
        ));
    }

    // ===== the verb, against the stored-playlists mock =====

    type HeldPlaylists = Arc<std::sync::Mutex<BTreeMap<String, Vec<String>>>>;

    struct Harness {
        ctx: PlaylistContext,
        conn: MpdConnection,
        playlists: HeldPlaylists,
        commands: Arc<std::sync::Mutex<Vec<String>>>,
        music: tempfile::TempDir,
        uploads: tempfile::TempDir,
    }

    async fn harness() -> Harness {
        use crate::playback_supervisor::test_mock::{
            short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
            ConnBehaviour,
        };
        let playlists: HeldPlaylists =
            Arc::new(std::sync::Mutex::new(BTreeMap::from([(
                DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
                vec!["INTERNAL/loved.flac".to_string()],
            )])));
        let commands = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (endpoint, _mock) =
            spawn_mock_mpd(vec![ConnBehaviour::StoredPlaylists {
                commands: Arc::clone(&commands),
                playlists: Arc::clone(&playlists),
                library: Vec::new(),
                queue: Vec::new(),
            }])
            .await;
        let conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let music = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(music.path().join("INTERNAL/Low/Trust"))
            .unwrap();
        std::fs::write(
            music.path().join("INTERNAL/Low/Trust/Trust.flac"),
            b"x",
        )
        .unwrap();
        let uploads = tempfile::tempdir().unwrap();
        let ctx = PlaylistContext::new(
            music.path().to_path_buf(),
            SourceRegistry::new(),
            Arc::new(CapturingSubjectAnnouncer::default()),
            DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
            None,
        );
        Harness {
            ctx,
            conn,
            playlists,
            commands,
            music,
            uploads,
        }
    }

    fn payload(name: &str) -> ImportFilePayload {
        ImportFilePayload {
            v: 1,
            name: name.to_string(),
            format: None,
            path: None,
            content: None,
            base: None,
            overwrite: false,
        }
    }

    /// Every path under a directory, sorted: the shape of a tree
    /// so two readings can be compared.
    fn tree(root: &Path) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if p.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    /// An inline M3U becomes a stored playlist with its rows in
    /// order, written through MPD, and the report says what went
    /// in and what did not.
    #[tokio::test]
    async fn an_m3u_is_imported_in_order_through_mpd() {
        let mut h = harness().await;
        let mut p = payload("From Elsewhere");
        p.format = Some(ImportFormat::M3u);
        p.content = Some(format!(
            "#EXTM3U\n\
             #EXTINF:1,one\n{}/INTERNAL/Low/Trust/Trust.flac\n\
             http://stream.example/live\n\
             /home/someone/else.flac\n\
             INTERNAL/relative.flac\n",
            h.music.path().display()
        ));
        let report =
            handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p)
                .await
                .unwrap();
        assert_eq!(report.status, "partial");
        assert_eq!(report.parsed, 4);
        assert_eq!(report.imported, 3);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].entry, "/home/someone/else.flac");
        assert_eq!(report.format, ImportFormat::M3u);
        assert!(report.tracks.is_empty());
        let held = h.playlists.lock().unwrap();
        assert_eq!(
            held.get("From Elsewhere").unwrap(),
            &vec![
                "INTERNAL/Low/Trust/Trust.flac".to_string(),
                "http://stream.example/live".to_string(),
                "INTERNAL/relative.flac".to_string(),
            ]
        );
        let seen = h.commands.lock().unwrap();
        assert_eq!(
            seen.iter().filter(|c| c.starts_with("playlistadd")).count(),
            3,
            "written through MPD, row by row: {seen:?}"
        );
    }

    /// A PLS from the uploads directory, format read from its
    /// name, with a base for its relative rows.
    #[tokio::test]
    async fn a_pls_upload_is_imported_with_its_base() {
        let mut h = harness().await;
        std::fs::write(
            h.uploads.path().join("mix.pls"),
            "[playlist]\nFile1=Trust.flac\nFile2=http://r.example/x\nNumberOfEntries=2\n",
        )
        .unwrap();
        let mut p = payload("Mix");
        p.path = Some("mix.pls".to_string());
        p.base = Some("INTERNAL/Low/Trust".to_string());
        let report =
            handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p)
                .await
                .unwrap();
        assert_eq!(report.status, "ok");
        assert_eq!(report.format, ImportFormat::Pls);
        assert_eq!(
            h.playlists.lock().unwrap().get("Mix").unwrap(),
            &vec![
                "INTERNAL/Low/Trust/Trust.flac".to_string(),
                "http://r.example/x".to_string()
            ]
        );
    }

    /// A cue sheet is read — its tracks come back in the report —
    /// and the stored playlist holds the audio file. Nothing is
    /// written beside the music: the library tree is the same
    /// after as before, and no `.cue` exists under it.
    #[tokio::test]
    async fn a_cue_is_read_and_never_written_beside_the_music() {
        let mut h = harness().await;
        let before = tree(h.music.path());
        std::fs::write(
            h.uploads.path().join("Trust.cue"),
            "PERFORMER \"Low\"\nTITLE \"Trust\"\nFILE \"Trust.flac\" WAVE\n\
             TRACK 01 AUDIO\nTITLE \"Amazing Grace\"\nINDEX 01 00:00:00\n\
             TRACK 02 AUDIO\nTITLE \"Canada\"\nINDEX 01 04:12:30\n",
        )
        .unwrap();
        let mut p = payload("Trust");
        p.path = Some("Trust.cue".to_string());
        p.base = Some("INTERNAL/Low/Trust".to_string());
        let report =
            handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p)
                .await
                .unwrap();
        assert_eq!(report.format, ImportFormat::Cue);
        assert_eq!(report.imported, 1);
        assert_eq!(report.tracks.len(), 2, "the sheet was read");
        assert_eq!(report.tracks[1].title.as_deref(), Some("Canada"));
        assert_eq!(report.tracks[1].start_ms, 252_400);
        assert_eq!(
            h.playlists.lock().unwrap().get("Trust").unwrap(),
            &vec!["INTERNAL/Low/Trust/Trust.flac".to_string()]
        );
        assert_eq!(
            tree(h.music.path()),
            before,
            "the library tree must be exactly as it was"
        );
        assert!(
            !tree(h.music.path()).iter().any(|p| p.ends_with(".cue")),
            "no .cue beside the music"
        );
        // The upload itself is still where it was: read, not moved.
        assert!(h.uploads.path().join("Trust.cue").is_file());
    }

    /// The refusals that happen before MPD is touched: both or
    /// neither source, an unknown format, a climbing upload path,
    /// the favourites name, a bad name, and a file none of whose
    /// rows resolve. None of them sends MPD a write.
    #[tokio::test]
    async fn refusals_leave_mpd_untouched() {
        let mut h = harness().await;
        let cases: Vec<(&str, ImportFilePayload)> = vec![
            ("neither", payload("A")),
            ("both", {
                let mut p = payload("A");
                p.path = Some("x.m3u".into());
                p.content = Some("x\n".into());
                p
            }),
            ("no format", {
                let mut p = payload("A");
                p.content = Some("x\n".into());
                p
            }),
            ("climbing path", {
                let mut p = payload("A");
                p.path = Some("../x.m3u".into());
                p
            }),
            ("favourites", {
                // With overwrite, so that only the favourites
                // guard stands between this and emptying the
                // operator's favourites.
                let mut p = payload(DEFAULT_FAVOURITES_PLAYLIST_NAME);
                p.format = Some(ImportFormat::M3u);
                p.content = Some("INTERNAL/a.flac\n".into());
                p.overwrite = true;
                p
            }),
            ("bad name", {
                let mut p = payload("a/b");
                p.format = Some(ImportFormat::M3u);
                p.content = Some("INTERNAL/a.flac\n".into());
                p
            }),
            ("nothing resolves", {
                let mut p = payload("A");
                p.format = Some(ImportFormat::M3u);
                p.content = Some("/elsewhere/a.flac\n../b.flac\n".into());
                p
            }),
            ("empty file", {
                let mut p = payload("A");
                p.format = Some(ImportFormat::M3u);
                p.content = Some("#EXTM3U\n".into());
                p
            }),
            ("not a pls", {
                let mut p = payload("A");
                p.format = Some(ImportFormat::Pls);
                p.content = Some("File1=a.flac\n".into());
                p
            }),
        ];
        for (what, p) in cases {
            let got =
                handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p)
                    .await;
            assert!(got.is_err(), "{what} must be refused: {got:?}");
        }
        let seen = h.commands.lock().unwrap();
        assert!(
            seen.iter().all(|c| !c.starts_with("playlistadd")
                && !c.starts_with("playlistclear")),
            "no refusal may write: {seen:?}"
        );
        assert!(h.playlists.lock().unwrap().get("A").is_none());
    }

    /// A name already in use is refused, and with `overwrite` the
    /// playlist is emptied before the rows go in.
    #[tokio::test]
    async fn a_duplicate_is_refused_unless_overwrite() {
        let mut h = harness().await;
        h.playlists
            .lock()
            .unwrap()
            .insert("Mine".into(), vec!["INTERNAL/old.flac".into()]);
        let mut p = payload("Mine");
        p.format = Some(ImportFormat::M3u);
        p.content = Some("INTERNAL/new.flac\n".into());
        let got =
            handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p).await;
        assert!(
            matches!(
                got,
                Err(ImportError::Verb(VerbError::DuplicateName { .. }))
            ),
            "{got:?}"
        );
        assert_eq!(
            h.playlists.lock().unwrap().get("Mine").unwrap(),
            &vec!["INTERNAL/old.flac".to_string()],
            "refused means untouched"
        );

        let mut p = payload("Mine");
        p.format = Some(ImportFormat::M3u);
        p.content = Some("INTERNAL/new.flac\n".into());
        p.overwrite = true;
        handle_import_file(&h.ctx, &mut h.conn, h.uploads.path(), p)
            .await
            .unwrap();
        assert_eq!(
            h.playlists.lock().unwrap().get("Mine").unwrap(),
            &vec!["INTERNAL/new.flac".to_string()],
            "overwrite replaces"
        );
    }
}
