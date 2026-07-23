// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Minimal ASF (Windows Media) picture + identity reader.
//!
//! `lofty` 0.22 does not implement ASF/WMA, so a `.wma` file
//! whose embedded artwork is stored under the standard
//! `WM/Picture` extended-content descriptor is invisible to the
//! generic embedded-art path. This module fills the gap with a
//! targeted parser that walks the ASF header object, finds the
//! Extended Content Description Object, and returns:
//!
//! - the first `WM/Picture` payload as `(mime, image bytes)`,
//! - the `WM/AlbumArtist` (fallback: Content Description Object
//!   "Author" field) and `WM/AlbumTitle` strings for identity.
//!
//! Scope is deliberately tight: this is not a full ASF parser.
//! We walk only the top-level Header Object and its immediate
//! children; anything inside the Header Extension Object or the
//! Metadata Library Object is out of scope for the piece 8
//! certification (Windows Media Player / foobar2000 write
//! WM/Picture to the Extended Content Description Object by
//! convention, which is what the operator's specimens exercise).
//!
//! ## References
//!
//! - Advanced Systems Format (ASF) Specification, Revision
//!   01.20.03, Microsoft Corporation, 2004.
//! - WM/Picture value layout: WMFSDK 11.0.

use std::path::Path;

/// GUID of the top-level Header Object (fixed, little-endian
/// byte order as written on disk).
const HEADER_OBJECT_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA,
    0x00, 0x62, 0xCE, 0x6C,
];
/// GUID of the Content Description Object (title / author /
/// copyright / description / rating).
const CONTENT_DESC_GUID: [u8; 16] = [
    0x33, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA,
    0x00, 0x62, 0xCE, 0x6C,
];
/// GUID of the Extended Content Description Object — where
/// `WM/Picture`, `WM/AlbumTitle`, `WM/AlbumArtist` live.
const EXT_CONTENT_DESC_GUID: [u8; 16] = [
    0x40, 0xA4, 0xD0, 0xD2, 0x07, 0xE3, 0xD2, 0x11, 0x97, 0xF0, 0x00, 0xA0,
    0xC9, 0x5E, 0xA8, 0x50,
];

/// Cap on the file bytes we're willing to read + parse. ASF
/// headers are typically well under 64 KB; anything above 4 MB
/// is unreasonable for a metadata pass and signals a malformed
/// or hostile file.
const MAX_HEADER_BYTES: usize = 4 * 1024 * 1024;

/// Extracted picture: `(mime, image bytes)`.
pub(crate) struct AsfPicture {
    pub(crate) mime: String,
    pub(crate) data: Vec<u8>,
}

/// Read the first `WM/Picture` from `path`, returning `None`
/// when the file is not ASF, the header is malformed, or no
/// picture descriptor is present.
pub(crate) fn read_embedded_picture(path: &Path) -> Option<AsfPicture> {
    let bytes = std::fs::read(path).ok()?;
    if !starts_with_asf_header(&bytes) {
        return None;
    }
    let cap = bytes.len().min(MAX_HEADER_BYTES);
    let sub_range = header_object_children(&bytes[..cap])?;
    let mut cursor = sub_range.0;
    while cursor + 24 <= sub_range.1 {
        let guid = &bytes[cursor..cursor + 16];
        let size = read_u64_le(&bytes[cursor + 16..cursor + 24]) as usize;
        if size < 24 || cursor + size > sub_range.1 {
            return None;
        }
        if guid == EXT_CONTENT_DESC_GUID {
            if let Some(p) = pick_first_picture_from_ext_desc(
                &bytes[cursor + 24..cursor + size],
            ) {
                return Some(p);
            }
        }
        cursor += size;
    }
    None
}

/// Read `(artist, album)` identity for the mpd-path resolve
/// cascade. Prefers `WM/AlbumArtist` (Extended Content
/// Description); falls back to the Content Description
/// Object's "Author" field. Album comes from `WM/AlbumTitle`.
pub(crate) fn read_identity(path: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(path).ok()?;
    if !starts_with_asf_header(&bytes) {
        return None;
    }
    let cap = bytes.len().min(MAX_HEADER_BYTES);
    let sub_range = header_object_children(&bytes[..cap])?;
    let mut author_from_cd: Option<String> = None;
    let mut album_artist: Option<String> = None;
    let mut album: Option<String> = None;
    let mut cursor = sub_range.0;
    while cursor + 24 <= sub_range.1 {
        let guid = &bytes[cursor..cursor + 16];
        let size = read_u64_le(&bytes[cursor + 16..cursor + 24]) as usize;
        if size < 24 || cursor + size > sub_range.1 {
            return None;
        }
        let body = &bytes[cursor + 24..cursor + size];
        if guid == CONTENT_DESC_GUID {
            author_from_cd = parse_content_description_author(body);
        } else if guid == EXT_CONTENT_DESC_GUID {
            let (aa, at) = pick_identity_from_ext_desc(body);
            if album_artist.is_none() {
                album_artist = aa;
            }
            if album.is_none() {
                album = at;
            }
        }
        cursor += size;
    }
    let artist = album_artist.or(author_from_cd);
    match (artist, album) {
        (Some(a), Some(b)) if !a.trim().is_empty() && !b.trim().is_empty() => {
            Some((a.trim().to_string(), b.trim().to_string()))
        }
        _ => None,
    }
}

fn starts_with_asf_header(bytes: &[u8]) -> bool {
    bytes.len() >= 16 && bytes[..16] == HEADER_OBJECT_GUID
}

/// Return the byte range of children immediately inside the top
/// Header Object: `(children_start, children_end)`. Returns
/// `None` when the header shape is malformed.
fn header_object_children(bytes: &[u8]) -> Option<(usize, usize)> {
    if bytes.len() < 30 {
        return None;
    }
    // Layout: GUID (16) + Size (8) + Num Objects (4) + Reserved1
    // (1) + Reserved2 (1) = 30 bytes header.
    let total = read_u64_le(&bytes[16..24]) as usize;
    if total < 30 || total > bytes.len() {
        return None;
    }
    Some((30, total))
}

/// Walk the Extended Content Description Object descriptor list
/// and return the first `WM/Picture` value decoded as a picture.
fn pick_first_picture_from_ext_desc(body: &[u8]) -> Option<AsfPicture> {
    let mut cursor = 0;
    if body.len() < 2 {
        return None;
    }
    let count = read_u16_le(&body[..2]) as usize;
    cursor += 2;
    for _ in 0..count {
        let d = read_next_descriptor(body, &mut cursor)?;
        if d.name == "WM/Picture" && d.data_type == 1 {
            if let Some(pic) = parse_wm_picture_value(&d.value) {
                return Some(pic);
            }
        }
    }
    None
}

/// Extract `(WM/AlbumArtist, WM/AlbumTitle)` in one pass.
fn pick_identity_from_ext_desc(
    body: &[u8],
) -> (Option<String>, Option<String>) {
    let mut cursor = 0;
    let mut album_artist: Option<String> = None;
    let mut album: Option<String> = None;
    if body.len() < 2 {
        return (None, None);
    }
    let count = read_u16_le(&body[..2]) as usize;
    cursor += 2;
    for _ in 0..count {
        let Some(d) = read_next_descriptor(body, &mut cursor) else {
            break;
        };
        if d.data_type != 0 {
            continue; // strings only for identity fields
        }
        let value_str = decode_utf16_le_null_terminated(&d.value);
        match d.name.as_str() {
            "WM/AlbumArtist" if album_artist.is_none() => {
                album_artist = Some(value_str.clone());
            }
            "WM/AlbumTitle" if album.is_none() => {
                album = Some(value_str.clone());
            }
            _ => {}
        }
        if album_artist.is_some() && album.is_some() {
            break;
        }
    }
    (album_artist, album)
}

struct Descriptor {
    name: String,
    data_type: u16,
    value: Vec<u8>,
}

/// Read one descriptor from the Extended Content Description
/// list and advance the cursor. Returns `None` on truncation.
fn read_next_descriptor(body: &[u8], cursor: &mut usize) -> Option<Descriptor> {
    if *cursor + 2 > body.len() {
        return None;
    }
    let name_len = read_u16_le(&body[*cursor..*cursor + 2]) as usize;
    *cursor += 2;
    if *cursor + name_len > body.len() {
        return None;
    }
    let name_bytes = &body[*cursor..*cursor + name_len];
    let name = decode_utf16_le_null_terminated(name_bytes);
    *cursor += name_len;
    if *cursor + 4 > body.len() {
        return None;
    }
    let data_type = read_u16_le(&body[*cursor..*cursor + 2]);
    *cursor += 2;
    let value_len = read_u16_le(&body[*cursor..*cursor + 2]) as usize;
    *cursor += 2;
    if *cursor + value_len > body.len() {
        return None;
    }
    let value = body[*cursor..*cursor + value_len].to_vec();
    *cursor += value_len;
    Some(Descriptor {
        name,
        data_type,
        value,
    })
}

/// Decode a WM/Picture value into an `AsfPicture`. Layout:
/// Picture Type (1B) + Data Length (4B LE) + MIME (WCHAR* NUL) +
/// Description (WCHAR* NUL) + Data (`data_length` bytes).
fn parse_wm_picture_value(value: &[u8]) -> Option<AsfPicture> {
    if value.len() < 5 {
        return None;
    }
    let mut cursor = 1; // skip picture type
    let data_length = read_u32_le(&value[cursor..cursor + 4]) as usize;
    cursor += 4;
    let (mime, mime_consumed) =
        read_utf16_le_null_terminated(&value[cursor..])?;
    cursor += mime_consumed;
    let (_desc, desc_consumed) =
        read_utf16_le_null_terminated(&value[cursor..])?;
    cursor += desc_consumed;
    if cursor + data_length > value.len() {
        return None;
    }
    let data = value[cursor..cursor + data_length].to_vec();
    if data.is_empty() {
        return None;
    }
    Some(AsfPicture { mime, data })
}

/// Parse the Content Description Object body and return the
/// "Author" string.
fn parse_content_description_author(body: &[u8]) -> Option<String> {
    // Layout: Title Len (2) + Author Len (2) + Copyright Len (2)
    // + Description Len (2) + Rating Len (2) + Title WCHAR* +
    // Author WCHAR* + ...
    if body.len() < 10 {
        return None;
    }
    let title_len = read_u16_le(&body[0..2]) as usize;
    let author_len = read_u16_le(&body[2..4]) as usize;
    let mut cursor = 10;
    cursor += title_len;
    if cursor + author_len > body.len() {
        return None;
    }
    let author_bytes = &body[cursor..cursor + author_len];
    let author = decode_utf16_le_null_terminated(author_bytes);
    if author.is_empty() {
        None
    } else {
        Some(author)
    }
}

/// Read a UTF-16 LE null-terminated string from `bytes`,
/// returning `(string, bytes_consumed)` where bytes_consumed
/// includes the null terminator. Returns `None` when no null
/// terminator is found.
fn read_utf16_le_null_terminated(bytes: &[u8]) -> Option<(String, usize)> {
    let mut i = 0;
    while i + 2 <= bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 {
            let s = decode_utf16_le_null_terminated(&bytes[..i + 2]);
            return Some((s, i + 2));
        }
        i += 2;
    }
    None
}

/// Decode a UTF-16 LE byte slice, stopping at the first NUL
/// code unit. Invalid surrogates are replaced with U+FFFD.
fn decode_utf16_le_null_terminated(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn read_u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn read_u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn read_u64_le(b: &[u8]) -> u64 {
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_asf_bytes_return_none_without_read() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("not-asf.bin");
        std::fs::write(&p, b"this is not an ASF file").unwrap();
        assert!(read_embedded_picture(&p).is_none());
        assert!(read_identity(&p).is_none());
    }

    #[test]
    fn utf16_null_terminated_decodes_ascii() {
        // 'H','i',NUL as UTF-16LE bytes
        let bytes = [0x48, 0x00, 0x69, 0x00, 0x00, 0x00];
        assert_eq!(decode_utf16_le_null_terminated(&bytes), "Hi");
    }
}
