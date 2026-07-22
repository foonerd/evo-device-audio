// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Embedded cover art via [`lofty`] (ID3, Vorbis, MP4, …).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::read_from_path;
use lofty::tag::{Accessor, Tag};

/// Picked from tags; `mime` is a best-effort `image/*` string.
pub(crate) struct EmbeddedImage {
    pub(crate) data: Vec<u8>,
    pub(crate) mime: String,
}

/// Return the first viable embedded front cover, else any other
/// non–back‑cover image, else any other image.
fn pick_picture(tag: &Tag) -> Option<EmbeddedImage> {
    if let Some(p) = tag.get_picture_type(PictureType::CoverFront) {
        return image_from_picture(p);
    }
    for p in tag.pictures() {
        if p.pic_type() == PictureType::CoverBack {
            continue;
        }
        if let Some(i) = image_from_picture(p) {
            return Some(i);
        }
    }
    None
}

fn image_from_picture(p: &lofty::picture::Picture) -> Option<EmbeddedImage> {
    let data = p.data();
    if data.is_empty() {
        return None;
    }
    let mut mime: String = p
        .mime_type()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    if !mime.starts_with("image/") {
        mime = sniff_image_mime(data);
    }
    if !mime.starts_with("image/") {
        return None;
    }
    Some(EmbeddedImage {
        data: data.to_vec(),
        mime,
    })
}

/// JPEG / PNG / GIF / WebP magic bytes.
fn sniff_image_mime(data: &[u8]) -> String {
    if data.len() >= 3 && &data[..3] == b"\xFF\xD8\xFF" {
        return "image/jpeg".to_string();
    }
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        return "image/png".to_string();
    }
    if data.len() >= 6
        && (data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a"))
    {
        return "image/gif".to_string();
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return "image/webp".to_string();
    }
    "application/octet-stream".to_string()
}

/// Read the best embedded image from a supported audio `path`.
pub(crate) fn read_embedded_cover(path: &Path) -> Option<EmbeddedImage> {
    let tagged = read_from_path(path).ok()?;
    if let Some(t) = tagged.primary_tag() {
        if let Some(i) = pick_picture(t) {
            return Some(i);
        }
    }
    for t in tagged.tags() {
        if let Some(i) = pick_picture(t) {
            return Some(i);
        }
    }
    None
}

/// Read the `(artist, album)` identity from a supported audio
/// `path`. Returns `Some((artist, album))` when BOTH fields are
/// present and non-empty after trimming; returns `None` when the
/// file has no tags, lofty cannot parse the format, or either
/// field is absent.
///
/// Used by the mpd-path resolve path in `resolve_cover_for_audio_file`
/// on the local-NotFound branch: the identity rides along on the
/// response so the framework endpoint can synthesise an mpd-album
/// target for the online tier. A now-playing surface with no
/// embedded / sidecar art still cascades to online instead of
/// dead-ending at 404.
///
/// Both artist and album MUST be present — the online cascade is
/// keyed on the compound identity. A file with an album tag but no
/// artist (or vice versa) returns `None` here so the endpoint
/// short-circuits online rather than fabricating a half-identity
/// lookup that could match unrelated releases at the CAA / iTunes
/// tier.
pub(crate) fn read_identity(path: &Path) -> Option<(String, String)> {
    let tagged = read_from_path(path).ok()?;
    // Consult the primary tag first (the format's canonical tag —
    // ID3v2 on MP3, Vorbis comments on FLAC/Ogg, iTunes on MP4);
    // fall through to any secondary tag if the primary is missing
    // one field. This mirrors `read_embedded_cover`'s fall-through
    // discipline so cover + identity resolve consistently for the
    // same file.
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;
    if let Some(t) = tagged.primary_tag() {
        artist = t
            .artist()
            .map(|c| c.trim().to_string())
            .filter(|s| !s.is_empty());
        album = t
            .album()
            .map(|c| c.trim().to_string())
            .filter(|s| !s.is_empty());
    }
    if artist.is_none() || album.is_none() {
        for t in tagged.tags() {
            if artist.is_none() {
                artist = t
                    .artist()
                    .map(|c| c.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
            if album.is_none() {
                album = t
                    .album()
                    .map(|c| c.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
            if artist.is_some() && album.is_some() {
                break;
            }
        }
    }
    match (artist, album) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    }
}

fn extension_for_mime(mime: &str) -> &str {
    if mime == "image/png" {
        return "png";
    }
    if mime == "image/webp" {
        return "webp";
    }
    if mime == "image/gif" {
        return "gif";
    }
    "jpg"
}

/// Stable path under `state_dir/artwork_cache/` for this `track` file.
fn cache_basename_for_track(track: &Path) -> String {
    let mut h = DefaultHasher::new();
    track.hash(&mut h);
    let digest = h.finish();
    let name = track
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("track");
    // Avoid collisions: hash + shortened file stem.
    let short: String = if name.len() > 32 {
        name.chars().take(32).collect()
    } else {
        name.to_string()
    };
    format!("{digest:016x}_{short}", digest = digest, short = short)
}

/// Write `image` to the plugin cache and return the absolute file path.
pub(crate) fn write_embedded_to_cache(
    state_dir: &Path,
    track: &Path,
    image: &EmbeddedImage,
) -> Result<PathBuf, String> {
    if !image.mime.starts_with("image/") {
        return Err("embedded payload is not an image/* type".to_string());
    }
    let dir = state_dir.join("artwork_cache");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("create artwork_cache: {e}"))?;
    let ext = extension_for_mime(&image.mime);
    let out = dir.join(format!("{}.{}", cache_basename_for_track(track), ext));
    std::fs::write(&out, &image.data)
        .map_err(|e| format!("write embedded cache: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_jpeg() {
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0, 0]), "image/jpeg");
    }

    #[test]
    fn read_embedded_from_plain_text_fails() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.flac");
        std::fs::write(&p, b"not an audio file").unwrap();
        assert!(read_embedded_cover(&p).is_none());
    }
}
