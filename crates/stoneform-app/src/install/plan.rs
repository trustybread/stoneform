//! What a fresh install downloads, worked out from the build's manifests.
//!
//! Pure: the `Installer` use case fetches the inputs, these functions decide.
//! Ported from Waypoint's `CASCInstaller.plan` (0BSD).

use std::collections::{HashMap, HashSet};

use stoneform_tact::archive_index::ArchiveEntry;
use stoneform_tact::config::{BuildConfig, CdnKind};
use stoneform_tact::download::DownloadManifest;
use stoneform_tact::encoding::EncodingTable;
use stoneform_tact::install::InstallManifest;
use stoneform_tact::keys::{ContentKey, EncodingKey};

/// Where a file is on the CDN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Inside the CDN config's archive number `archive`, at `offset`.
    Archive { archive: u32, offset: u64 },
    /// A file of its own, under `data/` or `patch/`.
    File(CdnKind),
}

impl Source {
    fn order(self) -> (u32, u64) {
        match self {
            Source::Archive { archive, offset } => (archive, offset),
            Source::File(_) => (u32::MAX, 0),
        }
    }
}

/// One encoded file for the game's local storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageItem {
    pub key: EncodingKey,
    /// The encoded (BLTE) size; 0 until an index tells.
    pub size: u64,
    /// Build-config files carry their whole key in the entry header, as the Agent writes them.
    pub full_key: bool,
    /// Stored as served (the patch manifest) and checked by size, not as BLTE.
    pub raw: bool,
    pub source: Source,
}

/// One file the game keeps outside local storage, such as its `.app`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LooseFile {
    /// Relative to the flavor folder (`_forever_`).
    pub path: String,
    pub content_key: ContentKey,
    pub encoding_key: EncodingKey,
    /// The decoded size.
    pub size: u64,
    /// The encoded size, once an index tells; 0 before.
    pub encoded_size: u64,
    pub source: Source,
}

/// The files an install needs, before they are located on the CDN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Sorted by key, without duplicates.
    pub storage: Vec<StorageItem>,
    pub loose: Vec<LooseFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error(transparent)]
    Format(#[from] stoneform_tact::Error),
    #[error("the encoding table has no entry for {0}")]
    NotEncoded(String),
}

/// Picks the files for `tags`: loose files from the install manifest, local
/// storage files from the download manifest plus the build-config files the
/// Agent stores with full keys. `stored` says what the storage already holds;
/// those are left out, so an interrupted install continues where it stopped.
pub fn select(
    build_config: &BuildConfig,
    tags: &str,
    install: &InstallManifest<'_>,
    encoding: &EncodingTable<'_>,
    download: &DownloadManifest<'_>,
    stored: impl Fn(&EncodingKey) -> bool,
) -> Result<Selection, PlanError> {
    let loose_entries = install.select(tags);
    let root = build_config.root()?;
    let wanted: HashSet<ContentKey> = loose_entries.iter().map(|entry| entry.content_key).chain([root]).collect();
    let encoded = encoding.find(&wanted);

    let loose = loose_entries
        .iter()
        .map(|entry| {
            let found = encoded.get(&entry.content_key).ok_or_else(|| PlanError::NotEncoded(entry.path.clone()))?;
            Ok(LooseFile {
                path: entry.path.clone(),
                content_key: entry.content_key,
                encoding_key: found.encoding_key,
                size: u64::from(entry.size),
                encoded_size: 0,
                source: Source::File(CdnKind::Data),
            })
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    // Every loose file's key stays out of storage.
    let loose_keys: HashSet<EncodingKey> = loose.iter().map(|file| file.encoding_key).collect();

    // Build-config files (encoding, install, download, …) with full keys; the
    // patch manifest raw, from the patch path; root with a short key.
    let mut storage: Vec<StorageItem> = build_config
        .storage_files()
        .into_iter()
        .map(|file| StorageItem {
            key: file.encoding_key,
            size: file.encoded_size,
            full_key: true,
            raw: false,
            source: Source::File(CdnKind::Data),
        })
        .collect();
    if let Some((key, size)) = build_config.patch() {
        storage.push(StorageItem { key, size, full_key: true, raw: true, source: Source::File(CdnKind::Patch) });
    }
    let root_key = encoded.get(&root).ok_or_else(|| PlanError::NotEncoded("root".to_owned()))?.encoding_key;
    storage.push(StorageItem {
        key: root_key,
        size: 0,
        full_key: false,
        raw: false,
        source: Source::File(CdnKind::Data),
    });
    let special: HashSet<EncodingKey> = storage.iter().map(|item| item.key).collect();

    let chosen = download.selection(tags);
    storage.reserve(chosen.count());
    for entry in download.selected(&chosen) {
        if loose_keys.contains(&entry.encoding_key) || special.contains(&entry.encoding_key) {
            continue;
        }
        storage.push(StorageItem {
            key: entry.encoding_key,
            size: entry.size,
            full_key: false,
            raw: false,
            source: Source::File(CdnKind::Data),
        });
    }
    storage.retain(|item| !stored(&item.key));
    storage.sort_by_key(|item| item.key);
    storage.dedup_by_key(|item| item.key);
    Ok(Selection { storage, loose })
}

/// Records where `index`'s archive (number `archive` in the CDN config) holds
/// selected files. Call it for every archive, in any order: when several hold
/// a file, the one listed first in the CDN config wins, as for Waypoint.
pub fn locate(selection: &mut Selection, archive: u32, entries: impl IntoIterator<Item = ArchiveEntry>) {
    let loose = loose_by_key(&selection.loose);
    for entry in entries {
        let source = Source::Archive { archive, offset: entry.offset };
        if let Ok(position) = selection.storage.binary_search_by_key(&entry.key, |item| item.key) {
            let item = &mut selection.storage[position];
            if earlier(source, item.source) {
                item.source = source;
                item.size = entry.size;
            }
        }
        for &position in loose.get(&entry.key).into_iter().flatten() {
            let file = &mut selection.loose[position];
            if earlier(source, file.source) {
                file.source = source;
                file.encoded_size = entry.size;
            }
        }
    }
}

/// Loose files by encoding key. Several paths can hold the same content (an
/// app bundle repeats small resources), so one key can stand for several files.
fn loose_by_key(loose: &[LooseFile]) -> HashMap<EncodingKey, Vec<usize>> {
    let mut by_key: HashMap<EncodingKey, Vec<usize>> = HashMap::new();
    for (position, file) in loose.iter().enumerate() {
        by_key.entry(file.encoding_key).or_default().push(position);
    }
    by_key
}

/// Sizes of files outside archives, from the CDN's file index.
pub fn size_unarchived(selection: &mut Selection, file_index: impl IntoIterator<Item = ArchiveEntry>) {
    let loose = loose_by_key(&selection.loose);
    for entry in file_index {
        if let Ok(position) = selection.storage.binary_search_by_key(&entry.key, |item| item.key) {
            let item = &mut selection.storage[position];
            if matches!(item.source, Source::File(_)) && item.size == 0 {
                item.size = entry.size;
            }
        }
        for &position in loose.get(&entry.key).into_iter().flatten() {
            let file = &mut selection.loose[position];
            if matches!(file.source, Source::File(_)) {
                file.encoded_size = entry.size;
            }
        }
    }
}

/// Download order: archive by archive, front to back; files of their own last.
pub fn order_for_download(storage: &mut [StorageItem]) {
    storage.sort_by_key(|item| item.source.order());
}

fn earlier(candidate: Source, current: Source) -> bool {
    match (candidate, current) {
        (Source::Archive { archive: new, .. }, Source::Archive { archive: old, .. }) => new < old,
        (Source::Archive { .. }, Source::File(_)) => true,
        _ => false,
    }
}

/// One download: a range of one CDN archive holding several wanted blobs, or
/// a file of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// `None` for a file of its own.
    pub archive: Option<u32>,
    /// The byte range in the archive, end exclusive; unused for files.
    pub start: u64,
    pub end: u64,
    /// Positions in the storage list.
    pub items: Vec<usize>,
}

/// Groups blobs that sit close together in one archive into single range
/// requests of at most `max_bytes`, bridging gaps of at most `max_gap`.
/// `storage` must be in download order; `skip` leaves out what is stored.
pub fn batches(
    storage: &[StorageItem],
    max_bytes: u64,
    max_gap: u64,
    skip: impl Fn(&StorageItem) -> bool,
) -> Vec<Batch> {
    let mut result: Vec<Batch> = Vec::new();
    for (position, item) in storage.iter().enumerate() {
        if skip(item) {
            continue;
        }
        match item.source {
            Source::Archive { archive, offset } => {
                let end = offset + item.size;
                if let Some(last) = result.last_mut()
                    && last.archive == Some(archive)
                    && offset >= last.end
                    && offset - last.end <= max_gap
                    && end - last.start <= max_bytes
                {
                    last.end = end;
                    last.items.push(position);
                    continue;
                }
                result.push(Batch { archive: Some(archive), start: offset, end, items: vec![position] });
            }
            Source::File(_) => result.push(Batch { archive: None, start: 0, end: 0, items: vec![position] }),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> EncodingKey {
        EncodingKey::from_slice(&[n; 16]).unwrap()
    }

    fn item(n: u8, size: u64, source: Source) -> StorageItem {
        StorageItem { key: key(n), size, full_key: false, raw: false, source }
    }

    fn archived(n: u8, archive: u32, offset: u64, size: u64) -> StorageItem {
        item(n, size, Source::Archive { archive, offset })
    }

    fn loose(path: &str, n: u8) -> LooseFile {
        LooseFile {
            path: path.to_owned(),
            content_key: ContentKey::of(&[n]),
            encoding_key: key(n),
            size: 1,
            encoded_size: 0,
            source: Source::File(CdnKind::Data),
        }
    }

    fn entry(n: u8, size: u64, offset: u64) -> ArchiveEntry {
        ArchiveEntry { key: key(n), size, offset }
    }

    /// An app bundle repeats small resources: every path with the same
    /// content must be found, or the missed ones are asked for as files of
    /// their own, which the CDN doesn't have (HTTP 403).
    #[test]
    fn locates_every_loose_file_that_shares_content() {
        let mut selection = Selection {
            storage: vec![item(9, 0, Source::File(CdnKind::Data))],
            loose: vec![
                loose("en.lproj/InfoPlist.strings", 1),
                loose("de.lproj/InfoPlist.strings", 1),
                loose("other", 2),
            ],
        };

        locate(&mut selection, 3, [entry(1, 641, 236_506_753), entry(9, 50, 10)]);

        let expected = Source::Archive { archive: 3, offset: 236_506_753 };
        assert_eq!((selection.loose[0].source, selection.loose[0].encoded_size), (expected, 641));
        assert_eq!((selection.loose[1].source, selection.loose[1].encoded_size), (expected, 641));
        assert_eq!(selection.loose[2].source, Source::File(CdnKind::Data));
        assert_eq!(selection.storage[0].source, Source::Archive { archive: 3, offset: 10 });
    }

    #[test]
    fn sizes_every_unarchived_loose_file_that_shares_content() {
        let mut selection = Selection { storage: vec![], loose: vec![loose("a", 1), loose("b", 1)] };
        size_unarchived(&mut selection, [entry(1, 77, 0)]);
        assert!(selection.loose.iter().all(|file| file.encoded_size == 77));
    }

    #[test]
    fn earlier_archives_win() {
        let first = Source::Archive { archive: 1, offset: 0 };
        let later = Source::Archive { archive: 5, offset: 0 };
        assert!(earlier(first, later));
        assert!(!earlier(later, first));
        assert!(earlier(later, Source::File(CdnKind::Data)));
        assert!(!earlier(Source::File(CdnKind::Data), first));
    }

    #[test]
    fn orders_archives_front_to_back_and_files_last() {
        let mut storage = vec![
            item(1, 10, Source::File(CdnKind::Data)),
            archived(2, 3, 100, 10),
            archived(3, 0, 50, 10),
            archived(4, 3, 0, 10),
        ];
        order_for_download(&mut storage);
        let keys: Vec<_> = storage.iter().map(|item| item.key).collect();
        assert_eq!(keys, vec![key(3), key(4), key(2), key(1)]);
    }

    #[test]
    fn batches_neighbours_and_splits_at_gaps_limits_and_archives() {
        let storage = vec![
            archived(1, 0, 0, 100),
            archived(2, 0, 100, 100),  // adjacent: same batch
            archived(3, 0, 250, 100),  // 50-byte gap: same batch
            archived(4, 0, 1000, 100), // 650-byte gap: new batch
            archived(5, 0, 1100, 900), // would make the batch 1000 bytes: new batch at 500
            archived(6, 1, 1200, 10),  // another archive
            item(7, 10, Source::File(CdnKind::Data)),
        ];
        let result = batches(&storage, 500, 100, |_| false);
        let shape: Vec<_> = result.iter().map(|b| (b.archive, b.start, b.end, b.items.clone())).collect();
        assert_eq!(
            shape,
            vec![
                (Some(0), 0, 350, vec![0, 1, 2]),
                (Some(0), 1000, 1100, vec![3]),
                (Some(0), 1100, 2000, vec![4]),
                (Some(1), 1200, 1210, vec![5]),
                (None, 0, 0, vec![6]),
            ]
        );
    }

    #[test]
    fn batches_leave_out_what_is_stored() {
        let storage = vec![archived(1, 0, 0, 100), archived(2, 0, 100, 100), archived(3, 0, 200, 100)];
        let result = batches(&storage, 1000, 1000, |item| item.key == key(2));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].items, vec![0, 2]);
        assert_eq!((result[0].start, result[0].end), (0, 300));
    }
}
