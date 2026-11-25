// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! 10-band parametric EQ DSP for the composition.alsa
//! `eq_only` mode.
//!
//! Cascades 10 [`Biquad`] filters per channel. Operates on
//! interleaved PCM byte buffers in the negotiated sample
//! format. Two sample formats are supported in this build —
//! `s16le` and `f32le` — covering the vast majority of Linux
//! audio chains. Other formats refuse at the mode-select
//! gesture with a structured Permanent error so the operator
//! sees the gap explicitly.
//!
//! The module is single-canonical-path: one `process_bytes`
//! function dispatches on `(sample_format, channels)`; the
//! biquad-cascade-per-channel state lives in
//! [`EqProcessor`]. Coefficient updates take a fresh
//! `[EqBandParams]` snapshot; the per-channel
//! cascades recompute in place, preserving delay state.

use crate::biquad::Biquad;

/// Number of parametric EQ bands per channel. Pinned by the
/// schema's `eq-band-count-and-domain` acceptance row;
/// consumer plugins on the audio.composition shelf honour
/// this count.
pub const EQ_BAND_COUNT: usize = 10;

/// The most bands one cascade holds. The operator's own EQ has
/// [`EQ_BAND_COUNT`]; a zone's EQ can have up to this many.
pub const EQ_BAND_MAX_COUNT: usize = 31;

/// Maximum channel count the DSP handles internally.
/// Sample-format dispatch covers mono + stereo for the
/// reference build; multichannel (5.1, 7.1) is vendor-
/// distribution territory.
const MAX_CHANNELS: usize = 2;

/// Parameters for one parametric EQ band. Mirrors the
/// `audio.options.set_eq_band` wire shape 1:1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EqBandParams {
    pub freq_hz: u32,
    pub gain_db: f32,
    pub q: f32,
}

impl Default for EqBandParams {
    fn default() -> Self {
        Self {
            freq_hz: 1000,
            gain_db: 0.0,
            q: 1.0,
        }
    }
}

/// Supported sample formats for the eq_only DSP path. The
/// `Other` variant carries the unsupported format string so
/// the mode-select refusal can name it explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EqSampleFormat {
    /// 16-bit signed little-endian PCM.
    S16Le,
    /// 32-bit IEEE 754 little-endian float PCM.
    F32Le,
    /// Any other format. Carries the format string so the
    /// refusal at mode-select can name it.
    Other(String),
}

impl EqSampleFormat {
    /// Bytes per sample per channel. `Other` returns 0 (the
    /// caller refuses the mode before opening any chain so
    /// this never reaches a byte loop).
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            Self::S16Le => 2,
            Self::F32Le => 4,
            Self::Other(_) => 0,
        }
    }
}

/// One band of a cascade: its filter, and whether it does anything.
/// A band at 0 dB is an identity filter, so it is left out of the
/// cascade's work rather than run for nothing.
#[derive(Debug, Clone)]
struct CascadeBand {
    biquad: Biquad,
    active: bool,
}

/// Per-channel biquad cascade. Owns one biquad per band, up to
/// [`EQ_BAND_MAX_COUNT`]; processes one channel's interleaved
/// samples through the bands that do something, in sequence.
#[derive(Debug, Clone)]
struct ChannelCascade {
    bands: Vec<CascadeBand>,
}

impl ChannelCascade {
    fn new() -> Self {
        Self { bands: Vec::new() }
    }

    /// Take a fresh band list. The filter memory of a band that
    /// stays is kept, so a change of one band does not click the
    /// others; a band that is new starts clear.
    fn configure(&mut self, bands: &[EqBandParams], sample_rate: f64) {
        let wanted = bands.len().min(EQ_BAND_MAX_COUNT);
        self.bands.resize_with(wanted, || CascadeBand {
            biquad: Biquad::identity(),
            active: false,
        });
        for (band, params) in self.bands.iter_mut().zip(bands.iter()) {
            band.biquad.configure_peaking(
                params.freq_hz as f64,
                params.gain_db as f64,
                params.q as f64,
                sample_rate,
            );
            let active = params.gain_db != 0.0;
            if active && !band.active {
                // It was not running; its memory is from before it
                // stopped, so it starts clear rather than clicking.
                band.biquad.reset();
            }
            band.active = active;
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        let mut s = sample;
        for band in self.bands.iter_mut().filter(|b| b.active) {
            s = band.biquad.process_sample(s);
        }
        s
    }

    #[allow(dead_code)]
    fn reset(&mut self) {
        for band in self.bands.iter_mut() {
            band.biquad.reset();
        }
    }
}

/// Parametric EQ processor (up to [`EQ_BAND_MAX_COUNT`] bands). Holds one cascade per
/// channel; `process_bytes` consumes interleaved PCM in the
/// configured format and writes back the filtered stream
/// in place.
#[derive(Debug, Clone)]
pub struct EqProcessor {
    channels: usize,
    sample_format: EqSampleFormat,
    sample_rate: f64,
    cascades: [ChannelCascade; MAX_CHANNELS],
}

impl EqProcessor {
    /// Construct a processor for the negotiated stream
    /// format. Caller MUST refuse the eq_only mode-select
    /// gesture before this point if `sample_format` is
    /// `Other(..)`; the byte-flow path assumes a supported
    /// format.
    pub fn new(
        channels: usize,
        sample_format: EqSampleFormat,
        sample_rate: u32,
    ) -> Self {
        assert!(
            (1..=MAX_CHANNELS).contains(&channels),
            "EqProcessor supports 1..={} channels; got {channels}",
            MAX_CHANNELS
        );
        Self {
            channels,
            sample_format,
            sample_rate: sample_rate as f64,
            cascades: std::array::from_fn(|_| ChannelCascade::new()),
        }
    }

    /// Recompute all biquad coefficients from a fresh band
    /// parameter snapshot. Preserves the per-channel delay
    /// state (no pop on parameter change).
    pub fn configure_bands(&mut self, bands: &[EqBandParams]) {
        for ch in 0..self.channels {
            self.cascades[ch].configure(bands, self.sample_rate);
        }
    }

    /// Reset all per-channel delay state. Call between
    /// disjoint streams.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        for ch in 0..self.channels {
            self.cascades[ch].reset();
        }
    }

    /// Process an interleaved PCM byte buffer in place.
    /// Returns `Err` on a buffer length that is not a clean
    /// multiple of `channels * bytes_per_sample` (the
    /// substrate guarantees frame alignment; a mismatch
    /// indicates a buffer-boundary bug the caller surfaces).
    pub fn process_bytes(&mut self, buf: &mut [u8]) -> Result<(), EqDspError> {
        let bps = self.sample_format.bytes_per_sample();
        if bps == 0 {
            return Err(EqDspError::UnsupportedFormat(format!(
                "{:?}",
                self.sample_format
            )));
        }
        let frame_size = self.channels * bps;
        if buf.len() % frame_size != 0 {
            return Err(EqDspError::UnalignedBuffer {
                buf_len: buf.len(),
                frame_size,
            });
        }
        match self.sample_format {
            EqSampleFormat::S16Le => self.process_s16le(buf),
            EqSampleFormat::F32Le => self.process_f32le(buf),
            EqSampleFormat::Other(_) => {
                unreachable!("process_bytes refused unsupported format above")
            }
        }
        Ok(())
    }

    fn process_s16le(&mut self, buf: &mut [u8]) {
        let frames = buf.len() / (2 * self.channels);
        for f in 0..frames {
            for ch in 0..self.channels {
                let off = (f * self.channels + ch) * 2;
                let raw = i16::from_le_bytes([buf[off], buf[off + 1]]);
                let sample = raw as f32 / 32768.0;
                let out = self.cascades[ch].process(sample);
                let clamped = (out * 32768.0)
                    .clamp(i16::MIN as f32, i16::MAX as f32)
                    as i16;
                let bytes = clamped.to_le_bytes();
                buf[off] = bytes[0];
                buf[off + 1] = bytes[1];
            }
        }
    }

    fn process_f32le(&mut self, buf: &mut [u8]) {
        let frames = buf.len() / (4 * self.channels);
        for f in 0..frames {
            for ch in 0..self.channels {
                let off = (f * self.channels + ch) * 4;
                let raw = f32::from_le_bytes([
                    buf[off],
                    buf[off + 1],
                    buf[off + 2],
                    buf[off + 3],
                ]);
                let out = self.cascades[ch].process(raw);
                // f32 PCM is not clamped — operator-supplied
                // EQ gain that pushes above ±1.0 will saturate
                // at the DAC; that's the operator's choice and
                // the audiophile-grade reference does not
                // silently rescale.
                let bytes = out.to_le_bytes();
                buf[off] = bytes[0];
                buf[off + 1] = bytes[1];
                buf[off + 2] = bytes[2];
                buf[off + 3] = bytes[3];
            }
        }
    }
}

/// Errors raised by the EQ DSP path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EqDspError {
    #[error("eq_only mode does not support sample format {0}")]
    UnsupportedFormat(String),

    #[error(
        "byte buffer length {buf_len} is not a multiple of \
         frame size {frame_size}; substrate alignment bug"
    )]
    UnalignedBuffer { buf_len: usize, frame_size: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_bands() -> [EqBandParams; EQ_BAND_COUNT] {
        [EqBandParams::default(); EQ_BAND_COUNT]
    }

    #[test]
    fn flat_bands_pass_dc_unchanged_after_settling() {
        let mut p = EqProcessor::new(2, EqSampleFormat::F32Le, 44100);
        p.configure_bands(&flat_bands());
        // Build a DC f32 buffer (interleaved stereo).
        let mut buf = Vec::with_capacity(4096);
        for _ in 0..512 {
            for _ in 0..2 {
                buf.extend_from_slice(&1.0f32.to_le_bytes());
            }
        }
        p.process_bytes(&mut buf).unwrap();
        // Read back the LAST sample and confirm it's close
        // to 1.0 (filters have settled).
        let last_off = buf.len() - 4;
        let last = f32::from_le_bytes([
            buf[last_off],
            buf[last_off + 1],
            buf[last_off + 2],
            buf[last_off + 3],
        ]);
        assert!(
            (last - 1.0).abs() < 1e-3,
            "flat EQ on DC must pass through; got {last}"
        );
    }

    #[test]
    fn process_bytes_refuses_unaligned_buffer() {
        let mut p = EqProcessor::new(2, EqSampleFormat::F32Le, 44100);
        p.configure_bands(&flat_bands());
        // Stereo f32 frame = 8 bytes; pass 7 to force misalign.
        let mut buf = vec![0u8; 7];
        let err = p.process_bytes(&mut buf).unwrap_err();
        match err {
            EqDspError::UnalignedBuffer {
                buf_len,
                frame_size,
            } => {
                assert_eq!(buf_len, 7);
                assert_eq!(frame_size, 8);
            }
            other => panic!("expected UnalignedBuffer, got {other:?}"),
        }
    }

    #[test]
    fn process_bytes_refuses_unsupported_format_explicitly() {
        let mut p =
            EqProcessor::new(2, EqSampleFormat::Other("s24le".into()), 44100);
        p.configure_bands(&flat_bands());
        let mut buf = vec![0u8; 16];
        let err = p.process_bytes(&mut buf).unwrap_err();
        assert!(
            matches!(err, EqDspError::UnsupportedFormat(_)),
            "expected UnsupportedFormat, got {err:?}"
        );
    }

    #[test]
    fn s16le_dc_round_trips_with_flat_bands() {
        let mut p = EqProcessor::new(2, EqSampleFormat::S16Le, 44100);
        p.configure_bands(&flat_bands());
        // Build a DC s16le buffer (interleaved stereo,
        // sample value 8000 / 32768 = 0.244).
        let mut buf = Vec::with_capacity(4096);
        for _ in 0..1024 {
            for _ in 0..2 {
                buf.extend_from_slice(&8000i16.to_le_bytes());
            }
        }
        p.process_bytes(&mut buf).unwrap();
        // Confirm the LAST sample is close to the input
        // (within the i16-quantisation noise floor).
        let last_off = buf.len() - 2;
        let last = i16::from_le_bytes([buf[last_off], buf[last_off + 1]]);
        assert!(
            (last - 8000).abs() < 100,
            "flat EQ on s16le DC must pass through; got {last}"
        );
    }

    #[test]
    fn positive_gain_boosts_centre_frequency_sine_f32le() {
        let mut p = EqProcessor::new(2, EqSampleFormat::F32Le, 44100);
        let mut bands = flat_bands();
        bands[0] = EqBandParams {
            freq_hz: 1000,
            gain_db: 6.0,
            q: 1.0,
        };
        p.configure_bands(&bands);

        // Build a 1 kHz sine, stereo, ~100 ms.
        let mut buf = Vec::with_capacity(44100);
        let two_pi_f = 2.0 * std::f64::consts::PI * 1000.0;
        for i in 0..4410 {
            let t = i as f64 / 44100.0;
            let x = (two_pi_f * t).sin() as f32;
            for _ in 0..2 {
                buf.extend_from_slice(&x.to_le_bytes());
            }
        }
        let mut copy = buf.clone();
        p.process_bytes(&mut copy).unwrap();

        // Measure peak amplitude on the second half of the
        // buffer (after the filter has settled). The boost
        // should yield peak_out > peak_in * 1.5.
        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        for f in 2205..4410 {
            let off = f * 2 * 4;
            let x = f32::from_le_bytes([
                buf[off],
                buf[off + 1],
                buf[off + 2],
                buf[off + 3],
            ]);
            let y = f32::from_le_bytes([
                copy[off],
                copy[off + 1],
                copy[off + 2],
                copy[off + 3],
            ]);
            peak_in = peak_in.max(x.abs());
            peak_out = peak_out.max(y.abs());
        }
        assert!(
            peak_out > peak_in * 1.5,
            "+6 dB band must boost the centre sine; \
             peak_in={peak_in} peak_out={peak_out}"
        );
    }

    #[test]
    fn process_is_deterministic_across_invocations() {
        let mut p1 = EqProcessor::new(2, EqSampleFormat::F32Le, 44100);
        let mut p2 = EqProcessor::new(2, EqSampleFormat::F32Le, 44100);
        let mut bands = flat_bands();
        bands[0] = EqBandParams {
            freq_hz: 1000,
            gain_db: 3.0,
            q: 1.0,
        };
        p1.configure_bands(&bands);
        p2.configure_bands(&bands);

        let mut buf1 = vec![0u8; 800];
        for f in 0..100 {
            let v = (f as f32 * 0.01).to_le_bytes();
            for ch in 0..2 {
                let off = (f * 2 + ch) * 4;
                buf1[off..off + 4].copy_from_slice(&v);
            }
        }
        let mut buf2 = buf1.clone();
        p1.process_bytes(&mut buf1).unwrap();
        p2.process_bytes(&mut buf2).unwrap();
        assert_eq!(buf1, buf2, "EQ DSP must be deterministic");
    }

    // ----- more bands -------------------------------------------

    fn tone_s16(freq: f32, frames: usize, amp: f32) -> Vec<u8> {
        let mut out = Vec::with_capacity(frames * 4);
        for n in 0..frames {
            let v = (amp
                * (2.0 * std::f32::consts::PI * freq * n as f32 / 44_100.0)
                    .sin()) as i16;
            for _ in 0..2 {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    fn peak_db(buf: &[u8], freq: f32, skip: usize) -> f64 {
        let frames = buf.len() / 4;
        let (mut c, mut s) = (0f64, 0f64);
        let k = 2.0 * std::f64::consts::PI * freq as f64 / 44_100.0;
        for f in skip..frames {
            let v = i16::from_le_bytes([buf[f * 4], buf[f * 4 + 1]]) as f64;
            c += v * (k * f as f64).cos();
            s += v * (k * f as f64).sin();
        }
        let n = (frames - skip) as f64;
        20.0 * (2.0 * c.hypot(s) / n / 32768.0).log10()
    }

    /// A 31-band cascade applies several bands at once, each at its
    /// own frequency: three tones, three different gains.
    #[test]
    fn thirty_one_bands_each_act_at_their_own_frequency() {
        let mut bands = vec![EqBandParams::default(); EQ_BAND_MAX_COUNT];
        for (i, b) in bands.iter_mut().enumerate() {
            b.freq_hz = 20 + (i as u32) * 20;
        }
        bands[5] = EqBandParams {
            freq_hz: 125,
            gain_db: 6.1,
            q: 4.0,
        };
        bands[17] = EqBandParams {
            freq_hz: 1000,
            gain_db: -4.7,
            q: 4.0,
        };
        bands[28] = EqBandParams {
            freq_hz: 8000,
            gain_db: 2.3,
            q: 4.0,
        };
        let mut p = EqProcessor::new(2, EqSampleFormat::S16Le, 44_100);
        p.configure_bands(&bands);
        for (freq, want) in
            [(125.0_f32, 6.1_f64), (1000.0, -4.7), (8000.0, 2.3)]
        {
            let input = tone_s16(freq, 44_100, 3000.0);
            let mut out = input.clone();
            p.reset();
            p.configure_bands(&bands);
            p.process_bytes(&mut out).unwrap();
            let got = peak_db(&out, freq, 8000) - peak_db(&input, freq, 8000);
            assert!(
                (got - want).abs() < 0.15,
                "{freq} Hz: wanted {want} dB, got {got:.2} dB"
            );
        }
    }

    /// The cascade follows the band count it is given, in either
    /// direction, and a flat band changes nothing.
    #[test]
    fn the_band_count_can_change_under_a_running_cascade() {
        let mut p = EqProcessor::new(2, EqSampleFormat::S16Le, 44_100);
        let one = vec![EqBandParams {
            freq_hz: 1000,
            gain_db: 6.0,
            q: 2.0,
        }];
        let mut many = one.clone();
        many.extend(vec![EqBandParams::default(); 30]);
        for set in [&one, &many, &one, &many] {
            p.configure_bands(set);
            let input = tone_s16(1000.0, 44_100, 3000.0);
            let mut out = input.clone();
            p.reset();
            p.process_bytes(&mut out).unwrap();
            let got =
                peak_db(&out, 1000.0, 8000) - peak_db(&input, 1000.0, 8000);
            assert!(
                (got - 6.0).abs() < 0.15,
                "{} bands: {got:.2} dB",
                set.len()
            );
        }
    }

    /// A cascade never holds more than the most, however many it is
    /// handed, and an empty list is a flat, silent-of-effect cascade.
    #[test]
    fn a_cascade_is_capped_and_an_empty_list_is_transparent() {
        let mut p = EqProcessor::new(2, EqSampleFormat::S16Le, 44_100);
        p.configure_bands(&vec![
            EqBandParams {
                freq_hz: 1000,
                gain_db: 3.0,
                q: 1.0
            };
            40
        ]);
        assert_eq!(p.cascades[0].bands.len(), EQ_BAND_MAX_COUNT);
        p.configure_bands(&[]);
        let input = tone_s16(1000.0, 4410, 3000.0);
        let mut out = input.clone();
        p.process_bytes(&mut out).unwrap();
        assert_eq!(out, input);
    }

    /// A band that was flat and is brought back starts clear, so
    /// bringing it in does not play what it held before it stopped.
    #[test]
    fn a_band_brought_back_from_flat_starts_with_clear_memory() {
        let mut p = EqProcessor::new(2, EqSampleFormat::S16Le, 44_100);
        let on = vec![EqBandParams {
            freq_hz: 200,
            gain_db: 12.0,
            q: 1.0,
        }];
        let off = vec![EqBandParams {
            freq_hz: 200,
            gain_db: 0.0,
            q: 1.0,
        }];
        p.configure_bands(&on);
        let mut loud = tone_s16(200.0, 4410, 20000.0);
        p.process_bytes(&mut loud).unwrap();
        p.configure_bands(&off);
        let mut quiet = tone_s16(200.0, 4410, 100.0);
        p.process_bytes(&mut quiet).unwrap();
        p.configure_bands(&on);
        assert!(
            p.cascades[0].bands[0].biquad.is_clear(),
            "its memory starts clear"
        );
    }
}
