// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The four-byte frame header, the frame length it implies, and the frame CRC.

/// The first byte of every SBC frame.
pub const SYNCWORD: u8 = 0x9C;

/// Bytes in the fixed part of the header (syncword, parameters, bitpool, CRC).
pub const HEADER_LEN: usize = 4;

/// The sampling frequency a frame declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    /// 16 kHz.
    Hz16000,
    /// 32 kHz.
    Hz32000,
    /// 44.1 kHz.
    Hz44100,
    /// 48 kHz.
    Hz48000,
}

impl Frequency {
    /// The frequency in hertz.
    pub const fn hz(self) -> u32 {
        match self {
            Frequency::Hz16000 => 16_000,
            Frequency::Hz32000 => 32_000,
            Frequency::Hz44100 => 44_100,
            Frequency::Hz48000 => 48_000,
        }
    }

    /// The two-bit field of the header: the row of the loudness offset table.
    pub(crate) const fn index(self) -> usize {
        match self {
            Frequency::Hz16000 => 0,
            Frequency::Hz32000 => 1,
            Frequency::Hz44100 => 2,
            Frequency::Hz48000 => 3,
        }
    }
}

/// How the channels of a frame are coded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMode {
    /// One channel.
    Mono,
    /// Two independent channels, each with its own bit budget.
    DualChannel,
    /// Two channels sharing one bit budget.
    Stereo,
    /// Two channels sharing one bit budget, with per-subband mid/side coding.
    JointStereo,
}

/// How a frame's bits are shared out between subbands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allocation {
    /// By the ear's sensitivity at each subband.
    Loudness,
    /// By signal-to-noise ratio alone.
    Snr,
}

/// A parsed, validated frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Sampling frequency.
    pub frequency: Frequency,
    /// Blocks per frame: 4, 8, 12 or 16.
    pub blocks: u8,
    /// Channel coding.
    pub channel_mode: ChannelMode,
    /// Bit allocation method.
    pub allocation: Allocation,
    /// Subbands: 4 or 8.
    pub subbands: u8,
    /// Bits per block available to the frame's audio (per channel for
    /// [`ChannelMode::DualChannel`], shared otherwise).
    pub bitpool: u8,
    crc: u8,
}

/// Why a frame was not decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than the header or the frame it describes.
    TooShort {
        /// Bytes needed.
        needed: usize,
        /// Bytes given.
        got: usize,
    },
    /// The first byte is not the syncword.
    BadSyncword(u8),
    /// The bitpool is outside what the specification allows for the mode.
    InvalidBitpool(u8),
    /// The CRC in the header does not match the frame.
    BadCrc,
    /// The output buffer cannot hold the decoded samples.
    OutputTooSmall {
        /// Samples needed (all channels).
        needed: usize,
        /// Samples the buffer holds.
        got: usize,
    },
    /// The frame's audio ran past its end (cannot happen for a frame that
    /// passed the CRC and the length check; reported rather than assumed).
    Truncated,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::TooShort { needed, got } => {
                write!(f, "frame needs {needed} bytes, {got} given")
            }
            Error::BadSyncword(b) => {
                write!(f, "not an SBC frame (first byte {b:#04x})")
            }
            Error::InvalidBitpool(b) => write!(f, "invalid bitpool {b}"),
            Error::BadCrc => write!(f, "frame CRC mismatch"),
            Error::OutputTooSmall { needed, got } => {
                write!(f, "output holds {got} samples, {needed} needed")
            }
            Error::Truncated => write!(f, "frame audio truncated"),
        }
    }
}

impl std::error::Error for Error {}

impl FrameHeader {
    /// Parse and validate the first four bytes of a frame.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let Some(&[sync, params, bitpool, crc]) =
            bytes.first_chunk::<HEADER_LEN>()
        else {
            return Err(Error::TooShort {
                needed: HEADER_LEN,
                got: bytes.len(),
            });
        };
        if sync != SYNCWORD {
            return Err(Error::BadSyncword(sync));
        }
        let frequency = match params >> 6 {
            0 => Frequency::Hz16000,
            1 => Frequency::Hz32000,
            2 => Frequency::Hz44100,
            _ => Frequency::Hz48000,
        };
        let blocks = 4 * (1 + ((params >> 4) & 3));
        let channel_mode = match (params >> 2) & 3 {
            0 => ChannelMode::Mono,
            1 => ChannelMode::DualChannel,
            2 => ChannelMode::Stereo,
            _ => ChannelMode::JointStereo,
        };
        let allocation = if (params >> 1) & 1 == 0 {
            Allocation::Loudness
        } else {
            Allocation::Snr
        };
        let subbands = if params & 1 == 0 { 4 } else { 8 };
        let header = FrameHeader {
            frequency,
            blocks,
            channel_mode,
            allocation,
            subbands,
            bitpool,
            crc,
        };
        if bitpool < 2 || usize::from(bitpool) > header.max_bitpool() {
            return Err(Error::InvalidBitpool(bitpool));
        }
        Ok(header)
    }

    /// Channels in the decoded output: 1 or 2.
    pub const fn channels(&self) -> usize {
        match self.channel_mode {
            ChannelMode::Mono => 1,
            _ => 2,
        }
    }

    /// Samples per channel a frame decodes to.
    pub const fn samples_per_channel(&self) -> usize {
        self.blocks as usize * self.subbands as usize
    }

    /// The largest bitpool the mode and subband count can use: every subband
    /// sample at its 16-bit ceiling, capped at the 250 of the specification.
    const fn max_bitpool(&self) -> usize {
        let per_channel = 16 * self.subbands as usize;
        let budget = match self.channel_mode {
            ChannelMode::Mono | ChannelMode::DualChannel => per_channel,
            ChannelMode::Stereo | ChannelMode::JointStereo => 2 * per_channel,
        };
        if budget > 250 {
            250
        } else {
            budget
        }
    }

    /// Bits of joint-stereo flags that follow the header (0 unless joint).
    pub(crate) const fn joint_bits(&self) -> usize {
        match self.channel_mode {
            ChannelMode::JointStereo => self.subbands as usize,
            _ => 0,
        }
    }

    /// Bits of scale factors: four per subband per channel.
    pub(crate) const fn scale_factor_bits(&self) -> usize {
        4 * self.subbands as usize * self.channels()
    }

    /// The total length of the frame in bytes.
    ///
    /// The joint-stereo flags, the scale factors and the audio are one
    /// continuous run of bits after the four-byte header, padded to a whole
    /// byte once, at the end.
    pub const fn frame_len(&self) -> usize {
        let blocks = self.blocks as usize;
        let bitpool = self.bitpool as usize;
        let audio_bits = match self.channel_mode {
            ChannelMode::Mono
            | ChannelMode::Stereo
            | ChannelMode::JointStereo => blocks * bitpool,
            ChannelMode::DualChannel => blocks * bitpool * 2,
        };
        HEADER_LEN
            + (self.joint_bits() + self.scale_factor_bits() + audio_bits)
                .div_ceil(8)
    }

    /// The CRC the header carries.
    pub(crate) const fn crc(&self) -> u8 {
        self.crc
    }
}

/// The CRC of a frame: polynomial 0x1D, initial value 0x0F, over bytes 1 and
/// 2 of the header and then the joint-stereo flags and scale factors that
/// follow it, bit by bit.
///
/// `frame` must hold at least `header.frame_len()` bytes.
pub(crate) fn frame_crc(header: &FrameHeader, frame: &[u8]) -> u8 {
    let body_bits = header.joint_bits() + header.scale_factor_bits();
    let mut crc = 0x0Fu8;
    let mut feed = |bit: u8| {
        let top = (crc >> 7) ^ bit;
        crc <<= 1;
        if top & 1 == 1 {
            crc ^= 0x1D;
        }
    };
    for byte in &frame[1..3] {
        for i in (0..8).rev() {
            feed((byte >> i) & 1);
        }
    }
    for n in 0..body_bits {
        let bit = 8 * HEADER_LEN + n;
        feed((frame[bit >> 3] >> (7 - (bit & 7))) & 1);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parameter byte from its fields: 2-bit frequency, 2-bit block code,
    /// 2-bit channel mode, allocation bit, subband bit.
    fn params(freq: u8, blocks: u8, mode: u8, alloc: u8, sub: u8) -> u8 {
        (freq << 6) | (blocks << 4) | (mode << 2) | (alloc << 1) | sub
    }

    /// 44.1 kHz, 16 blocks, joint stereo, loudness, 8 subbands, bitpool 53.
    fn hq() -> [u8; 4] {
        [0x9C, params(2, 3, 3, 0, 1), 53, 0x00]
    }

    #[test]
    fn parses_every_field() {
        let h = FrameHeader::parse(&hq()).unwrap();
        assert_eq!(h.frequency, Frequency::Hz44100);
        assert_eq!(h.frequency.hz(), 44_100);
        assert_eq!(h.blocks, 16);
        assert_eq!(h.channel_mode, ChannelMode::JointStereo);
        assert_eq!(h.allocation, Allocation::Loudness);
        assert_eq!(h.subbands, 8);
        assert_eq!(h.bitpool, 53);
        assert_eq!((h.channels(), h.samples_per_channel()), (2, 128));
    }

    #[test]
    fn frame_length_is_the_documented_formula() {
        // joint, 8 subbands, 16 blocks, bitpool 53: 4 + ceil((8 + 64) / 8) + ceil(16 * 53 / 8)
        assert_eq!(FrameHeader::parse(&hq()).unwrap().frame_len(), 4 + 9 + 106);
        // the 119-byte frame a computer sends over A2DP at 48 kHz
        let pw = [0x9C, params(3, 3, 3, 0, 1), 53, 0];
        assert_eq!(FrameHeader::parse(&pw).unwrap().frame_len(), 119);
        // mono, 4 subbands, 4 blocks, bitpool 2: 4 + 2 + 1
        let mono = [0x9C, params(0, 0, 0, 0, 0), 2, 0];
        assert_eq!(FrameHeader::parse(&mono).unwrap().frame_len(), 7);
        // dual, 4 subbands, 4 blocks, bitpool 5: 4 + ceil((32 + 4 * 5 * 2) / 8)
        let dual = [0x9C, params(0, 0, 1, 0, 0), 5, 0];
        assert_eq!(FrameHeader::parse(&dual).unwrap().frame_len(), 4 + 9);
    }

    #[test]
    fn the_bits_after_the_header_are_padded_once_not_per_part() {
        // Joint, 4 subbands, 12 blocks, bitpool 25: 4 flag bits + 32 scale
        // factor bits + 300 audio bits = 336 bits = 42 bytes, so 46 in all.
        // Padding the flags and scale factors to a byte first and the audio
        // after would give 47 and put every following frame one byte out.
        let h =
            FrameHeader::parse(&[0x9C, params(1, 2, 3, 1, 0), 25, 0]).unwrap();
        assert_eq!(
            (h.blocks, h.subbands, h.channel_mode),
            (12, 4, ChannelMode::JointStereo)
        );
        assert_eq!(h.frame_len(), 46);
        // The same with 8 flag bits (8 subbands) is byte-aligned either way.
        let h8 =
            FrameHeader::parse(&[0x9C, params(1, 2, 3, 1, 1), 25, 0]).unwrap();
        assert_eq!(h8.frame_len(), 4 + (8 + 64 + 300usize).div_ceil(8));
    }

    #[test]
    fn rejects_what_is_not_a_frame() {
        assert_eq!(
            FrameHeader::parse(&[]),
            Err(Error::TooShort { needed: 4, got: 0 })
        );
        assert_eq!(
            FrameHeader::parse(&[0x9C, 0, 0]),
            Err(Error::TooShort { needed: 4, got: 3 })
        );
        assert_eq!(
            FrameHeader::parse(&[0xAD, 0, 8, 0]),
            Err(Error::BadSyncword(0xAD))
        );
    }

    #[test]
    fn rejects_bitpools_outside_the_specification() {
        for bp in [0u8, 1] {
            assert_eq!(
                FrameHeader::parse(&[0x9C, params(2, 3, 3, 0, 1), bp, 0]),
                Err(Error::InvalidBitpool(bp))
            );
        }
        // mono, 4 subbands: at most 64
        assert!(
            FrameHeader::parse(&[0x9C, params(2, 3, 0, 0, 0), 64, 0]).is_ok()
        );
        assert_eq!(
            FrameHeader::parse(&[0x9C, params(2, 3, 0, 0, 0), 65, 0]),
            Err(Error::InvalidBitpool(65))
        );
        // joint, 8 subbands: capped at 250
        assert!(
            FrameHeader::parse(&[0x9C, params(2, 3, 3, 0, 1), 250, 0]).is_ok()
        );
        assert_eq!(
            FrameHeader::parse(&[0x9C, params(2, 3, 3, 0, 1), 251, 0]),
            Err(Error::InvalidBitpool(251))
        );
    }
}
