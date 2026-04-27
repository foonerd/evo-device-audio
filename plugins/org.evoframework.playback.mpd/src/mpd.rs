//! # MPD connection layer
//!
//! Private implementation module for the MPD playback warden. Owns
//! the MPD wire protocol end-to-end: the implementation does not
//! depend on any third-party MPD crate, so the critical-path
//! dependency surface is bounded to crates the showcase fully
//! vendors and audits (tokio, tracing, thiserror).
//!
//! ## Design
//!
//! The module is structured as a short stack, each layer
//! responsible for one concern:
//!
//! - [`types`]: domain types (play state, version, narrow status
//!   and song shapes, idle subsystems). No I/O, no parsing.
//! - [`error`]: classified error hierarchy. Every variant carries
//!   its underlying source through `#[source]` so `tracing`
//!   captures full causal chains.
//! - [`endpoint`]: server address type (TCP or Unix). Validates at
//!   construction; cannot represent an invalid endpoint.
//! - [`protocol`]: wire-format serialisation (commands out) and
//!   parsing (fields, OK/ACK terminators, welcome banner). Pure,
//!   no I/O, no time, no async - unit-testable against exact byte
//!   strings.
//! - [`framing`]: line-based reader/writer over arbitrary async
//!   byte streams, with mandatory timeouts and a hard line-length
//!   limit. Transport-agnostic: TCP, Unix, and in-memory duplex
//!   streams all work.
//! - [`connection`]: ties it together. Opens the transport, reads
//!   the welcome banner, dispatches commands with timeout budgets,
//!   projects protocol fields into the narrow domain types. Phase
//!   3.2a added transport commands (play, pause, stop, next,
//!   previous, seek, set_volume) and the `idle` subprotocol.
//!
//! ## Scope and consumption
//!
//! Phase 3.1 delivered the protocol stack and status / currentsong.
//! Phase 3.2a added transport commands and the idle subprotocol.
//! Phase 3.2b built the [`crate::playback_supervisor`] that
//! orchestrates two connections (one for commands, one for idle).
//! Phase 3.2c wired the supervisor into the warden trait impls in
//! `crate` root; the `unused_imports` suppression that guarded the
//! declared-but-unconsumed re-exports during 3.1-3.2b is retired
//! as part of 3.2c.
//!
//! Phase 3.3 adds the configuration layer that produces the
//! [`endpoint::MpdEndpoint`] the connection opens. Phase 3.4 uses
//! the parsed [`types::MpdSong`] to assert `track` and `album`
//! subjects for Milestone 4's album-art respondent to walk.
//!
//! ## dead_code suppression
//!
//! The module retains `#![allow(dead_code)]` for items that are
//! part of the connection-layer contract but not exercised by the
//! current consumers. These include:
//!
//! - `MpdConnection::version`, `::endpoint`, `::connected_at`,
//!   `::ping` - accessor / liveness helpers referenced only by
//!   tests today.
//! - `IdleSubsystem` variants the supervisor does not yet
//!   subscribe to (database, update, stored_playlist, output,
//!   partition, sticker, subscription, message, neighbor, mount);
//!   retained so a future subscription change does not need a
//!   round-trip through the type definition.
//! - Error sub-types used only for `#[from]` construction inside
//!   the module.
//!
//! Removing the module-level attribute requires a per-item
//! `#[cfg(test)]` or `#[allow(dead_code)]` audit and is deferred
//! to a later housekeeping commit.

#![allow(dead_code)]

mod connection;
mod endpoint;
mod error;
mod framing;
mod protocol;
mod types;

// Public surface within the crate. Consumed by `lib.rs`,
// `playback_supervisor::actor`, and `playback_supervisor::report`.
// Items re-exported here are all used via
// `crate::mpd::{...}`; unused re-exports would trip the default
// unused_imports lint and block the build.

pub(crate) use connection::{ConnectTimeouts, MpdConnection};
pub(crate) use endpoint::MpdEndpoint;
pub(crate) use error::MpdError;
pub(crate) use types::{IdleSubsystem, MpdSong, MpdStatus, PlayState};
