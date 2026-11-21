// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Asking the household door before a protected change.
//!
//! `library.add_source` and `library.remove_source` are not
//! protected as verbs: a USB source is guarded by the nested
//! `storage.usb.safe_remove`, and any other source was guarded by
//! nothing. At a household level that locks Sources, a caller that
//! never held a sitting could still add or remove a share.
//!
//! The steward answers the question for the consumer this plugin
//! is serving: dispatch [`HOUSEHOLD_REQUIRE`] on [`HOUSEHOLD_SHELF`]
//! with `{"scope": ...}`. It answers `{"household_door": "open"}`
//! or refuses by name (`household_policy_locked`). It changes
//! nothing.
//!
//! The constants repeat `evo_plugin_sdk::contract::HOUSEHOLD_SHELF`
//! and `HOUSEHOLD_REQUIRE`, which the pinned SDK tag does not carry
//! yet. A test reads the framework's own definition when the
//! sibling tree is present and fails if they differ.

/// The shelf the steward answers household questions on.
pub(crate) const HOUSEHOLD_SHELF: &str = "evo.household";
/// The question: may this consumer change what `scope` protects?
pub(crate) const HOUSEHOLD_REQUIRE: &str = "household.require";
/// The scope the household groups Network, File-sharing and
/// Sources all map to (`household_groups.rs`), and the scope the
/// share verbs of `network.shares` declare.
pub(crate) const SOURCES_SCOPE: &str = "network_admin";

#[cfg(test)]
mod tests {
    use super::*;

    /// The values must be the framework's. Read from the sibling
    /// `evo-core-eng` tree when it is there; a published tree has
    /// no sibling and the check is skipped, loudly.
    #[test]
    fn the_constants_are_the_frameworks() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../evo-core-eng/crates/evo-plugin-sdk/src/contract/shelf_dispatch.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!(
                "household_door: {} absent; SDK constants not compared",
                path.display()
            );
            return;
        };
        if !src.contains("HOUSEHOLD_SHELF") {
            eprintln!("household_door: sibling SDK predates the door; skipped");
            return;
        }
        assert!(
            src.contains(&format!(
                "HOUSEHOLD_SHELF: &str = \"{HOUSEHOLD_SHELF}\""
            )),
            "HOUSEHOLD_SHELF drifted from the SDK"
        );
        assert!(
            src.contains(&format!(
                "HOUSEHOLD_REQUIRE: &str = \"{HOUSEHOLD_REQUIRE}\""
            )),
            "HOUSEHOLD_REQUIRE drifted from the SDK"
        );
    }
}
