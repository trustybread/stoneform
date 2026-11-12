//! Appending blobs to a local storage, journaled so an interrupted run
//! resumes, and publishing them the way the Agent does.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use stoneform_tact::casc::{
    ARCHIVE_SIZE, ENTRY_HEADER_SIZE, IndexEntry, JOURNAL_RECORD_SIZE, JournalRecord, MAX_ARCHIVES, MAX_BLOB_SIZE,
    SEGMENT_HEADERS_SIZE, StorageLocation, archive_file_name, encode_index_file, encode_shmem, entry_header,
    index_file_name, segment_headers,
};
use stoneform_tact::{EncodingKey, TruncatedKey};

use crate::error::{At, Error, Result};
use crate::fsutil::{sync_dir, write_atomically};
use crate::journal::{self, JOURNAL_FILE_NAME, JournalHead};
use crate::lock::StorageLock;
use crate::storage::{StorageState, StoredKeys, archive_path, load_bucket, scan};

/// Unsynced bytes after which archive and journal are flushed to disk, so a
/// power loss costs at most this much of a download.
const SYNC_INTERVAL: u64 = 64 << 20;

/// The empty file the Agent keeps next to the index files.
const INDEX_LOCK_FILE_NAME: &str = "index.lock.0";

const SHMEM_FILE_NAME: &str = "shmem";

/// Writes a local storage one blob at a time: a new one from scratch, or
/// more blobs into a finished one (an update, or another product sharing
/// it), whoever wrote it.
///
/// Every blob is journaled as it is written, so an interrupted run picks up
/// where it stopped: opening the folder again replays the journal and trims
/// the archive being written to what it records. Existing data is never
/// moved or rewritten; new blobs go after the end of the last archive, then
/// into new ones. [`StorageWriter::finish`] writes every index file as its
/// next version, then `shmem`, and only then removes the old versions, so
/// the game always finds a complete set.
///
/// Memory stays small however big the storage: no per-blob state beyond the
/// sorted 9-byte keys a resumed run had written and (updating) those the
/// storage held. Index files are built from the journal at the end, one
/// bucket at a time. Like Waypoint's, the writer doesn't remember the keys
/// it appended this run: callers pass each key once (install plans are
/// deduplicated), and [`StorageWriter::contains`] answers for what was there
/// before.
///
/// Holds the folder's [`StorageLock`] until finished or dropped. Dropping it
/// unfinished is a pause: the journal stays for the next run.
pub struct StorageWriter {
    dir: PathBuf,
    journal_path: PathBuf,
    head: JournalHead,
    is_update: bool,
    /// The index versions the storage had when opened (all 0 for a new one).
    versions: [u32; 16],
    /// Updating: what the storage held when opened.
    existing: Option<StoredKeys>,
    /// Sorted: the keys an interrupted run had written.
    resumed: Vec<TruncatedKey>,
    resumed_bytes: u64,
    appended: usize,
    appended_bytes: u64,
    /// Archives from here on are this writer's, with segment headers to index.
    first_new_archive: u16,
    current: Option<OpenArchive>,
    archive_sizes: Vec<u64>,
    /// [`ARCHIVE_SIZE`]; smaller in tests, to fill archives quickly.
    archive_capacity: u64,
    journal: File,
    journal_len: u64,
    unsynced: u64,
    _lock: StorageLock,
}

/// The archive blobs are appended to.
#[derive(Debug)]
struct OpenArchive {
    number: u16,
    end: u64,
    file: File,
}

/// What [`StorageWriter::finish`] published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The index files' new versions, by bucket.
    pub index_versions: [u32; 16],
    /// The keys the storage holds, segment headers included.
    pub keys: usize,
}

impl StorageWriter {
    pub(crate) fn open(dir: &Path, allow_existing: bool) -> Result<Self> {
        // `shmem` records the folder's path, which must be absolute.
        let dir = std::path::absolute(dir).at(dir)?;
        fs::create_dir_all(&dir).at(&dir)?;
        let lock = StorageLock::acquire(&dir)?;
        let journal_path = dir.join(JOURNAL_FILE_NAME);
        let head = match journal::read_head(&journal_path)? {
            Some(head) if head.update_start.is_none() => match scan(&dir)? {
                // A markerless journal beside 16 published index files: a
                // first install that crashed after publishing, or a finished
                // storage next to someone's stale journal. The index files
                // are authoritative; resuming as a new storage would rewrite
                // data.000 and drop their entries. Continue as an update.
                Some(state) => {
                    if !allow_existing {
                        return Err(Error::AlreadyExists(dir));
                    }
                    let head = rebased_head(&dir, &state)?;
                    start_journal(&dir, &journal_path, &head)?;
                    tracing::info!(path = %dir.display(), "stale new-storage journal rebased onto the index files");
                    head
                }
                None => head,
            },
            Some(head) => head,
            None => {
                let head = match scan(&dir)? {
                    Some(state) => {
                        if !allow_existing {
                            return Err(Error::AlreadyExists(dir));
                        }
                        let base_key = match state.base_key {
                            Some(key) => key,
                            None => random_key(&dir)?,
                        };
                        update_head(&dir, &state, base_key)?
                    }
                    None => JournalHead { base_key: random_key(&dir)?, update_start: None },
                };
                start_journal(&dir, &journal_path, &head)?;
                head
            }
        };
        let journal = OpenOptions::new().read(true).write(true).open(&journal_path).at(&journal_path)?;

        let mut writer = Self {
            dir,
            journal_path,
            head,
            is_update: head.update_start.is_some(),
            versions: [0; 16],
            existing: None,
            resumed: Vec::new(),
            resumed_bytes: 0,
            appended: 0,
            appended_bytes: 0,
            first_new_archive: 0,
            current: None,
            archive_sizes: Vec::new(),
            archive_capacity: ARCHIVE_SIZE,
            journal,
            journal_len: 0,
            unsynced: 0,
            _lock: lock,
        };
        let mut start = None;
        let mut published = None;
        if let Some((archive, offset)) = head.update_start {
            if archive >= MAX_ARCHIVES || offset > ARCHIVE_SIZE {
                return Err(Error::damaged(&writer.dir, "the journal's update marker is out of range"));
            }
            let state = scan(&writer.dir)?
                .ok_or_else(|| Error::damaged(&writer.dir, "the storage this update adds to is gone"))?;
            // finish() may have published only some buckets before a crash.
            // Then a journal that turns out shorter must never cut off bytes
            // those buckets already point at.
            let mut beyond_start = false;
            let sizes = state.archive_sizes.clone();
            let keys = StoredKeys::load(&writer.dir, state, |entry| {
                let (at, end) = (entry.location.archive(), u64::from(entry.location.offset()) + u64::from(entry.size));
                beyond_start |= at > archive || (at == archive && end > offset);
            })?;
            if beyond_start {
                published = Some(sizes.clone());
            }
            writer.versions = *keys.versions();
            writer.existing = Some(keys);
            writer.archive_sizes = sizes;
            writer.archive_sizes.truncate(usize::from(archive) + 1);
            set_archive_size(&mut writer.archive_sizes, archive, offset);
            writer.first_new_archive = archive + 1;
            start = Some((archive, offset));
        }
        writer.replay(start, published)?;
        Ok(writer)
    }

    /// The folder written to (absolute).
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Whether this adds to a storage that existed before.
    pub fn is_update(&self) -> bool {
        self.is_update
    }

    /// Whether the storage holds `key` already (by its first 9 bytes, as the
    /// index files do): stored before this writer opened, or by the run it
    /// resumed.
    pub fn contains(&self, key: &EncodingKey) -> bool {
        let key = key.truncated();
        self.resumed.binary_search(&key).is_ok()
            || self.existing.as_ref().is_some_and(|keys| keys.contains_truncated(&key))
    }

    /// Keys stored, counting what the storage already had (with its segment
    /// headers) and what the resumed run wrote.
    pub fn key_count(&self) -> usize {
        self.resumed.len() + self.appended + self.existing.as_ref().map_or(0, StoredKeys::len)
    }

    /// Blob bytes (without entry headers) the resumed run had journaled.
    pub fn resumed_bytes(&self) -> u64 {
        self.resumed_bytes
    }

    /// Blob bytes (without entry headers) appended by this writer.
    pub fn appended_bytes(&self) -> u64 {
        self.appended_bytes
    }

    /// Appends one blob, exactly as the CDN served it (BLTE). `full_key`
    /// keeps all 16 key bytes in the entry header, as the Agent writes them
    /// for build-config files (encoding table, manifests). Returns `false`
    /// without writing when the storage holds the key already.
    ///
    /// A failed write (a full disk) leaves nothing behind, so the call can be
    /// retried, or the install resumed later.
    pub fn append(&mut self, key: &EncodingKey, blob: &[u8], full_key: bool) -> Result<bool> {
        if self.contains(key) {
            return Ok(false);
        }
        if blob.len() as u64 > MAX_BLOB_SIZE {
            return Err(Error::BlobTooLarge(blob.len()));
        }
        let size = (ENTRY_HEADER_SIZE + blob.len()) as u64;
        if self.current.as_ref().is_none_or(|archive| archive.end + size > self.archive_capacity) {
            self.start_archive()?;
        }
        let Some(archive) = self.current.as_mut() else {
            return Err(Error::damaged(&self.dir, "no archive to append to"));
        };
        let path = archive_path(&self.dir, archive.number);
        let location = StorageLocation::new(archive.number, archive.end)
            .ok_or_else(|| Error::damaged(&self.dir, "append position out of range"))?;
        let header = entry_header(key, blob.len() as u32, location, 0, full_key).at(&path)?;
        let record = JournalRecord::Blob {
            key: *key,
            archive: archive.number,
            offset: archive.end,
            size: size as u32,
            full_key,
        };

        // Archive first, then the journal record that vouches for it.
        let written = archive
            .file
            .write_all_at(&header, archive.end)
            .and_then(|()| archive.file.write_all_at(blob, archive.end + ENTRY_HEADER_SIZE as u64))
            .at(&path)
            .and_then(|()| self.journal.write_all_at(&record.encode(), self.journal_len).at(&self.journal_path));
        if let Err(error) = written {
            // Cut off the partial write, or the next blob would land after it
            // while the index points before it.
            let _ = archive.file.set_len(archive.end);
            let _ = self.journal.set_len(self.journal_len);
            return Err(error);
        }
        archive.end += size;
        self.journal_len += JOURNAL_RECORD_SIZE as u64;
        self.appended += 1;
        self.appended_bytes += blob.len() as u64;
        self.unsynced += size;
        if self.unsynced > SYNC_INTERVAL {
            self.sync()?;
        }
        Ok(true)
    }

    /// Publishes what was written: the next version of every index file (1
    /// for a new storage) with the new entries, then `shmem`, then removes
    /// the old index files and the journal. The storage is the game's to
    /// read afterwards. A failure leaves the journal, and opening the folder
    /// again finishes the job.
    pub fn finish(mut self) -> Result<Finished> {
        let last = self.seal()?;
        let versions = self.versions.map(|version| version.wrapping_add(1));
        self.publish_indexes(last, &versions, 16)?;
        sync_dir(&self.dir)?;
        self.publish_shmem(&versions)?;
        sync_dir(&self.dir)?;
        self.remove_old_indexes(&versions)?;
        let index_lock = self.dir.join(INDEX_LOCK_FILE_NAME);
        OpenOptions::new().write(true).create(true).truncate(false).open(&index_lock).at(&index_lock)?;
        fs::remove_file(&self.journal_path).at(&self.journal_path)?;
        sync_dir(&self.dir)?;
        let keys = self.key_count();
        Ok(Finished { index_versions: versions, keys })
    }

    /// Rebuilds the state from the journal. Data after the last trusted blob
    /// is cut off (for an update with nothing journaled yet, everything after
    /// the storage's old end), and the next write continues right there.
    fn replay(&mut self, start: Option<(u16, u64)>, published: Option<Vec<u64>>) -> Result<()> {
        let records = journal::blob_records(&self.journal_path, &self.head)?;
        let mut sizes = std::mem::take(&mut self.archive_sizes);
        let mut resumed = Vec::new();
        let mut resumed_bytes = 0;
        let mut last = start;
        let trusted = journal::trusted_records(
            &self.dir,
            records,
            start.map(|(archive, _)| archive),
            &self.journal_path,
            |blob| {
                let archive = usize::from(blob.archive());
                if sizes.len() <= archive {
                    sizes.resize(archive + 1, SEGMENT_HEADERS_SIZE as u64);
                }
                sizes[archive] = sizes[archive].max(blob.end());
                resumed.push(blob.entry.key);
                resumed_bytes += blob.blob_size();
                last = Some((blob.archive(), blob.end()));
            },
        )?;
        resumed.sort_unstable();
        self.resumed = resumed;
        self.resumed_bytes = resumed_bytes;

        // Forget records whose data never reached disk.
        self.journal_len = self.head.records_start() + (trusted * JOURNAL_RECORD_SIZE) as u64;
        self.journal.set_len(self.journal_len).at(&self.journal_path)?;

        if let Some(published) = published {
            if sizes.len() < published.len() {
                sizes.resize(published.len(), 0);
            }
            for (size, &published) in sizes.iter_mut().zip(&published) {
                *size = (*size).max(published);
            }
            if let Some(newest) = published.iter().rposition(|&size| size > 0) {
                let newest_end = published[newest];
                let newest = newest as u16;
                last = match last {
                    Some((archive, end)) if archive == newest => Some((archive, end.max(newest_end))),
                    Some((archive, end)) if archive > newest => Some((archive, end)),
                    _ => Some((newest, newest_end)),
                };
            }
        }
        self.archive_sizes = sizes;
        if let Some((number, end)) = last {
            // Drop unjournaled bytes, but never what a published index uses.
            let path = archive_path(&self.dir, number);
            let file = OpenOptions::new().write(true).open(&path).at(&path)?;
            file.set_len(end).at(&path)?;
            self.current = Some(OpenArchive { number, end, file });
        }
        Ok(())
    }

    /// Starts the next archive with its segment headers. The writer's state
    /// changes only once that all succeeded.
    fn start_archive(&mut self) -> Result<()> {
        let next = self.current.as_ref().map_or(0, |archive| archive.number + 1);
        if next >= MAX_ARCHIVES {
            return Err(Error::StorageFull(self.dir.clone()));
        }
        if let Some(archive) = &self.current {
            archive.file.sync_all().at(&archive_path(&self.dir, archive.number))?;
        }
        let path = archive_path(&self.dir, next);
        let headers = segment_headers(&self.head.base_key, next).at(&path)?;
        // Replaces whatever an interrupted run may have left there.
        let file =
            File::create(&path).and_then(|file| file.write_all_at(&headers.bytes, 0).map(|()| file)).at(&path)?;
        if let Some(archive) = self.current.take() {
            set_archive_size(&mut self.archive_sizes, archive.number, archive.end);
        }
        let end = SEGMENT_HEADERS_SIZE as u64;
        set_archive_size(&mut self.archive_sizes, next, end);
        self.current = Some(OpenArchive { number: next, end, file });
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        if let Some(archive) = &self.current {
            archive.file.sync_all().at(&archive_path(&self.dir, archive.number))?;
        }
        self.journal.sync_all().at(&self.journal_path)?;
        self.unsynced = 0;
        Ok(())
    }

    /// Flushes and closes the archive, records its size, and removes
    /// archives past it (an interrupted run can leave a started one behind).
    /// Returns the last archive.
    fn seal(&mut self) -> Result<Option<u16>> {
        self.sync()?;
        let last = self.current.take().map(|archive| {
            set_archive_size(&mut self.archive_sizes, archive.number, archive.end);
            archive.number
        });
        let mut extra = last.map_or(0, |archive| archive + 1);
        loop {
            let path = archive_path(&self.dir, extra);
            match fs::remove_file(&path) {
                Ok(()) => extra += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) => return Err(error).at(&path),
            }
        }
        self.archive_sizes.truncate(usize::from(last.map_or(0, |archive| archive + 1)));
        Ok(last)
    }

    /// Writes the next version of the first `buckets` index files: what was
    /// there, this writer's segment headers, and the journal's blobs, which
    /// supersede older entries of their keys.
    fn publish_indexes(&self, last: Option<u16>, versions: &[u32; 16], buckets: u8) -> Result<()> {
        let mut segments: [Vec<IndexEntry>; 16] = Default::default();
        if let Some(last) = last {
            for archive in self.first_new_archive..=last {
                let headers = segment_headers(&self.head.base_key, archive).at(&archive_path(&self.dir, archive))?;
                for (bucket, entry) in segments.iter_mut().zip(headers.entries) {
                    bucket.push(entry);
                }
            }
        }
        for bucket in 0..buckets {
            let index = usize::from(bucket);
            let mut entries =
                if self.is_update { load_bucket(&self.dir, bucket, self.versions[index])? } else { Vec::new() };
            entries.extend_from_slice(&segments[index]);
            let mut additions = Vec::new();
            for record in journal::blob_records(&self.journal_path, &self.head)? {
                let record = record.at(&self.journal_path)?;
                if let Some(entry) = record.index_entry().filter(|entry| entry.key.bucket() == bucket) {
                    additions.push(entry);
                }
            }
            let replaced: HashSet<TruncatedKey> = additions.iter().map(|entry| entry.key).collect();
            entries.retain(|entry| !replaced.contains(&entry.key));
            entries.extend(additions);
            let mut seen = HashSet::with_capacity(entries.len());
            entries.retain(|entry| seen.insert((entry.key, entry.location)));
            let path = self.dir.join(index_file_name(bucket, versions[index]));
            write_atomically(&path, &encode_index_file(bucket, &entries))?;
        }
        Ok(())
    }

    fn publish_shmem(&self, versions: &[u32; 16]) -> Result<()> {
        let path = self.dir.join(SHMEM_FILE_NAME);
        match self.dir.to_str().and_then(|dir| encode_shmem(dir, versions, &self.archive_sizes, &[])) {
            Some(shmem) => write_atomically(&path, &shmem),
            // The path doesn't fit shmem's field. The game rebuilds a missing
            // shmem; an old one would name index versions about to go.
            None => {
                tracing::warn!(path = %self.dir.display(), "storage path doesn't fit shmem; leaving it to the game");
                match fs::remove_file(&path) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error).at(&path),
                    _ => Ok(()),
                }
            }
        }
    }

    fn remove_old_indexes(&self, versions: &[u32; 16]) -> Result<()> {
        let current: HashSet<String> =
            (0u8..).zip(versions).map(|(bucket, &version)| index_file_name(bucket, version)).collect();
        for item in fs::read_dir(&self.dir).at(&self.dir)? {
            let name = item.at(&self.dir)?.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".idx") && !current.contains(name) {
                let path = self.dir.join(name);
                if let Err(error) = fs::remove_file(&path) {
                    tracing::warn!(%error, path = %path.display(), "old index file left behind");
                }
            }
        }
        Ok(())
    }

    /// Fills archives after `capacity` bytes instead of 1 GiB.
    #[cfg(test)]
    pub(crate) fn with_archive_capacity(mut self, capacity: u64) -> Self {
        self.archive_capacity = capacity;
        self
    }

    /// Stops `finish` after publishing the first `buckets` index files, as a
    /// crash there would.
    #[cfg(test)]
    pub(crate) fn crash_while_finishing(mut self, buckets: u8) -> Result<()> {
        let last = self.seal()?;
        let versions = self.versions.map(|version| version.wrapping_add(1));
        self.publish_indexes(last, &versions, buckets)
    }
}

impl fmt::Debug for StorageWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageWriter")
            .field("dir", &self.dir)
            .field("is_update", &self.is_update)
            .field("versions", &self.versions)
            .field("resumed", &self.resumed.len())
            .field("appended", &self.appended)
            .field("current", &self.current.as_ref().map(|archive| (archive.number, archive.end)))
            .finish_non_exhaustive()
    }
}

/// Records an archive's size, growing the table to it.
fn set_archive_size(sizes: &mut Vec<u64>, archive: u16, size: u64) {
    let index = usize::from(archive);
    if sizes.len() <= index {
        sizes.resize(index + 1, 0);
    }
    sizes[index] = size;
}

/// The update journal's head for adding to a finished storage: appending
/// starts at the end of its last archive. Waypoint allowed one shorter than
/// its segment headers; blobs would land on them, so that is refused.
fn update_head(dir: &Path, state: &StorageState, base_key: [u8; 16]) -> Result<JournalHead> {
    let last =
        state.archive_sizes.len().checked_sub(1).ok_or_else(|| Error::damaged(dir, "index files without archives"))?;
    let end = state.archive_sizes[last];
    if end < SEGMENT_HEADERS_SIZE as u64 {
        return Err(Error::damaged(
            dir,
            format!("{} is shorter than its segment headers", archive_file_name(last as u16)),
        ));
    }
    Ok(JournalHead { base_key, update_start: Some((last as u16, end)) })
}

/// An update journal's head for a storage whose index files are published:
/// appending resumes at the end of its last archive.
fn rebased_head(dir: &Path, state: &StorageState) -> Result<JournalHead> {
    let cannot = || Error::damaged(dir, "a stale journal can't be safely resumed; use Battle.net's Scan and Repair");
    let base_key = state.base_key.ok_or_else(cannot)?;
    let head = update_head(dir, state, base_key).map_err(|_| cannot())?;
    // The index files must load before the old journal is replaced.
    StoredKeys::load(dir, state.clone(), |_| {})?;
    Ok(head)
}

/// Writes a new journal whole, so it never exists with half a marker.
fn start_journal(dir: &Path, path: &Path, head: &JournalHead) -> Result<()> {
    write_atomically(path, &head.encode())?;
    sync_dir(dir)
}

/// A new storage's segment-header base key: random, as the Agent picks it.
fn random_key(dir: &Path) -> Result<[u8; 16]> {
    let mut key = [0u8; 16];
    getrandom::fill(&mut key).map_err(io::Error::other).at(dir)?;
    Ok(key)
}
