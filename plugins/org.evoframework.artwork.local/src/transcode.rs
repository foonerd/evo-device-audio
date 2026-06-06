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

pub(crate) use evo_device_audio_shared::transcode::{
    transcode, ArtworkSize, TranscodeError,
};

// `TranscodedArtwork` is destructured directly by `resolve.rs`
// when it constructs the cache payload; re-export it under the
// same name so existing callers keep compiling.
pub(crate) use evo_device_audio_shared::transcode::TranscodedArtwork;
