// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The decoder is fed by a radio, so it is fed by strangers. Whatever it is
//! given, it must return a result (never panic, never loop) and a rejected
//! frame must leave it exactly as it was.

use evo_sbc::{Decoder, Error, FrameHeader};

/// A small, fixed xorshift generator: the tests are deterministic.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

/// The bit-serial CRC of the specification, written independently of the
/// crate's own so a mistake in one shows up as a failure of the other.
fn crc(frame: &[u8], body_bits: usize) -> u8 {
    let mut crc = 0x0Fu8;
    let mut bits: Vec<u8> = Vec::new();
    for b in &frame[1..3] {
        for i in (0..8).rev() {
            bits.push((b >> i) & 1);
        }
    }
    for n in 0..body_bits {
        let at = 32 + n;
        bits.push((frame[at / 8] >> (7 - at % 8)) & 1);
    }
    for bit in bits {
        let top = (crc >> 7) ^ bit;
        crc <<= 1;
        if top & 1 == 1 {
            crc ^= 0x1D;
        }
    }
    crc
}

/// A frame for the given header bytes with `fill` as every payload byte and
/// a correct CRC: well-formed as far as the checks go, whatever it says.
fn frame_with(params: u8, bitpool: u8, fill: u8) -> Vec<u8> {
    let h =
        FrameHeader::parse(&[0x9C, params, bitpool, 0]).expect("valid header");
    let mut f = vec![fill; h.frame_len()];
    f[0] = 0x9C;
    f[1] = params;
    f[2] = bitpool;
    let joint = if matches!(h.channel_mode, evo_sbc::ChannelMode::JointStereo) {
        h.subbands as usize
    } else {
        0
    };
    let sf = 4 * h.subbands as usize * h.channels();
    f[3] = crc(&f, joint + sf);
    f
}

fn real_frames() -> Vec<u8> {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let raw = std::fs::read(dir.join("real_a2dp_48k_joint_bp53_96frames.sbcv"))
        .unwrap();
    let n = u32::from_le_bytes(raw[6..10].try_into().unwrap()) as usize;
    raw[14..14 + n].to_vec()
}

#[test]
fn random_bytes_never_panic() {
    let mut rng = Rng(0x5BC0_DEC0_DE5B_C0DE);
    let mut buf = [0i16; 256];
    let mut decoder = Decoder::new();
    let (mut ok, mut rejected) = (0u32, 0u32);
    for i in 0..60_000 {
        let len = (rng.next() % 300) as usize;
        let mut data: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        // Most of the time make it look like a frame so the later checks run.
        if i % 4 != 0 && len > 3 {
            data[0] = 0x9C;
            data[2] = 2 + (rng.byte() % 62);
        }
        match decoder.decode(&data, &mut buf) {
            Ok(_) => ok += 1,
            Err(_) => rejected += 1,
        }
    }
    assert!(
        rejected > 50_000,
        "almost all noise is rejected: {rejected}"
    );
    // A CRC is 8 bits: some noise legitimately passes. The point is only that
    // it was handled.
    let _ = ok;
}

#[test]
fn every_prefix_of_a_frame_is_too_short_never_a_panic() {
    let stream = real_frames();
    let h = FrameHeader::parse(&stream).unwrap();
    let frame = &stream[..h.frame_len()];
    let mut buf = [0i16; 256];
    let mut d = Decoder::new();
    for n in 0..frame.len() {
        match d.decode(&frame[..n], &mut buf) {
            Err(Error::TooShort { needed, got }) => {
                assert_eq!(got, n);
                assert!(needed > n);
            }
            other => panic!("prefix of {n} bytes: {other:?}"),
        }
    }
    assert!(d.decode(frame, &mut buf).is_ok());
}

#[test]
fn every_single_bit_flip_is_a_result() {
    // Flip each bit of a real frame in turn. A flip is either caught (CRC,
    // syncword, bitpool) or, when it lands in the audio part the CRC does not
    // cover, decodes to something. It is never a panic.
    let stream = real_frames();
    let h = FrameHeader::parse(&stream).unwrap();
    let frame = stream[..h.frame_len()].to_vec();
    let mut buf = [0i16; 256];
    let (mut caught, mut decoded) = (0, 0);
    for bit in 0..frame.len() * 8 {
        let mut f = frame.clone();
        f[bit / 8] ^= 1 << (7 - bit % 8);
        match Decoder::new().decode(&f, &mut buf) {
            Ok(_) => decoded += 1,
            Err(_) => caught += 1,
        }
    }
    assert!(
        caught > 0 && decoded > 0,
        "caught {caught}, decoded {decoded}"
    );
}

#[test]
fn a_rejected_frame_leaves_the_decoder_exactly_as_it_was() {
    let stream = real_frames();
    let h = FrameHeader::parse(&stream).unwrap();
    let len = h.frame_len();
    let mut buf = [0i16; 256];
    let mut d = Decoder::new();
    for k in 0..10 {
        d.decode(&stream[k * len..], &mut buf).unwrap();
    }
    let before = d.clone();
    // Damage a frame in each way a frame can be rejected.
    let mut bad_crc = stream[10 * len..11 * len].to_vec();
    bad_crc[3] ^= 0x55;
    let mut bad_sync = bad_crc.clone();
    bad_sync[0] = 0x00;
    let short = &stream[10 * len..11 * len - 1];
    for damaged in [&bad_crc[..], &bad_sync[..], short] {
        assert!(d.decode(damaged, &mut buf).is_err());
        let mut a = before.clone();
        let mut b = d.clone();
        let (mut oa, mut ob) = ([0i16; 256], [0i16; 256]);
        a.decode(&stream[10 * len..], &mut oa).unwrap();
        b.decode(&stream[10 * len..], &mut ob).unwrap();
        assert_eq!(oa, ob, "the decoder moved on a rejected frame");
    }
}

#[test]
fn a_buffer_that_is_too_small_is_refused_before_anything_changes() {
    let stream = real_frames();
    let mut d = Decoder::new();
    let mut small = [0i16; 255];
    assert_eq!(
        d.decode(&stream, &mut small),
        Err(Error::OutputTooSmall {
            needed: 256,
            got: 255
        })
    );
}

#[test]
fn extreme_but_well_formed_frames_neither_panic_nor_overflow() {
    // Correct CRC, every payload bit set / clear / alternating, scale factors
    // at their maximum, every layout and the largest bitpools. Debug builds
    // check arithmetic overflow, so this also checks the sizes of the
    // intermediates.
    let mut buf = [0i16; 256];
    let mut count = 0;
    for fill in [0x00u8, 0xFF, 0xAA, 0x55, 0xF0, 0x0F] {
        for mode in 0..4u8 {
            for sub in 0..2u8 {
                for blocks in 0..4u8 {
                    for alloc in 0..2u8 {
                        for freq in 0..4u8 {
                            let max = {
                                let per = 16 * if sub == 1 { 8 } else { 4 };
                                if mode >= 2 {
                                    (2 * per).min(250)
                                } else {
                                    per
                                }
                            };
                            for bitpool in [2u8, 3, 17, 53, max as u8] {
                                let params = (freq << 6)
                                    | (blocks << 4)
                                    | (mode << 2)
                                    | (alloc << 1)
                                    | sub;
                                let f = frame_with(params, bitpool, fill);
                                let mut d = Decoder::new();
                                // run several frames so the filter history fills
                                for _ in 0..12 {
                                    let _ = d.decode(&f, &mut buf);
                                }
                                count += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(count > 5000, "{count}");
}

#[test]
fn a_stream_survives_a_damaged_frame_in_the_middle() {
    // The decoder is a pure function of the frames it accepts: dropping a
    // damaged frame is the same as the frame never having arrived.
    let stream = real_frames();
    let h = FrameHeader::parse(&stream).unwrap();
    let len = h.frame_len();
    let mut with_damage = Decoder::new();
    let mut without = Decoder::new();
    let (mut a, mut b) = ([0i16; 256], [0i16; 256]);
    for k in 0..30 {
        let good = &stream[k * len..];
        if k == 12 {
            let mut bad = good[..len].to_vec();
            bad[5] ^= 0xFF; // inside the CRC-covered scale factors
            assert!(with_damage.decode(&bad, &mut a).is_err());
            continue;
        }
        with_damage.decode(good, &mut a).unwrap();
        without.decode(good, &mut b).unwrap();
        assert_eq!(a, b, "frame {k}");
    }
}
