// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! State reports emitted by the playback supervisor.
//!
//! The report struct mirrors the narrow MPD status shape the
//! warden cares about. The TOML serialiser is hand-rolled so the
//! crate does not gain a `toml` + `serde` dependency on the
//! critical path; the surface is small (two tables, scalar
//! fields, a handful of strings) and a purpose-built encoder is
//! ~100 lines including tests.
//!
//! String values are truncated to [`MAX_STRING_BYTES`] at UTF-8
//! char boundaries with a trailing `...` marker. The cap prevents
//! a pathologically long tag value from blowing up the reporter
//! channel; 256 bytes is comfortably larger than any real-world
//! tag.
//!
//! The output is valid TOML parseable by any compliant decoder.
//! Consumers downstream of the reporter (the warden, the
//! steward's subject-assertion path) use a standard TOML parser
//! to read these reports.

use std::time::Duration;

use crate::mpd::{MpdSong, MpdStatus, PlayState};

/// Maximum byte length of any string value in a serialised
/// report. Longer strings are truncated at a char boundary with
/// `...` appended; the result is guaranteed to be at most
/// `MAX_STRING_BYTES` bytes.
pub(crate) const MAX_STRING_BYTES: usize = 256;

/// Narrow view of playback state, sized for the reporter surface.
///
/// Built from [`MpdStatus`] plus an optional [`MpdSong`] via
/// [`PlaybackStateReport::from_mpd`]. Durations are stored in
/// milliseconds because the reporter surface is byte-oriented and
/// integer milliseconds are easier to parse downstream than
/// floating-point seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlaybackStateReport {
    /// Playback state (always present).
    pub(crate) state: PlayState,
    /// Zero-based position of the current song in the queue.
    pub(crate) song_position: Option<u32>,
    /// Elapsed time within the current song, in milliseconds.
    pub(crate) elapsed_ms: Option<u64>,
    /// Total duration of the current song, in milliseconds.
    pub(crate) duration_ms: Option<u64>,
    /// Volume level, 0-100.
    pub(crate) volume: Option<u8>,
    /// Explicit mute state. `true` when the warden's `set_mute`
    /// toggle is engaged. Distinct from `volume == 0` because the
    /// operator may legitimately choose 0 as a non-muted setting;
    /// the now-playing subject's `muted` field surfaces this state
    /// so UI subscribers do not have to infer mute from volume.
    pub(crate) muted: bool,
    /// MPD `repeat` mode (queue-repeat).
    pub(crate) repeat: bool,
    /// MPD `random` mode (operator-facing name: shuffle).
    ///
    /// Unchanged: this is MPD's flag and nothing else. Album
    /// order lives in the queue with `random` off, so this reads
    /// false under album shuffle.
    pub(crate) shuffle: bool,
    /// How shuffle picks what plays next: `off`, `tracks` or
    /// `albums`.
    ///
    /// MPD cannot report this -- it has no album mode -- so every
    /// publisher had to guess, and the only guess `status` offers
    /// is "not shuffling". That is how the control lost album
    /// across a refresh. Carried beside `shuffle` rather than
    /// replacing it: the flag is a fielded contract.
    pub(crate) shuffle_mode: crate::album_shuffle::ShuffleMode,
    /// MPD `single` mode (stop after current song; combined with
    /// `repeat`, becomes single-track loop).
    pub(crate) single: bool,
    /// MPD `consume` mode (remove each played song from the
    /// queue).
    pub(crate) consume: bool,
    /// Narrow view of the current song's tags. Absent when MPD
    /// reports no current song.
    pub(crate) current_song: Option<CurrentSongReport>,
}

/// Narrow view of the current song's tag content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentSongReport {
    /// MPD-relative file path.
    pub(crate) file_path: String,
    /// Track title tag, if present.
    pub(crate) title: Option<String>,
    /// Artist tag, if present.
    pub(crate) artist: Option<String>,
    /// Album tag, if present.
    pub(crate) album: Option<String>,
    /// Track duration, in milliseconds.
    pub(crate) duration_ms: Option<u64>,
    /// Classical-music metadata tags carried from the MpdSong;
    /// projected onto the now_playing track envelope by the
    /// subject emitter.
    pub(crate) classical: crate::mpd::ClassicalTags,
}

/// Operator intent MPD cannot report.
///
/// Mute and the shuffle mode are the same category of fact: the
/// device holds them and MPD's `status` does not carry them. Every
/// now-playing publisher needs both, so they travel together
/// rather than as two more positional arguments through five call
/// chains.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PublisherIntent {
    /// The operator's mute intent.
    pub(crate) muted: bool,
    /// The shuffle mode the operator asked for.
    pub(crate) shuffle_mode: crate::album_shuffle::ShuffleMode,
}

/// The cells that intent lives in, travelling as one borrow.
///
/// A publisher does not take a snapshot from its caller — it
/// reads both cells at the moment it builds the report, so the
/// report cannot carry a mute from before the toggle it is
/// reporting. The observers that own the cells pass this down
/// their call chains; `read` makes the snapshot at the leaf.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IntentCells<'a> {
    /// The software mute cell.
    pub(crate) mute: &'a crate::mute_cell::MuteCell,
    /// The remembered shuffle mode.
    pub(crate) shuffle: &'a crate::album_shuffle::ShuffleModeCell,
}

impl IntentCells<'_> {
    /// Snapshot both cells for one report.
    pub(crate) fn read(&self) -> PublisherIntent {
        PublisherIntent {
            muted: self.mute.is_muted(),
            shuffle_mode: self.shuffle.get(),
        }
    }
}

impl PlaybackStateReport {
    /// Build a report from an MPD status plus an optional current
    /// song. `intent` carries the two facts MPD's `status` does
    /// not: the operator's mute, and the shuffle mode behind
    /// MPD's `random` flag.
    pub(crate) fn from_mpd(
        status: MpdStatus,
        song: Option<MpdSong>,
        intent: PublisherIntent,
    ) -> Self {
        Self {
            state: status.state,
            song_position: status.song_position,
            elapsed_ms: status.elapsed.map(duration_to_millis_u64),
            duration_ms: status.duration.map(duration_to_millis_u64),
            volume: status.volume,
            muted: intent.muted,
            repeat: status.repeat,
            shuffle: status.random,
            shuffle_mode: crate::album_shuffle::reported_shuffle_mode(
                status.random,
                intent.shuffle_mode,
            ),
            single: status.single,
            consume: status.consume,
            current_song: song.map(CurrentSongReport::from_mpd_song),
        }
    }

    /// Serialise to a TOML document. The result is a plain
    /// UTF-8 string; callers turn it into bytes at the edge.
    pub(crate) fn serialise(&self) -> String {
        let mut out = String::with_capacity(256);

        out.push_str("state = ");
        out.push_str(&quote_toml_string(play_state_wire_name(self.state)));
        out.push('\n');

        if let Some(v) = self.song_position {
            out.push_str(&format!("song_position = {}\n", v));
        }
        if let Some(v) = self.elapsed_ms {
            out.push_str(&format!("elapsed_ms = {}\n", v));
        }
        if let Some(v) = self.duration_ms {
            out.push_str(&format!("duration_ms = {}\n", v));
        }
        if let Some(v) = self.volume {
            out.push_str(&format!("volume = {}\n", v));
        }

        if let Some(song) = &self.current_song {
            out.push('\n');
            out.push_str("[current_song]\n");
            out.push_str("file_path = ");
            out.push_str(&quote_toml_string(&truncate_to_byte_len(
                &song.file_path,
                MAX_STRING_BYTES,
            )));
            out.push('\n');
            if let Some(t) = &song.title {
                out.push_str("title = ");
                out.push_str(&quote_toml_string(&truncate_to_byte_len(
                    t,
                    MAX_STRING_BYTES,
                )));
                out.push('\n');
            }
            if let Some(a) = &song.artist {
                out.push_str("artist = ");
                out.push_str(&quote_toml_string(&truncate_to_byte_len(
                    a,
                    MAX_STRING_BYTES,
                )));
                out.push('\n');
            }
            if let Some(a) = &song.album {
                out.push_str("album = ");
                out.push_str(&quote_toml_string(&truncate_to_byte_len(
                    a,
                    MAX_STRING_BYTES,
                )));
                out.push('\n');
            }
            if let Some(v) = song.duration_ms {
                out.push_str(&format!("duration_ms = {}\n", v));
            }
        }

        out
    }
}

impl CurrentSongReport {
    pub(crate) fn from_mpd_song(song: MpdSong) -> Self {
        Self {
            file_path: song.file_path,
            title: song.title,
            artist: song.artist,
            album: song.album,
            duration_ms: song.duration.map(duration_to_millis_u64),
            classical: song.classical,
        }
    }
}

fn duration_to_millis_u64(d: Duration) -> u64 {
    // `Duration::as_millis` returns u128; for anything a playback
    // warden encounters (song lengths, elapsed times) u64 is
    // generous. Saturate on the absurd case rather than panic.
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn play_state_wire_name(s: PlayState) -> &'static str {
    match s {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Stopped => "stopped",
    }
}

/// Truncate `s` to at most `max_bytes` UTF-8 bytes, appending
/// `...` if truncation occurs. The returned string is guaranteed
/// to be at most `max_bytes` bytes and to end on a char boundary.
fn truncate_to_byte_len(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Reserve 3 bytes for the trailing marker.
    let limit = max_bytes.saturating_sub(3);
    let mut end = limit.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = String::with_capacity(end + 3);
    truncated.push_str(&s[..end]);
    truncated.push_str("...");
    truncated
}

/// Encode `s` as a TOML basic string (enclosed in double quotes)
/// with the required escapes per the TOML spec. Handles every
/// control character below U+0020.
fn quote_toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stopped_status() -> MpdStatus {
        MpdStatus {
            state: PlayState::Stopped,
            song_position: None,
            elapsed: None,
            duration: None,
            volume: None,
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        }
    }

    // ----- intent projection -----

    /// The projection carries intent MPD's status cannot.
    ///
    /// `MpdStatus` has only `random`. Album shuffle leaves it off
    /// and lives in the queue, so the mode arrives from the
    /// device's own cell and nowhere else. A projection that
    /// dropped it would still serialise and still render — and
    /// the control would lose album across a refresh exactly as
    /// it did before, because the payload would say "off".
    #[test]
    fn the_projection_carries_the_remembered_album_mode() {
        let r = PlaybackStateReport::from_mpd(
            stopped_status(),
            None,
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Albums,
            },
        );
        assert_eq!(
            r.shuffle_mode,
            crate::album_shuffle::ShuffleMode::Albums,
            "album shuffle in force must reach the report"
        );
        assert!(
            !r.shuffle,
            "and must not turn MPD's flag on to say so: the flag is \
             MPD's own and album mode leaves it off"
        );
    }

    /// The publisher reads both cells at the moment it reports.
    ///
    /// `IntentCells` is a borrow, not a snapshot: a report built
    /// after a toggle must carry what the cells hold then, not
    /// what they held when the publisher was handed them. Holding
    /// a snapshot instead is how a report comes to disagree with
    /// the device it is reporting on.
    #[test]
    fn the_publisher_reads_both_cells_when_it_builds_the_report() {
        let mute = crate::mute_cell::MuteCell::new();
        let shuffle = crate::album_shuffle::ShuffleModeCell::new();
        let cells = IntentCells {
            mute: &mute,
            shuffle: &shuffle,
        };

        assert_eq!(
            cells.read().shuffle_mode,
            crate::album_shuffle::ShuffleMode::Off
        );
        assert!(!cells.read().muted);

        shuffle.set(crate::album_shuffle::ShuffleMode::Albums);
        assert_eq!(
            cells.read().shuffle_mode,
            crate::album_shuffle::ShuffleMode::Albums,
            "a mode set after the borrow was taken must still be read"
        );

        mute.set_muted(true);
        assert!(
            cells.read().muted,
            "and the same cell pair carries the mute, read at the \
             same moment"
        );
        assert_eq!(
            cells.read().shuffle_mode,
            crate::album_shuffle::ShuffleMode::Albums,
            "reading one must not disturb the other"
        );
    }

    // ----- serialisation -----

    #[test]
    fn minimal_report_has_only_state() {
        let r = PlaybackStateReport::from_mpd(
            stopped_status(),
            None,
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Off,
            },
        );
        let out = r.serialise();
        assert_eq!(out, "state = \"stopped\"\n");
    }

    #[test]
    fn full_report_serialises_all_scalar_fields() {
        let status = MpdStatus {
            state: PlayState::Playing,
            song_position: Some(3),
            elapsed: Some(Duration::from_millis(12_345)),
            duration: Some(Duration::from_millis(180_000)),
            volume: Some(50),
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        };
        let song = MpdSong {
            file_path: "INTERNAL/Artist/Album/track.flac".to_string(),
            title: Some("Track One".to_string()),
            artist: Some("An Artist".to_string()),
            album: Some("An Album".to_string()),
            duration: Some(Duration::from_millis(180_000)),
            codec_name: Some("flac".to_string()),
            classical: Default::default(),
        };
        let r = PlaybackStateReport::from_mpd(
            status,
            Some(song),
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Off,
            },
        );
        let out = r.serialise();

        // Main table, in declared order.
        assert!(out.starts_with("state = \"playing\"\n"));
        assert!(out.contains("song_position = 3\n"));
        assert!(out.contains("elapsed_ms = 12345\n"));
        assert!(out.contains("duration_ms = 180000\n"));
        assert!(out.contains("volume = 50\n"));
        // Subtable.
        assert!(out.contains("\n[current_song]\n"));
        assert!(
            out.contains("file_path = \"INTERNAL/Artist/Album/track.flac\"\n")
        );
        assert!(out.contains("title = \"Track One\"\n"));
        assert!(out.contains("artist = \"An Artist\"\n"));
        assert!(out.contains("album = \"An Album\"\n"));
    }

    #[test]
    fn current_song_table_omitted_when_none() {
        let status = MpdStatus {
            state: PlayState::Paused,
            song_position: Some(0),
            elapsed: None,
            duration: None,
            volume: None,
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        };
        let r = PlaybackStateReport::from_mpd(
            status,
            None,
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Off,
            },
        );
        let out = r.serialise();
        assert!(!out.contains("[current_song]"));
        assert!(out.contains("state = \"paused\""));
        assert!(out.contains("song_position = 0"));
    }

    #[test]
    fn song_optional_tag_fields_omitted_when_none() {
        let status = MpdStatus {
            state: PlayState::Playing,
            song_position: Some(0),
            elapsed: None,
            duration: None,
            volume: None,
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        };
        let song = MpdSong {
            file_path: "some/file.flac".to_string(),
            title: None,
            artist: None,
            album: None,
            duration: None,
            codec_name: Some("flac".to_string()),
            classical: Default::default(),
        };
        let r = PlaybackStateReport::from_mpd(
            status,
            Some(song),
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Off,
            },
        );
        let out = r.serialise();
        assert!(out.contains("file_path = \"some/file.flac\""));
        assert!(!out.contains("title ="));
        assert!(!out.contains("artist ="));
        assert!(!out.contains("album ="));
    }

    // ----- string escaping -----

    #[test]
    fn escapes_double_quote_in_string() {
        let out = quote_toml_string("he said \"hi\"");
        assert_eq!(out, "\"he said \\\"hi\\\"\"");
    }

    #[test]
    fn escapes_backslash_in_string() {
        let out = quote_toml_string("path\\to\\file");
        assert_eq!(out, "\"path\\\\to\\\\file\"");
    }

    #[test]
    fn escapes_newline_and_tab() {
        let out = quote_toml_string("line1\nline2\tcol");
        assert_eq!(out, "\"line1\\nline2\\tcol\"");
    }

    #[test]
    fn escapes_control_characters_as_unicode() {
        let out = quote_toml_string("\u{01}\u{1f}");
        assert_eq!(out, "\"\\u0001\\u001F\"");
    }

    #[test]
    fn preserves_utf8_characters() {
        let out = quote_toml_string("Bj\u{00f6}rk \u{2014} album");
        assert_eq!(out, "\"Bj\u{00f6}rk \u{2014} album\"");
    }

    // ----- truncation -----

    #[test]
    fn truncation_noop_for_short_strings() {
        let s = "hello";
        let t = truncate_to_byte_len(s, 256);
        assert_eq!(t, "hello");
    }

    #[test]
    fn truncation_at_exact_length_is_noop() {
        let s = "a".repeat(256);
        let t = truncate_to_byte_len(&s, 256);
        assert_eq!(t, s);
    }

    #[test]
    fn truncation_shortens_long_strings_with_ellipsis() {
        let s = "a".repeat(300);
        let t = truncate_to_byte_len(&s, 256);
        assert!(t.len() <= 256);
        assert!(t.ends_with("..."));
        // Prefix is 253 'a' characters (256 - 3 for "...").
        assert_eq!(t.len(), 256);
        assert_eq!(&t[..253], &"a".repeat(253));
    }

    #[test]
    fn truncation_respects_utf8_char_boundary() {
        // Fill up to the byte limit with a mix such that the
        // naive cut point falls inside a multi-byte char.
        //
        // "a" * 251 + "\u{2014}" (em dash, 3 bytes) = 254 bytes.
        // Cap at 256 with room for "..." means the limit for
        // prefix is 253 bytes. The em dash starts at byte 251 and
        // occupies 251-253. Truncating at exactly 253 would cut
        // the em dash mid-byte; the algorithm must back off to a
        // boundary.
        let mut s = "a".repeat(251);
        s.push('\u{2014}'); // 3 bytes
        s.push_str(&"b".repeat(10)); // total > 256 bytes
        let t = truncate_to_byte_len(&s, 256);
        assert!(t.len() <= 256);
        assert!(t.ends_with("..."));
        // The truncation cannot split the em dash. Either the
        // prefix includes the whole em dash or ends before it;
        // both are valid UTF-8.
        let _ = t.chars().count(); // does not panic on invalid utf-8
    }

    // ----- output discipline -----

    #[test]
    fn full_output_is_parseable_ascii_or_utf8() {
        let status = MpdStatus {
            state: PlayState::Playing,
            song_position: Some(0),
            elapsed: Some(Duration::from_millis(100)),
            duration: Some(Duration::from_millis(200)),
            volume: Some(42),
            repeat: false,
            random: false,
            single: false,
            consume: false,
            crossfade_seconds: 0,
            updating_db: None,
        };
        let song = MpdSong {
            file_path: "a/b.flac".to_string(),
            title: Some("T".to_string()),
            artist: Some("A".to_string()),
            album: Some("B".to_string()),
            duration: Some(Duration::from_millis(300)),
            codec_name: Some("flac".to_string()),
            classical: Default::default(),
        };
        let r = PlaybackStateReport::from_mpd(
            status,
            Some(song),
            PublisherIntent {
                muted: false,
                shuffle_mode: crate::album_shuffle::ShuffleMode::Off,
            },
        );
        let out = r.serialise();
        // Every byte is valid UTF-8 (String guarantees this);
        // every line ends with '\n'.
        assert!(out.ends_with('\n'));
        for line in out.lines() {
            // Either a key = value line, a [table] header, or empty.
            assert!(
                line.is_empty() || line.starts_with('[') || line.contains('='),
                "suspicious line: {:?}",
                line
            );
        }
    }
}
