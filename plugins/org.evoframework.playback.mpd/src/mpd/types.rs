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
/// MPD reports (xfade, mixrampdb, audio, etc.) are intentionally
/// dropped rather than surfaced: the connection layer's surface grows
/// by explicit opt-in, not by accumulating every tag MPD emits.
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
}
