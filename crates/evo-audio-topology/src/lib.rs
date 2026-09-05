// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Audio topology, routing, operator policy and chain scoring.
//!
//! This is the audio product plane. It decides where volume
//! scaling lives, whether a chain is bit-perfect, what an
//! implicit resampler costs, and which endpoints a plugin is
//! handed. None of that means anything on a device that does not
//! move audio, which is why it ships with the distribution rather
//! than the framework.
//!
//! The steward keeps the wire ops that report on this, and their
//! response types with them. It stores what it is given and
//! returns it; it does not score.

pub mod audio_policy;
pub mod audio_routing;
pub mod audio_topology;
pub mod control;
pub mod topology_scoring;
