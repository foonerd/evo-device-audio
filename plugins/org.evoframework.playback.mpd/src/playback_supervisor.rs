//! # Playback supervisor
//!
//! Long-lived orchestrator over two [`crate::mpd::MpdConnection`]
//! instances: the warden's answer to "what actually happens during
//! a custody". Phase 3.2c wired this module into the warden trait
//! impls in `lib.rs`; Phase 3.4 extends it with subject and
//! relation emission so Milestone 4's album-art respondent can
//! walk the resulting graph.
//!
//! ## Layers
//!
//! - [`command`]: the [`PlaybackCommand`] enum (the things the
//!   warden tells the supervisor to do) and the
//!   [`PlaybackError`] hierarchy classifying supervisor failures
//!   for the warden to map onto `PluginError` variants.
//! - [`report`]: the `PlaybackStateReport` struct emitted on
//!   every state transition, plus its hand-rolled TOML serialiser
//!   (no `toml` or `serde` dependency in the critical path).
//!   Internal to the module; not re-exported.
//! - [`subject_emitter`]: [`SubjectEmitter`] bundling the
//!   subject and relation announcer handles and emitting
//!   track/album subjects and the `album_of` relation for a
//!   given `MpdSong`. Best-effort; never fails the supervisor.
//! - [`actor`]: [`SupervisorHandle`] and [`spawn`]. Two tokio
//!   tasks communicate via channels to serve custody commands and
//!   emit state reports; reconnection with bounded exponential
//!   backoff is handled transparently; subject emission piggy-
//!   backs on the state-report flow so fresh emission happens
//!   every time a song change is observed.
//! - `test_mock` (cfg(test) only): shared fixtures used by both
//!   this module's tests and the warden's integration tests.

mod actor;
mod command;
mod report;
mod subject_emitter;

#[cfg(test)]
pub(crate) mod test_mock;

// Public-within-crate surface. `lib.rs` consumes these from
// `crate::playback_supervisor::{...}`. `report` types are not
// re-exported because they are internal helpers used only inside
// the module graph.
pub(crate) use actor::{spawn, SupervisorHandle};
pub(crate) use command::{PlaybackCommand, PlaybackError};
pub(crate) use subject_emitter::SubjectEmitter;
