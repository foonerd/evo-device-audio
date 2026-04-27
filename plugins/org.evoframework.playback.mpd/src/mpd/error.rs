//! MPD connection error hierarchy.
//!
//! Classified by cause so the warden in Phase 3.2 can map failures
//! to `PluginError::{Permanent, Transient, Fatal}` without guessing:
//!
//! - `Transport(_)` is typically transient (reconnect may succeed).
//! - `Protocol(_)` is fatal (server is not speaking MPD correctly).
//! - `Ack { .. }` is command-scoped; depends on the code (2 = not
//!   list, 3 = password, 50 = no exist, etc. - Phase 3.2 decides).
//! - `Timeout { .. }` is transient.
//! - `Config(_)` is permanent (operator supplied a bad endpoint).
//!
//! Every variant carries its underlying source so `tracing` captures
//! the full causal chain via `thiserror`'s `#[source]` wiring.

use std::io;
use std::time::Duration;

/// Top-level error type for MPD connection operations.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MpdError {
    /// Underlying transport failure.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),

    /// Protocol-level failure: malformed frames, unparseable fields,
    /// or the server responded in a way the protocol does not define.
    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),

    /// MPD returned a command-level ACK (the server refused to
    /// execute the command or failed during execution).
    #[error("MPD ACK [{code}@{list_position}] {{{command}}} {message}")]
    Ack {
        /// MPD error code; see MPD's `ack.h` for the canonical list.
        code: u32,
        /// Position within a command list where the error occurred.
        /// Zero for single-command dispatch.
        list_position: u32,
        /// Name of the command that failed.
        command: String,
        /// Human-readable message from MPD.
        message: String,
    },

    /// An operation's deadline was exceeded.
    #[error("timeout: {operation} after {elapsed:?}")]
    Timeout {
        /// Identifier naming which operation timed out. Static string
        /// so structured logs can filter without allocations.
        operation: &'static str,
        /// The budget that was exceeded.
        elapsed: Duration,
    },

    /// Configuration refused the endpoint or option.
    #[error("config: {0}")]
    Config(#[from] ConfigError),
}

/// Transport-level errors (I/O on the underlying socket).
#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    /// Generic I/O error on an established stream.
    #[error("I/O error: {source}")]
    Io {
        /// Underlying `std::io::Error`.
        #[source]
        source: io::Error,
    },

    /// The stream was closed by the peer before the operation could
    /// complete (EOF on read, write on a half-closed stream, etc.).
    #[error("connection closed by MPD")]
    Closed,

    /// TCP connect failed.
    #[error("TCP connect to {endpoint} failed: {source}")]
    TcpConnect {
        /// The `host:port` we tried to reach.
        endpoint: String,
        /// Underlying `std::io::Error`.
        #[source]
        source: io::Error,
    },

    /// Unix socket connect failed.
    #[error("Unix connect to {path} failed: {source}")]
    UnixConnect {
        /// The filesystem path of the socket.
        path: String,
        /// Underlying `std::io::Error`.
        #[source]
        source: io::Error,
    },
}

impl From<io::Error> for TransportError {
    fn from(source: io::Error) -> Self {
        Self::Io { source }
    }
}

/// Protocol-level errors: the server responded but not in a shape the
/// MPD protocol defines.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProtocolError {
    /// The welcome banner did not start with the expected `OK MPD `
    /// prefix.
    #[error("expected welcome banner starting with 'OK MPD ', got: {0:?}")]
    BadWelcome(String),

    /// The welcome banner's version component could not be parsed.
    #[error("unparseable version string in welcome banner: {0:?}")]
    BadVersion(String),

    /// A single protocol line exceeded the configured limit.
    #[error("line too long: {len} bytes exceeds limit {limit}")]
    LineTooLong {
        /// Actual length of the line, in bytes.
        len: usize,
        /// Configured limit.
        limit: usize,
    },

    /// The server sent bytes that are not valid UTF-8. MPD's protocol
    /// is defined over UTF-8; anything else is malformed.
    #[error("non-UTF-8 byte sequence in response ({0} bytes)")]
    NonUtf8(usize),

    /// The stream ended before a response terminator (OK or ACK)
    /// arrived.
    #[error("unterminated response (EOF before OK or ACK)")]
    Unterminated,

    /// A response line claimed to be a key/value but could not be
    /// split on the `: ` separator.
    #[error("malformed key/value line: {0:?}")]
    MalformedKeyValue(String),

    /// An ACK line could not be decomposed into its canonical parts.
    #[error("malformed ACK line: {0:?}")]
    MalformedAck(String),

    /// A field the warden needs could not be parsed as its expected
    /// type (e.g. `volume: abc`).
    #[error("unparseable field {field}: {value:?}")]
    UnparseableField {
        /// Name of the field, static so logs are allocation-free.
        field: &'static str,
        /// The raw value that failed to parse.
        value: String,
    },

    /// A field expected to be present was missing.
    #[error("missing required field: {field}")]
    MissingField {
        /// Name of the field, static so logs are allocation-free.
        field: &'static str,
    },

    /// A `state:` field contained a value other than `play`, `pause`,
    /// or `stop`.
    #[error("unknown play state: {0:?}")]
    UnknownPlayState(String),

    /// A command was composed with a character that cannot be
    /// represented on the wire (newline, CR, NUL).
    #[error("command contains forbidden character: {ch:?}")]
    CommandForbiddenChar {
        /// The offending character.
        ch: char,
    },
}

/// Configuration errors (caught before any I/O is attempted).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// TCP endpoint was supplied with an empty or whitespace-only host.
    #[error("empty host in TCP endpoint")]
    EmptyHost,

    /// Unix endpoint was supplied with an empty path.
    #[error("empty path in Unix endpoint")]
    EmptyPath,
}
