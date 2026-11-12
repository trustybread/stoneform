//! Carrying out an `InstallPlan`: downloads into the game folder.
//! Ported from Waypoint's `CASCInstaller.apply` (0BSD).

use std::collections::HashMap;
use std::path::Path;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream;
use stoneform_domain::{Locale, Product, Region};
use stoneform_tact::blte;
use stoneform_tact::config::{CdnConfig, CdnKind};
use stoneform_tact::keys::ContentKey;

use super::plan::{self, Batch, LooseFile, Source, StorageItem};
use super::{InstallError, InstallPlan, Installer, utf8};
use crate::ports::{ContentDelivery, GameStorage, InstallRecords, LooseFiles, StorageWriter};

/// Range requests in flight at once, and their limits, as Waypoint.
const BATCH_CONCURRENCY: usize = 8;
const MAX_BATCH_BYTES: u64 = 32 << 20;
const MAX_GAP_BYTES: u64 = 256 << 10;
const LOOSE_CONCURRENCY: usize = 4;
const INDEX_CONCURRENCY: usize = 16;
/// Room left on the disk after the download, as Waypoint.
const SPARE_BYTES: u64 = 1 << 30;

/// Where and how to install.
#[derive(Debug, Clone, Copy)]
pub struct InstallTarget<'a> {
    /// The `World of Warcraft` folder.
    pub root: &'a Path,
    pub product: &'a Product,
    pub region: Region,
    pub locale: &'a Locale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// Configs and CDN indexes.
    Preparing,
    Downloading,
    /// Moving files into place and writing records.
    Finishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstallProgress {
    pub stage: InstallStage,
    /// Bytes downloaded or already in place, of `total`.
    pub done: u64,
    pub total: u64,
}

impl<V, D, S, L, R> Installer<V, D, S, L, R>
where
    D: ContentDelivery,
    S: GameStorage,
    L: LooseFiles,
    R: InstallRecords,
{
    /// Downloads and writes everything `plan` lists. What an interrupted run
    /// already stored or staged is kept and counted as done, so calling this
    /// again after a pause or a crash continues where it stopped.
    pub async fn apply(
        &self,
        plan: &InstallPlan,
        target: InstallTarget<'_>,
        progress: &(dyn Fn(InstallProgress) + Send + Sync),
    ) -> Result<(), InstallError> {
        let total = plan.total_bytes();
        let report = |stage, done| progress(InstallProgress { stage, done, total });
        let data_root = target.root.join(&plan.product_config.data_directory);
        let flavor = if plan.product_config.subfolder.is_empty() {
            target.product.flavor_folder
        } else {
            plan.product_config.subfolder.as_str()
        };
        let flavor_root = target.root.join(flavor);
        report(InstallStage::Preparing, 0);

        let mut writer = self.storage.open(&data_root)?;
        let stored: u64 = plan.storage.iter().filter(|item| writer.contains(&item.key)).map(|item| item.size).sum();
        let loose = self.loose_work(plan, &flavor_root);
        let staged = loose.in_place;
        let needed = total.saturating_sub(stored + staged);
        if let Some(available) = self.storage.available_space(target.root)
            && available < needed + SPARE_BYTES
        {
            return Err(InstallError::NotEnoughSpace { needed, available });
        }
        tracing::info!(total, stored, staged, needed, "installing");

        self.storage.write_config(&data_root, &plan.build.build_config, &plan.build_config)?;
        self.storage.write_config(&data_root, &plan.build.cdn_config, &plan.cdn_config)?;
        self.write_indexes(plan, &data_root).await?;

        let mut done = stored + staged;
        report(InstallStage::Downloading, done);
        let batches = plan::batches(&plan.storage, MAX_BATCH_BYTES, MAX_GAP_BYTES, |item| writer.contains(&item.key));
        let mut downloads =
            // Indices, not references: see `Installer::plan`.
            stream::iter(0..batches.len()).map(|n| self.fetch_batch(plan, &batches[n])).buffer_unordered(BATCH_CONCURRENCY);
        while let Some(blobs) = downloads.next().await {
            for (item, blob) in blobs? {
                writer.append(&item.key, &blob, item.full_key)?;
                done += blob.len() as u64;
                report(InstallStage::Downloading, done);
            }
        }
        drop(downloads);
        writer.finish()?;

        let flavor = &flavor_root;
        let mut stages = stream::iter(loose.fetch)
            .map(|(n, bytes)| async move {
                let file = &plan.loose[n];
                let blob = self.fetch_loose(plan, file).await?;
                self.loose.stage(flavor, file, blob).await?;
                Ok::<_, InstallError>(bytes)
            })
            .buffer_unordered(LOOSE_CONCURRENCY);
        while let Some(bytes) = stages.next().await {
            done += bytes?;
            report(InstallStage::Downloading, done);
        }
        drop(stages);

        report(InstallStage::Finishing, done);
        self.loose.commit(&flavor_root, &loose.commit, &plan.obsolete)?;
        self.records.record_build(target.root, plan, target.region)?;
        self.records.set_language(&flavor_root, target.locale)?;
        // Battle.net reads these to recognize the install; the game doesn't need them.
        if let Err(error) = self.records.register_with_battle_net(target.root, plan, target.region, target.locale) {
            tracing::warn!(%error, "could not add the install to Battle.net's records");
        }
        report(InstallStage::Finishing, total);
        tracing::info!(version = %plan.build.version, root = %target.root.display(), "installed");
        Ok(())
    }

    /// Sorts the files outside storage: already current on disk (an update
    /// leaves them alone), staged by an interrupted run, or to fetch. Fetched
    /// by content: several paths with the same content (an app bundle repeats
    /// small resources) are fetched and staged once, and count for all their files.
    fn loose_work(&self, plan: &InstallPlan, flavor_root: &Path) -> LooseWork {
        let mut work = LooseWork::default();
        let mut seen: HashMap<ContentKey, usize> = HashMap::new();
        for (n, file) in plan.loose.iter().enumerate() {
            if self.loose.is_current(flavor_root, file) {
                work.in_place += loose_bytes(file);
                continue;
            }
            work.commit.push(file.clone());
            if self.loose.is_staged(flavor_root, file) {
                work.in_place += loose_bytes(file);
                continue;
            }
            match seen.get(&file.content_key) {
                Some(&slot) => work.fetch[slot].1 += loose_bytes(file),
                None => {
                    seen.insert(file.content_key, work.fetch.len());
                    work.fetch.push((n, loose_bytes(file)));
                }
            }
        }
        work
    }

    /// The CDN indexes the game keeps under `Data/indices`, and the archive
    /// groups the Agent builds from them. Archive indexes are required; the
    /// rest the game can fetch itself, so a failure there is only logged.
    async fn write_indexes(&self, plan: &InstallPlan, data_root: &Path) -> Result<(), InstallError> {
        let cdn_config = CdnConfig::parse(utf8(&plan.cdn_config, "CDN config")?);
        // Owned names: see `Installer::plan` on borrowed closure arguments.
        let mut wanted: Vec<(CdnKind, String, bool)> =
            cdn_config.archives().iter().map(|name| (CdnKind::Data, name.clone(), true)).collect();
        wanted.extend(cdn_config.patch_archives().iter().map(|name| (CdnKind::Patch, name.clone(), false)));
        wanted.extend(cdn_config.file_index().map(|name| (CdnKind::Data, name.to_owned(), false)));
        wanted.extend(cdn_config.patch_file_index().map(|name| (CdnKind::Patch, name.to_owned(), false)));
        wanted.retain(|(_, name, _)| !self.storage.has_index(data_root, name));

        let cdn = &plan.build.cdn;
        let mut fetches = stream::iter(wanted)
            .map(|(kind, name, required)| async move {
                let fetched = self.cdn.index(cdn, kind, &name).await;
                (name, required, fetched)
            })
            .buffer_unordered(INDEX_CONCURRENCY);
        while let Some((name, required, fetched)) = fetches.next().await {
            match fetched {
                Ok(bytes) => self.storage.write_index(data_root, &name, &bytes)?,
                Err(error) if required => return Err(error.into()),
                Err(error) => tracing::warn!(%name, %error, "skipping an optional CDN index"),
            }
        }
        drop(fetches);

        for (group, archives) in [
            (cdn_config.archive_group(), cdn_config.archives()),
            (cdn_config.patch_archive_group(), cdn_config.patch_archives()),
        ] {
            let Some(group) = group else { continue };
            if archives.is_empty() || !archives.iter().all(|name| self.storage.has_index(data_root, name)) {
                continue;
            }
            // The game merges the indexes itself when the group is missing.
            if let Err(error) = self.storage.build_archive_group(data_root, group, archives) {
                tracing::warn!(group, %error, "could not build an archive group");
            }
        }
        Ok(())
    }

    /// Downloads one batch and checks every blob in it against its key.
    async fn fetch_batch<'p>(
        &self,
        plan: &'p InstallPlan,
        batch: &Batch,
    ) -> Result<Vec<(&'p StorageItem, Bytes)>, InstallError> {
        let cdn = &plan.build.cdn;
        let Some(archive) = batch.archive else {
            let item = &plan.storage[batch.items[0]];
            let Source::File(kind) = item.source else { unreachable!("plan::batches puts archived items in ranges") };
            let blob = self.cdn.file(cdn, kind, &item.key).await?;
            check(item, &blob)?;
            return Ok(vec![(item, blob)]);
        };
        let name = &plan.archives[archive as usize];
        let range = self.cdn.archive_range(cdn, name, batch.start..batch.end).await?;
        batch
            .items
            .iter()
            .map(|&position| {
                let item = &plan.storage[position];
                let Source::Archive { offset, .. } = item.source else { unreachable!("a range holds archived items") };
                let from = usize::try_from(offset - batch.start).unwrap_or(usize::MAX);
                let to = from.saturating_add(usize::try_from(item.size).unwrap_or(usize::MAX));
                if to > range.len() {
                    let message = format!("archive range too short for {}", item.key);
                    return Err(stoneform_tact::Error::Malformed(message).into());
                }
                let blob = range.slice(from..to);
                check(item, &blob)?;
                Ok((item, blob))
            })
            .collect()
    }

    async fn fetch_loose(&self, plan: &InstallPlan, file: &LooseFile) -> Result<Bytes, InstallError> {
        let cdn = &plan.build.cdn;
        Ok(match file.source {
            Source::Archive { archive, offset } => {
                let name = &plan.archives[archive as usize];
                self.cdn.archive_range(cdn, name, offset..offset + file.encoded_size).await?
            }
            Source::File(kind) => self.cdn.file(cdn, kind, &file.encoding_key).await?,
        })
    }
}

/// What the files outside storage need.
#[derive(Debug, Default)]
struct LooseWork {
    /// Bytes already current on disk or staged: counted as done.
    in_place: u64,
    /// Files to move into place at the end: all but the current ones.
    commit: Vec<LooseFile>,
    /// One file per content to fetch and stage, with the bytes it stands for.
    fetch: Vec<(usize, u64)>,
}

/// A blob is what its key says: BLTE checked against the encoding key, raw
/// files (the patch manifest) by size.
fn check(item: &StorageItem, blob: &[u8]) -> Result<(), InstallError> {
    if item.raw {
        if item.size != 0 && blob.len() as u64 != item.size {
            return Err(stoneform_tact::Error::Malformed(format!("size of {}", item.key)).into());
        }
        return Ok(());
    }
    blte::verify(blob, &item.key)?;
    Ok(())
}

fn loose_bytes(file: &LooseFile) -> u64 {
    if file.encoded_size > 0 { file.encoded_size } else { file.size }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::ops::Range;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use stoneform_domain::{Product, WOW_FOREVER};
    use stoneform_tact::keys::{ContentKey, EncodingKey};
    use stoneform_tact::product_config::ProductConfig;

    use super::*;
    use crate::ports::{CdnHosts, DiskError, LiveBuild, RemoteError};

    const ARCHIVE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A BLTE container without a chunk table: its encoding key is its MD5.
    fn blte(content: &[u8]) -> (EncodingKey, Bytes) {
        let mut blob = b"BLTE\0\0\0\0N".to_vec();
        blob.extend_from_slice(content);
        (EncodingKey::from_slice(ContentKey::of(&blob).as_bytes()).unwrap(), Bytes::from(blob))
    }

    #[derive(Default)]
    struct FakeCdn {
        archive: Bytes,
        files: HashMap<EncodingKey, Bytes>,
        ranges: Mutex<Vec<Range<u64>>>,
        file_fetches: Mutex<usize>,
    }

    impl ContentDelivery for FakeCdn {
        async fn config(&self, _: &CdnHosts, _: &ContentKey) -> Result<Bytes, RemoteError> {
            unreachable!("applying never fetches configs")
        }
        async fn product_config(&self, _: &CdnHosts, _: &ContentKey) -> Result<Bytes, RemoteError> {
            unreachable!("applying never fetches the product config")
        }
        async fn decoded(&self, _: &CdnHosts, _: &EncodingKey) -> Result<Bytes, RemoteError> {
            unreachable!("applying never fetches manifests")
        }
        async fn index(&self, _: &CdnHosts, _: CdnKind, name: &str) -> Result<Bytes, RemoteError> {
            Ok(Bytes::from(format!("index of {name}")))
        }
        async fn archive_range(&self, _: &CdnHosts, archive: &str, range: Range<u64>) -> Result<Bytes, RemoteError> {
            assert_eq!(archive, ARCHIVE);
            self.ranges.lock().unwrap().push(range.clone());
            Ok(self.archive.slice(range.start as usize..range.end as usize))
        }
        async fn file(&self, _: &CdnHosts, _: CdnKind, key: &EncodingKey) -> Result<Bytes, RemoteError> {
            *self.file_fetches.lock().unwrap() += 1;
            self.files.get(key).cloned().ok_or_else(|| RemoteError::new(format!("no file {key}")))
        }
    }

    #[derive(Default)]
    struct StorageState {
        published: HashMap<EncodingKey, Vec<u8>>,
        pending: HashMap<EncodingKey, Vec<u8>>,
        files: HashSet<String>,
    }

    #[derive(Default, Clone)]
    struct FakeStorage(Arc<Mutex<StorageState>>);

    struct FakeWriter(Arc<Mutex<StorageState>>);

    impl GameStorage for FakeStorage {
        type Writer = FakeWriter;
        fn open(&self, _: &Path) -> Result<FakeWriter, DiskError> {
            Ok(FakeWriter(self.0.clone()))
        }
        fn write_config(&self, _: &Path, key: &ContentKey, _: &[u8]) -> Result<(), DiskError> {
            self.0.lock().unwrap().files.insert(format!("config/{key}"));
            Ok(())
        }
        fn has_index(&self, _: &Path, name: &str) -> bool {
            self.0.lock().unwrap().files.contains(&format!("indices/{name}"))
        }
        fn write_index(&self, _: &Path, name: &str, _: &[u8]) -> Result<(), DiskError> {
            self.0.lock().unwrap().files.insert(format!("indices/{name}"));
            Ok(())
        }
        fn build_archive_group(&self, _: &Path, _: &str, _: &[String]) -> Result<(), DiskError> {
            Ok(())
        }
        fn available_space(&self, _: &Path) -> Option<u64> {
            Some(u64::MAX / 2)
        }
    }

    impl StorageWriter for FakeWriter {
        fn contains(&self, key: &EncodingKey) -> bool {
            let state = self.0.lock().unwrap();
            state.published.contains_key(key) || state.pending.contains_key(key)
        }
        fn append(&mut self, key: &EncodingKey, blob: &[u8], _: bool) -> Result<(), DiskError> {
            self.0.lock().unwrap().pending.insert(*key, blob.to_vec());
            Ok(())
        }
        fn finish(self) -> Result<(), DiskError> {
            let mut state = self.0.lock().unwrap();
            let pending = std::mem::take(&mut state.pending);
            state.published.extend(pending);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeLoose {
        current: Mutex<HashSet<String>>,
        staged: Mutex<HashMap<ContentKey, Bytes>>,
        committed: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
    }

    impl LooseFiles for FakeLoose {
        fn is_current(&self, _: &Path, file: &LooseFile) -> bool {
            self.current.lock().unwrap().contains(&file.path)
        }
        fn is_staged(&self, _: &Path, file: &LooseFile) -> bool {
            self.staged.lock().unwrap().contains_key(&file.content_key)
        }
        async fn stage(&self, _: &Path, file: &LooseFile, blob: Bytes) -> Result<(), DiskError> {
            self.staged.lock().unwrap().insert(file.content_key, blob);
            Ok(())
        }
        fn commit(&self, _: &Path, files: &[LooseFile], obsolete: &[String]) -> Result<(), DiskError> {
            self.committed.lock().unwrap().extend(files.iter().map(|file| file.path.clone()));
            self.removed.lock().unwrap().extend(obsolete.iter().cloned());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeRecords(Mutex<Vec<&'static str>>);

    impl InstallRecords for FakeRecords {
        fn installed(&self, _: &Path, _: &Product) -> Result<Option<crate::ports::InstalledBuild>, DiskError> {
            Ok(None)
        }
        fn record_build(&self, _: &Path, _: &InstallPlan, _: Region) -> Result<(), DiskError> {
            self.0.lock().unwrap().push("build");
            Ok(())
        }
        fn set_language(&self, _: &Path, _: &Locale) -> Result<(), DiskError> {
            self.0.lock().unwrap().push("language");
            Ok(())
        }
        fn register_with_battle_net(&self, _: &Path, _: &InstallPlan, _: Region, _: &Locale) -> Result<(), DiskError> {
            self.0.lock().unwrap().push("battle.net");
            Err(DiskError::new("no Agent here"))
        }
    }

    struct Fixture {
        cdn: FakeCdn,
        plan: InstallPlan,
        keys: [EncodingKey; 3],
    }

    /// Two blobs in one archive with a gap between them, one file of its
    /// own, and one loose file.
    fn fixture() -> Fixture {
        let (a, blob_a) = blte(b"first blob");
        let (b, blob_b) = blte(b"second blob, a bit longer");
        let (c, blob_c) = blte(b"a file of its own");
        let (loose_key, loose_blob) = blte(b"#!/bin/sh\n");
        let mut archive = blob_a.to_vec();
        archive.extend_from_slice(&[0; 10]);
        let offset_b = archive.len() as u64;
        archive.extend_from_slice(&blob_b);
        let item =
            |key, size: usize, source| StorageItem { key, size: size as u64, full_key: false, raw: false, source };
        let storage = vec![
            item(a, blob_a.len(), Source::Archive { archive: 0, offset: 0 }),
            item(b, blob_b.len(), Source::Archive { archive: 0, offset: offset_b }),
            item(c, blob_c.len(), Source::File(CdnKind::Data)),
        ];
        let loose = vec![LooseFile {
            path: "run.sh".to_owned(),
            content_key: ContentKey::of(b"#!/bin/sh\n"),
            encoding_key: loose_key,
            size: 10,
            encoded_size: loose_blob.len() as u64,
            source: Source::File(CdnKind::Data),
        }];
        let cdn = FakeCdn {
            archive: Bytes::from(archive),
            files: HashMap::from([(c, blob_c), (loose_key, loose_blob)]),
            ranges: Mutex::default(),
            file_fetches: Mutex::default(),
        };
        let product_config =
            ProductConfig::parse(include_bytes!("../../../stoneform-tact/tests/fixtures/wowf-product.json")).unwrap();
        let plan = InstallPlan {
            product: WOW_FOREVER,
            build: LiveBuild {
                version: "1.60.1.70888".to_owned(),
                build_config: ContentKey::of(b"build"),
                cdn_config: ContentKey::of(b"cdn"),
                product_config: None,
                key_ring: None,
                cdn: CdnHosts {
                    path: "tpr/wow".to_owned(),
                    config_path: "tpr/configs/data".to_owned(),
                    servers: vec![],
                    hosts_field: String::new(),
                    servers_field: String::new(),
                },
            },
            tags: "OSX enUS".to_owned(),
            product_config,
            build_config: Bytes::from_static(b"build"),
            cdn_config: Bytes::from(format!("archives = {ARCHIVE}\n")),
            archives: vec![ARCHIVE.to_owned()],
            storage,
            loose,
            obsolete: vec![],
        };
        Fixture { cdn, plan, keys: [a, b, c] }
    }

    type TestInstaller = Installer<(), FakeCdn, FakeStorage, FakeLoose, FakeRecords>;

    fn installer(cdn: FakeCdn, storage: FakeStorage) -> TestInstaller {
        Installer::new((), cdn).with_disk(storage, FakeLoose::default(), FakeRecords::default())
    }

    async fn run(installer: &TestInstaller, plan: &InstallPlan) -> (Result<(), InstallError>, Vec<InstallProgress>) {
        let locale = Locale::parse("enUS").unwrap();
        let root = PathBuf::from("/tmp/World of Warcraft");
        let target = InstallTarget { root: &root, product: &WOW_FOREVER, region: Region::Eu, locale: &locale };
        let seen = Mutex::new(Vec::new());
        let result = installer.apply(plan, target, &|progress| seen.lock().unwrap().push(progress)).await;
        (result, seen.into_inner().unwrap())
    }

    #[tokio::test]
    async fn installs_everything_and_reports_progress() {
        let Fixture { cdn, plan, keys } = fixture();
        let storage = FakeStorage::default();
        let installer = installer(cdn, storage.clone());

        let (result, progress) = run(&installer, &plan).await;

        result.unwrap();
        let state = storage.0.lock().unwrap();
        assert!(keys.iter().all(|key| state.published.contains_key(key)));
        assert!(state.pending.is_empty());
        assert!(state.files.contains(&format!("indices/{ARCHIVE}")));
        assert_eq!(state.files.iter().filter(|file| file.starts_with("config/")).count(), 2);
        assert_eq!(*installer.loose.committed.lock().unwrap(), vec!["run.sh".to_owned()]);
        assert_eq!(*installer.records.0.lock().unwrap(), vec!["build", "language", "battle.net"]);
        // One range request covers both archived blobs.
        assert_eq!(installer.cdn.ranges.lock().unwrap().len(), 1);
        let last = progress.last().unwrap();
        assert_eq!(
            (last.stage, last.done, last.total),
            (InstallStage::Finishing, plan.total_bytes(), plan.total_bytes())
        );
        assert!(progress.windows(2).all(|pair| pair[0].done <= pair[1].done));
    }

    #[tokio::test]
    async fn resumes_without_downloading_what_is_stored() {
        let Fixture { cdn, plan, keys } = fixture();
        let storage = FakeStorage::default();
        storage.0.lock().unwrap().published.insert(keys[0], b"stored earlier".to_vec());
        let installer = installer(cdn, storage.clone());

        let (result, progress) = run(&installer, &plan).await;

        result.unwrap();
        let ranges = installer.cdn.ranges.lock().unwrap().clone();
        let Source::Archive { offset, .. } = plan.storage[1].source else { unreachable!() };
        assert_eq!(ranges, vec![offset..offset + plan.storage[1].size]);
        let first_download = progress.iter().find(|p| p.stage == InstallStage::Downloading).unwrap();
        assert_eq!(first_download.done, plan.storage[0].size);
    }

    #[tokio::test]
    async fn fetches_shared_content_once_and_still_reaches_the_total() {
        let Fixture { cdn, mut plan, .. } = fixture();
        let copy = LooseFile { path: "copy/run.sh".to_owned(), ..plan.loose[0].clone() };
        plan.loose.push(copy);
        let installer = installer(cdn, FakeStorage::default());

        let (result, progress) = run(&installer, &plan).await;

        result.unwrap();
        // One fetch for the storage file of its own, one for the shared loose content.
        assert_eq!(*installer.cdn.file_fetches.lock().unwrap(), 2);
        assert_eq!(installer.loose.committed.lock().unwrap().len(), 2);
        let downloaded = progress.iter().filter(|p| p.stage == InstallStage::Downloading).map(|p| p.done).max();
        assert_eq!(downloaded, Some(plan.total_bytes()));
    }

    /// An update: storage holds the old build's blobs, a loose file is current
    /// already, and one the new build dropped goes away.
    #[tokio::test]
    async fn updates_only_what_changed_and_removes_what_is_gone() {
        let Fixture { cdn, mut plan, keys } = fixture();
        plan.obsolete = vec!["old/gone.txt".to_owned()];
        let storage = FakeStorage::default();
        storage.0.lock().unwrap().published.insert(keys[0], b"old blob".to_vec());
        let installer = installer(cdn, storage.clone());
        installer.loose.current.lock().unwrap().insert("run.sh".to_owned());

        let (result, progress) = run(&installer, &plan).await;

        result.unwrap();
        // The current loose file is neither fetched nor moved; only the
        // storage file of its own is fetched whole.
        assert_eq!(*installer.cdn.file_fetches.lock().unwrap(), 1);
        assert!(installer.loose.committed.lock().unwrap().is_empty());
        assert_eq!(*installer.loose.removed.lock().unwrap(), vec!["old/gone.txt".to_owned()]);
        assert_eq!(progress.last().map(|p| p.done), Some(plan.total_bytes()));
    }

    #[tokio::test]
    async fn stops_at_a_corrupt_blob_and_publishes_nothing() {
        let Fixture { mut cdn, plan, keys } = fixture();
        cdn.files.insert(keys[2], Bytes::from_static(b"BLTE\0\0\0\0Ntampered"));
        let storage = FakeStorage::default();
        let installer = installer(cdn, storage.clone());

        let (result, _) = run(&installer, &plan).await;

        assert!(matches!(result, Err(InstallError::Format(_))), "{result:?}");
        assert!(storage.0.lock().unwrap().published.is_empty());
        assert!(installer.records.0.lock().unwrap().is_empty());
    }
}
