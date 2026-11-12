//! A local storage folder (`Data/data`) as it is on disk: what it holds,
//! reading blobs back, and the repairs that touch no game data.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use stoneform_tact::casc::{
    ENTRY_HEADER_SIZE, EntryHeader, IndexEntry, IndexFile, ReserveCheck, SEGMENT_HEADERS_SIZE, archive_file_name,
    check_update_reserve, entry_header, index_file_name, parse_archive_file_name, parse_index_file_name,
};
use stoneform_tact::{EncodingKey, TruncatedKey};

use crate::error::{At, Error, Result};
use crate::fsutil::{file_len, read_if_exists, sync_dir, write_atomically};
use crate::journal::{self, JOURNAL_FILE_NAME, UnfinishedSession};
use crate::lock::StorageLock;
use crate::writer::StorageWriter;

/// A local storage folder: `<install root>/Data/data`.
///
/// Reading through it takes no lock: a snapshot can be outdated by a writer
/// a moment later, which is what [`StoredKeys::versions`] and
/// [`StoredKeys::archive_sizes`] let a caller notice. Writing goes through a
/// [`StorageWriter`], which holds the folder's lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascStorage {
    dir: PathBuf,
}

/// A storage's shape, without its entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageState {
    /// The newest index file version per bucket.
    pub versions: [u32; 16],
    /// The size of every `data.###` file by archive number, up to the
    /// highest one; 0 for a missing one.
    pub archive_sizes: Vec<u64>,
    /// The segment-header base key, from the start of `data.000`; `None`
    /// without that file.
    pub base_key: Option<[u8; 16]>,
}

impl CascStorage {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Starts writing: a new storage, an interrupted run resumed from its
    /// journal, or more blobs for a finished storage (an update, or another
    /// product sharing it). See [`StorageWriter`].
    pub fn open_writer(&self) -> Result<StorageWriter> {
        StorageWriter::open(&self.dir, true)
    }

    /// Like [`CascStorage::open_writer`], but a finished storage is an error
    /// ([`Error::AlreadyExists`]) rather than something to add to: for a
    /// first install that must not write over one.
    pub fn create_writer(&self) -> Result<StorageWriter> {
        StorageWriter::open(&self.dir, false)
    }

    /// The storage's shape; `None` when the folder holds no storage, or only
    /// a new one still being written (archives and a journal, not yet 16
    /// index files). Missing index files without that journal mean a broken
    /// storage, which is an error.
    pub fn scan(&self) -> Result<Option<StorageState>> {
        scan(&self.dir)
    }

    /// The keys the storage holds; `None` as for [`CascStorage::scan`].
    pub fn stored_keys(&self) -> Result<Option<StoredKeys>> {
        let Some(state) = self.scan()? else { return Ok(None) };
        StoredKeys::load(&self.dir, state, |_| {}).map(Some)
    }

    /// One bucket's entries from its index file, the update section applied.
    pub fn load_bucket(&self, bucket: u8, version: u32) -> Result<Vec<IndexEntry>> {
        load_bucket(&self.dir, bucket, version)
    }

    /// The blob stored under `key`, exactly as it was appended (the BLTE
    /// bytes the CDN served); `None` when the storage doesn't index it.
    /// Fails when the entry header at the indexed place isn't the key's.
    pub fn read(&self, key: &EncodingKey) -> Result<Option<Vec<u8>>> {
        let Some(state) = self.scan()? else { return Ok(None) };
        let wanted = key.truncated();
        let bucket = wanted.bucket();
        let entries = load_bucket(&self.dir, bucket, state.versions[usize::from(bucket)])?;
        // The update section's entries come last; the last one wins.
        let Some(entry) = entries.iter().rev().find(|entry| entry.key == wanted) else { return Ok(None) };
        let path = archive_path(&self.dir, entry.location.archive());
        let size = entry.size as usize;
        if size < ENTRY_HEADER_SIZE {
            return Err(Error::damaged(&self.dir, format!("the index entry of {key} is shorter than its header")));
        }
        let mut bytes = vec![0; size];
        File::open(&path).and_then(|file| file.read_exact_at(&mut bytes, entry.location.offset().into())).at(&path)?;
        let header: &[u8; ENTRY_HEADER_SIZE] = bytes[..ENTRY_HEADER_SIZE].try_into().expect("checked the size above");
        let fields = EntryHeader::parse(header);
        if fields.truncated_key() != wanted || fields.size != entry.size {
            return Err(Error::damaged(&self.dir, format!("the entry of {key} doesn't match its index")));
        }
        bytes.drain(..ENTRY_HEADER_SIZE);
        Ok(Some(bytes))
    }

    /// What an interrupted writer left here, if anything.
    pub fn unfinished_session(&self) -> Result<Option<UnfinishedSession>> {
        Ok(journal::read_head(&self.journal_path())?.map(|head| head.session()))
    }

    /// Whether a writer has to run even when nothing is left to download:
    /// an interrupted update must publish what it journaled, and a stale
    /// journal next to a finished storage must be cleared. (A first install's
    /// journal is resumed by the install itself.)
    pub fn needs_resume(&self) -> Result<bool> {
        if file_len(&self.journal_path())?.is_none() {
            return Ok(false);
        }
        Ok(self.unfinished_session()? == Some(UnfinishedSession::Update) || self.scan()?.is_some())
    }

    /// The blob bytes (without entry headers) an interrupted run journaled
    /// that resuming keeps: what a resumed download needn't fetch again.
    pub fn journaled_bytes(&self) -> Result<u64> {
        let mut total = 0;
        journal::for_each_trusted(&self.dir, |blob| total += blob.blob_size())?;
        Ok(total)
    }

    /// Calls `body` with each key an interrupted run wrote that resuming
    /// keeps; nothing without a journal. Reads only, so a plan can tell
    /// what's left to download before a writer is opened.
    pub fn for_each_resumable_key(&self, mut body: impl FnMut(&EncodingKey)) -> Result<()> {
        journal::for_each_trusted(&self.dir, |blob| body(&blob.key))
    }

    /// Gives the game the update-section room an older Waypoint left out
    /// (Waypoint issue #3: "Truncated KMT update section detected", then
    /// NeedsRepair, with every file of the install present and sound).
    ///
    /// Only the newest version of each index file is considered, only when
    /// it is shorter than the game wants, and the only change is zeros past
    /// its end: header, entries, checksums and anything the game already
    /// wrote keep their bytes, and index versions don't change. A storage the
    /// Agent or this crate wrote is long enough already and comes out
    /// untouched, so this is safe on any install.
    ///
    /// An unfinished journal is [`Error::Unfinished`]: its writer rewrites
    /// the index files itself, and must run first.
    pub fn repair_index_reserve(&self) -> Result<IndexRepair> {
        let mut result = IndexRepair::default();
        if file_len(&self.dir)?.is_none() {
            return Ok(result);
        }
        let journal = self.journal_path();
        if file_len(&journal)?.is_some() {
            return Err(Error::Unfinished(self.dir.clone()));
        }
        let _lock = StorageLock::acquire(&self.dir)?;
        // A writer may have left a journal while this waited for the lock.
        if file_len(&journal)?.is_some() {
            return Err(Error::Unfinished(self.dir.clone()));
        }
        let Some(state) = self.scan()? else { return Ok(result) };
        for (bucket, &version) in (0u8..).zip(&state.versions) {
            let name = index_file_name(bucket, version);
            let path = self.dir.join(&name);
            let data = fs::read(&path).at(&path)?;
            let check = IndexFile::parse(&data)
                .and_then(|parsed| Ok((parsed.bucket, check_update_reserve(&data)?)))
                .ok()
                .filter(|(found, _)| *found == bucket);
            drop(data);
            match check {
                Some((_, ReserveCheck::Sufficient)) => {}
                Some((_, ReserveCheck::Extend { len })) => {
                    // Past the end, so the new bytes read as zeros: an empty
                    // update section, which is what the game expects to find.
                    OpenOptions::new()
                        .write(true)
                        .open(&path)
                        .and_then(|file| {
                            file.set_len(len as u64)?;
                            file.sync_all()
                        })
                        .at(&path)?;
                    result.repaired.push(name);
                }
                Some((_, ReserveCheck::InUse)) => {
                    tracing::warn!(path = %path.display(), "short index file whose update section is in use");
                    result.skipped.push(name);
                }
                // A format this crate doesn't write is not ours to pad.
                None => {
                    tracing::warn!(path = %path.display(), "unreadable index file left alone");
                    result.skipped.push(name);
                }
            }
        }
        if !result.repaired.is_empty() || !result.skipped.is_empty() {
            tracing::info!(
                path = %self.dir.display(),
                repaired = result.repaired.len(),
                skipped = result.skipped.len(),
                "index update sections repaired"
            );
        }
        Ok(result)
    }

    /// Recreates archives that went missing (or shorter than their segment
    /// headers) from the segment-header keys their index entries still hold,
    /// so the storage's archive numbering stays whole without any index
    /// changing; the blobs that were in them are gone and have to be
    /// downloaded again. Returns the archives restored. Safe to repeat after
    /// a crash. Takes the folder's lock, so it can't run during a write.
    pub fn restore_missing_archive_headers(&self) -> Result<Vec<u16>> {
        if file_len(&self.dir)?.is_none() {
            return Ok(Vec::new());
        }
        let _lock = StorageLock::acquire(&self.dir)?;
        let Some(state) = self.scan()? else { return Ok(Vec::new()) };
        let mut headers: BTreeMap<u16, Vec<IndexEntry>> = BTreeMap::new();
        for (bucket, &version) in (0u8..).zip(&state.versions) {
            for entry in load_bucket(&self.dir, bucket, version)? {
                if entry.size as usize == ENTRY_HEADER_SIZE && (entry.location.offset() as usize) < SEGMENT_HEADERS_SIZE
                {
                    headers.entry(entry.location.archive()).or_default().push(entry);
                }
            }
        }
        let mut restored = Vec::new();
        for (archive, entries) in headers {
            let path = archive_path(&self.dir, archive);
            if file_len(&path)?.unwrap_or(0) >= SEGMENT_HEADERS_SIZE as u64 {
                continue;
            }
            let offsets: BTreeSet<u32> = entries.iter().map(|entry| entry.location.offset()).collect();
            if !offsets.iter().copied().eq((0..16).map(|i| i * ENTRY_HEADER_SIZE as u32)) {
                return Err(Error::damaged(
                    &self.dir,
                    format!("{} lost segment headers the index can't restore", archive_file_name(archive)),
                ));
            }
            let mut bytes = vec![0u8; SEGMENT_HEADERS_SIZE];
            for entry in &entries {
                let mut key = [0u8; 16];
                key[..9].copy_from_slice(entry.key.as_bytes());
                let header = entry_header(&EncodingKey::new(key), 0, entry.location, 1, true).at(&path)?;
                let at = entry.location.offset() as usize;
                bytes[at..at + ENTRY_HEADER_SIZE].copy_from_slice(&header);
            }
            write_atomically(&path, &bytes)?;
            restored.push(archive);
        }
        if !restored.is_empty() {
            sync_dir(&self.dir)?;
            tracing::info!(path = %self.dir.display(), ?restored, "archive segment headers restored");
        }
        Ok(restored)
    }

    fn journal_path(&self) -> PathBuf {
        self.dir.join(JOURNAL_FILE_NAME)
    }
}

/// What [`CascStorage::repair_index_reserve`] did, by index file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexRepair {
    /// Index files extended to the size the game wants.
    pub repaired: Vec<String>,
    /// Short index files left alone: their update section already holds
    /// records (better left to the game's own repair), or they aren't in a
    /// format this crate writes.
    pub skipped: Vec<String>,
}

/// The 9-byte keys a storage holds, sorted per bucket and found by bisection:
/// 9 bytes a key, a fraction of a hash set's cost for WoW's two million.
#[derive(Clone)]
pub struct StoredKeys {
    buckets: [Vec<TruncatedKey>; 16],
    state: StorageState,
}

impl StoredKeys {
    /// Loads every bucket's current entries, showing each to `inspect` too.
    pub(crate) fn load(dir: &Path, state: StorageState, mut inspect: impl FnMut(&IndexEntry)) -> Result<Self> {
        let mut buckets: [Vec<TruncatedKey>; 16] = Default::default();
        for ((bucket, &version), keys) in (0u8..).zip(&state.versions).zip(&mut buckets) {
            let entries = load_bucket(dir, bucket, version)?;
            entries.iter().for_each(&mut inspect);
            *keys = entries.iter().map(|entry| entry.key).collect();
            keys.sort_unstable();
            keys.dedup();
            keys.shrink_to_fit();
        }
        Ok(Self { buckets, state })
    }

    pub fn contains(&self, key: &EncodingKey) -> bool {
        self.contains_truncated(&key.truncated())
    }

    pub fn contains_truncated(&self, key: &TruncatedKey) -> bool {
        self.buckets[usize::from(key.bucket())].binary_search(key).is_ok()
    }

    /// Distinct keys, segment headers included.
    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The index versions these keys were read from: every finished write
    /// changes them, so a caller can tell the storage changed since.
    pub fn versions(&self) -> &[u32; 16] {
        &self.state.versions
    }

    /// The archive sizes when these keys were read.
    pub fn archive_sizes(&self) -> &[u64] {
        &self.state.archive_sizes
    }
}

impl fmt::Debug for StoredKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredKeys").field("len", &self.len()).field("state", &self.state).finish()
    }
}

pub(crate) fn archive_path(dir: &Path, archive: u16) -> PathBuf {
    dir.join(archive_file_name(archive))
}

pub(crate) fn scan(dir: &Path) -> Result<Option<StorageState>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).at(dir),
    };
    let mut latest: [Option<u32>; 16] = [None; 16];
    let mut highest_archive: Option<u16> = None;
    for item in listing {
        let item = item.at(dir)?;
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some((bucket, version)) = parse_index_file_name(name) {
            let slot = &mut latest[usize::from(bucket)];
            *slot = Some(slot.map_or(version, |seen| seen.max(version)));
        } else if let Some(archive) = parse_archive_file_name(name) {
            highest_archive = Some(highest_archive.map_or(archive, |seen| seen.max(archive)));
        }
    }
    let found = latest.iter().flatten().count();
    if found == 0 && highest_archive.is_none() {
        return Ok(None);
    }
    if found != 16 {
        // A first install that stopped before publishing its index files:
        // not a storage yet. The writer resumes it from its journal.
        let head = journal::read_head(&dir.join(JOURNAL_FILE_NAME))?;
        if head.is_some_and(|head| head.session() == UnfinishedSession::NewStorage) {
            return Ok(None);
        }
        return Err(Error::damaged(dir, format!("{found} of 16 index files")));
    }
    let versions = latest.map(|version| version.unwrap_or(0));
    let archive_sizes = match highest_archive {
        Some(highest) => (0..=highest)
            .map(|archive| Ok(file_len(&archive_path(dir, archive))?.unwrap_or(0)))
            .collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let first = archive_path(dir, 0);
    let base_key = match File::open(&first) {
        Ok(file) => {
            let mut head = Vec::with_capacity(16);
            file.take(16).read_to_end(&mut head).at(&first)?;
            <[u8; 16]>::try_from(head).ok().map(|mut key| {
                key.reverse();
                key
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).at(&first),
    };
    Ok(Some(StorageState { versions, archive_sizes, base_key }))
}

pub(crate) fn load_bucket(dir: &Path, bucket: u8, version: u32) -> Result<Vec<IndexEntry>> {
    let path = dir.join(index_file_name(bucket, version));
    let data = read_if_exists(&path)?
        .ok_or_else(|| Error::damaged(dir, format!("{} is missing", index_file_name(bucket, version))))?;
    let parsed = IndexFile::parse(&data).at(&path)?;
    if parsed.bucket != bucket {
        return Err(Error::damaged(
            dir,
            format!("{} is for bucket {}", index_file_name(bucket, version), parsed.bucket),
        ));
    }
    Ok(parsed.current_entries())
}
