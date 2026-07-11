// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Size-variant transcoding for resolved artwork.
//!
//! Reads bytes from a resolved cover file (sidecar or extracted
//! embedded image), computes a deterministic content hash, and
//! optionally produces a resized WebP variant for sub-original
//! sizes.
//!
//! ## Size taxonomy
//!
//! Four sizes — three resized plus one passthrough — chosen to
//! match the operator-UI's actual rendering targets at 2x
//! retina density (the prevailing browser DPR for the target
//! distribution):
//!
//! - [`ArtworkSize::Tiny`] → 128 px square (sidebar thumbnails
//!   at 60-64 px logical × 2x)
//! - [`ArtworkSize::Medium`] → 300 px square (queue rows and
//!   library tiles at 150 px logical × 2x)
//! - [`ArtworkSize::Large`] → 600 px square (now-playing card
//!   at 300 px logical × 2x)
//! - [`ArtworkSize::Original`] → no resize; original bytes are
//!   returned verbatim for the album-art view, multi-room
//!   propagation, and archival.
//!
//! Three resized sizes plus the original cover every UI render
//! surface without thumbnail-proliferation overhead.
//!
//! ## Resize algorithm
//!
//! Lanczos3 — the industry-standard downsampling filter used
//! by ImageMagick, GraphicsMagick, Sharp, Pillow, and every
//! mature thumbnail pipeline. Superior to bilinear / bicubic
//! for photographic content; perceptually indistinguishable
//! from Mitchell for typical album-art content.
//!
//! ## Encoder posture
//!
//! Lossy WebP at quality 82 for tiny / medium / large. The
//! quality factor matches Roon's published thumbnail default:
//! perceptually identical to quality 95 at thumbnail
//! resolutions, 40-60% smaller bytes than equivalent JPEG.
//! Original-size cover art is served verbatim — no transcode,
//! so the master fidelity is preserved.
//!
//! ## Aspect ratio
//!
//! Resized variants preserve aspect ratio. The size taxonomy
//! is interpreted as a **bounding box** — the output's longer
//! edge equals the target px, the shorter edge scales
//! proportionally. The operator UI's rendering layer (CSS
//! `object-fit: cover`) handles the visual centering against
//! a square slot; this preserves photographic content rather
//! than distorting it via forced square crop.

use image::imageops::FilterType;
use sha2::{Digest, Sha256};

/// Quality factor for the lossy WebP encoder. The Roon-class
/// default — perceptually identical to quality 95 at thumbnail
/// resolutions; 40-60% bytes smaller than equivalent JPEG. Set
/// here as a module constant so a documented quality change is
/// a one-line edit reviewers can audit.
const WEBP_QUALITY_FACTOR: f32 = 82.0;

/// Size variant the operator UI requests via the
/// `?size=` query parameter on `/api/v1/audio/artwork`.
///
/// See module docs for the pixel-target rationale per variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkSize {
    /// 128 px bounding box — sidebar thumbnails.
    Tiny,
    /// 300 px bounding box — queue rows / library tiles.
    Medium,
    /// 600 px bounding box — now-playing card.
    Large,
    /// No resize; original bytes returned verbatim.
    Original,
}

impl ArtworkSize {
    /// Parse the wire-shape `size` string. Returns `None` on
    /// unrecognised input so callers can surface a structured
    /// `bad_request` rather than silently coercing.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tiny" => Some(Self::Tiny),
            "medium" => Some(Self::Medium),
            "large" => Some(Self::Large),
            "original" => Some(Self::Original),
            _ => None,
        }
    }

    /// Wire-shape string for the variant. Round-trips with
    /// `parse`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Medium => "medium",
            Self::Large => "large",
            Self::Original => "original",
        }
    }

    /// Bounding-box pixel dimension for the variant. `None`
    /// for `Original` (no resize).
    fn pixel_target(self) -> Option<u32> {
        match self {
            Self::Tiny => Some(128),
            Self::Medium => Some(300),
            Self::Large => Some(600),
            Self::Original => None,
        }
    }
}

/// Result of transcoding a resolved cover into a size variant.
///
/// Carries the bytes to push to the framework's asset cache,
/// the SHA-256 content hash (the asset cache key + the wire-
/// shape `content_hash` field on the resolve response), and
/// the MIME type the HTTPS layer serves with.
#[derive(Debug, Clone)]
pub struct TranscodedArtwork {
    /// Final bytes for the requested size variant.
    pub bytes: Vec<u8>,
    /// SHA-256 of `bytes`, hex-encoded lowercase (64 chars).
    /// Matches the framework's existing
    /// `/api/v1/audio/artwork/:content_hash` endpoint's address
    /// shape.
    pub content_hash: String,
    /// MIME type for the bytes. `image/webp` for resized
    /// variants; matches the source MIME for `Original`.
    pub mime: String,
}

/// Errors the transcode pipeline surfaces. All map to plugin
/// `permanent` responses — a malformed image bytes / encoder
/// failure is structurally invalid input rather than a
/// transient condition.
#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    /// The original bytes could not be decoded as an image.
    /// Operator-side condition: the sidecar file is corrupt
    /// or carries an unsupported format.
    #[error("image decode failed: {0}")]
    Decode(String),
    /// The encoder failed to produce WebP bytes from the
    /// resized image. Should not happen in practice with the
    /// `image` crate's WebP encoder; surfaces if it does.
    #[error("webp encode failed: {0}")]
    Encode(String),
}

/// Pipeline entrypoint.
///
/// For `Original`: returns the input bytes verbatim, computes
/// the SHA-256 of them, and reuses the source MIME.
///
/// For `Tiny | Medium | Large`: decodes the input, resizes via
/// Lanczos3 to fit within the variant's bounding box (aspect
/// preserved), encodes as WebP at quality 82, computes the
/// SHA-256 of the encoded bytes, returns `image/webp` as the
/// MIME.
pub fn transcode(
    bytes: Vec<u8>,
    source_mime: &str,
    size: ArtworkSize,
) -> Result<TranscodedArtwork, TranscodeError> {
    if size == ArtworkSize::Original {
        let content_hash = hash_bytes(&bytes);
        return Ok(TranscodedArtwork {
            bytes,
            content_hash,
            mime: source_mime.to_string(),
        });
    }
    let bounding = size
        .pixel_target()
        .expect("non-Original size carries a pixel target");
    // Decode the source image. lofty stripped EXIF metadata at
    // the extraction step (embedded path); sidecar files carry
    // metadata the image crate ignores at resize time.
    let dyn_image = image::load_from_memory(&bytes)
        .map_err(|e| TranscodeError::Decode(e.to_string()))?;
    // Resize to fit within the bounding box, preserving aspect
    // ratio. `resize` returns the resized image; its filter
    // argument is Lanczos3 per the module-doc rationale.
    let resized =
        if dyn_image.width() <= bounding && dyn_image.height() <= bounding {
            // Source is already smaller than the target — no
            // upscale; the original master is what the operator
            // gets. Preserves perceived quality (upscaling adds
            // softness without adding information).
            dyn_image
        } else {
            dyn_image.resize(bounding, bounding, FilterType::Lanczos3)
        };
    // Encode as lossy WebP at quality 82 via libwebp bindings.
    // Quality 82 matches the Roon-class default — perceptually
    // identical to quality 95 at thumbnail resolutions; 40-60%
    // smaller bytes than equivalent JPEG; smaller still than
    // lossless WebP at the same visual quality for
    // photographic content (typical album art).
    //
    // The image crate's `image-webp` backend (v0.2) only
    // exposes a lossless encoder, which paradoxically inflates
    // thumbnail bytes versus a JPEG of equivalent perceived
    // quality. We use the `webp` crate's libwebp bindings to
    // deliver the lossy encoding the catalogue acceptance row
    // pins against.
    let rgba = resized.to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    let encoder = webp::Encoder::from_rgba(rgba.as_raw(), width, height);
    let memory = encoder.encode(WEBP_QUALITY_FACTOR);
    let encoded = memory.to_vec();
    let content_hash = hash_bytes(&encoded);
    Ok(TranscodedArtwork {
        bytes: encoded,
        content_hash,
        mime: "image/webp".to_string(),
    })
}

/// SHA-256 of `bytes`, hex-encoded lowercase. The framework's
/// `/api/v1/audio/artwork/:content_hash` endpoint accepts
/// 64-lowercase-hex paths; this output matches that shape
/// without truncation.
fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_test_jpeg(width: u32, height: u32) -> Vec<u8> {
        // Tiny synthetic image — solid grey, just to exercise
        // the decode + resize + encode round-trip. The
        // `image` crate's PNG encoder is simpler than JPEG
        // for test fixtures.
        let mut img = image::ImageBuffer::from_pixel(
            width,
            height,
            image::Rgba([128u8, 128, 128, 255]),
        );
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let _ = &mut img; // silence the "unused mut" lint
        bytes
    }

    #[test]
    fn size_parse_round_trip() {
        for s in ["tiny", "medium", "large", "original"] {
            let parsed = ArtworkSize::parse(s).unwrap();
            assert_eq!(parsed.as_str(), s);
        }
        assert!(ArtworkSize::parse("xl").is_none());
        assert!(ArtworkSize::parse("").is_none());
        assert!(ArtworkSize::parse("TINY").is_none()); // case-sensitive
    }

    #[test]
    fn original_passes_bytes_through_unchanged() {
        let png = make_test_jpeg(64, 64);
        let result =
            transcode(png.clone(), "image/png", ArtworkSize::Original).unwrap();
        assert_eq!(result.bytes, png);
        assert_eq!(result.mime, "image/png");
        assert_eq!(result.content_hash.len(), 64);
        assert!(result.content_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tiny_resizes_to_bounding_box_and_transcodes_to_webp() {
        let png = make_test_jpeg(1024, 1024);
        let result = transcode(png, "image/png", ArtworkSize::Tiny).unwrap();
        assert_eq!(result.mime, "image/webp");
        // Decode the output to verify the resize happened
        let decoded = image::load_from_memory(&result.bytes).unwrap();
        assert!(decoded.width() <= 128, "got {}", decoded.width());
        assert!(decoded.height() <= 128, "got {}", decoded.height());
    }

    #[test]
    fn small_source_not_upscaled_for_large_size() {
        // Source 200x200, requested Large (600px bounding box).
        // Should pass through at 200x200 — no upscale.
        let png = make_test_jpeg(200, 200);
        let result = transcode(png, "image/png", ArtworkSize::Large).unwrap();
        let decoded = image::load_from_memory(&result.bytes).unwrap();
        assert_eq!(decoded.width(), 200);
        assert_eq!(decoded.height(), 200);
    }

    #[test]
    fn aspect_ratio_preserved_on_resize() {
        // 800x400 source resized for Medium (300px) — output
        // should be 300x150 (width-bounded, height
        // proportional).
        let png = make_test_jpeg(800, 400);
        let result = transcode(png, "image/png", ArtworkSize::Medium).unwrap();
        let decoded = image::load_from_memory(&result.bytes).unwrap();
        assert_eq!(decoded.width(), 300);
        assert_eq!(decoded.height(), 150);
    }

    #[test]
    fn content_hash_is_deterministic_for_same_input() {
        let png = make_test_jpeg(64, 64);
        let a =
            transcode(png.clone(), "image/png", ArtworkSize::Original).unwrap();
        let b = transcode(png, "image/png", ArtworkSize::Original).unwrap();
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn different_sizes_produce_distinct_content_hashes() {
        let png = make_test_jpeg(1024, 1024);
        let original =
            transcode(png.clone(), "image/png", ArtworkSize::Original).unwrap();
        let tiny =
            transcode(png.clone(), "image/png", ArtworkSize::Tiny).unwrap();
        let medium = transcode(png, "image/png", ArtworkSize::Medium).unwrap();
        // All three variants have distinct content hashes —
        // the multi-room receiver fetching by hash gets the
        // exact variant the leader produced for that size.
        assert_ne!(original.content_hash, tiny.content_hash);
        assert_ne!(tiny.content_hash, medium.content_hash);
        assert_ne!(original.content_hash, medium.content_hash);
    }

    #[test]
    fn malformed_bytes_surface_as_decode_error() {
        let result =
            transcode(b"not an image".to_vec(), "image/png", ArtworkSize::Tiny);
        assert!(matches!(result, Err(TranscodeError::Decode(_))));
    }

    /// Construct a small JPEG fixture and inject an EXIF APP1
    /// marker after the SOI so the input genuinely carries
    /// metadata for the strip-on-transcode test to bite on.
    /// The minimal EXIF payload is just a "Exif\0\0" tag —
    /// enough to satisfy a metadata-walker looking for the
    /// signature, even if the TIFF body is degenerate.
    fn make_jpeg_with_exif() -> Vec<u8> {
        // Encode a small JPEG via the image crate.
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_pixel(
                128,
                128,
                image::Rgb([100u8, 150, 200]),
            );
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "encoded JPEG missing SOI");

        // Inject an EXIF APP1 marker right after the SOI.
        // APP1 frame: FF E1 [length bytes 2] "Exif\0\0" [data].
        // The length value is big-endian and INCLUDES the length
        // bytes themselves but NOT the marker bytes.
        let exif_signature: &[u8] = b"Exif\0\0";
        let exif_tiff_stub: &[u8] = &[
            b'I', b'I', // little-endian TIFF byte order
            0x2A, 0x00, // TIFF magic 42
            0x08, 0x00, 0x00, 0x00, // IFD0 offset
            0x00, 0x00, // entry count = 0 (degenerate IFD)
        ];
        let payload_len = exif_signature.len() + exif_tiff_stub.len();
        let frame_len = (payload_len + 2) as u16; // includes the 2 length bytes
        let mut with_exif = Vec::new();
        with_exif.extend_from_slice(&jpeg[..2]); // SOI
        with_exif.push(0xFF);
        with_exif.push(0xE1); // APP1 marker
        with_exif.extend_from_slice(&frame_len.to_be_bytes());
        with_exif.extend_from_slice(exif_signature);
        with_exif.extend_from_slice(exif_tiff_stub);
        with_exif.extend_from_slice(&jpeg[2..]); // rest of JPEG
        with_exif
    }

    /// Pin the wire-shape contract: resized WebP variants MUST
    /// NOT embed EXIF / XMP / ICC profile metadata. The
    /// `webp::Encoder::from_rgba` path takes pure pixel bytes
    /// and has no metadata source — so the invariant holds by
    /// construction today, but a future encoder swap that adds
    /// metadata pass-through would silently violate the
    /// catalogue's `EXIF stripped on resized variants` claim
    /// without this test catching it.
    ///
    /// Asserts: the output WebP container carries no RIFF
    /// chunks with the known metadata chunk IDs (`EXIF`,
    /// `XMP `, `ICCP`).
    #[test]
    fn resized_variants_emit_no_webp_metadata_chunks() {
        let jpeg_with_exif = make_jpeg_with_exif();
        // Confirm our fixture genuinely carries an EXIF marker.
        assert!(
            jpeg_with_exif.windows(4).any(|w| w == b"Exif"),
            "fixture JPEG should carry an EXIF marker"
        );

        // WebP RIFF chunk IDs that the spec uses for metadata.
        let metadata_chunk_ids: &[&[u8; 4]] = &[
            b"EXIF", // EXIF block
            b"XMP ", // XMP metadata (note trailing space)
            b"ICCP", // ICC color profile
        ];

        for size in [ArtworkSize::Tiny, ArtworkSize::Medium, ArtworkSize::Large]
        {
            let result = transcode(jpeg_with_exif.clone(), "image/jpeg", size)
                .expect("transcode succeeds");
            assert_eq!(result.mime, "image/webp");

            for chunk_id in metadata_chunk_ids {
                assert!(
                    !result.bytes.windows(4).any(|w| w == *chunk_id),
                    "size={:?}: transcoded WebP contains metadata chunk {:?}",
                    size,
                    std::str::from_utf8(*chunk_id).unwrap()
                );
            }
        }
    }

    /// Original-size passthrough does NOT strip metadata —
    /// archival callers + multi-room propagation receive the
    /// master bytes verbatim. The contract is "stripped on
    /// resized variants ONLY"; pin the asymmetry here so a
    /// future change to strip on Original (which would reduce
    /// archival fidelity) is caught.
    #[test]
    fn original_size_preserves_input_metadata() {
        let jpeg_with_exif = make_jpeg_with_exif();
        let result = transcode(
            jpeg_with_exif.clone(),
            "image/jpeg",
            ArtworkSize::Original,
        )
        .expect("transcode succeeds");
        assert_eq!(result.bytes, jpeg_with_exif);
        assert!(
            result.bytes.windows(4).any(|w| w == b"Exif"),
            "Original-size output must preserve input EXIF marker"
        );
    }
}
