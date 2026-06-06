//! Transport-state gate.
//!
//! The plugin's contract is that spectrum emission is silent
//! unless `transport_state == "playing"` on the
//! `audio_playback_now_playing` subject the plugin subscribes
//! to. Stopped / paused -> no frames emitted (the capture loop
//! continues so a resume picks up at the next tick without
//! spawn latency).
//!
//! This module owns the gate's wire-shape contract:
//!
//! - `TransportGate` — the two-state enum the capture loop
//!   checks before each emit.
//! - `parse_from_state` — the pure projection from the
//!   `now_playing` subject's state payload to a gate value.
//!
//! The subscriber loop that watches `audio_playback_now_playing`
//! and pushes gate values lives in `lib.rs::spawn_transport_gate`
//! (it owns the SDK handles + the watch::Sender); this module
//! keeps zero I/O so the parser is trivially testable against
//! synthesised state payloads.

#[cfg(any(test, feature = "alsa-substrate"))]
use serde_json::Value;

/// Whether the capture loop should emit the current FFT frame.
///
/// Conservative default: `NotPlaying`. When the subject has
/// not yet been announced (load-order race), or the payload
/// is malformed, or `transport_state` is missing / unknown,
/// the gate stays `NotPlaying`. Emission resumes only when a
/// `transport_state == "playing"` update arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportGate {
    /// The local now-playing transport is `playing`. The
    /// capture loop emits on every successful FFT compute.
    Playing,
    /// Any non-playing state: `paused`, `stopped`, unknown,
    /// unparseable, or the subject has not yet been seeded.
    /// The capture loop continues to compute frames (so the
    /// next play resumes without spawn latency) but does not
    /// emit them.
    NotPlaying,
}

impl TransportGate {
    /// `true` if the capture loop should publish the current
    /// frame onto the spectrum subject.
    pub fn should_emit(self) -> bool {
        matches!(self, TransportGate::Playing)
    }
}

/// Pure projection: read the `transport_state` field of an
/// `audio_playback_now_playing` state payload and project it
/// onto the gate.
///
/// The wire payload's `transport_state` field is a string —
/// `"playing"` / `"paused"` / `"stopped"`. Only the literal
/// string `"playing"` opens the gate; every other value
/// (including missing field, non-string value, unknown string)
/// keeps the gate closed.
///
/// Gated on `alsa-substrate` because the only non-test
/// consumer (`now_playing_subscriber`) is itself
/// feature-gated. The unit tests below stay accessible to
/// the default build via the standard `#[cfg(test)]` block.
#[cfg(any(test, feature = "alsa-substrate"))]
pub fn parse_from_state(state: &Value) -> TransportGate {
    match state.get("transport_state").and_then(Value::as_str) {
        Some("playing") => TransportGate::Playing,
        _ => TransportGate::NotPlaying,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn playing_string_opens_the_gate() {
        let state = json!({ "transport_state": "playing" });
        assert_eq!(parse_from_state(&state), TransportGate::Playing);
        assert!(parse_from_state(&state).should_emit());
    }

    #[test]
    fn paused_string_closes_the_gate() {
        let state = json!({ "transport_state": "paused" });
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
        assert!(!parse_from_state(&state).should_emit());
    }

    #[test]
    fn stopped_string_closes_the_gate() {
        let state = json!({ "transport_state": "stopped" });
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
    }

    #[test]
    fn missing_field_closes_the_gate() {
        let state = json!({ "title": "irrelevant" });
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
    }

    #[test]
    fn non_string_field_closes_the_gate() {
        let state = json!({ "transport_state": 1 });
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
    }

    #[test]
    fn unknown_string_closes_the_gate() {
        // Defensive: a future wire-shape addition (e.g.
        // "seeking", "buffering") must NOT silently re-open
        // the gate. Only the literal "playing" qualifies.
        let state = json!({ "transport_state": "buffering" });
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
    }

    #[test]
    fn null_state_closes_the_gate() {
        let state = Value::Null;
        assert_eq!(parse_from_state(&state), TransportGate::NotPlaying);
    }

    #[test]
    fn should_emit_only_when_playing() {
        assert!(TransportGate::Playing.should_emit());
        assert!(!TransportGate::NotPlaying.should_emit());
    }
}
