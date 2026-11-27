// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A decoder for SBC, the mandatory audio codec of Bluetooth A2DP.
//!
//! Frames in, 16-bit PCM out. It is written from the specification, has no
//! dependencies, contains no `unsafe`, and takes untrusted input: a frame
//! that is malformed, truncated or damaged is an [`Error`], never a panic,
//! and never changes the decoder.
//!
//! The output is **bit-exact** with the reference fixed-point decoder (the
//! one in BlueZ's `libsbc`, which FFmpeg's decoder also matches) for every
//! channel mode, subband count, block count, allocation method, bitpool and
//! sampling frequency: the conformance vectors in `tests/vectors` were made
//! with that decoder and are compared sample for sample.
//!
//! ```
//! use evo_sbc::{Decoder, FrameHeader};
//!
//! /// Decode every frame of `stream`, handing each block of samples on.
//! fn decode_all(stream: &[u8], mut play: impl FnMut(&[i16])) {
//!     let mut decoder = Decoder::new();
//!     let mut pcm = [0i16; 256];
//!     let mut at = 0;
//!     while at < stream.len() {
//!         let Ok(header) = FrameHeader::parse(&stream[at..]) else { break };
//!         match decoder.decode(&stream[at..], &mut pcm) {
//!             Ok(done) => play(&pcm[..done.samples_per_channel * done.channels]),
//!             Err(_) => { /* a damaged frame: play silence for it */ }
//!         }
//!         at += header.frame_len();
//!     }
//! }
//! decode_all(&[], |_| {});
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod alloc;
mod bits;
mod decoder;
mod header;
mod tables;

pub use decoder::{Decoded, Decoder};
pub use header::{
    Allocation, ChannelMode, Error, FrameHeader, Frequency, HEADER_LEN,
    SYNCWORD,
};
