// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Byte-flow worker substrate.
//!
//! The composition plugin's reactor (in the parent module)
//! publishes endpoint snapshots onto a watch channel. This
//! module's worker consumes those snapshots and runs the
//! actual byte-flow loop: open the OS-native primitives the
//! framework has selected for the chain stage, and pump
//! audio frames from input to output.
//!
//! The substrate selection is per-endpoint-kind:
//!
//! - `EndpointKind::NamedPipe` — opens both endpoints as
//!   filesystem FIFOs via [`tokio::fs::OpenOptions`] and
//!   pumps bytes between them. Hermetic, testable on any
//!   Unix box; the production path for chains the framework
//!   wires through named-pipe substrate.
//! - `EndpointKind::AlsaPcm` — opens both endpoints as ALSA
//!   capture/playback PCMs via libasound (the `alsa` crate),
//!   configures hwparams from the negotiated `AudioFormat`,
//!   pumps frames input → output with cancel-aware
//!   non-blocking PCM waits. Compiled only when the
//!   `alsa-substrate` Cargo feature is enabled — typically
//!   for cross-builds targeting reference target / Debian Trixie /
//!   ALSA-having hardware. The dev-rig native build runs
//!   without the feature and reports AlsaPcm as
//!   `UnsupportedKind`.
//! - `EndpointKind::SharedMemory` / `JackPort` — reported as
//!   `UnsupportedKind`. Shared-memory and JACK-port
//!   substrates are vendor-distribution territory; the
//!   reference plugin in this build does not implement
//!   them.
//!
//! When the input and output endpoints declare different
//! substrate kinds, the worker reports `MixedSubstrate` —
//! the passthrough mode this build supports requires
//! homogeneous substrate. (Future modes that perform
//! transformation may bridge substrates; that's a per-mode
//! choice, not a passthrough invariant.)

use std::sync::Arc;

use evo_plugin_sdk::audio::{AudioFormat, PcmCodec};
use evo_plugin_sdk::contract::audio_routing::{
    CompositionEndpoints, EndpointKind,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{watch, Notify};

use crate::eq_dsp::{EqProcessor, EqSampleFormat};
use crate::{EqRuntimeState, MODE_EQ_ONLY};

/// Errors raised by the byte-flow substrate. Surfaced via
/// the worker status channel so observability surfaces
/// (test harness + operator UI) can render the failure
/// mode without losing structure.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ByteFlowError {
    /// The endpoints declared a substrate kind this build
    /// does not implement. Carries the kind for
    /// diagnostics. Surfaced as a structured operator-
    /// readable state, not a panic — the framework should
    /// not have selected an unsupported substrate, so a
    /// failure here indicates topology / vendor-build
    /// drift the operator needs.
    #[error("substrate kind not implemented in this build: {0:?}")]
    UnsupportedKind(EndpointKind),

    /// Input and output endpoints declared different
    /// substrate kinds; passthrough mode requires
    /// homogeneous substrate. Future composition modes
    /// (resampler, DSD-to-PCM converter) may bridge
    /// substrates; passthrough cannot.
    #[error(
        "input/output substrate kinds differ for passthrough \
         mode: input={input:?} output={output:?}"
    )]
    MixedSubstrate {
        /// Input endpoint substrate kind.
        input: EndpointKind,
        /// Output endpoint substrate kind.
        output: EndpointKind,
    },

    /// Failure opening the substrate primitive at the
    /// path/identifier the framework configured. The
    /// inner string carries the `std::io::Error`'s message
    /// alongside which endpoint failed.
    #[error("substrate open failed: {0}")]
    OpenFailed(String),

    /// Read from the input substrate failed mid-flow. The
    /// worker terminates the current substrate and waits
    /// for the next route change to retry.
    #[error("substrate read failed: {0}")]
    ReadFailed(String),

    /// Write to the output substrate failed mid-flow.
    /// Same recovery semantics as
    /// [`Self::ReadFailed`].
    #[error("substrate write failed: {0}")]
    WriteFailed(String),
}

/// Frame buffer size for the byte-flow loop. Tuned to keep
/// the syscall rate reasonable across sub-millisecond
/// audio formats (192 kHz stereo s24le ≈ 1.15 MB/s) without
/// making latency a concern (4 KiB ≈ 5 ms at the same
/// rate). Production builds may tune this against the
/// negotiated format's `buffer_frames` hint; the constant
/// here is a sensible default.
const PUMP_BUFFER_BYTES: usize = 4096;

/// How many times the ALSA substrate tries to open its two
/// PCMs before the failure is reported.
///
/// A rewire hands the output PCM to this stage while the source
/// that held it is still being restarted, so the first open can
/// find the device busy. A short, bounded window rides that out;
/// a device that stays unopenable for the whole window is a real
/// failure and is reported as one.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
const ALSA_OPEN_ATTEMPTS: u32 = 20;

/// Pause between two open attempts.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
const ALSA_OPEN_RETRY_DELAY: std::time::Duration =
    std::time::Duration::from_millis(500);

/// How much audio the ALSA pump keeps queued in the output ahead of
/// what is playing, in milliseconds.
///
/// The pump moves each buffer as it arrives, so without a cushion the
/// output holds only the few milliseconds written a moment ago. Any
/// stall of the pump thread longer than that (a busy box after a
/// restart, a scheduling hiccup) underruns the output, and the gap
/// shifts the rest of the stream: a click on a tone, a slip on music.
/// A cushion this long turns a stall shorter than it into nothing at
/// all, at the cost of this much delay through the EQ path.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
pub(crate) const OUTPUT_CUSHION_MS: u32 = 200;

/// The cushion in frames at `rate_hz`.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
pub(crate) fn cushion_frames(rate_hz: u32) -> usize {
    (u64::from(rate_hz) * u64::from(OUTPUT_CUSHION_MS) / 1000) as usize
}

/// Run `attempt` until it stops failing to open, at most
/// `attempts` times, pausing `delay` between tries.
///
/// Only [`ByteFlowError::OpenFailed`] is retried: a read or write
/// that fails mid-flow, or a kind the build does not drive, is
/// reported at once. `cancel` ends the wait immediately and the
/// call answers `Ok(())`, as a cancelled pump does. The last
/// error is returned when the attempts run out.
#[cfg_attr(not(feature = "alsa-substrate"), allow(dead_code))]
pub(crate) async fn retry_open<F, Fut>(
    attempts: u32,
    delay: std::time::Duration,
    cancel: Arc<Notify>,
    mut attempt: F,
) -> Result<(), ByteFlowError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), ByteFlowError>>,
{
    let mut tried = 0u32;
    loop {
        tried += 1;
        match attempt().await {
            Err(ByteFlowError::OpenFailed(reason)) if tried < attempts => {
                tracing::debug!(
                    attempt = tried,
                    attempts,
                    reason = %reason,
                    "substrate open failed; retrying"
                );
                tokio::select! {
                    biased;
                    _ = cancel.notified() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            other => return other,
        }
    }
}

/// The EQ branch of a pump: whether the cascade runs on the
/// frames about to be written, and the cascade itself.
///
/// One stage serves every substrate, so the rule is stated once:
/// the cascade runs only when the mode is `eq_only`, the EQ is
/// engaged, and the format is one the DSP covers (s16le, f32le;
/// mono or stereo). Anything else leaves the bytes alone.
pub(crate) struct EqStage {
    processor: Option<EqProcessor>,
    mode: String,
    engaged: bool,
}

impl EqStage {
    /// A stage for the negotiated input `format`, in `mode`, with
    /// the bands and engagement of `state`.
    pub(crate) fn new(
        format: &AudioFormat,
        mode: &str,
        state: &EqRuntimeState,
    ) -> Self {
        let (sample_fmt, channels, rate_hz) =
            eq_format_from_audio_format(format);
        let supported = !matches!(sample_fmt, EqSampleFormat::Other(_))
            && (1..=2).contains(&channels);
        let processor = supported.then(|| {
            let mut p = EqProcessor::new(channels, sample_fmt, rate_hz);
            p.configure_bands(&state.bands);
            p
        });
        Self {
            processor,
            mode: mode.to_string(),
            engaged: state.engaged,
        }
    }

    /// The operator changed the composition mode.
    pub(crate) fn set_mode(&mut self, mode: &str) {
        self.mode = mode.to_string();
    }

    /// The sounding EQ changed: take its bands in place (the
    /// filter memory is kept, so nothing pops) and its engagement.
    pub(crate) fn set_state(&mut self, state: &EqRuntimeState) {
        self.engaged = state.engaged;
        if let Some(p) = self.processor.as_mut() {
            p.configure_bands(&state.bands);
        }
    }

    /// True when the next buffer will go through the cascade.
    pub(crate) fn applies(&self) -> bool {
        self.mode == MODE_EQ_ONLY && self.engaged && self.processor.is_some()
    }

    /// Run the cascade over `buf` when [`Self::applies`], else
    /// leave it untouched. A buffer the DSP cannot take (a
    /// misaligned length) is a substrate-boundary fault and is
    /// reported as a failed read.
    pub(crate) fn apply(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(), ByteFlowError> {
        if !self.applies() {
            return Ok(());
        }
        let Some(p) = self.processor.as_mut() else {
            return Ok(());
        };
        p.process_bytes(buf)
            .map_err(|e| ByteFlowError::ReadFailed(format!("eq dsp: {e}")))
    }
}

/// Run the byte-flow substrate appropriate to the
/// endpoint pair. Returns `Ok(())` on graceful exit
/// (cancel signalled, or input EOF), `Err` on any
/// substrate-side failure. The caller (the worker)
/// reports the outcome to the status channel and waits
/// for the next snapshot or shutdown.
pub async fn run_substrate(
    endpoints: &CompositionEndpoints,
    mode_rx: watch::Receiver<String>,
    eq_state_rx: watch::Receiver<EqRuntimeState>,
    cancel: Arc<Notify>,
) -> Result<(), ByteFlowError> {
    if endpoints.input.kind != endpoints.output.kind {
        return Err(ByteFlowError::MixedSubstrate {
            input: endpoints.input.kind,
            output: endpoints.output.kind,
        });
    }
    match endpoints.input.kind {
        EndpointKind::NamedPipe => {
            run_named_pipe(endpoints.clone(), mode_rx, eq_state_rx, cancel)
                .await
        }
        #[cfg(feature = "alsa-substrate")]
        EndpointKind::AlsaPcm => {
            // The ALSA pump applies the same EQ stage as the
            // named-pipe pump: in `eq_only`, with the EQ
            // engaged and a format the DSP covers, frames go
            // through the cascade; otherwise they pass
            // byte-identical. Opening is retried for a bounded
            // window, because a rewire hands the DAC to this
            // stage while the source is still letting go of it.
            let ep = endpoints.clone();
            let attempt_cancel = Arc::clone(&cancel);
            retry_open(
                ALSA_OPEN_ATTEMPTS,
                ALSA_OPEN_RETRY_DELAY,
                cancel,
                move || {
                    crate::byte_flow_alsa::run_alsa_pcm(
                        ep.clone(),
                        mode_rx.clone(),
                        eq_state_rx.clone(),
                        Arc::clone(&attempt_cancel),
                    )
                },
            )
            .await
        }
        #[cfg(not(feature = "alsa-substrate"))]
        EndpointKind::AlsaPcm => {
            let _ = mode_rx;
            let _ = eq_state_rx;
            Err(ByteFlowError::UnsupportedKind(EndpointKind::AlsaPcm))
        }
        kind @ (EndpointKind::SharedMemory | EndpointKind::JackPort) => {
            let _ = mode_rx;
            let _ = eq_state_rx;
            Err(ByteFlowError::UnsupportedKind(kind))
        }
    }
}

/// Map an SDK [`AudioFormat`] onto an [`EqSampleFormat`] +
/// (channels, rate_hz) triple. PCM streams in s16le and f32le
/// are supported; other PCM codecs and DSD streams surface
/// the format string verbatim so the eq_only branch can refuse
/// the mode if engaged (or pass through unchanged if not).
fn eq_format_from_audio_format(
    fmt: &AudioFormat,
) -> (EqSampleFormat, usize, u32) {
    match fmt {
        AudioFormat::Pcm {
            codec,
            rate_hz,
            channels,
            ..
        } => {
            let sample_fmt = match codec {
                PcmCodec::PcmS16Le => EqSampleFormat::S16Le,
                PcmCodec::PcmF32 => EqSampleFormat::F32Le,
                PcmCodec::PcmS24Le => {
                    EqSampleFormat::Other("pcm_s24le".to_string())
                }
                PcmCodec::PcmS32Le => {
                    EqSampleFormat::Other("pcm_s32le".to_string())
                }
            };
            (sample_fmt, *channels as usize, *rate_hz)
        }
        other => (EqSampleFormat::Other(format!("{other:?}")), 2, 44100),
    }
}

/// Named-pipe substrate: open both endpoints as FIFOs at
/// their configured paths and pump bytes from input to
/// output until cancel fires or input reaches EOF.
///
/// The pump loop observes mode + EQ-state watch channels on
/// every iteration; when mode == `eq_only` AND
/// `eq_state.engaged = true` AND the negotiated format is one
/// the EQ DSP supports (s16le / f32le), the read buffer flows
/// through an [`EqProcessor`] before write. Other mode +
/// engagement + format combinations pass through unchanged
/// (the byte-identical passthrough path). One canonical pump
/// loop; mode selects the per-iteration branch.
async fn run_named_pipe(
    endpoints: CompositionEndpoints,
    mut mode_rx: watch::Receiver<String>,
    mut eq_state_rx: watch::Receiver<EqRuntimeState>,
    cancel: Arc<Notify>,
) -> Result<(), ByteFlowError> {
    let mut input = tokio::fs::OpenOptions::new()
        .read(true)
        .open(&endpoints.input.path)
        .await
        .map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "input fifo {}: {e}",
                endpoints.input.path.display()
            ))
        })?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&endpoints.output.path)
        .await
        .map_err(|e| {
            ByteFlowError::OpenFailed(format!(
                "output fifo {}: {e}",
                endpoints.output.path.display()
            ))
        })?;

    // The EQ stage for the negotiated input format. A format
    // outside the DSP's coverage collapses the stage to
    // passthrough so the substrate pumps cleanly even when the
    // operator selected eq_only on a stream the DSP does not
    // handle — the schema's `eq-only-sample-format-coverage`
    // acceptance row pins refusal at the mode-select gesture,
    // not at byte time; here we honour the safer fallthrough.
    let mut stage = {
        let mode = mode_rx.borrow_and_update().clone();
        let state = eq_state_rx.borrow_and_update().clone();
        EqStage::new(&endpoints.input.format, &mode, &state)
    };

    let mut buf = vec![0u8; PUMP_BUFFER_BYTES];
    loop {
        tokio::select! {
            biased;
            _ = cancel.notified() => return Ok(()),
            // Operator changed composition mode mid-stream.
            // Drop into the new branch on the next read.
            changed = mode_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let mode = mode_rx.borrow().clone();
                stage.set_mode(&mode);
            }
            // Operator changed an EQ band parameter or
            // engagement flag. Reconfigure the processor in
            // place; delay state is preserved so no pop.
            changed = eq_state_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let new_state = eq_state_rx.borrow().clone();
                stage.set_state(&new_state);
            }
            result = input.read(&mut buf) => {
                match result {
                    Ok(0) => return Ok(()),
                    Ok(n) => {
                        // A buffer the DSP cannot take surfaces as
                        // ReadFailed so the worker exits the substrate
                        // and waits for the next route change.
                        stage.apply(&mut buf[..n])?;
                        output
                            .write_all(&buf[..n])
                            .await
                            .map_err(|e| {
                                ByteFlowError::WriteFailed(e.to_string())
                            })?;
                    }
                    Err(e) => {
                        return Err(ByteFlowError::ReadFailed(e.to_string()));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq_dsp::{EqBandParams, EQ_BAND_COUNT};
    use std::sync::atomic::{AtomicU32, Ordering};

    const RATE: u32 = 44_100;

    fn pcm(codec: PcmCodec) -> AudioFormat {
        AudioFormat::Pcm {
            codec,
            rate_hz: RATE,
            channels: 2,
        }
    }

    /// Bands flat except band 2, set to `gain_db` at 1 kHz, Q 1.
    fn state(engaged: bool, gain_db: f32) -> EqRuntimeState {
        let mut bands = [EqBandParams::default(); EQ_BAND_COUNT];
        bands[2] = EqBandParams {
            freq_hz: 1000,
            gain_db,
            q: 1.0,
        };
        EqRuntimeState {
            engaged,
            bands: bands.to_vec(),
        }
    }

    /// `frames` of a 1 kHz stereo sine at amplitude `amp`, s16le.
    fn sine(frames: usize, amp: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 4);
        for n in 0..frames {
            let t = n as f32 / RATE as f32;
            let v =
                (amp * (2.0 * std::f32::consts::PI * 1000.0 * t).sin()) as i16;
            for _ in 0..2 {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    /// RMS of the left channel of an s16le stereo buffer, skipping
    /// the first `skip` frames so the filter has settled.
    fn rms(buf: &[u8], skip: usize) -> f64 {
        let frames = buf.len() / 4;
        let mut sum = 0f64;
        let mut n = 0f64;
        for f in skip..frames {
            let v = i16::from_le_bytes([buf[f * 4], buf[f * 4 + 1]]) as f64;
            sum += v * v;
            n += 1.0;
        }
        (sum / n).sqrt()
    }

    fn db(ratio: f64) -> f64 {
        20.0 * ratio.log10()
    }

    #[test]
    fn eq_only_engaged_raises_a_boosted_band_by_its_gain() {
        let input = sine(RATE as usize / 2, 4000.0);
        let mut out = input.clone();
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            MODE_EQ_ONLY,
            &state(true, 12.0),
        );
        assert!(stage.applies());
        stage.apply(&mut out).unwrap();
        let delta = db(rms(&out, 4000) / rms(&input, 4000));
        assert!(
            (delta - 12.0).abs() < 1.0,
            "a +12 dB band at 1 kHz should lift a 1 kHz tone by about 12 dB, got {delta:.2} dB"
        );
    }

    #[test]
    fn a_cut_band_lowers_the_tone() {
        let input = sine(RATE as usize / 2, 8000.0);
        let mut out = input.clone();
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            MODE_EQ_ONLY,
            &state(true, -9.0),
        );
        stage.apply(&mut out).unwrap();
        let delta = db(rms(&out, 4000) / rms(&input, 4000));
        assert!((delta + 9.0).abs() < 1.0, "got {delta:.2} dB");
    }

    #[test]
    fn passthrough_mode_leaves_the_bytes_identical_even_when_engaged() {
        let input = sine(4096, 4000.0);
        let mut out = input.clone();
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            "passthrough",
            &state(true, 12.0),
        );
        assert!(!stage.applies());
        stage.apply(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn eq_only_with_the_eq_not_engaged_leaves_the_bytes_identical() {
        let input = sine(4096, 4000.0);
        let mut out = input.clone();
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            MODE_EQ_ONLY,
            &state(false, 12.0),
        );
        assert!(!stage.applies());
        stage.apply(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn a_format_the_dsp_does_not_cover_passes_through_even_in_eq_only() {
        let input = sine(4096, 4000.0);
        let mut out = input.clone();
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS24Le),
            MODE_EQ_ONLY,
            &state(true, 12.0),
        );
        assert!(!stage.applies());
        stage.apply(&mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn the_mode_and_the_bands_can_change_while_the_stage_runs() {
        let input = sine(RATE as usize / 2, 4000.0);
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            "passthrough",
            &state(true, 12.0),
        );
        let mut a = input.clone();
        stage.apply(&mut a).unwrap();
        assert_eq!(a, input, "passthrough first");

        stage.set_mode(MODE_EQ_ONLY);
        let mut b = input.clone();
        stage.apply(&mut b).unwrap();
        assert!(db(rms(&b, 4000) / rms(&input, 4000)) > 10.0, "then boosted");

        stage.set_state(&state(true, 0.0));
        let mut c = input.clone();
        stage.apply(&mut c).unwrap();
        let flat = db(rms(&c, 4000) / rms(&input, 4000));
        assert!(flat.abs() < 0.5, "then flat again, got {flat:.2} dB");
    }

    #[test]
    fn a_misaligned_buffer_is_a_read_failure_not_a_panic() {
        let mut out = vec![0u8; 7];
        let mut stage = EqStage::new(
            &pcm(PcmCodec::PcmS16Le),
            MODE_EQ_ONLY,
            &state(true, 6.0),
        );
        let err = stage.apply(&mut out).unwrap_err();
        assert!(matches!(err, ByteFlowError::ReadFailed(_)), "got {err:?}");
    }

    #[test]
    fn the_cushion_is_two_tenths_of_a_second_of_frames() {
        assert_eq!(cushion_frames(44_100), 8_820);
        assert_eq!(cushion_frames(48_000), 9_600);
        assert_eq!(cushion_frames(96_000), 19_200);
        assert_eq!(cushion_frames(192_000), 38_400);
    }

    #[test]
    fn the_cushion_fits_the_output_buffer_the_dac_gives() {
        // The DAC's playback buffer on the reference target is 131072
        // frames; a cushion must sit well inside it at every rate the
        // chain can carry.
        for rate in [44_100u32, 48_000, 96_000, 192_000] {
            assert!(cushion_frames(rate) * 2 < 131_072, "{rate} Hz");
        }
    }

    fn quick() -> std::time::Duration {
        std::time::Duration::from_millis(1)
    }

    #[tokio::test]
    async fn an_open_that_succeeds_on_a_later_try_is_a_success() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let r = retry_open(5, quick(), Arc::new(Notify::new()), move || {
            let n = c.fetch_add(1, Ordering::SeqCst) + 1;
            async move {
                if n < 3 {
                    Err(ByteFlowError::OpenFailed("busy".into()))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert_eq!(r, Ok(()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_open_that_never_succeeds_fails_after_the_attempts_run_out() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let r = retry_open(4, quick(), Arc::new(Notify::new()), move || {
            c.fetch_add(1, Ordering::SeqCst);
            async { Err(ByteFlowError::OpenFailed("busy".into())) }
        })
        .await;
        assert_eq!(r, Err(ByteFlowError::OpenFailed("busy".into())));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_failure_that_is_not_an_open_is_not_retried() {
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let r = retry_open(9, quick(), Arc::new(Notify::new()), move || {
            c.fetch_add(1, Ordering::SeqCst);
            async { Err(ByteFlowError::ReadFailed("xrun".into())) }
        })
        .await;
        assert_eq!(r, Err(ByteFlowError::ReadFailed("xrun".into())));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cancel_during_the_wait_ends_the_retry_quietly() {
        let cancel = Arc::new(Notify::new());
        let calls = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&calls);
        let to_cancel = Arc::clone(&cancel);
        let r = retry_open(
            50,
            std::time::Duration::from_secs(30),
            cancel,
            move || {
                let first = c.fetch_add(1, Ordering::SeqCst) == 0;
                let to_cancel = Arc::clone(&to_cancel);
                async move {
                    if first {
                        to_cancel.notify_one();
                    }
                    Err(ByteFlowError::OpenFailed("busy".into()))
                }
            },
        )
        .await;
        assert_eq!(r, Ok(()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
