//! MPD-domain types.
//!
//! Narrow, concrete types the MPD connection layer speaks in. These
//! are not distribution-shaped; they are MPD-domain facts the warden will
//! later project into whatever the steward's contract requires.
//!
//! All types are `pub(crate)` because they are implementation detail
//! of the plugin; the admission surface in `lib.rs` does not expose
//! them.

use std::time::Duration;

/// MPD playback state, as reported by the `status` command's
/// `state:` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PlayState {
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
pub(crate) struct MpdVersion {
    /// Major version number.
    pub(crate) major: u32,
    /// Minor version number.
    pub(crate) minor: u32,
    /// Patch version number.
    pub(crate) patch: u32,
}

impl MpdVersion {
    /// Construct a version with the three components.
    pub(crate) fn new(major: u32, minor: u32, patch: u32) -> Self {
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
pub(crate) struct MpdStatus {
    /// Playback state (always present in MPD responses).
    pub(crate) state: PlayState,
    /// Zero-based position of the current song within the queue.
    /// `None` when the queue is empty or nothing is selected.
    pub(crate) song_position: Option<u32>,
    /// Elapsed time within the current song. `None` when the player
    /// is stopped, or when MPD does not report it (some sources omit
    /// elapsed on initial response; this is treated as unknown, not
    /// zero).
    pub(crate) elapsed: Option<Duration>,
    /// Total duration of the current song. `None` when MPD does not
    /// report it (streams, some CD rips).
    pub(crate) duration: Option<Duration>,
    /// Volume level, 0-100. `None` when MPD reports -1 (no mixer
    /// configured) or when the field is absent.
    pub(crate) volume: Option<u8>,
    /// `repeat` mode: when set, MPD restarts the queue from
    /// position 0 after the last song ends. `false` when MPD
    /// omits the field. Captured by the `emit_test_tone`
    /// diagnostic so the operator's prior value is restored
    /// after the tone completes.
    pub(crate) repeat: bool,
    /// `random` mode: when set, MPD plays queue entries in
    /// random order. `false` when MPD omits the field.
    /// Captured then restored by `emit_test_tone` for the
    /// same reason as [`Self::repeat`].
    pub(crate) random: bool,
    /// `single` mode: when set, MPD stops after the current
    /// song instead of advancing. `false` when MPD omits the
    /// field. Captured then restored by `emit_test_tone` for
    /// the same reason as [`Self::repeat`].
    pub(crate) single: bool,
    /// `consume` mode: when set, MPD removes each song from
    /// the queue after it plays. `false` when MPD omits the
    /// field. Captured then restored by `emit_test_tone` for
    /// the same reason as [`Self::repeat`].
    pub(crate) consume: bool,
    /// Inter-song crossfade in seconds; `0` disables. `0`
    /// when MPD omits the field. Captured then restored by
    /// `emit_test_tone` for the same reason as
    /// [`Self::repeat`].
    pub(crate) crossfade_seconds: u32,
}

/// Narrow view of MPD's `currentsong` response.
///
/// Only the fields the playback warden needs today. A richer shape
/// (composer, date, track number, disc number, etc.) lives as a
/// future extension when Phase 3.4's subject assertion demands it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MpdSong {
    /// MPD-relative file path (e.g. `INTERNAL/Artist/Album/track.flac`).
    /// Always present when `currentsong` returns a non-empty response.
    pub(crate) file_path: String,
    /// Track title tag, if present.
    pub(crate) title: Option<String>,
    /// Artist tag, if present (prefers Artist over AlbumArtist; the
    /// warden's subject-assertion logic in Phase 3.4 may walk both).
    pub(crate) artist: Option<String>,
    /// Album tag, if present.
    pub(crate) album: Option<String>,
    /// Track duration from the `duration:` field (MPD 0.21+) or
    /// `Time:` (older).
    pub(crate) duration: Option<Duration>,
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
    pub(crate) codec_name: Option<String>,
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
pub(crate) fn derive_source_codec_name(path: &str) -> Option<String> {
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
pub(crate) enum IdleSubsystem {
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
    pub(crate) fn as_protocol_str(&self) -> &str {
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
    pub(crate) fn from_protocol_str(s: &str) -> Self {
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
