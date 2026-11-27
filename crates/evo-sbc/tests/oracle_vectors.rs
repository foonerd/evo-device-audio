// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! The decoder against the reference decoder, sample for sample.
//!
//! Every file in `tests/vectors` is a run of SBC frames and the PCM the
//! reference decoder made of them (see `tests/vectors/generate.py`). The
//! decoder must reproduce it **exactly**: not within a tolerance, exactly.

use evo_sbc::{Decoder, FrameHeader};
use std::path::PathBuf;

struct Vector {
    name: String,
    frames: Vec<u8>,
    pcm: Vec<i16>,
    count: usize,
}

fn vectors() -> Vec<Vector> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("vectors directory") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("sbcv") {
            continue;
        }
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..4], b"SBCV", "{path:?}");
        let count = usize::from(u16::from_le_bytes([raw[4], raw[5]]));
        let frames_len =
            u32::from_le_bytes(raw[6..10].try_into().unwrap()) as usize;
        let pcm_len =
            u32::from_le_bytes(raw[10..14].try_into().unwrap()) as usize;
        assert_eq!(raw.len(), 14 + frames_len + pcm_len, "{path:?}");
        let frames = raw[14..14 + frames_len].to_vec();
        let pcm = raw[14 + frames_len..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        out.push(Vector {
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            frames,
            pcm,
            count,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Decode a whole run of frames, returning the samples and the frame count.
fn decode_run(frames: &[u8]) -> (Vec<i16>, usize) {
    let mut decoder = Decoder::new();
    let mut out = Vec::new();
    let mut buf = [0i16; 256];
    let (mut at, mut n) = (0, 0);
    while at < frames.len() {
        let done = decoder
            .decode(&frames[at..], &mut buf)
            .unwrap_or_else(|e| panic!("frame {n} at byte {at}: {e}"));
        out.extend_from_slice(&buf[..done.samples_per_channel * done.channels]);
        at += done.consumed;
        n += 1;
    }
    (out, n)
}

#[test]
fn the_corpus_is_what_it_claims_to_be() {
    let all = vectors();
    // 95 frames-from-an-encoder vectors, 64 crafted-frame vectors, 1 real capture.
    assert_eq!(all.len(), 160, "the corpus changed");
    assert_eq!(
        all.iter()
            .filter(|v| v.name.starts_with("crafted_"))
            .count(),
        64
    );
    let samples: usize = all.iter().map(|v| v.pcm.len()).sum();
    assert!(samples > 110_000, "{samples} samples");
}

#[test]
fn every_vector_decodes_bit_for_bit() {
    let mut failures = Vec::new();
    for v in vectors() {
        let (out, frames) = decode_run(&v.frames);
        if frames != v.count || out.len() != v.pcm.len() {
            failures.push(format!(
                "{}: {frames} frames / {} samples, expected {} / {}",
                v.name,
                out.len(),
                v.count,
                v.pcm.len()
            ));
        } else if let Some(i) = out.iter().zip(&v.pcm).position(|(a, b)| a != b)
        {
            let bad = out.iter().zip(&v.pcm).filter(|(a, b)| a != b).count();
            failures.push(format!(
                "{}: {bad} samples differ, the first at {i}: got {}, reference {}",
                v.name, out[i], v.pcm[i]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "not bit-exact:\n{}",
        failures.join("\n")
    );
}

#[test]
fn every_layout_the_specification_allows_is_covered() {
    use std::collections::BTreeSet;
    let mut seen = BTreeSet::new();
    for v in vectors() {
        let h = FrameHeader::parse(&v.frames).unwrap();
        seen.insert((
            format!("{:?}", h.channel_mode),
            h.subbands,
            h.blocks,
            format!("{:?}", h.allocation),
            h.frequency.hz(),
        ));
    }
    for mode in ["Mono", "DualChannel", "Stereo", "JointStereo"] {
        for sub in [4u8, 8] {
            for blocks in [4u8, 8, 12, 16] {
                for alloc in ["Loudness", "Snr"] {
                    assert!(
                        seen.contains(&(mode.to_string(), sub, blocks, alloc.to_string(), 44_100)),
                        "no vector for {mode} {sub} subbands {blocks} blocks {alloc}"
                    );
                }
            }
        }
    }
    for hz in [16_000u32, 32_000, 44_100, 48_000] {
        assert!(seen.iter().any(|s| s.4 == hz), "no vector at {hz} Hz");
    }
}

#[test]
fn decoding_the_same_stream_twice_gives_the_same_samples() {
    let v = vectors()
        .into_iter()
        .find(|v| v.name.starts_with("real_"))
        .unwrap();
    let (a, _) = decode_run(&v.frames);
    let (b, _) = decode_run(&v.frames);
    assert_eq!(a, b);
    // and a reset decoder is a fresh one
    let mut d = Decoder::new();
    let mut buf = [0i16; 256];
    let first = d.decode(&v.frames, &mut buf).unwrap();
    d.reset();
    let mut again = [0i16; 256];
    d.decode(&v.frames, &mut again).unwrap();
    assert_eq!(
        buf[..first.samples_per_channel * 2],
        again[..first.samples_per_channel * 2]
    );
}

#[test]
fn a_stream_changing_layout_midway_decodes_each_part_as_a_fresh_stream() {
    // A different channel count or subband count cannot reuse the filter
    // history: the decoder starts the new layout from silence.
    let all = vectors();
    let a = all
        .iter()
        .find(|v| v.name.starts_with("f44_mono_4sb_16blk_loudness"))
        .unwrap();
    let b = all
        .iter()
        .find(|v| v.name.starts_with("f44_joint_8sb_16blk_loudness"))
        .unwrap();
    let mut joined = a.frames.clone();
    joined.extend_from_slice(&b.frames);
    let (out, _) = decode_run(&joined);
    let mut expect = a.pcm.clone();
    expect.extend_from_slice(&b.pcm);
    assert_eq!(out, expect);
}
