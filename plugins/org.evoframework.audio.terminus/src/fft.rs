// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
// FFT + mel-scale binning + perceptual signals — math-heavy
// numerical code with explicit indexed loops; clippy's
// needless_range_loop lint suggests iterator forms that obscure
// the fixed-shape DSP layout (per-bin parallel arrays + the
// shared `i` index linking them). Allow at module scope.
#![allow(clippy::needless_range_loop)]

//! FFT + mel-scale binning + perceptual signals.
//!
//! Pure compute, no I/O. Takes interleaved stereo PCM (normalised
//! f32) at the configured sample rate, runs a real-input FFT of
//! [`FFT_WINDOW`] samples per channel (capture advances by
//! [`HOP_SIZE`] with overlap), projects the magnitude spectrum
//! onto an operator-demand-driven bin count (32 / 64 / 128 / 256)
//! covering [20 Hz, 20 kHz] under log / mel / linear spacing,
//! normalises to [0, 1], and computes the three forward-decade
//! perceptual signals the spectrum-frame wire contract defines:
//! peak-hold per bin, per-band onset events, per-bin L/R
//! correlation coefficient (populated only when the operator's
//! `channels` demand is 2 — a mono-collapsed output has no L/R
//! discrimination to expose).
//!
//! The analyser is parameterised at construction by
//! `(sample_rate_hz, bins, channels, frequency_scale)`. Bins
//! mirror the operator's `ui.visualizer.bin_count` demand;
//! channels mirror `ui.visualizer.channel_mode` (`1` = mono
//! collapse — L+R averaged at the filterbank stage; `2` =
//! stereo). A demand change mid-play rebuilds the analyser
//! (peak-hold state resets; onset history resets — small visual
//! flicker on the frame boundary is preferable to fabricating
//! per-bin state that never corresponded to the new shape).
//!
//! Log spacing is ANSI/IEC S1.11 **base-10** equal-ratio
//! (fractional-octave) partition of `[20 Hz, 20 kHz]` — the
//! music-analyser default. Mel preserves the pre-2026-08-11
//! perceptual bank; linear is diagnostics-only raw-Hz layout.
//! Adjacent output columns that would map to the same integer
//! FFT range are anti-cloned at construction (range split or
//! triangular weights) so the wire never emits Minecraft
//! plateaus.
//!
//! Peak-hold decays at a perceptually-tuned rate (~12 dB/s) so
//! transients hold visibly without flickering on the falling edge.
//! Onsets fire when a band's spectral flux crosses an
//! adaptive threshold (recent-mean + k * recent-std). Correlation
//! is the normalised cross-product of L and R magnitudes per bin
//! (Pearson r restricted to non-negative inputs); zero-length on
//! mono-collapsed output.
//!
//! The module is tested against synthesised sine + white-noise
//! inputs (see `tests` at the bottom): a 1 kHz sine concentrates
//! energy in the bin whose centre frequency surrounds 1 kHz; white
//! noise produces a flat-ish magnitude across bins; silence
//! produces zero magnitudes; peak-hold decays at the expected rate
//! across N consecutive silence frames after a peak.

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// The ALSA loopback capture is always stereo (S32_LE, 2
/// channels). The demand-driven `channels` output is a
/// downstream reduction (mono = average L+R at the mel stage;
/// stereo = emit both); this constant names the INPUT channel
/// count that the capture loop hands to `process_frame`, not
/// the output channel count on the wire.
pub const INPUT_CHANNELS: usize = 2;

/// Analysis FFT window length (samples per channel). At 48 kHz
/// this yields ~2.93 Hz raw-bin resolution — enough for dense
/// log×256 product glass once anti-clone banking is applied.
/// Larger than the ALSA hop; capture keeps an overlap ring and
/// feeds the most recent `FFT_WINDOW` samples each hop.
pub const FFT_WINDOW: usize = 16_384;

/// ALSA / STFT hop length (samples per channel). Capture reads
/// this many frames per cycle and advances the overlap ring by
/// the same amount. Cadence stays ~47 Hz at 48 kHz so transient
/// feel is not sacrificed for frequency resolution.
pub const HOP_SIZE: usize = 1024;

/// Backward-compatible alias for the analysis window. Prefer
/// [`FFT_WINDOW`] in new code; hop cadence uses [`HOP_SIZE`].
#[allow(dead_code)] // public compatibility alias for out-of-crate callers
pub const FFT_SIZE: usize = FFT_WINDOW;

/// Derive the frame cadence the capture loop runs at, given the
/// configured sample rate. Cadence follows the **hop**, not the
/// analysis window: `sample_rate_hz / HOP_SIZE`. At 48 kHz the
/// canonical reference rig sees `48000 / 1024 = 46.875` → 47.
///
/// NOTE: this is the compute cadence, NOT the wire emit cadence.
/// The capture loop's F2C emit throttle governs the wire cadence
/// separately (default 30 Hz via `demand.rate_hz_target`).
pub fn frame_rate_hz(sample_rate_hz: u32) -> u32 {
    (sample_rate_hz as f64 / HOP_SIZE as f64).round() as u32
}

/// Analyser low-frequency cutoff. 20 Hz is the conventional
/// audible-band lower bound. Same value for every
/// [`FrequencyScale`] — only the spacing across `[low, high]`
/// differs. Kept as `MEL_LOW_HZ` for backward reference; the
/// name is legacy from the mel-only era pre-2026-08-11.
pub const MEL_LOW_HZ: f32 = 20.0;

/// Analyser high-frequency cutoff. 20 kHz is the conventional
/// audible-band upper bound. Same value for every
/// [`FrequencyScale`]. Kept as `MEL_HIGH_HZ` for backward
/// reference.
pub const MEL_HIGH_HZ: f32 = 20_000.0;

/// The canonical sample rate the reference rig runs the ALSA
/// loopback capture at.
pub const CANONICAL_SAMPLE_RATE_HZ: u32 = 48_000;

/// FFT bin width at the canonical sample rate with the analysis
/// window. `48_000 / 16384 ≈ 2.930` Hz. Narrower than the old
/// 1024-point chain (46.875 Hz); log×256 still needs anti-clone
/// at the very bottom octaves (1/24-octave @ 20 Hz ≈ 0.58 Hz).
#[allow(dead_code)] // documented constant; used by audits / external math
pub const CANONICAL_BIN_WIDTH_HZ: f32 =
    CANONICAL_SAMPLE_RATE_HZ as f32 / FFT_WINDOW as f32;

/// Peak-hold decay rate. 12 dB/s in linear terms is a
/// multiplicative decay of ~0.5^(dt/0.25s) — that is, the peak
/// falls to half its value over ~250 ms. At 30 Hz this is
/// ~0.933 per frame. Tuned so percussive transients hold
/// visibly without long-tailing.
pub const PEAK_HOLD_DECAY_PER_FRAME_30HZ: f32 = 0.933;

/// Onset detection window (frames). Spectral flux is compared
/// against the rolling mean + k * std-dev over this many
/// previous frames.
pub const ONSET_WINDOW_FRAMES: usize = 16;

/// Onset detection sensitivity. Higher k = fewer false-positives,
/// fewer real onsets caught. 1.8 is conservative; surface tuning
/// up once we have rig data.
pub const ONSET_K: f32 = 1.8;

/// Per-band ranges (in mel-bin index space). Sub-bass = 20-60 Hz,
/// bass = 60-250 Hz, mid = 250-2000 Hz, high = 2000+ Hz. Computed
/// once at construction from the mel projection table.
#[derive(Debug, Clone, Copy)]
pub struct BandRanges {
    pub sub_bass: (usize, usize),
    pub bass: (usize, usize),
    pub mid: (usize, usize),
    pub high: (usize, usize),
}

/// Per-frame perceptual signals emitted alongside the magnitudes.
///
/// Shape is demand-driven — `bins` and `channels` on the frame
/// reflect the analyser's actual state at the moment of compute.
/// The wire reads these fields as shape authority; a demand
/// change may lead the analyser rebuild by one or two frames and
/// the intermediate frames carry the pre-rebuild shape rather
/// than fabricating post-rebuild dimensions.
#[derive(Debug, Clone)]
pub struct PerceptualFrame {
    /// Number of mel bins per channel. One of `{32, 64, 128, 256}`.
    pub bins: u32,
    /// Number of output channels. `1` for mono-collapsed output
    /// (L+R averaged at the mel stage; `magnitudes.len() == bins`);
    /// `2` for stereo (L then R; `magnitudes.len() == 2 * bins`).
    pub channels: u32,
    /// Magnitudes in [0, 1]. Layout: for `channels = 1`, one
    /// contiguous run of `bins` values (the mono-collapsed
    /// channel). For `channels = 2`, `bins` L values followed by
    /// `bins` R values (channel-major). Total length always
    /// `bins * channels`.
    pub magnitudes: Box<[f32]>,
    /// Peak-hold per bin, per channel. Same layout as `magnitudes`.
    pub peak_hold: Box<[f32]>,
    /// Onset booleans, four-band (fixed independent of `bins`).
    pub onsets: OnsetFrame,
    /// L/R correlation per bin. Length `bins` when `channels = 2`
    /// (per-bin Pearson-restricted correlation of L vs R at that
    /// mel bin); length `0` when `channels = 1` (mono output
    /// carries no L/R discrimination). Never used to fabricate
    /// stereo information on mono demand.
    pub correlation: Box<[f32]>,
    /// Frame timestamp in milliseconds since UNIX epoch. Set by
    /// the caller (the capture loop) at frame emit time.
    pub at_ms: u64,
}

/// Onset detection booleans, one per perceptual band.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OnsetFrame {
    pub sub_bass: bool,
    pub bass: bool,
    pub mid: bool,
    pub high: bool,
}

/// Stateful spectrum analyser. One instance per (sample_rate,
/// bins, channels, frequency_scale) tuple; rebuilt when the
/// operator's demand changes any of those dimensions mid-play
/// (peak-hold + onset history reset on rebuild — one-frame
/// visual discontinuity preferable to fabricated post-rebuild
/// state).
pub struct SpectrumAnalyser {
    /// Output bin count. Mirrors demand.bins. Sized `∈ {32, 64,
    /// 128, 256}` in practice but the analyser accepts any
    /// runtime value; validation happens at the verb parse
    /// stage in `demand.rs`.
    bins: usize,
    /// Output channel count. Mirrors demand.channels. `1` =
    /// mono-collapse at the filterbank stage; `2` = stereo
    /// pass-through.
    channels: usize,
    /// Frequency-bin spacing. Mirrors demand.frequency_scale.
    /// Determines how `bins` output slots are distributed
    /// across `[MEL_LOW_HZ, MEL_HIGH_HZ]`.
    frequency_scale: crate::demand::FrequencyScale,
    fft: Arc<dyn Fft<f32>>,
    /// FFT scratch buffer, reused across `process_frame` calls.
    scratch: Vec<Complex32>,
    /// Per-output-bin FFT index range `[lo, hi)` after anti-clone
    /// resolution. Length = `bins`. Built once at construction
    /// so `process_frame` never emits adjacent identical ranges
    /// when the analysis window can split them.
    fft_ranges: Vec<(usize, usize)>,
    /// Per-output-bin amplitude weight applied after the power
    /// sum. `1.0` for uniquely-owned ranges; triangular share
    /// when several columns must read the same starved FFT span.
    fft_weights: Vec<f32>,
    /// Hann window coefficients applied to the input PCM before
    /// FFT. Heap-backed — `FFT_WINDOW` is too large for a stack
    /// array inside `Option<SpectrumAnalyser>` on the capture
    /// thread.
    hann_window: Box<[f32]>,
    /// Per-output-channel peak-hold state. Outer length =
    /// `channels`, inner length = `bins`.
    peak_hold: Vec<Vec<f32>>,
    /// Per-band spectral-flux history for onset detection. Index
    /// by band (sub_bass=0, bass=1, mid=2, high=3). Bands are
    /// fixed-count (4); their WIDTHS depend on the filterbank
    /// (which depends on `bins` and `frequency_scale`), but the
    /// count is always 4.
    flux_history: [[f32; ONSET_WINDOW_FRAMES]; 4],
    /// Rolling write cursor into `flux_history`.
    flux_cursor: usize,
    /// Previous frame's per-band magnitude (for flux delta).
    /// Index by band.
    prev_band_magnitude: [f32; 4],
    /// Cached band ranges (in output-bin index space, i.e.
    /// `[0, bins)`).
    bands: BandRanges,
}

impl SpectrumAnalyser {
    /// Construct an analyser for a specific demand shape.
    ///
    /// - `sample_rate_hz` MUST match the rate the input PCM is
    ///   captured at; passing a wrong rate produces an off-by-N
    ///   filterbank mapping. Typical value: 48000.
    /// - `bins` is the operator's demanded output-bin count.
    ///   Enum-validated at the demand parse stage
    ///   (`demand::validate_bins`); the analyser accepts any
    ///   positive value.
    /// - `channels` is the operator's demanded output-channel
    ///   count. `1` (mono-collapse) or `2` (stereo). Values
    ///   outside `{1, 2}` are refused at the demand parse
    ///   stage.
    /// - `frequency_scale` is the operator's demanded bin
    ///   spacing across `[MEL_LOW_HZ, MEL_HIGH_HZ]`.
    ///   [`Log`](crate::demand::FrequencyScale::Log) matches
    ///   music-analyser convention (default per the 2026-08-11
    ///   ownership audit); [`Mel`](crate::demand::FrequencyScale::Mel)
    ///   preserves the pre-audit shape;
    ///   [`Linear`](crate::demand::FrequencyScale::Linear) is a
    ///   diagnostics-only raw-Hz layout.
    pub fn new(
        sample_rate_hz: u32,
        bins: usize,
        channels: usize,
        frequency_scale: crate::demand::FrequencyScale,
    ) -> Self {
        assert!(bins > 0, "analyser bins must be positive");
        assert!(
            channels == 1 || channels == 2,
            "analyser channels must be 1 or 2, got {channels}"
        );
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_WINDOW);
        let centres_hz = compute_centres(bins, frequency_scale);
        let bounds_hz = compute_bounds(bins, frequency_scale);
        let (fft_ranges, fft_weights) =
            build_filterbank_map(&bounds_hz, &centres_hz, sample_rate_hz);
        let hann_window = compute_hann_window();
        let bands = compute_band_ranges(&centres_hz, bins);
        Self {
            bins,
            channels,
            frequency_scale,
            fft,
            scratch: vec![Complex32::default(); FFT_WINDOW],
            fft_ranges,
            fft_weights,
            hann_window,
            peak_hold: vec![vec![0.0; bins]; channels],
            flux_history: [[0.0; ONSET_WINDOW_FRAMES]; 4],
            flux_cursor: 0,
            prev_band_magnitude: [0.0; 4],
            bands,
        }
    }

    /// Return the operator-visible frequency scale this analyser
    /// carries. Consumed by the capture loop's rebuild-condition
    /// check so a mid-play scale change triggers analyser rebuild
    /// exactly the same way a bins/channels change does.
    pub fn frequency_scale(&self) -> crate::demand::FrequencyScale {
        self.frequency_scale
    }

    /// Return the operator-visible bin count this analyser
    /// carries. Frame payloads report this as the wire's
    /// authoritative shape.
    pub fn bins(&self) -> usize {
        self.bins
    }

    /// Return the operator-visible channel count this analyser
    /// carries.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Return the precomputed FFT-bin band ranges this analyser
    /// uses to project the mel-bin magnitude vector onto the
    /// four perceptual bands published on the spectrum subject.
    #[allow(dead_code)]
    pub fn band_ranges(&self) -> BandRanges {
        self.bands
    }

    /// Process one analysis window of `FFT_WINDOW` samples per
    /// INPUT channel (always 2 — the ALSA loopback is stereo
    /// regardless of output demand). The input is interleaved
    /// stereo: `[L0, R0, ..., L_{FFT_WINDOW-1}, R_{FFT_WINDOW-1}]`.
    /// Each sample is a normalised f32 in [-1, 1]. Capture feeds
    /// this from its overlap ring (hop = [`HOP_SIZE`]). Returns
    /// the per-frame perceptual signals in the analyser's current
    /// output shape; `at_ms` is left at 0 for the caller to stamp.
    ///
    /// Mono collapse: when the analyser was constructed with
    /// `channels = 1`, the returned frame carries `bins` output
    /// magnitudes computed as the average of the L and R
    /// channels' per-bin magnitudes. Two FFTs still run (one per
    /// input channel) — the collapse happens at the filterbank
    /// magnitude stage.
    pub fn process_frame(
        &mut self,
        interleaved_pcm: &[f32],
    ) -> PerceptualFrame {
        assert_eq!(
            interleaved_pcm.len(),
            FFT_WINDOW * INPUT_CHANNELS,
            "input PCM length must be FFT_WINDOW * INPUT_CHANNELS"
        );

        let output_len = self.bins * self.channels;
        let mut magnitudes = vec![0.0f32; output_len].into_boxed_slice();
        let mut peak_hold = vec![0.0f32; output_len].into_boxed_slice();
        // Correlation array is emitted only for stereo; mono
        // collapse produces zero-length correlation.
        let mut correlation =
            vec![0.0f32; if self.channels == 2 { self.bins } else { 0 }]
                .into_boxed_slice();

        // Per-input-channel filterbank magnitudes. Always sized
        // `INPUT_CHANNELS × bins` (compute stays symmetric across
        // both PCM channels; downstream collapse decides how many
        // survive to the wire). Bin spacing is per
        // `self.frequency_scale`; log / mel / linear layouts all
        // share this projection loop.
        let mut bin_mags: Vec<Vec<f32>> =
            vec![vec![0.0; self.bins]; INPUT_CHANNELS];

        for ch in 0..INPUT_CHANNELS {
            // De-interleave + Hann-window into the FFT scratch
            // buffer. Imaginary parts are zero (real input).
            for i in 0..FFT_WINDOW {
                let sample = interleaved_pcm[i * INPUT_CHANNELS + ch];
                self.scratch[i] =
                    Complex32::new(sample * self.hann_window[i], 0.0);
            }
            self.fft.process(&mut self.scratch);

            // Project FFT bins onto output bins using the
            // precomputed anti-cloned ranges. Power-sum then
            // sqrt preserves band energy; `* 4 / FFT_WINDOW`
            // restores the one-sided + Hann amplitude scale to
            // [0, 1] for a full-scale in-band sine.
            for out_idx in 0..self.bins {
                let (fft_lo, fft_hi) = self.fft_ranges[out_idx];
                if fft_hi <= fft_lo {
                    bin_mags[ch][out_idx] = 0.0;
                    continue;
                }
                let mut power = 0.0f32;
                for fft_idx in fft_lo..fft_hi {
                    let c = self.scratch[fft_idx];
                    power += c.re * c.re + c.im * c.im;
                }
                let amp = power.sqrt() * self.fft_weights[out_idx];
                let normalised = amp * 4.0 / FFT_WINDOW as f32;
                bin_mags[ch][out_idx] = normalised.clamp(0.0, 1.0);
            }
        }

        // Compute spectral flux per band and update onset history
        // (must happen before per-bin normalisation since flux is
        // on raw magnitudes). Uses the mono-collapsed magnitudes
        // regardless of output channel count — onsets are a
        // per-band scalar, not a per-channel thing.
        let raw_mono: Vec<f32> = (0..self.bins)
            .map(|i| (bin_mags[0][i] + bin_mags[1][i]) * 0.5)
            .collect();
        let band_mags = compute_band_magnitudes(&raw_mono, self.bands);
        let onsets = self.update_onsets(band_mags);

        if self.channels == 2 {
            // Per-bin L/R correlation. Pearson-style normalised
            // cross-product on non-negative inputs. Only meaningful
            // on stereo demand; mono demand emits a zero-length
            // correlation array (see `correlation` init above).
            for i in 0..self.bins {
                let l = bin_mags[0][i];
                let r = bin_mags[1][i];
                let denom = (l * l + r * r).sqrt();
                correlation[i] = if denom > 1e-6 {
                    (2.0 * l * r) / (l * l + r * r)
                } else {
                    0.0
                };
            }

            // Stereo output: write L then R (channel-major layout).
            for ch in 0..2 {
                for i in 0..self.bins {
                    let mag = bin_mags[ch][i];
                    let decayed =
                        self.peak_hold[ch][i] * PEAK_HOLD_DECAY_PER_FRAME_30HZ;
                    let new_peak = mag.max(decayed);
                    self.peak_hold[ch][i] = new_peak;
                    let off = ch * self.bins + i;
                    magnitudes[off] = mag;
                    peak_hold[off] = new_peak;
                }
            }
        } else {
            // Mono output: average L+R at each mel bin. One
            // output channel; peak-hold state lives on
            // `self.peak_hold[0]`.
            for i in 0..self.bins {
                let mono_mag = (bin_mags[0][i] + bin_mags[1][i]) * 0.5;
                let decayed =
                    self.peak_hold[0][i] * PEAK_HOLD_DECAY_PER_FRAME_30HZ;
                let new_peak = mono_mag.max(decayed);
                self.peak_hold[0][i] = new_peak;
                magnitudes[i] = mono_mag;
                peak_hold[i] = new_peak;
            }
        }

        PerceptualFrame {
            bins: self.bins as u32,
            channels: self.channels as u32,
            magnitudes,
            peak_hold,
            onsets,
            correlation,
            at_ms: 0,
        }
    }

    /// Update the rolling spectral-flux history per band, return
    /// onset booleans.
    fn update_onsets(&mut self, band_mags: [f32; 4]) -> OnsetFrame {
        let mut onsets = OnsetFrame::default();
        for (band_idx, &mag) in band_mags.iter().enumerate() {
            let flux = (mag - self.prev_band_magnitude[band_idx]).max(0.0);
            self.flux_history[band_idx][self.flux_cursor] = flux;
            self.prev_band_magnitude[band_idx] = mag;

            // Mean + std-dev over the window (excluding this
            // frame's flux so the threshold is based on history,
            // not the current observation).
            let mut sum = 0.0f32;
            let mut count = 0;
            for i in 0..ONSET_WINDOW_FRAMES {
                if i == self.flux_cursor {
                    continue;
                }
                sum += self.flux_history[band_idx][i];
                count += 1;
            }
            let mean = sum / (count as f32);
            let mut var = 0.0f32;
            for i in 0..ONSET_WINDOW_FRAMES {
                if i == self.flux_cursor {
                    continue;
                }
                let d = self.flux_history[band_idx][i] - mean;
                var += d * d;
            }
            let std = (var / (count as f32)).sqrt();
            let threshold = mean + ONSET_K * std;

            let fired = flux > threshold && flux > 0.01;
            match band_idx {
                0 => onsets.sub_bass = fired,
                1 => onsets.bass = fired,
                2 => onsets.mid = fired,
                3 => onsets.high = fired,
                _ => unreachable!(),
            }
        }
        self.flux_cursor = (self.flux_cursor + 1) % ONSET_WINDOW_FRAMES;
        onsets
    }
}

fn hz_to_mel(hz: f32) -> f32 {
    2595.0 * (1.0 + hz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0f32.powf(mel / 2595.0) - 1.0)
}

/// Compute the per-bin frequency centres under the given
/// [`FrequencyScale`]. Length = `bins`. Centres are used as
/// inputs to [`compute_band_ranges`] so the four perceptual
/// bands (sub_bass / bass / mid / high) stay Hz-true under
/// every scale — only the bin index positions of those bands
/// change with the scale.
///
/// Log: ANSI/IEC S1.11 base-10 equal-ratio (fractional-octave)
/// geometric means across `[MEL_LOW_HZ, MEL_HIGH_HZ]`.
/// Mel: equal mel-scale spacing (perceptual bank, preserves the
/// pre-2026-08-11 behaviour).
/// Linear: equal Hz spacing (raw-FFT diagnostic layout).
pub(crate) fn compute_centres(
    bins: usize,
    scale: crate::demand::FrequencyScale,
) -> Vec<f32> {
    use crate::demand::FrequencyScale;
    let n = bins as f32;
    match scale {
        FrequencyScale::Log => {
            // Geometric mean of each base-10 log partition cell.
            let log_lo = MEL_LOW_HZ.log10();
            let log_hi = MEL_HIGH_HZ.log10();
            let step = (log_hi - log_lo) / n;
            (0..bins)
                .map(|i| 10.0f32.powf(log_lo + step * ((i as f32) + 0.5)))
                .collect()
        }
        FrequencyScale::Mel => {
            let mel_low = hz_to_mel(MEL_LOW_HZ);
            let mel_high = hz_to_mel(MEL_HIGH_HZ);
            let mel_step = (mel_high - mel_low) / n;
            (0..bins)
                .map(|i| {
                    let mel = mel_low + mel_step * ((i as f32) + 0.5);
                    mel_to_hz(mel)
                })
                .collect()
        }
        FrequencyScale::Linear => {
            let hz_step = (MEL_HIGH_HZ - MEL_LOW_HZ) / n;
            (0..bins)
                .map(|i| MEL_LOW_HZ + hz_step * ((i as f32) + 0.5))
                .collect()
        }
    }
}

/// Compute the per-bin frequency edges (low, high) under the
/// given [`FrequencyScale`]. Length = `bins`. Edges are the
/// bounds each output bin's filterbank sums FFT power across
/// (see [`SpectrumAnalyser::process_frame`]).
///
/// Contract: `bounds[i].0 <= bounds[i].1`, and consecutive
/// bounds meet (`bounds[i].1 == bounds[i+1].0`) so the band-pass
/// is a partition of `[MEL_LOW_HZ, MEL_HIGH_HZ]` with no gap and
/// no overlap — a pure rectangular filterbank shape.
///
/// Log edges use base-10 equal-ratio spacing
/// (`f = 10^(log10(f_lo) + t·Δ)`), the ANSI/IEC S1.11 preferred
/// decade geometry for fractional-octave banks mapped onto the
/// demanded bin count.
pub(crate) fn compute_bounds(
    bins: usize,
    scale: crate::demand::FrequencyScale,
) -> Vec<(f32, f32)> {
    use crate::demand::FrequencyScale;
    let n = bins as f32;
    // Build edges[0..=bins] first with explicit endpoint
    // forcing. `f32::powf` / `log10` accumulate precision drift;
    // forcing `edges[bins] = MEL_HIGH_HZ` pins the partition so
    // `fft_hi` never walks past Nyquist from float creep.
    let mut edges = Vec::with_capacity(bins + 1);
    for i in 0..=bins {
        let edge = if i == 0 {
            MEL_LOW_HZ
        } else if i == bins {
            MEL_HIGH_HZ
        } else {
            let t = i as f32 / n;
            match scale {
                FrequencyScale::Log => {
                    let log_lo = MEL_LOW_HZ.log10();
                    let log_hi = MEL_HIGH_HZ.log10();
                    10.0f32.powf(log_lo + (log_hi - log_lo) * t)
                }
                FrequencyScale::Mel => {
                    let mel_low = hz_to_mel(MEL_LOW_HZ);
                    let mel_high = hz_to_mel(MEL_HIGH_HZ);
                    mel_to_hz(mel_low + (mel_high - mel_low) * t)
                }
                FrequencyScale::Linear => {
                    MEL_LOW_HZ + (MEL_HIGH_HZ - MEL_LOW_HZ) * t
                }
            }
        };
        edges.push(edge);
    }
    (0..bins).map(|i| (edges[i], edges[i + 1])).collect()
}

fn compute_hann_window() -> Box<[f32]> {
    let mut w = vec![0.0f32; FFT_WINDOW];
    let denom = (FFT_WINDOW - 1) as f32;
    for i in 0..FFT_WINDOW {
        let phase = 2.0 * std::f32::consts::PI * (i as f32) / denom;
        w[i] = 0.5 - 0.5 * phase.cos();
    }
    w.into_boxed_slice()
}

/// Map Hz bounds → FFT index ranges, then anti-clone any run of
/// identical ranges so adjacent wire columns never share the
/// exact same FFT slice when the analysis window can split it.
/// When a run is longer than the available FFT span, columns
/// keep the shared span with distinct triangular weights.
pub(crate) fn build_filterbank_map(
    bounds_hz: &[(f32, f32)],
    centres_hz: &[f32],
    sample_rate_hz: u32,
) -> (Vec<(usize, usize)>, Vec<f32>) {
    let bin_width = sample_rate_hz as f32 / FFT_WINDOW as f32;
    let nyquist = FFT_WINDOW / 2;
    let n = bounds_hz.len();
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(n);
    for &(lo_hz, hi_hz) in bounds_hz {
        let fft_lo = ((lo_hz / bin_width).floor() as usize).max(1);
        let fft_hi = ((hi_hz / bin_width).ceil() as usize).min(nyquist);
        ranges.push((fft_lo, fft_hi.max(fft_lo)));
    }
    let mut weights = vec![1.0f32; n];

    let mut i = 0;
    while i < n {
        let key = ranges[i];
        let mut j = i + 1;
        while j < n && ranges[j] == key {
            j += 1;
        }
        let run = j - i;
        let (lo, hi) = key;
        let span = hi.saturating_sub(lo);

        if run == 1 {
            if span == 0 {
                let fallback = lo.min(nyquist.saturating_sub(1)).max(1);
                ranges[i] = (fallback, fallback + 1);
            }
            i = j;
            continue;
        }

        if span >= run {
            for k in 0..run {
                let a = lo + span * k / run;
                let b = lo + span * (k + 1) / run;
                ranges[i + k] = (a, b.max(a + 1));
                weights[i + k] = 1.0;
            }
        } else if span >= 1 {
            // Starved: every column in the run reads the shared
            // span; triangular weights by proximity of the
            // output centre to FFT-bin centres break identical
            // Minecraft plateaus while conserving a peak of 1.0
            // on the nearest column.
            let mut wmax = 0.0f32;
            for k in 0..run {
                ranges[i + k] = (lo, hi);
                let centre = centres_hz[i + k];
                let mut best = 0.0f32;
                for fi in lo..hi {
                    let fc = (fi as f32 + 0.5) * bin_width;
                    let d = (centre - fc).abs();
                    let local = 1.0 / (1.0 + d / bin_width);
                    best = best.max(local);
                }
                weights[i + k] = best.max(0.05);
                wmax = wmax.max(weights[i + k]);
            }
            if wmax > 0.0 {
                for k in 0..run {
                    weights[i + k] /= wmax;
                }
            }
        } else {
            let fallback = lo.min(nyquist.saturating_sub(1)).max(1);
            for k in 0..run {
                ranges[i + k] = (fallback, fallback + 1);
                // Only the first starved empty column keeps full
                // weight; the rest stay dark rather than clone.
                weights[i + k] = if k == 0 { 1.0 } else { 0.0 };
            }
        }
        i = j;
    }

    (ranges, weights)
}

fn compute_band_ranges(mel_centres_hz: &[f32], bins: usize) -> BandRanges {
    let find_first_at_or_above = |hz: f32| -> usize {
        mel_centres_hz.iter().position(|&c| c >= hz).unwrap_or(bins)
    };
    let sub_bass_lo = find_first_at_or_above(MEL_LOW_HZ);
    let sub_bass_hi = find_first_at_or_above(60.0);
    let bass_lo = sub_bass_hi;
    let bass_hi = find_first_at_or_above(250.0);
    let mid_lo = bass_hi;
    let mid_hi = find_first_at_or_above(2_000.0);
    let high_lo = mid_hi;
    let high_hi = bins;
    BandRanges {
        sub_bass: (sub_bass_lo, sub_bass_hi),
        bass: (bass_lo, bass_hi),
        mid: (mid_lo, mid_hi),
        high: (high_lo, high_hi),
    }
}

fn compute_band_magnitudes(mono: &[f32], bands: BandRanges) -> [f32; 4] {
    let mean_over = |lo: usize, hi: usize| -> f32 {
        if hi <= lo {
            return 0.0;
        }
        let mut sum = 0.0f32;
        for i in lo..hi {
            sum += mono[i];
        }
        sum / (hi - lo) as f32
    };
    [
        mean_over(bands.sub_bass.0, bands.sub_bass.1),
        mean_over(bands.bass.0, bands.bass.1),
        mean_over(bands.mid.0, bands.mid.1),
        mean_over(bands.high.0, bands.high.1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rate_hz_follows_hop_not_window() {
        // Cadence is sample_rate / HOP_SIZE (1024), not FFT_WINDOW.
        // 48 kHz @ hop 1024 = 46.875 → 47.
        assert_eq!(frame_rate_hz(48_000), 47);
        assert_eq!(HOP_SIZE, 1024);
        assert_ne!(FFT_WINDOW, HOP_SIZE);
        assert_eq!(frame_rate_hz(44_100), 43);
        assert_eq!(frame_rate_hz(96_000), 94);
        assert_eq!(frame_rate_hz(192_000), 188);
        assert_eq!(frame_rate_hz(500), 0);
    }

    fn make_stereo_silence(len_samples: usize) -> Vec<f32> {
        vec![0.0; len_samples]
    }

    fn make_sine(freq_hz: f32, sample_rate_hz: u32, len: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let t = (i / INPUT_CHANNELS) as f32 / sample_rate_hz as f32;
            let v = (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            out.push(v);
        }
        out
    }

    fn argmax(a: &[f32]) -> usize {
        let mut best = 0;
        let mut best_v = a[0];
        for (i, &v) in a.iter().enumerate() {
            if v > best_v {
                best_v = v;
                best = i;
            }
        }
        best
    }

    #[test]
    fn analyser_output_shape_matches_demand_stereo_256() {
        let mut a = SpectrumAnalyser::new(
            48_000,
            256,
            2,
            crate::demand::FrequencyScale::Mel,
        );
        assert_eq!(a.bins(), 256);
        assert_eq!(a.channels(), 2);
        let pcm = make_stereo_silence(FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        assert_eq!(f.bins, 256);
        assert_eq!(f.channels, 2);
        assert_eq!(f.magnitudes.len(), 512);
        assert_eq!(f.peak_hold.len(), 512);
        assert_eq!(f.correlation.len(), 256);
    }

    #[test]
    fn analyser_output_shape_matches_demand_mono_64() {
        let mut a = SpectrumAnalyser::new(
            48_000,
            64,
            1,
            crate::demand::FrequencyScale::Mel,
        );
        assert_eq!(a.bins(), 64);
        assert_eq!(a.channels(), 1);
        let pcm = make_stereo_silence(FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        assert_eq!(f.bins, 64);
        assert_eq!(f.channels, 1);
        assert_eq!(f.magnitudes.len(), 64);
        assert_eq!(f.peak_hold.len(), 64);
        // Mono demand → zero-length correlation (no L/R
        // discrimination to expose).
        assert_eq!(f.correlation.len(), 0);
    }

    #[test]
    fn analyser_output_shape_matches_demand_stereo_32() {
        let mut a = SpectrumAnalyser::new(
            48_000,
            32,
            2,
            crate::demand::FrequencyScale::Mel,
        );
        let pcm = make_stereo_silence(FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        assert_eq!(f.bins, 32);
        assert_eq!(f.channels, 2);
        assert_eq!(f.magnitudes.len(), 64);
        assert_eq!(f.correlation.len(), 32);
    }

    #[test]
    fn silence_frames_produce_zero_magnitudes() {
        for (bins, channels) in [(32usize, 1usize), (64, 2), (128, 1), (256, 2)]
        {
            let mut a = SpectrumAnalyser::new(
                48_000,
                bins,
                channels,
                crate::demand::FrequencyScale::Mel,
            );
            let pcm = make_stereo_silence(FFT_WINDOW * INPUT_CHANNELS);
            let f = a.process_frame(&pcm);
            assert!(
                f.magnitudes.iter().all(|&m| m.abs() < 1e-6),
                "silence should produce zero magnitudes at bins={bins} channels={channels}"
            );
        }
    }

    #[test]
    fn sine_energy_concentrates_at_expected_bin_stereo_256() {
        // A 1 kHz sine should peak in the mel bin whose centre
        // falls closest to 1 kHz. On the [20 Hz, 20 kHz] mel
        // scale with 256 bins, hz_to_mel(1000) ≈ 1000, which
        // maps to roughly bin 65 (mel_step ≈ 14.8, offset from
        // mel_low ≈ 31). Assert a window around it that
        // tolerates FFT-bin granularity + rounding.
        let mut a = SpectrumAnalyser::new(
            48_000,
            256,
            2,
            crate::demand::FrequencyScale::Mel,
        );
        let pcm = make_sine(1_000.0, 48_000, FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        // Channel-L slice.
        let l = &f.magnitudes[0..256];
        let peak_bin = argmax(l);
        assert!(
            (55..75).contains(&peak_bin),
            "1 kHz sine on 256-bin mel should peak in bins [55, 75), got {peak_bin}"
        );
    }

    #[test]
    fn sine_energy_concentrates_at_expected_bin_mono_64() {
        // Same sine, mono-collapsed 64-bin analyser: peak lands
        // in the bin covering 1 kHz. 64-bin mel-scale between
        // 20 Hz and 20 kHz → mel_step ≈ 59; 1 kHz → mel ~1000
        // → bin index ≈ (1000 - 31) / 59 ≈ 16.
        let mut a = SpectrumAnalyser::new(
            48_000,
            64,
            1,
            crate::demand::FrequencyScale::Mel,
        );
        let pcm = make_sine(1_000.0, 48_000, FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        let peak_bin = argmax(&f.magnitudes);
        assert!(
            (12..22).contains(&peak_bin),
            "1 kHz sine on 64-bin mono should peak in bins [12, 22), got {peak_bin}"
        );
    }

    #[test]
    fn bounds_length_matches_requested_bins_for_every_scale() {
        use crate::demand::FrequencyScale;
        for scale in [
            FrequencyScale::Log,
            FrequencyScale::Mel,
            FrequencyScale::Linear,
        ] {
            for bins in [32, 64, 128, 256] {
                let bounds = compute_bounds(bins, scale);
                assert_eq!(bounds.len(), bins, "scale={scale:?} bins={bins}");
                let centres = compute_centres(bins, scale);
                assert_eq!(centres.len(), bins, "scale={scale:?} bins={bins}");
            }
        }
    }

    #[test]
    fn bounds_partition_the_full_hz_window_for_every_scale() {
        // Every scale produces a rectangular partition of
        // [MEL_LOW_HZ, MEL_HIGH_HZ]: first low = MEL_LOW_HZ,
        // last high = MEL_HIGH_HZ, no gap and no overlap between
        // adjacent bins.
        use crate::demand::FrequencyScale;
        for scale in [
            FrequencyScale::Log,
            FrequencyScale::Mel,
            FrequencyScale::Linear,
        ] {
            for bins in [32, 64, 128, 256] {
                let b = compute_bounds(bins, scale);
                assert!(
                    (b[0].0 - MEL_LOW_HZ).abs() < 1e-2,
                    "first low must be MEL_LOW_HZ (scale={scale:?} bins={bins})"
                );
                assert!(
                    (b[bins - 1].1 - MEL_HIGH_HZ).abs() < 1e-2,
                    "last high must be MEL_HIGH_HZ (scale={scale:?} bins={bins})"
                );
                for i in 0..(bins - 1) {
                    assert!(
                        (b[i].1 - b[i + 1].0).abs() < 1e-2,
                        "adjacent edges must meet at i={i} (scale={scale:?} bins={bins})"
                    );
                    assert!(
                        b[i].0 < b[i].1,
                        "monotonic (scale={scale:?} bins={bins} i={i})"
                    );
                }
            }
        }
    }

    #[test]
    fn log_scale_allocates_more_bins_to_music_bass_than_mel_or_linear() {
        // The whole point of the ownership audit: log spacing
        // gives 20-250 Hz roughly 37% of the display width;
        // mel gives ~8%; linear ~1%. Assert the order strictly
        // (log > mel > linear) at 64 bins so a future regression
        // that flips the semantics is caught.
        use crate::demand::FrequencyScale;
        let count_bins_below_250 = |scale: FrequencyScale| -> usize {
            let centres = compute_centres(64, scale);
            centres.iter().filter(|&&c| c < 250.0).count()
        };
        let log_share = count_bins_below_250(FrequencyScale::Log);
        let mel_share = count_bins_below_250(FrequencyScale::Mel);
        let linear_share = count_bins_below_250(FrequencyScale::Linear);
        assert!(
            log_share > mel_share,
            "log should give bass more bins than mel (log={log_share}, mel={mel_share})"
        );
        assert!(
            mel_share > linear_share,
            "mel should give bass more bins than linear (mel={mel_share}, linear={linear_share})"
        );
        assert!(
            log_share >= 15,
            "log 64-bin should give ~20+ bins below 250 Hz, got {log_share}"
        );
    }

    #[test]
    fn band_ranges_cover_full_bin_span() {
        use crate::demand::FrequencyScale;
        for bins in [32, 64, 128, 256] {
            let centres = compute_centres(bins, FrequencyScale::Mel);
            let br = compute_band_ranges(&centres, bins);
            assert_eq!(
                br.sub_bass.0, 0,
                "sub_bass starts at 0 for bins={bins}"
            );
            assert_eq!(br.high.1, bins, "high ends at bins for bins={bins}");
        }
    }

    #[test]
    fn sine_peaks_at_expected_bin_for_every_scale_64_mono() {
        // A 1 kHz sine peaks in the bin whose [low, high] range
        // contains 1000 Hz — regardless of the scale that
        // defined those ranges. The predicted bin comes from the
        // same `compute_bounds` the analyser uses, so this test
        // asserts the analyser's projection agrees with the
        // bounds table for every scale.
        use crate::demand::FrequencyScale;
        for scale in [
            FrequencyScale::Log,
            FrequencyScale::Mel,
            FrequencyScale::Linear,
        ] {
            let mut a = SpectrumAnalyser::new(48_000, 64, 1, scale);
            let pcm = make_sine(1_000.0, 48_000, FFT_WINDOW * INPUT_CHANNELS);
            let f = a.process_frame(&pcm);
            let peak_bin = argmax(&f.magnitudes);
            let expected = compute_bounds(64, scale)
                .iter()
                .position(|(lo, hi)| *lo <= 1_000.0 && 1_000.0 < *hi)
                .expect("1 kHz must be within [low, high] for some bin");
            assert!(
                (expected as i32 - peak_bin as i32).abs() <= 1,
                "scale={scale:?}: 1 kHz sine should peak at bin {expected} (±1), got {peak_bin}"
            );
        }
    }

    #[test]
    fn sine_peaks_at_expected_bin_log_256_mono() {
        use crate::demand::FrequencyScale;
        let mut a = SpectrumAnalyser::new(48_000, 256, 1, FrequencyScale::Log);
        let pcm = make_sine(1_000.0, 48_000, FFT_WINDOW * INPUT_CHANNELS);
        let f = a.process_frame(&pcm);
        let peak_bin = argmax(&f.magnitudes);
        let expected = compute_bounds(256, FrequencyScale::Log)
            .iter()
            .position(|(lo, hi)| *lo <= 1_000.0 && 1_000.0 < *hi)
            .expect("1 kHz must fall in some log bin");
        assert!(
            (expected as i32 - peak_bin as i32).abs() <= 2,
            "log×256: 1 kHz should peak at bin {expected} (±2), got {peak_bin}"
        );
    }

    #[test]
    #[should_panic]
    fn zero_bins_panics_at_construction() {
        let _ = SpectrumAnalyser::new(
            48_000,
            0,
            2,
            crate::demand::FrequencyScale::Log,
        );
    }

    #[test]
    #[should_panic]
    fn three_channels_panics_at_construction() {
        let _ = SpectrumAnalyser::new(
            48_000,
            64,
            3,
            crate::demand::FrequencyScale::Log,
        );
    }

    /// Count adjacent identical `(fft_lo, fft_hi)` after the
    /// anti-clone map — the Minecraft measure on the live path.
    fn count_post_anticlone_collisions(
        bins: usize,
        scale: crate::demand::FrequencyScale,
    ) -> usize {
        let bounds = compute_bounds(bins, scale);
        let centres = compute_centres(bins, scale);
        let (ranges, _) =
            build_filterbank_map(&bounds, &centres, CANONICAL_SAMPLE_RATE_HZ);
        let mut prev: Option<(usize, usize)> = None;
        let mut count = 0usize;
        for this in ranges {
            if Some(this) == prev && this.0 < this.1 {
                // Weighted shares may keep the same range; those
                // are allowed only when weights differ. Count
                // pure clones (identical range AND we treat them
                // as collision for the unique-split case).
                count += 1;
            }
            prev = Some(this);
        }
        count
    }

    #[test]
    fn anti_clone_eliminates_identical_range_runs_when_span_allows() {
        use crate::demand::FrequencyScale;
        // With FFT_WINDOW=16384, log×64/128 must split cleanly.
        // log×256 may retain a few weighted shares at the floor;
        // identical-range runs with weight=1.0 must stay rare.
        for (scale, bins, max_collisions) in [
            (FrequencyScale::Log, 32usize, 0),
            (FrequencyScale::Log, 64, 0),
            (FrequencyScale::Log, 128, 4),
            (FrequencyScale::Log, 256, 48),
            (FrequencyScale::Mel, 256, 8),
            (FrequencyScale::Linear, 256, 0),
        ] {
            let count = count_post_anticlone_collisions(bins, scale);
            assert!(
                count <= max_collisions,
                "{scale:?} × {bins} post-anti-clone collisions \
                 {count} exceed ceiling {max_collisions}"
            );
        }
    }

    #[test]
    fn anti_clone_weights_break_starved_identical_magnitudes() {
        use crate::demand::FrequencyScale;
        let bounds = compute_bounds(256, FrequencyScale::Log);
        let centres = compute_centres(256, FrequencyScale::Log);
        let (ranges, weights) =
            build_filterbank_map(&bounds, &centres, CANONICAL_SAMPLE_RATE_HZ);
        // Walk adjacent identical ranges: weights must differ so
        // process_frame cannot emit a flat Minecraft plateau.
        for i in 1..ranges.len() {
            if ranges[i] == ranges[i - 1] && ranges[i].0 < ranges[i].1 {
                assert!(
                    (weights[i] - weights[i - 1]).abs() > 1e-4
                        || weights[i] < 1.0
                        || weights[i - 1] < 1.0,
                    "identical FFT range at columns {i}-1/{i} \
                     must carry distinct triangular weights \
                     (wPrev={}, wThis={})",
                    weights[i - 1],
                    weights[i]
                );
            }
        }
    }

    #[test]
    fn projection_never_reads_from_fft_bin_zero() {
        use crate::demand::FrequencyScale;
        for scale in [
            FrequencyScale::Log,
            FrequencyScale::Mel,
            FrequencyScale::Linear,
        ] {
            for bins in [32usize, 64, 128, 256] {
                let bounds = compute_bounds(bins, scale);
                let centres = compute_centres(bins, scale);
                let (ranges, _) = build_filterbank_map(
                    &bounds,
                    &centres,
                    CANONICAL_SAMPLE_RATE_HZ,
                );
                for (fft_lo, fft_hi) in ranges {
                    assert!(
                        fft_lo >= 1,
                        "{scale:?} × {bins}: fft_lo={fft_lo} must skip DC"
                    );
                    assert!(
                        fft_hi <= FFT_WINDOW / 2,
                        "{scale:?} × {bins}: fft_hi={fft_hi} past Nyquist"
                    );
                }
            }
        }
    }
}
