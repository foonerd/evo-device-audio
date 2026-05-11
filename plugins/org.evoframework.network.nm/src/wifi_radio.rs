// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0

//! Wi-Fi radio state machine for the network.nm plugin.
//!
//! ## Inputs
//!
//! The wire-op surface owes the UI an unambiguous answer to
//! "what is the wifi radio doing right now, and why?". Three
//! independent inputs feed the answer:
//!
//! 1. **Master flight-mode** (boolean, persisted) — operator-
//!    facing master switch. When `true` every radio is off
//!    regardless of per-radio preference.
//! 2. **Per-radio preference** (boolean, persisted) — what the
//!    operator wants this radio to be when flight-mode is off.
//!    The UI's per-radio toggle writes here; effective state
//!    follows once flight-mode is off.
//! 3. **rfkill state** (read-only, kernel-level) — soft-block
//!    (the plugin can clear it via nmcli) and hard-block (a
//!    physical switch / BIOS / firmware policy the plugin
//!    cannot override).
//!
//! ## Composition rule
//!
//! ```text
//! effective_enabled = !flight_mode && !hard_blocked && preference
//! ```
//!
//! soft_blocked is informational only — when preference is
//! `enabled` but soft_blocked is `true`, the load-time policy
//! applier issues `nmcli radio wifi on` which clears the soft-
//! block (NetworkManager goes through rfkill under the hood).
//!
//! ## Coordination with future radio plugins
//!
//! `flight_mode` is the cross-radio master switch. The plugin
//! that owns it today (this one) exposes it via the existing
//! `network.nm.flight_mode.get|set` wire ops. When a Bluetooth
//! radio plugin (or any second consumer) lands, the natural
//! next chunk extracts `flight_mode` to a domain-neutral
//! `device.options` (or equivalent) plugin so both radio
//! plugins read from one source of truth. The state-machine
//! shape stays unchanged — only the storage / wire-op owner
//! moves.

use serde::{Deserialize, Serialize};

use crate::rfkill::RadioBlockState;

/// Operator-facing on/off for a radio family. Surfaced as a
/// lowercase string in wire-op JSON (`"enabled"` / `"disabled"`)
/// rather than a boolean so the UI's tooltip vocabulary
/// (`"disabled"` / `"hardware-blocked"`) stays consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RadioState {
    /// The radio is operational and accepting traffic.
    Enabled,
    /// The radio is off (any reason — see [`RadioSource`]).
    Disabled,
}

impl RadioState {
    /// Boolean projection. Useful at the call sites that need
    /// to flip `nmcli radio wifi on|off`.
    pub fn is_enabled(self) -> bool {
        matches!(self, RadioState::Enabled)
    }

    /// Construct from a boolean. Mirrors the persisted form.
    pub fn from_bool(b: bool) -> Self {
        if b {
            RadioState::Enabled
        } else {
            RadioState::Disabled
        }
    }
}

/// Why the radio is in its current effective state. The UI
/// renders a different tooltip per source so operators
/// understand whether they can change the state and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioSource {
    /// Effective state matches the operator's per-radio
    /// preference. UI shows the toggle as live; flipping it
    /// rewrites the preference and applies immediately.
    Preference,
    /// Master flight-mode is on; effective state is forced off
    /// regardless of preference. UI shows the toggle as
    /// "disabled because flight mode is on"; flipping the
    /// toggle is still permitted and updates the preference so
    /// the change takes effect once flight-mode clears.
    FlightMode,
    /// rfkill reports the radio as hard-blocked (physical
    /// switch / BIOS / firmware policy). The plugin cannot
    /// override; UI shows the toggle as "hardware-blocked,
    /// resolve at the device".
    HardwareBlock,
}

/// Composite radio state surfaced to the UI in a single
/// wire-op response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RadioStateView {
    /// What the user sees (the OR-composition of preference +
    /// flight_mode + hard_block).
    pub effective_state: RadioState,
    /// What the user has asked for. The UI writes here via the
    /// per-radio set wire-op.
    pub preference: RadioState,
    /// `true` when the kernel reports a hardware-level block
    /// the plugin cannot clear.
    pub hard_blocked: bool,
    /// `true` when the kernel reports a software-level block.
    /// Informational; the plugin's load-time policy applier
    /// clears this when preference is `enabled`.
    pub soft_blocked: bool,
    /// Why the radio is in [`Self::effective_state`].
    pub source: RadioSource,
}

/// Resolve the effective radio state from the three inputs.
/// Pure function — no side effects, no syscalls; called by
/// every wire-op handler.
pub fn resolve(
    preference: RadioState,
    flight_mode: bool,
    block: Option<RadioBlockState>,
) -> RadioStateView {
    let (hard_blocked, soft_blocked) = match block {
        Some(b) => (b.hard_blocked, b.soft_blocked),
        None => (false, false),
    };
    let (effective_state, source) = if hard_blocked {
        (RadioState::Disabled, RadioSource::HardwareBlock)
    } else if flight_mode {
        (RadioState::Disabled, RadioSource::FlightMode)
    } else {
        (preference, RadioSource::Preference)
    };
    RadioStateView {
        effective_state,
        preference,
        hard_blocked,
        soft_blocked,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(soft: bool, hard: bool) -> Option<RadioBlockState> {
        Some(RadioBlockState {
            soft_blocked: soft,
            hard_blocked: hard,
        })
    }

    #[test]
    fn preference_enabled_no_blocks_no_flight() {
        let v = resolve(RadioState::Enabled, false, blk(false, false));
        assert_eq!(v.effective_state, RadioState::Enabled);
        assert_eq!(v.source, RadioSource::Preference);
        assert!(!v.hard_blocked);
        assert!(!v.soft_blocked);
    }

    #[test]
    fn preference_disabled_no_blocks_no_flight() {
        let v = resolve(RadioState::Disabled, false, blk(false, false));
        assert_eq!(v.effective_state, RadioState::Disabled);
        assert_eq!(v.source, RadioSource::Preference);
    }

    #[test]
    fn flight_mode_overrides_preference_enabled() {
        let v = resolve(RadioState::Enabled, true, blk(false, false));
        assert_eq!(v.effective_state, RadioState::Disabled);
        assert_eq!(v.source, RadioSource::FlightMode);
    }

    #[test]
    fn flight_mode_with_preference_disabled_still_disabled() {
        let v = resolve(RadioState::Disabled, true, blk(false, false));
        assert_eq!(v.effective_state, RadioState::Disabled);
        // Source: flight_mode wins because it's the higher-
        // priority forcing factor; the preference happens to
        // also be disabled but the operator's tooltip should
        // explain the master switch.
        assert_eq!(v.source, RadioSource::FlightMode);
    }

    #[test]
    fn hard_block_overrides_everything() {
        // hard-blocked with everything else permissive: still off.
        let v = resolve(RadioState::Enabled, false, blk(false, true));
        assert_eq!(v.effective_state, RadioState::Disabled);
        assert_eq!(v.source, RadioSource::HardwareBlock);
        assert!(v.hard_blocked);
    }

    #[test]
    fn hard_block_takes_priority_over_flight_mode_for_source() {
        // Both forcing factors active. UI tooltip should
        // surface hardware-block because it is operator-
        // actionable (resolve at device); flight-mode is a
        // setting the operator already knows they toggled.
        let v = resolve(RadioState::Enabled, true, blk(false, true));
        assert_eq!(v.effective_state, RadioState::Disabled);
        assert_eq!(v.source, RadioSource::HardwareBlock);
    }

    #[test]
    fn soft_block_is_informational_only() {
        // Soft-blocked but preference enabled and no flight
        // mode: effective state matches preference. The
        // plugin's load-time policy applier will issue
        // `nmcli radio wifi on` which clears the soft-block.
        let v = resolve(RadioState::Enabled, false, blk(true, false));
        assert_eq!(v.effective_state, RadioState::Enabled);
        assert_eq!(v.source, RadioSource::Preference);
        assert!(v.soft_blocked);
        assert!(!v.hard_blocked);
    }

    #[test]
    fn no_block_information_treated_as_clear() {
        // Hosts without rfkill (non-Linux dev environment, sandboxed
        // sysfs) return None from the rfkill reader; the
        // resolver treats this as "no block known" so the
        // preference applies directly.
        let v = resolve(RadioState::Enabled, false, None);
        assert_eq!(v.effective_state, RadioState::Enabled);
        assert_eq!(v.source, RadioSource::Preference);
        assert!(!v.hard_blocked);
        assert!(!v.soft_blocked);
    }

    #[test]
    fn radio_state_serde_roundtrip() {
        let v = serde_json::to_value(RadioState::Enabled).unwrap();
        assert_eq!(v, serde_json::json!("enabled"));
        let back: RadioState =
            serde_json::from_value(serde_json::json!("disabled")).unwrap();
        assert_eq!(back, RadioState::Disabled);
    }

    #[test]
    fn radio_state_view_serde_roundtrip() {
        let v = RadioStateView {
            effective_state: RadioState::Disabled,
            preference: RadioState::Enabled,
            hard_blocked: false,
            soft_blocked: true,
            source: RadioSource::FlightMode,
        };
        let s = serde_json::to_string(&v).unwrap();
        let back: RadioStateView = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }
}
