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

use crate::fft::{PerceptualFrame, BIN_COUNT, CHANNEL_COUNT};

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
) {
    let addressing = ExternalAddressing::new(
        SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let announcement =
        SubjectAnnouncement::new(SPECTRUM_SUBJECT_TYPE, vec![addressing])
            .with_state(render_empty_frame(rate_hz));
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
    if let Err(e) = announcer.update_state(addressing, state).await {
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
    // Split the channel-interleaved magnitudes / peak_hold back
    // into the two-arrays-of-256 wire shape the renderer expects.
    let mut mags_l = Vec::with_capacity(BIN_COUNT);
    let mut mags_r = Vec::with_capacity(BIN_COUNT);
    let mut peak_l = Vec::with_capacity(BIN_COUNT);
    let mut peak_r = Vec::with_capacity(BIN_COUNT);
    for i in 0..BIN_COUNT {
        mags_l.push(frame.magnitudes[i]);
        mags_r.push(frame.magnitudes[i + BIN_COUNT]);
        peak_l.push(frame.peak_hold[i]);
        peak_r.push(frame.peak_hold[i + BIN_COUNT]);
    }
    let correlation: Vec<f32> = frame.correlation.to_vec();

    json!({
        "v": SPECTRUM_PAYLOAD_VERSION,
        "bins": BIN_COUNT,
        "channels": CHANNEL_COUNT,
        "rate_hz": rate_hz,
        "magnitudes": [mags_l, mags_r],
        "peak_hold": [peak_l, peak_r],
        "onsets": {
            "sub_bass": frame.onsets.sub_bass,
            "bass": frame.onsets.bass,
            "mid": frame.onsets.mid,
            "high": frame.onsets.high,
        },
        "correlation": correlation,
        "at_ms": frame.at_ms,
    })
}

/// Render the empty-frame shape returned by `get_spectrum_frame`
/// before the capture loop has computed its first frame. Same
/// wire shape, all-zero magnitudes + peak_hold, all-false
/// onsets, zero correlation, `at_ms: 0`. Renderers handle this
/// as "silent" — the visual renders idle.
///
/// `rate_hz` matches the value `render_spectrum_frame` would
/// emit once the capture loop computes its first frame — single
/// source of truth via `fft::frame_rate_hz`.
pub fn render_empty_frame(rate_hz: u32) -> serde_json::Value {
    let zero_bins: Vec<f32> = vec![0.0; BIN_COUNT];
    let zero_corr: Vec<f32> = vec![0.0; BIN_COUNT];
    json!({
        "v": SPECTRUM_PAYLOAD_VERSION,
        "bins": BIN_COUNT,
        "channels": CHANNEL_COUNT,
        "rate_hz": rate_hz,
        "magnitudes": [zero_bins.clone(), zero_bins.clone()],
        "peak_hold": [zero_bins.clone(), zero_bins],
        "onsets": {
            "sub_bass": false,
            "bass": false,
            "mid": false,
            "high": false,
        },
        "correlation": zero_corr,
        "at_ms": 0u64,
    })
}

/// Thin wrapper bundling the announcer, a stable addressing,
/// and the wire `rate_hz` so callers don't re-construct any of
/// them on every emit. Mirrors the playback.mpd `SubjectEmitter`
/// shape.
#[allow(dead_code)]
pub struct SpectrumEmitter {
    announcer: Arc<dyn SubjectAnnouncer>,
    rate_hz: u32,
}

impl SpectrumEmitter {
    /// Construct an emitter bound to the supplied announcer.
    /// The announcer is the plugin's `LoadContext.subject_announcer`
    /// clone; cheap to wrap since SpectrumEmitter holds an Arc.
    /// `rate_hz` is the value the wire field carries on every emit
    /// — derived once via `fft::frame_rate_hz(sample_rate_hz)` at
    /// emitter construction.
    pub fn new(announcer: Arc<dyn SubjectAnnouncer>, rate_hz: u32) -> Self {
        Self { announcer, rate_hz }
    }

    /// Announce the spectrum subject and seed its initial state.
    /// Called once at plugin load; subsequent state changes go
    /// through `emit`. The emitter's cached `rate_hz` threads
    /// into the initial empty-frame envelope so subscribers
    /// connecting before the first FFT compute see the wire
    /// shape immediately.
    pub async fn announce(&self) {
        announce_initial_state(&self.announcer, self.rate_hz).await;
    }

    /// Publish a fresh spectrum frame on the subject. Best
    /// effort: announcer errors log at debug and do not fail
    /// the capture loop.
    pub async fn emit(&self, frame: &PerceptualFrame) {
        emit_frame(&self.announcer, frame, self.rate_hz).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{OnsetFrame, BIN_COUNT, CHANNEL_COUNT};
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

    fn make_frame(
        magnitude_value: f32,
        peak_value: f32,
        onsets: OnsetFrame,
        at_ms: u64,
    ) -> PerceptualFrame {
        let mut mags = Box::new([0.0f32; BIN_COUNT * CHANNEL_COUNT]);
        let mut peak = Box::new([0.0f32; BIN_COUNT * CHANNEL_COUNT]);
        let mut corr = Box::new([0.0f32; BIN_COUNT]);
        for i in 0..BIN_COUNT * CHANNEL_COUNT {
            mags[i] = magnitude_value;
            peak[i] = peak_value;
        }
        for i in 0..BIN_COUNT {
            corr[i] = 0.5;
        }
        PerceptualFrame {
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
    fn render_spectrum_frame_emits_v1_envelope() {
        let frame = make_frame(0.5, 0.7, OnsetFrame::default(), 1_000);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(v["bins"], BIN_COUNT);
        assert_eq!(v["channels"], CHANNEL_COUNT);
        assert_eq!(v["rate_hz"], TEST_RATE_HZ);
        assert_eq!(v["at_ms"], 1_000);
    }

    #[test]
    fn render_spectrum_frame_rate_hz_reflects_parameter() {
        // The wire `rate_hz` field MUST be parameter-driven — not
        // a hardcoded literal. Three distinct rates, three
        // distinct wire values; would have failed under the
        // hardcoded-30 form regardless of input.
        let frame = make_frame(0.0, 0.0, OnsetFrame::default(), 0);
        assert_eq!(render_spectrum_frame(&frame, 30)["rate_hz"], 30);
        assert_eq!(render_spectrum_frame(&frame, 47)["rate_hz"], 47);
        assert_eq!(render_spectrum_frame(&frame, 94)["rate_hz"], 94);
    }

    #[test]
    fn render_empty_frame_rate_hz_reflects_parameter() {
        assert_eq!(render_empty_frame(30)["rate_hz"], 30);
        assert_eq!(render_empty_frame(47)["rate_hz"], 47);
        assert_eq!(render_empty_frame(94)["rate_hz"], 94);
    }

    #[test]
    fn render_spectrum_frame_splits_channels_into_two_arrays() {
        let frame = make_frame(0.5, 0.7, OnsetFrame::default(), 2_000);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        let mags = v["magnitudes"].as_array().expect("magnitudes is array");
        assert_eq!(mags.len(), 2, "two channels");
        let l = mags[0].as_array().expect("L is array");
        let r = mags[1].as_array().expect("R is array");
        assert_eq!(l.len(), BIN_COUNT);
        assert_eq!(r.len(), BIN_COUNT);
        for i in 0..BIN_COUNT {
            assert_eq!(l[i].as_f64(), Some(0.5));
            assert_eq!(r[i].as_f64(), Some(0.5));
        }
    }

    #[test]
    fn render_spectrum_frame_carries_peak_hold() {
        let frame = make_frame(0.0, 0.9, OnsetFrame::default(), 0);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        let peak = v["peak_hold"].as_array().expect("peak_hold is array");
        assert_eq!(peak.len(), 2);
        let l = peak[0].as_array().unwrap();
        assert_eq!(l.len(), BIN_COUNT);
        assert!((l[0].as_f64().unwrap() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn render_spectrum_frame_carries_onsets() {
        let onsets = OnsetFrame {
            sub_bass: true,
            bass: false,
            mid: true,
            high: false,
        };
        let frame = make_frame(0.0, 0.0, onsets, 0);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(v["onsets"]["sub_bass"], true);
        assert_eq!(v["onsets"]["bass"], false);
        assert_eq!(v["onsets"]["mid"], true);
        assert_eq!(v["onsets"]["high"], false);
    }

    #[test]
    fn render_spectrum_frame_carries_correlation() {
        let frame = make_frame(0.0, 0.0, OnsetFrame::default(), 0);
        let v = render_spectrum_frame(&frame, TEST_RATE_HZ);
        let corr = v["correlation"].as_array().expect("correlation is array");
        assert_eq!(corr.len(), BIN_COUNT);
        for sample in corr.iter().take(BIN_COUNT) {
            assert!((sample.as_f64().unwrap() - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn render_empty_frame_matches_v1_shape() {
        let v = render_empty_frame(TEST_RATE_HZ);
        assert_eq!(v["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(v["bins"], BIN_COUNT);
        assert_eq!(v["channels"], CHANNEL_COUNT);
        assert_eq!(v["rate_hz"], TEST_RATE_HZ);
        assert_eq!(v["at_ms"], 0);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 2);
        let l = mags[0].as_array().unwrap();
        assert!(l.iter().all(|x| x.as_f64() == Some(0.0)));
    }

    #[test]
    fn render_is_deterministic_for_identical_input() {
        let frame = make_frame(0.3, 0.4, OnsetFrame::default(), 12345);
        let a = render_spectrum_frame(&frame, TEST_RATE_HZ);
        let b = render_spectrum_frame(&frame, TEST_RATE_HZ);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn announce_initial_state_seeds_full_envelope_in_announcement() {
        // Subscribers connecting between plugin load and first
        // FFT compute must see the full wire shape immediately —
        // no separate `get_spectrum_frame` round-trip required.
        // The announcement carries the empty-frame envelope via
        // SubjectAnnouncement::with_state; the framework stores
        // non-null announcement state on the subject record.
        let cap = Arc::new(CapturingAnnouncer::new());
        let announcer: Arc<dyn SubjectAnnouncer> = cap.clone();
        announce_initial_state(&announcer, TEST_RATE_HZ).await;

        let announced = cap.announced();
        assert_eq!(announced.len(), 1, "exactly one announce call");
        let a = &announced[0];
        assert_eq!(a.subject_type, SPECTRUM_SUBJECT_TYPE);

        let state = &a.state;
        assert!(!state.is_null(), "announcement state MUST be non-null");
        assert_eq!(state["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(state["bins"], BIN_COUNT);
        assert_eq!(state["channels"], CHANNEL_COUNT);
        assert_eq!(state["rate_hz"], TEST_RATE_HZ);
        assert_eq!(state["at_ms"], 0);

        let mags = state["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 2, "stereo magnitudes");
        let l = mags[0].as_array().unwrap();
        assert_eq!(l.len(), BIN_COUNT);
        assert!(l.iter().all(|v| v.as_f64() == Some(0.0)));

        let peak = state["peak_hold"].as_array().unwrap();
        assert_eq!(peak.len(), 2);
        let pl = peak[0].as_array().unwrap();
        assert_eq!(pl.len(), BIN_COUNT);
        assert!(pl.iter().all(|v| v.as_f64() == Some(0.0)));

        assert_eq!(state["onsets"]["sub_bass"], false);
        assert_eq!(state["onsets"]["bass"], false);
        assert_eq!(state["onsets"]["mid"], false);
        assert_eq!(state["onsets"]["high"], false);

        let corr = state["correlation"].as_array().unwrap();
        assert_eq!(corr.len(), BIN_COUNT);
        assert!(corr.iter().all(|v| v.as_f64() == Some(0.0)));
    }
}
