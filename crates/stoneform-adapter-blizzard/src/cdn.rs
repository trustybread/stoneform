use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::ops::Range;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use stoneform_app::ports::{CdnHosts, ContentDelivery, RemoteError};
use stoneform_tact::archive_index::ArchiveIndex;
use stoneform_tact::blte;
use stoneform_tact::config::{CdnKind, cdn_path, product_config_path};
use stoneform_tact::keys::{ContentKey, EncodingKey};

use crate::BlizzardHttp;

#[derive(Debug, thiserror::Error)]
enum ContentError {
    #[error("{what} {key} does not match its hash")]
    Mismatch { what: &'static str, key: String },
    #[error("the decoding task stopped: {0}")]
    Task(String),
}

/// The `ContentDelivery` port on Blizzard's CDN, with a disk cache.
///
/// Configs, product configs, CDN indexes and decoded manifests are kept under
/// the cache folder by hash: they never change, so a cached copy is as good as
/// the CDN's and a second plan (or a resumed install) starts at once. Archive
/// ranges and loose files are not cached; they go straight to the game folder.
#[derive(Debug, Clone)]
pub struct HttpContentDelivery {
    http: BlizzardHttp,
    cache: PathBuf,
}

impl HttpContentDelivery {
    pub fn new(http: BlizzardHttp, cache: impl Into<PathBuf>) -> Self {
        Self { http, cache: cache.into() }
    }

    fn cache_path(&self, folder: &str, hex: &str) -> PathBuf {
        self.cache.join(folder).join(&hex[..2]).join(&hex[2..4]).join(hex)
    }

    /// A small file by hash: from the cache when it is there and intact,
    /// else from the CDN, checked and then cached.
    async fn small(
        &self,
        cdn: &CdnHosts,
        folder: &str,
        remote_path: String,
        key: &ContentKey,
        what: &'static str,
    ) -> Result<Bytes, RemoteError> {
        let hex = key.to_hex();
        let local = self.cache_path(folder, &hex);
        if let Ok(data) = fs::read(&local)
            && ContentKey::of(&data) == *key
        {
            return Ok(Bytes::from(data));
        }
        let data = self.http.fetch(&cdn.servers, &remote_path, None).await.map_err(RemoteError::new)?;
        if ContentKey::of(&data) != *key {
            return Err(RemoteError::new(ContentError::Mismatch { what, key: hex }));
        }
        write_atomically(&local, &data).map_err(RemoteError::new)?;
        Ok(data)
    }
}

impl ContentDelivery for HttpContentDelivery {
    async fn config(&self, cdn: &CdnHosts, key: &ContentKey) -> Result<Bytes, RemoteError> {
        let remote = cdn_path(&cdn.path, CdnKind::Config, &key.to_hex());
        self.small(cdn, "config", remote, key, "config").await
    }

    async fn product_config(&self, cdn: &CdnHosts, key: &ContentKey) -> Result<Bytes, RemoteError> {
        let remote = product_config_path(&cdn.config_path, &key.to_hex());
        self.small(cdn, "product", remote, key, "product config").await
    }

    async fn decoded(&self, cdn: &CdnHosts, key: &EncodingKey) -> Result<Bytes, RemoteError> {
        let local = self.cache_path("decoded", &key.to_hex());
        if local.is_file() {
            return map(&local).map_err(RemoteError::new);
        }
        let blob = self
            .http
            .fetch(&cdn.servers, &cdn_path(&cdn.path, CdnKind::Data, &key.to_hex()), None)
            .await
            .map_err(RemoteError::new)?;
        let key = *key;
        let target = local.clone();
        // Checking and inflating a 100 MB table is work for a blocking thread.
        tokio::task::spawn_blocking(move || -> Result<(), RemoteError> {
            blte::verify(&blob, &key).map_err(RemoteError::new)?;
            write_with(&target, |file| blte::decode_to(&blob[..], file).map(drop).map_err(io::Error::other))
                .map_err(RemoteError::new)
        })
        .await
        .map_err(|error| RemoteError::new(ContentError::Task(error.to_string())))??;
        map(&local).map_err(RemoteError::new)
    }

    async fn index(&self, cdn: &CdnHosts, kind: CdnKind, name: &str) -> Result<Bytes, RemoteError> {
        let local = self.cache.join("indices").join(format!("{name}.index"));
        if local.is_file() {
            return map(&local).map_err(RemoteError::new);
        }
        let remote = format!("{}.index", cdn_path(&cdn.path, kind, name));
        let data = self.http.fetch(&cdn.servers, &remote, None).await.map_err(RemoteError::new)?;
        // An index names itself by the hash of its footer; parsing checks it.
        ArchiveIndex::parse(&data).map_err(RemoteError::new)?;
        write_atomically(&local, &data).map_err(RemoteError::new)?;
        Ok(data)
    }

    async fn archive_range(&self, cdn: &CdnHosts, archive: &str, range: Range<u64>) -> Result<Bytes, RemoteError> {
        let remote = cdn_path(&cdn.path, CdnKind::Data, archive);
        let inclusive = range.start..=range.end.saturating_sub(1);
        self.http.fetch(&cdn.servers, &remote, Some(inclusive)).await.map_err(RemoteError::new)
    }

    async fn file(&self, cdn: &CdnHosts, kind: CdnKind, key: &EncodingKey) -> Result<Bytes, RemoteError> {
        let remote = cdn_path(&cdn.path, kind, &key.to_hex());
        self.http.fetch(&cdn.servers, &remote, None).await.map_err(RemoteError::new)
    }
}

/// A read-only memory map of a cached file, so a 100 MB table costs page
/// cache rather than heap.
fn map(path: &Path) -> io::Result<Bytes> {
    let file = File::open(path)?;
    // SAFETY: cache files are written once, next to their final name, and
    // renamed into place; nothing changes them afterwards.
    let mapped = unsafe { memmap2::Mmap::map(&file)? };
    Ok(Bytes::from_owner(mapped))
}

fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    write_with(path, |file| io::Write::write_all(file, data))
}

/// Writes through `fill` to a temporary file beside `path`, then renames it.
fn write_with(path: &Path, fill: impl FnOnce(&mut BufWriter<File>) -> io::Result<()>) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::other("cache path has no parent"))?;
    fs::create_dir_all(dir)?;
    let temp = path.with_extension("partial");
    let mut file = BufWriter::new(File::create(&temp)?);
    fill(&mut file)?;
    io::Write::flush(&mut file)?;
    file.into_inner().map_err(io::IntoInnerError::into_error)?.sync_all()?;
    fs::rename(&temp, path)
}
