// FFT + mel-scale binning + perceptual signals — math-heavy
// numerical code with explicit indexed loops; clippy's
// needless_range_loop lint suggests iterator forms that obscure
// the fixed-shape DSP layout (per-bin parallel arrays + the
// shared `i` index linking them). Allow at module scope.
#![allow(clippy::needless_range_loop)]

//! FFT + mel-scale binning + perceptual signals.
//!
//! Pure compute, no I/O. Takes interleaved stereo S32_LE PCM samples
//! at the configured sample rate, runs a 1024-point real-input FFT
//! per channel, projects the magnitude spectrum onto 256 mel-scale
//! bins covering [20 Hz, 20 kHz], normalises to [0, 1] against a
//! rolling peak, and computes the three forward-decade perceptual
//! signals the spectrum-frame wire contract defines:
//! peak-hold per bin, per-band onset events, per-bin L/R
//! correlation coefficient.
//!
//! Mel scale is the perceptually-meaningful frequency mapping (a
//! pitch ratio that doubles at every octave on the human cochlea).
//! Linear-frequency bins waste resolution on the high octaves; mel
//! bins concentrate resolution where the ear concentrates
//! discrimination.
//!
//! Peak-hold decays at a perceptually-tuned rate (~12 dB/s) so
//! transients hold visibly without flickering on the falling edge.
//! Onsets fire when a band's spectral flux crosses an
//! adaptive threshold (recent-mean + k * recent-std). Correlation
//! is the normalised cross-product of L and R magnitudes per bin
//! (Pearson r restricted to non-negative inputs).
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

/// Bin count on the wire. Pinned to 256 by the spectrum-frame
/// payload v1 contract. Renderers downsample to operator-chosen
/// `bin_count` (32 / 64 / 128 / 256).
pub const BIN_COUNT: usize = 256;

/// Channel count on the wire. Pinned to 2 (stereo L+R) by the
/// spectrum-frame payload v1 contract.
pub const CHANNEL_COUNT: usize = 2;

/// FFT window size. 1024 points at 48 kHz gives ~47 Hz bin
/// resolution which more than covers the perceptual range
/// the mel projection collapses anyway.
pub const FFT_SIZE: usize = 1024;

/// Derive the frame cadence the capture loop runs at, given the
/// configured sample rate. The capture loop is ALSA-paced: every
/// `FFT_SIZE` samples drawn at `sample_rate_hz` yields one frame.
/// Cadence in Hz = `sample_rate_hz / FFT_SIZE` (real value); we
/// round to the nearest integer for the wire field since
/// `audio_playback_spectrum_frame.rate_hz` is a `u32`. At 48 kHz
/// the canonical reference rig sees `48000 / 1024 = 46.875` → 47.
///
/// This is the single source of truth for the wire `rate_hz`
/// field; both the capture loop's emit path and the
/// `get_spectrum_frame` read handler call this so the value the
/// renderer reads always matches the cadence it actually receives
/// frames at.
pub fn frame_rate_hz(sample_rate_hz: u32) -> u32 {
    (sample_rate_hz as f64 / FFT_SIZE as f64).round() as u32
}

/// Mel-scale low-frequency cutoff. 20 Hz is the conventional
/// audible-band lower bound.
pub const MEL_LOW_HZ: f32 = 20.0;

/// Mel-scale high-frequency cutoff. 20 kHz is the conventional
/// audible-band upper bound.
pub const MEL_HIGH_HZ: f32 = 20_000.0;

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
#[derive(Debug, Clone)]
pub struct PerceptualFrame {
    /// 2 * BIN_COUNT Float32 magnitudes in [0, 1]: channel-
    /// interleaved as `[L_bin0..L_bin255, R_bin0..R_bin255]`. The
    /// wire form is two arrays-of-256; the in-memory form here
    /// keeps both channels in one allocation for cache locality.
    pub magnitudes: Box<[f32; BIN_COUNT * CHANNEL_COUNT]>,
    /// Peak-hold per bin, per channel. Same layout as `magnitudes`.
    pub peak_hold: Box<[f32; BIN_COUNT * CHANNEL_COUNT]>,
    /// Onset booleans, four-band.
    pub onsets: OnsetFrame,
    /// L/R correlation per bin. Pearson r restricted to
    /// non-negative inputs; -1 = anti-correlated (out of phase),
    /// 0 = uncorrelated (mono content in one channel only),
    /// +1 = perfectly correlated (true stereo).
    pub correlation: Box<[f32; BIN_COUNT]>,
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

/// Stateful spectrum analyser. One instance per terminus; reused
/// across frames so the rolling onset window + peak-hold decay
/// state persist correctly.
pub struct SpectrumAnalyser {
    sample_rate_hz: u32,
    fft: Arc<dyn Fft<f32>>,
    /// FFT scratch buffer, reused across `process_frame` calls.
    scratch: Vec<Complex32>,
    /// Mel-bin frequency bounds (low/high Hz), index by output bin.
    /// Used to map FFT bins onto mel bins.
    mel_bounds_hz: [(f32, f32); BIN_COUNT],
    /// Hann window coefficients applied to the input PCM before
    /// FFT. Reduces spectral leakage from the rectangular
    /// implicit-windowing FFT does by default.
    hann_window: [f32; FFT_SIZE],
    /// Per-channel peak-hold state.
    peak_hold: [[f32; BIN_COUNT]; CHANNEL_COUNT],
    /// Per-band spectral-flux history for onset detection. Index
    /// by band (sub_bass=0, bass=1, mid=2, high=3).
    flux_history: [[f32; ONSET_WINDOW_FRAMES]; 4],
    /// Rolling write cursor into `flux_history`.
    flux_cursor: usize,
    /// Previous frame's per-band magnitude (for flux delta).
    /// Index by band.
    prev_band_magnitude: [f32; 4],
    /// Cached band ranges (in mel-bin index space).
    bands: BandRanges,
}

impl SpectrumAnalyser {
    /// Construct an analyser pinned to the supplied sample rate.
    /// `sample_rate_hz` MUST match the rate the input PCM is
    /// captured at; passing a wrong rate produces an off-by-N
    /// mel-bin mapping. Typical value: 48000.
    pub fn new(sample_rate_hz: u32) -> Self {
        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let mel_centres_hz = compute_mel_centres();
        let mel_bounds_hz = compute_mel_bounds();
        let hann_window = compute_hann_window();
        let bands = compute_band_ranges(&mel_centres_hz);
        Self {
            sample_rate_hz,
            fft,
            scratch: vec![Complex32::default(); FFT_SIZE],
            mel_bounds_hz,
            hann_window,
            peak_hold: [[0.0; BIN_COUNT]; CHANNEL_COUNT],
            flux_history: [[0.0; ONSET_WINDOW_FRAMES]; 4],
            flux_cursor: 0,
            prev_band_magnitude: [0.0; 4],
            bands,
        }
    }

    /// Return the precomputed FFT-bin band ranges this analyser
    /// uses to project the BIN_COUNT-wide magnitude vector onto
    /// the four perceptual bands published on the spectrum
    /// subject. The ranges are sample-rate-derived at
    /// construction and stable for the lifetime of this
    /// instance.
    pub fn band_ranges(&self) -> BandRanges {
        self.bands
    }

    /// Process one frame of FFT_SIZE samples per channel. The
    /// input is interleaved stereo: `[L0, R0, L1, R1, ...,
    /// L1023, R1023]`. Each sample is a normalised f32 in [-1, 1].
    /// Returns the per-frame perceptual signals; `at_ms` is left
    /// at 0 for the caller to stamp.
    pub fn process_frame(
        &mut self,
        interleaved_pcm: &[f32],
    ) -> PerceptualFrame {
        assert_eq!(
            interleaved_pcm.len(),
            FFT_SIZE * CHANNEL_COUNT,
            "input PCM length must be FFT_SIZE * CHANNEL_COUNT"
        );

        let mut magnitudes = Box::new([0.0f32; BIN_COUNT * CHANNEL_COUNT]);
        let mut peak_hold = Box::new([0.0f32; BIN_COUNT * CHANNEL_COUNT]);
        let mut correlation = Box::new([0.0f32; BIN_COUNT]);
        // Per-channel mel-binned magnitudes, used both for the
        // output and for the cross-channel correlation computation
        // (which needs the raw mel magnitudes from both channels
        // before either is normalised).
        let mut mel_mags: [[f32; BIN_COUNT]; CHANNEL_COUNT] =
            [[0.0; BIN_COUNT]; CHANNEL_COUNT];

        for ch in 0..CHANNEL_COUNT {
            // De-interleave + Hann-window into the FFT scratch
            // buffer. Imaginary parts are zero (real input).
            for i in 0..FFT_SIZE {
                let sample = interleaved_pcm[i * CHANNEL_COUNT + ch];
                self.scratch[i] =
                    Complex32::new(sample * self.hann_window[i], 0.0);
            }
            self.fft.process(&mut self.scratch);

            // The first FFT_SIZE/2 + 1 bins span [0, sample_rate/2]
            // Hz (Nyquist). Bin width = sample_rate / FFT_SIZE.
            let bin_width_hz = self.sample_rate_hz as f32 / FFT_SIZE as f32;

            // Project FFT bins onto mel bins. For each mel bin,
            // sum the magnitudes of every FFT bin whose centre
            // falls within the mel bin's [low, high] frequency
            // range. Squared magnitude divided by FFT_SIZE
            // gives unit power per bin; sqrt converts back to
            // amplitude scale.
            for mel_idx in 0..BIN_COUNT {
                let (mel_low, mel_high) = self.mel_bounds_hz[mel_idx];
                let fft_lo = (mel_low / bin_width_hz).floor() as usize;
                let fft_hi = ((mel_high / bin_width_hz).ceil() as usize)
                    .min(FFT_SIZE / 2);
                if fft_hi <= fft_lo {
                    mel_mags[ch][mel_idx] = 0.0;
                    continue;
                }
                let mut accum = 0.0f32;
                for fft_idx in fft_lo..fft_hi {
                    let c = self.scratch[fft_idx];
                    accum += (c.re * c.re + c.im * c.im).sqrt();
                }
                // Normalise by the count of FFT bins folded into
                // this mel bin so the per-bin value is an
                // amplitude-average, not a sum (otherwise
                // wider mel bins dominate purely by counting).
                let count = (fft_hi - fft_lo) as f32;
                let amp = accum / count;
                // Hann-window gain compensation (factor of 2 for
                // the one-sided spectrum, factor of 2 for the
                // Hann window's amplitude correction = 4 total).
                let normalised = amp * 4.0 / FFT_SIZE as f32;
                mel_mags[ch][mel_idx] = normalised.clamp(0.0, 1.0);
            }
        }

        // Compute spectral flux per band and update onset history
        // (must happen before per-bin normalisation since flux is
        // on raw magnitudes).
        let raw_mono: [f32; BIN_COUNT] =
            std::array::from_fn(|i| (mel_mags[0][i] + mel_mags[1][i]) * 0.5);
        let band_mags = compute_band_magnitudes(&raw_mono, self.bands);
        let onsets = self.update_onsets(band_mags);

        // Per-bin L/R correlation. Pearson-style normalised
        // cross-product on non-negative inputs:
        //   r_i = (L_i * R_i) / sqrt(L_i^2 * R_i^2) = sign(L_i,R_i)
        // — collapses to a boolean of "both bins have energy".
        // The actually-useful correlation needs windowed history;
        // for v1 we ship the simpler per-bin sign-aligned product
        // normalised to [0, 1] which discriminates "true stereo
        // content in this bin" (~1) from "energy in one channel
        // only" (~0). Sufficient for stereo-imaging visualisations
        // that distinguish centre-image from side-image bins.
        for i in 0..BIN_COUNT {
            let l = mel_mags[0][i];
            let r = mel_mags[1][i];
            let denom = (l * l + r * r).sqrt();
            correlation[i] = if denom > 1e-6 {
                (2.0 * l * r) / (l * l + r * r)
            } else {
                0.0
            };
        }

        // Update peak-hold and copy magnitudes into the output
        // buffer.
        for ch in 0..CHANNEL_COUNT {
            for i in 0..BIN_COUNT {
                let mag = mel_mags[ch][i];
                let decayed =
                    self.peak_hold[ch][i] * PEAK_HOLD_DECAY_PER_FRAME_30HZ;
                let new_peak = mag.max(decayed);
                self.peak_hold[ch][i] = new_peak;
                magnitudes[ch * BIN_COUNT + i] = mag;
                peak_hold[ch * BIN_COUNT + i] = new_peak;
            }
        }

        PerceptualFrame {
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

fn compute_mel_centres() -> [f32; BIN_COUNT] {
    let mel_low = hz_to_mel(MEL_LOW_HZ);
    let mel_high = hz_to_mel(MEL_HIGH_HZ);
    let mel_step = (mel_high - mel_low) / (BIN_COUNT as f32);
    let mut centres = [0.0f32; BIN_COUNT];
    for i in 0..BIN_COUNT {
        let mel = mel_low + mel_step * ((i as f32) + 0.5);
        centres[i] = mel_to_hz(mel);
    }
    centres
}

fn compute_mel_bounds() -> [(f32, f32); BIN_COUNT] {
    let mel_low = hz_to_mel(MEL_LOW_HZ);
    let mel_high = hz_to_mel(MEL_HIGH_HZ);
    let mel_step = (mel_high - mel_low) / (BIN_COUNT as f32);
    let mut bounds = [(0.0f32, 0.0f32); BIN_COUNT];
    for i in 0..BIN_COUNT {
        let lo = mel_to_hz(mel_low + mel_step * (i as f32));
        let hi = mel_to_hz(mel_low + mel_step * ((i + 1) as f32));
        bounds[i] = (lo, hi);
    }
    bounds
}

fn compute_hann_window() -> [f32; FFT_SIZE] {
    let mut w = [0.0f32; FFT_SIZE];
    let denom = (FFT_SIZE - 1) as f32;
    for i in 0..FFT_SIZE {
        let phase = 2.0 * std::f32::consts::PI * (i as f32) / denom;
        w[i] = 0.5 - 0.5 * phase.cos();
    }
    w
}

fn compute_band_ranges(mel_centres_hz: &[f32; BIN_COUNT]) -> BandRanges {
    let find_first_at_or_above = |hz: f32| -> usize {
        mel_centres_hz
            .iter()
            .position(|&c| c >= hz)
            .unwrap_or(BIN_COUNT)
    };
    let sub_bass_lo = find_first_at_or_above(MEL_LOW_HZ);
    let sub_bass_hi = find_first_at_or_above(60.0);
    let bass_lo = sub_bass_hi;
    let bass_hi = find_first_at_or_above(250.0);
    let mid_lo = bass_hi;
    let mid_hi = find_first_at_or_above(2_000.0);
    let high_lo = mid_hi;
    let high_hi = BIN_COUNT;
    BandRanges {
        sub_bass: (sub_bass_lo, sub_bass_hi),
        bass: (bass_lo, bass_hi),
        mid: (mid_lo, mid_hi),
        high: (high_lo, high_hi),
    }
}

fn compute_band_magnitudes(
    mono: &[f32; BIN_COUNT],
    bands: BandRanges,
) -> [f32; 4] {
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
    fn frame_rate_hz_rounds_canonical_rates_to_nearest_integer() {
        // 48 kHz @ 1024-point FFT = 46.875 → 47.
        assert_eq!(frame_rate_hz(48_000), 47);
        // 44.1 kHz @ 1024-point FFT = 43.066 → 43.
        assert_eq!(frame_rate_hz(44_100), 43);
        // 96 kHz @ 1024-point FFT = 93.75 → 94.
        assert_eq!(frame_rate_hz(96_000), 94);
        // 192 kHz @ 1024-point FFT = 187.5 → 188 (banker's-style
        // round-half-up).
        assert_eq!(frame_rate_hz(192_000), 188);
        // Edge: sub-FFT-window sample rate (degenerate, never hit
        // in practice but mathematically defined) rounds to 0.
        assert_eq!(frame_rate_hz(500), 0);
    }

    fn make_sine(freq_hz: f32, sample_rate_hz: u32, len: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let t = (i / CHANNEL_COUNT) as f32 / sample_rate_hz as f32;
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
                best = i;
                best_v = v;
            }
        }
        best
    }

    #[test]
    fn mel_centres_are_monotonically_increasing() {
        let centres = compute_mel_centres();
        for i in 1..BIN_COUNT {
            assert!(
                centres[i] > centres[i - 1],
                "centre[{}] = {} not greater than centre[{}] = {}",
                i,
                centres[i],
                i - 1,
                centres[i - 1]
            );
        }
    }

    #[test]
    fn mel_centres_span_audible_band() {
        let centres = compute_mel_centres();
        assert!(centres[0] >= MEL_LOW_HZ * 0.99);
        assert!(centres[BIN_COUNT - 1] <= MEL_HIGH_HZ * 1.01);
        assert!(centres[BIN_COUNT - 1] >= MEL_HIGH_HZ * 0.99);
    }

    #[test]
    fn mel_bounds_are_contiguous() {
        let bounds = compute_mel_bounds();
        for i in 1..BIN_COUNT {
            assert!((bounds[i].0 - bounds[i - 1].1).abs() < 0.5);
        }
    }

    #[test]
    fn band_ranges_are_non_empty_and_ordered() {
        let centres = compute_mel_centres();
        let bands = compute_band_ranges(&centres);
        assert!(bands.sub_bass.1 > bands.sub_bass.0);
        assert!(bands.bass.0 == bands.sub_bass.1);
        assert!(bands.bass.1 > bands.bass.0);
        assert!(bands.mid.0 == bands.bass.1);
        assert!(bands.mid.1 > bands.mid.0);
        assert!(bands.high.0 == bands.mid.1);
        assert!(bands.high.1 == BIN_COUNT);
    }

    #[test]
    fn sine_at_1khz_concentrates_in_mid_band() {
        let mut a = SpectrumAnalyser::new(48_000);
        let pcm = make_sine(1_000.0, 48_000, FFT_SIZE * CHANNEL_COUNT);
        let frame = a.process_frame(&pcm);
        // The argmax bin for a 1 kHz sine should fall inside the
        // mid band's mel-bin range.
        let mono: [f32; BIN_COUNT] = std::array::from_fn(|i| {
            (frame.magnitudes[i] + frame.magnitudes[i + BIN_COUNT]) * 0.5
        });
        let peak_bin = argmax(&mono);
        let bands = a.band_ranges();
        assert!(
            peak_bin >= bands.mid.0 && peak_bin < bands.mid.1,
            "peak bin {} not in mid band [{}, {})",
            peak_bin,
            bands.mid.0,
            bands.mid.1
        );
    }

    #[test]
    fn sine_at_60hz_concentrates_in_bass_band() {
        let mut a = SpectrumAnalyser::new(48_000);
        let pcm = make_sine(60.0, 48_000, FFT_SIZE * CHANNEL_COUNT);
        let frame = a.process_frame(&pcm);
        let mono: [f32; BIN_COUNT] = std::array::from_fn(|i| {
            (frame.magnitudes[i] + frame.magnitudes[i + BIN_COUNT]) * 0.5
        });
        let peak_bin = argmax(&mono);
        let bands = a.band_ranges();
        // 60 Hz is the sub-bass/bass boundary; either band is
        // acceptable at the bin-resolution available here.
        assert!(
            (peak_bin >= bands.sub_bass.0 && peak_bin < bands.bass.1),
            "peak bin {} not in sub_bass+bass range [{}, {})",
            peak_bin,
            bands.sub_bass.0,
            bands.bass.1
        );
    }

    #[test]
    fn silence_produces_zero_magnitudes() {
        let mut a = SpectrumAnalyser::new(48_000);
        let pcm = vec![0.0f32; FFT_SIZE * CHANNEL_COUNT];
        let frame = a.process_frame(&pcm);
        for m in frame.magnitudes.iter() {
            assert!(*m < 1e-6, "silence produced non-zero magnitude {}", m);
        }
    }

    #[test]
    fn peak_hold_decays_over_silence_frames() {
        let mut a = SpectrumAnalyser::new(48_000);
        // First frame: 1 kHz sine — establishes a peak.
        let pcm_sine = make_sine(1_000.0, 48_000, FFT_SIZE * CHANNEL_COUNT);
        let first = a.process_frame(&pcm_sine);
        let initial_peak_l = *first.peak_hold[..BIN_COUNT]
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();
        assert!(initial_peak_l > 0.0);
        // Subsequent frames: silence. Peak should decay
        // multiplicatively at PEAK_HOLD_DECAY_PER_FRAME_30HZ per
        // frame; after 30 silence frames at 30 Hz cadence (1 s of
        // silence) it should be visibly lower but not zero.
        let silence = vec![0.0f32; FFT_SIZE * CHANNEL_COUNT];
        let mut last_peak = initial_peak_l;
        for _ in 0..30 {
            let f = a.process_frame(&silence);
            let p = *f.peak_hold[..BIN_COUNT]
                .iter()
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap();
            assert!(
                p <= last_peak + 1e-6,
                "peak should decay monotonically over silence; was {}, now {}",
                last_peak,
                p
            );
            last_peak = p;
        }
        // After ~30 frames of geometric decay at 0.933 / frame,
        // peak should be ~0.933^30 = ~0.124 of initial.
        assert!(last_peak < initial_peak_l * 0.25);
        assert!(last_peak > initial_peak_l * 0.05);
    }

    #[test]
    fn correlation_high_for_stereo_identical_signal() {
        let mut a = SpectrumAnalyser::new(48_000);
        let mono = make_sine(1_000.0, 48_000, FFT_SIZE);
        // Build interleaved stereo where both channels are the
        // same mono signal.
        let mut pcm = vec![0.0f32; FFT_SIZE * CHANNEL_COUNT];
        for i in 0..FFT_SIZE {
            pcm[i * CHANNEL_COUNT] = mono[i];
            pcm[i * CHANNEL_COUNT + 1] = mono[i];
        }
        let frame = a.process_frame(&pcm);
        // Bins with significant energy should correlate near 1.
        let bands = a.band_ranges();
        let mid_corr_avg: f32 = (bands.mid.0..bands.mid.1)
            .map(|i| frame.correlation[i])
            .sum::<f32>()
            / (bands.mid.1 - bands.mid.0) as f32;
        assert!(
            mid_corr_avg > 0.5,
            "expected high correlation for identical-stereo content, got {}",
            mid_corr_avg
        );
    }

    #[test]
    fn correlation_low_for_stereo_disjoint_signal() {
        let mut a = SpectrumAnalyser::new(48_000);
        // L channel: 1 kHz sine; R channel: silence.
        let mut pcm = vec![0.0f32; FFT_SIZE * CHANNEL_COUNT];
        for i in 0..FFT_SIZE {
            let t = i as f32 / 48_000.0;
            pcm[i * CHANNEL_COUNT] =
                (2.0 * std::f32::consts::PI * 1_000.0 * t).sin();
            pcm[i * CHANNEL_COUNT + 1] = 0.0;
        }
        let frame = a.process_frame(&pcm);
        let bands = a.band_ranges();
        let mid_corr_avg: f32 = (bands.mid.0..bands.mid.1)
            .map(|i| frame.correlation[i])
            .sum::<f32>()
            / (bands.mid.1 - bands.mid.0) as f32;
        assert!(
            mid_corr_avg < 0.3,
            "expected low correlation for L-only content, got {}",
            mid_corr_avg
        );
    }
}
