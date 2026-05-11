// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Kernel-level radio block (rfkill) state reader.
//!
//! Most Linux distributions expose hardware radios through the
//! `rfkill` subsystem. Each radio family (WLAN / Bluetooth / WWAN
//! / GPS / etc.) gets a `/sys/class/rfkill/rfkillN/` directory
//! with three files this module reads:
//!
//! - `name` — human-readable identifier (e.g. `phy0`, `hci0`).
//! - `type` — radio family: `wlan`, `bluetooth`, `wwan`, `gps`,
//!   `fm`, `nfc`. The `wlan` type is what the rest of the plugin
//!   maps to the operator-facing "wifi" label.
//! - `soft` — 1 if software-blocked (kernel-level block; can be
//!   cleared via NetworkManager / nmcli / `rfkill unblock`).
//! - `hard` — 1 if hardware-blocked (physical switch, BIOS, or
//!   firmware policy; software cannot override).
//!
//! The reader is read-only and never escalates. Soft-block CLEAR
//! is delegated to the existing `nmcli radio wifi on` path which
//! goes through rfkill under the hood; soft-block SET likewise
//! delegates to `nmcli radio wifi off`. This module exists to
//! distinguish hard-block (operator-actionable: flip the
//! switch / reset BIOS) from soft-block (we can clear it) so the
//! wire-op surface and UI render the right state.
//!
//! ## Why sysfs over `rfkill list`
//!
//! Parsing the `rfkill` command's text output is fragile — the
//! format has shifted across util-linux versions (the legacy
//! `0: phy0: Wireless LAN` form vs the newer JSON `-o json`
//! form). Reading sysfs files directly produces stable bytes
//! whose schema is part of the kernel ABI: changes are
//! exceptional and well-publicised. The reader stays trivial
//! (`read_to_string` + `trim().parse()`) and has no external
//! command-dispatch dependency.
//!
//! ## Cross-OS posture
//!
//! Only Linux ships `/sys/class/rfkill`. On non-Linux dev hosts
//! the reader returns an empty `Vec` and the plugin's radio
//! state machine treats the radio as not-blocked (preference
//! applies). The plugin's `cfg(target_os = "linux")` guards
//! ensure no syscall / sysfs read attempt happens on platforms
//! that lack the surface.

use std::fs;
use std::path::Path;

/// Radio family the kernel rfkill subsystem reports. Matches
/// the values in `/sys/class/rfkill/*/type` verbatim except for
/// the `Unknown` variant which captures any future family the
/// kernel adds that this build does not know.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RadioKind {
    /// `wlan` — Wi-Fi family. Operator-facing label is `wifi`.
    Wlan,
    /// `bluetooth` — Bluetooth family.
    Bluetooth,
    /// `wwan` — cellular modems.
    Wwan,
    /// Any other family the kernel reports (gps / fm / nfc /
    /// future additions). Carries the raw kernel string so
    /// diagnostics name the family even when this build does
    /// not handle it specifically.
    Unknown(String),
}

impl RadioKind {
    /// Parse the kernel `/sys/class/rfkill/*/type` value into a
    /// [`RadioKind`]. Unknown families are preserved verbatim
    /// (no information loss).
    pub fn from_kernel_str(s: &str) -> Self {
        match s.trim() {
            "wlan" => RadioKind::Wlan,
            "bluetooth" => RadioKind::Bluetooth,
            "wwan" => RadioKind::Wwan,
            other => RadioKind::Unknown(other.to_string()),
        }
    }

    /// Operator-facing label used in wire-op responses (the UI
    /// reads this verbatim). `Wlan` maps to `wifi` for legacy
    /// alignment with the operator vocabulary that pre-dates
    /// the kernel's `wlan` naming choice.
    pub fn operator_label(&self) -> &str {
        match self {
            RadioKind::Wlan => "wifi",
            RadioKind::Bluetooth => "bluetooth",
            RadioKind::Wwan => "wwan",
            RadioKind::Unknown(s) => s.as_str(),
        }
    }
}

/// One radio entry from `/sys/class/rfkill/rfkillN/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RfkillEntry {
    /// Kernel index (`rfkillN` → `N`).
    pub index: u32,
    /// Human-readable kernel name (`phy0`, `hci0`, ...).
    pub name: String,
    /// Radio family.
    pub kind: RadioKind,
    /// `true` when software-blocked (kernel rfkill state). The
    /// plugin's nmcli path can flip this.
    pub soft_blocked: bool,
    /// `true` when hardware-blocked (physical switch / BIOS /
    /// firmware). The plugin cannot flip this; the operator
    /// must resolve it manually.
    pub hard_blocked: bool,
}

/// Read every entry under `/sys/class/rfkill/`. Returns an empty
/// `Vec` on:
///
/// - non-Linux platforms (the sysfs path does not exist),
/// - hosts without any rfkill-managed radio,
/// - hosts where the rfkill kernel module is not loaded,
/// - sysfs masked by sandboxing.
///
/// Individual entries that fail to parse are skipped with a
/// `tracing::debug!` line; remaining entries are still returned
/// so a single broken radio does not blind the plugin to the
/// rest of the host's radios.
pub fn read_all() -> Vec<RfkillEntry> {
    read_from(Path::new("/sys/class/rfkill"))
}

/// Internal: read entries from a caller-supplied root. Tests
/// build a tempdir laid out like sysfs and pass it here.
pub(crate) fn read_from(root: &Path) -> Vec<RfkillEntry> {
    let dir = match fs::read_dir(root) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(idx_str) = name.strip_prefix("rfkill") else {
            continue;
        };
        let Ok(index) = idx_str.parse::<u32>() else {
            continue;
        };
        match parse_one(&path, index) {
            Ok(e) => out.push(e),
            Err(reason) => {
                tracing::debug!(
                    rfkill_index = index,
                    reason = %reason,
                    "rfkill entry skipped"
                );
            }
        }
    }
    out.sort_by_key(|e| e.index);
    out
}

fn parse_one(dir: &Path, index: u32) -> Result<RfkillEntry, String> {
    let name =
        read_trim(&dir.join("name")).map_err(|e| format!("name: {e}"))?;
    let kind_raw =
        read_trim(&dir.join("type")).map_err(|e| format!("type: {e}"))?;
    let soft =
        read_bool(&dir.join("soft")).map_err(|e| format!("soft: {e}"))?;
    let hard =
        read_bool(&dir.join("hard")).map_err(|e| format!("hard: {e}"))?;
    Ok(RfkillEntry {
        index,
        name,
        kind: RadioKind::from_kernel_str(&kind_raw),
        soft_blocked: soft,
        hard_blocked: hard,
    })
}

fn read_trim(path: &Path) -> Result<String, std::io::Error> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn read_bool(path: &Path) -> Result<bool, String> {
    let s = read_trim(path).map_err(|e| e.to_string())?;
    match s.as_str() {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(format!("expected 0 or 1, got {other:?}")),
    }
}

/// Aggregate the rfkill state for one [`RadioKind`] across every
/// matching entry (a host may expose multiple WLAN phys; the
/// effective block for the family is the OR over the set).
///
/// Returns `None` when no entry of the given kind exists — the
/// plugin's state machine treats this as "no kernel-level block
/// information available", which the rest of the plugin
/// interprets as not-blocked (i.e. the per-radio preference
/// applies directly).
pub fn aggregate_for(
    entries: &[RfkillEntry],
    kind: &RadioKind,
) -> Option<RadioBlockState> {
    let matching: Vec<&RfkillEntry> =
        entries.iter().filter(|e| &e.kind == kind).collect();
    if matching.is_empty() {
        return None;
    }
    let soft = matching.iter().any(|e| e.soft_blocked);
    let hard = matching.iter().any(|e| e.hard_blocked);
    Some(RadioBlockState {
        soft_blocked: soft,
        hard_blocked: hard,
    })
}

/// Aggregated block state for one radio family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RadioBlockState {
    /// Any matching entry is soft-blocked.
    pub soft_blocked: bool,
    /// Any matching entry is hard-blocked.
    pub hard_blocked: bool,
}

impl RadioBlockState {
    /// `true` when neither block is active.
    pub fn is_clear(&self) -> bool {
        !self.soft_blocked && !self.hard_blocked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_entry(
        root: &Path,
        idx: u32,
        name: &str,
        kind: &str,
        soft: u8,
        hard: u8,
    ) {
        let dir = root.join(format!("rfkill{idx}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), format!("{name}\n")).unwrap();
        fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        fs::write(dir.join("soft"), format!("{soft}\n")).unwrap();
        fs::write(dir.join("hard"), format!("{hard}\n")).unwrap();
    }

    #[test]
    fn parse_wlan_unblocked() {
        let dir = tempdir().unwrap();
        make_entry(dir.path(), 0, "phy0", "wlan", 0, 0);
        let entries = read_from(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, RadioKind::Wlan);
        assert_eq!(entries[0].name, "phy0");
        assert!(!entries[0].soft_blocked);
        assert!(!entries[0].hard_blocked);
    }

    #[test]
    fn parse_wlan_soft_blocked() {
        let dir = tempdir().unwrap();
        make_entry(dir.path(), 0, "phy0", "wlan", 1, 0);
        let entries = read_from(dir.path());
        assert!(entries[0].soft_blocked);
        assert!(!entries[0].hard_blocked);
    }

    #[test]
    fn parse_wlan_hard_blocked() {
        let dir = tempdir().unwrap();
        make_entry(dir.path(), 0, "phy0", "wlan", 0, 1);
        let entries = read_from(dir.path());
        assert!(!entries[0].soft_blocked);
        assert!(entries[0].hard_blocked);
    }

    #[test]
    fn parse_mixed_radios() {
        let dir = tempdir().unwrap();
        make_entry(dir.path(), 0, "phy0", "wlan", 0, 0);
        make_entry(dir.path(), 1, "hci0", "bluetooth", 1, 0);
        make_entry(dir.path(), 2, "cdc-wdm0", "wwan", 0, 0);
        let entries = read_from(dir.path());
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, RadioKind::Wlan);
        assert_eq!(entries[1].kind, RadioKind::Bluetooth);
        assert!(entries[1].soft_blocked);
        assert_eq!(entries[2].kind, RadioKind::Wwan);
    }

    #[test]
    fn missing_root_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nonexistent");
        let entries = read_from(&missing);
        assert!(entries.is_empty());
    }

    #[test]
    fn unknown_radio_kind_preserves_label() {
        let dir = tempdir().unwrap();
        make_entry(dir.path(), 0, "fmradio", "fm", 0, 0);
        let entries = read_from(dir.path());
        assert_eq!(entries.len(), 1);
        match &entries[0].kind {
            RadioKind::Unknown(s) => assert_eq!(s, "fm"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        assert_eq!(entries[0].kind.operator_label(), "fm");
    }

    #[test]
    fn malformed_entry_skipped_without_blinding_others() {
        let dir = tempdir().unwrap();
        // Good entry.
        make_entry(dir.path(), 0, "phy0", "wlan", 0, 0);
        // Bad entry — missing `hard` file.
        let bad = dir.path().join("rfkill1");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("name"), "phy1").unwrap();
        fs::write(bad.join("type"), "wlan").unwrap();
        fs::write(bad.join("soft"), "0").unwrap();
        // (no `hard` file).

        let entries = read_from(dir.path());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "phy0");
    }

    #[test]
    fn aggregate_soft_or_hard_across_entries() {
        let entries = vec![
            RfkillEntry {
                index: 0,
                name: "phy0".into(),
                kind: RadioKind::Wlan,
                soft_blocked: false,
                hard_blocked: false,
            },
            RfkillEntry {
                index: 1,
                name: "phy1".into(),
                kind: RadioKind::Wlan,
                soft_blocked: true,
                hard_blocked: false,
            },
        ];
        let agg = aggregate_for(&entries, &RadioKind::Wlan).unwrap();
        assert!(agg.soft_blocked);
        assert!(!agg.hard_blocked);
        assert!(!agg.is_clear());
    }

    #[test]
    fn aggregate_returns_none_for_missing_kind() {
        let entries = vec![RfkillEntry {
            index: 0,
            name: "phy0".into(),
            kind: RadioKind::Wlan,
            soft_blocked: false,
            hard_blocked: false,
        }];
        assert!(aggregate_for(&entries, &RadioKind::Bluetooth).is_none());
    }

    #[test]
    fn block_state_is_clear() {
        let s = RadioBlockState {
            soft_blocked: false,
            hard_blocked: false,
        };
        assert!(s.is_clear());

        let s = RadioBlockState {
            soft_blocked: true,
            hard_blocked: false,
        };
        assert!(!s.is_clear());

        let s = RadioBlockState {
            soft_blocked: false,
            hard_blocked: true,
        };
        assert!(!s.is_clear());
    }
}
