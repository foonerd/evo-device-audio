// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! FairPlay `fp-setup` — a canned compatibility handshake.
//!
//! A stock Apple sender requires `POST /fp-setup` after pairing, before it
//! will send `SETUP`. This is not live crypto: the receiver replies with fixed
//! tables, which are interoperability data derived from Apple's AirPlay.
//!
//! The reply tables are in `apple_interop.rs`.
//!
//! Requests are `application/octet-stream` beginning with `FPLY`
//! (`46 50 4c 59`):
//! - byte 4 = version (3), byte 5 = type (1), byte 6 = sequence, byte 14 =
//!   mode (phase 1 only).
//! - Phase 1 (seq 1, 16-byte request): reply with the 142-byte table for the
//!   mode byte (0..=3).
//! - Phase 2 (seq 3, 164-byte request): reply with a 12-byte header followed
//!   by the request's last 20 bytes (32 bytes total).

use crate::apple_interop::{HEADER, REPLY0, REPLY1, REPLY2, REPLY3};

const MAGIC: &[u8; 4] = b"FPLY";
const VERSION_POS: usize = 4;
const TYPE_POS: usize = 5;
const SEQ_POS: usize = 6;
const MODE_POS: usize = 14;
const SETUP_TYPE: u8 = 1;
const SETUP1_SEQ: u8 = 1;
const SETUP2_SEQ: u8 = 3;
const SETUP2_SUFFIX_LEN: usize = 20;

/// Compute the `fp-setup` reply for a `FPLY` request, or `None` if the request
/// is malformed / unsupported.
pub fn fp_setup(request: &[u8]) -> Option<Vec<u8>> {
    if request.len() <= MODE_POS
        || &request[0..4] != MAGIC
        || request[VERSION_POS] != 3
        || request[TYPE_POS] != SETUP_TYPE
    {
        return None;
    }
    match request[SEQ_POS] {
        SETUP1_SEQ => {
            let table: &[u8; 142] = match request[MODE_POS] {
                0 => &REPLY0,
                1 => &REPLY1,
                2 => &REPLY2,
                3 => &REPLY3,
                _ => return None,
            };
            Some(table.to_vec())
        }
        SETUP2_SEQ => {
            if request.len() < SETUP2_SUFFIX_LEN {
                return None;
            }
            let mut reply = HEADER.to_vec();
            reply.extend_from_slice(
                &request[request.len() - SETUP2_SUFFIX_LEN..],
            );
            Some(reply)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase1(mode: u8) -> Vec<u8> {
        // 16-byte FPLY phase-1 request with the given mode byte.
        let mut req = vec![0u8; 16];
        req[0..4].copy_from_slice(MAGIC);
        req[VERSION_POS] = 3;
        req[TYPE_POS] = 1;
        req[SEQ_POS] = 1;
        req[MODE_POS] = mode;
        req
    }

    #[test]
    fn phase1_returns_table_for_each_mode() {
        assert_eq!(fp_setup(&phase1(0)).unwrap(), REPLY0.to_vec());
        assert_eq!(fp_setup(&phase1(1)).unwrap(), REPLY1.to_vec());
        assert_eq!(fp_setup(&phase1(2)).unwrap(), REPLY2.to_vec());
        assert_eq!(fp_setup(&phase1(3)).unwrap(), REPLY3.to_vec());
        assert_eq!(fp_setup(&phase1(0)).unwrap().len(), 142);
    }

    #[test]
    fn matches_captured_mac_request() {
        // The exact phase-1 request a real macOS sender sent (mode 1).
        let req = hex_literal(b"46504c590301010000000004020001bb");
        assert_eq!(req.len(), 16);
        assert_eq!(fp_setup(&req).unwrap(), REPLY1.to_vec());
    }

    #[test]
    fn phase2_echoes_last_20_bytes() {
        let mut req = vec![0u8; 164];
        req[0..4].copy_from_slice(MAGIC);
        req[VERSION_POS] = 3;
        req[TYPE_POS] = 1;
        req[SEQ_POS] = 3;
        for (i, b) in req.iter_mut().enumerate().skip(144) {
            *b = i as u8; // last 20 bytes are 144..164
        }
        let reply = fp_setup(&req).unwrap();
        assert_eq!(reply.len(), 32);
        assert_eq!(&reply[0..12], &HEADER);
        assert_eq!(&reply[12..32], &req[144..164]);
    }

    #[test]
    fn rejects_non_fply_and_short() {
        assert!(fp_setup(b"NOPE").is_none());
        assert!(fp_setup(&[]).is_none());
        assert!(fp_setup(&phase1(9)).is_none()); // unknown mode
    }

    fn hex_literal(h: &[u8]) -> Vec<u8> {
        // Parse an ASCII-hex byte string in tests.
        h.chunks(2)
            .map(|c| {
                u8::from_str_radix(std::str::from_utf8(c).unwrap(), 16).unwrap()
            })
            .collect()
    }
}
