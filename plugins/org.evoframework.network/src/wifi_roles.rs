// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Multi-radio role assignment for the network plugin.
//!
//! A device may expose multiple Wi-Fi radios — on-SoC, PCIe, USB —
//! each with different band capabilities (2.4 GHz only / dual-band
//! / WiFi 6E tri-band) and stability characteristics (USB dongles
//! are notoriously unstable as long-running AP-hosts). The runtime
//! supervisor needs to pick **which radio bears STA traffic** and
//! **which radio bears the AP / management surface** based on:
//!
//! 1. The operator's explicit intent — `wifi.ifname` and
//!    `fallback.hotspot_ifname` win if set (operator override).
//! 2. The operator's `radio_policy.band_priority` — the ordered
//!    list of preferred bands for STA traffic (default
//!    `6 GHz → 5 GHz → 2.4 GHz`).
//! 3. PHY capability — concurrent `managed + AP` support drives
//!    same-PHY virtual-vif assignment; multi-band PHYs cover both
//!    STA and AP duties when only one radio is present.
//! 4. Connection-class quirks — USB radios prefer STA-only when an
//!    on-SoC / PCIe alternative exists for AP duty.
//!
//! The role assigner is **pure**: takes the inventory + operator
//! intent, returns an assignment. I/O happens upstream in the
//! plugin's `enumerate_wifi_radios` method, which feeds this
//! module from the `wifi_phy` substrate.

use crate::wifi_phy::{PhyBandSupport, PhyCapability, WifiDev};

/// Connection class for one Wi-Fi radio. Derived from the
/// netdev's `/sys/class/net/<if>/device` symlink — `usb` for USB
/// dongles, anything else collapses to `Onboard`. The class
/// influences role assignment: when both Onboard and USB radios
/// are present, USB is preferred for AP (it can be reset without
/// disturbing the on-SoC link) — but if the USB is the only
/// dual-band radio, STA wins on it instead per the band-priority
/// rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionClass {
    /// Anything not classified as USB — PCIe, on-SoC, SDIO,
    /// integrated.
    #[default]
    Onboard,
    /// USB-attached radio. Detected via the
    /// `/sys/class/net/<if>/device/uevent` `DEVTYPE=usb_*` line.
    Usb,
}

/// Inventory row for one Wi-Fi radio. The assigner reads these
/// to pick STA and AP duties.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WifiRadio {
    /// Kernel netdev name (`wlan0`, `wlan1`, ...).
    pub ifname: String,
    /// Backing PHY (`phy0`, `phy1`, ...). Multiple ifnames share
    /// one PHY when concurrent-vif mode is active.
    pub phy: String,
    /// PHY capability summary (concurrent `managed + AP`, band
    /// support, supported interface modes).
    pub capability: PhyCapability,
    /// Connection class (USB vs onboard / PCIe / on-SoC).
    pub connection_class: ConnectionClass,
    /// `true` when this row corresponds to a virtual AP vif
    /// created via `iw … type __ap` (current `iftype` is `AP`).
    /// Such rows are excluded from STA assignment.
    pub is_ap_vif: bool,
}

impl WifiRadio {
    /// Construct an inventory row from raw `iw dev` data + a
    /// resolved PHY capability summary. `connection_class` is
    /// supplied by the I/O layer (sysfs read).
    pub fn from_dev_and_capability(
        dev: &WifiDev,
        capability: PhyCapability,
        connection_class: ConnectionClass,
    ) -> Self {
        let is_ap_vif = dev.iftype.eq_ignore_ascii_case("ap")
            || dev.iftype.to_ascii_lowercase().contains("__ap");
        Self {
            ifname: dev.ifname.clone(),
            phy: dev.phy.clone(),
            capability,
            connection_class,
            is_ap_vif,
        }
    }

    /// `true` when the PHY's supported-interface-modes list
    /// contains `managed` (STA-capable). Used to filter the
    /// candidate set for STA duty.
    pub fn sta_capable(&self) -> bool {
        self.capability
            .interface_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case("managed"))
    }

    /// `true` when the PHY's supported-interface-modes list
    /// contains `AP`. Used to filter the candidate set for AP
    /// duty.
    pub fn ap_capable(&self) -> bool {
        self.capability
            .interface_modes
            .iter()
            .any(|m| m.eq_ignore_ascii_case("ap"))
    }
}

/// Band ranking for STA assignment. Higher index = lower priority.
/// Derived from `RadioPolicy.band_priority`; defaulted to
/// `[6 GHz, 5 GHz, 2.4 GHz]` when the operator did not set one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandClass {
    /// 6 GHz family — WiFi 6E and beyond.
    Ghz6,
    /// 5 GHz family — WiFi 5 / 6 mainstream.
    Ghz5,
    /// 2.4 GHz family — legacy reach, congested.
    Ghz2_4,
}

impl BandClass {
    /// `true` when the PHY supports this band.
    pub fn supported_by(&self, bands: &PhyBandSupport) -> bool {
        match self {
            BandClass::Ghz6 => bands.ghz_6,
            BandClass::Ghz5 => bands.ghz_5,
            BandClass::Ghz2_4 => bands.ghz_2_4,
        }
    }
}

/// Outcome of role assignment. `same_iface` is `true` when STA
/// and AP land on the same kernel netdev (single-PHY single-mode);
/// callers detect concurrent-vif mode by checking
/// `sta_ifname != ap_ifname && concurrent_capable_phy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAssignment {
    /// Interface chosen to bear STA traffic.
    pub sta_ifname: String,
    /// Interface chosen to bear AP / hotspot duty.
    pub ap_ifname: String,
    /// `true` when STA and AP collide on the same ifname.
    pub same_iface: bool,
    /// `true` when the AP interface was pinned by the operator's
    /// explicit `fallback.hotspot_ifname` override. The apply
    /// pipeline reads this to decide whether to auto-promote
    /// same-PHY pairings to a virtual `ap0` vif (only when the
    /// operator did not pin) and whether to auto-adopt the STA's
    /// channel for the AP (same condition).
    pub had_explicit_ap: bool,
    /// Operator-readable rationale for the chosen pairing.
    pub rationale: String,
}

/// Operator override hints honoured by [`assign_wifi_roles`].
/// Mirrors the `wifi.ifname` and `fallback.hotspot_ifname` fields
/// from `NetworkIntent` so the assigner does not need to depend
/// on the intent struct directly (keeps this module pure).
#[derive(Debug, Default)]
pub struct RoleOverrides<'a> {
    /// Explicit STA ifname from `wifi.ifname`. Empty string =
    /// auto-resolve.
    pub explicit_sta: &'a str,
    /// Explicit AP ifname from `fallback.hotspot_ifname`. Empty
    /// string = auto-resolve.
    pub explicit_ap: &'a str,
    /// Default STA ifname fallback when the inventory is empty
    /// (matches the plugin's prior `effective_wifi_ifname`
    /// fallback to `wlan0`).
    pub default_sta_fallback: &'a str,
}

/// Compute STA + AP role assignment from an inventory and the
/// operator's band priority. Pure function — the I/O layer feeds
/// the inventory; this module returns the resolution.
pub fn assign_wifi_roles(
    inventory: &[WifiRadio],
    band_priority: &[BandClass],
    overrides: RoleOverrides<'_>,
) -> RoleAssignment {
    // Explicit operator override — both legs.
    let explicit_sta = overrides.explicit_sta.trim();
    let explicit_ap = overrides.explicit_ap.trim();
    if !explicit_sta.is_empty() && !explicit_ap.is_empty() {
        let same = explicit_sta == explicit_ap;
        return RoleAssignment {
            sta_ifname: explicit_sta.to_string(),
            ap_ifname: explicit_ap.to_string(),
            same_iface: same,
            had_explicit_ap: true,
            rationale: if same {
                "operator pinned both legs to one iface".to_string()
            } else {
                "operator pinned STA + AP explicitly (split-radio)".to_string()
            },
        };
    }

    // Filter candidate sets. AP vifs (already-virtual) never bear
    // STA duty; the auto-assigner picks the underlying physical
    // radio for STA and lets the apply pipeline (re)create the
    // vif on demand.
    let sta_candidates: Vec<&WifiRadio> = inventory
        .iter()
        .filter(|r| r.sta_capable() && !r.is_ap_vif)
        .collect();
    let ap_candidates: Vec<&WifiRadio> =
        inventory.iter().filter(|r| r.ap_capable()).collect();

    // Empty inventory: fall back to the default STA ifname; AP
    // collides on the same iface. The caller's apply pipeline
    // handles the "no PHY found" diagnostic.
    if sta_candidates.is_empty() {
        let fallback = if !overrides.default_sta_fallback.is_empty() {
            overrides.default_sta_fallback.to_string()
        } else {
            "wlan0".to_string()
        };
        let chosen_ap = if !explicit_ap.is_empty() {
            explicit_ap.to_string()
        } else {
            fallback.clone()
        };
        let same = chosen_ap == fallback;
        return RoleAssignment {
            sta_ifname: fallback,
            ap_ifname: chosen_ap,
            same_iface: same,
            had_explicit_ap: !explicit_ap.is_empty(),
            rationale: "no STA-capable radio in inventory; falling back to \
                       default ifname (apply pipeline will surface the gap)"
                .to_string(),
        };
    }

    // STA pick: respect explicit override; otherwise rank by
    // band priority + connection class. Higher band priority
    // wins; ties broken by onboard > USB (USB dongles freed for
    // AP duty when a stable on-SoC alternative exists).
    let chosen_sta = if !explicit_sta.is_empty() {
        sta_candidates
            .iter()
            .find(|r| r.ifname == explicit_sta)
            .copied()
            .unwrap_or(sta_candidates[0])
    } else {
        rank_for_sta(&sta_candidates, band_priority)
    };

    // AP pick: respect explicit override; otherwise pick a
    // different radio when one exists and is AP-capable;
    // otherwise share the STA radio.
    let chosen_ap = if !explicit_ap.is_empty() {
        ap_candidates
            .iter()
            .find(|r| r.ifname == explicit_ap)
            .copied()
            .unwrap_or(chosen_sta)
    } else {
        let split = ap_candidates
            .iter()
            .find(|r| r.ifname != chosen_sta.ifname)
            .copied();
        split.unwrap_or(chosen_sta)
    };

    let same_iface = chosen_sta.ifname == chosen_ap.ifname;
    let rationale = if explicit_sta.is_empty() && explicit_ap.is_empty() {
        if same_iface {
            format!(
                "auto-assigned: only one radio ({}, phy {}); STA + AP share \
                 — concurrent-vif if PHY supports, else single-mode",
                chosen_sta.ifname, chosen_sta.phy,
            )
        } else {
            format!(
                "auto-assigned: STA on {} (phy {}, class {:?}), AP on {} \
                 (phy {}, class {:?}) — split-radio by band priority + \
                 connection class",
                chosen_sta.ifname,
                chosen_sta.phy,
                chosen_sta.connection_class,
                chosen_ap.ifname,
                chosen_ap.phy,
                chosen_ap.connection_class,
            )
        }
    } else if !explicit_sta.is_empty() {
        format!(
            "STA pinned to {} by operator; AP auto-assigned to {}",
            chosen_sta.ifname, chosen_ap.ifname,
        )
    } else {
        format!(
            "AP pinned to {} by operator; STA auto-assigned to {}",
            chosen_ap.ifname, chosen_sta.ifname,
        )
    };

    RoleAssignment {
        sta_ifname: chosen_sta.ifname.clone(),
        ap_ifname: chosen_ap.ifname.clone(),
        same_iface,
        had_explicit_ap: !explicit_ap.is_empty(),
        rationale,
    }
}

/// Score-and-pick the best STA candidate. Higher score wins;
/// ties broken by inventory order (which mirrors `iw dev`
/// enumeration order, typically phy index ascending).
fn rank_for_sta<'a>(
    candidates: &[&'a WifiRadio],
    band_priority: &[BandClass],
) -> &'a WifiRadio {
    candidates
        .iter()
        .max_by_key(|r| sta_score(r, band_priority))
        .copied()
        .expect("rank_for_sta requires a non-empty candidate set")
}

fn sta_score(radio: &WifiRadio, band_priority: &[BandClass]) -> u32 {
    // Band-priority score: the highest-priority band that the
    // PHY supports contributes the bulk of the score.
    let mut band_score: u32 = 0;
    for (idx, band) in band_priority.iter().enumerate() {
        if band.supported_by(&radio.capability.bands) {
            // First match (highest priority) dominates: 1000 for
            // index 0, 100 for index 1, 10 for index 2, etc.
            band_score = 10u32.pow(3u32.saturating_sub(idx as u32));
            break;
        }
    }
    // Connection-class tie-breaker: onboard radios are stabler
    // long-running STAs than USB dongles, so onboard wins ties.
    let class_score = match radio.connection_class {
        ConnectionClass::Onboard => 2,
        ConnectionClass::Usb => 1,
    };
    // Multi-band PHYs are more flexible than single-band even
    // when both happen to match the same top-priority band, so
    // give a small bonus for `band_count > 1`.
    let multi_band_bonus = if radio.capability.bands.band_count() > 1 {
        1
    } else {
        0
    };
    band_score + class_score + multi_band_bonus
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radio(
        ifname: &str,
        phy: &str,
        bands: PhyBandSupport,
        modes: &[&str],
        class: ConnectionClass,
        is_ap_vif: bool,
    ) -> WifiRadio {
        WifiRadio {
            ifname: ifname.to_string(),
            phy: phy.to_string(),
            capability: PhyCapability {
                phy: phy.to_string(),
                supports_managed_plus_ap: modes.contains(&"managed")
                    && modes.contains(&"AP"),
                max_channels: 1,
                interface_modes: modes.iter().map(|s| s.to_string()).collect(),
                bands,
            },
            connection_class: class,
            is_ap_vif,
        }
    }

    fn default_band_priority() -> Vec<BandClass> {
        vec![BandClass::Ghz6, BandClass::Ghz5, BandClass::Ghz2_4]
    }

    #[test]
    fn explicit_overrides_win_over_inventory() {
        let inv = vec![radio(
            "wlan0",
            "phy0",
            PhyBandSupport {
                ghz_2_4: true,
                ghz_5: false,
                ghz_6: false,
            },
            &["managed", "AP"],
            ConnectionClass::Onboard,
            false,
        )];
        let r = assign_wifi_roles(
            &inv,
            &default_band_priority(),
            RoleOverrides {
                explicit_sta: "wlan9",
                explicit_ap: "wlan8",
                default_sta_fallback: "wlan0",
            },
        );
        assert_eq!(r.sta_ifname, "wlan9");
        assert_eq!(r.ap_ifname, "wlan8");
        assert!(!r.same_iface);
        assert!(r.rationale.contains("pinned"));
    }

    #[test]
    fn single_dual_band_radio_collides_on_one_iface() {
        let inv = vec![radio(
            "wlan0",
            "phy0",
            PhyBandSupport {
                ghz_2_4: true,
                ghz_5: true,
                ghz_6: false,
            },
            &["managed", "AP"],
            ConnectionClass::Onboard,
            false,
        )];
        let r = assign_wifi_roles(
            &inv,
            &default_band_priority(),
            RoleOverrides::default(),
        );
        assert_eq!(r.sta_ifname, "wlan0");
        assert_eq!(r.ap_ifname, "wlan0");
        assert!(r.same_iface);
    }

    #[test]
    fn split_radio_picks_5_ghz_for_sta_and_2_4_for_ap() {
        let inv = vec![
            radio(
                "wlan0",
                "phy0",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: false,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
            radio(
                "wlan1",
                "phy1",
                PhyBandSupport {
                    ghz_2_4: false,
                    ghz_5: true,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
        ];
        let r = assign_wifi_roles(
            &inv,
            &default_band_priority(),
            RoleOverrides::default(),
        );
        assert_eq!(r.sta_ifname, "wlan1");
        assert_eq!(r.ap_ifname, "wlan0");
        assert!(!r.same_iface);
        assert!(r.rationale.contains("split-radio"));
    }

    #[test]
    fn tri_band_wins_over_2_4_only_for_sta() {
        let inv = vec![
            radio(
                "wlan0",
                "phy0",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: false,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
            radio(
                "wlan1",
                "phy1",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: true,
                    ghz_6: true,
                },
                &["managed"],
                ConnectionClass::Usb,
                false,
            ),
        ];
        let r = assign_wifi_roles(
            &inv,
            &default_band_priority(),
            RoleOverrides::default(),
        );
        // 6 GHz beats 2.4 GHz even when the 6 GHz radio is on USB.
        assert_eq!(r.sta_ifname, "wlan1");
        // wlan1 isn't AP-capable so AP lands on wlan0.
        assert_eq!(r.ap_ifname, "wlan0");
    }

    #[test]
    fn ap_vif_row_excluded_from_sta_pool() {
        let inv = vec![
            radio(
                "wlan0",
                "phy0",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: true,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
            radio(
                "ap0",
                "phy0",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: true,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                true,
            ),
        ];
        let r = assign_wifi_roles(
            &inv,
            &default_band_priority(),
            RoleOverrides::default(),
        );
        assert_eq!(r.sta_ifname, "wlan0");
    }

    #[test]
    fn empty_inventory_falls_back_to_default() {
        let r = assign_wifi_roles(
            &[],
            &default_band_priority(),
            RoleOverrides {
                explicit_sta: "",
                explicit_ap: "",
                default_sta_fallback: "wlan0",
            },
        );
        assert_eq!(r.sta_ifname, "wlan0");
        assert_eq!(r.ap_ifname, "wlan0");
        assert!(r.rationale.contains("no STA-capable radio"));
    }

    #[test]
    fn operator_band_priority_overrides_default() {
        // Two radios: one 6 GHz capable, one 2.4 GHz only.
        // Operator priority puts 2.4 GHz first.
        let inv = vec![
            radio(
                "wlan0",
                "phy0",
                PhyBandSupport {
                    ghz_2_4: true,
                    ghz_5: false,
                    ghz_6: false,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
            radio(
                "wlan1",
                "phy1",
                PhyBandSupport {
                    ghz_2_4: false,
                    ghz_5: false,
                    ghz_6: true,
                },
                &["managed", "AP"],
                ConnectionClass::Onboard,
                false,
            ),
        ];
        let priority =
            vec![BandClass::Ghz2_4, BandClass::Ghz5, BandClass::Ghz6];
        let r = assign_wifi_roles(&inv, &priority, RoleOverrides::default());
        assert_eq!(r.sta_ifname, "wlan0");
        assert_eq!(r.ap_ifname, "wlan1");
    }
}
