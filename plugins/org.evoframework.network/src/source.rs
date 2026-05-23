// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Link event source surface.
//!
//! The trait + the polling + rtnetlink backend implementations
//! live in the shared `evo-network-state` crate so the framework
//! core's chain-stack supervisor and this plugin consume the
//! same canonical substrate; both wake on the same kernel
//! event with the same trait shape.
//!
//! NetworkManager-specific behaviour stays in this plugin —
//! it carries plugin-tier policy semantics (connectivity
//! verdict reconciliation, captive-portal detection) that the
//! framework core does not depend on.

pub use evo_network_state::{
    polling, LinkEvent, LinkEventSource, LinkSourceCapabilities,
    LinkSourceError,
};

#[cfg(all(feature = "source-rtnetlink", target_os = "linux"))]
pub use evo_network_state::rtnetlink;

// NetworkManager D-Bus event source. Same gating posture as
// the shared rtnetlink backend — feature flag plus
// `target_os = "linux"` cfg, so non-Linux builds of the
// plugin compile without the dependency tree.
#[cfg(all(feature = "source-nm", target_os = "linux"))]
pub mod nm;
