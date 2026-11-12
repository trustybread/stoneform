//! Driven ports: what the core needs from the outside world.

use std::error::Error as StdError;
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use stoneform_domain::{AccountId, GameCodename, LaunchToken, Locale, Product, Region};
use stoneform_tact::config::CdnKind;
use stoneform_tact::keys::{ContentKey, EncodingKey};

use crate::install::InstallPlan;
use crate::install::plan::LooseFile;

type BoxError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("sign-in was cancelled")]
    Cancelled,
    #[error("sign-in failed: {0}")]
    Failed(String),
}

/// Shows Blizzard's web login and returns the launch token it hands out.
pub trait LoginFlow: Send + Sync {
    /// Resolves with the token from the login page's final redirect, or with
    /// [`LoginError::Cancelled`] when the user closes the window.
    fn acquire(&self, region: Region) -> impl Future<Output = Result<LaunchToken, LoginError>> + Send;

    /// Forgets the web session (cookies) kept for `account`.
    fn forget(&self, account: &AccountId) -> impl Future<Output = Result<(), LoginError>> + Send;
}

#[derive(Debug, thiserror::Error)]
#[error("could not access saved sign-in: {0}")]
pub struct SecretError(#[source] pub BoxError);

/// Keeps launch tokens, one per account and codename.
pub trait SecretStore: Send + Sync {
    /// Whether a token is kept for `account` and `codename`, without reading
    /// it. Cheap and quiet: the Keychain asks the user before handing out a
    /// secret, not before saying one exists.
    fn has_token(&self, account: &AccountId, codename: GameCodename) -> Result<bool, SecretError>;
    fn token(&self, account: &AccountId, codename: GameCodename) -> Result<Option<LaunchToken>, SecretError>;
    fn save(&self, token: &LaunchToken, codename: GameCodename) -> Result<(), SecretError>;
    /// Removes every token of `account`; succeeds if there were none.
    fn forget(&self, account: &AccountId) -> Result<(), SecretError>;
}

/// What StoneForm remembers between runs. Never secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub region: Option<Region>,
    pub account: Option<AccountId>,
    /// The `World of Warcraft` folder WoW: Forever is installed in.
    pub install_path: Option<PathBuf>,
    /// The game's language, chosen at install.
    pub locale: Option<Locale>,
}

#[derive(Debug, thiserror::Error)]
#[error("could not access settings: {0}")]
pub struct SettingsError(#[source] pub BoxError);

pub trait SettingsStore: Send + Sync {
    /// The saved settings, or the defaults when nothing was saved yet.
    fn load(&self) -> Result<Settings, SettingsError>;
    fn save(&self, settings: &Settings) -> Result<(), SettingsError>;
}

/// Where a product's files are on Blizzard's CDN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdnHosts {
    /// The product's folder on every mirror, e.g. `tpr/wow`.
    pub path: String,
    /// Where product configs live, e.g. `tpr/configs/data`.
    pub config_path: String,
    /// Mirror base URLs, most preferred first, e.g. `https://level3.ssl.blizzard.com`.
    pub servers: Vec<String>,
    /// The `cdns` row's `Hosts` and `Servers` as published; `.build.info` keeps them.
    pub hosts_field: String,
    pub servers_field: String,
}

/// The build Blizzard currently serves for a product in a region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBuild {
    /// E.g. `1.60.1.70888`.
    pub version: String,
    pub build_config: ContentKey,
    pub cdn_config: ContentKey,
    pub product_config: Option<ContentKey>,
    pub key_ring: Option<String>,
    pub cdn: CdnHosts,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RemoteError(#[source] pub BoxError);

impl RemoteError {
    pub fn new(error: impl Into<BoxError>) -> Self {
        Self(error.into())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct DiskError(#[source] pub BoxError);

impl DiskError {
    pub fn new(error: impl Into<BoxError>) -> Self {
        Self(error.into())
    }
}

/// Files the game keeps outside local storage (its `.app` and friends):
/// staged one by one, then moved into place together.
pub trait LooseFiles: Send + Sync {
    /// Whether `file` is already staged intact under `flavor_root`, by an
    /// earlier, interrupted run.
    fn is_staged(&self, flavor_root: &Path, file: &LooseFile) -> bool;

    /// Decodes `blob`, checks it against the file's content key and stages it.
    fn stage(
        &self,
        flavor_root: &Path,
        file: &LooseFile,
        blob: Bytes,
    ) -> impl Future<Output = Result<(), DiskError>> + Send;

    /// Moves every staged file to its place under `flavor_root`.
    fn commit(&self, flavor_root: &Path, files: &[LooseFile]) -> Result<(), DiskError>;
}

/// The game's local CASC storage (`<root>/Data`), shared by every WoW flavor.
pub trait GameStorage: Send + Sync {
    type Writer: StorageWriter;

    /// Opens or creates the storage under `data_root`, takes its lock and
    /// resumes an interrupted session from its journal.
    fn open(&self, data_root: &Path) -> Result<Self::Writer, DiskError>;

    /// Keeps a build or CDN config as the game does: `config/xx/yy/<hash>`.
    fn write_config(&self, data_root: &Path, key: &ContentKey, bytes: &[u8]) -> Result<(), DiskError>;

    fn has_index(&self, data_root: &Path, name: &str) -> bool;

    /// Keeps a CDN index as the game does: `indices/<name>.index`.
    fn write_index(&self, data_root: &Path, name: &str, bytes: &[u8]) -> Result<(), DiskError>;

    /// Builds the archive-group index the Agent makes from `archives`' indexes
    /// and checks it is named `group`.
    fn build_archive_group(&self, data_root: &Path, group: &str, archives: &[String]) -> Result<(), DiskError>;

    /// Bytes free for `path`'s volume.
    fn available_space(&self, path: &Path) -> Option<u64>;
}

/// One open session on the game's local storage.
pub trait StorageWriter: Send {
    fn contains(&self, key: &EncodingKey) -> bool;
    /// Appends a blob exactly as the CDN served it.
    fn append(&mut self, key: &EncodingKey, blob: &[u8], full_key: bool) -> Result<(), DiskError>;
    /// Publishes everything appended: new index files, then `shmem`.
    fn finish(self) -> Result<(), DiskError>;
}

/// The build `.build.info` says is installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBuild {
    pub version: String,
    /// The build config's hex key.
    pub build_key: String,
}

/// The records that say which build is installed, for the game and Battle.net.
pub trait InstallRecords: Send + Sync {
    /// The installed build of `product` under `root`, if `.build.info` has an
    /// active row for it and the game's app is there.
    fn installed(&self, root: &Path, product: &Product) -> Result<Option<InstalledBuild>, DiskError>;

    /// Writes `.build.info` (keeping other flavors' rows) and the flavor's `.flavor.info`.
    fn record_build(&self, root: &Path, plan: &InstallPlan, region: Region) -> Result<(), DiskError>;

    /// Sets the game's text and speech language in `<flavor>/WTF/Config.wtf`.
    fn set_language(&self, flavor_root: &Path, locale: &Locale) -> Result<(), DiskError>;

    /// Adds the records Battle.net reads (`.product.db`, and the Agent's
    /// `product.db` where the Agent is installed), so it recognizes the install.
    fn register_with_battle_net(
        &self,
        root: &Path,
        plan: &InstallPlan,
        region: Region,
        locale: &Locale,
    ) -> Result<(), DiskError>;
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SystemError(#[source] pub BoxError);

impl SystemError {
    pub fn new(error: impl Into<BoxError>) -> Self {
        Self(error.into())
    }
}

/// Hands the launch token to the game the way Battle.net does: encrypted, in
/// the preferences the game reads on start.
pub trait TokenHandoff: Send + Sync {
    fn write(
        &self,
        codename: GameCodename,
        token: &LaunchToken,
        region: Region,
        locale: Option<&Locale>,
    ) -> Result<(), SystemError>;
}

/// The processes that matter to StoneForm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Running {
    /// A process running from inside the install folder: the game.
    pub game: Option<u32>,
    /// The Battle.net app, or its background Agent that patches games.
    pub battle_net: bool,
}

pub trait GameProcess: Send + Sync {
    fn running(&self, install_root: &Path) -> Running;

    /// Starts the executable of the app bundle at `app`, detached and on its
    /// own, from `working_directory`. Returns its process id.
    fn spawn(&self, app: &Path, arguments: &[String], working_directory: &Path) -> Result<u32, SystemError>;

    /// Brings a running app to the front; false when it is gone.
    fn activate(&self, pid: u32) -> bool;
}

/// Blizzard's version service.
pub trait VersionSource: Send + Sync {
    fn live(&self, product: &str, region: Region) -> impl Future<Output = Result<LiveBuild, RemoteError>> + Send;
}

/// Blizzard's CDN. Every answer is checked against the key it was asked for,
/// so callers can trust what they get.
pub trait ContentDelivery: Send + Sync {
    /// A build or CDN config, by its hash.
    fn config(&self, cdn: &CdnHosts, key: &ContentKey) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;

    /// The product config (JSON).
    fn product_config(
        &self,
        cdn: &CdnHosts,
        key: &ContentKey,
    ) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;

    /// A data file decoded from BLTE: a manifest or the encoding table. Large;
    /// adapters cache it on disk and may hand out a memory map.
    fn decoded(&self, cdn: &CdnHosts, key: &EncodingKey) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;

    /// A CDN index (`<name>.index`) of an archive, patch archive or file index.
    fn index(
        &self,
        cdn: &CdnHosts,
        kind: CdnKind,
        name: &str,
    ) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;

    /// Bytes `range` of a CDN archive, as stored (BLTE blobs back to back).
    fn archive_range(
        &self,
        cdn: &CdnHosts,
        archive: &str,
        range: Range<u64>,
    ) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;

    /// A file stored on its own on the CDN, as served.
    fn file(
        &self,
        cdn: &CdnHosts,
        kind: CdnKind,
        key: &EncodingKey,
    ) -> impl Future<Output = Result<Bytes, RemoteError>> + Send;
}
