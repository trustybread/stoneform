//! The rest of the game's `Data` folder the Agent lays down next to the
//! storage: build and CDN configs, and the CDN archive indexes.
//!
//! ```text
//! Data/config/xx/yy/{hash}    build and CDN configs, by the hash's first two byte pairs
//! Data/indices/{hash}.index   CDN archive indexes, and the archive groups built from them
//! Data/data/                  the local storage (see CascStorage)
//! ```

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use stoneform_tact::archive_group;

use crate::error::{At, Error, Result};
use crate::fsutil::{sync_dir, temp_path, write_atomically};
use crate::storage::CascStorage;

/// An archive index's footer, which an archive group is named by.
const FOOTER_SIZE: u64 = 28;

/// A game's `Data` folder (WoW: `<install root>/Data`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir {
    dir: PathBuf,
}

impl DataDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The local storage in `data`.
    pub fn storage(&self) -> CascStorage {
        CascStorage::new(self.dir.join("data"))
    }

    /// `config/xx/yy/{hash}`, where the Agent keeps a config and the game
    /// looks for it. `hash` is the config's 32-digit hex key, either case.
    pub fn config_path(&self, hash: &str) -> Result<PathBuf> {
        let hash = hash_name(hash)?;
        Ok(self.dir.join("config").join(&hash[..2]).join(&hash[2..4]).join(&hash))
    }

    /// Writes a config (build or CDN) to its place atomically; returns where.
    pub fn write_config(&self, hash: &str, contents: &[u8]) -> Result<PathBuf> {
        let path = self.config_path(hash)?;
        write_new(&path, contents)?;
        Ok(path)
    }

    /// `indices`, the CDN archive indexes' folder.
    pub fn indices_path(&self) -> PathBuf {
        self.dir.join("indices")
    }

    /// `indices/{name}.index`. `name` is the archive's 32-digit hex key.
    pub fn index_path(&self, name: &str) -> Result<PathBuf> {
        Ok(self.indices_path().join(format!("{}.index", hash_name(name)?)))
    }

    /// Writes a CDN archive index atomically; returns where.
    pub fn write_index(&self, name: &str, contents: &[u8]) -> Result<PathBuf> {
        let path = self.index_path(name)?;
        write_new(&path, contents)?;
        Ok(path)
    }

    /// Copies a cached CDN archive index into place atomically (on APFS a
    /// clone, which costs no space); returns where.
    pub fn copy_index(&self, name: &str, from: &Path) -> Result<PathBuf> {
        let path = self.index_path(name)?;
        let dir = self.indices_path();
        fs::create_dir_all(&dir).at(&dir)?;
        let temp = temp_path(&path);
        let copied = fs::copy(from, &temp).and_then(|_| File::open(&temp)?.sync_all());
        if let Err(error) = copied {
            let _ = fs::remove_file(&temp);
            return Err(error).at(from);
        }
        fs::rename(&temp, &path).inspect_err(|_| drop(fs::remove_file(&temp))).at(&path)?;
        sync_dir(&dir)?;
        Ok(path)
    }

    /// Builds the archive group `group` (the CDN config's `archive-group` or
    /// `patch-archive-group`) from the `.index` files of `archives` (its
    /// `archives` or `patch-archives`, in order), all of which must be in
    /// [`DataDir::indices_path`]. The Agent builds it locally since it isn't
    /// on the CDN, and the game merges one itself at start without it.
    ///
    /// Returns `true` when the group is in place (already, or now), `false`
    /// when what these indexes build isn't named `group`: of no use to the
    /// game, so nothing is left behind.
    pub fn build_archive_group(&self, archives: &[impl AsRef<str>], group: &str) -> Result<bool> {
        let name = hash_name(group)?;
        let path = self.index_path(&name)?;
        if is_complete_group(&path, &name)? {
            return Ok(true);
        }
        let maps = archives
            .iter()
            .map(|archive| {
                let index = self.index_path(archive.as_ref())?;
                let file = File::open(&index).at(&index)?;
                // SAFETY: archive indexes are written once beside their final
                // name and renamed into place; nothing changes them after.
                unsafe { Mmap::map(&file) }.at(&index)
            })
            .collect::<Result<Vec<_>>>()?;
        let indexes: Vec<&[u8]> = maps.iter().map(|map| &map[..]).collect();

        let temp = temp_path(&path);
        let built = File::create(&temp).at(&temp).and_then(|file| {
            let mut output = BufWriter::with_capacity(1 << 20, file);
            let built = archive_group::build(&indexes, &mut output).at(&path)?;
            let file = output.into_inner().map_err(io::IntoInnerError::into_error).at(&temp)?;
            file.sync_all().at(&temp)?;
            Ok(built)
        });
        match built {
            Ok(built) if built == name => {
                fs::rename(&temp, &path).inspect_err(|_| drop(fs::remove_file(&temp))).at(&path)?;
                sync_dir(&self.indices_path())?;
                Ok(true)
            }
            Ok(built) => {
                let _ = fs::remove_file(&temp);
                tracing::warn!(expected = %name, %built, archives = archives.len(), "archive group doesn't match its name");
                Ok(false)
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                Err(error)
            }
        }
    }
}

/// Writes `contents` atomically, creating the folders on the way.
fn write_new(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).at(dir)?;
    write_atomically(path, contents)?;
    sync_dir(dir)
}

/// Whether the group at `path` is whole: its footer hashes to its name.
fn is_complete_group(path: &Path, name: &str) -> Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).at(path),
    };
    let len = file.metadata().at(path)?.len();
    if len < FOOTER_SIZE {
        return Ok(false);
    }
    let mut footer = [0u8; FOOTER_SIZE as usize];
    file.seek(SeekFrom::Start(len - FOOTER_SIZE)).and_then(|_| file.read_exact(&mut footer)).at(path)?;
    Ok(archive_group::is_complete(&footer, name))
}

/// A config or CDN index name as the Agent spells it: 32 hex digits,
/// lowercase. Anything else (`..`, a slash) would put the file elsewhere.
fn hash_name(name: &str) -> Result<String> {
    if name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(name.to_ascii_lowercase())
    } else {
        Err(Error::InvalidName(name.to_owned()))
    }
}
