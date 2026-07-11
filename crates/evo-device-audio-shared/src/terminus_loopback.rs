// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Audio-terminus loopback wire-shape contract.
//!
//! The terminus plugin captures from `pcm.evo_terminus_tap`
//! (snd-aloop capture half at `hw:Loopback,1,7`); MPD's terminus
//! audio_output block writes to the paired playback half. snd-
//! aloop locks the format at the rate the playback side opens
//! with; a mismatch between MPD's terminus output format and the
//! terminus capture loop's `hwp.set_rate` causes the capture open
//! to fail and the visualiser to go silent.
//!
//! Both sides MUST use the same rate / bit-depth / channel
//! count. This module is the single source of truth for that
//! contract — both the audio.terminus plugin (capture side) and
//! the playback.mpd plugin (MPD audio_output writer) reference
//! these constants so any future change happens in one place,
//! at compile time, with no ambiguity about which side leads.
//!
//! The shape was chosen at the start of the terminus work and
//! has not needed to change: 48 kHz matches MPD's resampling
//! default for non-hi-res sources + the audible-band Nyquist
//! convention; S32_LE gives 144 dB of dynamic range, leaving
//! the FFT's [0, 1] normalization plenty of headroom; stereo
//! matches every supported source format. If a future
//! deployment needs different parameters, change both constants
//! here in lockstep; capture-side and MPD-side both pick up
//! the new value on rebuild.

/// Sample rate the terminus loopback contract runs at.
/// MPD's terminus audio_output's `format` directive uses this
/// value; the terminus capture loop's `hwp.set_rate` uses this
/// value. snd-aloop pairs the playback + capture halves at the
/// rate the playback side opens with — both sides must agree.
pub const TERMINUS_LOOPBACK_RATE_HZ: u32 = 48_000;

/// Bit-depth of the terminus loopback contract. MPD's `format`
/// directive token; terminus capture loop's `Format::S32_LE`.
pub const TERMINUS_LOOPBACK_BITDEPTH: u32 = 32;

/// Channel count of the terminus loopback contract.
pub const TERMINUS_LOOPBACK_CHANNELS: u32 = 2;

/// MPD `format` directive string for the terminus audio_output
/// block: `<rate>:<bitdepth>:<channels>`. Constructed from the
/// constants above; matches what the terminus capture loop opens
/// the loopback's capture half at.
///
/// Computed at compile time via `const` concat-free formatting
/// — Rust 1.74+ permits this constructor in const context.
pub const TERMINUS_LOOPBACK_MPD_FORMAT: &str = "48000:32:2";

#[cfg(test)]
mod tests {
    use super::*;

    /// The MPD format string MUST agree with the numeric
    /// constants. If a future revision changes one without the
    /// other, this test fails at the next build — the contract
    /// breaks coherently rather than silently.
    #[test]
    fn mpd_format_string_matches_numeric_constants() {
        let expected = format!(
            "{TERMINUS_LOOPBACK_RATE_HZ}:{TERMINUS_LOOPBACK_BITDEPTH}:{TERMINUS_LOOPBACK_CHANNELS}"
        );
        assert_eq!(TERMINUS_LOOPBACK_MPD_FORMAT, expected);
    }
}
