// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! MPD-domain types.
//!
//! Narrow, concrete types the MPD connection layer speaks in. These
//! are not distribution-shaped; they are MPD-domain facts the warden will
//! later project into whatever the steward's contract requires.
//!
//! All types are `pub` because they are implementation detail
//! of the plugin; the admission surface in `lib.rs` does not expose
//! them.

use std::time::Duration;

/// Classical-music metadata tags extracted from MPD's per-song
/// tag set.
///
/// Every field is `Option<String>` (or `Option<u32>` for the
/// numeric MovementNumber) per the wire-shape-defaults-must-be-
/// truth-or-null invariant (see PLUGIN_CONTRACT.md §15): an
/// absent or empty MPD tag serialises as JSON `null`, never as
/// an empty string masquerading as known-empty. Empty strings
/// arriving from MPD (rare but possible — MPD can emit a tag
/// line with an empty value) are normalised to `None` at parse
/// time via [`some_if_non_empty`].
///
/// Carried alongside the always-on title / artist / album
/// triplet on every track-bearing MPD type
/// (MpdSong / MpdQueueItem / MpdPlaylistEntry /
/// MpdLibraryEntry::File) so every track-bearing wire envelope
/// projects the same fields from the same source. The per-shelf
/// envelope serialisers flatten this struct into their JSON
/// output (see queue / favourites / playlist / library / playback
/// modules).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClassicalTags {
    /// MPD `Composer` tag — name of the composition's composer.
    pub composer: Option<String>,
    /// MPD `ComposerSort` tag — sort-form of composer name
    /// (e.g. "Beethoven, Ludwig van" for sorting under "B").
    pub composer_sort: Option<String>,
    /// MPD `Conductor` tag — conductor of the recording.
    pub conductor: Option<String>,
    /// MPD `Ensemble` tag — orchestra / chamber group / quartet.
    pub ensemble: Option<String>,
    /// MPD `Performer` tag — soloist or featured performer.
    pub performer: Option<String>,
    /// MPD `Work` tag — canonical composition name
    /// (e.g. "Symphony No. 5 in C minor, Op. 67").
    pub work: Option<String>,
    /// MPD `WorkSort` tag — sort-form of work name.
    pub work_sort: Option<String>,
    /// MPD `Movement` tag — movement name within the work
    /// (e.g. "Allegro con brio").
    pub movement: Option<String>,
    /// MPD `MovementNumber` tag — movement number (1, 2, 3, ...).
    /// Parsed as u32; non-numeric values surface as `None`.
    pub movement_number: Option<u32>,
    /// MPD `OriginalDate` tag — year the recording was made.
    /// The audiophile-relevant year, distinct from `Date`
    /// which is the issue / release year.
    pub original_date: Option<String>,
    /// MPD `Date` tag — year of release / issue.
    pub recording_date: Option<String>,
    /// MPD `Label` tag — record label (DG / EMI / Sony / etc.).
    pub label: Option<String>,
    /// MPD `Media` tag — physical or distribution medium
    /// (e.g. "CD", "SACD", "Vinyl", "Streaming").
    pub medium: Option<String>,
}

impl ClassicalTags {
    /// Apply one MPD field to the appropriate classical-tag
    /// slot. Returns `true` when the key matched a classical
    /// tag (caller can skip the default arm); `false` when the
    /// key is not one of the classical tags (caller continues
    /// the match).
    ///
    /// Empty-string values are normalised to `None` per the
    /// truth-or-null invariant — MPD's wire occasionally emits
    /// a tag line with an empty value, and the wire MUST NOT
    /// surface that as a known-empty string.
    pub fn try_apply(&mut self, key: &str, value: &str) -> bool {
        match key {
            "Composer" => {
                self.composer = some_if_non_empty(value);
                true
            }
            "ComposerSort" => {
                self.composer_sort = some_if_non_empty(value);
                true
            }
            "Conductor" => {
                self.conductor = some_if_non_empty(value);
                true
            }
            "Ensemble" => {
                self.ensemble = some_if_non_empty(value);
                true
            }
            "Performer" => {
                self.performer = some_if_non_empty(value);
                true
            }
            "Work" => {
                self.work = some_if_non_empty(value);
                true
            }
            "WorkSort" => {
                self.work_sort = some_if_non_empty(value);
                true
            }
            "Movement" => {
                self.movement = some_if_non_empty(value);
                true
            }
            "MovementNumber" => {
                // Non-numeric values surface as None, not as
                // a fabricated zero — the truth-or-null
                // contract.
                self.movement_number = value.trim().parse::<u32>().ok();
                true
            }
            "OriginalDate" => {
                self.original_date = some_if_non_empty(value);
                true
            }
            "Date" => {
                self.recording_date = some_if_non_empty(value);
                true
            }
            "Label" => {
                self.label = some_if_non_empty(value);
                true
            }
            "Media" => {
                self.medium = some_if_non_empty(value);
                true
            }
            _ => false,
        }
    }
}

/// Normalise an MPD tag value to `Option<String>`: empty or
/// whitespace-only strings become `None`. Used by
/// [`ClassicalTags::try_apply`] and any other parser path that
/// surfaces MPD-tag values to the wire.
pub fn some_if_non_empty(v: &str) -> Option<String> {
    if v.trim().is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// MPD playback state, as reported by the `status` command's
/// `state:` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlayState {
    /// Actively playing a song.
    Playing,
    /// Paused mid-song.
    Paused,
    /// Stopped (nothing playing; position not retained).
    Stopped,
}

/// MPD protocol version, parsed from the welcome banner
/// (`OK MPD <major>.<minor>.<patch>`).
///
/// Comparable and orderable so later phases can gate feature use on
/// minimum protocol versions (for example, `partition` support arrived
/// in 0.22, `readpicture` in 0.22, `albumart` in 0.21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MpdVersion {
    /// Major version number.
    pub major: u32,
    /// Minor version number.
    pub minor: u32,
    /// Patch version number.
    pub patch: u32,
}

impl MpdVersion {
    /// Construct a version with the three components.
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl std::fmt::Display for MpdVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Narrow view of MPD's `status` response.
///
/// Only the fields the playback warden needs today. Additional fields
/// MPD reports (mixrampdb, audio, etc.) are intentionally dropped
/// rather than surfaced: the connection layer's surface grows by
/// explicit opt-in, not by accumulating every tag MPD emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdStatus {
    /// Playback state (always present in MPD responses).
    pub state: PlayState,
    /// Zero-based position of the current song within the queue.
    /// `None` when the queue is empty or nothing is selected.
    pub song_position: Option<u32>,
    /// Elapsed time within the current song. `None` when the player
    /// is stopped, or when MPD does not report it (some sources omit
    /// elapsed on initial response; this is treated as unknown, not
    /// zero).
    pub elapsed: Option<Duration>,
    /// Total duration of the current song. `None` when MPD does not
    /// report it (streams, some CD rips).
    pub duration: Option<Duration>,
    /// Volume level, 0-100. `None` when MPD reports -1 (no mixer
    /// configured) or when the field is absent.
    pub volume: Option<u8>,
    /// `repeat` mode: when set, MPD restarts the queue from
    /// position 0 after the last song ends. `false` when MPD
    /// omits the field. Captured by the `emit_test_tone`
    /// diagnostic so the operator's prior value is restored
    /// after the tone completes.
    pub repeat: bool,
    /// `random` mode: when set, MPD plays queue entries in
    /// random order. `false` when MPD omits the field.
    /// Captured then restored by `emit_test_tone` for the
    /// same reason as [`Self::repeat`].
    pub random: bool,
    /// `single` mode: when set, MPD stops after the current
    /// song instead of advancing. `false` when MPD omits the
    /// field. Captured then restored by `emit_test_tone` for
    /// the same reason as [`Self::repeat`].
    pub single: bool,
    /// `consume` mode: when set, MPD removes each song from
    /// the queue after it plays. `false` when MPD omits the
    /// field. Captured then restored by `emit_test_tone` for
    /// the same reason as [`Self::repeat`].
    pub consume: bool,
    /// Inter-song crossfade in seconds; `0` disables. `0`
    /// when MPD omits the field. Captured then restored by
    /// `emit_test_tone` for the same reason as
    /// [`Self::repeat`].
    pub crossfade_seconds: u32,
}

/// Narrow view of MPD's `stats` response.
///
/// Drives library-state rehydration on plugin init: the
/// `audio_library_state` subject's `total_tracks` field comes
/// from [`Self::songs`]; `last_full_scan_at_ms` derives from
/// [`Self::db_update_unix_s`] when present. Other fields MPD
/// reports (artists, albums, uptime, db_playtime, playtime) are
/// surfaced for diagnostics + cross-shelf consumers but not
/// every consumer needs every field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MpdStats {
    /// Number of unique artists in the database. `0` when MPD
    /// omits the field.
    pub artists: u32,
    /// Number of unique albums in the database. `0` when MPD
    /// omits the field.
    pub albums: u32,
    /// Number of songs in the database. `0` when MPD omits the
    /// field. Drives library-state `total_tracks`.
    pub songs: u32,
    /// Unix timestamp (seconds) of MPD's last database update.
    /// `None` when the field is absent (fresh MPD with no
    /// completed scan). Drives library-state
    /// `last_full_scan_at_ms` (multiplied by 1000).
    pub db_update_unix_s: Option<u64>,
}

/// Narrow view of MPD's `currentsong` response.
///
/// Only the fields the playback warden needs today. A richer shape
/// (composer, date, track number, disc number, etc.) extends this
/// when the consuming subject assertion demands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdSong {
    /// MPD-relative file path (e.g. `INTERNAL/Artist/Album/track.flac`).
    /// Always present when `currentsong` returns a non-empty response.
    pub file_path: String,
    /// Track title tag, if present.
    pub title: Option<String>,
    /// Artist tag, if present (prefers Artist over AlbumArtist; the
    /// warden's subject-assertion logic may walk both).
    pub artist: Option<String>,
    /// Album tag, if present.
    pub album: Option<String>,
    /// Track duration from the `duration:` field (MPD 0.21+) or
    /// `Time:` (older).
    pub duration: Option<Duration>,
    /// Classical-music metadata tags. Carried alongside the
    /// always-on title / artist / album triplet so every
    /// track-bearing envelope can project the same fields without
    /// extra round-trips. See [`ClassicalTags`].
    pub classical: ClassicalTags,
    /// Source codec name derived from the file path's extension at
    /// parse time, lowercased and normalised to the canonical token
    /// the audio.playback.v1 wire contract uses (see
    /// [`derive_source_codec_name`]). `None` when the extension is
    /// absent, unknown, or the path is empty.
    ///
    /// This is the authoritative source-codec signal MPD exposes:
    /// MPD's `audio:` field reports the OUTPUT format (post-decode,
    /// post-resample, post-DoP-wrap), not the source. The file
    /// extension is the only honest source-side fact in MPD's
    /// per-song surface. Stream URLs without an extension are
    /// reported as `None` rather than guessing from MIME or URL
    /// path heuristics.
    pub codec_name: Option<String>,
}

/// Derive a canonical source-codec name from a file path's
/// extension.
///
/// Returns the lowercase short codec token (e.g. `"flac"`,
/// `"mp3"`, `"dsf"`) that the audio.playback.v1 wire contract
/// surfaces on the `source_codec` field of the
/// `audio_playback_stream_format` subject. The function answers
/// honestly with `None` when:
///
/// - the path is empty,
/// - the path has no extension,
/// - the extension is not in the well-known set below.
///
/// The well-known set is curated to cover the codecs MPD actually
/// decodes in standard upstream + Debian builds. New entries are
/// added by editing this function (and adding a unit test). The
/// table is deliberately conservative: filenames frequently lie
/// (e.g. `.mp3` containing AAC) but the extension is what the
/// user actually sees, and matching the user-visible label is the
/// honest UI contract. A future enhancement can sharpen the
/// signal by reading the file's magic bytes; until that lands,
/// the extension is the load-bearing signal.
pub fn derive_source_codec_name(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    // Find the rightmost `.` after the last `/` so paths with
    // dots in directory names (e.g. `Music/Artist v.2/track.flac`)
    // resolve to the file's extension only.
    let last_slash = path.rfind('/').map(|i| i + 1).unwrap_or(0);
    let filename = &path[last_slash..];
    let dot = filename.rfind('.')?;
    let raw_ext = &filename[dot + 1..];
    if raw_ext.is_empty() {
        return None;
    }
    let ext = raw_ext.to_ascii_lowercase();
    // Strip any URL query string / fragment from the extension
    // (e.g. `track.flac?token=abc` → `flac`). MPD passes stream
    // URLs verbatim; we treat them defensively.
    let ext = ext.split(['?', '#', '&']).next()?;
    let canonical = match ext {
        // Lossless PCM containers.
        "flac" => "flac",
        "wav" => "wav",
        "aif" | "aiff" => "aiff",
        "ape" => "ape",
        "alac" | "m4a" => "alac",
        "wv" => "wavpack",
        "tta" => "tta",
        "shn" => "shorten",
        // DSD containers.
        "dsf" => "dsf",
        "dff" => "dff",
        // Lossy.
        "mp3" => "mp3",
        "ogg" | "oga" => "vorbis",
        "opus" => "opus",
        "aac" | "mp4" => "aac",
        "wma" => "wma",
        "mpc" => "musepack",
        // Tracker / module formats MPD's modplug decoder handles.
        "mod" | "s3m" | "xm" | "it" => "mod",
        // Speech / low-bitrate.
        "spx" => "speex",
        _ => return None,
    };
    Some(canonical.to_string())
}

/// MPD idle subsystems.
///
/// The canonical set listed in MPD's protocol documentation. Used by
/// the `idle` command both to request subscription (client tells MPD
/// which subsystems it cares about) and to surface change events
/// (MPD tells the client which subsystems changed).
///
/// Unknown values parse to [`IdleSubsystem::Other`] rather than
/// erroring. This lets the warden keep running against a future MPD
/// that adds a new subsystem; the change event is simply observed
/// under its protocol name and ignored if the warden does not yet
/// recognise it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdleSubsystem {
    /// The song database.
    Database,
    /// A database update has started or finished.
    Update,
    /// A stored playlist has been modified.
    StoredPlaylist,
    /// The current queue has been modified.
    Playlist,
    /// The player has been started, stopped or seeked.
    Player,
    /// The volume has been changed.
    Mixer,
    /// An audio output has been added, removed, or toggled.
    Output,
    /// Playback options (repeat, random, crossfade, replay gain).
    Options,
    /// A partition was added, removed, or changed.
    Partition,
    /// The sticker database has been modified.
    Sticker,
    /// A client has subscribed or unsubscribed to a channel.
    Subscription,
    /// A message was received on a channel.
    Message,
    /// A neighbor was found or lost.
    Neighbor,
    /// The mount list has changed.
    Mount,
    /// An unknown subsystem name. Stored as the raw protocol string
    /// so the warden can surface it in diagnostics without losing
    /// information.
    Other(String),
}

impl IdleSubsystem {
    /// The MPD protocol wire name for this subsystem.
    pub fn as_protocol_str(&self) -> &str {
        match self {
            Self::Database => "database",
            Self::Update => "update",
            Self::StoredPlaylist => "stored_playlist",
            Self::Playlist => "playlist",
            Self::Player => "player",
            Self::Mixer => "mixer",
            Self::Output => "output",
            Self::Options => "options",
            Self::Partition => "partition",
            Self::Sticker => "sticker",
            Self::Subscription => "subscription",
            Self::Message => "message",
            Self::Neighbor => "neighbor",
            Self::Mount => "mount",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parse an MPD protocol subsystem name.
    ///
    /// Unknown names return `IdleSubsystem::Other(s.to_string())`
    /// rather than erroring: the protocol can gain subsystems
    /// without our crate having to handle every one explicitly.
    pub fn from_protocol_str(s: &str) -> Self {
        match s {
            "database" => Self::Database,
            "update" => Self::Update,
            "stored_playlist" => Self::StoredPlaylist,
            "playlist" => Self::Playlist,
            "player" => Self::Player,
            "mixer" => Self::Mixer,
            "output" => Self::Output,
            "options" => Self::Options,
            "partition" => Self::Partition,
            "sticker" => Self::Sticker,
            "subscription" => Self::Subscription,
            "message" => Self::Message,
            "neighbor" => Self::Neighbor,
            "mount" => Self::Mount,
            other => Self::Other(other.to_string()),
        }
    }
}

// ----- queue / stored-playlist / sticker / library types -----

/// One entry in MPD's live playback queue. Projected from
/// `playlistinfo`'s response — every queue entry MPD reports
/// is one of these.
///
/// Distinct from [`MpdSong`] (which is `currentsong`'s narrower
/// projection). Queue items carry the per-entry `id` (MPD's
/// `Id:` field, stable across queue reorderings within MPD's
/// current lifetime) + `position` (the `Pos:` field, mutates on
/// reorder). The plugin's queue subject emitter projects these
/// onto the wire's `audio_queue.items` array with the addition
/// of the per-song `evo:available` sticker as the wire's
/// `available` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdQueueItem {
    /// MPD songid (`Id:`); stable across queue reorderings within
    /// MPD's current lifetime, but NOT durable across MPD restart.
    pub id: u32,
    /// Zero-based queue position (`Pos:`); mutates when items move.
    pub position: u32,
    /// MPD-relative file path (or external URI for streams).
    pub file_path: String,
    /// Track title tag, if present.
    pub title: Option<String>,
    /// Artist tag, if present.
    pub artist: Option<String>,
    /// Album tag, if present.
    pub album: Option<String>,
    /// Track duration; from `duration:` (0.21+) or `Time:` (older).
    pub duration: Option<Duration>,
    /// Classical-music metadata tags; see [`ClassicalTags`].
    pub classical: ClassicalTags,
}

/// Summary of one stored playlist. Projected from `listplaylists`'s
/// response: each entry is a `playlist:` line + optional
/// `Last-Modified:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdPlaylistSummary {
    /// Playlist name (operator-facing identifier).
    pub name: String,
    /// MPD's `Last-Modified:` field as an ISO-8601 string when
    /// reported by MPD; absent for very old MPD versions.
    pub last_modified: Option<String>,
}

/// One entry in a stored playlist's listing. Projected from
/// `listplaylistinfo NAME`'s response — same per-song metadata
/// shape as `playlistinfo` but without the queue-specific `Id` /
/// `Pos` fields. The playlist module assigns positions by parse
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdPlaylistEntry {
    /// Zero-based position within the playlist (assigned by parse
    /// order; not reported by MPD).
    pub position: u32,
    /// MPD-relative file path (or external URI).
    pub file_path: String,
    /// Track title tag, if present.
    pub title: Option<String>,
    /// Artist tag, if present.
    pub artist: Option<String>,
    /// Album tag, if present.
    pub album: Option<String>,
    /// Track duration; from `duration:` (0.21+) or `Time:` (older).
    pub duration: Option<Duration>,
    /// Classical-music metadata tags; see [`ClassicalTags`].
    pub classical: ClassicalTags,
}

/// One sticker key/value pair. Projected from `sticker get`,
/// `sticker list`, and `sticker find` responses. MPD's sticker
/// subsystem attaches durable per-song key/value pairs that
/// survive MPD restart, database update, and mount-unmount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdSticker {
    /// Sticker name (the framework uses `evo:available` for the
    /// per-song availability flag).
    pub name: String,
    /// Sticker value (opaque string; framework convention is `0`
    /// / `1` for boolean stickers).
    pub value: String,
}

/// One entry returned by `sticker find`. Projected from the
/// per-match repeated `file:` + `sticker:` lines MPD emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdStickerMatch {
    /// MPD-relative file path the sticker is attached to.
    pub file_path: String,
    /// The matching sticker.
    pub sticker: MpdSticker,
}

/// One entry in MPD's library tree listing, projected from
/// `lsinfo PATH`. MPD interleaves directory / file / playlist
/// entries in the response, separated by leading key types.
/// Operator UI's browse view consumes the projected
/// `BrowseEntry` shape on the wire; the plugin's library module
/// translates these MPD-domain entries to the wire shape.
///
/// The variant-size difference is intentional. Entries are
/// produced by the streaming `listallinfo` parser, projected to
/// the wire `BrowseEntry`, and dropped — they never live past
/// the request-handling call. Boxing the `File` variant would
/// add one heap allocation per parsed track (1000+ on a typical
/// library walk) for zero correctness benefit; the lint's
/// optimisation pressure does not apply to short-lived
/// projection types.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpdLibraryEntry {
    /// A subdirectory under the queried path.
    Directory {
        /// Path relative to MPD's music_directory.
        path: String,
        /// MPD's `Last-Modified:` field when reported.
        last_modified: Option<String>,
    },
    /// A music file.
    File {
        /// Path relative to MPD's music_directory.
        path: String,
        /// Track title tag.
        title: Option<String>,
        /// Artist tag (per-track credit; on collaboration
        /// releases this differs from `albumartist`).
        artist: Option<String>,
        /// Album-artist tag (release-level primary credit;
        /// distinct from the per-track `artist` on
        /// compilations and collaborations).
        albumartist: Option<String>,
        /// Album tag.
        album: Option<String>,
        /// Track duration.
        duration: Option<Duration>,
        /// Classical-music metadata tags; see [`ClassicalTags`].
        classical: ClassicalTags,
    },
    /// A stored playlist file (`.m3u` etc.) discovered in the tree.
    Playlist {
        /// Playlist path relative to MPD's music_directory (or
        /// the bare playlist name when under MPD's playlist
        /// directory).
        path: String,
        /// MPD's `Last-Modified:` field when reported.
        last_modified: Option<String>,
    },
}

/// One MPD mount, projected from `listmounts`. A mount makes
/// remote storage (NAS over CIFS/SMB/NFS, cloud via WebDAV,
/// network attached storage via the smb_client / nfs / curl /
/// webdav storage plugins) accessible under a named alias that
/// scopes the resulting songs in MPD's database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdMount {
    /// The mount alias name. MPD `lsinfo "NAME"` lists songs
    /// from this mount; the path scope for `update NAME` and
    /// `unmount NAME` is the mount alias.
    pub name: String,
    /// The storage URI MPD has mounted. Empty for the root
    /// (un-aliased) storage; non-empty for explicit mounts.
    pub storage: String,
}

/// One MPD storage neighbour, projected from `listneighbors`.
/// Neighbours are storage providers MPD discovered via its
/// neighbor plugins (smbclient discovery, upnp, etc.); the
/// operator can subsequently issue `mount NAME URI` to mount
/// one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpdNeighbor {
    /// The storage URI the neighbour offers (e.g.
    /// `smb://server/share`, `upnp://uuid:abc.../`).
    pub uri: String,
    /// Operator-facing display name MPD reports (server name,
    /// share name, etc.). May be empty.
    pub name: String,
}

/// Search field MPD's `find` / `search` commands accept as the
/// `TYPE` argument. The MPD protocol allows tag names verbatim;
/// this enum bounds the surface the plugin exposes to the
/// operator-facing library shelf so a contributor cannot
/// accidentally surface every internal MPD tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpdSearchField {
    /// `any` — matches against every tag MPD indexes.
    Any,
    /// `artist`.
    Artist,
    /// `albumartist`.
    AlbumArtist,
    /// `album`.
    Album,
    /// `title`.
    Title,
    /// `genre`.
    Genre,
    /// `composer`.
    Composer,
    /// `file` — the relative file path.
    File,
    /// `base` — anchor the search at a directory prefix; MPD
    /// 0.20+ accepts this as a filter against the file path.
    Base,
    /// `date` — the release-date tag; MPD stores whatever the
    /// file was tagged with (`YYYY`, `YYYY-MM`, `YYYY-MM-DD`).
    /// A `search date <YYYY>` substring-matches every file
    /// whose date starts with the given year, which is exactly
    /// what a facet-drill on the year bucket wants.
    Date,
}

impl MpdSearchField {
    /// Wire token the MPD protocol expects.
    pub fn as_protocol_str(&self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Artist => "artist",
            Self::AlbumArtist => "albumartist",
            Self::Album => "album",
            Self::Title => "title",
            Self::Genre => "genre",
            Self::Composer => "composer",
            Self::File => "file",
            Self::Base => "base",
            Self::Date => "date",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_displays_dotted_triple() {
        let v = MpdVersion::new(0, 23, 5);
        assert_eq!(format!("{}", v), "0.23.5");
    }

    #[test]
    fn versions_order_by_component() {
        let a = MpdVersion::new(0, 22, 0);
        let b = MpdVersion::new(0, 23, 0);
        let c = MpdVersion::new(0, 23, 1);
        assert!(a < b);
        assert!(b < c);
        assert_eq!(b, MpdVersion::new(0, 23, 0));
    }

    #[test]
    fn idle_subsystem_all_known_variants_round_trip() {
        // Exhaustive over the canonical MPD subsystem set. If MPD
        // adds a new subsystem, from_protocol_str maps it to
        // Other(_) rather than failing; this test does not need to
        // be updated in that case (a new round-trip test for the
        // new variant would be added).
        for s in [
            IdleSubsystem::Database,
            IdleSubsystem::Update,
            IdleSubsystem::StoredPlaylist,
            IdleSubsystem::Playlist,
            IdleSubsystem::Player,
            IdleSubsystem::Mixer,
            IdleSubsystem::Output,
            IdleSubsystem::Options,
            IdleSubsystem::Partition,
            IdleSubsystem::Sticker,
            IdleSubsystem::Subscription,
            IdleSubsystem::Message,
            IdleSubsystem::Neighbor,
            IdleSubsystem::Mount,
        ] {
            let wire = s.as_protocol_str().to_string();
            let back = IdleSubsystem::from_protocol_str(&wire);
            assert_eq!(back, s, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn idle_subsystem_stored_playlist_uses_underscored_wire_name() {
        assert_eq!(
            IdleSubsystem::StoredPlaylist.as_protocol_str(),
            "stored_playlist"
        );
    }

    #[test]
    fn idle_subsystem_unknown_parses_as_other() {
        let parsed = IdleSubsystem::from_protocol_str("future_subsystem");
        match parsed {
            IdleSubsystem::Other(s) => assert_eq!(s, "future_subsystem"),
            other => panic!("expected Other(_), got {other:?}"),
        }
    }

    #[test]
    fn idle_subsystem_other_variant_round_trips_its_contents() {
        let original = IdleSubsystem::Other("custom".to_string());
        assert_eq!(original.as_protocol_str(), "custom");
        let back = IdleSubsystem::from_protocol_str("custom");
        assert_eq!(back, IdleSubsystem::Other("custom".to_string()));
    }

    #[test]
    fn derive_source_codec_name_returns_canonical_token_for_well_known_extensions(
    ) {
        // One representative per codec family. The full mapping is
        // the function itself; this proves every branch resolves.
        let cases: &[(&str, &str)] = &[
            ("track.flac", "flac"),
            ("track.FLAC", "flac"),
            ("Music/Artist/Album/track.mp3", "mp3"),
            ("song.wav", "wav"),
            ("song.aif", "aiff"),
            ("song.aiff", "aiff"),
            ("file.ape", "ape"),
            ("file.alac", "alac"),
            ("file.m4a", "alac"),
            ("file.wv", "wavpack"),
            ("file.tta", "tta"),
            ("file.shn", "shorten"),
            ("dsd.dsf", "dsf"),
            ("dsd.dff", "dff"),
            ("file.ogg", "vorbis"),
            ("file.oga", "vorbis"),
            ("file.opus", "opus"),
            ("file.aac", "aac"),
            ("file.mp4", "aac"),
            ("file.wma", "wma"),
            ("file.mpc", "musepack"),
            ("file.mod", "mod"),
            ("file.s3m", "mod"),
            ("file.xm", "mod"),
            ("file.it", "mod"),
            ("file.spx", "speex"),
        ];
        for (path, expected) in cases {
            assert_eq!(
                derive_source_codec_name(path).as_deref(),
                Some(*expected),
                "path {path} did not resolve to {expected}"
            );
        }
    }

    #[test]
    fn derive_source_codec_name_returns_none_on_unknown_extension() {
        // Unknown extensions resolve to None rather than guessing.
        // The UI surfaces "unknown" honestly when this is the case.
        assert_eq!(derive_source_codec_name("file.xyz"), None);
        assert_eq!(derive_source_codec_name("file.txt"), None);
        assert_eq!(derive_source_codec_name("file.bin"), None);
    }

    #[test]
    fn derive_source_codec_name_returns_none_on_missing_extension() {
        // Paths with no extension (rare for MPD library entries,
        // common for stream URLs without a file path) resolve to
        // None.
        assert_eq!(derive_source_codec_name("noext"), None);
        assert_eq!(derive_source_codec_name("Music/Album/track"), None);
    }

    #[test]
    fn derive_source_codec_name_returns_none_on_empty_path() {
        assert_eq!(derive_source_codec_name(""), None);
    }

    #[test]
    fn derive_source_codec_name_ignores_dots_in_directory_names() {
        // A dot in the directory must not be confused for the
        // file's extension — only the rightmost dot AFTER the
        // last slash counts.
        assert_eq!(
            derive_source_codec_name("Music/Artist v.2/track.flac").as_deref(),
            Some("flac")
        );
        // Trailing-dot edge case: directory name ends in a dot
        // but the file itself has no extension → None.
        assert_eq!(derive_source_codec_name("Artist v.2/track"), None);
    }

    #[test]
    fn derive_source_codec_name_strips_url_query_string() {
        // Stream URLs sometimes carry tokens after the extension;
        // strip them so the well-known-extension lookup still
        // resolves.
        assert_eq!(
            derive_source_codec_name("http://example/stream.flac?token=abc")
                .as_deref(),
            Some("flac")
        );
        assert_eq!(
            derive_source_codec_name("http://example/stream.mp3#anchor")
                .as_deref(),
            Some("mp3")
        );
        assert_eq!(
            derive_source_codec_name("https://stream/song.ogg&x=1").as_deref(),
            Some("vorbis")
        );
    }

    #[test]
    fn derive_source_codec_name_case_insensitive() {
        // Filesystems vary on case-sensitivity; uppercase
        // extensions resolve identically.
        assert_eq!(
            derive_source_codec_name("TRACK.FLAC").as_deref(),
            Some("flac")
        );
        assert_eq!(
            derive_source_codec_name("Track.Mp3").as_deref(),
            Some("mp3")
        );
    }

    #[test]
    fn derive_source_codec_name_handles_extensionless_dotfile() {
        // A leading-dot filename ("dotfile") with no further
        // extension resolves to None (the only dot IS the
        // leading dot; the function looks for the rightmost dot
        // in the filename portion).
        //
        // This case is mostly defensive — MPD does not surface
        // dotfiles as music library entries — but the function's
        // honest answer is None, not a synthetic codec name.
        assert_eq!(derive_source_codec_name(".hidden"), None);
    }
}
