//! Local-role gate.
//!
//! The plugin's contract is that spectrum emission is silent
//! when the local device is a follower (non-leader) of an
//! active multi-room group — operator seats in a group receive
//! the leader's wavefront, so a follower publishing a parallel
//! spectrum subject under the same name is duplicate signal
//! that doesn't help the visualiser.
//!
//! Solo devices (no live group), source-hosts (leaders), and
//! devices whose multi-room engagement is otherwise quiescent
//! all open the gate; only the `Receiver` role closes it.
//!
//! This module owns the gate's wire-shape contract:
//!
//! - `LocalRole` — the three-state enum the capture loop
//!   reads alongside the transport gate.
//! - `parse_from_state` — the pure projection from the
//!   `audio_multiroom_local_role` subject's state payload
//!   to a `LocalRole`.
//!
//! The subscriber loop that watches the singleton role
//! subject lives in `local_role_subscriber.rs`; this module
//! keeps zero I/O so the parser is trivially testable.

#[cfg(any(test, feature = "alsa-substrate"))]
use serde_json::Value;

/// Effective multi-room engagement role for the local node,
/// projected onto the spectrum gate.
///
/// Conservative-permissive default: `Auto`. The default
/// matches the operational majority case (no multi-room
/// plugin loaded, or solo device with no group configured)
/// where the visualiser is expected to render. Only an
/// explicit `Receiver` value from the local-role subject
/// closes the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRole {
    /// Local node is the source-host (leader) of an active
    /// group. The capture loop emits.
    Source,
    /// Local node is a non-leader member of an active group.
    /// The capture loop does NOT emit — the source-host's
    /// spectrum is the authoritative wavefront for every
    /// member of the group.
    Receiver,
    /// Local node has no live multi-room engagement. DAC is
    /// free for local playback; the capture loop emits as a
    /// solo device. Default for the watch channel before the
    /// subscriber seeds it.
    Auto,
}

impl LocalRole {
    /// `true` if the capture loop should publish the current
    /// frame onto the spectrum subject (subject also to the
    /// transport gate). Only `Receiver` closes the gate.
    pub fn should_emit(self) -> bool {
        !matches!(self, LocalRole::Receiver)
    }
}

/// Pure projection: read the `role` field of an
/// `audio_multiroom_local_role` state payload and project it
/// onto the gate.
///
/// The wire payload's `role` field is the lowercase string
/// `"source"` / `"receiver"` / `"auto"`. Any unknown / missing
/// / non-string value defaults to `Auto` (the permissive
/// default — solo-device behaviour).
///
/// Gated on `alsa-substrate` because the only non-test
/// consumer (`local_role_subscriber`) is itself
/// feature-gated. The unit tests below stay accessible to
/// the default build via the standard `#[cfg(test)]` block.
#[cfg(any(test, feature = "alsa-substrate"))]
pub fn parse_from_state(state: &Value) -> LocalRole {
    match state.get("role").and_then(Value::as_str) {
        Some("source") => LocalRole::Source,
        Some("receiver") => LocalRole::Receiver,
        Some("auto") => LocalRole::Auto,
        _ => LocalRole::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_role_opens_the_gate() {
        let state = json!({ "role": "source" });
        assert_eq!(parse_from_state(&state), LocalRole::Source);
        assert!(parse_from_state(&state).should_emit());
    }

    #[test]
    fn auto_role_opens_the_gate() {
        let state = json!({ "role": "auto" });
        assert_eq!(parse_from_state(&state), LocalRole::Auto);
        assert!(parse_from_state(&state).should_emit());
    }

    #[test]
    fn receiver_role_closes_the_gate() {
        let state = json!({ "role": "receiver" });
        assert_eq!(parse_from_state(&state), LocalRole::Receiver);
        assert!(!parse_from_state(&state).should_emit());
    }

    #[test]
    fn missing_field_defaults_to_auto_open() {
        let state = json!({ "title": "irrelevant" });
        assert_eq!(parse_from_state(&state), LocalRole::Auto);
        assert!(parse_from_state(&state).should_emit());
    }

    #[test]
    fn unknown_string_defaults_to_auto_open() {
        // Defensive against future wire-shape additions.
        // Conservative-permissive: if we don't recognise it,
        // we're probably solo / no-engagement — emit.
        let state = json!({ "role": "follower" });
        assert_eq!(parse_from_state(&state), LocalRole::Auto);
    }

    #[test]
    fn non_string_field_defaults_to_auto_open() {
        let state = json!({ "role": 1 });
        assert_eq!(parse_from_state(&state), LocalRole::Auto);
    }

    #[test]
    fn null_state_defaults_to_auto_open() {
        let state = Value::Null;
        assert_eq!(parse_from_state(&state), LocalRole::Auto);
    }

    #[test]
    fn should_emit_excludes_only_receiver() {
        assert!(LocalRole::Source.should_emit());
        assert!(LocalRole::Auto.should_emit());
        assert!(!LocalRole::Receiver.should_emit());
    }
}
