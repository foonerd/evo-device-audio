// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! Minimal DMAP (DAAP-tagged) metadata walker.
//!
//! `SET_PARAMETER` with `Content-Type: application/x-dmap-tagged` carries
//! now-playing metadata as DMAP: a sequence of entries, each a 4-byte ASCII
//! tag code followed by a big-endian u32 payload length and that many bytes
//! of payload. The track fields sit inside an `mlit` (dmap.listingitem)
//! container whose payload is itself such a sequence. This module walks
//! exactly that shape — tag code plus length — and extracts the fields the
//! host can display; unknown tags are skipped silently and a truncated
//! entry ends the walk. It is not a general DAAP implementation.

/// Track metadata from one DMAP payload. Fields the payload did not carry
/// are `None` — a payload is a complete statement about the current track,
/// not a delta.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrackMetadata {
    /// `minm` (dmap.itemname)
    pub title: Option<String>,
    /// `asar` (daap.songartist)
    pub artist: Option<String>,
    /// `asal` (daap.songalbum)
    pub album: Option<String>,
}

/// The longest metadata string passed on, in bytes. A sender's title is a
/// display string, not a document.
const MAX_FIELD_BYTES: usize = 1024;

/// A lossy string, cut to [`MAX_FIELD_BYTES`] at a character boundary.
fn bounded(payload: &[u8]) -> String {
    let text = String::from_utf8_lossy(payload);
    let mut end = text.len().min(MAX_FIELD_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// Extract track metadata from a DMAP payload. Returns `None` when the body
/// contains no `mlit` container — without it there is nothing recognizable
/// as a track statement. Strings are converted lossily; malformed trailing
/// bytes never fail the parse, they just end it.
pub fn parse(body: &[u8]) -> Option<TrackMetadata> {
    let mut meta: Option<TrackMetadata> = None;
    walk(body, &mut |tag, payload| {
        if tag == *b"mlit" {
            let m = meta.get_or_insert_with(TrackMetadata::default);
            walk(payload, &mut |tag, payload| {
                let field = match &tag {
                    b"minm" => &mut m.title,
                    b"asar" => &mut m.artist,
                    b"asal" => &mut m.album,
                    _ => return,
                };
                *field = Some(bounded(payload));
            });
        }
    });
    meta
}

/// Walk one level of DMAP entries, calling `f(tag, payload)` for each. An
/// entry whose declared length runs past the end of the data (truncation —
/// or bytes that are not DMAP at all) ends the walk.
fn walk(mut data: &[u8], f: &mut impl FnMut([u8; 4], &[u8])) {
    while let [t0, t1, t2, t3, l0, l1, l2, l3, rest @ ..] = data {
        let tag = [*t0, *t1, *t2, *t3];
        let len = u32::from_be_bytes([*l0, *l1, *l2, *l3]) as usize;
        if len > rest.len() {
            return;
        }
        f(tag, &rest[..len]);
        data = &rest[len..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut e = Vec::with_capacity(8 + payload.len());
        e.extend_from_slice(tag);
        e.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        e.extend_from_slice(payload);
        e
    }

    fn mlit(children: &[Vec<u8>]) -> Vec<u8> {
        entry(b"mlit", &children.concat())
    }

    #[test]
    fn extracts_title_artist_album_from_mlit() {
        let body = mlit(&[
            entry(b"minm", "Song Title".as_bytes()),
            entry(b"asar", "The Artist".as_bytes()),
            entry(b"asal", "The Album".as_bytes()),
        ]);
        assert_eq!(
            parse(&body),
            Some(TrackMetadata {
                title: Some("Song Title".into()),
                artist: Some("The Artist".into()),
                album: Some("The Album".into()),
            })
        );
    }

    #[test]
    fn unknown_tags_are_skipped() {
        // Real payloads interleave many tags we don't want (song time,
        // persistent id, ...) — they must not derail the wanted ones.
        let body = mlit(&[
            entry(b"astm", &180_000u32.to_be_bytes()),
            entry(b"minm", b"Song"),
            entry(b"mper", &42u64.to_be_bytes()),
            entry(b"asal", b"Album"),
        ]);
        let meta = parse(&body).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.album.as_deref(), Some("Album"));
        assert_eq!(meta.artist, None);
    }

    #[test]
    fn missing_fields_stay_none() {
        // A title-only statement: artist/album are None, not carried over.
        let meta = parse(&mlit(&[entry(b"minm", b"Just A Title")])).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Just A Title"));
        assert_eq!(meta.artist, None);
        assert_eq!(meta.album, None);
    }

    #[test]
    fn no_mlit_container_yields_none() {
        // The wanted tags outside their container are not a track statement.
        assert_eq!(parse(&entry(b"minm", b"loose")), None);
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"not dmap at all"), None);
    }

    #[test]
    fn truncated_entries_end_the_walk_without_losing_earlier_fields() {
        // Truncated entry after a complete container: container survives.
        let mut body = mlit(&[entry(b"minm", b"Song")]);
        body.extend_from_slice(b"asar");
        body.extend_from_slice(&100u32.to_be_bytes()); // claims 100 bytes, has 0
        assert_eq!(parse(&body).unwrap().title.as_deref(), Some("Song"));

        // Truncated entry inside the container: earlier siblings survive.
        let mut inner = entry(b"minm", b"Song");
        inner.extend_from_slice(b"asal");
        inner.extend_from_slice(&100u32.to_be_bytes());
        let meta = parse(&entry(b"mlit", &inner)).unwrap();
        assert_eq!(meta.title.as_deref(), Some("Song"));
        assert_eq!(meta.album, None);
    }

    #[test]
    fn invalid_utf8_is_converted_lossily() {
        let meta = parse(&mlit(&[entry(b"minm", &[0xff, b'A'])])).unwrap();
        assert_eq!(meta.title.as_deref(), Some("\u{fffd}A"));
    }

    #[test]
    fn a_long_title_is_cut_at_a_character_boundary() {
        let long = "é".repeat(3_000); // two bytes each
        let body = mlit(&[entry(b"minm", long.as_bytes())]);
        let title = parse(&body).unwrap().title.unwrap();
        assert!(title.len() <= MAX_FIELD_BYTES);
        assert!(title.chars().all(|c| c == 'é'), "no split character");
        assert!(!title.is_empty());
    }
}
