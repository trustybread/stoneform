//! The encoding table (`EN`): which encoding key a content key is stored
//! under, and the file's decoded size.
//!
//! ```text
//! "EN"  version  content key size  encoding key size  CE page size (KB, u16 BE)
//! ESpec page size (KB, u16 BE)  CE page count (u32 BE)  ESpec page count (u32 BE)
//! 0  ESpec block size (u32 BE)  ESpec block
//! CE page index: per page, its first content key and MD5
//! CE pages: entries of key count (u8), file size (u40 BE), content key,
//!           encoding keys; zero-padded
//! ESpec pages (not read here)
//! ```
//!
//! WoW's table is about 110 MB and lists every file of every build, so it is
//! read in place: [`EncodingTable::find`] scans the pages once for the keys
//! asked for.

use std::collections::{HashMap, HashSet};

use crate::bytes::{self, Reader};
use crate::{ContentKey, EncodingKey, Error, Result};

const KEY_SIZE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodingEntry {
    /// The first encoding key the table lists for the content.
    pub encoding_key: EncodingKey,
    /// The decoded size.
    pub size: u64,
}

/// One content key's entry, read in place.
#[derive(Debug, Clone, Copy)]
pub struct ContentEntry<'a> {
    pub content_key: ContentKey,
    pub size: u64,
    keys: &'a [u8],
}

impl<'a> ContentEntry<'a> {
    /// Every encoding key the content is stored under (usually one).
    pub fn encoding_keys(&self) -> impl Iterator<Item = EncodingKey> + use<'a> {
        self.keys.chunks_exact(KEY_SIZE).filter_map(EncodingKey::from_slice)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncodingTable<'a> {
    data: &'a [u8],
    page_size: usize,
    page_count: usize,
    pages_start: usize,
}

impl<'a> EncodingTable<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        let mut r = Reader::new(data, "encoding table");
        if r.take(2)? != b"EN" {
            return Err(Error::malformed("encoding table header"));
        }
        let _version = r.u8()?;
        let content_key_size = usize::from(r.u8()?);
        let encoding_key_size = usize::from(r.u8()?);
        let page_size = r.be(2)? as usize * 1024;
        let _espec_page_size = r.be(2)?;
        let page_count = r.be(4)? as usize;
        let _espec_page_count = r.be(4)?;
        let _unknown = r.u8()?;
        let espec_size = r.be(4)? as usize;
        if content_key_size != KEY_SIZE || encoding_key_size != KEY_SIZE || page_size == 0 {
            return Err(Error::unsupported(format!(
                "encoding table with {content_key_size}/{encoding_key_size}-byte keys, {page_size}-byte pages"
            )));
        }
        // The CE page index (first key and MD5 per page) sits between the
        // ESpec block and the pages.
        let pages_start = r.pos() + espec_size + page_count * (content_key_size + 16);
        if pages_start + page_count * page_size > data.len() {
            return Err(Error::malformed("encoding table (unexpected end)"));
        }
        Ok(Self { data, page_size, page_count, pages_start })
    }

    /// Every content entry, page by page, read lazily.
    pub fn entries(&self) -> impl Iterator<Item = ContentEntry<'a>> + use<'a> {
        let Self { data, page_size, page_count, pages_start } = *self;
        (0..page_count).flat_map(move |page| {
            let start = pages_start + page * page_size;
            PageEntries { page: &data[start..start + page_size] }
        })
    }

    /// The entries for `wanted`, scanning the table once and stopping as soon
    /// as every key is found. Keys the table lacks are missing from the map.
    pub fn find(&self, wanted: &HashSet<ContentKey>) -> HashMap<ContentKey, EncodingEntry> {
        let mut found = HashMap::with_capacity(wanted.len());
        if wanted.is_empty() {
            return found;
        }
        for entry in self.entries() {
            if !wanted.contains(&entry.content_key) || found.contains_key(&entry.content_key) {
                continue;
            }
            if let Some(encoding_key) = entry.encoding_keys().next() {
                found.insert(entry.content_key, EncodingEntry { encoding_key, size: entry.size });
                if found.len() == wanted.len() {
                    break;
                }
            }
        }
        found
    }

    /// Every encoding key the table lists: all files of the build.
    pub fn encoding_keys(&self) -> impl Iterator<Item = EncodingKey> + use<'a> {
        self.entries().flat_map(|entry| entry.encoding_keys())
    }
}

/// The entries of one CE page, up to the zero padding at its end.
struct PageEntries<'a> {
    page: &'a [u8],
}

impl<'a> Iterator for PageEntries<'a> {
    type Item = ContentEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // key count, size (5), content key, at least one encoding key
        let page = self.page;
        if page.len() < 6 + 2 * KEY_SIZE || page[0] == 0 {
            return None;
        }
        let key_count = usize::from(page[0]);
        let end = (6 + KEY_SIZE + key_count * KEY_SIZE).min(page.len());
        let entry = ContentEntry {
            content_key: ContentKey::from_slice(&page[6..6 + KEY_SIZE])?,
            size: bytes::be_uint(&page[1..6]),
            keys: &page[6 + KEY_SIZE..end],
        };
        self.page = &page[end..];
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytes::push_be;

    /// A table of one 1 KB page whose entries map key i to key 0xA0 + i.
    fn table(count: u8, page_count: u64) -> Vec<u8> {
        let mut page = Vec::new();
        for i in 1..=count {
            page.push(1);
            push_be(&mut page, u64::from(i) * 1000, 5);
            page.extend([i; 16]);
            page.extend([0xA0 + i; 16]);
        }
        page.resize(1024, 0);
        let mut data = b"EN\x01\x10\x10".to_vec();
        push_be(&mut data, 1, 2);
        push_be(&mut data, 1, 2);
        push_be(&mut data, page_count, 4);
        push_be(&mut data, 0, 4);
        data.push(0);
        push_be(&mut data, 0, 4);
        data.extend([1; 16]);
        data.extend(bytes::md5(&page));
        data.extend(page);
        data
    }

    /// Port of Waypoint's `encodingTableFindsWantedKeys`.
    #[test]
    fn finds_wanted_keys() {
        let data = table(3, 1);
        let table = EncodingTable::parse(&data).unwrap();
        let wanted = HashSet::from([ContentKey::new([2; 16]), ContentKey::new([3; 16]), ContentKey::new([9; 16])]);
        let found = table.find(&wanted);
        assert_eq!(found.len(), 2);
        assert_eq!(found[&ContentKey::new([2; 16])].encoding_key, EncodingKey::new([0xA2; 16]));
        assert_eq!(found[&ContentKey::new([3; 16])].size, 3000);
        assert!(table.find(&HashSet::new()).is_empty());
    }

    #[test]
    fn lists_every_encoding_key() {
        let data = table(3, 1);
        let keys: Vec<EncodingKey> = EncodingTable::parse(&data).unwrap().encoding_keys().collect();
        assert_eq!(keys, (1..=3).map(|i| EncodingKey::new([0xA0 + i; 16])).collect::<Vec<_>>());
    }

    #[test]
    fn rejects_truncated_tables_and_other_key_sizes() {
        let data = table(3, 1);
        assert!(matches!(EncodingTable::parse(&data[..data.len() - 1]), Err(Error::Malformed(_))));
        assert!(matches!(EncodingTable::parse(&table(3, 2)), Err(Error::Malformed(_))));
        let mut nine = data.clone();
        nine[4] = 9;
        assert!(matches!(EncodingTable::parse(&nine), Err(Error::Unsupported(_))));
        assert!(matches!(EncodingTable::parse(b"XX"), Err(Error::Malformed(_))));
    }
}
