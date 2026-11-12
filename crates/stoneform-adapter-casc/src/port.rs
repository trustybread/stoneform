//! The core's `GameStorage` port on this crate's storage writer.

use std::path::Path;

use stoneform_app::ports::{self, DiskError};
use stoneform_tact::keys::{ContentKey, EncodingKey};

use crate::{DataDir, StorageWriter, available_space};

/// How free space is measured. macOS's "important usage" capacity, which
/// counts purgeable space, is the better measure; the app passes it in.
pub type FreeSpace = fn(&Path) -> Option<u64>;

/// The `GameStorage` port: a game's `Data` folder.
#[derive(Debug, Clone, Copy)]
pub struct CascGameStorage {
    free_space: FreeSpace,
}

impl Default for CascGameStorage {
    /// Free space by `statvfs`, which leaves out purgeable space.
    fn default() -> Self {
        Self { free_space: |path| available_space(path).ok() }
    }
}

impl CascGameStorage {
    pub fn with_free_space(free_space: FreeSpace) -> Self {
        Self { free_space }
    }
}

/// An open session on the storage, holding its lock.
#[derive(Debug)]
pub struct CascWriter(StorageWriter);

impl ports::GameStorage for CascGameStorage {
    type Writer = CascWriter;

    fn open(&self, data_root: &Path) -> Result<CascWriter, DiskError> {
        let writer = DataDir::new(data_root).storage().open_writer().map_err(DiskError::new)?;
        Ok(CascWriter(writer))
    }

    fn write_config(&self, data_root: &Path, key: &ContentKey, bytes: &[u8]) -> Result<(), DiskError> {
        DataDir::new(data_root).write_config(&key.to_hex(), bytes).map(drop).map_err(DiskError::new)
    }

    fn has_index(&self, data_root: &Path, name: &str) -> bool {
        DataDir::new(data_root).index_path(name).is_ok_and(|path| path.is_file())
    }

    fn write_index(&self, data_root: &Path, name: &str, bytes: &[u8]) -> Result<(), DiskError> {
        DataDir::new(data_root).write_index(name, bytes).map(drop).map_err(DiskError::new)
    }

    fn build_archive_group(&self, data_root: &Path, group: &str, archives: &[String]) -> Result<(), DiskError> {
        match DataDir::new(data_root).build_archive_group(archives, group) {
            Ok(true) => Ok(()),
            Ok(false) => Err(DiskError::new(format!("the archive indexes do not build the group {group}"))),
            Err(error) => Err(DiskError::new(error)),
        }
    }

    fn available_space(&self, path: &Path) -> Option<u64> {
        (self.free_space)(path)
    }
}

impl ports::StorageWriter for CascWriter {
    fn contains(&self, key: &EncodingKey) -> bool {
        self.0.contains(key)
    }

    fn append(&mut self, key: &EncodingKey, blob: &[u8], full_key: bool) -> Result<(), DiskError> {
        self.0.append(key, blob, full_key).map(drop).map_err(DiskError::new)
    }

    fn finish(self) -> Result<(), DiskError> {
        self.0.finish().map(drop).map_err(DiskError::new)
    }
}
