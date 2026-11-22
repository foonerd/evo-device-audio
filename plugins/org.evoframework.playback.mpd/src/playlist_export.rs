// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Export a stored playlist as file text.
//!
//! The operator wants their playlist somewhere else — another
//! player, a NAS share, a friend. The playlist is read from MPD
//! and rendered into the text of a `.m3u` or `.pls` file, which
//! is returned in the response for the glass to offer as a
//! download. That is the whole of it: this module writes no file.
//! Nothing is placed in the music directory, nothing beside the
//! original of anything, nothing in the uploads directory. The
//! bytes go over the wire to the operator and nowhere else.
//!
//! The renderers are pure: entries in, text out.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::mpd::{MpdConnection, MpdPlaylistEntry};
use crate::playlist::{
    validate_playlist_name, PlaylistContext, VerbError,
    PLAYLIST_PAYLOAD_VERSION,
};

/// The formats a playlist can be exported as. There is no cue
/// export: a cue sheet describes one audio file's tracks, which
/// is not what a playlist is.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ExportFormat {
    #[default]
    M3u,
    Pls,
}

impl ExportFormat {
    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::M3u => "m3u",
            Self::Pls => "pls",
        }
    }
}

/// `playlist.export_file` request payload.
#[derive(Debug, Deserialize)]
pub(crate) struct ExportFilePayload {
    pub(crate) v: u32,
    /// The stored playlist to export.
    pub(crate) name: String,
    /// `m3u` unless asked otherwise.
    #[serde(default)]
    pub(crate) format: ExportFormat,
    /// Write library rows as absolute paths under MPD's music
    /// directory, for a player that reads the same share. Off,
    /// rows are MPD-relative, as the playlist holds them.
    #[serde(default)]
    pub(crate) absolute: bool,
}

/// What `playlist.export_file` answers: the file, as text, with
/// a name to save it under.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ExportReport {
    pub(crate) v: u32,
    pub(crate) name: String,
    pub(crate) format: ExportFormat,
    /// `<name>.<ext>` — the playlist name is already a legal
    /// file name: no `/`, no control characters.
    pub(crate) filename: String,
    pub(crate) entries: usize,
    /// The file's text. The consumer offers it as a download.
    pub(crate) content: String,
}

// ----- rendering -----

/// Whether a row is a URI with a scheme rather than a path:
/// streams and stored identities are written as they are.
fn has_scheme(row: &str) -> bool {
    row.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && (rest.starts_with("//") || scheme.eq_ignore_ascii_case("dlna"))
    })
}

/// The row as the file will carry it.
fn location(row: &str, music_directory: &Path, absolute: bool) -> String {
    if absolute && !has_scheme(row) {
        music_directory.join(row).to_string_lossy().into_owned()
    } else {
        row.to_string()
    }
}

/// What the row is called: `Artist - Title`, the title alone, or
/// the file's stem when there is no tag to name it by.
fn display_name(e: &MpdPlaylistEntry) -> String {
    match (e.artist.as_deref(), e.title.as_deref()) {
        (Some(a), Some(t)) => format!("{a} - {t}"),
        (None, Some(t)) => t.to_string(),
        _ => Path::new(&e.file_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| e.file_path.clone()),
    }
}

/// Whole seconds, or `-1` when the length is not known — the
/// value M3U and PLS both use for "unknown".
fn length_secs(e: &MpdPlaylistEntry) -> i64 {
    e.duration.map(|d| d.as_secs() as i64).unwrap_or(-1)
}

/// Extended M3U: `#EXTM3U`, the playlist's name, then per row an
/// `#EXTINF:<secs>,<name>` line and the location. LF line ends;
/// UTF-8; no BOM.
pub(crate) fn render_m3u(
    name: &str,
    entries: &[MpdPlaylistEntry],
    music_directory: &Path,
    absolute: bool,
) -> String {
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str(&format!("#PLAYLIST:{name}\n"));
    for e in entries {
        out.push_str(&format!(
            "#EXTINF:{},{}\n",
            length_secs(e),
            display_name(e)
        ));
        out.push_str(&location(&e.file_path, music_directory, absolute));
        out.push('\n');
    }
    out
}

/// PLS: `[playlist]`, then `FileN`, `TitleN`, `LengthN` per row,
/// `NumberOfEntries` and `Version=2`.
pub(crate) fn render_pls(
    entries: &[MpdPlaylistEntry],
    music_directory: &Path,
    absolute: bool,
) -> String {
    let mut out = String::new();
    out.push_str("[playlist]\n");
    for (i, e) in entries.iter().enumerate() {
        let n = i + 1;
        out.push_str(&format!(
            "File{n}={}\n",
            location(&e.file_path, music_directory, absolute)
        ));
        out.push_str(&format!("Title{n}={}\n", display_name(e)));
        out.push_str(&format!("Length{n}={}\n", length_secs(e)));
    }
    out.push_str(&format!("NumberOfEntries={}\n", entries.len()));
    out.push_str("Version=2\n");
    out
}

/// Render by format.
pub(crate) fn render(
    format: ExportFormat,
    name: &str,
    entries: &[MpdPlaylistEntry],
    music_directory: &Path,
    absolute: bool,
) -> String {
    match format {
        ExportFormat::M3u => {
            render_m3u(name, entries, music_directory, absolute)
        }
        ExportFormat::Pls => render_pls(entries, music_directory, absolute),
    }
}

// ----- the verb -----

/// `playlist.export_file` — read the playlist from MPD and hand
/// back the file's text. Read-only: nothing is written anywhere,
/// and no subject is published.
pub(crate) async fn handle_export_file(
    ctx: &PlaylistContext,
    conn: &mut MpdConnection,
    payload: ExportFilePayload,
) -> Result<ExportReport, VerbError> {
    if payload.v != PLAYLIST_PAYLOAD_VERSION {
        return Err(VerbError::PayloadVersion {
            verb: "playlist.export_file".to_string(),
            got: payload.v,
            expected: PLAYLIST_PAYLOAD_VERSION,
        });
    }
    validate_playlist_name(&payload.name)?;
    let entries = match conn.listplaylistinfo(&payload.name).await {
        Ok(e) => e,
        Err(crate::mpd::MpdError::Ack { code: 50, .. }) => {
            return Err(VerbError::NotFound {
                name: payload.name.clone(),
            });
        }
        Err(e) => {
            return Err(VerbError::Mpd {
                verb: "export_file".to_string(),
                reason: e.to_string(),
            });
        }
    };
    let content = render(
        payload.format,
        &payload.name,
        &entries,
        &ctx.music_directory,
        payload.absolute,
    );
    Ok(ExportReport {
        v: PLAYLIST_PAYLOAD_VERSION,
        filename: format!("{}.{}", payload.name, payload.format.extension()),
        name: payload.name,
        format: payload.format,
        entries: entries.len(),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn entry(
        pos: u32,
        file: &str,
        artist: Option<&str>,
        title: Option<&str>,
        secs: Option<u64>,
    ) -> MpdPlaylistEntry {
        MpdPlaylistEntry {
            position: pos,
            file_path: file.to_string(),
            title: title.map(str::to_string),
            artist: artist.map(str::to_string),
            album: None,
            duration: secs.map(Duration::from_secs),
            classical: Default::default(),
        }
    }

    fn three() -> Vec<MpdPlaylistEntry> {
        vec![
            entry(
                0,
                "INTERNAL/Low/Trust/01 Words.flac",
                Some("Low"),
                Some("Words"),
                Some(240),
            ),
            entry(1, "http://stream.example/live", None, Some("Live"), None),
            entry(2, "USB/Stick/untagged.mp3", None, None, Some(61)),
        ]
    }

    /// The M3U is the extended form: header, playlist name, one
    /// `#EXTINF` per row with `Artist - Title`, the title alone,
    /// or the file stem, and `-1` for an unknown length.
    #[test]
    fn the_m3u_is_the_extended_form() {
        let text = render_m3u(
            "Mine",
            &three(),
            Path::new("/var/lib/evo/music"),
            false,
        );
        assert_eq!(
            text,
            "#EXTM3U\n\
             #PLAYLIST:Mine\n\
             #EXTINF:240,Low - Words\n\
             INTERNAL/Low/Trust/01 Words.flac\n\
             #EXTINF:-1,Live\n\
             http://stream.example/live\n\
             #EXTINF:61,untagged\n\
             USB/Stick/untagged.mp3\n"
        );
    }

    /// The PLS carries the same rows, numbered from one.
    #[test]
    fn the_pls_is_numbered_from_one() {
        let text = render_pls(&three(), Path::new("/var/lib/evo/music"), false);
        assert_eq!(
            text,
            "[playlist]\n\
             File1=INTERNAL/Low/Trust/01 Words.flac\n\
             Title1=Low - Words\n\
             Length1=240\n\
             File2=http://stream.example/live\n\
             Title2=Live\n\
             Length2=-1\n\
             File3=USB/Stick/untagged.mp3\n\
             Title3=untagged\n\
             Length3=61\n\
             NumberOfEntries=3\n\
             Version=2\n"
        );
    }

    /// `absolute` puts library rows under the music directory
    /// and leaves streams and stored identities alone.
    #[test]
    fn absolute_rows_sit_under_the_music_directory() {
        let mut rows = three();
        rows.push(entry(3, "dlna:sid/obj", None, Some("Remote"), None));
        let text = render_m3u("X", &rows, Path::new("/srv/music"), true);
        assert!(
            text.contains("\n/srv/music/INTERNAL/Low/Trust/01 Words.flac\n")
        );
        assert!(text.contains("\nhttp://stream.example/live\n"));
        assert!(text.contains("\n/srv/music/USB/Stick/untagged.mp3\n"));
        assert!(text.contains("\ndlna:sid/obj\n"), "{text}");
        // And a row that merely contains a colon is a path.
        assert!(has_scheme("https://a/b"));
        assert!(has_scheme("dlna:sid/obj"));
        assert!(!has_scheme("INTERNAL/12:34 take.flac"));
    }

    /// An empty playlist exports as a valid, empty file.
    #[test]
    fn an_empty_playlist_is_an_empty_file() {
        let m = Path::new("/m");
        assert_eq!(render_m3u("E", &[], m, false), "#EXTM3U\n#PLAYLIST:E\n");
        assert_eq!(
            render_pls(&[], m, false),
            "[playlist]\nNumberOfEntries=0\nVersion=2\n"
        );
    }

    /// Where nothing is written: this module has no filesystem
    /// write in it at all. The text goes into the response and
    /// nowhere else. The source outside this test module is
    /// held to that.
    #[test]
    fn the_export_writes_no_file() {
        let src = include_str!("playlist_export.rs");
        let production = src.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "std::fs",
            "tokio::fs",
            "File::create",
            "OpenOptions",
            "fs::write",
            "fs::rename",
            "fs::copy",
            "write_all",
        ] {
            assert!(
                !production.contains(forbidden),
                "the export module must not write a file; found {forbidden:?}"
            );
        }
    }

    /// The verb, against the stored-playlists mock: the held
    /// playlist comes back as a named file, read-only, with
    /// nothing written and no subject published.
    #[tokio::test]
    async fn the_verb_hands_back_the_file_and_touches_nothing() {
        use crate::playback_supervisor::test_mock::{
            short_timeouts, spawn_mock_mpd, CapturingSubjectAnnouncer,
            ConnBehaviour,
        };
        use crate::playlist::DEFAULT_FAVOURITES_PLAYLIST_NAME;
        use crate::source_registry::SourceRegistry;
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let playlists = Arc::new(std::sync::Mutex::new(BTreeMap::from([(
            "Road".to_string(),
            vec![
                "INTERNAL/a.flac".to_string(),
                "http://s.example/x".to_string(),
            ],
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
        let mut conn =
            MpdConnection::connect_with_timeouts(endpoint, short_timeouts())
                .await
                .unwrap();
        let music = tempfile::tempdir().unwrap();
        let subjects = Arc::new(CapturingSubjectAnnouncer::default());
        let ctx = PlaylistContext::new(
            music.path().to_path_buf(),
            SourceRegistry::new(),
            Arc::clone(&subjects)
                as Arc<dyn evo_plugin_sdk::contract::SubjectAnnouncer>,
            DEFAULT_FAVOURITES_PLAYLIST_NAME.to_string(),
            None,
        );

        let report = handle_export_file(
            &ctx,
            &mut conn,
            ExportFilePayload {
                v: 1,
                name: "Road".to_string(),
                format: ExportFormat::M3u,
                absolute: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(report.filename, "Road.m3u");
        assert_eq!(report.entries, 2);
        assert_eq!(
            report.content,
            "#EXTM3U\n#PLAYLIST:Road\n#EXTINF:-1,a\nINTERNAL/a.flac\n\
             #EXTINF:-1,x\nhttp://s.example/x\n"
        );
        // Read-only: nothing written through MPD, nothing on
        // disk, nothing published.
        {
            let seen = commands.lock().unwrap();
            assert!(
                seen.iter().all(|c| c.starts_with("listplaylistinfo")),
                "an export only reads: {seen:?}"
            );
        }
        assert!(std::fs::read_dir(music.path()).unwrap().next().is_none());
        assert_eq!(subjects.state_update_count(), 0);

        let pls = handle_export_file(
            &ctx,
            &mut conn,
            ExportFilePayload {
                v: 1,
                name: "Road".to_string(),
                format: ExportFormat::Pls,
                absolute: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(pls.filename, "Road.pls");
        assert!(pls.content.starts_with("[playlist]\nFile1="));
        assert!(pls.content.contains(&format!(
            "File1={}/INTERNAL/a.flac\n",
            music.path().display()
        )));

        // A bad name never reaches MPD.
        let bad = handle_export_file(
            &ctx,
            &mut conn,
            ExportFilePayload {
                v: 1,
                name: "a/b".to_string(),
                format: ExportFormat::M3u,
                absolute: false,
            },
        )
        .await;
        assert!(matches!(bad, Err(VerbError::InvalidName { .. })));
        let _ = PathBuf::new();
    }
}
