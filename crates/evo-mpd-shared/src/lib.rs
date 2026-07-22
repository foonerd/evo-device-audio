// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! # Shared MPD wire-protocol client for the evo audio distribution
//!
//! Single implementation of the MPD client — every plugin in the
//! audio reference distribution that needs to talk to MPD
//! consumes this crate. `playback.mpd` is the primary user (owns
//! the operational surface for library / queue / playback /
//! favourites / etc.); `metadata.online` uses the `list`
//! operations for the recording-type facet browse verb.
//!
//! Extraction rationale: previously the MPD client lived as a
//! private module inside `playback.mpd`, and `metadata.online`
//! shipped a parallel `MinimalMpd` type as a temporary
//! duplication. That path violated the "one implementation, no
//! parallel truths" pin. This crate is the resolution — both
//! plugins now consume the same client. The `metadata.online`
//! composition remains where it was (that plugin owns the
//! reconciliation cache); the retirement item for moving
//! `library.browse_by_recording_type` to `playback.mpd` is
//! tracked separately.
//!
//! ## Design
//!
//! The module stack (lifted verbatim from the original private
//! module):
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
//!   no I/O, no time, no async — unit-testable against exact byte
//!   strings.
//! - [`framing`]: line-based reader/writer over arbitrary async
//!   byte streams, with mandatory timeouts and a hard line-length
//!   limit. Transport-agnostic: TCP, Unix, and in-memory duplex
//!   streams all work.
//! - [`connection`]: the operational client — [`MpdConnection`]
//!   composes framing over TCP/Unix and exposes typed verbs.
//!
//! No third-party MPD crate dependency; owns the wire protocol
//! end-to-end. Only tokio, tracing, thiserror.

pub mod connection;
pub mod endpoint;
pub mod error;
pub mod framing;
pub mod protocol;
pub mod types;

pub use connection::{ConnectTimeouts, MpdConnection};
pub use endpoint::MpdEndpoint;
pub use error::MpdError;
pub use types::{
    MpdLibraryEntry, MpdMount, MpdSearchField, MpdStats, MpdStatus,
};
