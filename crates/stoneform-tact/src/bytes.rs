//! Byte-level helpers shared by the codecs: a bounds-checked reader over a
//! borrowed slice, fixed-width integers, hex and MD5.

use md5::{Digest, Md5};

use crate::{Error, Result};

pub(crate) fn md5(data: &[u8]) -> [u8; 16] {
    Md5::digest(data).into()
}

/// A big-endian unsigned integer of up to 8 bytes.
pub(crate) fn be_uint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |value, &byte| value << 8 | u64::from(byte))
}

/// A little-endian unsigned integer of up to 8 bytes.
pub(crate) fn le_uint(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |value, &byte| value << 8 | u64::from(byte))
}

/// The four bytes at `at`, little-endian. The caller checked the bounds.
pub(crate) fn le_u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// `value`'s low `width` bytes, big-endian, appended to `out` (for building
/// test fixtures).
#[cfg(test)]
pub(crate) fn push_be(out: &mut Vec<u8>, value: u64, width: usize) {
    for i in (0..width).rev() {
        out.push((value >> (8 * i)) as u8);
    }
}

/// `value`'s low `width` bytes, big-endian, written at `at`.
pub(crate) fn put_be(out: &mut [u8], at: usize, value: u64, width: usize) {
    for i in 0..width {
        out[at + i] = (value >> (8 * (width - 1 - i))) as u8;
    }
}

pub(crate) fn put_le32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Lowercase hex, as Blizzard's configs and CDN paths spell keys.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// Hex digits (either case) as bytes; `None` for an odd length or a non-hex digit.
pub(crate) fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text.as_bytes();
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high << 4 | low) as u8)
        })
        .collect()
}

/// Reads a borrowed slice front to back, failing instead of panicking when
/// the data runs out. `what` names the format in those errors.
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8], what: &'static str) -> Self {
        Self { data, pos: 0, what }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn truncated(&self) -> Error {
        Error::malformed(format!("{} (unexpected end)", self.what))
    }

    pub(crate) fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        if count > self.remaining() {
            return Err(self.truncated());
        }
        let bytes = &self.data[self.pos..self.pos + count];
        self.pos += count;
        Ok(bytes)
    }

    pub(crate) fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// A big-endian unsigned integer of `size` bytes (at most 8).
    pub(crate) fn be(&mut self, size: usize) -> Result<u64> {
        Ok(be_uint(self.take(size)?))
    }

    /// The bytes up to the next NUL, which is consumed but not returned.
    pub(crate) fn c_str(&mut self) -> Result<&'a [u8]> {
        let rest = &self.data[self.pos..];
        let end = rest.iter().position(|&b| b == 0).ok_or_else(|| Error::malformed(format!("{} string", self.what)))?;
        self.pos += end + 1;
        Ok(&rest[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_and_hex_round_trip() {
        assert_eq!(be_uint(&[0x01, 0x02, 0x03]), 0x010203);
        assert_eq!(le_uint(&[0x01, 0x02, 0x03]), 0x030201);
        let mut out = Vec::new();
        push_be(&mut out, 0x0102030405, 5);
        assert_eq!(out, [1, 2, 3, 4, 5]);
        assert_eq!(hex(&[0x00, 0xAB, 0x7F]), "00ab7f");
        assert_eq!(parse_hex("00AB7f"), Some(vec![0x00, 0xAB, 0x7F]));
        assert_eq!(parse_hex("abc"), None);
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn reader_fails_instead_of_panicking() {
        let mut r = Reader::new(b"ab\0c", "test");
        assert_eq!(r.c_str().unwrap(), b"ab");
        assert_eq!(r.u8().unwrap(), b'c');
        assert!(matches!(r.u8(), Err(Error::Malformed(_))));
        assert!(Reader::new(b"abc", "test").c_str().is_err());
    }
}
