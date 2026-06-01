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
//!   rate_hz: 30,
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

/// Announce the spectrum subject in its initial empty state.
/// Called at plugin load before the capture loop starts. Best
/// effort: announcer failures log a warn and don't refuse the
/// load (the operator UI degrades to no-spectrum, not crash).
pub async fn announce_initial_state(announcer: &Arc<dyn SubjectAnnouncer>) {
    let addressing = ExternalAddressing::new(
        SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let announcement =
        SubjectAnnouncement::new(SPECTRUM_SUBJECT_TYPE, vec![addressing]);
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
pub async fn emit_frame(
    announcer: &Arc<dyn SubjectAnnouncer>,
    frame: &PerceptualFrame,
) {
    let addressing = ExternalAddressing::new(
        SPECTRUM_SUBJECT_ADDRESSING_SCHEME,
        SPECTRUM_SUBJECT_ADDRESSING_VALUE,
    );
    let state = render_spectrum_frame(frame);
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
pub fn render_spectrum_frame(frame: &PerceptualFrame) -> serde_json::Value {
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
        "rate_hz": 30,
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
pub fn render_empty_frame() -> serde_json::Value {
    let zero_bins: Vec<f32> = vec![0.0; BIN_COUNT];
    let zero_corr: Vec<f32> = vec![0.0; BIN_COUNT];
    json!({
        "v": SPECTRUM_PAYLOAD_VERSION,
        "bins": BIN_COUNT,
        "channels": CHANNEL_COUNT,
        "rate_hz": 30,
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

/// Thin wrapper bundling the announcer + a stable addressing
/// so callers don't re-construct the addressing on every emit.
/// Mirrors the playback.mpd `SubjectEmitter` shape.
#[allow(dead_code)]
pub struct SpectrumEmitter {
    announcer: Arc<dyn SubjectAnnouncer>,
}

impl SpectrumEmitter {
    /// Construct an emitter bound to the supplied announcer.
    /// The announcer is the plugin's `LoadContext.subject_announcer`
    /// clone; cheap to wrap since SpectrumEmitter holds an Arc.
    pub fn new(announcer: Arc<dyn SubjectAnnouncer>) -> Self {
        Self { announcer }
    }

    /// Announce the spectrum subject in its initial empty state.
    /// Called once at plugin load; subsequent state changes go
    /// through `emit`.
    pub async fn announce(&self) {
        announce_initial_state(&self.announcer).await;
    }

    /// Publish a fresh spectrum frame on the subject. Best
    /// effort: announcer errors log at debug and do not fail
    /// the capture loop.
    pub async fn emit(&self, frame: &PerceptualFrame) {
        emit_frame(&self.announcer, frame).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{OnsetFrame, BIN_COUNT, CHANNEL_COUNT};

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

    #[test]
    fn render_spectrum_frame_emits_v1_envelope() {
        let frame = make_frame(0.5, 0.7, OnsetFrame::default(), 1_000);
        let v = render_spectrum_frame(&frame);
        assert_eq!(v["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(v["bins"], BIN_COUNT);
        assert_eq!(v["channels"], CHANNEL_COUNT);
        assert_eq!(v["rate_hz"], 30);
        assert_eq!(v["at_ms"], 1_000);
    }

    #[test]
    fn render_spectrum_frame_splits_channels_into_two_arrays() {
        let frame = make_frame(0.5, 0.7, OnsetFrame::default(), 2_000);
        let v = render_spectrum_frame(&frame);
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
        let v = render_spectrum_frame(&frame);
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
        let v = render_spectrum_frame(&frame);
        assert_eq!(v["onsets"]["sub_bass"], true);
        assert_eq!(v["onsets"]["bass"], false);
        assert_eq!(v["onsets"]["mid"], true);
        assert_eq!(v["onsets"]["high"], false);
    }

    #[test]
    fn render_spectrum_frame_carries_correlation() {
        let frame = make_frame(0.0, 0.0, OnsetFrame::default(), 0);
        let v = render_spectrum_frame(&frame);
        let corr = v["correlation"].as_array().expect("correlation is array");
        assert_eq!(corr.len(), BIN_COUNT);
        for i in 0..BIN_COUNT {
            assert!((corr[i].as_f64().unwrap() - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn render_empty_frame_matches_v1_shape() {
        let v = render_empty_frame();
        assert_eq!(v["v"], SPECTRUM_PAYLOAD_VERSION);
        assert_eq!(v["bins"], BIN_COUNT);
        assert_eq!(v["channels"], CHANNEL_COUNT);
        assert_eq!(v["at_ms"], 0);
        let mags = v["magnitudes"].as_array().unwrap();
        assert_eq!(mags.len(), 2);
        let l = mags[0].as_array().unwrap();
        assert!(l.iter().all(|x| x.as_f64() == Some(0.0)));
    }

    #[test]
    fn render_is_deterministic_for_identical_input() {
        let frame = make_frame(0.3, 0.4, OnsetFrame::default(), 12345);
        let a = render_spectrum_frame(&frame);
        let b = render_spectrum_frame(&frame);
        assert_eq!(a, b);
    }
}
