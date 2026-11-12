//! The keys TACT addresses files by.
//!
//! A *content key* is the MD5 of a file's decoded content; an *encoding key*
//! is the MD5 of its BLTE header (the encoded file as the CDN serves it). The
//! encoding table maps one to the other. They are distinct types so one can
//! never be passed where the other is expected. Both are plain 16-byte values:
//! an install plan holds millions, and none of them allocates.
//!
//! Local storage indexes files by the first 9 bytes of the encoding key, a
//! [`TruncatedKey`], and spreads them over 16 index files by
//! [`TruncatedKey::bucket`].

use std::fmt;
use std::str::FromStr;

use crate::bytes;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a key is 32 hex digits, not {0:?}")]
pub struct ParseKeyError(String);

macro_rules! md5_key {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name([u8; 16]);

        impl $name {
            pub const fn new(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            /// The key in exactly 16 bytes; `None` for any other length.
            pub fn from_slice(bytes: &[u8]) -> Option<Self> {
                bytes.try_into().ok().map(Self)
            }

            /// Parses 32 hex digits, either case.
            pub fn from_hex(text: &str) -> Result<Self, ParseKeyError> {
                bytes::parse_hex(text)
                    .and_then(|bytes| Self::from_slice(&bytes))
                    .ok_or_else(|| ParseKeyError(text.to_owned()))
            }

            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }

            /// Lowercase hex, as configs and CDN paths spell keys.
            pub fn to_hex(&self) -> String {
                bytes::hex(&self.0)
            }
        }

        impl From<[u8; 16]> for $name {
            fn from(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }
        }

        impl FromStr for $name {
            type Err = ParseKeyError;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Self::from_hex(text)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.to_hex())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.to_hex())
            }
        }
    };
}

md5_key!(
    /// The MD5 of a file's decoded content: what install manifests and the
    /// build config name files by.
    ContentKey
);

md5_key!(
    /// The MD5 of a file's BLTE header (or of the whole BLTE blob when it has
    /// no chunk table): what the CDN, archive indexes, the download manifest
    /// and local storage name files by.
    EncodingKey
);

impl ContentKey {
    /// The content key of `content`: its MD5.
    pub fn of(content: &[u8]) -> Self {
        Self(bytes::md5(content))
    }
}

impl EncodingKey {
    /// The first 9 bytes, which local storage indexes files by.
    pub fn truncated(&self) -> TruncatedKey {
        let mut key = [0; 9];
        key.copy_from_slice(&self.0[..9]);
        TruncatedKey(key)
    }
}

/// The first 9 bytes of an encoding key: what local storage's `.idx` files
/// and journal key their entries by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TruncatedKey([u8; 9]);

impl TruncatedKey {
    pub const fn new(bytes: [u8; 9]) -> Self {
        Self(bytes)
    }

    /// The key in exactly 9 bytes; `None` for any other length.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    pub const fn as_bytes(&self) -> &[u8; 9] {
        &self.0
    }

    /// Which of local storage's 16 index files the key goes in.
    pub fn bucket(&self) -> u8 {
        bucket(&self.0, 0)
    }
}

impl fmt::Debug for TruncatedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TruncatedKey({})", bytes::hex(&self.0))
    }
}

/// The CASC bucket function: the XOR of the key's 9 bytes, its two nibbles
/// folded together, plus `seed`. Data keys use seed 0; the segment headers
/// at the start of each archive are placed with seed 1.
pub(crate) fn bucket(key: &[u8; 9], seed: u8) -> u8 {
    let x = key.iter().fold(0u8, |x, &byte| x ^ byte);
    ((x >> 4) ^ x).wrapping_add(seed) & 0x0F
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of Waypoint's `compactKeysMatchTheirBytes`: keys keep the byte
    /// order, and the bucket matches the byte-wise definition.
    #[test]
    fn keys_match_their_bytes() {
        let bytes: [u8; 16] = std::array::from_fn(|i| (i * 17 + 3) as u8);
        let key = EncodingKey::new(bytes);
        assert_eq!(key.as_bytes(), &bytes);
        assert_eq!(key.to_hex(), bytes::hex(&bytes));
        assert_eq!(key.truncated().as_bytes(), &bytes[..9]);
        let x = bytes[..9].iter().fold(0u8, |x, b| x ^ b);
        assert_eq!(key.truncated().bucket(), (x >> 4) ^ (x & 0x0F));

        let mut smaller = [0xFF; 16];
        smaller[0] = 0x00;
        let mut larger = [0x00; 16];
        larger[0] = 0x01;
        assert!(EncodingKey::new(smaller) < EncodingKey::new(larger));
        assert!(TruncatedKey::new([1, 0, 0, 0, 0, 0, 0, 0, 5]) < TruncatedKey::new([2, 0, 0, 0, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn parses_and_formats_hex() {
        let key: ContentKey = "00112233445566778899AABBCCDDEEFF".parse().unwrap();
        assert_eq!(key.to_string(), "00112233445566778899aabbccddeeff");
        assert_eq!(format!("{key:?}"), "ContentKey(00112233445566778899aabbccddeeff)");
        assert_eq!(ContentKey::from_hex(&key.to_hex()), Ok(key));
        assert!(ContentKey::from_hex("0011").is_err());
        assert!(EncodingKey::from_hex(&"g".repeat(32)).is_err());
        assert!(EncodingKey::from_slice(&[0; 15]).is_none());
        let truncated = EncodingKey::new([0xAB; 16]).truncated();
        assert_eq!(format!("{truncated:?}"), "TruncatedKey(ababababababababab)");
    }

    #[test]
    fn content_key_is_the_md5_of_the_content() {
        // RFC 1321's test vector for "abc".
        assert_eq!(ContentKey::of(b"abc").to_hex(), "900150983cd24fb0d6963f7d28e17f72");
    }

    /// A real key from an Agent-written Warcraft III storage lands in bucket 0.
    #[test]
    fn buckets_like_the_agent() {
        let key = EncodingKey::from_hex("000ca380442463454b8e13ec98a7b437").unwrap();
        assert_eq!(key.truncated().bucket(), 0);
    }
}
