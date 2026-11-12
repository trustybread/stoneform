//! File helpers: atomic replacement, directory syncs, sizes.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{At, Result};

/// Writes `contents` to a temporary file beside `path`, flushes it to disk,
/// then renames it over `path`: a reader (the game, a resumed run) finds the
/// old file or the new one, never half of one. Make the rename itself
/// durable with [`sync_dir`] before relying on it.
pub(crate) fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    replace_with(path, |file| file.write_all(contents))
}

/// [`write_atomically`], with the temporary file filled by `fill`.
pub(crate) fn replace_with(path: &Path, fill: impl FnOnce(&mut File) -> io::Result<()>) -> Result<()> {
    let temp = temp_path(path);
    let written = File::create(&temp).and_then(|mut file| {
        fill(&mut file)?;
        file.sync_all()
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error).at(path);
    }
    fs::rename(&temp, path).inspect_err(|_| drop(fs::remove_file(&temp))).at(path)
}

/// A name beside `path` no other writer uses at the same time: the process
/// and a per-process counter.
pub(crate) fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(format!(".{}-{}.part", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    path.with_file_name(name)
}

/// Flushes a directory's entries (renames, new and removed files) to disk,
/// so the order of a multi-file commit survives a power loss. Filesystems
/// that can't sync a directory are written all the same.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    match File::open(dir).and_then(|dir| dir.sync_all()) {
        Err(error) if matches!(error.kind(), io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput) => {
            tracing::debug!(%error, path = %dir.display(), "directory sync not supported");
            Ok(())
        }
        result => result.at(dir),
    }
}

/// The file's size; `None` when it doesn't exist.
pub(crate) fn file_len(path: &Path) -> Result<Option<u64>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).at(path),
    }
}

/// The file's bytes; `None` when it doesn't exist.
pub(crate) fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).at(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_files_whole_and_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shmem");
        write_atomically(&path, b"old").unwrap();
        write_atomically(&path, b"new").unwrap();
        sync_dir(dir.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        // A failed fill keeps the old file and cleans up.
        let failed = replace_with(&path, |file| {
            file.write_all(b"half")?;
            Err(io::Error::other("disk full"))
        });
        assert!(failed.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

        assert_eq!(file_len(&path).unwrap(), Some(3));
        assert_eq!(file_len(&dir.path().join("missing")).unwrap(), None);
        assert_eq!(read_if_exists(&dir.path().join("missing")).unwrap(), None);
        assert_ne!(temp_path(&path), temp_path(&path));
    }
}
