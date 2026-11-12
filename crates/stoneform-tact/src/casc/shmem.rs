//! `shmem` (version 5): the storage's index versions and free-space table.
//!
//! ```text
//! 0x0000  5, 0x154 (u32 LE)  "<storage path>/index", NUL-padded to 0x100
//! 0x0108  0x2AB8, 0x1000 (u32 LE)
//! 0x0110  the 16 index files' versions (u32 LE)
//! 0x0150  2, 1, 0x228 (u32 LE)   0x01D8  0x4000 (u32 LE)
//! 0x1000  1, span count (u32 LE)
//! 0x1020  span sizes (u40 BE), 1,090 slots; then span storage offsets (u40 BE)
//! 0x4000  1, 0, 0, 0, 1, 0, 0x40, 0x0C, 0x40, 0x104 (u32 LE)
//! ```
//!
//! The free spans are archive tails, unused archives, and (after cleaning)
//! holes inside archives, so the game and the Agent could reuse them.

use super::{ARCHIVE_SIZE, MAX_ARCHIVES, OFFSET_BITS};
use crate::bytes::{be_uint, le_u32_at, put_be, put_le32};
use crate::{Error, Result};

pub const SHMEM_SIZE: usize = 0x5000;

/// Free spans the table has room for.
pub const FREE_SPAN_CAPACITY: usize = 1090;

const VERSION: u32 = 5;
const PATH_FIELD: usize = 0x100;
const VERSIONS_AT: usize = 0x110;
const SPANS_AT: usize = 0x1020;
/// Spans of this size or smaller aren't worth listing.
const MIN_SPAN: u64 = 0x40;

/// A free byte range: its size, and where it starts as a packed storage offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeSpan {
    pub size: u64,
    pub offset: u64,
}

/// The `shmem` for a storage at `storage_path` (the `Data/data` folder) with
/// these index versions and archive sizes. `holes` are free gaps inside
/// archives, listed after the archive tails and unused archives as room
/// allows, biggest first. `None` when the path doesn't fit its fixed field;
/// the game rebuilds the file then.
pub fn encode_shmem(
    storage_path: &str,
    index_versions: &[u32; 16],
    archive_sizes: &[u64],
    holes: &[FreeSpan],
) -> Option<Vec<u8>> {
    let path = format!("{}/index", storage_path.strip_suffix('/').unwrap_or(storage_path));
    if path.len() >= PATH_FIELD {
        return None;
    }
    let mut d = vec![0u8; SHMEM_SIZE];
    put_le32(&mut d, 0, VERSION);
    put_le32(&mut d, 4, 0x154);
    d[8..8 + path.len()].copy_from_slice(path.as_bytes());
    put_le32(&mut d, 0x108, 0x2AB8);
    put_le32(&mut d, 0x10C, 0x1000);
    for (i, &version) in index_versions.iter().enumerate() {
        put_le32(&mut d, VERSIONS_AT + 4 * i, version);
    }
    put_le32(&mut d, 0x150, 2);
    put_le32(&mut d, 0x154, 1);
    put_le32(&mut d, 0x158, 0x228);
    put_le32(&mut d, 0x1D8, 0x4000);
    for (i, value) in [1, 0, 0, 0, 1, 0, 0x40, 0x0C, 0x40, 0x104].into_iter().enumerate() {
        put_le32(&mut d, 0x4000 + 4 * i, value);
    }

    let mut spans = Vec::new();
    for (archive, &size) in archive_sizes.iter().enumerate() {
        let free = ARCHIVE_SIZE.saturating_sub(size);
        if free > MIN_SPAN {
            spans.push(FreeSpan { size: free, offset: (archive as u64) << OFFSET_BITS | size });
        }
    }
    for archive in archive_sizes.len()..usize::from(MAX_ARCHIVES) {
        spans.push(FreeSpan { size: ARCHIVE_SIZE, offset: (archive as u64) << OFFSET_BITS });
    }
    // The table holds 1,090 spans: archive tails and unused archives first,
    // then the biggest holes.
    let room = FREE_SPAN_CAPACITY.saturating_sub(spans.len());
    let mut big_holes: Vec<FreeSpan> = holes.iter().copied().filter(|hole| hole.size > MIN_SPAN).collect();
    big_holes.sort_by_key(|hole| std::cmp::Reverse(hole.size));
    spans.extend(big_holes.into_iter().take(room));
    spans.sort_by_key(|span| span.offset);
    spans.truncate(FREE_SPAN_CAPACITY);

    put_le32(&mut d, 0x1000, 1);
    put_le32(&mut d, 0x1004, spans.len() as u32);
    for (i, span) in spans.iter().enumerate() {
        put_be(&mut d, SPANS_AT + 5 * i, span.size, 5);
        put_be(&mut d, SPANS_AT + 5 * FREE_SPAN_CAPACITY + 5 * i, span.offset, 5);
    }
    Some(d)
}

/// What a `shmem` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shmem {
    /// `<storage path>/index`, as written.
    pub path: String,
    pub index_versions: [u32; 16],
    pub free_spans: Vec<FreeSpan>,
}

impl Shmem {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < SPANS_AT + 10 * FREE_SPAN_CAPACITY {
            return Err(Error::malformed("shmem (unexpected end)"));
        }
        let version = le_u32_at(data, 0);
        if version != VERSION {
            return Err(Error::unsupported(format!("shmem version {version}")));
        }
        let field = &data[8..8 + PATH_FIELD];
        let path_len = field.iter().position(|&byte| byte == 0).unwrap_or(PATH_FIELD);
        let path = String::from_utf8_lossy(&field[..path_len]).into_owned();
        let index_versions = std::array::from_fn(|i| le_u32_at(data, VERSIONS_AT + 4 * i));
        let count = le_u32_at(data, 0x1004) as usize;
        if count > FREE_SPAN_CAPACITY {
            return Err(Error::malformed(format!("shmem with {count} free spans")));
        }
        let free_spans = (0..count)
            .map(|i| {
                let size_at = SPANS_AT + 5 * i;
                let offset_at = SPANS_AT + 5 * FREE_SPAN_CAPACITY + 5 * i;
                FreeSpan {
                    size: be_uint(&data[size_at..size_at + 5]),
                    offset: be_uint(&data[offset_at..offset_at + 5]),
                }
            })
            .collect();
        Ok(Self { path, index_versions, free_spans })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_v5_layout() {
        let versions: [u32; 16] = std::array::from_fn(|i| i as u32 + 1);
        let d = encode_shmem("/Applications/World of Warcraft/Data/data/", &versions, &[0x1000, ARCHIVE_SIZE], &[])
            .unwrap();
        assert_eq!(d.len(), 0x5000);
        assert_eq!(d[..8], [5, 0, 0, 0, 0x54, 1, 0, 0]);
        let path = b"/Applications/World of Warcraft/Data/data/index";
        assert_eq!(&d[8..8 + path.len()], path);
        assert_eq!(d[8 + path.len()], 0);
        assert_eq!(d[0x108..0x110], [0xB8, 0x2A, 0, 0, 0, 0x10, 0, 0]);
        assert_eq!(d[0x110..0x118], [1, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(d[0x150..0x15C], [2, 0, 0, 0, 1, 0, 0, 0, 0x28, 2, 0, 0]);
        assert_eq!(d[0x1D8..0x1DC], [0, 0x40, 0, 0]);
        assert_eq!(d[0x4018..0x4028], [0x40, 0, 0, 0, 0x0C, 0, 0, 0, 0x40, 0, 0, 0, 4, 1, 0, 0]);
        // data.000's tail, then archives 2 to 1,022 (data.001 is full).
        assert_eq!(d[0x1000..0x1008], [1, 0, 0, 0, 0xFE, 0x03, 0, 0]);
        assert_eq!(d[SPANS_AT..SPANS_AT + 5], [0, 0x3F, 0xFF, 0xF0, 0]);
        assert_eq!(d[SPANS_AT + 5450..SPANS_AT + 5455], [0, 0, 0, 0x10, 0]);
        assert_eq!(d[SPANS_AT + 5455..SPANS_AT + 5460], [0, 0x80, 0, 0, 0]);

        let parsed = Shmem::parse(&d).unwrap();
        assert_eq!(parsed.path, "/Applications/World of Warcraft/Data/data/index");
        assert_eq!(parsed.index_versions, versions);
        assert_eq!(parsed.free_spans.len(), 1 + 1021);
        assert_eq!(parsed.free_spans[0], FreeSpan { size: ARCHIVE_SIZE - 0x1000, offset: 0x1000 });
        assert_eq!(parsed.free_spans[1], FreeSpan { size: ARCHIVE_SIZE, offset: 2 << 30 });
    }

    /// As after `cleaningRemovesUnneededFilesAndGivesSpaceBack`: data.000's
    /// tail and 1,022 unused archives, plus the holes, in offset order.
    #[test]
    fn lists_holes_after_tails_and_unused_archives() {
        let holes = [
            FreeSpan { size: 0x30, offset: 0x2000 }, // too small to list
            FreeSpan { size: 0x5000, offset: 0x8000 },
            FreeSpan { size: 0x9000, offset: 0x1000 },
        ];
        let d = encode_shmem("/x", &[2; 16], &[0x20000], &holes).unwrap();
        let spans = Shmem::parse(&d).unwrap().free_spans;
        assert_eq!(spans.len(), 0x3FF + 2);
        assert_eq!(spans[..3], [holes[2], holes[1], FreeSpan { size: ARCHIVE_SIZE - 0x20000, offset: 0x20000 }]);
        assert!(spans.windows(2).all(|pair| pair[0].offset < pair[1].offset));

        // With the table full, only the biggest holes make it.
        let many: Vec<FreeSpan> = (0..100).map(|i| FreeSpan { size: 0x100 + i, offset: 0x1000 * (i + 1) }).collect();
        let d = encode_shmem("/x", &[1; 16], &[0x200], &many).unwrap();
        let spans = Shmem::parse(&d).unwrap().free_spans;
        assert_eq!(spans.len(), FREE_SPAN_CAPACITY);
        assert!(spans.contains(&many[99]) && !spans.contains(&many[0]));
    }

    #[test]
    fn gives_up_on_long_paths() {
        assert!(encode_shmem(&"x".repeat(250), &[1; 16], &[], &[]).is_none());
        assert!(encode_shmem(&"x".repeat(249), &[1; 16], &[], &[]).is_some());
        assert!(Shmem::parse(&[0; 100]).is_err());
    }
}
