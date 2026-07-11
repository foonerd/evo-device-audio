// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Thin re-export of the shared transcode pipeline.
//!
//! The artwork transcode primitive (size taxonomy + Lanczos3
//! resize + lossy WebP quality 82 encode + SHA-256 content hash)
//! lives in [`evo_device_audio_shared::transcode`] so every
//! artwork provider plugin in the audio reference distribution
//! (local + online + future siblings) consumes one production-
//! validated primitive. Each plugin's request handler still
//! sits in its own crate, but the bytes-to-cache-payload
//! pipeline is shared.
//!
//! Only the items this crate actually names are re-exported:
//! the `transcode` function (called from resolve.rs) and the
//! `ArtworkSize` enum (constructed via its `parse` method and
//! used as a parameter type). `TranscodedArtwork` is consumed
//! via field access on the returned value rather than by name,
//! so it does not appear here; `TranscodeError` flows through
//! `?` into `String` and is not named directly either.

pub(crate) use evo_device_audio_shared::transcode::{transcode, ArtworkSize};
