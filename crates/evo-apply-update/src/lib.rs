// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! `evo-apply-update`: the root helper behind the distribution's in-app
//! updater.
//!
//! Two programs share this library:
//!
//! - `evo-apply-update` is the only thing the service user may run as
//!   root. It parses a closed grammar (`check`, `apply <component>
//!   <version>`, `channel <value>`), and starts one transient instance of
//!   the `evo-apply-update@.service` unit. It does no other work.
//! - `evo-apply-update-worker` is that unit's process. It runs outside the
//!   steward's sandbox, takes its trust only from a root-owned chain, and
//!   installs only what a signed document and a hash chain verified.
//!
//! The caller supplies a component name, a version and a channel value.
//! Every URL, path, key and destination is the worker's own.

pub mod apply_host;
pub mod apply_plugin;
pub mod archive;
pub mod check;
pub mod ctx;
pub mod docs;
pub mod error;
pub mod fetch;
pub mod grammar;
pub mod host;
pub mod request;
pub mod roots;
pub mod state;
pub mod timeutil;
pub mod trust;
pub mod version;
pub mod worker;

#[cfg(test)]
mod flows;
