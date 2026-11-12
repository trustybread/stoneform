//! The `Install` use case: from the live build to files in the game folder.

mod apply;
pub mod plan;

pub use apply::{InstallProgress, InstallStage, InstallTarget};

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream;
use stoneform_domain::{Locale, Product, Region};
use stoneform_tact::archive_index::ArchiveIndex;
use stoneform_tact::config::{BuildConfig, CdnConfig, CdnKind};
use stoneform_tact::download::DownloadManifest;
use stoneform_tact::encoding::EncodingTable;
use stoneform_tact::install::InstallManifest;
use stoneform_tact::keys::EncodingKey;
use stoneform_tact::languages::Languages;
use stoneform_tact::product_config::ProductConfig;

use crate::ports::{ContentDelivery, DiskError, LiveBuild, RemoteError, VersionSource};
use plan::{LooseFile, PlanError, Source, StorageItem};

/// Archive indexes fetched at once while planning, as Waypoint.
const INDEX_CONCURRENCY: usize = 16;

/// Everything an install of one build downloads and writes.
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub product: Product,
    pub build: LiveBuild,
    /// The tag string the files were selected with; `.build.info` records it.
    pub tags: String,
    pub product_config: ProductConfig,
    /// The raw configs, as the game keeps them under `Data/config`.
    pub build_config: Bytes,
    pub cdn_config: Bytes,
    /// The CDN config's archives, in its order: `Source::Archive` numbers index this.
    pub archives: Vec<String>,
    /// Local storage files, in download order.
    pub storage: Vec<StorageItem>,
    pub loose: Vec<LooseFile>,
}

impl InstallPlan {
    pub fn storage_bytes(&self) -> u64 {
        self.storage.iter().map(|item| item.size).sum()
    }

    /// Encoded sizes where an index told them, decoded sizes otherwise.
    pub fn loose_bytes(&self) -> u64 {
        self.loose.iter().map(|file| if file.encoded_size > 0 { file.encoded_size } else { file.size }).sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.storage_bytes() + self.loose_bytes()
    }

    /// Storage files no archive holds: fetched one by one.
    pub fn unarchived(&self) -> usize {
        self.storage.iter().filter(|item| matches!(item.source, Source::File(_))).count()
    }
}

/// How far planning got, for the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStep {
    Version,
    Manifests,
    Indexes { done: usize, total: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error(transparent)]
    Remote(#[from] RemoteError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Format(#[from] stoneform_tact::Error),
    #[error("Blizzard does not publish {0} for this build")]
    Missing(&'static str),
    #[error(transparent)]
    Disk(#[from] DiskError),
    #[error("not enough free space: {needed} bytes needed, {available} available")]
    NotEnoughSpace { needed: u64, available: u64 },
}

/// The `Install` use case. Planning needs only the version service and the
/// CDN; applying a plan also needs the disk-side ports, added by `with_disk`.
#[derive(Debug)]
pub struct Installer<V, D, S = (), L = (), R = ()> {
    versions: V,
    cdn: D,
    storage: S,
    loose: L,
    records: R,
}

impl<V, D> Installer<V, D> {
    pub fn new(versions: V, cdn: D) -> Self {
        Self { versions, cdn, storage: (), loose: (), records: () }
    }
}

impl<V, D, S, L, R> Installer<V, D, S, L, R> {
    pub fn with_disk<S2, L2, R2>(self, storage: S2, loose: L2, records: R2) -> Installer<V, D, S2, L2, R2> {
        Installer { versions: self.versions, cdn: self.cdn, storage, loose, records }
    }
}

impl<V: VersionSource, D: ContentDelivery, S, L, R> Installer<V, D, S, L, R> {
    /// Works out what installing `product`'s live build in `locale` takes.
    /// `stored` says which storage files are already in place (an interrupted
    /// install); they are left out.
    pub async fn plan(
        &self,
        product: &Product,
        region: Region,
        locale: &Locale,
        stored: impl Fn(&EncodingKey) -> bool + Send + Sync,
        progress: &(dyn Fn(PlanStep) + Send + Sync),
    ) -> Result<InstallPlan, InstallError> {
        progress(PlanStep::Version);
        let build = self.versions.live(product.code, region).await?;
        let cdn = &build.cdn;
        let product_config_key = build.product_config.ok_or(InstallError::Missing("a product config"))?;
        let (build_config, cdn_config, product_config) = futures_util::try_join!(
            self.cdn.config(cdn, &build.build_config),
            self.cdn.config(cdn, &build.cdn_config),
            self.cdn.product_config(cdn, &product_config_key),
        )?;
        let product_config = ProductConfig::parse(&product_config)?;
        let tags = product_config.tag_string(region.as_str(), &Languages::new(locale.as_str(), None));
        let parsed_build = BuildConfig::parse(utf8(&build_config, "build config")?);
        let parsed_cdn = CdnConfig::parse(utf8(&cdn_config, "CDN config")?);

        progress(PlanStep::Manifests);
        let (install_key, encoding_key, download_key) =
            (parsed_build.install()?, parsed_build.encoding()?, parsed_build.download()?);
        let (install, encoding, download) = futures_util::try_join!(
            self.cdn.decoded(cdn, &install_key),
            self.cdn.decoded(cdn, &encoding_key),
            self.cdn.decoded(cdn, &download_key),
        )?;
        let mut selection = plan::select(
            &parsed_build,
            &tags,
            &InstallManifest::parse(&install)?,
            &EncodingTable::parse(&encoding)?,
            &DownloadManifest::parse(&download)?,
            stored,
        )?;
        tracing::info!(
            version = %build.version,
            %tags,
            storage_files = selection.storage.len(),
            loose_files = selection.loose.len(),
            "selected the install's files"
        );

        let archives: Vec<String> = parsed_cdn.archives().to_vec();
        let total = archives.len();
        progress(PlanStep::Indexes { done: 0, total });
        // Closures take indices, not references: rustc cannot yet prove a
        // spawned future `Send` when stream closures take borrowed arguments.
        let archive_list = &archives;
        let mut indexes = stream::iter(0..archives.len())
            .map(|number| async move { (number, self.cdn.index(cdn, CdnKind::Data, &archive_list[number]).await) })
            .buffer_unordered(INDEX_CONCURRENCY);
        let mut done = 0;
        while let Some((number, index)) = indexes.next().await {
            let index = index?;
            let archive = u32::try_from(number).map_err(|_| InstallError::Missing("an archive list this long"))?;
            plan::locate(&mut selection, archive, ArchiveIndex::parse(&index)?.iter());
            done += 1;
            progress(PlanStep::Indexes { done, total });
        }
        drop(indexes);
        let unarchived = selection.storage.iter().any(|item| matches!(item.source, Source::File(_)))
            || selection.loose.iter().any(|file| matches!(file.source, Source::File(_)));
        if let (true, Some(file_index)) = (unarchived, parsed_cdn.file_index()) {
            let index = self.cdn.index(cdn, CdnKind::Data, file_index).await?;
            plan::size_unarchived(&mut selection, ArchiveIndex::parse(&index)?.iter());
        }
        plan::order_for_download(&mut selection.storage);

        let plan = InstallPlan {
            product: *product,
            build,
            tags,
            product_config,
            build_config,
            cdn_config,
            archives,
            storage: selection.storage,
            loose: selection.loose,
        };
        tracing::info!(
            storage_bytes = plan.storage_bytes(),
            loose_bytes = plan.loose_bytes(),
            unarchived = plan.unarchived(),
            "install planned"
        );
        Ok(plan)
    }
}

fn utf8<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str, InstallError> {
    std::str::from_utf8(bytes).map_err(|_| stoneform_tact::Error::Malformed(format!("{what} is not UTF-8")).into())
}
