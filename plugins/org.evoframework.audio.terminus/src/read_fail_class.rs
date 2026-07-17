// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Classification of the ALSA read/recover failure inside the
//! capture inner loop.
//!
//! Pure logic — no ALSA dependency — so this module compiles and
//! tests on any host regardless of the `alsa-substrate` feature.
//! The capture loop (feature-gated on `alsa-substrate`) uses this
//! classifier at every read failure to pick the tracing level; the
//! classifier is the single source of truth for level selection
//! so the two call sites in `capture.rs` (`pcm.recover()` failure
//! and sustained read failures) cannot drift.
//!
//! The classification composes two signals available at failure
//! time:
//!
//! - `frames_processed_this_session > 0` — whether the current
//!   inner-loop session already produced at least one FFT frame.
//!   When zero the loop just opened the PCM handle; a "recover
//!   failed" outcome here is the well-known hardware-less-target
//!   cycling pattern that the outer loop already debug-classes
//!   on `InnerExit::TransportFailed`.
//!
//! - `transport_playing` — whether the transport gate is
//!   `should_emit()` at the failure moment. When true the
//!   operator is actively playing music; the capture just broke
//!   mid-flight and the fault is operator-visible. When false the
//!   transport has already transitioned to `NotPlaying`
//!   (typically because the loopback writer — MPD — shut down
//!   during a normal idle-time socket-activation flap) and the
//!   readi failure is a downstream consequence of the expected
//!   lifecycle rather than a fault.

/// Log level to emit at for an ALSA read/recover failure inside
/// the capture inner loop. `ReadFailClass::Warn` maps to
/// `tracing::warn!`, `::Info` to `::info!`, `::Debug` to
/// `::debug!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadFailClass {
    /// Session was producing frames AND transport is still
    /// playing at failure time — real capture fault, operator
    /// is affected.
    Warn,
    /// Session was producing frames but transport already
    /// transitioned to NotPlaying at failure time — expected
    /// consequence of MPD idle-time socket-activation flap.
    Info,
    /// Session had not yet produced any frames — the
    /// just-opened-but-no-audio-yet cycling pattern the outer
    /// loop already debug-classes.
    Debug,
}

pub(crate) fn classify_read_failure(
    frames_processed_this_session: u64,
    transport_playing: bool,
) -> ReadFailClass {
    if frames_processed_this_session == 0 {
        ReadFailClass::Debug
    } else if transport_playing {
        ReadFailClass::Warn
    } else {
        ReadFailClass::Info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frames_is_debug_regardless_of_transport() {
        assert_eq!(
            classify_read_failure(0, true),
            ReadFailClass::Debug,
            "no-frames-yet is the just-opened cycling pattern; \
             transport state is irrelevant"
        );
        assert_eq!(classify_read_failure(0, false), ReadFailClass::Debug);
    }

    #[test]
    fn frames_and_playing_is_warn_mid_playback_fault() {
        assert_eq!(
            classify_read_failure(1, true),
            ReadFailClass::Warn,
            "session had frames and transport still says playing \
             — operator-visible capture fault"
        );
        assert_eq!(classify_read_failure(1_000_000, true), ReadFailClass::Warn);
    }

    #[test]
    fn frames_and_not_playing_is_info_expected_idle_transition() {
        // Regression fixture: the VM's chronic MPD socket-activation
        // churn during idle used to fire WARN because the classifier
        // only looked at frames_processed_this_session. Consulting
        // transport_gate at failure time downgrades to INFO — the
        // MPD stop IS the reason readi failed, and the transport
        // gate already reflects the transition.
        assert_eq!(
            classify_read_failure(1, false),
            ReadFailClass::Info,
            "session had frames but transport transitioned to \
             NotPlaying — expected consequence of MPD idle-time \
             lifecycle, not a fault"
        );
        assert_eq!(classify_read_failure(30, false), ReadFailClass::Info);
    }
}
