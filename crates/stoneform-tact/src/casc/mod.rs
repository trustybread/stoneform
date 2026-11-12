//! Local CASC storage (`<root>/Data/data`): the archive store the game reads
//! its data from, in the byte formats the Battle.net Agent writes. Waypoint
//! checked every layout and checksum here byte for byte against an
//! Agent-written install (Warcraft III: 48 `.idx` files, 35 archive heads,
//! `shmem`).
//!
//! ```text
//! data.###         archives of [30-byte entry header][BLTE blob verbatim from the CDN],
//!                  at most 1 GiB each, starting with 16 segment headers
//! XXYYYYYYYY.idx   16 index files (bucket XX, version YYYYYYYY): truncated
//!                  key → archive, offset, size; sorted, lookup3-checked,
//!                  then an update section the game appends to
//! shmem            index versions and the free-space table
//! ```
//!
//! Only the bytes live here; the storage writer (journaling, publishing a
//! new index version, then `shmem`, then removing the old versions) is the
//! adapter's.

mod entry_header;
mod index_file;
mod journal;
mod shmem;

pub use entry_header::{EntryHeader, SegmentHeaders, check_entry_header, entry_header, segment_headers};
pub use index_file::{
    INDEX_FILE_ALIGNMENT, IndexEntry, IndexFile, IndexUpdate, ReserveCheck, UPDATE_SECTION_ALIGNMENT,
    UPDATE_SECTION_RESERVE, check_update_reserve, encode_index_file, encode_update_record, index_file_name,
    index_file_size, parse_index_file_name,
};
pub use journal::{JOURNAL_RECORD_SIZE, Journal, JournalRecord, MARKER_KEY};
pub use shmem::{FREE_SPAN_CAPACITY, FreeSpan, SHMEM_SIZE, Shmem, encode_shmem};

/// Bits of a packed storage offset that hold the offset in the archive; the
/// archive number is above them.
pub const OFFSET_BITS: u32 = 30;

/// The size every archive stays within: 1 GiB.
pub const ARCHIVE_SIZE: u64 = 1 << OFFSET_BITS;

/// The archive numbers a storage can use are below this.
pub const MAX_ARCHIVES: u16 = 0x3FF;

/// The header in front of every blob in an archive.
pub const ENTRY_HEADER_SIZE: usize = 30;

/// The 16 segment headers every archive starts with.
pub const SEGMENT_HEADERS_SIZE: usize = 16 * ENTRY_HEADER_SIZE;

/// The biggest blob an archive can hold: one that fills it after the segment
/// headers and its own entry header.
pub const MAX_BLOB_SIZE: u64 = ARCHIVE_SIZE - SEGMENT_HEADERS_SIZE as u64 - ENTRY_HEADER_SIZE as u64;

/// Where an entry sits: an archive number (10 bits) and an offset in it (30
/// bits), which index files pack into 40 bits. Always within those limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StorageLocation {
    archive: u16,
    offset: u32,
}

impl StorageLocation {
    /// `None` when the archive number needs more than 10 bits or the offset
    /// is past the 1 GiB an archive can hold.
    pub fn new(archive: u16, offset: u64) -> Option<Self> {
        (archive <= MAX_ARCHIVES && offset < ARCHIVE_SIZE).then_some(Self { archive, offset: offset as u32 })
    }

    pub fn archive(self) -> u16 {
        self.archive
    }

    pub fn offset(self) -> u32 {
        self.offset
    }

    /// The 40-bit storage offset index files and `shmem` use.
    pub fn packed(self) -> u64 {
        u64::from(self.archive) << OFFSET_BITS | u64::from(self.offset)
    }

    /// Unpacks a 40-bit storage offset; `None` above 40 bits.
    pub fn from_packed(packed: u64) -> Option<Self> {
        Self::new(u16::try_from(packed >> OFFSET_BITS).ok()?, packed & (ARCHIVE_SIZE - 1))
    }
}

/// Whether a blob of `blob_size` bytes still fits in an archive whose data
/// ends at `archive_end`; when it doesn't, it goes into the next archive.
pub fn fits_in_archive(archive_end: u64, blob_size: u64) -> bool {
    archive_end.saturating_add(ENTRY_HEADER_SIZE as u64).saturating_add(blob_size) <= ARCHIVE_SIZE
}

/// `data.000`, `data.001`, …
pub fn archive_file_name(archive: u16) -> String {
    format!("data.{archive:03}")
}

/// The archive number of a `data.###` file name.
pub fn parse_archive_file_name(name: &str) -> Option<u16> {
    let digits = name.strip_prefix("data.")?;
    if digits.len() != 3 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_storage_locations_into_40_bits() {
        let location = StorageLocation::new(0xB, 0x39297ED4).unwrap();
        assert_eq!(location.packed(), 0xB << 30 | 0x39297ED4);
        assert_eq!(StorageLocation::from_packed(location.packed()), Some(location));
        assert_eq!(StorageLocation::new(0x3FF, ARCHIVE_SIZE - 1).map(StorageLocation::packed), Some((1 << 40) - 1));
        assert_eq!(StorageLocation::new(0x400, 0), None);
        assert_eq!(StorageLocation::new(0, ARCHIVE_SIZE), None);
        assert_eq!(StorageLocation::from_packed(1 << 40), None);
    }

    #[test]
    fn knows_the_archive_limits() {
        assert_eq!(MAX_BLOB_SIZE, (1 << 30) - 510);
        assert!(fits_in_archive(SEGMENT_HEADERS_SIZE as u64, MAX_BLOB_SIZE));
        assert!(!fits_in_archive(SEGMENT_HEADERS_SIZE as u64 + 1, MAX_BLOB_SIZE));
        assert!(!fits_in_archive(u64::MAX, 1));
    }

    #[test]
    fn names_archives() {
        assert_eq!(archive_file_name(7), "data.007");
        assert_eq!(archive_file_name(1022), "data.1022");
        assert_eq!(parse_archive_file_name("data.042"), Some(42));
        assert_eq!(parse_archive_file_name("data.42"), None);
        assert_eq!(parse_archive_file_name("data.04a"), None);
        assert_eq!(parse_archive_file_name("shmem"), None);
    }
}
