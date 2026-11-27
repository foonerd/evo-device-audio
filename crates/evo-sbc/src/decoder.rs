// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Frame decoding: unpack, dequantise, undo joint stereo, synthesise.

use crate::alloc::{allocate, ScaleFactors, MAX_CHANNELS, MAX_SUBBANDS};
use crate::bits::BitReader;
use crate::header::{frame_crc, ChannelMode, FrameHeader, HEADER_LEN};
use crate::tables::{COS_4, COS_8, WIN_4, WIN_8};
use crate::Error;

/// Most blocks in a frame.
const MAX_BLOCKS: usize = 16;

/// Headroom bits carried through dequantisation and the first matrix.
const EXTRA_BITS: u32 = 2;

/// Length of the synthesis history: ten blocks of `2 * subbands`.
const HISTORY: usize = 20 * MAX_SUBBANDS;

/// What a successful [`Decoder::decode`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// The header of the frame.
    pub header: FrameHeader,
    /// Bytes of the input the frame occupied.
    pub consumed: usize,
    /// Samples per channel written to the output.
    pub samples_per_channel: usize,
    /// Channels written (1 or 2); samples are interleaved.
    pub channels: usize,
}

/// A decoder for one SBC stream.
///
/// Its only state is the synthesis filter's history. A frame that is
/// rejected leaves it exactly as it was, so a stream can carry on after a
/// damaged frame (the caller decides what to play in its place).
#[derive(Debug, Clone)]
pub struct Decoder {
    history: [[i32; HISTORY]; MAX_CHANNELS],
    /// `(channels, subbands)` the history belongs to.
    layout: Option<(usize, usize)>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder at the start of a stream.
    pub const fn new() -> Self {
        Self {
            history: [[0; HISTORY]; MAX_CHANNELS],
            layout: None,
        }
    }

    /// Forget the stream: the next frame starts from silence.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Decode the frame at the start of `frame` into `out`, interleaved.
    ///
    /// `out` must hold `channels * samples_per_channel` samples (at most
    /// 256). Bytes after the frame are ignored; `consumed` says where the
    /// next frame starts. On error nothing is written and the decoder is
    /// unchanged.
    pub fn decode(
        &mut self,
        frame: &[u8],
        out: &mut [i16],
    ) -> Result<Decoded, Error> {
        let header = FrameHeader::parse(frame)?;
        let len = header.frame_len();
        let Some(frame) = frame.get(..len) else {
            return Err(Error::TooShort {
                needed: len,
                got: frame.len(),
            });
        };
        let samples = header.samples_per_channel();
        let channels = header.channels();
        if out.len() < samples * channels {
            return Err(Error::OutputTooSmall {
                needed: samples * channels,
                got: out.len(),
            });
        }
        if frame_crc(&header, frame) != header.crc() {
            return Err(Error::BadCrc);
        }

        // Everything that can fail happens before the decoder is touched.
        let subbands = usize::from(header.subbands);
        let blocks = usize::from(header.blocks);
        let mut reader = BitReader::at(frame, 8 * HEADER_LEN);
        let joint = if header.channel_mode == ChannelMode::JointStereo {
            let flags = reader.read(subbands)?;
            // The last subband's flag is not used: it is always coded as
            // plain stereo.
            (0..subbands - 1).fold(0u32, |acc, sb| {
                acc | (((flags >> (subbands - 1 - sb)) & 1) << sb)
            })
        } else {
            0
        };
        let mut sf: ScaleFactors = [[0; MAX_SUBBANDS]; MAX_CHANNELS];
        for ch_sf in sf.iter_mut().take(channels) {
            for v in ch_sf.iter_mut().take(subbands) {
                *v = reader.read(4)? as u8;
            }
        }
        let bits = allocate(&header, &sf);

        let mut samples_sb = [[[0i32; MAX_SUBBANDS]; MAX_CHANNELS]; MAX_BLOCKS];
        for block in samples_sb.iter_mut().take(blocks) {
            for ch in 0..channels {
                for sb in 0..subbands {
                    let n = usize::from(bits[ch][sb]);
                    if n == 0 {
                        continue;
                    }
                    let q = i64::from(reader.read(n)?);
                    let levels = (1i64 << n) - 1;
                    let shift = u32::from(sf[ch][sb]) + 1 + EXTRA_BITS;
                    block[ch][sb] = ((((q << 1) | 1) << shift) / levels
                        - (1i64 << shift))
                        as i32;
                }
            }
            if channels == 2 {
                for sb in 0..subbands {
                    if (joint >> sb) & 1 == 1 {
                        let (mid, side) = (block[0][sb], block[1][sb]);
                        block[0][sb] = mid + side;
                        block[1][sb] = mid - side;
                    }
                }
            }
        }

        if self.layout != Some((channels, subbands)) {
            self.reset();
            self.layout = Some((channels, subbands));
        }
        for (t, block) in samples_sb.iter().take(blocks).enumerate() {
            for ch in 0..channels {
                let mut pcm = [0i16; MAX_SUBBANDS];
                self.synthesise(ch, subbands, &block[ch], &mut pcm);
                for (j, &s) in pcm.iter().take(subbands).enumerate() {
                    out[(t * subbands + j) * channels + ch] = s;
                }
            }
        }
        Ok(Decoded {
            header,
            consumed: len,
            samples_per_channel: samples,
            channels,
        })
    }

    /// One block of one channel through the synthesis filter bank.
    ///
    /// Integer arithmetic throughout, in the widths of the reference
    /// decoder: the matrix is Q13 and shifted down by 15 (the two headroom
    /// bits of dequantisation make up the difference), the window is Q15 and
    /// shifted down by 15.
    ///
    /// Both sums are **32-bit signed and wrap**, as the reference decoder's
    /// do. Audio an encoder made from real signals never gets near the edge,
    /// but a frame of extreme scale factors and bit counts can, and what the
    /// reference makes of it (wrapped, then clamped to 16 bits) is part of
    /// being bit-exact. Wrapping is explicit here, so it is the same in debug
    /// and release builds and on every platform.
    fn synthesise(
        &mut self,
        ch: usize,
        m: usize,
        x: &[i32; MAX_SUBBANDS],
        pcm: &mut [i16; MAX_SUBBANDS],
    ) {
        let v = &mut self.history[ch];
        v.copy_within(0..(20 * m - 2 * m), 2 * m);
        for i in 0..2 * m {
            let row: &[i32] = if m == 4 { &COS_4[i] } else { &COS_8[i] };
            let acc = row.iter().zip(x).fold(0i32, |acc, (&c, &s)| {
                acc.wrapping_add(c.wrapping_mul(s))
            });
            v[i] = acc >> 15;
        }
        for (j, out) in pcm.iter_mut().take(m).enumerate() {
            let mut acc = 0i32;
            for i in 0..5 {
                let (a, b) = (2 * i * m + j, (2 * i + 1) * m + j);
                let (wa, wb) = if m == 4 {
                    (WIN_4[a], WIN_4[b])
                } else {
                    (WIN_8[a], WIN_8[b])
                };
                acc = acc.wrapping_add(wa.wrapping_mul(v[2 * i * 2 * m + j]));
                acc = acc.wrapping_add(
                    wb.wrapping_mul(v[(2 * i + 1) * 2 * m + m + j]),
                );
            }
            *out = saturate(acc >> 15);
        }
    }
}

/// A synthesised sample as 16-bit PCM: values beyond the range are clamped.
fn saturate(sample: i32) -> i16 {
    sample.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::saturate;

    #[test]
    fn samples_beyond_sixteen_bits_are_clamped_not_wrapped() {
        assert_eq!(saturate(0), 0);
        assert_eq!(saturate(32767), 32767);
        assert_eq!(saturate(32768), 32767);
        assert_eq!(saturate(1 << 20), 32767);
        assert_eq!(saturate(-32768), -32768);
        assert_eq!(saturate(-32769), -32768);
        assert_eq!(saturate(i32::MIN), -32768);
        assert_eq!(saturate(i32::MAX), 32767);
    }
}
