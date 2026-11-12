//! The 30-byte header in front of every blob in an archive, and the 16
//! segment headers every archive starts with.
//!
//! ```text
//! 0x00  the encoding key, reversed (16 bytes; only the first 9 key bytes,
//!       the rest zero, except for build-config files)
//! 0x10  size: header + blob (u32 LE)
//! 0x14  flags (u16 LE)
//! 0x16  hashlittle of bytes 0x00..0x16, seeded 0x3D6BE971 (u32 LE)
//! 0x1A  a checksum tied to the entry's position (4 bytes)
//! ```

use super::{ENTRY_HEADER_SIZE, IndexEntry, MAX_BLOB_SIZE, SEGMENT_HEADERS_SIZE, StorageLocation};
use crate::bytes::{le_u32_at, put_le32};
use crate::keys::bucket;
use crate::{EncodingKey, Error, Result, TruncatedKey, lookup3};

const CHECKSUM_SEED: u32 = 0x3D6BE971;

const POSITION_TABLE: [u32; 16] = [
    0x049396B8, 0x72A82A9B, 0xEE626CCA, 0x9917754F, 0x15DE40B1, 0xF5A8A9B6, 0x421EAC7E, 0xA9D55C9A, 0x317FD40C,
    0x04FAF80D, 0x3D6BE971, 0x52933CFD, 0x27F64B7D, 0xC6F5C11B, 0xD5757E3A, 0x6C388745,
];

/// The header for a blob of `blob_size` bytes stored at `location`.
///
/// `full_key` keeps all 16 key bytes, as the Agent writes them for
/// build-config files (encoding table, manifests); everything else keeps 9.
/// Fails for a blob too big for any archive.
pub fn entry_header(
    key: &EncodingKey,
    blob_size: u32,
    location: StorageLocation,
    flags: u16,
    full_key: bool,
) -> Result<[u8; ENTRY_HEADER_SIZE]> {
    if u64::from(blob_size) > MAX_BLOB_SIZE {
        return Err(Error::unsupported(format!("{blob_size}-byte blob: larger than a storage archive")));
    }
    let mut key = *key.as_bytes();
    if !full_key {
        key[9..].fill(0);
    }
    Ok(encode(&key, blob_size + ENTRY_HEADER_SIZE as u32, location, flags))
}

/// The header bytes for an already truncated (or full) `key`.
fn encode(key: &[u8; 16], size: u32, location: StorageLocation, flags: u16) -> [u8; ENTRY_HEADER_SIZE] {
    let mut h = [0u8; ENTRY_HEADER_SIZE];
    for (out, byte) in h[..16].iter_mut().zip(key.iter().rev()) {
        *out = *byte;
    }
    put_le32(&mut h, 0x10, size);
    h[0x14..0x16].copy_from_slice(&flags.to_le_bytes());
    let checksum = lookup3::hashlittle(&h[..0x16], CHECKSUM_SEED);
    put_le32(&mut h, 0x16, checksum);

    // The position checksum: the storage offset (truncated to 32 bits),
    // scrambled by a table, XORed into the header folded to 4 bytes.
    let p = location.packed() as u32;
    let x = p.wrapping_add(0x1E);
    let encoded = (POSITION_TABLE[(x & 0xF) as usize] ^ x).to_le_bytes();
    let mut folded = [0u8; 4];
    for (i, byte) in h[..0x1A].iter().enumerate() {
        folded[i & 3] ^= byte;
    }
    for i in 0x1A..ENTRY_HEADER_SIZE {
        h[i] = folded[i & 3] ^ encoded[(p.wrapping_add(i as u32) & 3) as usize];
    }
    h
}

/// An entry header's fields, as read from an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryHeader {
    /// The key in its usual byte order: 9 bytes then zeros, or all 16.
    pub key: [u8; 16],
    /// Header plus blob.
    pub size: u32,
    pub flags: u16,
}

impl EntryHeader {
    pub fn parse(header: &[u8; ENTRY_HEADER_SIZE]) -> Self {
        let mut key = [0u8; 16];
        for (out, byte) in key.iter_mut().zip(header[..16].iter().rev()) {
            *out = *byte;
        }
        Self { key, size: le_u32_at(header, 0x10), flags: u16::from_le_bytes([header[0x14], header[0x15]]) }
    }

    pub fn truncated_key(&self) -> TruncatedKey {
        let mut key = [0; 9];
        key.copy_from_slice(&self.key[..9]);
        TruncatedKey::new(key)
    }
}

/// Whether `header` is exactly what [`entry_header`] writes for its own key,
/// size and flags at `location`: both checksums hold. The game also accepts
/// zeroed headers and older checksums, so this is for checking headers this
/// writer wrote (say, when resuming), not for rejecting someone else's.
pub fn check_entry_header(header: &[u8; ENTRY_HEADER_SIZE], location: StorageLocation) -> bool {
    let fields = EntryHeader::parse(header);
    encode(&fields.key, fields.size, location, fields.flags) == *header
}

/// The 16 headers an archive starts with, and their index entries: header
/// i is indexed in bucket i.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeaders {
    pub bytes: [u8; SEGMENT_HEADERS_SIZE],
    /// Entry i goes in index file i.
    pub entries: [IndexEntry; 16],
}

/// The segment headers of `archive`. Their keys are `base_key` with the
/// archive number in bytes 1–2 and byte 0 picked so header i falls in bucket
/// i; the Agent picks a random base key per storage, and every archive's
/// first header carries it (reversed), so an existing storage's base key is
/// the first 16 bytes of `data.000`, reversed. Fails for an archive number
/// above [`MAX_ARCHIVES`](super::MAX_ARCHIVES).
pub fn segment_headers(base_key: &[u8; 16], archive: u16) -> Result<SegmentHeaders> {
    if archive > super::MAX_ARCHIVES {
        return Err(Error::unsupported(format!("storage archive {archive}")));
    }
    let mut bytes = [0u8; SEGMENT_HEADERS_SIZE];
    let entries = std::array::from_fn(|bucket_index| {
        let mut key = *base_key;
        key[1..3].copy_from_slice(&archive.to_le_bytes());
        key[0] = (0..=255u8)
            .find(|&probe| {
                key[0] = probe;
                bucket(first_nine(&key), 1) == bucket_index as u8
            })
            .expect("byte 0 reaches every bucket");
        let offset = bucket_index * ENTRY_HEADER_SIZE;
        let location = StorageLocation { archive, offset: offset as u32 };
        bytes[offset..offset + ENTRY_HEADER_SIZE].copy_from_slice(&encode(&key, ENTRY_HEADER_SIZE as u32, location, 1));
        IndexEntry { key: TruncatedKey::new(*first_nine(&key)), location, size: ENTRY_HEADER_SIZE as u32 }
    });
    Ok(SegmentHeaders { bytes, entries })
}

fn first_nine(key: &[u8; 16]) -> &[u8; 9] {
    key[..9].try_into().expect("16 bytes hold 9")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::parse_hex;

    fn hex(text: &str) -> Vec<u8> {
        parse_hex(&text.replace(' ', "")).unwrap()
    }

    /// Port of Waypoint's `entryHeaderMatchesAgentWrittenExample`: a real
    /// entry from an Agent-written Warcraft III storage (data.011).
    #[test]
    fn matches_agent_written_example() {
        let key = EncodingKey::from_hex("000ca380442463454b8e13ec98a7b437").unwrap();
        assert_eq!(key.truncated().bucket(), 0);
        let location = StorageLocation::new(0xB, 0x39297ED4).unwrap();
        let header = entry_header(&key, 24394, location, 0, false).unwrap();
        assert_eq!(header.to_vec(), hex("00000000000000 4b4563244480a30c00 685f0000 0000 1a5c6cfd 7944f970"));
        assert!(check_entry_header(&header, location));
        assert!(!check_entry_header(&header, StorageLocation::new(0xB, 0x39297ED5).unwrap()));

        let fields = EntryHeader::parse(&header);
        assert_eq!(fields.truncated_key(), key.truncated());
        assert_eq!(fields.key[9..], [0; 7]);
        assert_eq!((fields.size, fields.flags), (24394 + 30, 0));
    }

    #[test]
    fn keeps_the_full_key_when_asked_and_refuses_oversized_blobs() {
        let key = EncodingKey::new(std::array::from_fn(|i| i as u8 + 1));
        let location = StorageLocation::new(0, 480).unwrap();
        let header = entry_header(&key, 100, location, 0, true).unwrap();
        assert_eq!(EntryHeader::parse(&header).key, *key.as_bytes());
        assert!(check_entry_header(&header, location));
        assert!(entry_header(&key, MAX_BLOB_SIZE as u32 + 1, location, 0, false).is_err());
    }

    /// Every archive's 16 headers: header i in bucket i (with seed 1), the
    /// archive number in bytes 1–2, flags 1, an empty blob.
    #[test]
    fn segment_headers_fill_every_bucket() {
        let base = [7u8; 16];
        let headers = segment_headers(&base, 0x102).unwrap();
        assert!(segment_headers(&base, 0x400).is_err());
        for (i, entry) in headers.entries.iter().enumerate() {
            let at = i * ENTRY_HEADER_SIZE;
            let header: &[u8; 30] = headers.bytes[at..at + 30].try_into().unwrap();
            let fields = EntryHeader::parse(header);
            assert_eq!(bucket(first_nine(&fields.key), 1), i as u8);
            assert_eq!(fields.key[1..3], [0x02, 0x01]);
            assert_eq!(fields.key[3..], base[3..]);
            assert_eq!((fields.size, fields.flags), (30, 1));
            assert!(check_entry_header(header, entry.location));
            assert_eq!(*entry, IndexEntry { key: fields.truncated_key(), location: entry.location, size: 30 });
            assert_eq!((entry.location.archive(), entry.location.offset()), (0x102, at as u32));
        }
        // The base key comes back from the first header.
        let first: [u8; 16] = std::array::from_fn(|i| headers.bytes[15 - i]);
        assert_eq!(segment_headers(&first, 0x102).unwrap(), headers);
    }
}
