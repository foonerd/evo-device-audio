// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! This distribution's household-protection group table.
//!
//! The framework owns the protection mechanism entire — the policy
//! object, the level ladder, persistence, the two wire ops, the
//! change happening, the origin-aware stamp and the dispatch gate.
//! It deliberately owns no group names: "file sharing" and
//! "metadata" are this product's vocabulary, and compiling them
//! into the steward would fail `BOUNDARY.md` §5's own test — would
//! this still make sense in an `evo-device-<non-audio>` repository?
//! A lighting distribution has no file-sharing group.
//!
//! So the names live here, on the `HouseholdGroupTable` seam
//! (`BOUNDARY.md` §3, third family, alongside `RtcWakeCallback` and
//! `HttpsSetup`). One table, owned by the distribution. Plugins
//! never register groups — a plugin that wanted one would be
//! inventing policy the operator never chose.
//!
//! The steward asks this exactly one question at dispatch: is scope
//! `S` protected under the policy in force. It stores only the
//! opaque group keys the operator marked, and learns nothing else
//! from what is written here.

use evo::household_protection::{HouseholdGroupTable, ProtectionLevel};

/// Group keys, in the order the operator surface offers them.
const NETWORK: &str = "network";
const FILE_SHARING: &str = "file-sharing";
const SOURCES: &str = "sources";
const SYSTEM: &str = "system";
const METADATA: &str = "metadata";
const SMART_HOME: &str = "smart-home";

/// Scope names. These are capability scopes the framework already
/// knows from the operator bootstrap set plus this distribution's
/// own; the mapping from a Settings group to them is what this file
/// exists to state.
const NETWORK_ADMIN: &str = "network_admin";
const SYSTEM_ADMIN: &str = "system_admin";
const ONLINE_PROVIDERS: &str = "online_providers";
const CREDENTIALS: &str = "credentials";

/// The audio appliance's Settings groups.
///
/// Three methods, no state. Anything more would be a second policy
/// object competing with the one the steward persists.
#[derive(Debug, Default, Clone, Copy)]
pub struct AudioHouseholdGroups;

impl HouseholdGroupTable for AudioHouseholdGroups {
    fn group_keys(&self) -> Vec<String> {
        vec![
            NETWORK.to_owned(),
            FILE_SHARING.to_owned(),
            SOURCES.to_owned(),
            SYSTEM.to_owned(),
            METADATA.to_owned(),
            SMART_HOME.to_owned(),
        ]
    }

    fn scopes_for_group(&self, key: &str) -> Vec<String> {
        match key {
            // Wi-Fi, AP, mounts, and the share surfaces that ride
            // the same privileged writer.
            NETWORK | FILE_SHARING | SOURCES => {
                vec![NETWORK_ADMIN.to_owned()]
            }
            // Display and panel writers: cursor, on-screen keyboard,
            // rotation, brightness. Smart-home rides the same scope
            // today; it is a separate group because the operator
            // thinks of them separately, and the steward does not
            // care that two keys resolve alike.
            SYSTEM | SMART_HOME => vec![SYSTEM_ADMIN.to_owned()],
            // Online providers and the credential writers behind
            // them. Last.fm keys ride `credential_put`
            // (`write:credentials`); provider toggles ride
            // `online_providers`. Both must sit in this group or
            // Play only marks Metadata and the key writer stays
            // open.
            METADATA => {
                vec![ONLINE_PROVIDERS.to_owned(), CREDENTIALS.to_owned()]
            }
            // An unknown key — a stale mark left by an older
            // release, say — resolves to nothing rather than
            // panicking the gate.
            _ => Vec::new(),
        }
    }

    fn groups_for_level(&self, level: ProtectionLevel) -> Vec<String> {
        match level {
            // Home player. Anyone here can play and change everyday
            // settings.
            ProtectionLevel::Open => Vec::new(),
            // Play and everyday settings; networking and sharing
            // stay with the owner.
            ProtectionLevel::Low => vec![
                NETWORK.to_owned(),
                FILE_SHARING.to_owned(),
                SOURCES.to_owned(),
            ],
            // Play and listen; settings stay with the owner.
            ProtectionLevel::Standard => vec![
                NETWORK.to_owned(),
                FILE_SHARING.to_owned(),
                SOURCES.to_owned(),
                SYSTEM.to_owned(),
                METADATA.to_owned(),
                SMART_HOME.to_owned(),
            ],
            // Play only. The wildcard expands against `group_keys`,
            // so a group added here later is covered without anyone
            // remembering to restate this arm.
            ProtectionLevel::Strict => {
                vec![evo::household_protection::ALL_GROUPS.to_owned()]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_keys_are_the_six_the_operator_surface_offers() {
        assert_eq!(
            AudioHouseholdGroups.group_keys(),
            vec![
                "network",
                "file-sharing",
                "sources",
                "system",
                "metadata",
                "smart-home"
            ]
        );
    }

    #[test]
    fn every_group_key_resolves_to_at_least_one_scope() {
        // A group that resolves to nothing would be a mark the
        // operator can set and that protects nothing — the same lie
        // as a lend with no floor.
        for key in AudioHouseholdGroups.group_keys() {
            assert!(
                !AudioHouseholdGroups.scopes_for_group(&key).is_empty(),
                "group {key} resolves to no scope"
            );
        }
    }

    #[test]
    fn network_sharing_and_sources_ride_the_network_writer() {
        for key in ["network", "file-sharing", "sources"] {
            assert_eq!(
                AudioHouseholdGroups.scopes_for_group(key),
                vec!["network_admin"]
            );
        }
    }

    #[test]
    fn system_and_smart_home_ride_the_system_writer() {
        for key in ["system", "smart-home"] {
            assert_eq!(
                AudioHouseholdGroups.scopes_for_group(key),
                vec!["system_admin"]
            );
        }
    }

    #[test]
    fn metadata_rides_online_providers_and_the_credential_writer() {
        assert_eq!(
            AudioHouseholdGroups.scopes_for_group("metadata"),
            vec!["online_providers", "credentials"]
        );
    }

    #[test]
    fn metadata_scopes_follow_the_ladder() {
        let table: evo::household_protection::GroupTable =
            std::sync::Arc::new(AudioHouseholdGroups);
        let boot = evo::https_boot::operator_bootstrap_capability_set();
        let protected =
            |policy: &evo::household_protection::HouseholdProtectionPolicy| {
                (
                    evo::household_protection::is_scope_protected(
                        policy,
                        Some(&table),
                        &boot,
                        "online_providers",
                    ),
                    evo::household_protection::is_scope_protected(
                        policy,
                        Some(&table),
                        &boot,
                        "credentials",
                    ),
                )
            };
        let at = |level: ProtectionLevel, groups: Vec<String>| {
            evo::household_protection::HouseholdProtectionPolicy {
                chosen: true,
                level,
                protected_groups: groups,
                ..Default::default()
            }
        };

        assert_eq!(
            protected(&at(ProtectionLevel::Open, vec![METADATA.to_owned()])),
            (false, false),
            "open protects nothing, marks included"
        );
        assert_eq!(
            protected(&at(
                ProtectionLevel::Low,
                AudioHouseholdGroups.groups_for_level(ProtectionLevel::Low)
            )),
            (false, false),
            "low protects networking, not metadata"
        );
        for level in [ProtectionLevel::Standard, ProtectionLevel::Strict] {
            assert_eq!(
                protected(&at(
                    level,
                    AudioHouseholdGroups.groups_for_level(level)
                )),
                (true, true),
                "{level:?} must protect both metadata writers"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_inert_not_fatal() {
        assert!(AudioHouseholdGroups
            .scopes_for_group("gone-in-an-upgrade")
            .is_empty());
    }

    #[test]
    fn level_defaults_match_the_frozen_ladder() {
        assert!(AudioHouseholdGroups
            .groups_for_level(ProtectionLevel::Open)
            .is_empty());
        assert_eq!(
            AudioHouseholdGroups.groups_for_level(ProtectionLevel::Low),
            vec!["network", "file-sharing", "sources"]
        );
        assert_eq!(
            AudioHouseholdGroups.groups_for_level(ProtectionLevel::Standard),
            vec![
                "network",
                "file-sharing",
                "sources",
                "system",
                "metadata",
                "smart-home"
            ]
        );
        assert_eq!(
            AudioHouseholdGroups.groups_for_level(ProtectionLevel::Strict),
            vec![evo::household_protection::ALL_GROUPS]
        );
    }

    #[test]
    fn strict_expands_to_every_group_through_the_framework() {
        // The wildcard is expanded by the steward against
        // `group_keys`, so strict covers a group added later without
        // this arm being touched.
        let policy = evo::household_protection::HouseholdProtectionPolicy {
            chosen: true,
            level: ProtectionLevel::Strict,
            ..Default::default()
        };
        let table: evo::household_protection::GroupTable =
            std::sync::Arc::new(AudioHouseholdGroups);
        let scopes = evo::household_protection::protected_scopes(
            &policy,
            Some(&table),
            &evo::https_boot::operator_bootstrap_capability_set(),
        );
        for expected in [
            "network_admin",
            "system_admin",
            "online_providers",
            "credentials",
        ] {
            assert!(
                scopes.contains(expected),
                "strict must protect {expected}"
            );
        }
    }

    /// The other half of the wizard-gate proof.
    ///
    /// The `system.kiosk` shelf declares every write at one scope
    /// — `system_admin` — and the plugin crate pins that with
    /// `every_write_on_this_shelf_rides_the_one_scope`. This
    /// asserts the half that lives here: that scope sits inside
    /// the `system` group, so an operator who protects system
    /// settings protects the whole shelf, including the touch
    /// wizard's `derive_touch_calibration_from_corners`. The
    /// framework then refuses it at dispatch with
    /// `household_policy_locked`, exactly as it refuses
    /// `set_touch_calibration`.
    ///
    /// If a future verb on that shelf were declared at another
    /// scope it would escape this group; the plugin-side test is
    /// what catches that, and this is the pointer to it.
    #[test]
    fn the_system_group_protects_every_system_kiosk_write() {
        const SHELF_WRITE_SCOPE: &str = "system_admin";
        let table: evo::household_protection::GroupTable =
            std::sync::Arc::new(AudioHouseholdGroups);
        let boot = evo::https_boot::operator_bootstrap_capability_set();
        let protected =
            |policy: &evo::household_protection::HouseholdProtectionPolicy| {
                evo::household_protection::is_scope_protected(
                    policy,
                    Some(&table),
                    &boot,
                    SHELF_WRITE_SCOPE,
                )
            };
        let at = |level: ProtectionLevel, groups: Vec<String>| {
            evo::household_protection::HouseholdProtectionPolicy {
                chosen: true,
                level,
                protected_groups: groups,
                ..Default::default()
            }
        };

        assert_eq!(
            AudioHouseholdGroups.scopes_for_group(SYSTEM),
            vec![SHELF_WRITE_SCOPE],
            "the system group is what carries the kiosk shelf"
        );

        // Marked explicitly by the operator.
        assert!(
            protected(&at(ProtectionLevel::Standard, vec![SYSTEM.to_owned()])),
            "an explicit system mark must protect the kiosk shelf"
        );

        // And by the ladder's own defaults, from standard upward.
        for level in [ProtectionLevel::Standard, ProtectionLevel::Strict] {
            assert!(
                protected(&at(
                    level,
                    AudioHouseholdGroups.groups_for_level(level)
                )),
                "{level:?} must protect the kiosk shelf"
            );
        }

        // Controls - without these the assertions above prove
        // nothing. Open protects nothing at all (marks included:
        // the framework short-circuits an unlent Open), and low
        // protects the network groups but deliberately not this
        // one, so the operator can still align their own screen.
        assert!(
            !protected(&at(ProtectionLevel::Open, vec![SYSTEM.to_owned()])),
            "open is the home player: nothing is protected"
        );
        assert!(
            !protected(&at(
                ProtectionLevel::Low,
                AudioHouseholdGroups.groups_for_level(ProtectionLevel::Low)
            )),
            "low protects networking, not the screen"
        );
    }

    #[test]
    fn playback_is_never_protected_at_any_level() {
        // No level on this table may reach the transport scopes.
        // "Play only" is the floor, not a state the box can be
        // argued out of.
        let table: evo::household_protection::GroupTable =
            std::sync::Arc::new(AudioHouseholdGroups);
        let boot = evo::https_boot::operator_bootstrap_capability_set();
        for level in [
            ProtectionLevel::Open,
            ProtectionLevel::Low,
            ProtectionLevel::Standard,
            ProtectionLevel::Strict,
        ] {
            let policy = evo::household_protection::HouseholdProtectionPolicy {
                chosen: true,
                level,
                ..Default::default()
            };
            let scopes = evo::household_protection::protected_scopes(
                &policy,
                Some(&table),
                &boot,
            );
            for transport in ["audio", "subjects", "request"] {
                assert!(
                    !scopes.contains(transport),
                    "{level:?} protected transport scope {transport}"
                );
            }
        }
    }
}

#[cfg(test)]
mod catalog_shape {
    use super::*;

    /// What `household_protection_get` puts on the wire when this
    /// binary is the steward: the six group ids and the four level
    /// defaults, assembled by the framework from this table.
    #[test]
    fn get_snapshot_carries_the_frozen_catalog() {
        let table: evo::household_protection::GroupTable =
            std::sync::Arc::new(AudioHouseholdGroups);
        let snapshot = evo::household_protection::snapshot(
            &evo::household_protection::HouseholdProtectionPolicy::default(),
            Some(&table),
        );
        let wire = serde_json::to_value(&snapshot).unwrap();

        let groups: Vec<&str> = wire["catalog"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            groups,
            vec![
                "network",
                "file-sharing",
                "sources",
                "system",
                "metadata",
                "smart-home"
            ]
        );

        let levels: Vec<(&str, Vec<&str>)> = wire["catalog"]["levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["id"].as_str().unwrap(),
                    l["default_groups"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|g| g.as_str().unwrap())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            levels,
            vec![
                ("open", vec![]),
                ("low", vec!["network", "file-sharing", "sources"]),
                (
                    "standard",
                    vec![
                        "network",
                        "file-sharing",
                        "sources",
                        "system",
                        "metadata",
                        "smart-home"
                    ]
                ),
                ("strict", vec!["*"]),
            ]
        );

        // Scope mapping travels with the catalog, so the surface
        // paints marks without a second map.
        assert_eq!(
            wire["catalog"]["groups"][4]["scopes"],
            serde_json::json!(["online_providers", "credentials"])
        );
    }
}
