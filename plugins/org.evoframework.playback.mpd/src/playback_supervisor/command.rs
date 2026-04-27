//! Playback commands and their failure classification.
//!
//! The warden receives high-level `CourseCorrection` values from
//! the steward; Phase 3.2c translates those into [`PlaybackCommand`]
//! values and hands them to the supervisor. The command type sits
//! in this crate so the translation layer has a single place to
//! round-trip against.

use std::time::Duration;

/// Commands the playback supervisor executes against MPD.
///
/// Variants map 1:1 to [`MpdConnection`] transport methods. `Clone`
/// because the supervisor's reconnection path needs to retry a
/// command after re-establishing the command connection.
///
/// [`MpdConnection`]: crate::mpd::MpdConnection
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlaybackCommand {
    /// Start or resume playback from the current queue position.
    Play,
    /// Start playback at a specific queue position (zero-based).
    PlayPosition(u32),
    /// Pause (`true`) or resume (`false`) playback.
    Pause(bool),
    /// Stop playback. Position is not preserved.
    Stop,
    /// Skip to the next song in the queue.
    Next,
    /// Skip to the previous song in the queue.
    Previous,
    /// Seek within the current song to an absolute position.
    Seek(Duration),
    /// Set output volume (0-100; MPD ACKs values above 100).
    SetVolume(u8),
}

/// Failure modes of playback command execution.
///
/// Classified so the warden in Phase 3.2c can map cleanly onto
/// `PluginError::{Permanent, Transient, Fatal}` without guessing:
///
/// - [`PlaybackError::Ack`] is command-level: the connection is
///   healthy, the command itself was refused. Phase 3.2c maps to
///   `Permanent` (retrying the same command gets the same ACK).
/// - [`PlaybackError::ConnectionExhausted`] is transient: MPD was
///   unreachable across all reconnection attempts. Phase 3.2c maps
///   to `Transient` so the steward can retry the correction at a
///   higher level.
/// - [`PlaybackError::Protocol`] is fatal at the connection level:
///   the server is not speaking MPD correctly. Phase 3.2c maps to
///   `Fatal`.
/// - [`PlaybackError::Shutdown`] indicates the supervisor is no
///   longer alive. Phase 3.2c maps to `Permanent`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PlaybackError {
    /// MPD rejected the command. Connection remains healthy.
    #[error("MPD rejected command: code {code}, {message}")]
    Ack {
        /// MPD error code; see MPD's `ack.h` for canonical values.
        code: u32,
        /// Human-readable message from MPD.
        message: String,
    },

    /// Connection to MPD could not be established after all
    /// attempts. The supervisor is still alive but the command
    /// could not be delivered.
    #[error(
        "connection to MPD could not be established after {attempts} attempts"
    )]
    ConnectionExhausted {
        /// How many reconnection attempts were made.
        attempts: u32,
    },

    /// MPD's wire responses violated the protocol (malformed
    /// frame, unknown play state, etc.). Not retryable.
    #[error("MPD protocol violation: {0}")]
    Protocol(String),

    /// The supervisor is shutting down or has already shut down.
    /// The command was not executed.
    #[error("supervisor is shutting down")]
    Shutdown,
}
