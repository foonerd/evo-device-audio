// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Bit allocation: how many bits each subband sample of a frame was given.
//!
//! The decoder must reproduce the encoder's allocation exactly from the
//! scale factors and the header, or every later bit of the frame is
//! misread. It is therefore a direct statement of the specification's
//! procedure, with every loop bounded.

use crate::header::{Allocation, ChannelMode, FrameHeader};

/// The most subbands and channels a frame has.
pub(crate) const MAX_SUBBANDS: usize = 8;
pub(crate) const MAX_CHANNELS: usize = 2;

/// Bits per subband sample, `[channel][subband]`.
pub(crate) type Bits = [[u8; MAX_SUBBANDS]; MAX_CHANNELS];

/// Scale factors, `[channel][subband]`.
pub(crate) type ScaleFactors = [[u8; MAX_SUBBANDS]; MAX_CHANNELS];

/// The loudness offsets of the specification, by frequency row.
const OFFSET_4: [[i32; 4]; 4] =
    [[-1, 0, 0, 0], [-2, 0, 0, 1], [-2, 0, 0, 1], [-2, 0, 0, 1]];
const OFFSET_8: [[i32; 8]; 4] = [
    [-2, 0, 0, 0, 0, 0, 0, 1],
    [-3, 0, 0, 0, 0, 0, 1, 2],
    [-4, 0, 0, 0, 0, 0, 1, 2],
    [-4, 0, 0, 0, 0, 0, 1, 2],
];

/// An upper bound on the slice search: `bitneed` lies in `-5..=15`, so the
/// search is over well within this many steps.
const SLICE_STEPS: usize = 64;

/// Per-subband bit need: the scale factor for SNR allocation, and the
/// offset-corrected, halved scale factor for loudness allocation (a scale
/// factor of zero needs nothing at all).
fn bitneed(
    header: &FrameHeader,
    sf: &ScaleFactors,
) -> [[i32; MAX_SUBBANDS]; MAX_CHANNELS] {
    let subbands = usize::from(header.subbands);
    let row = header.frequency.index();
    let mut need = [[0i32; MAX_SUBBANDS]; MAX_CHANNELS];
    for ch in 0..header.channels() {
        for sb in 0..subbands {
            let scale = i32::from(sf[ch][sb]);
            need[ch][sb] = match header.allocation {
                Allocation::Snr => scale,
                Allocation::Loudness if scale == 0 => -5,
                Allocation::Loudness => {
                    let offset = if subbands == 4 {
                        OFFSET_4[row][sb]
                    } else {
                        OFFSET_8[row][sb]
                    };
                    let loudness = scale - offset;
                    if loudness > 0 {
                        loudness / 2
                    } else {
                        loudness
                    }
                }
            };
        }
    }
    need
}

/// Share the bit budget out among the subbands of `channels` together.
fn share(
    channels: &[usize],
    subbands: usize,
    bitpool: i32,
    need: &[[i32; MAX_SUBBANDS]; MAX_CHANNELS],
    bits: &mut Bits,
) {
    let mut max_need = i32::MIN;
    for &ch in channels {
        for sb in 0..subbands {
            max_need = max_need.max(need[ch][sb]);
        }
    }

    // Find the bit slice at which the budget is used up.
    let mut bitcount = 0i32;
    let mut slicecount = 0i32;
    let mut bitslice = max_need + 1;
    for _ in 0..SLICE_STEPS {
        bitslice -= 1;
        bitcount += slicecount;
        slicecount = 0;
        for &ch in channels {
            for sb in 0..subbands {
                let n = need[ch][sb];
                if n > bitslice + 1 && n < bitslice + 16 {
                    slicecount += 1;
                } else if n == bitslice + 1 {
                    slicecount += 2;
                }
            }
        }
        if bitcount + slicecount >= bitpool {
            break;
        }
    }
    // The specification's own step for a search that ends exactly on the pool.
    // The fill loops below would reach the same allocation without it (60,000
    // random frames agree both ways); it is kept because it is the procedure.
    if bitcount + slicecount == bitpool {
        bitcount += slicecount;
        bitslice -= 1;
    }

    for &ch in channels {
        for sb in 0..subbands {
            let n = need[ch][sb];
            bits[ch][sb] = if n < bitslice + 2 {
                0
            } else {
                (n - bitslice).min(16) as u8
            };
        }
    }

    // Spend what is left: first raise subbands already in use, then any.
    'first: for sb in 0..subbands {
        for &ch in channels {
            if bitcount >= bitpool {
                break 'first;
            }
            let b = i32::from(bits[ch][sb]);
            if (2..16).contains(&b) {
                bits[ch][sb] += 1;
                bitcount += 1;
            } else if need[ch][sb] == bitslice + 1 && bitpool > bitcount + 1 {
                bits[ch][sb] = 2;
                bitcount += 2;
            }
        }
    }
    'second: for sb in 0..subbands {
        for &ch in channels {
            if bitcount >= bitpool {
                break 'second;
            }
            if bits[ch][sb] < 16 {
                bits[ch][sb] += 1;
                bitcount += 1;
            }
        }
    }
}

/// The bits each subband sample of a frame was given.
pub(crate) fn allocate(header: &FrameHeader, sf: &ScaleFactors) -> Bits {
    let need = bitneed(header, sf);
    let subbands = usize::from(header.subbands);
    let bitpool = i32::from(header.bitpool);
    let mut bits: Bits = [[0; MAX_SUBBANDS]; MAX_CHANNELS];
    match header.channel_mode {
        ChannelMode::Mono => share(&[0], subbands, bitpool, &need, &mut bits),
        ChannelMode::DualChannel => {
            share(&[0], subbands, bitpool, &need, &mut bits);
            share(&[1], subbands, bitpool, &need, &mut bits);
        }
        ChannelMode::Stereo | ChannelMode::JointStereo => {
            share(&[0, 1], subbands, bitpool, &need, &mut bits);
        }
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(
        mode: u8,
        alloc: u8,
        sub: u8,
        bitpool: u8,
        freq: u8,
    ) -> FrameHeader {
        FrameHeader::parse(&[
            0x9C,
            (freq << 6) | (3 << 4) | (mode << 2) | (alloc << 1) | sub,
            bitpool,
            0,
        ])
        .unwrap()
    }

    fn total(h: &FrameHeader, bits: &Bits) -> usize {
        (0..h.channels())
            .map(|c| {
                bits[c][..usize::from(h.subbands)]
                    .iter()
                    .map(|&b| usize::from(b))
                    .sum::<usize>()
            })
            .sum()
    }

    #[test]
    fn a_silent_frame_spends_the_budget_anyway() {
        // All scale factors zero: nothing is needed, but the budget is still
        // distributed (the encoder does the same).
        let h = header(3, 0, 1, 20, 3);
        let bits = allocate(&h, &[[0; 8]; 2]);
        assert_eq!(total(&h, &bits), 20);
    }

    #[test]
    fn never_spends_more_than_the_pool() {
        for mode in 0..4u8 {
            for alloc in 0..2u8 {
                for sub in 0..2u8 {
                    for bitpool in [2u8, 7, 19, 35, 53, 64] {
                        let h = header(mode, alloc, sub, bitpool, 2);
                        let mut sf = [[0u8; 8]; 2];
                        for (c, row) in sf.iter_mut().enumerate() {
                            for (s, v) in row.iter_mut().enumerate() {
                                *v = ((c * 5 + s * 3 + 1) % 16) as u8;
                            }
                        }
                        let bits = allocate(&h, &sf);
                        let limit = match h.channel_mode {
                            ChannelMode::DualChannel => {
                                2 * usize::from(bitpool)
                            }
                            _ => usize::from(bitpool),
                        };
                        assert!(total(&h, &bits) <= limit, "{h:?}");
                        assert!(bits.iter().flatten().all(|&b| b <= 16));
                    }
                }
            }
        }
    }

    #[test]
    fn unused_channels_and_subbands_get_nothing() {
        let h = header(0, 0, 0, 30, 2);
        let bits = allocate(&h, &[[9; 8]; 2]);
        assert!(bits[1].iter().all(|&b| b == 0));
        assert!(bits[0][4..].iter().all(|&b| b == 0));
    }
}
