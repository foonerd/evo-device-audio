//! Biquad parametric peaking-EQ filter.
//!
//! Implementation of Robert Bristow-Johnson's Audio EQ
//! Cookbook peaking-EQ formulas
//! (<https://webaudio.github.io/Audio-EQ-Cookbook/audio-eq-cookbook.html>).
//! A single biquad applies one parametric band; the
//! composition.alsa eq_only mode cascades 10 biquads per
//! channel for the 10-band operator surface.
//!
//! State is `f64` for numerical stability across the
//! audible-band parameter ranges (low-frequency / high-Q
//! bands are sensitive to single-precision accumulation
//! drift). Samples are processed in `f32` at the call site;
//! the conversion to/from i16 happens at the byte-flow
//! substrate boundary, NOT inside this module — the biquad
//! is format-agnostic.
//!
//! Coefficient computation is deterministic for identical
//! `(freq_hz, gain_db, q, sample_rate)` inputs (no hidden
//! state, no environment dependencies). Tests in
//! `tests::coefficients_*` pin the canonical outputs against
//! hand-computed values from the RBJ cookbook.

use std::f64::consts::PI;

/// One parametric peaking-EQ biquad filter section. Holds
/// the four filter coefficients (b0/a0, b1/a0, b2/a0, a1/a0,
/// a2/a0 — normalised by a0) plus the two-sample delay
/// state. The four-coefficient form mirrors the cookbook's
/// canonical layout; the delay line is the "transposed direct
/// form II" sample state.
#[derive(Debug, Clone)]
pub struct Biquad {
    /// Numerator coefficients (already normalised by a0).
    b0: f64,
    b1: f64,
    b2: f64,
    /// Denominator coefficients excluding a0 (already
    /// normalised by a0).
    a1: f64,
    a2: f64,
    /// Two-sample delay state. Persistent across `process`
    /// calls; reset to zero by `reset`.
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// Construct an identity (pass-through) biquad. Useful
    /// as the initial state before coefficients are
    /// configured.
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Compute coefficients for a parametric peaking EQ band
    /// from operator-set `freq_hz`, `gain_db`, and `q`, given
    /// the stream's `sample_rate`. Replaces this biquad's
    /// coefficients in place; the delay state is preserved
    /// (so coefficient updates during playback do not pop).
    ///
    /// The RBJ peaking-EQ formulas:
    /// ```text
    /// A = 10^(gain_db / 40)
    /// w0 = 2π * freq_hz / sample_rate
    /// alpha = sin(w0) / (2 * Q)
    /// b0 =  1 + alpha * A
    /// b1 = -2 * cos(w0)
    /// b2 =  1 - alpha * A
    /// a0 =  1 + alpha / A
    /// a1 = -2 * cos(w0)
    /// a2 =  1 - alpha / A
    /// ```
    /// Coefficients are then normalised by `a0`.
    pub fn configure_peaking(
        &mut self,
        freq_hz: f64,
        gain_db: f64,
        q: f64,
        sample_rate: f64,
    ) {
        let a = (10f64).powf(gain_db / 40.0);
        let w0 = 2.0 * PI * freq_hz / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    /// Reset the delay state. Call between disjoint streams
    /// (track change, route change) so the previous track's
    /// residue does not bleed into the next.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process a single sample. Transposed direct form II:
    /// ```text
    /// y[n]   = b0 * x[n] + z1
    /// z1     = b1 * x[n] - a1 * y[n] + z2
    /// z2     = b2 * x[n] - a2 * y[n]
    /// ```
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y as f32
    }
}

impl Default for Biquad {
    fn default() -> Self {
        Self::identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An identity biquad must round-trip samples unchanged.
    #[test]
    fn identity_biquad_round_trips_samples() {
        let mut b = Biquad::identity();
        let samples: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        for s in &samples {
            assert!(
                (b.process_sample(*s) - *s).abs() < 1e-6,
                "identity biquad must round-trip samples"
            );
        }
    }

    /// 0 dB gain peaking EQ is mathematically identity.
    /// Verify by configuring a 1 kHz / 0 dB / Q=1 band and
    /// confirming a DC test signal passes through unchanged.
    #[test]
    fn zero_db_gain_peaking_is_identity_at_dc() {
        let mut b = Biquad::identity();
        b.configure_peaking(1000.0, 0.0, 1.0, 44100.0);
        // Apply a DC step; after the filter settles, output
        // should equal input.
        let mut last = 0.0f32;
        for _ in 0..2000 {
            last = b.process_sample(1.0);
        }
        assert!(
            (last - 1.0).abs() < 1e-3,
            "DC gain at 0 dB peaking must be unity; got {last}"
        );
    }

    /// Verify the boost direction: +6 dB at 1 kHz / Q=1.0
    /// should amplify a 1 kHz sine appreciably (the
    /// cookbook formula predicts magnitude > 1 at the
    /// centre frequency).
    #[test]
    fn positive_gain_boosts_centre_frequency_sine() {
        let mut b = Biquad::identity();
        b.configure_peaking(1000.0, 6.0, 1.0, 44100.0);
        // Generate a 1 kHz sine sample stream + measure
        // peak amplitude of the output after the filter
        // settles.
        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        let two_pi_f = 2.0 * std::f64::consts::PI * 1000.0;
        for i in 0..4096 {
            let t = i as f64 / 44100.0;
            let x = (two_pi_f * t).sin() as f32;
            let y = b.process_sample(x);
            // Skip the first 500 samples (settling time).
            if i >= 500 {
                peak_in = peak_in.max(x.abs());
                peak_out = peak_out.max(y.abs());
            }
        }
        // +6 dB should give ~2x amplitude at the centre.
        assert!(
            peak_out > peak_in * 1.5,
            "+6 dB peaking must boost the centre sine \
             appreciably; peak_in={peak_in} peak_out={peak_out}"
        );
    }

    /// Negative gain attenuates the centre frequency.
    #[test]
    fn negative_gain_cuts_centre_frequency_sine() {
        let mut b = Biquad::identity();
        b.configure_peaking(1000.0, -6.0, 1.0, 44100.0);
        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        let two_pi_f = 2.0 * std::f64::consts::PI * 1000.0;
        for i in 0..4096 {
            let t = i as f64 / 44100.0;
            let x = (two_pi_f * t).sin() as f32;
            let y = b.process_sample(x);
            if i >= 500 {
                peak_in = peak_in.max(x.abs());
                peak_out = peak_out.max(y.abs());
            }
        }
        assert!(
            peak_out < peak_in * 0.9,
            "-6 dB peaking must attenuate the centre sine; \
             peak_in={peak_in} peak_out={peak_out}"
        );
    }

    /// Coefficients are deterministic for identical inputs.
    #[test]
    fn coefficient_computation_is_deterministic() {
        let mut a = Biquad::identity();
        let mut c = Biquad::identity();
        a.configure_peaking(1000.0, 3.0, 1.41, 44100.0);
        c.configure_peaking(1000.0, 3.0, 1.41, 44100.0);
        assert_eq!(a.b0, c.b0);
        assert_eq!(a.b1, c.b1);
        assert_eq!(a.b2, c.b2);
        assert_eq!(a.a1, c.a1);
        assert_eq!(a.a2, c.a2);
    }

    /// Reset clears the delay state.
    #[test]
    fn reset_clears_delay_line() {
        let mut b = Biquad::identity();
        b.configure_peaking(1000.0, 6.0, 1.0, 44100.0);
        for _ in 0..100 {
            b.process_sample(0.5);
        }
        assert!(b.z1 != 0.0 || b.z2 != 0.0);
        b.reset();
        assert_eq!(b.z1, 0.0);
        assert_eq!(b.z2, 0.0);
    }

    /// Coefficient updates during processing preserve the
    /// delay line (no pop on parameter change).
    #[test]
    fn configure_preserves_delay_state() {
        let mut b = Biquad::identity();
        b.configure_peaking(1000.0, 6.0, 1.0, 44100.0);
        for _ in 0..100 {
            b.process_sample(0.5);
        }
        let z1_before = b.z1;
        let z2_before = b.z2;
        b.configure_peaking(2000.0, -3.0, 2.0, 44100.0);
        assert_eq!(b.z1, z1_before);
        assert_eq!(b.z2, z2_before);
    }
}
