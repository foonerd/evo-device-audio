// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Multi-room domain crate.
//!
//! Hosts the source-host election runtime and the cross-device
//! coordination primitives the framework treats as a plugged-in
//! domain. Consumes framework substrates (`HappeningBus`,
//! `GroupStore`, `PersistenceStore`, `AudioPlaneRuntime`) via
//! the `evo` crate's public types and implements the
//! [`evo_primitives::ElectionState`] trait the framework reads
//! through.
//!
//! ## Architectural line
//!
//! The framework crate (`evo`) MUST NOT depend on this crate in
//! production deps. That one-way edge is what makes the
//! framework / domain separation enforceable at the build
//! system level: the framework compiles without any multi-room
//! code, and the multi-room runtime is wired in by a
//! distribution binary via the
//! [`evo::RuntimeSetup`] callback (see the binary
//! `evo-device-audio-distribution` for the canonical wiring).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod election;
pub mod role_store;
pub mod substrate_adapter;

pub use election::{
    elect_source_host, ElectionConfig, ElectionError, ElectionRuntime,
    SourceHostElection,
};
pub use role_store::RoleStore;
pub use substrate_adapter::MultiroomSubstrateAdapter;

// Re-export the primitives this crate's API uses so a
// distribution importing `evo_multiroom::Role` lands at the
// canonical type without reaching for the primitives crate
// directly.
pub use evo_primitives::{Role, RoleChange, RoleStoreError};
