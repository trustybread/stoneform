//! CDN archive indexes (`<archive>.index`): where each encoded file sits
//! inside an archive.
//!
//! ```text
//! blocks of block-size KB: entries of key, size (BE), offset (BE); an
//!   all-zero key ends a block early, the rest is zero padding
//! table of contents: each block's last key, then each block's hash
//! footer (28 bytes): TOC hash (8), version, 2 reserved, block size (KB),
//!   offset bytes, size bytes, key bytes, hash bytes, entry count (u32 LE),
//!   footer hash (8)
//! ```
//!
//! The same layout serves the CDN's file index (offset bytes 0: files outside
//! archives, with their sizes) and the archive-group index (offset bytes 5 or
//! 6: the archive's position in the CDN config, then the offset; see
//! [`crate::archive_group`]).

use std::collections::{HashMap, HashSet};

use crate::bytes;
use crate::{EncodingKey, Error, Result};

pub const FOOTER_SIZE: usize = 28;

const KEY_SIZE: usize = 16;

/// An archive index's footer fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub toc_hash: [u8; 8],
    pub version: u8,
    pub block_size_kb: u8,
    pub offset_bytes: u8,
    pub size_bytes: u8,
    pub key_bytes: u8,
    pub hash_bytes: u8,
    pub entry_count: u32,
    pub footer_hash: [u8; 8],
}

impl Footer {
    pub fn parse(footer: &[u8; FOOTER_SIZE]) -> Self {
        let mut toc_hash = [0; 8];
        toc_hash.copy_from_slice(&footer[..8]);
        let mut footer_hash = [0; 8];
        footer_hash.copy_from_slice(&footer[20..]);
        Self {
            toc_hash,
            version: footer[8],
            // 9 and 10 are reserved.
            block_size_kb: footer[11],
            offset_bytes: footer[12],
            size_bytes: footer[13],
            key_bytes: footer[14],
            hash_bytes: footer[15],
            entry_count: bytes::le_u32_at(footer, 16),
            footer_hash,
        }
    }
}

/// One file in an archive (or, in a group index, in one of the archives).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub key: EncodingKey,
    /// The encoded size: the length of the byte range to fetch.
    pub size: u64,
    /// The offset in the archive. In an archive-group index, the archive's
    /// position in the CDN config's list is above bit 32 (see
    /// [`ArchiveEntry::group_location`]).
    pub offset: u64,
}

impl ArchiveEntry {
    /// For an archive-group index entry: (the archive's position in the CDN
    /// config's `archives`, the offset in that archive).
    pub fn group_location(&self) -> (usize, u32) {
        ((self.offset >> 32) as usize, self.offset as u32)
    }
}

/// An `.index` file, read in place.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveIndex<'a> {
    data: &'a [u8],
    footer: Footer,
    block_size: usize,
    block_count: usize,
    entry_size: usize,
}

impl<'a> ArchiveIndex<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let footer_bytes = data
            .len()
            .checked_sub(FOOTER_SIZE)
            .and_then(|start| <&[u8; FOOTER_SIZE]>::try_from(&data[start..]).ok())
            .ok_or_else(|| Error::malformed("archive index (shorter than its footer)"))?;
        let footer = Footer::parse(footer_bytes);
        let block_size = usize::from(footer.block_size_kb) * 1024;
        let (offset_bytes, size_bytes) = (usize::from(footer.offset_bytes), usize::from(footer.size_bytes));
        if usize::from(footer.key_bytes) != KEY_SIZE || offset_bytes > 8 || size_bytes > 8 || block_size == 0 {
            return Err(Error::unsupported(format!(
                "archive index layout (key {}, size {size_bytes}, offset {offset_bytes}, block {block_size})",
                footer.key_bytes
            )));
        }
        // Each block has its last key and hash in the table of contents.
        let block_count = (data.len() - FOOTER_SIZE) / (block_size + KEY_SIZE + usize::from(footer.hash_bytes));
        let entry_size = KEY_SIZE + size_bytes + offset_bytes;
        Ok(Self { data, footer, block_size, block_count, entry_size })
    }

    pub fn footer(&self) -> &Footer {
        &self.footer
    }

    /// Every entry, block by block, in key order.
    pub fn iter(&self) -> impl Iterator<Item = ArchiveEntry> + use<'a> {
        let index = *self;
        (0..index.block_count).flat_map(move |block| index.block_entries(block))
    }

    /// Finds `wanted` in one pass over the index, stopping once all are found.
    pub fn locate(&self, wanted: &HashSet<EncodingKey>) -> HashMap<EncodingKey, ArchiveEntry> {
        let mut found = HashMap::new();
        if wanted.is_empty() {
            return found;
        }
        for entry in self.iter() {
            if wanted.contains(&entry.key) {
                found.insert(entry.key, entry);
                if found.len() == wanted.len() {
                    break;
                }
            }
        }
        found
    }

    /// Looks one key up: a binary search of the table of contents (each
    /// block's last key, as CDN and group indexes have it) for its block,
    /// then a scan of that block.
    pub fn find(&self, key: &EncodingKey) -> Option<ArchiveEntry> {
        let toc = self.block_count * self.block_size;
        let last_key = |block: usize| &self.data[toc + block * KEY_SIZE..toc + (block + 1) * KEY_SIZE];
        let (mut low, mut high) = (0, self.block_count);
        while low < high {
            let mid = (low + high) / 2;
            if last_key(mid) < key.as_bytes().as_slice() { low = mid + 1 } else { high = mid }
        }
        if low == self.block_count {
            return None;
        }
        self.block_entries(low).find(|entry| entry.key == *key)
    }

    fn block_entries(&self, block: usize) -> impl Iterator<Item = ArchiveEntry> + use<'a> {
        let start = block * self.block_size;
        let records = &self.data[start..start + self.block_size];
        let (size_bytes, entry_size) = (usize::from(self.footer.size_bytes), self.entry_size);
        records
            .chunks_exact(entry_size)
            .take_while(|record| record[..KEY_SIZE].iter().any(|&byte| byte != 0))
            .filter_map(move |record| {
                Some(ArchiveEntry {
                    key: EncodingKey::from_slice(&record[..KEY_SIZE])?,
                    size: bytes::be_uint(&record[KEY_SIZE..KEY_SIZE + size_bytes]),
                    offset: bytes::be_uint(&record[KEY_SIZE + size_bytes..]),
                })
            })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bytes::push_be;

    pub(crate) fn key(byte: u8) -> EncodingKey {
        EncodingKey::new([byte; 16])
    }

    /// Waypoint's fixture: one 4 KB block of two entries, TOC, footer.
    fn two_entry_index() -> Vec<u8> {
        let mut data = Vec::new();
        for (k, size, offset) in [(0x11, 500, 0), (0x22, 700, 500)] {
            data.extend(key(k).as_bytes());
            push_be(&mut data, size, 4);
            push_be(&mut data, offset, 4);
        }
        data.resize(4096, 0);
        data.extend(key(0x22).as_bytes()); // table of contents
        data.extend([0; 8]);
        data.extend([0; 8]);
        data.extend([1, 0, 0, 4, 4, 4, 16, 8, 2, 0, 0, 0]);
        data.extend([0; 8]);
        data
    }

    /// Port of Waypoint's `archiveIndexLocatesEntries`.
    #[test]
    fn locates_entries() {
        let data = two_entry_index();
        let index = ArchiveIndex::parse(&data).unwrap();
        let found = index.locate(&HashSet::from([key(0x22), key(0x33)]));
        assert_eq!(found, HashMap::from([(key(0x22), ArchiveEntry { key: key(0x22), size: 700, offset: 500 })]));
        assert_eq!(index.footer().entry_count, 2);
        assert_eq!(index.iter().count(), 2);
    }

    #[test]
    fn finds_single_keys_through_the_table_of_contents() {
        let data = two_entry_index();
        let index = ArchiveIndex::parse(&data).unwrap();
        assert_eq!(index.find(&key(0x11)).map(|e| (e.size, e.offset)), Some((500, 0)));
        assert_eq!(index.find(&key(0x22)).map(|e| (e.size, e.offset)), Some((700, 500)));
        assert_eq!(index.find(&key(0x15)), None);
        assert_eq!(index.find(&key(0x33)), None);
    }

    #[test]
    fn rejects_short_and_unknown_layouts() {
        assert!(matches!(ArchiveIndex::parse(&[0; 27]), Err(Error::Malformed(_))));
        let mut data = two_entry_index();
        let n = data.len();
        data[n - 28 + 14] = 9; // key bytes
        assert!(matches!(ArchiveIndex::parse(&data), Err(Error::Unsupported(_))));
    }

    #[test]
    fn splits_group_locations() {
        let entry = ArchiveEntry { key: key(1), size: 10, offset: 3 << 32 | 0x1234 };
        assert_eq!(entry.group_location(), (3, 0x1234));
    }
}
