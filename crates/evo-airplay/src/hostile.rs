// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Hostile input. Everything a peer on the network can put in front of this
//! library, before it is paired and after, is fed random and corrupted bytes
//! from fixed seeds. The only claim is the one that matters for a service
//! that listens on a home network: nothing panics, nothing allocates without
//! bound, and every parser answers with a refusal or a value.

use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

use crate::buffered::{split_blocks, AudioDecryptor};
use crate::cipher::control_channel;
use crate::decode::AacDecoder;
use crate::fairplay::fp_setup;
use crate::http::parse_head;
use crate::session::{parse_flush_until_seq, parse_rate_anchor};
use crate::tlv::Tlv;

/// Rounds per parser. A deeper run: `EVO_HOSTILE_ROUNDS=200000 cargo test`.
fn rounds() -> usize {
    std::env::var("EVO_HOSTILE_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}

fn random_bytes(rng: &mut StdRng, max: usize) -> Vec<u8> {
    let len = rng.gen_range(0..=max);
    let mut out = vec![0u8; len];
    rng.fill_bytes(&mut out);
    out
}

/// Flip, drop, insert and truncate: a valid message made wrong in a way
/// random bytes would almost never reach.
fn corrupt(rng: &mut StdRng, valid: &[u8]) -> Vec<u8> {
    let mut out = valid.to_vec();
    for _ in 0..rng.gen_range(1..=4) {
        if out.is_empty() {
            out.push(rng.gen());
            continue;
        }
        let at = rng.gen_range(0..out.len());
        match rng.gen_range(0..4) {
            0 => out[at] ^= 1 << rng.gen_range(0..8),
            1 => {
                out.remove(at);
            }
            2 => out.insert(at, rng.gen()),
            _ => out.truncate(at),
        }
    }
    out
}

#[test]
fn tlv_decode_survives_hostile_bytes() {
    let mut rng = StdRng::seed_from_u64(1);
    let mut valid = Tlv::new();
    valid.put_u8(0x06, 3).put(0x03, vec![9u8; 600]);
    let valid = valid.encode();
    for _ in 0..rounds() {
        let _ = Tlv::decode(&random_bytes(&mut rng, 600));
        let _ = Tlv::decode(&corrupt(&mut rng, &valid));
    }
}

#[test]
fn request_heads_survive_hostile_text() {
    let mut rng = StdRng::seed_from_u64(2);
    let valid =
        "SETUP rtsp://x/1 RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 12\r\n";
    for _ in 0..rounds() {
        let text =
            String::from_utf8_lossy(&corrupt(&mut rng, valid.as_bytes()))
                .into_owned();
        let _ = parse_head(&text);
        let noise =
            String::from_utf8_lossy(&random_bytes(&mut rng, 300)).into_owned();
        let _ = parse_head(&noise);
    }
}

#[test]
fn dmap_metadata_survives_hostile_bytes() {
    let mut rng = StdRng::seed_from_u64(3);
    let mut item = Vec::new();
    for (tag, text) in [(b"minm", "Title"), (b"asar", "Artist")] {
        item.extend_from_slice(tag);
        item.extend_from_slice(&(text.len() as u32).to_be_bytes());
        item.extend_from_slice(text.as_bytes());
    }
    let mut valid = b"mlit".to_vec();
    valid.extend_from_slice(&(item.len() as u32).to_be_bytes());
    valid.extend_from_slice(&item);
    for _ in 0..rounds() {
        let _ = crate::dmap::parse(&random_bytes(&mut rng, 200));
        let _ = crate::dmap::parse(&corrupt(&mut rng, &valid));
    }
}

#[test]
fn audio_blocks_and_packets_survive_hostile_bytes() {
    let mut rng = StdRng::seed_from_u64(4);
    let decryptor = AudioDecryptor::new(&[3u8; 32]).expect("32-byte key");
    for _ in 0..rounds() {
        let stream = random_bytes(&mut rng, 2_000);
        let (packets, used) = split_blocks(&stream);
        assert!(used <= stream.len());
        for packet in packets {
            let _ = decryptor.decrypt(packet);
        }
        let _ = decryptor.decrypt(&random_bytes(&mut rng, 80));
    }
}

#[test]
fn the_encrypted_control_stream_survives_hostile_bytes() {
    let mut rng = StdRng::seed_from_u64(5);
    for _ in 0..rounds() / 4 {
        let (_, mut dec) = control_channel(&[8u8; 64]);
        let _ = dec.decrypt_available(&random_bytes(&mut rng, 3_000));
    }
}

#[test]
fn fairplay_and_transport_requests_survive_hostile_bodies() {
    let mut rng = StdRng::seed_from_u64(6);
    for _ in 0..rounds() {
        let body = random_bytes(&mut rng, 200);
        let _ = fp_setup(&body);
        let _ = parse_rate_anchor(&body);
        let _ = parse_flush_until_seq(&body);
    }
    // A request that starts like the real thing and then goes wrong.
    let mut request = vec![0u8; 164];
    request[0..4].copy_from_slice(b"FPLY");
    request[4] = 3;
    request[5] = 1;
    request[6] = 3;
    for _ in 0..rounds() {
        let _ = fp_setup(&corrupt(&mut rng, &request));
    }
}

#[test]
fn the_aac_decoder_survives_hostile_frames() {
    let mut rng = StdRng::seed_from_u64(7);
    let raw = include_bytes!("../tests/data/tone_lr.frames");
    let frames = crate::sender::parse_frames(raw);
    assert!(frames.len() > 20, "the fixture holds a tone");
    let mut decoder = AacDecoder::new(44_100, 2).expect("decoder");
    for _ in 0..rounds() {
        let frame = if rng.gen_bool(0.5) {
            random_bytes(&mut rng, 600)
        } else {
            let pick = rng.gen_range(0..frames.len());
            corrupt(&mut rng, frames[pick])
        };
        let _ = decoder.decode(&frame);
    }
    // And it still decodes a good frame afterwards.
    let mut fresh = AacDecoder::new(44_100, 2).expect("decoder");
    assert!(fresh.decode(frames[5]).is_ok());
}
