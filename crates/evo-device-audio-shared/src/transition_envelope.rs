//! Mixer-transition envelope coordination types + shared
//! parse / build helpers for the two paired subjects:
//!
//! * `evo.audio.mixer_transition:envelope_requested` — the
//!   orchestrator publishes this with a fresh `generation` +
//!   the `requested_state` (muted / unmuted) on each
//!   safety-envelope step (pre-mute, unmute).
//! * `evo.audio.mixer_transition:envelope_observed` — the
//!   playback warden subscribes to envelope_requested,
//!   transitions its playback chain (pause / resume MPD), and
//!   publishes envelope_observed with the matching
//!   `generation` + `observed_state` once the chain has
//!   actually transitioned.
//!
//! The orchestrator subscribes to envelope_observed and
//! advances the eight-step state machine ONLY when it
//! observes a matching `(generation, state)` pair within a
//! bounded timeout. Timeout routes to the orchestrator's
//! rollback chain.
//!
//! Both plugins share the addressing constants + the JSON
//! shape via this module so the wire contract has one
//! source of truth.

use serde::{Deserialize, Serialize};

/// Addressing scheme constant for both envelope subjects.
pub const ENVELOPE_SCHEME: &str = "evo.audio.mixer_transition";

/// Addressing value for the orchestrator-published request.
pub const ENVELOPE_REQUESTED_VALUE: &str = "envelope_requested";

/// Addressing value for the warden-published observation.
pub const ENVELOPE_OBSERVED_VALUE: &str = "envelope_observed";

/// Subject type for envelope_requested. Identifies the
/// subject's schema to consumers (UI affordances, audit
/// tooling) browsing the subject registry.
pub const ENVELOPE_REQUESTED_SUBJECT_TYPE: &str =
    "audio_mixer_transition_envelope_requested";

/// Subject type for envelope_observed.
pub const ENVELOPE_OBSERVED_SUBJECT_TYPE: &str =
    "audio_mixer_transition_envelope_observed";

/// The two valid envelope states the safety envelope can
/// request OR observe. `muted` engages the safety envelope
/// (playback chain paused, no audio leaks through the
/// mixer-mode transition); `unmuted` releases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeState {
    /// Safety envelope engaged — playback chain is muted /
    /// paused; the orchestrator can proceed with the
    /// mixer-mode mutation without risk of an audible
    /// blast / jump.
    Muted,
    /// Safety envelope released — playback chain is
    /// unmuted / resumed; normal audio flow.
    Unmuted,
}

/// Wire shape of the envelope_requested subject's state.
/// Orchestrator publishes one of these per safety-envelope
/// step (pre-mute / unmute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeRequested {
    /// Wire-protocol payload version.
    pub v: u32,
    /// Monotonically-increasing generation counter. Bumped
    /// per orchestrator-step so the warden can
    /// disambiguate a stale observation from a fresh
    /// request. The orchestrator's `await_envelope_ack`
    /// matches on this value.
    pub generation: u64,
    /// The state the orchestrator is requesting the warden
    /// transition to.
    pub requested_state: EnvelopeState,
    /// Unix-epoch millisecond timestamp the request was
    /// published at. Diagnostic; not load-bearing for
    /// matching.
    pub requested_at_ms: u64,
}

/// Wire shape of the envelope_observed subject's state. The
/// warden publishes one of these after its playback chain
/// has actually transitioned to the requested state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvelopeObserved {
    /// Wire-protocol payload version.
    pub v: u32,
    /// Matches the generation field of the
    /// envelope_requested entry the warden is
    /// acknowledging.
    pub generation: u64,
    /// The state the warden has actually transitioned to.
    /// Equal to the matching `EnvelopeRequested.requested_state`
    /// on a successful step; mismatched values indicate the
    /// warden could not honour the request and surface a
    /// failure path the orchestrator MUST treat as a timeout
    /// equivalent (does NOT advance).
    pub observed_state: EnvelopeState,
    /// Unix-epoch millisecond timestamp the observation
    /// was published at. Diagnostic.
    pub observed_at_ms: u64,
}

/// Wire-protocol payload version for the envelope subjects.
/// Bumped only on shape-incompatible changes.
pub const ENVELOPE_PAYLOAD_VERSION: u32 = 1;

/// Parse an envelope_observed JSON value (as returned by
/// `SubjectStateSubscriber.current_state` / a stream
/// update's `state` field). Returns `None` when the value
/// is not a well-formed envelope payload — callers treat
/// `None` as "no usable observation; continue awaiting."
pub fn parse_envelope_observed(
    state: &serde_json::Value,
) -> Option<EnvelopeObserved> {
    serde_json::from_value(state.clone()).ok()
}

/// Parse an envelope_requested JSON value. Symmetric to
/// [`parse_envelope_observed`].
pub fn parse_envelope_requested(
    state: &serde_json::Value,
) -> Option<EnvelopeRequested> {
    serde_json::from_value(state.clone()).ok()
}

/// Predicate: does the observed payload match the requested
/// `(generation, state)` tuple? Used by the orchestrator's
/// await loop.
pub fn observation_matches(
    observed: &EnvelopeObserved,
    expected_generation: u64,
    expected_state: EnvelopeState,
) -> bool {
    observed.generation == expected_generation
        && observed.observed_state == expected_state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_requested_round_trips_through_json() {
        let req = EnvelopeRequested {
            v: ENVELOPE_PAYLOAD_VERSION,
            generation: 42,
            requested_state: EnvelopeState::Muted,
            requested_at_ms: 1_700_000_000_000,
        };
        let v = serde_json::to_value(&req).unwrap();
        let parsed = parse_envelope_requested(&v).expect("parses");
        assert_eq!(parsed, req);
    }

    #[test]
    fn envelope_observed_round_trips_through_json() {
        let obs = EnvelopeObserved {
            v: ENVELOPE_PAYLOAD_VERSION,
            generation: 42,
            observed_state: EnvelopeState::Muted,
            observed_at_ms: 1_700_000_000_100,
        };
        let v = serde_json::to_value(&obs).unwrap();
        let parsed = parse_envelope_observed(&v).expect("parses");
        assert_eq!(parsed, obs);
    }

    #[test]
    fn observation_matches_on_pair_or_rejects() {
        let obs = EnvelopeObserved {
            v: 1,
            generation: 7,
            observed_state: EnvelopeState::Muted,
            observed_at_ms: 0,
        };
        assert!(observation_matches(&obs, 7, EnvelopeState::Muted));
        // Wrong generation.
        assert!(!observation_matches(&obs, 8, EnvelopeState::Muted));
        // Wrong state.
        assert!(!observation_matches(&obs, 7, EnvelopeState::Unmuted));
    }

    #[test]
    fn parse_returns_none_on_malformed_json() {
        let v = serde_json::json!({ "garbage": "not an envelope" });
        assert!(parse_envelope_observed(&v).is_none());
        assert!(parse_envelope_requested(&v).is_none());
    }

    #[test]
    fn envelope_state_serialises_snake_case() {
        let muted = serde_json::to_string(&EnvelopeState::Muted).unwrap();
        assert_eq!(muted, "\"muted\"");
        let unmuted = serde_json::to_string(&EnvelopeState::Unmuted).unwrap();
        assert_eq!(unmuted, "\"unmuted\"");
    }
}
