// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0
//! A bounds-checked, most-significant-bit-first reader.

use crate::Error;

/// Reads bit fields from a byte slice. Every read is bounds-checked and
/// returns [`Error::Truncated`] rather than panicking.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader positioned at bit `start` of `data`.
    pub(crate) fn at(data: &'a [u8], start: usize) -> Self {
        Self { data, pos: start }
    }

    /// The next `n` bits (at most 24) as an unsigned number.
    pub(crate) fn read(&mut self, n: usize) -> Result<u32, Error> {
        debug_assert!(n <= 24);
        let end = self.pos + n;
        if end > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        let mut value = 0u32;
        for bit in self.pos..end {
            let byte = self.data[bit >> 3];
            value = (value << 1) | u32::from((byte >> (7 - (bit & 7))) & 1);
        }
        self.pos = end;
        Ok(value)
    }

    /// The bit position of the next read.
    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_msb_first_across_bytes() {
        let mut r = BitReader::at(&[0b1010_1100, 0b0101_0011], 0);
        assert_eq!(r.read(3).unwrap(), 0b101);
        assert_eq!(r.read(7).unwrap(), 0x31);
        assert_eq!(r.read(6).unwrap(), 0x13);
        assert_eq!(r.position(), 16);
    }

    #[test]
    fn a_read_past_the_end_is_an_error_and_moves_nothing() {
        let mut r = BitReader::at(&[0xFF], 4);
        assert_eq!(r.read(5), Err(Error::Truncated));
        assert_eq!(r.position(), 4);
        assert_eq!(r.read(4).unwrap(), 0xF);
        assert_eq!(r.read(1), Err(Error::Truncated));
    }

    #[test]
    fn a_zero_width_read_is_zero() {
        let mut r = BitReader::at(&[], 0);
        assert_eq!(r.read(0).unwrap(), 0);
    }
}
