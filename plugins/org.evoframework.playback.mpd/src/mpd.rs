// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! # MPD connection layer — re-exported from the shared crate
//!
//! Previously a private module owning the MPD wire protocol
//! implementation. Extracted into the workspace crate
//! `evo-mpd-shared` so `metadata.online`'s
//! `library.browse_by_recording_type` verb consumes the same
//! implementation rather than shipping a parallel `MinimalMpd`
//! type.
//!
//! This module is now a thin re-exporter — every existing
//! `use crate::mpd::...` import in this plugin resolves against
//! the shared crate's public surface. Behaviour unchanged; the
//! single-implementation discipline is now enforced by the
//! type system (both plugins use the same `MpdConnection`).

// Re-export every public item playback.mpd's other modules
// consume via `use crate::mpd::...`. Types + connection API +
// the codec-name helper the currentsong parser uses.
pub use evo_mpd_shared::connection::*;
pub use evo_mpd_shared::endpoint::*;
pub use evo_mpd_shared::error::*;
pub use evo_mpd_shared::types::*;
