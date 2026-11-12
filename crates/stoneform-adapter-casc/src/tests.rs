//! Storage scenarios across writer, journal and reader: ports of Waypoint's
//! file-backed `CASCTests` (the pure ones live in `stoneform-tact`).

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::Path;

use stoneform_tact::casc::{
    ENTRY_HEADER_SIZE, EntryHeader, IndexEntry, IndexFile, JOURNAL_RECORD_SIZE, ReserveCheck, SEGMENT_HEADERS_SIZE,
    Shmem, StorageLocation, archive_file_name, check_entry_header, check_update_reserve, encode_index_file,
    encode_update_record, entry_header, index_file_name, index_file_size, segment_headers,
};
use stoneform_tact::{ContentKey, EncodingKey, TruncatedKey, archive_group, blte};

use crate::{CascStorage, DataDir, Error, JOURNAL_FILE_NAME, StorageWriter, UnfinishedSession};

fn storage_key(i: usize) -> EncodingKey {
    EncodingKey::new(std::array::from_fn(|byte| (byte * 13 + i * 29 + 1) as u8))
}

fn storage_blob(i: usize) -> Vec<u8> {
    vec![i as u8; 200 + i]
}

fn blob_bytes(keys: Range<usize>) -> u64 {
    keys.map(|i| storage_blob(i).len() as u64).sum()
}

fn append_range(writer: &mut StorageWriter, keys: Range<usize>) {
    for i in keys {
        writer.append(&storage_key(i), &storage_blob(i), false).unwrap();
    }
}

fn archive(dir: &Path, number: u16) -> Vec<u8> {
    fs::read(dir.join(archive_file_name(number))).unwrap()
}

fn journal(dir: &Path) -> std::path::PathBuf {
    dir.join(JOURNAL_FILE_NAME)
}

fn index_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|item| item.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".idx"))
        .collect();
    names.sort();
    names
}

fn expect_indexes_only(dir: &Path, version: u32) {
    assert_eq!(index_names(dir), (0..16).map(|bucket| index_file_name(bucket, version)).collect::<Vec<_>>());
}

/// Port of `expectStored`: exactly one index file per bucket, every key's
/// entry points at its header and blob, nothing else is indexed but segment
/// headers, `shmem` names the index versions, and the journal is gone.
/// Returns the index versions.
fn expect_stored(dir: &Path, keys: impl IntoIterator<Item = usize> + Clone) -> [u32; 16] {
    let storage = CascStorage::new(dir);
    let state = storage.scan().unwrap().expect("a storage");
    assert_eq!(
        index_names(dir),
        (0u8..).zip(&state.versions).map(|(bucket, &version)| index_file_name(bucket, version)).collect::<Vec<_>>()
    );
    let mut entries: HashMap<TruncatedKey, IndexEntry> = HashMap::new();
    for (bucket, &version) in (0u8..).zip(&state.versions) {
        let file = fs::read(dir.join(index_file_name(bucket, version))).unwrap();
        // The game's update section is there, as long as the Agent leaves it.
        assert_eq!(check_update_reserve(&file).unwrap(), ReserveCheck::Sufficient);
        let parsed = IndexFile::parse(&file).unwrap();
        assert_eq!(file.len(), index_file_size(parsed.sorted_end));
        for entry in parsed.entries {
            let is_segment_header =
                entry.size as usize == ENTRY_HEADER_SIZE && (entry.location.offset() as usize) < SEGMENT_HEADERS_SIZE;
            if is_segment_header {
                continue;
            }
            assert_eq!(entry.key.bucket(), bucket);
            assert!(entries.insert(entry.key, entry).is_none(), "one entry per key");
        }
    }
    assert_eq!(entries.len(), keys.clone().into_iter().count());
    for i in keys {
        let entry = entries[&storage_key(i).truncated()];
        let data = archive(dir, entry.location.archive());
        let start = entry.location.offset() as usize;
        let header = entry_header(&storage_key(i), storage_blob(i).len() as u32, entry.location, 0, false).unwrap();
        assert_eq!(data[start..start + ENTRY_HEADER_SIZE], header, "key {i}");
        assert_eq!(data[start + ENTRY_HEADER_SIZE..start + entry.size as usize], storage_blob(i), "key {i}");
        assert_eq!(storage.read(&storage_key(i)).unwrap(), Some(storage_blob(i)), "key {i}");
    }
    let shmem = Shmem::parse(&fs::read(dir.join("shmem")).unwrap()).unwrap();
    assert_eq!(shmem.index_versions, state.versions);
    assert_eq!(shmem.path, format!("{}/index", dir.display()));
    assert!(!journal(dir).exists());
    state.versions
}

/// A fresh storage, written and opened again: 16 index files with room for
/// the game's update section, `shmem`, and every blob back byte for byte.
#[test]
fn fresh_storage_is_written_and_reads_back() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("Data/data");
    let storage = CascStorage::new(&dir);
    assert!(storage.scan().unwrap().is_none());

    let mut writer = storage.create_writer().unwrap();
    assert!(!writer.is_update());
    for i in 0..20 {
        assert!(writer.append(&storage_key(i), &storage_blob(i), false).unwrap());
    }
    assert_eq!(writer.appended_bytes(), blob_bytes(0..20));
    let finished = writer.finish().unwrap();
    assert_eq!(finished.index_versions, [1; 16]);
    assert_eq!(finished.keys, 20);

    let mut files: Vec<String> =
        fs::read_dir(&dir).unwrap().map(|item| item.unwrap().file_name().into_string().unwrap()).collect();
    files.sort();
    let mut expected: Vec<String> = (0..16).map(|bucket| index_file_name(bucket, 1)).collect();
    expected.extend(["data.000", "index.lock.0", "shmem"].map(String::from));
    expected.sort();
    assert_eq!(files, expected);

    let keys = storage.stored_keys().unwrap().unwrap();
    assert_eq!(keys.len(), 20 + 16); // and the segment headers
    assert!((0..20).all(|i| keys.contains(&storage_key(i))));
    assert!(!keys.contains(&storage_key(20)));
    assert_eq!(keys.versions(), &[1; 16]);
    assert_eq!(storage.read(&storage_key(20)).unwrap(), None);

    // The segment headers lead data.000 and are indexed, header i in bucket i.
    let data = archive(&dir, 0);
    let base: [u8; 16] = std::array::from_fn(|i| data[15 - i]);
    assert_eq!(data[..SEGMENT_HEADERS_SIZE], segment_headers(&base, 0).unwrap().bytes);
    assert_eq!(storage.scan().unwrap().unwrap().base_key, Some(base));
    expect_stored(&dir, 0..20);
    assert_eq!(storage.unfinished_session().unwrap(), None);
    assert!(!storage.needs_resume().unwrap());
}

/// Build-config files keep their whole key in the entry header, as the
/// Agent writes them; everything else keeps 9 bytes.
#[test]
fn keeps_full_keys_for_build_config_files() {
    let temp = tempfile::tempdir().unwrap();
    let storage = CascStorage::new(temp.path());
    let (full, short) = (EncodingKey::new([0xC3; 16]), EncodingKey::new([0x3C; 16]));
    let mut writer = storage.create_writer().unwrap();
    writer.append(&full, b"encoding table", true).unwrap();
    writer.append(&short, b"game file", false).unwrap();
    writer.finish().unwrap();
    let data = archive(temp.path(), 0);
    for (key, kept) in [(full, *full.as_bytes()), (short, [[0x3C; 9].as_slice(), &[0; 7]].concat().try_into().unwrap())]
    {
        let entries = storage.load_bucket(key.truncated().bucket(), 1).unwrap();
        let entry = entries.iter().find(|entry| entry.key == key.truncated()).unwrap();
        let at = entry.location.offset() as usize;
        let header: [u8; 30] = data[at..at + 30].try_into().unwrap();
        assert_eq!(EntryHeader::parse(&header).key, kept);
        assert!(check_entry_header(&header, entry.location));
    }
    assert_eq!(storage.read(&full).unwrap().unwrap(), b"encoding table");
    assert_eq!(storage.read(&short).unwrap().unwrap(), b"game file");
}

/// A BLTE blob comes back byte for byte and still verifies against its key.
#[test]
fn stored_blobs_verify_against_their_keys() {
    let temp = tempfile::tempdir().unwrap();
    let storage = CascStorage::new(temp.path());
    let blob = b"BLTE\0\0\0\0Nhello".to_vec();
    let key = EncodingKey::new(*ContentKey::of(&blob).as_bytes());
    blte::verify(&blob, &key).unwrap();
    let mut writer = storage.create_writer().unwrap();
    writer.append(&key, &blob, false).unwrap();
    writer.finish().unwrap();
    let read = storage.read(&key).unwrap().unwrap();
    assert_eq!(read, blob);
    blte::verify(&read, &key).unwrap();
}

/// Port of `writerRoundTripsAndResumesAfterACrash`: garbage after the last
/// journaled blob is cut off, the resumed writer knows what it had, and
/// what it skips isn't written twice.
#[test]
fn writer_round_trips_and_resumes_after_a_crash() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let key = |i: usize| EncodingKey::new(std::array::from_fn(|byte| (byte * 31 + i * 7) as u8));
    let blob = |i: usize| vec![i as u8; 100 + i];
    let storage = CascStorage::new(dir);

    let mut writer = storage.create_writer().unwrap();
    for i in 0..20 {
        writer.append(&key(i), &blob(i), i == 0).unwrap();
    }
    drop(writer); // crash: no finish(), then garbage lands after the last blob
    OpenOptions::new().append(true).open(dir.join("data.000")).unwrap().write_all(&[0xEE; 777]).unwrap();

    let mut resumed = storage.create_writer().unwrap();
    assert_eq!(resumed.key_count(), 20);
    assert!(resumed.contains(&key(5)));
    assert_eq!(resumed.resumed_bytes(), (0..20).map(|i| blob(i).len() as u64).sum::<u64>());
    for i in 15..30 {
        assert_eq!(resumed.append(&key(i), &blob(i), false).unwrap(), i >= 20);
    }
    resumed.finish().unwrap();

    assert!(!journal(dir).exists());
    assert!(dir.join("shmem").exists());
    let data = archive(dir, 0);
    let mut found = 0;
    for bucket in 0..16 {
        let file = fs::read(dir.join(index_file_name(bucket, 1))).unwrap();
        for entry in IndexFile::parse(&file).unwrap().entries {
            let (start, end) = (entry.location.offset() as usize, (entry.location.offset() + entry.size) as usize);
            assert!(end <= data.len());
            if entry.size == 30 {
                continue; // segment header
            }
            assert_eq!(entry.key.bucket(), bucket);
            let i = (0..30).find(|&i| key(i).truncated() == entry.key).unwrap();
            let header = entry_header(&key(i), blob(i).len() as u32, entry.location, 0, i == 0).unwrap();
            assert_eq!(data[start..start + 30], header);
            assert_eq!(data[start + 30..end], blob(i));
            found += 1;
        }
    }
    assert_eq!(found, 30);
    // Nothing but whole entries: the garbage was cut off.
    let expected = SEGMENT_HEADERS_SIZE + (0..30).map(|i| 30 + blob(i).len()).sum::<usize>();
    assert_eq!(data.len(), expected);
}

/// A record cut off in the middle (the process died writing it) is ignored
/// and overwritten; its blob, never vouched for, is written again.
#[test]
fn half_written_last_record_is_discarded() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..8);
    drop(writer);
    let complete = fs::metadata(journal(dir)).unwrap().len();
    assert_eq!(complete, 16 + 8 * JOURNAL_RECORD_SIZE as u64);
    // Blob 8 reached the archive, but only part of its record the journal.
    let end = fs::metadata(dir.join("data.000")).unwrap().len();
    let header = entry_header(&storage_key(8), 208, StorageLocation::new(0, end).unwrap(), 0, false).unwrap();
    let mut data = OpenOptions::new().append(true).open(dir.join("data.000")).unwrap();
    data.write_all(&header).unwrap();
    data.write_all(&storage_blob(8)).unwrap();
    let record = stoneform_tact::casc::JournalRecord::Blob {
        key: storage_key(8),
        archive: 0,
        offset: end,
        size: 238,
        full_key: false,
    };
    OpenOptions::new().append(true).open(journal(dir)).unwrap().write_all(&record.encode()[..17]).unwrap();

    assert_eq!(storage.journaled_bytes().unwrap(), blob_bytes(0..8));
    let mut resumed = storage.open_writer().unwrap();
    assert_eq!(resumed.key_count(), 8);
    assert!(!resumed.contains(&storage_key(8)));
    assert_eq!(fs::metadata(journal(dir)).unwrap().len(), complete);
    assert_eq!(fs::metadata(dir.join("data.000")).unwrap().len(), end);
    append_range(&mut resumed, 8..12);
    resumed.finish().unwrap();
    expect_stored(dir, 0..12);
    assert_eq!(
        archive(dir, 0).len(),
        SEGMENT_HEADERS_SIZE + (0..12).map(|i| 30 + storage_blob(i).len()).sum::<usize>()
    );
}

const CHILD: &str = "STONEFORM_CASC_WRITE_FAILURE_CHILD";

/// Port of `writeFailureThrowsAndCanBeRetried`: a failed write (a full disk;
/// here a file-size limit) errors, leaves no partial blob or record, and the
/// same append succeeds afterwards. Runs in a child process because the
/// limit applies to the whole process.
#[test]
fn write_failure_leaves_nothing_behind_and_can_be_retried() {
    if std::env::var_os(CHILD).is_some() {
        return write_failure_in_child();
    }
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::write_failure_leaves_nothing_behind_and_can_be_retried", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&child.stdout) + String::from_utf8_lossy(&child.stderr);
    assert!(child.status.success(), "{output}");
    assert!(output.contains("1 passed"), "{output}");
}

fn write_failure_in_child() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let (first, second) = (EncodingKey::new([0xA1; 16]), EncodingKey::new([0xB2; 16]));
    let (small, big) = (vec![1u8; 1000], vec![2u8; 200_000]);
    let mut writer = CascStorage::new(dir).create_writer().unwrap();
    writer.append(&first, &small, false).unwrap();
    let archive_before = fs::metadata(dir.join("data.000")).unwrap().len();
    let journal_before = fs::metadata(journal(dir)).unwrap().len();

    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain libc calls on a local struct, in a single-threaded child.
    unsafe {
        // write(2) fails with EFBIG instead of the signal killing the process.
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit), 0);
    }
    let unlimited = limit;
    limit.rlim_cur = 100_000;
    // SAFETY: as above.
    unsafe { assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0) };
    let failed = writer.append(&second, &big, false);
    // SAFETY: as above.
    unsafe { assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &unlimited), 0) };
    assert!(matches!(failed, Err(Error::Io { .. })), "{failed:?}");
    assert!(!writer.contains(&second));
    assert_eq!(fs::metadata(dir.join("data.000")).unwrap().len(), archive_before);
    assert_eq!(fs::metadata(journal(dir)).unwrap().len(), journal_before);

    assert!(writer.append(&second, &big, false).unwrap());
    writer.finish().unwrap();
    let data = archive(dir, 0);
    assert_eq!(data.len() as u64, archive_before + 30 + big.len() as u64);
    assert!(data.ends_with(&big));
    assert_eq!(CascStorage::new(dir).read(&second).unwrap(), Some(big));
}

/// Port of `finishedStorageIsNeverOverwritten`: starting a new storage over
/// a finished one fails and changes nothing. (Appending to or finishing a
/// finished writer can't be written: `finish` consumes it.)
#[test]
fn finished_storage_is_never_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let key = EncodingKey::new([0x5A; 16]);
    let mut writer = storage.create_writer().unwrap();
    // Too big for any archive: it would overflow the 30-bit offsets.
    let huge = vec![0u8; 1 << 30];
    assert!(matches!(writer.append(&key, &huge, false), Err(Error::BlobTooLarge(_))));
    drop(huge);
    writer.append(&key, &[7; 500], false).unwrap();
    writer.finish().unwrap();

    let before = archive(dir, 0);
    assert!(matches!(storage.create_writer(), Err(Error::AlreadyExists(_))));
    assert_eq!(archive(dir, 0), before);
    expect_indexes_only(dir, 1);
    assert!(!journal(dir).exists());
}

/// Port of `updateAppendsToAFinishedStorage`.
#[test]
fn update_appends_to_a_finished_storage() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut first = storage.create_writer().unwrap();
    append_range(&mut first, 0..20);
    first.finish().unwrap();
    let before = archive(dir, 0);
    assert_eq!(expect_stored(dir, 0..20), [1; 16]);

    let mut update = storage.open_writer().unwrap();
    assert!(update.is_update());
    assert!(update.contains(&storage_key(7)));
    append_range(&mut update, 15..30); // 15..20 are skipped
    assert_eq!(update.appended_bytes(), blob_bytes(20..30));
    assert_eq!(update.finish().unwrap().index_versions, [2; 16]);

    assert_eq!(expect_stored(dir, 0..30), [2; 16]);
    let after = archive(dir, 0);
    assert_eq!(after[..before.len()], before, "existing data is never rewritten");
    assert_eq!(after.len(), before.len() + (20..30).map(|i| 30 + storage_blob(i).len()).sum::<usize>());

    // An update with nothing new still leaves a consistent storage.
    storage.open_writer().unwrap().finish().unwrap();
    assert_eq!(expect_stored(dir, 0..30), [3; 16]);
}

/// Port of `interruptedUpdateResumesAndKeepsOldData`.
#[test]
fn interrupted_update_resumes_and_keeps_old_data() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut first = storage.create_writer().unwrap();
    append_range(&mut first, 0..10);
    first.finish().unwrap();
    let before = archive(dir, 0);
    let add_garbage =
        || OpenOptions::new().append(true).open(dir.join("data.000")).unwrap().write_all(&[0xEE; 333]).unwrap();

    // Crash before anything was written: the garbage after the old end goes.
    drop(storage.open_writer().unwrap());
    add_garbage();
    // Crash midway: the old index files are still the complete old storage.
    let mut update = storage.open_writer().unwrap();
    append_range(&mut update, 10..15);
    drop(update);
    add_garbage();
    expect_indexes_only(dir, 1);
    assert_eq!(storage.unfinished_session().unwrap(), Some(UnfinishedSession::Update));
    assert!(storage.needs_resume().unwrap());

    // A journal: resuming needs no permission to add to a finished storage.
    let mut resumed = storage.create_writer().unwrap();
    assert!(resumed.is_update());
    assert!(resumed.contains(&storage_key(12)) && resumed.contains(&storage_key(3)));
    append_range(&mut resumed, 15..20);
    resumed.finish().unwrap();

    assert_eq!(expect_stored(dir, 0..20), [2; 16]);
    let after = archive(dir, 0);
    assert_eq!(after[..before.len()], before);
    assert_eq!(after.len(), before.len() + (10..20).map(|i| 30 + storage_blob(i).len()).sum::<usize>());
}

/// Port of `unfinishedNewStorageIsNotAStorageYet`: archives and a journal
/// without index files are a first install to resume; without the journal,
/// missing index files mean a broken storage.
#[test]
fn unfinished_new_storage_is_not_a_storage_yet() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..10);
    drop(writer);
    assert!(storage.stored_keys().unwrap().is_none());
    assert_eq!(storage.unfinished_session().unwrap(), Some(UnfinishedSession::NewStorage));
    assert!(!storage.needs_resume().unwrap()); // the install itself resumes it

    let resumed = storage.create_writer().unwrap();
    assert!(!resumed.is_update());
    assert!(resumed.contains(&storage_key(4)));
    resumed.finish().unwrap();
    expect_stored(dir, 0..10);

    for name in index_names(dir) {
        fs::remove_file(dir.join(name)).unwrap();
    }
    assert!(matches!(storage.stored_keys(), Err(Error::Damaged { .. })));
    // A journal without its base key isn't one the writer resumes either.
    fs::write(journal(dir), [0; 10]).unwrap();
    assert!(matches!(storage.stored_keys(), Err(Error::Damaged { .. })));
    fs::remove_file(journal(dir)).unwrap();

    // An update's journal (it starts with a marker) needs the old index files.
    let other = tempfile::tempdir().unwrap();
    let other_storage = CascStorage::new(other.path());
    let mut first = other_storage.create_writer().unwrap();
    append_range(&mut first, 0..1);
    first.finish().unwrap();
    let mut update = other_storage.open_writer().unwrap();
    append_range(&mut update, 1..2);
    drop(update);
    for name in index_names(other.path()) {
        fs::remove_file(other.path().join(name)).unwrap();
    }
    assert!(matches!(other_storage.stored_keys(), Err(Error::Damaged { .. })));
    assert!(matches!(other_storage.open_writer(), Err(Error::Damaged { .. })));
}

/// Port of `plansCountWhatResumingKeeps`: what counts as downloaded is
/// exactly what resuming keeps: journaled blobs whose bytes reached disk,
/// and for an update only what it added.
#[test]
fn plans_count_what_resuming_keeps() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let resumable = || {
        let mut keys = Vec::new();
        storage.for_each_resumable_key(|key| keys.push(*key)).unwrap();
        keys.sort();
        keys
    };
    let keys = |range: Range<usize>| {
        let mut keys: Vec<EncodingKey> = range.map(storage_key).collect();
        keys.sort();
        keys
    };
    assert!(resumable().is_empty());
    assert_eq!(storage.journaled_bytes().unwrap(), 0);

    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..10);
    drop(writer);
    // A torn write: the last blob's bytes didn't all reach disk.
    let archive0 = OpenOptions::new().write(true).open(dir.join("data.000")).unwrap();
    archive0.set_len(archive0.metadata().unwrap().len() - 10).unwrap();
    assert_eq!(resumable(), keys(0..9));
    assert_eq!(storage.journaled_bytes().unwrap(), blob_bytes(0..9));

    let resumed = storage.create_writer().unwrap();
    assert_eq!(resumed.key_count(), 9);
    assert!(!resumed.contains(&storage_key(9)));
    assert_eq!(resumed.resumed_bytes(), blob_bytes(0..9));
    resumed.finish().unwrap();
    assert!(resumable().is_empty());

    let mut update = storage.open_writer().unwrap();
    append_range(&mut update, 9..12);
    drop(update);
    assert_eq!(resumable(), keys(9..12));
    assert_eq!(storage.journaled_bytes().unwrap(), blob_bytes(9..12));
}

/// Port of `oneWriterPerStorage`: WoW's flavors share one storage, so a
/// second writer must not write to it while another one is.
#[test]
fn one_writer_per_storage() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..1);
    assert!(matches!(storage.open_writer(), Err(Error::Busy(_))));
    assert!(matches!(storage.restore_missing_archive_headers(), Err(Error::Busy(_))));
    drop(writer); // paused: the journal stays, the folder is free again

    let mut resumed = storage.open_writer().unwrap();
    assert!(resumed.contains(&storage_key(0)));
    append_range(&mut resumed, 1..2);
    resumed.finish().unwrap(); // done: the next writer needn't wait for this one
    storage.open_writer().unwrap().finish().unwrap();
    assert_eq!(expect_stored(dir, 0..2), [2; 16]);
}

/// Port of `loadsIndexUpdateSection`, on files: later records win, and a
/// non-zero status removes the key.
#[test]
fn loads_the_index_update_section() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let entry = |key: u8, offset: u64, size: u32| IndexEntry {
        key: TruncatedKey::new([key; 9]),
        location: StorageLocation::new(0, offset).unwrap(),
        size,
    };
    let mut index = encode_index_file(0, &[entry(0x11, 0x1E0, 100), entry(0x22, 0x244, 100)]);
    let page = (0x28 + 2 * 18usize).next_multiple_of(0x200);
    let updates = [encode_update_record(&entry(0x33, 0x2A8, 50), 0), encode_update_record(&entry(0x22, 0x244, 100), 3)];
    index[page..page + 48].copy_from_slice(&updates.concat());
    fs::write(dir.join(index_file_name(0, 5)), index).unwrap();
    for bucket in 1..16 {
        fs::write(dir.join(index_file_name(bucket, 5)), encode_index_file(bucket, &[])).unwrap();
    }
    let storage = CascStorage::new(dir);
    let state = storage.scan().unwrap().unwrap();
    assert_eq!(state.versions, [5; 16]);
    assert!(state.archive_sizes.is_empty() && state.base_key.is_none());
    let entries = storage.load_bucket(0, 5).unwrap();
    assert_eq!(entries, [entry(0x11, 0x1E0, 100), entry(0x33, 0x2A8, 50)]);
    assert_eq!(storage.stored_keys().unwrap().unwrap().len(), 2);
}

// The game's update section (Waypoint issue #3).

/// `count` distinct entries for one index file.
fn index_entries(count: usize) -> Vec<IndexEntry> {
    (0..count)
        .map(|i| {
            let mut key = [0u8; 9];
            key[..4].copy_from_slice(&(i as u32).to_le_bytes());
            key[8] = 0xA5;
            IndexEntry {
                key: TruncatedKey::new(key),
                location: StorageLocation::new(0, (SEGMENT_HEADERS_SIZE + i * 0x40) as u64).unwrap(),
                size: 0x40,
            }
        })
        .collect()
}

/// What Waypoint 0.9.5 wrote: the reserve measured from the entries' end.
fn short_index_file_size(sorted_end: usize) -> usize {
    (sorted_end + 0x7800).next_multiple_of(0x10000)
}

/// Port of `shortIndexStorage`: a storage whose bucket-0 index is as short
/// as Waypoint 0.9.5 left it (the other 15 are empty and were big enough
/// all along). `tail` goes where the update section starts.
fn short_index_storage(dir: &Path, entries: &[IndexEntry], tail: &[u8]) -> Vec<u8> {
    fs::create_dir_all(dir).unwrap();
    let mut file = encode_index_file(0, entries);
    let sorted_end = 0x28 + 18 * entries.len();
    let short = short_index_file_size(sorted_end);
    assert!(short < file.len(), "this index was long enough already");
    file.truncate(short);
    let page = sorted_end.next_multiple_of(0x200);
    file[page..page + tail.len()].copy_from_slice(tail);
    fs::write(dir.join(index_file_name(0, 1)), &file).unwrap();
    for bucket in 1..16 {
        fs::write(dir.join(index_file_name(bucket, 1)), encode_index_file(bucket, &[])).unwrap();
    }
    fs::write(dir.join("data.000"), segment_headers(&[7; 16], 0).unwrap().bytes).unwrap();
    file
}

/// Port of `repairGivesShortIndexesTheirUpdateSectionBack`: the short index
/// gets the room the game needs, and nothing else about it moves.
#[test]
fn repair_gives_short_indexes_their_update_section_back() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let entries = index_entries(1_819);
    let before = short_index_storage(dir, &entries, &[]);
    let storage = CascStorage::new(dir);
    let keys = storage.stored_keys().unwrap().unwrap().len();

    let repair = storage.repair_index_reserve().unwrap();
    assert_eq!(repair.repaired, [index_file_name(0, 1)]);
    assert!(repair.skipped.is_empty());

    let after = fs::read(dir.join(index_file_name(0, 1))).unwrap();
    let sorted_end = 0x28 + 18 * entries.len();
    assert_eq!(after.len(), index_file_size(sorted_end));
    assert!(after.len() - sorted_end.next_multiple_of(0x1000) >= 0x7800);
    // Header, entries and checksums keep their bytes; only zeros were added.
    assert_eq!(after[..before.len()], before);
    assert!(after[before.len()..].iter().all(|&byte| byte == 0));
    assert_eq!(storage.load_bucket(0, 1).unwrap().len(), entries.len());
    assert_eq!(storage.stored_keys().unwrap().unwrap().len(), keys);
    // Index versions don't change, so no reader looks for a new file.
    assert_eq!(storage.scan().unwrap().unwrap().versions, [1; 16]);
    expect_indexes_only(dir, 1);
    // Running it again changes nothing.
    assert_eq!(storage.repair_index_reserve().unwrap(), Default::default());
    assert_eq!(fs::read(dir.join(index_file_name(0, 1))).unwrap(), after);
}

/// Port of `repairLeavesSoundAndBusyStoragesAlone`: storages this crate or
/// the Agent wrote, an index whose update section is in use, and an
/// unfinished install stay as they are.
#[test]
fn repair_leaves_sound_and_busy_storages_alone() {
    // A storage this crate wrote (so, by size, an Agent-written one too).
    let written = tempfile::tempdir().unwrap();
    let storage = CascStorage::new(written.path());
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..20);
    writer.finish().unwrap();
    let sizes = || -> Vec<u64> {
        (0..16).map(|bucket| fs::metadata(written.path().join(index_file_name(bucket, 1))).unwrap().len()).collect()
    };
    let before = sizes();
    assert_eq!(storage.repair_index_reserve().unwrap(), Default::default());
    assert_eq!(sizes(), before);

    // A short index whose update section already holds a record: a game
    // that got that far is better left to its own repair.
    let used = tempfile::tempdir().unwrap();
    let entries = index_entries(1_819);
    let record = encode_update_record(&IndexEntry { size: 0x40, ..entries[0] }, 0);
    let used_before = short_index_storage(used.path(), &entries, &record);
    let repair = CascStorage::new(used.path()).repair_index_reserve().unwrap();
    assert!(repair.repaired.is_empty());
    assert_eq!(repair.skipped, [index_file_name(0, 1)]);
    assert_eq!(fs::read(used.path().join(index_file_name(0, 1))).unwrap(), used_before);

    // An unfinished install or update: its own writer finishes the job.
    let busy = tempfile::tempdir().unwrap();
    let busy_before = short_index_storage(busy.path(), &entries, &[]);
    fs::write(journal(busy.path()), [0; 16]).unwrap();
    assert!(matches!(
        CascStorage::new(busy.path()).repair_index_reserve(),
        Err(Error::Unfinished(path)) if path == busy.path()
    ));
    assert_eq!(fs::read(busy.path().join(index_file_name(0, 1))).unwrap(), busy_before);

    // Nothing to repair where there is no storage.
    assert_eq!(CascStorage::new(busy.path().join("missing")).repair_index_reserve().unwrap(), Default::default());
}

/// Port of `updateAfterRepairWritesFullSizedIndexes`: the next index version
/// is written from scratch, at the size the game wants.
#[test]
fn update_after_repair_writes_full_sized_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let entries = index_entries(1_819);
    short_index_storage(dir, &entries, &[]);
    let storage = CascStorage::new(dir);
    assert_eq!(storage.repair_index_reserve().unwrap().repaired.len(), 1);

    let mut update = storage.open_writer().unwrap();
    append_range(&mut update, 1..2);
    update.finish().unwrap();
    expect_indexes_only(dir, 2);
    for bucket in 0..16 {
        let file = fs::read(dir.join(index_file_name(bucket, 2))).unwrap();
        let parsed = IndexFile::parse(&file).unwrap();
        assert_eq!(file.len(), index_file_size(parsed.sorted_end));
        assert_eq!(check_update_reserve(&file).unwrap(), ReserveCheck::Sufficient);
    }
    assert!(storage.load_bucket(0, 2).unwrap().len() >= entries.len());
    assert_eq!(storage.read(&storage_key(1)).unwrap(), Some(storage_blob(1)));
}

/// Port of `markerlessJournalsNeverReplaceCompletedStorage`: a crash after
/// publishing a first install's indexes, or a finished storage beside a
/// stale 16-byte journal, is added to, never started over.
#[test]
fn markerless_journals_never_replace_completed_storage() {
    for published_journal in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let storage = CascStorage::new(dir);
        let mut writer = storage.create_writer().unwrap();
        append_range(&mut writer, 0..3);
        let saved = fs::read(journal(dir)).unwrap();
        writer.finish().unwrap();
        let before = archive(dir, 0);
        fs::write(journal(dir), if published_journal { saved } else { vec![0xA5; 16] }).unwrap();
        let journal_before = fs::read(journal(dir)).unwrap();
        assert!(storage.needs_resume().unwrap());

        assert!(matches!(storage.create_writer(), Err(Error::AlreadyExists(_))));
        assert_eq!(fs::read(journal(dir)).unwrap(), journal_before);
        assert_eq!(archive(dir, 0), before);

        let mut resumed = storage.open_writer().unwrap();
        assert!(resumed.is_update());
        assert!((0..3).all(|i| resumed.contains(&storage_key(i))));
        append_range(&mut resumed, 42..43);
        resumed.finish().unwrap();

        let after = archive(dir, 0);
        assert_eq!(after[..before.len()], before);
        assert_eq!(after.len(), before.len() + 30 + storage_blob(42).len());
        assert_eq!(expect_stored(dir, (0..3).chain(42..43)), [2; 16]);
    }
}

/// Archives fill up and the next one starts with its own segment headers;
/// a crash in a later archive resumes there, and leftovers of archives that
/// were started but never used go.
#[test]
fn fills_archives_one_after_another() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let blob = |i: usize| vec![i as u8; 300];
    // Two 330-byte entries fit after the segment headers.
    let capacity = (SEGMENT_HEADERS_SIZE + 2 * (ENTRY_HEADER_SIZE + 300)) as u64;
    let mut writer = storage.create_writer().unwrap().with_archive_capacity(capacity);
    for i in 0..5 {
        writer.append(&storage_key(i), &blob(i), false).unwrap();
    }
    drop(writer);
    // Garbage after data.002's only blob, and a data.003 started and lost.
    OpenOptions::new().append(true).open(dir.join("data.002")).unwrap().write_all(&[0xEE; 99]).unwrap();
    fs::write(dir.join("data.003"), [0xDD; 700]).unwrap();
    assert_eq!(storage.journaled_bytes().unwrap(), 5 * 300);

    let mut resumed = storage.open_writer().unwrap().with_archive_capacity(capacity);
    assert_eq!(resumed.key_count(), 5);
    for i in 5..7 {
        resumed.append(&storage_key(i), &blob(i), false).unwrap();
    }
    fs::write(dir.join("data.004"), [0xDD; 10]).unwrap();
    resumed.finish().unwrap();

    assert!(!dir.join("data.004").exists());
    let sizes = storage.scan().unwrap().unwrap().archive_sizes;
    assert_eq!(sizes, [capacity, capacity, capacity, capacity - 330]);
    for number in 0..4 {
        let data = archive(dir, number);
        let base: [u8; 16] = std::array::from_fn(|i| data[15 - i]);
        assert_eq!(data[..SEGMENT_HEADERS_SIZE], segment_headers(&base, number).unwrap().bytes);
    }
    for i in 0..7 {
        assert_eq!(storage.read(&storage_key(i)).unwrap(), Some(blob(i)), "key {i}");
    }
    // Every bucket indexes one segment header per archive.
    for bucket in 0..16 {
        let headers = storage.load_bucket(bucket, 1).unwrap().into_iter().filter(|entry| entry.size == 30).count();
        assert_eq!(headers, 4, "bucket {bucket}");
    }
    let shmem = Shmem::parse(&fs::read(dir.join("shmem")).unwrap()).unwrap();
    assert_eq!(shmem.free_spans[0].offset, capacity);
    assert_eq!(shmem.free_spans[3].offset, 3 << 30 | (capacity - 330));
}

/// A crash while finishing, after only some buckets were published: for a
/// new storage that's still no storage, for an update a mixed set the game
/// reads fine; either way the next run finishes the job without losing a
/// blob, and never cuts off bytes a published index points at.
#[test]
fn crash_while_finishing_resumes() {
    // A new storage.
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..10);
    writer.crash_while_finishing(5).unwrap();
    assert_eq!(index_names(dir).len(), 5);
    assert!(storage.scan().unwrap().is_none());
    storage.create_writer().unwrap().finish().unwrap();
    assert_eq!(expect_stored(dir, 0..10), [1; 16]);

    // An update, whose journal then lost its last two records.
    let mut update = storage.open_writer().unwrap();
    append_range(&mut update, 10..15);
    update.crash_while_finishing(8).unwrap();
    let length = fs::metadata(dir.join("data.000")).unwrap().len();
    let state = storage.scan().unwrap().unwrap();
    assert_eq!(state.versions, [2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1]);
    let journal_file = OpenOptions::new().write(true).open(journal(dir)).unwrap();
    journal_file.set_len(journal_file.metadata().unwrap().len() - 2 * JOURNAL_RECORD_SIZE as u64).unwrap();

    let resumed = storage.open_writer().unwrap();
    assert!((10..13).all(|i| resumed.contains(&storage_key(i))));
    let finished = resumed.finish().unwrap();
    assert_eq!(finished.index_versions, [3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2]);
    assert_eq!(fs::metadata(dir.join("data.000")).unwrap().len(), length, "nothing cut off");
    expect_indexes_only_versions(dir, &finished.index_versions);
    for i in 0..15 {
        let published = storage_key(i).truncated().bucket() < 8;
        let read = storage.read(&storage_key(i)).unwrap();
        if i < 13 || published {
            assert_eq!(read, Some(storage_blob(i)), "key {i}");
        } else {
            assert_eq!(read, None, "key {i} was in neither the journal nor a published index");
        }
    }
}

fn expect_indexes_only_versions(dir: &Path, versions: &[u32; 16]) {
    let expected: Vec<String> =
        (0u8..).zip(versions).map(|(bucket, &version)| index_file_name(bucket, version)).collect();
    assert_eq!(index_names(dir), expected);
}

/// A lost archive gets its segment headers back from the index, and nothing
/// else changes.
#[test]
fn restores_missing_archive_headers() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let storage = CascStorage::new(dir);
    let mut writer = storage.create_writer().unwrap();
    append_range(&mut writer, 0..5);
    writer.finish().unwrap();
    let indexes: Vec<Vec<u8>> = index_names(dir).iter().map(|name| fs::read(dir.join(name)).unwrap()).collect();
    assert!(storage.restore_missing_archive_headers().unwrap().is_empty());

    fs::remove_file(dir.join("data.000")).unwrap();
    assert_eq!(storage.restore_missing_archive_headers().unwrap(), [0]);
    let data = archive(dir, 0);
    assert_eq!(data.len(), SEGMENT_HEADERS_SIZE);
    for bucket in 0..16u8 {
        let entry = storage.load_bucket(bucket, 1).unwrap().into_iter().find(|entry| entry.size == 30).unwrap();
        let at = entry.location.offset() as usize;
        assert_eq!(at, usize::from(bucket) * 30);
        let header: [u8; 30] = data[at..at + 30].try_into().unwrap();
        assert!(check_entry_header(&header, entry.location));
        assert_eq!(EntryHeader::parse(&header).truncated_key(), entry.key);
    }
    let after: Vec<Vec<u8>> = index_names(dir).iter().map(|name| fs::read(dir.join(name)).unwrap()).collect();
    assert_eq!(after, indexes, "no index changes");
    assert!(storage.restore_missing_archive_headers().unwrap().is_empty());
    // The blobs that were in it are gone, which reading says.
    assert!(storage.read(&storage_key(0)).is_err());
}

// The rest of `Data`.

fn cdn_index(entries: &[([u8; 16], u32, u32)]) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|entry| entry.0);
    let mut pages = Vec::new();
    let mut page = Vec::new();
    for (key, size, offset) in sorted {
        page.extend(key);
        page.extend(size.to_be_bytes());
        page.extend(offset.to_be_bytes());
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

fn part_files(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter(|item| item.as_ref().unwrap().file_name().to_string_lossy().ends_with(".part"))
        .count()
}

#[test]
fn writes_configs_and_cdn_indexes_where_the_agent_keeps_them() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::new(temp.path().join("Data"));
    let hash = "ABCDEF0123456789abcdef0123456789";
    let path = data.write_config(hash, b"# Build Configuration\n").unwrap();
    assert_eq!(path, temp.path().join("Data/config/ab/cd/abcdef0123456789abcdef0123456789"));
    assert_eq!(fs::read(&path).unwrap(), b"# Build Configuration\n");
    assert_eq!(part_files(path.parent().unwrap()), 0);
    for bad in ["../../../../../../etc/passwd", "abcd", "abcdef0123456789abcdef012345678g", ""] {
        assert!(matches!(data.config_path(bad), Err(Error::InvalidName(_))), "{bad:?}");
        assert!(matches!(data.index_path(bad), Err(Error::InvalidName(_))), "{bad:?}");
    }

    let index = data.write_index("00112233445566778899aabbccddeeff", b"index").unwrap();
    assert_eq!(index, temp.path().join("Data/indices/00112233445566778899aabbccddeeff.index"));
    let cached = temp.path().join("cache.index");
    fs::write(&cached, b"cached index").unwrap();
    let copied = data.copy_index("FFEEDDCCBBAA99887766554433221100", &cached).unwrap();
    assert_eq!(copied, temp.path().join("Data/indices/ffeeddccbbaa99887766554433221100.index"));
    assert_eq!(fs::read(&copied).unwrap(), b"cached index");
    assert_eq!(part_files(&data.indices_path()), 0);
    assert_eq!(data.storage().path(), temp.path().join("Data/data"));
}

/// The archive group is built into `Data/indices` under its name; one that
/// is there already is kept, and a mismatch leaves nothing behind.
#[test]
fn builds_archive_groups_next_to_their_indexes() {
    let temp = tempfile::tempdir().unwrap();
    let data = DataDir::new(temp.path());
    let key = |i: usize| -> [u8; 16] {
        std::array::from_fn(|byte| if byte == 0 { 0xA5 } else { (i >> ((8 * (15 - byte)) % 64)) as u8 })
    };
    let mut lists: [Vec<([u8; 16], u32, u32)>; 3] = Default::default();
    for i in 0..400 {
        lists[i % 3].push((key(i * 7919 % 100_003), i as u32, i as u32 * 10));
    }
    let names =
        ["0000000000000000000000000000000a", "0000000000000000000000000000000b", "0000000000000000000000000000000c"];
    let indexes: Vec<Vec<u8>> = lists.iter().map(|list| cdn_index(list)).collect();
    for (name, index) in names.iter().zip(&indexes) {
        data.write_index(name, index).unwrap();
    }
    let mut expected = Vec::new();
    let slices: Vec<&[u8]> = indexes.iter().map(Vec::as_slice).collect();
    let group = archive_group::build(&slices, &mut expected).unwrap();

    assert!(data.build_archive_group(&names, &group).unwrap());
    let path = data.index_path(&group).unwrap();
    assert_eq!(fs::read(&path).unwrap(), expected);
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(data.build_archive_group(&names, &group.to_uppercase()).unwrap());
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified, "kept, not rebuilt");

    let wrong = "0".repeat(32);
    assert!(!data.build_archive_group(&names, &wrong).unwrap());
    assert!(!data.index_path(&wrong).unwrap().exists());
    assert_eq!(part_files(&data.indices_path()), 0);
    // An index that isn't there is an error, not a silently smaller group.
    let missing = ["0000000000000000000000000000000d"];
    assert!(matches!(data.build_archive_group(&missing, &wrong), Err(Error::Io { .. })));
}
