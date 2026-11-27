// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! Reading the property lists a sender posts.
//!
//! Senders speak the binary property-list format. The plist library reads
//! whatever format a body turns out to be, XML included, and its XML reader
//! is a known source of unbounded allocation and quadratic parse time on
//! hostile input. A body that is not binary is therefore refused before the
//! library sees it: no code path here can reach the XML reader.
//!
//! Even a binary list is checked before it is parsed. The library builds and
//! drops nested values recursively, so a body a megabyte or two long that
//! nests arrays a few hundred thousand deep overflows the stack, which aborts
//! the whole process: no panic guard can stop it. Shared references can also
//! expand a small list into an enormous tree. The object graph is walked
//! iteratively first, and a list nested deeper or expanding larger than any
//! real request is refused.

use std::io;

use plist::Value;

/// The magic and version that open every binary property list.
const MAGIC: &[u8] = b"bplist00";

/// The deepest nesting accepted. Real requests nest three or four levels.
const MAX_DEPTH: usize = 12;
/// The most nodes accepted once shared references are expanded into a tree.
const MAX_NODES: usize = 20_000;
/// The trailer that closes a binary list.
const TRAILER: usize = 32;

/// Parse `body` as a binary property list. Anything else, XML included, and
/// anything nested or expanding past the limits, is `None`.
pub fn parse(body: &[u8]) -> Option<Value> {
    if !body.starts_with(MAGIC) || !shape_is_within_limits(body) {
        return None;
    }
    Value::from_reader(io::Cursor::new(body)).ok()
}

/// A big-endian unsigned integer of `width` bytes at `at`.
fn be_uint(data: &[u8], at: usize, width: usize) -> Option<usize> {
    if width == 0 || width > 8 {
        return None;
    }
    let bytes = data.get(at..at.checked_add(width)?)?;
    let mut value: u64 = 0;
    for b in bytes {
        value = (value << 8) | u64::from(*b);
    }
    usize::try_from(value).ok()
}

/// Walk the object graph from the top object, iteratively, and say whether it
/// stays within [`MAX_DEPTH`] and [`MAX_NODES`] once expanded as a tree. Every
/// read is bounds-checked: a malformed list is simply not within limits.
fn shape_is_within_limits(data: &[u8]) -> bool {
    walk(data).is_some()
}

fn walk(data: &[u8]) -> Option<()> {
    let trailer_at = data.len().checked_sub(TRAILER)?;
    if trailer_at < MAGIC.len() {
        return None;
    }
    let offset_width = usize::from(*data.get(trailer_at + 6)?);
    let ref_width = usize::from(*data.get(trailer_at + 7)?);
    let objects = be_uint(data, trailer_at + 8, 8)?;
    let top = be_uint(data, trailer_at + 16, 8)?;
    let table_at = be_uint(data, trailer_at + 24, 8)?;
    if objects == 0 || top >= objects || objects > data.len() {
        return None;
    }
    if !(1..=8).contains(&offset_width) || !(1..=8).contains(&ref_width) {
        return None;
    }
    let table_end = table_at.checked_add(objects.checked_mul(offset_width)?)?;
    if table_at < MAGIC.len() || table_end > trailer_at {
        return None;
    }
    let offset_of = |index: usize| -> Option<usize> {
        let at = table_at.checked_add(index.checked_mul(offset_width)?)?;
        let offset = be_uint(data, at, offset_width)?;
        (offset >= MAGIC.len() && offset < table_at).then_some(offset)
    };

    let mut stack: Vec<(usize, usize)> = vec![(top, 1)];
    let mut visited = 0usize;
    while let Some((index, depth)) = stack.pop() {
        visited += 1;
        if depth > MAX_DEPTH || visited > MAX_NODES {
            return None;
        }
        let at = offset_of(index)?;
        let marker = *data.get(at)?;
        let (kind, low) = (marker >> 4, usize::from(marker & 0x0f));
        if !matches!(kind, 0xA | 0xC | 0xD) {
            continue; // a scalar: nothing below it
        }
        // The element count: in the marker, or in an integer that follows.
        let (count, body_at) = if low == 0x0f {
            let int_marker = *data.get(at + 1)?;
            if int_marker >> 4 != 0x1 {
                return None;
            }
            let width = 1usize.checked_shl(u32::from(int_marker & 0x0f))?;
            (be_uint(data, at + 2, width)?, at + 2 + width)
        } else {
            (low, at + 1)
        };
        let references = if kind == 0xD {
            count.checked_mul(2)?
        } else {
            count
        };
        if references > MAX_NODES {
            return None;
        }
        for n in 0..references {
            let child = be_uint(
                data,
                body_at.checked_add(n.checked_mul(ref_width)?)?,
                ref_width,
            )?;
            if child >= objects {
                return None;
            }
            stack.push((child, depth + 1));
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Dictionary;

    fn binary(value: Value) -> Vec<u8> {
        let mut out = Vec::new();
        value.to_writer_binary(&mut out).expect("serialise");
        out
    }

    #[test]
    fn a_binary_plist_is_read() {
        let mut dict = Dictionary::new();
        dict.insert("rate".into(), Value::Integer(1u64.into()));
        let value = parse(&binary(Value::Dictionary(dict))).expect("parsed");
        assert!(value.as_dictionary().is_some());
    }

    #[test]
    fn an_xml_plist_is_refused_before_the_xml_reader_is_reached() {
        let xml = b"<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
            <key>rate</key><integer>1</integer></dict></plist>";
        assert!(parse(xml).is_none());
        // The same document does parse in the library, so the refusal is ours.
        assert!(Value::from_reader(io::Cursor::new(&xml[..])).is_ok());
    }

    /// A list `depth` arrays deep, each holding the next, ending in an integer.
    fn nested(depth: usize) -> Vec<u8> {
        let mut out = b"bplist00".to_vec();
        for i in 0..depth {
            out.push(0xA1);
            out.extend_from_slice(&((i as u32) + 1).to_be_bytes());
        }
        out.extend_from_slice(&[0x10, 0x01]);
        let table_at = out.len();
        for i in 0..depth {
            out.extend_from_slice(&((8 + 5 * i) as u32).to_be_bytes());
        }
        out.extend_from_slice(&((8 + 5 * depth) as u32).to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&[4, 4]);
        out.extend_from_slice(&((depth + 1) as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&(table_at as u64).to_be_bytes());
        out
    }

    #[test]
    fn real_nesting_is_read_and_absurd_nesting_is_refused() {
        // `nested(n)` is n arrays and a leaf: n + 1 levels.
        assert!(parse(&nested(4)).is_some(), "real requests nest like this");
        assert!(parse(&nested(MAX_DEPTH - 1)).is_some(), "the limit itself");
        assert!(parse(&nested(MAX_DEPTH)).is_none(), "one past it");
        assert!(
            parse(&nested(300_000)).is_none(),
            "and without a stack overflow"
        );
    }

    #[test]
    fn a_small_list_that_expands_enormously_is_refused() {
        // Twelve arrays, each holding the next one three times: a tree of
        // 3^12 nodes from a dozen objects.
        let depth = 12usize;
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for i in 0..depth {
            offsets.push(out.len());
            out.push(0xA3);
            for _ in 0..3 {
                out.push((i + 1) as u8);
            }
        }
        offsets.push(out.len());
        out.extend_from_slice(&[0x10, 0x01]);
        let table_at = out.len();
        for o in &offsets {
            out.push(*o as u8);
        }
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&[1, 1]);
        out.extend_from_slice(&((depth + 1) as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&(table_at as u64).to_be_bytes());
        let tree: usize = 3usize.pow(depth as u32);
        assert!(
            tree > MAX_NODES,
            "the fixture really does expand past the limit"
        );
        assert!(parse(&out).is_none());
    }

    #[test]
    fn a_dictionary_with_real_content_is_read() {
        let mut stream = Dictionary::new();
        stream.insert("type".into(), Value::Integer(103u64.into()));
        stream.insert("shk".into(), Value::Data(vec![7; 32]));
        let mut top = Dictionary::new();
        top.insert(
            "streams".into(),
            Value::Array(vec![Value::Dictionary(stream)]),
        );
        assert!(parse(&binary(Value::Dictionary(top))).is_some());
    }

    #[test]
    fn nothing_that_is_not_a_plist_is_read() {
        for body in [&b""[..], b"bplist", b"bplist99", b"\x00\x01\x02", b"{}"] {
            assert!(parse(body).is_none());
        }
    }
}
