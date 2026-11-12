//! The writer's resume journal on disk: reading its head, streaming its
//! records, and deciding which of them to trust.
//!
//! The byte format is `stoneform_tact::casc::Journal`'s. Records are read
//! one at a time rather than mapped: a WoW install journals about two million
//! blobs (60 MB), and nothing here needs them all at once.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use stoneform_tact::EncodingKey;
use stoneform_tact::casc::{
    ENTRY_HEADER_SIZE, IndexEntry, JOURNAL_RECORD_SIZE, Journal, JournalRecord, StorageLocation,
};

use crate::error::{At, Result};
use crate::fsutil::file_len;
use crate::storage::archive_path;

/// The journal's name in `Data/data`. Waypoint's, on purpose: the format is
/// the same, so either tool resumes what the other was interrupted in, and
/// neither mistakes the other's unfinished first install for a broken
/// storage.
pub const JOURNAL_FILE_NAME: &str = ".waypoint-journal";

const BASE_KEY_SIZE: u64 = 16;
const RECORD_SIZE: u64 = JOURNAL_RECORD_SIZE as u64;

/// What an interrupted writer left in a storage folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnfinishedSession {
    /// A first install that hasn't published its index files (or, next to
    /// 16 published ones, a stale journal the writer rebases onto them).
    NewStorage,
    /// More blobs for a finished storage: an update, or another product
    /// joining a shared storage.
    Update,
}

/// The start of a journal: the storage's base key and, for an update, where
/// appending began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JournalHead {
    pub base_key: [u8; 16],
    pub update_start: Option<(u16, u64)>,
}

impl JournalHead {
    /// Where the blob records start.
    pub fn records_start(&self) -> u64 {
        BASE_KEY_SIZE + RECORD_SIZE * u64::from(self.update_start.is_some())
    }

    /// The journal's first bytes, to write when starting one.
    pub fn encode(&self) -> Vec<u8> {
        Journal::start(&self.base_key, self.update_start)
    }

    pub fn session(&self) -> UnfinishedSession {
        if self.update_start.is_some() { UnfinishedSession::Update } else { UnfinishedSession::NewStorage }
    }
}

/// The journal's head; `None` without a journal or without a whole base key,
/// which the writer ignores and starts over from. A marker cut off inside
/// its record reads as no marker, as in Waypoint: next to published index
/// files the writer rebases such a journal onto them.
pub(crate) fn read_head(path: &Path) -> Result<Option<JournalHead>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).at(path),
    };
    let mut head = Vec::with_capacity(BASE_KEY_SIZE as usize + JOURNAL_RECORD_SIZE);
    (&mut file).take(BASE_KEY_SIZE + RECORD_SIZE).read_to_end(&mut head).at(path)?;
    Ok(Journal::parse(&head)
        .map(|journal| JournalHead { base_key: *journal.base_key(), update_start: journal.update_start() }))
}

/// The blob records after `head`, oldest first; a partly written last record
/// is left out.
pub(crate) fn blob_records(path: &Path, head: &JournalHead) -> Result<BlobRecords> {
    let mut file = File::open(path).at(path)?;
    file.seek(SeekFrom::Start(head.records_start())).at(path)?;
    Ok(BlobRecords { reader: BufReader::with_capacity(1 << 20, file) })
}

#[derive(Debug)]
pub(crate) struct BlobRecords {
    reader: BufReader<File>,
}

impl Iterator for BlobRecords {
    type Item = io::Result<JournalRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut record = [0u8; JOURNAL_RECORD_SIZE];
        match self.reader.read_exact(&mut record) {
            Ok(()) => Some(Ok(JournalRecord::decode(&record))),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => None,
            Err(error) => Some(Err(error)),
        }
    }
}

/// A journaled blob that resuming keeps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TrustedBlob {
    pub key: EncodingKey,
    pub entry: IndexEntry,
}

impl TrustedBlob {
    pub fn archive(&self) -> u16 {
        self.entry.location.archive()
    }

    /// Where its entry ends in the archive.
    pub fn end(&self) -> u64 {
        u64::from(self.entry.location.offset()) + u64::from(self.entry.size)
    }

    /// The blob's size without its entry header.
    pub fn blob_size(&self) -> u64 {
        u64::from(self.entry.size) - ENTRY_HEADER_SIZE as u64
    }
}

/// Walks `records` while they can be trusted: each one's bytes made it into
/// its archive (the file reaches the record's end), and archives only go
/// forward from `from` (an update's first archive). Calls `body` for each
/// and returns how many it walked; the rest the writer cuts off, or new
/// writes would land after them and a later replay would trust overwritten
/// bytes.
pub(crate) fn trusted_records(
    dir: &Path,
    records: impl Iterator<Item = io::Result<JournalRecord>>,
    from: Option<u16>,
    journal: &Path,
    mut body: impl FnMut(&TrustedBlob),
) -> Result<usize> {
    let mut current = from.unwrap_or(0);
    let mut archive_sizes: HashMap<u16, u64> = HashMap::new();
    let mut trusted = 0;
    for record in records {
        let JournalRecord::Blob { key, archive, offset, size, .. } = record.at(journal)? else { break };
        let Some(location) = StorageLocation::new(archive, offset) else { break };
        if archive < current || (size as usize) < ENTRY_HEADER_SIZE {
            break;
        }
        let archive_size = match archive_sizes.get(&archive) {
            Some(&size) => size,
            None => {
                let size = file_len(&archive_path(dir, archive))?.unwrap_or(0);
                *archive_sizes.entry(archive).or_insert(size)
            }
        };
        let blob = TrustedBlob { key, entry: IndexEntry { key: key.truncated(), location, size } };
        if blob.end() > archive_size {
            break;
        }
        current = archive;
        body(&blob);
        trusted += 1;
    }
    Ok(trusted)
}

/// What [`trusted_records`] keeps of the journal in `dir`, read only: the
/// keys a resumed run won't download again.
pub(crate) fn for_each_trusted(dir: &Path, body: impl FnMut(&TrustedBlob)) -> Result<()> {
    let path = dir.join(JOURNAL_FILE_NAME);
    let Some(head) = read_head(&path)? else { return Ok(()) };
    let records = blob_records(&path, &head)?;
    trusted_records(dir, records, head.update_start.map(|(archive, _)| archive), &path, body)?;
    Ok(())
}
