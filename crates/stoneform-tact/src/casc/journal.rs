//! The storage writer's resume journal: what an interrupted install or
//! update had written, so the next run picks up where it stopped.
//!
//! ```text
//! base key (16)      the storage's segment-header base key
//! records (31 each): key (16), archive (u16 LE), offset (u64 LE),
//!                    size: header + blob (u32 LE), flag (0 truncated key,
//!                    1 full key, 2 update marker)
//! ```
//!
//! An update's journal starts with a marker record (key all `0xFF`, flag 2)
//! holding where appending began: the end of the storage before it. A
//! partly written last record is ignored. Which records to trust (their bytes
//! reached the archive, archives only go forward) depends on the archives'
//! sizes, which only the writer can see.

use super::{IndexEntry, StorageLocation};
use crate::bytes::le_uint;
use crate::{EncodingKey, TruncatedKey};

pub const JOURNAL_RECORD_SIZE: usize = 16 + 2 + 8 + 4 + 1;

/// The key of an update's first record.
pub const MARKER_KEY: [u8; 16] = [0xFF; 16];

const BASE_KEY_SIZE: usize = 16;
const MARKER_FLAG: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalRecord {
    /// Where an update started appending: the end of the storage before it.
    Marker { archive: u16, offset: u64 },
    /// A blob written at `archive`/`offset`, `size` bytes with its header.
    Blob { key: EncodingKey, archive: u16, offset: u64, size: u32, full_key: bool },
}

impl JournalRecord {
    pub fn encode(&self) -> [u8; JOURNAL_RECORD_SIZE] {
        let (key, archive, offset, size, flag) = match *self {
            JournalRecord::Marker { archive, offset } => (MARKER_KEY, archive, offset, 0, MARKER_FLAG),
            JournalRecord::Blob { key, archive, offset, size, full_key } => {
                (*key.as_bytes(), archive, offset, size, u8::from(full_key))
            }
        };
        let mut out = [0u8; JOURNAL_RECORD_SIZE];
        out[..16].copy_from_slice(&key);
        out[16..18].copy_from_slice(&archive.to_le_bytes());
        out[18..26].copy_from_slice(&offset.to_le_bytes());
        out[26..30].copy_from_slice(&size.to_le_bytes());
        out[30] = flag;
        out
    }

    pub fn decode(record: &[u8; JOURNAL_RECORD_SIZE]) -> Self {
        let archive = le_uint(&record[16..18]) as u16;
        let offset = le_uint(&record[18..26]);
        if record[..16] == MARKER_KEY && record[30] == MARKER_FLAG {
            return JournalRecord::Marker { archive, offset };
        }
        let mut key = [0u8; 16];
        key.copy_from_slice(&record[..16]);
        JournalRecord::Blob {
            key: EncodingKey::new(key),
            archive,
            offset,
            size: le_uint(&record[26..30]) as u32,
            full_key: record[30] == 1,
        }
    }

    /// A blob's index entry; `None` for a marker or a position no archive has.
    pub fn index_entry(&self) -> Option<IndexEntry> {
        match *self {
            JournalRecord::Marker { .. } => None,
            JournalRecord::Blob { key, archive, offset, size, .. } => {
                Some(IndexEntry { key: key.truncated(), location: StorageLocation::new(archive, offset)?, size })
            }
        }
    }

    /// A blob's truncated key, which local storage indexes it by.
    pub fn truncated_key(&self) -> Option<TruncatedKey> {
        match self {
            JournalRecord::Marker { .. } => None,
            JournalRecord::Blob { key, .. } => Some(key.truncated()),
        }
    }
}

/// A journal file's contents, read in place.
#[derive(Debug, Clone, Copy)]
pub struct Journal<'a> {
    base_key: [u8; 16],
    records: &'a [u8],
}

impl<'a> Journal<'a> {
    /// `None` without a whole base key: the writer starts over then.
    pub fn parse(data: &'a [u8]) -> Option<Self> {
        let base_key = data.get(..BASE_KEY_SIZE)?.try_into().ok()?;
        Some(Self { base_key, records: &data[BASE_KEY_SIZE..] })
    }

    /// The start of a new journal: the storage's base key, then (for an
    /// update) the marker of where appending begins.
    pub fn start(base_key: &[u8; 16], update_from: Option<(u16, u64)>) -> Vec<u8> {
        let mut out = base_key.to_vec();
        if let Some((archive, offset)) = update_from {
            out.extend(JournalRecord::Marker { archive, offset }.encode());
        }
        out
    }

    pub fn base_key(&self) -> &[u8; 16] {
        &self.base_key
    }

    /// For an update's journal: where appending began. `None` for a new
    /// storage's journal, and for one cut off inside its first record, which
    /// may be an update whose marker never fully reached the disk: check
    /// [`Journal::first_record_is_partial`] before treating it as new.
    pub fn update_start(&self) -> Option<(u16, u64)> {
        match self.record(0)? {
            JournalRecord::Marker { archive, offset } => Some((archive, offset)),
            JournalRecord::Blob { .. } => None,
        }
    }

    /// Whether the journal ends inside its first record, so it can't be told
    /// whether it's an update's.
    pub fn first_record_is_partial(&self) -> bool {
        (1..JOURNAL_RECORD_SIZE).contains(&self.records.len())
    }

    /// The blob records, oldest first, without the marker; a partly written
    /// last record is left out.
    pub fn blobs(&self) -> impl Iterator<Item = JournalRecord> + use<'a> {
        let journal = *self;
        let skip = usize::from(journal.update_start().is_some());
        (skip..journal.records.len() / JOURNAL_RECORD_SIZE).filter_map(move |index| journal.record(index))
    }

    /// The journal's length when only its first `trusted` blob records are
    /// kept: what to truncate it to before appending again.
    pub fn trusted_len(&self, trusted: usize) -> usize {
        BASE_KEY_SIZE + (usize::from(self.update_start().is_some()) + trusted) * JOURNAL_RECORD_SIZE
    }

    fn record(&self, index: usize) -> Option<JournalRecord> {
        let at = index * JOURNAL_RECORD_SIZE;
        let bytes = self.records.get(at..at + JOURNAL_RECORD_SIZE)?;
        bytes.try_into().ok().map(JournalRecord::decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(i: u8, offset: u64) -> JournalRecord {
        JournalRecord::Blob { key: EncodingKey::new([i; 16]), archive: 1, offset, size: 130, full_key: i == 0 }
    }

    #[test]
    fn records_round_trip_byte_for_byte() {
        let record = blob(0x5A, 0x1E0);
        let bytes = record.encode();
        let expected = [[0x5A; 16].as_slice(), &[1, 0], &[0xE0, 1, 0, 0, 0, 0, 0, 0], &[130, 0, 0, 0], &[0]].concat();
        assert_eq!(bytes.to_vec(), expected);
        assert_eq!(JournalRecord::decode(&bytes), record);

        let marker = JournalRecord::Marker { archive: 3, offset: 0x1234 };
        let bytes = marker.encode();
        assert_eq!(bytes[..16], MARKER_KEY);
        assert_eq!(bytes[30], 2);
        assert_eq!(JournalRecord::decode(&bytes), marker);
        assert_eq!(marker.index_entry(), None);

        let entry = record.index_entry().unwrap();
        assert_eq!(
            (entry.key, entry.location.packed(), entry.size),
            (EncodingKey::new([0x5A; 16]).truncated(), 1 << 30 | 0x1E0, 130)
        );
    }

    #[test]
    fn reads_a_new_storage_journal() {
        let mut data = Journal::start(&[7; 16], None);
        data.extend(blob(1, 480).encode());
        data.extend(blob(2, 610).encode());
        data.extend(&blob(3, 740).encode()[..10]); // a record cut short
        let journal = Journal::parse(&data).unwrap();
        assert_eq!(journal.base_key(), &[7; 16]);
        assert_eq!(journal.update_start(), None);
        assert_eq!(journal.blobs().collect::<Vec<_>>(), [blob(1, 480), blob(2, 610)]);
        assert_eq!(journal.trusted_len(1), 16 + 31);
        assert!(Journal::parse(&[0; 15]).is_none());
    }

    #[test]
    fn reads_an_update_journal() {
        let mut data = Journal::start(&[7; 16], Some((2, 5000)));
        assert_eq!(data.len(), 16 + 31);
        data.extend(blob(1, 5000).encode());
        let journal = Journal::parse(&data).unwrap();
        assert_eq!(journal.update_start(), Some((2, 5000)));
        assert_eq!(journal.blobs().collect::<Vec<_>>(), [blob(1, 5000)]);
        assert_eq!(journal.trusted_len(0), 16 + 31);
        assert!(!journal.first_record_is_partial());
        // A marker that never fully reached the disk.
        let partial = Journal::parse(&data[..16 + 20]).unwrap();
        assert!(partial.first_record_is_partial() && partial.update_start().is_none());
    }
}
