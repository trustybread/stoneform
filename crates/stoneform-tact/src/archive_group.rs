//! The archive-group index: one index over every archive a CDN config lists
//! (and the patch-archive-group, over its patch archives).
//!
//! It isn't on the CDN. The Battle.net Agent builds it from the archive
//! indexes and keeps it in `Data/indices`, and the game rebuilds it at start
//! when it's missing. [`build`] writes it exactly as the Agent does (byte for
//! byte, checked by Waypoint against Agent-written Warcraft III and
//! Hearthstone groups), so the game finds it ready.
//!
//! The layout is an [archive index](crate::archive_index): 4 KB pages of
//! entries (key, size u32 BE, then the archive's position in the config's
//! list and the offset in it, BE: 1 byte of position for up to 256 archives,
//! else 2), zero-padded; the table of contents (each page's last key, then
//! the first 8 bytes of each page's MD5); and the 28-byte footer. Entries are
//! sorted by key; a key in several archives keeps the first archive. The
//! file is named by the MD5 of its footer, which the CDN config lists as
//! `archive-group`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::Write;

use crate::archive_index::FOOTER_SIZE;
use crate::bytes::{self, put_be};
use crate::{Error, Result};

const PAGE_SIZE: usize = 4096;

/// Builds the group over `indexes` (the archives' `.index` files, in the CDN
/// config's order) into `output`, page by page, and returns the name it must
/// have: the lowercase hex MD5 of its footer. When that isn't the name the
/// CDN config expects, the result is no use to the game: discard it.
///
/// Memory stays small: a k-way merge over the borrowed indexes, one page
/// buffered at a time.
pub fn build(indexes: &[&[u8]], mut output: impl Write) -> Result<String> {
    if indexes.len() > 0x10000 {
        return Err(Error::unsupported(format!("archive group over {} archives", indexes.len())));
    }
    let mut sources = indexes.iter().enumerate().map(|(n, data)| Source::new(data, n)).collect::<Result<Vec<_>>>()?;
    let position_bytes = if sources.len() <= 0x100 { 1 } else { 2 };
    let offset_bytes = 4 + position_bytes;
    let entry_size = 16 + 4 + offset_bytes;
    let per_page = PAGE_SIZE / entry_size;

    // A min-heap of (key, source); ties go to the earlier source.
    let mut heap = BinaryHeap::with_capacity(sources.len());
    for (n, source) in sources.iter().enumerate() {
        if let Some(entry) = source.current() {
            heap.push(Reverse((entry.key, n)));
        }
    }

    let mut page = [0u8; PAGE_SIZE];
    let mut in_page = 0;
    let mut last_keys = Vec::new();
    let mut page_hashes = Vec::new();
    let mut count = 0u32;
    let mut previous: Option<[u8; 16]> = None;
    let mut flush = |page: &mut [u8; PAGE_SIZE], in_page: &mut usize, last: [u8; 16]| -> Result<()> {
        page[*in_page * entry_size..].fill(0);
        output.write_all(page)?;
        page_hashes.extend_from_slice(&bytes::md5(page)[..8]);
        last_keys.extend_from_slice(&last);
        *in_page = 0;
        Ok(())
    };
    while let Some(Reverse((key, n))) = heap.pop() {
        if previous != Some(key) {
            let Some(entry) = sources[n].current() else { break };
            let at = in_page * entry_size;
            page[at..at + 16].copy_from_slice(&key);
            put_be(&mut page, at + 16, u64::from(entry.size), 4);
            put_be(&mut page, at + 20, (n as u64) << 32 | u64::from(entry.offset), offset_bytes);
            previous = Some(key);
            count += 1;
            in_page += 1;
            if in_page == per_page {
                flush(&mut page, &mut in_page, key)?;
            }
        }
        sources[n].advance();
        if let Some(next) = sources[n].current() {
            heap.push(Reverse((next.key, n)));
        }
    }
    match previous {
        Some(last) if in_page > 0 => flush(&mut page, &mut in_page, last)?,
        _ => {}
    }

    let toc = [last_keys, page_hashes].concat();
    output.write_all(&toc)?;
    let footer = footer(&toc, offset_bytes as u8, count);
    output.write_all(&footer)?;
    output.flush()?;
    Ok(bytes::hex(&bytes::md5(&footer)))
}

/// The footer: TOC hash, the fields (version 1, page size 4 KB, offset,
/// size, key and hash widths, entry count), and their hash.
fn footer(toc: &[u8], offset_bytes: u8, count: u32) -> [u8; FOOTER_SIZE] {
    let mut fields = vec![1, 0, 0, (PAGE_SIZE / 1024) as u8, offset_bytes, 4, 16, 8];
    fields.extend(count.to_le_bytes());
    let mut footer = [0u8; FOOTER_SIZE];
    footer[..8].copy_from_slice(&bytes::md5(toc)[..8]);
    footer[8..20].copy_from_slice(&fields);
    fields.extend([0; 8]);
    footer[20..].copy_from_slice(&bytes::md5(&fields)[..8]);
    footer
}

/// Whether a group file is complete: its footer's MD5 is its name.
pub fn is_complete(file: &[u8], name: &str) -> bool {
    file.len() >= FOOTER_SIZE && bytes::hex(&bytes::md5(&file[file.len() - FOOTER_SIZE..])).eq_ignore_ascii_case(name)
}

/// One archive index's entries in order, read in place by position (the
/// footer's entry count, pages of whole entries), as the Agent reads them.
struct Source<'a> {
    data: &'a [u8],
    per_page: usize,
    page_bytes: usize,
    count: usize,
    index: usize,
}

#[derive(Debug, Clone, Copy)]
struct SourceEntry {
    key: [u8; 16],
    size: u32,
    offset: u32,
}

impl<'a> Source<'a> {
    fn new(data: &'a [u8], number: usize) -> Result<Self> {
        let footer = data
            .len()
            .checked_sub(FOOTER_SIZE)
            .map(|start| &data[start..])
            .ok_or_else(|| Error::malformed(format!("archive index {number} (shorter than its footer)")))?;
        let (page_kb, offset_bytes, size_bytes, key_bytes) = (footer[11], footer[12], footer[13], footer[14]);
        if page_kb == 0 || key_bytes != 16 || size_bytes != 4 || offset_bytes != 4 {
            return Err(Error::unsupported(format!(
                "archive index {number} layout ({key_bytes}/{size_bytes}/{offset_bytes})"
            )));
        }
        let page_bytes = usize::from(page_kb) * 1024;
        let per_page = page_bytes / 24;
        let count = bytes::le_u32_at(footer, 16) as usize;
        if count.div_ceil(per_page) * page_bytes > data.len() {
            return Err(Error::malformed(format!("archive index {number} entry count")));
        }
        Ok(Self { data, per_page, page_bytes, count, index: 0 })
    }

    fn current(&self) -> Option<SourceEntry> {
        if self.index >= self.count {
            return None;
        }
        let at = (self.index / self.per_page) * self.page_bytes + (self.index % self.per_page) * 24;
        let record = &self.data[at..at + 24];
        let mut key = [0; 16];
        key.copy_from_slice(&record[..16]);
        Some(SourceEntry {
            key,
            size: bytes::be_uint(&record[16..20]) as u32,
            offset: bytes::be_uint(&record[20..24]) as u32,
        })
    }

    fn advance(&mut self) {
        self.index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_index::ArchiveIndex;
    use crate::bytes::push_be;

    /// A minimal archive index: 24-byte entries in 4 KB pages, then the TOC
    /// and the footer fields the group builder reads.
    fn archive_index(entries: &[([u8; 16], u32, u32)]) -> Vec<u8> {
        let mut sorted = entries.to_vec();
        sorted.sort_by_key(|entry| entry.0);
        let mut pages = Vec::new();
        let mut page = Vec::new();
        for (key, size, offset) in sorted {
            page.extend(key);
            push_be(&mut page, u64::from(size), 4);
            push_be(&mut page, u64::from(offset), 4);
            if page.len() + 24 > 4096 {
                page.resize(4096, 0);
                pages.append(&mut page);
            }
        }
        if !page.is_empty() {
            page.resize(4096, 0);
            pages.append(&mut page);
        }
        let page_count = pages.len() / 4096;
        pages.resize(pages.len() + page_count * 24 + 8, 0);
        pages.extend([1, 0, 0, 4, 4, 4, 16, 8]);
        pages.extend((entries.len() as u32).to_le_bytes());
        pages.extend([0; 8]);
        pages
    }

    /// Never all zeros: an index reads a zero key as the end of a page.
    fn k(i: usize) -> [u8; 16] {
        std::array::from_fn(|byte| if byte == 0 { 0xA5 } else { (i >> ((8 * (15 - byte)) % 64)) as u8 })
    }

    /// Port of Waypoint's `archiveGroupMergesSortsAndKeepsTheFirstArchive`.
    #[test]
    fn merges_sorts_and_keeps_the_first_archive() {
        // 400 keys over three archives; ten of archive 0's keys are in archive 2 too.
        let mut lists: [Vec<([u8; 16], u32, u32)>; 3] = Default::default();
        for i in 0..400 {
            lists[i % 3].push((k(i * 7919 % 100_003), i as u32, i as u32 * 10));
        }
        for i in 0..10 {
            let duplicate = (lists[0][i].0, 999, 999);
            lists[2].push(duplicate);
        }
        let files: Vec<Vec<u8>> = lists.iter().map(|list| archive_index(list)).collect();
        let indexes: Vec<&[u8]> = files.iter().map(Vec::as_slice).collect();
        let mut group = Vec::new();
        let name = build(&indexes, &mut group).unwrap();

        assert_eq!(bytes::hex(&bytes::md5(&group[group.len() - 28..])), name);
        assert!(is_complete(&group, &name.to_uppercase()));
        assert!(!is_complete(&group, &"0".repeat(32)));
        let footer = &group[group.len() - 28..];
        assert_eq!(footer[8..16], [1, 0, 0, 4, 5, 4, 16, 8]); // 1-byte archive position
        assert_eq!(footer[16..20], 400u32.to_le_bytes());

        // Read it back like any index: sorted, one entry per key, the first archive wins.
        let index = ArchiveIndex::parse(&group).unwrap();
        let seen: Vec<_> = index.iter().collect();
        assert_eq!(seen.len(), 400);
        assert!(seen.windows(2).all(|pair| pair[0].key < pair[1].key));
        for (n, list) in lists.iter().enumerate() {
            for &(key, size, offset) in list.iter().filter(|entry| entry.1 != 999) {
                let hit = seen.iter().find(|entry| *entry.key.as_bytes() == key).unwrap();
                assert_eq!((hit.size, hit.group_location()), (u64::from(size), (n, offset)));
                assert_eq!(index.find(&hit.key), Some(*hit));
            }
        }
    }

    /// More than 256 archives take 2 bytes of position; an empty group still
    /// has a footer.
    #[test]
    fn widens_the_position_for_many_archives() {
        let files: Vec<Vec<u8>> = (0..300).map(|i| archive_index(&[(k(i + 1), 1, 2)])).collect();
        let indexes: Vec<&[u8]> = files.iter().map(Vec::as_slice).collect();
        let mut group = Vec::new();
        build(&indexes, &mut group).unwrap();
        let index = ArchiveIndex::parse(&group).unwrap();
        assert_eq!(index.footer().offset_bytes, 6);
        let last = index.iter().find(|entry| *entry.key.as_bytes() == k(300)).unwrap();
        assert_eq!(last.group_location(), (299, 2));

        let mut empty = Vec::new();
        let name = build(&[], &mut empty).unwrap();
        assert_eq!(empty.len(), 28);
        assert!(is_complete(&empty, &name));
    }

    #[test]
    fn rejects_other_index_layouts() {
        let mut file = archive_index(&[(k(1), 1, 2)]);
        let n = file.len();
        file[n - 28 + 12] = 5; // offset bytes
        assert!(matches!(build(&[&file], Vec::new()), Err(Error::Unsupported(_))));
        assert!(matches!(build(&[&file[..10]], Vec::new()), Err(Error::Malformed(_))));
        let too_many = vec![&file[..]; 0x10001];
        assert!(matches!(build(&too_many, Vec::new()), Err(Error::Unsupported(_))));
    }
}
