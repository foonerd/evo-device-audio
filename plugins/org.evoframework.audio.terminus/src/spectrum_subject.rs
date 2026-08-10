// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Spectrum subject emitter + payload rendering.
//!
//! Owns the wire-side projection of the FFT module's
//! `PerceptualFrame` onto the `audio_playback_spectrum_frame`
//! subject's state payload. The subject's addressing rides the
//! same `evo.audio.playback` scheme the shipped `now_playing`
//! subject uses (consistent operator-facing surface; the
//! audio.playback scheme is the operator's audio-state
//! addressing root) with value `spectrum_frame`.
//!
//! Payload v1 shape:
//!
//! ```text
//! { v: 1,
//!   bins: 256,
//!   channels: 2,
//!   rate_hz: <actual capture-loop cadence, computed from
//!            sample_rate_hz / FFT_SIZE via fft::frame_rate_hz>,
//!   magnitudes: [[256 f32 L], [256 f32 R]],
//!   peak_hold:  [[256 f32 L], [256 f32 R]],
//!   onsets:     { sub_bass, bass, mid, high },
//!   correlation: [256 f32],
//!   at_ms: <emit timestamp> }
//! ```
//!
//! Pure projection — no I/O, no state. The capture loop calls
//! `update_state` on every successful FFT compute; the
//! `get_spectrum_frame` read handler calls `render_spectrum_frame`
//! against the latest cached `PerceptualFrame`.

use std::sync::Arc;

use evo_plugin_sdk::contract::{
    ExternalAddressing, SubjectAnnouncement, SubjectAnnouncer,
};
use serde_json::json;

use crate::fft::PerceptualFrame;

const PLUGIN_NAME: &str = "org.evoframework.audio.terminus";

/// Subject-type catalogue identifier. Declared in the
/// evo-device-audio catalogue alongside
/// `audio_playback_now_playing` + `audio_playback_stream_format`;
/// the shared `audio_playback_` prefix groups every audio-state
/// subject under one operator-facing namespace.
pub const SPECTRUM_SUBJECT_TYPE: &str = "audio_playback_spectrum_frame";

/// Subject-addressing scheme. Matches the now_playing subject's
/// scheme exactly so a single subscription pattern works across
/// the audio-state subjects.
pub const SPECTRUM_SUBJECT_ADDRESSING_SCHEME: &str = "evo.audio.playback";

/// Subject-addressing value (instance). Singleton — the terminus
/// is a singleton, the subject is a singleton.
pub const SPECTRUM_SUBJECT_ADDRESSING_VALUE: &str = "spectrum_frame";

/// Subject payload version. Increment + version-discriminate when
/// breaking the wire contract; renderers MUST honour the
/// version field.
pub const SPECTRUM_PAYLOAD_VERSION: u32 = 1;

/// Announce the spectrum subject and seed its initial state
/// with the empty-frame envelope. Called at plugin load before
/// the capture loop starts. Best effort: announcer failures log
/// a warn and don't refuse the load (the operator UI degrades
/// to no-spectrum, not crash).
///
/// The announcement carries the complete wire-shape envelope
/// (`v`, `bins`, `channels`, `rate_hz`, zero-valued magnitudes /
/// peak_hold, all-false onsets, zero correlation, `at_ms: 0`)
/// via `SubjectAnnouncement::with_state`. The framework stores
/// non-null announcement state on the subject record (see
/// `evo::subjects::ingest`), so subscribers connecting after the
/// announce but before the first FFT compute see the wire shape
/// immediately — no separate `get_spectrum_frame` round-trip
/// needed to learn the bin count / channel count / rate.
///
/// `rate_hz` is the value the first real frame will also carry
/// — single source of truth via `fft::frame_rate_hz`.
pub async fn announce_initial_state(
    announcer: &Arc<dyn SubjectAnnouncer>,
    rate_hz: u32,
    bins: u32,
    channels: u32,
) {
    let addressing = ExternalAddressing::new(
        SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let announcement =
        SubjectAnnouncement::new(SPECTRUM_SUBJECT_TYPE, vec![addressing])
            .with_state(render_empty_frame(rate_hz, bins, channels));
    if let Err(e) = announcer.announce(announcement).await {
        tracing::warn!(
            plugin = PLUGIN_NAME,
            error = %e,
            "spectrum_frame subject announcement failed; operator UI \
             visualiser will be unavailable until the next admit"
        );
    }
}

/// Emit a new spectrum-frame state to subscribers. Capture-loop
/// calls this on every successful FFT compute. Best effort: an
/// announcer error logs at debug (every-frame errors at warn
/// would flood) and is otherwise silent; the next frame retries.
///
/// `rate_hz` is the capture loop's actual cadence (derived once
/// at loop construction via `fft::frame_rate_hz`); it threads
/// through to the wire `rate_hz` field so subscribers see the
/// cadence they actually receive frames at.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
pub async fn emit_frame(
    announcer: &Arc<dyn SubjectAnnouncer>,
    frame: &PerceptualFrame,
    rate_hz: u32,
) {
    let addressing = ExternalAddressing::new(
        SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let state = render_spectrum_frame(frame, rate_hz);
    // Volatile emission — the spectrum subject is high-rate
    // telemetry (30 Hz sustained during playback); mirroring
    // every emit into the framework's durable `subject_states`
    // table would issue ~108k sqlite writes per hour of playback
    // for zero operator-visible payoff. The volatile path skips
    // the durable persist entirely while preserving the
    // in-memory update + `SubjectStateChanged` happening
    // emission that wire subscribers observe.
    if let Err(e) = announcer.update_state_volatile(addressing, state).await {
        tracing::debug!(
            plugin = PLUGIN_NAME,
            error = %e,
            "spectrum_frame update_state failed; next-frame retries"
        );
    }
}

/// Pure projection: render a `PerceptualFrame` into the v1 wire
/// payload. Public-within-crate for testing + reuse by the
/// `get_spectrum_frame` read handler (which projects the same
/// way against the latest cached frame).
///
/// `rate_hz` is the cadence value the caller derives via
/// `fft::frame_rate_hz(sample_rate_hz)` — single source of truth
/// for the wire field. No default; callers supply.
pub fn render_spectrum_frame(
    frame: &PerceptualFrame,
    rate_hz: u32,
) -> serde_json::Value {
    // Frame shape is the analyser's actual state at compute
    // time — bins + channels come from the frame itself, not
    // from any external constant. This is the payload-truth
    // invariant: consumers read shape per-frame, not from the
    // demand subject.
    let bins = frame.bins as usize;
    let channels = frame.channels as usize;
    let mags = split_channels(&frame.magnitudes, bins, channels);
    let peaks = split_channels(&frame.peak_hold, bins, channels);
    let correlation: Vec<f32> = frame.correlation.to_vec();

    json!({
        "v":          SPECTRUM_PAYLOAD_VERSION,
        "bins":       frame.bins,
        "channels":   frame.channels,
        "rate_hz":    rate_hz,
        "magnitudes": mags,
        "peak_hold":  peaks,
        "onsets": {
            "sub_bass": frame.onsets.sub_bass,
            "bass":     frame.onsets.bass,
            "mid":      frame.onsets.mid,
            "high":     frame.onsets.high,
        },
        "correlation": correlation,
        "at_ms":       frame.at_ms,
    })
}

/// Slice a flat channel-major buffer into a `[channel][bin]`
/// nested array. For `channels = 1` this returns `[[bins...]]`;
/// for `channels = 2` this returns `[[L...], [R...]]`. Matches
/// the wire shape UI consumers parse for the `magnitudes` /
/// `peak_hold` fields.
fn split_channels(flat: &[f32], bins: usize, channels: usize) -> Vec<Vec<f32>> {
    (0..channels)
        .map(|ch| {
            let start = ch * bins;
            let end = start + bins;
            flat[start..end].to_vec()
        })
        .collect()
}

/// Render the empty-frame shape returned by `get_spectrum_frame`
/// before the capture loop has computed its first frame. Same
/// wire shape as a real frame, sized to the current analyser
/// demand (`bins` + `channels`) so consumers subscribing before
/// the first FFT compute learn the shape immediately from the
/// seeded state.
///
/// `rate_hz` matches the value `render_spectrum_frame` would
/// emit once the capture loop computes its first frame — single
/// source of truth via `fft::frame_rate_hz`.
///
/// `bins` and `channels` are the current demand's output shape;
/// consumers subscribing between an announce and the first live
/// frame see this shape and can size their decoder buffers
/// without waiting for a live frame.
pub fn render_empty_frame(
    rate_hz: u32,
    bins: u32,
    channels: u32,
) -> serde_json::Value {
    let bins_us = bins as usize;
    let channels_us = channels as usize;
    let zero_per_channel: Vec<f32> = vec![0.0; bins_us];
    let per_channel: Vec<Vec<f32>> =
        (0..channels_us).map(|_| zero_per_channel.clone()).collect();
    // Correlation is only meaningful for stereo output; mono
    // demand carries a zero-length correlation array (matches
    // the live-frame shape).
    let zero_corr: Vec<f32> = if channels_us == 2 {
        vec![0.0; bins_us]
    } else {
        Vec::new()
    };
    json!({
        "v":          SPECTRUM_PAYLOAD_VERSION,
        "bins":       bins,
        "channels":   channels,
        "rate_hz":    rate_hz,
        "magnitudes": per_channel.clone(),
        "peak_hold":  per_channel,
        "onsets": {
            "sub_bass": false,
            "bass":     false,
            "mid":      false,
            "high":     false,
        },
        "correlation": zero_corr,
        "at_ms":       0u64,
    })
}

// `SpectrumEmitter` previously wrapped the announcer + rate for
// a shorter emit call-site; the capture loop calls `emit_frame`
// directly with the demand-driven shape now, so the wrapper is
// unused. Reintroduce when the API stabilises around a shape
// worth wrapping.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::OnsetFrame;
    use evo_plugin_sdk::contract::ReportError;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    /// Capturing announcer for testing. Records every
    /// `announce` and `update_state` call so tests can assert
    /// what the production code emitted onto the wire.
    struct CapturingAnnouncer {
        announced: Mutex<Vec<SubjectAnnouncement>>,
    }

    impl CapturingAnnouncer {
        fn new() -> Self {
            Self {
                announced: Mutex::new(Vec::new()),
            }
        }

        fn announced(&self) -> Vec<SubjectAnnouncement> {
            self.announced.lock().unwrap().clone()
        }
    }

    impl SubjectAnnouncer for CapturingAnnouncer {
        fn announce<'a>(
            &'a self,
            announcement: SubjectAnnouncement,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.announced.lock().unwrap().push(announcement);
                Ok(())
            })
        }

        fn retract<'a>(
            &'a self,
            _: ExternalAddressing,
            _: Option<String>,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }

        fn update_state<'a>(
            &'a self,
            _: ExternalAddressing,
            _: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<(), ReportError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }
    }

    /// Build a synthetic PerceptualFrame at the given
    /// (bins, channels) shape with every magnitude/peak/corr
    /// value set uniformly. Used by the render-shape tests to
    /// pin the wire projection for the operator-visible enums.
    fn make_frame(
        bins: u32,
        channels: u32,
        magnitude_value: f32,
        peak_value: f32,
        onsets: OnsetFrame,
        at_ms: u64,
    ) -> PerceptualFrame {
        let output_len = (bins as usize) * (channels as usize);
        let mags: Box<[f32]> =
            vec![magnitude_value; output_len].into_boxed_slice();
        let peak: Box<[f32]> = vec![peak_value; output_len].into_boxed_slice();
        // Correlation only when channels==2 (mirrors the analyser).
        let corr: Box<[f32]> = if channels == 2 {
            vec![0.5; bins as usize].into_boxed_slice()
        } else {
            Vec::new().into_boxed_slice()
        };
        PerceptualFrame {
            bins,
            channels,
            magnitudes: mags,
            peak_hold: peak,
            onsets,
            correlation: corr,
            at_ms,
        }
    }

    /// Canonical reference rate used across these tests. Matches
    /// `fft::frame_rate_hz(48_000)` (48 kHz / 1024-point FFT
    /// rounds to 47) which is what every reference rig emits on
    /// the wire today.
    const TEST_RATE_HZ: u32 = 47;

    #[test]
    fn render_spectrum_frame_emits_v1_envelope_stereo_256() {
        let frame = make_frame(256, 2, 0.5, 0.7, OnsetFrame::default(), 1_000);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(v["bins"], 256);
        assert_eq!(v["channels"], 2);
        assert_eq!(v["rate_hz"], TEST_RATE_HZ);
        assert_eq!(v["at_ms"], 1_000);
    }

    #[test]
    fn render_spectrum_frame_emits_shape_matching_frame_mono_64() {
        // Frame at demand.bins=64, channels=1 — the wire MUST
        // reflect the frame's shape, not any hardcoded constant.
        let frame = make_frame(64, 1, 0.3, 0.4, OnsetFrame::default(), 500);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["bins"], 64);
        assert_eq!(v["channels"], 1);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 1, "mono → one channel");
        let mono = mags[0].as_array().unwrap();
        assert_eq!(mono.len(), 64);
        // Correlation array is empty on mono demand.
        assert_eq!(v["correlation"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_spectrum_frame_emits_shape_matching_frame_stereo_32() {
        let frame = make_frame(32, 2, 0.1, 0.2, OnsetFrame::default(), 0);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["bins"], 32);
        assert_eq!(v["channels"], 2);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 2);
        assert_eq!(mags[0].as_array().unwrap().len(), 32);
        assert_eq!(mags[1].as_array().unwrap().len(), 32);
        assert_eq!(v["correlation"].as_array().unwrap().len(), 32);
    }

    #[test]
    fn render_spectrum_frame_rate_hz_reflects_parameter() {
        // The wire `rate_hz` field MUST be parameter-driven — not
        // a hardcoded literal. Three distinct rates, three
        // distinct wire values.
        let frame = make_frame(64, 1, 0.0, 0.0, OnsetFrame::default(), 0);
        assert_eq!(render_spectrum_frame(&frame, 30)["rate_hz"], 30);
        assert_eq!(render_spectrum_frame(&frame, 47)["rate_hz"], 47);
        assert_eq!(render_spectrum_frame(&frame, 94)["rate_hz"], 94);
    }

    #[test]
    fn render_empty_frame_carries_requested_shape() {
        // Empty frame at 32×2:
        let v = render_empty_frame(30, 32, 2);
        assert_eq!(v["bins"], 32);
        assert_eq!(v["channels"], 2);
        assert_eq!(v["rate_hz"], 30);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 2);
        assert_eq!(mags[0].as_array().unwrap().len(), 32);
        assert_eq!(v["correlation"].as_array().unwrap().len(), 32);

        // Empty frame at 128×1 (mono):
        let v = render_empty_frame(30, 128, 1);
        assert_eq!(v["bins"], 128);
        assert_eq!(v["channels"], 1);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 1);
        assert_eq!(mags[0].as_array().unwrap().len(), 128);
        // Mono → zero-length correlation.
        assert_eq!(v["correlation"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn render_spectrum_frame_carries_onsets() {
        let onsets = OnsetFrame {
            sub_bass: true,
            bass: false,
            mid: true,
            high: false,
        };
        let frame = make_frame(64, 2, 0.0, 0.0, onsets, 0);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["onsets"]["sub_bass"], true);
        assert_eq!(v["onsets"]["bass"], false);
        assert_eq!(v["onsets"]["mid"], true);
        assert_eq!(v["onsets"]["high"], false);
    }

    #[test]
    fn render_is_deterministic_for_identical_input() {
        let frame = make_frame(128, 2, 0.3, 0.4, OnsetFrame::default(), 12345);
        let a = render_spectrum_frame(&frame, TEST_RATE_HZ);
        let b = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn announce_initial_state_seeds_envelope_at_requested_shape() {
        // Subscribers connecting between plugin load and the
        // first FFT compute must see the full wire shape
        // immediately — no separate `get_spectrum_frame`
        // round-trip required. The announcement carries the
        // empty-frame envelope via SubjectAnnouncement::with_state;
        // the framework stores non-null announcement state on
        // the subject record.
        let cap = Arc::new(CapturingAnnouncer::new());
        let announcer: Arc<dyn SubjectAnnouncer> = cap.clone();
        // Seed at the disabled-default demand (64 bins, mono) so
        // the announced envelope reflects what the first live
        // frame will carry after the operator opts in.
        announce_initial_state(&announcer, TEST_RATE_HZ, 64, 1).await;

        let announced = cap.announced();
        assert_eq!(announced.len(), 1, "exactly one announce call");
        let a = &announced[0];
        assert_eq!(a.subject_type, SPECTRUM_SUBJECT_TYPE);

        let state = &a.state;
        assert!(!state.is_null(), "announcement state MUST be non-null");
        assert_eq!(state["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(state["bins"], 64);
        assert_eq!(state["channels"], 1);
        assert_eq!(state["rate_hz"], TEST_RATE_HZ);
        assert_eq!(state["at_ms"], 0);

        let mags = state["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 1, "mono seed → one channel");
        let mono = mags[0].as_array().unwrap();
        assert_eq!(mono.len(), 64);
        assert!(mono.iter().all(|v| v.as_f64() == Some(0.0)));

        // Correlation empty on mono seed (matches the live-frame
        // shape on mono demand).
        assert_eq!(state["correlation"].as_array().unwrap().len(), 0);
    }
}
