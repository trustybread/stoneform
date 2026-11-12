//! The `LooseFiles` port: the game's files outside local storage, such as
//! `World of Warcraft.app`. Ported from Waypoint's `GameUpdater` (0BSD).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use stoneform_app::install::plan::LooseFile;
use stoneform_app::ports::{DiskError, LooseFiles};
use stoneform_tact::blte;
use stoneform_tact::keys::ContentKey;

/// Numbers temporary files, so concurrent stagings never collide.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Where files wait, named by content key, until all of them are there.
const STAGING: &str = ".stoneform-staging";

#[derive(Debug, thiserror::Error)]
enum LooseError {
    #[error("{path} does not match its content key")]
    Mismatch { path: String },
    #[error("{0:?} is not a safe path inside the game folder")]
    UnsafePath(String),
    #[error("{path} was not staged")]
    NotStaged { path: String },
    #[error(transparent)]
    Format(#[from] stoneform_tact::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Stages decoded, verified files under `<flavor>/.stoneform-staging` and
/// moves them into place when the install commits.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskLooseFiles;

fn staged_path(flavor_root: &Path, key: &ContentKey) -> PathBuf {
    flavor_root.join(STAGING).join(key.to_hex())
}

impl LooseFiles for DiskLooseFiles {
    fn is_current(&self, flavor_root: &Path, file: &LooseFile) -> bool {
        let Ok(path) = safe_join(flavor_root, &file.path) else { return false };
        // Size first: it settles most changed files without reading them.
        fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() == file.size)
            && content_key_of(&path).is_ok_and(|key| key == file.content_key)
    }

    fn is_staged(&self, flavor_root: &Path, file: &LooseFile) -> bool {
        // Only verified files are renamed into the staging name, so its size suffices.
        fs::metadata(staged_path(flavor_root, &file.content_key)).is_ok_and(|meta| meta.len() == file.size)
    }

    async fn stage(&self, flavor_root: &Path, file: &LooseFile, blob: Bytes) -> Result<(), DiskError> {
        let target = staged_path(flavor_root, &file.content_key);
        let (key, size, path) = (file.content_key, file.size, file.path.clone());
        // Inflating a 100 MB binary is work for a blocking thread.
        tokio::task::spawn_blocking(move || stage(&target, &blob, key, size, &path))
            .await
            .map_err(DiskError::new)?
            .map_err(DiskError::new)
    }

    fn commit(&self, flavor_root: &Path, files: &[LooseFile], obsolete: &[String]) -> Result<(), DiskError> {
        commit(flavor_root, files).map_err(DiskError::new)?;
        remove_obsolete(flavor_root, obsolete);
        Ok(())
    }
}

/// MD5 of a file's content, read in pieces: the game binary is over 100 MB.
fn content_key_of(path: &Path) -> io::Result<ContentKey> {
    use md5::{Digest, Md5};
    let mut file = File::open(path)?;
    let mut hasher = Md5::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    ContentKey::from_slice(&hasher.finalize()).ok_or_else(|| io::Error::other("MD5 is 16 bytes"))
}

/// Deletes files an update no longer has, then folders it left empty. A
/// leftover file does no harm, so failures are only logged.
fn remove_obsolete(flavor_root: &Path, obsolete: &[String]) {
    for path in obsolete {
        let Ok(full) = safe_join(flavor_root, path) else { continue };
        match fs::remove_file(&full) {
            Ok(()) => tracing::info!(path, "removed a file the new build no longer has"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(path, %error, "could not remove an obsolete file");
                continue;
            }
        }
        // Empty folders up to, not including, the flavor folder.
        for folder in full.ancestors().skip(1).take_while(|folder| *folder != flavor_root) {
            if fs::remove_dir(folder).is_err() {
                break;
            }
        }
    }
}

fn stage(target: &Path, blob: &[u8], key: ContentKey, size: u64, path: &str) -> Result<(), LooseError> {
    let dir = target.parent().ok_or_else(|| LooseError::UnsafePath(path.to_owned()))?;
    fs::create_dir_all(dir)?;
    // A name of its own per write, so two stagings of one content never
    // share a temporary file; the rename onto the staged name is atomic.
    let temp = target.with_extension(format!("{}.partial", NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
    let mut out = BufWriter::new(File::create(&temp)?);
    let decoded = blte::decode_to(blob, &mut out);
    let written = out.flush().and_then(|()| out.get_ref().sync_all());
    match (decoded, written) {
        (Ok(decoded), Ok(())) if decoded.content_key == key && decoded.size == size => {
            fs::rename(&temp, target)?;
            Ok(())
        }
        (decoded, written) => {
            let _ = fs::remove_file(&temp);
            decoded?;
            written?;
            Err(LooseError::Mismatch { path: path.to_owned() })
        }
    }
}

fn commit(flavor_root: &Path, files: &[LooseFile]) -> Result<(), LooseError> {
    // Several paths can share one content key: copy for all but the last.
    let mut remaining: HashMap<ContentKey, usize> = HashMap::new();
    for file in files {
        *remaining.entry(file.content_key).or_default() += 1;
    }
    for file in files {
        let destination = safe_join(flavor_root, &file.path)?;
        let staged = staged_path(flavor_root, &file.content_key);
        if !staged.is_file() {
            return Err(LooseError::NotStaged { path: file.path.clone() });
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mode = match fs::metadata(&destination) {
            Ok(existing) => existing.permissions().mode() & 0o7777,
            Err(_) if is_executable(&staged)? => 0o755,
            Err(_) => 0o644,
        };
        fs::set_permissions(&staged, fs::Permissions::from_mode(mode))?;
        let count = remaining.get_mut(&file.content_key).expect("counted above");
        if *count > 1 {
            fs::copy(&staged, &destination)?;
        } else {
            fs::rename(&staged, &destination)?;
        }
        *count -= 1;
    }
    match fs::remove_dir_all(flavor_root.join(STAGING)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// `path` under `root`, refusing anything that could leave it: the manifest
/// comes from the network.
fn safe_join(root: &Path, path: &str) -> Result<PathBuf, LooseError> {
    let relative = Path::new(path);
    let safe = !path.is_empty() && relative.components().all(|component| matches!(component, Component::Normal(_)));
    if safe { Ok(root.join(relative)) } else { Err(LooseError::UnsafePath(path.to_owned())) }
}

/// Mach-O binaries (thin or universal) and scripts get the execute bit.
fn is_executable(path: &Path) -> io::Result<bool> {
    const MACH_O: [[u8; 4]; 4] =
        [[0xcf, 0xfa, 0xed, 0xfe], [0xce, 0xfa, 0xed, 0xfe], [0xca, 0xfe, 0xba, 0xbe], [0xbe, 0xba, 0xfe, 0xca]];
    let mut head = [0u8; 4];
    let read = File::open(path)?.read(&mut head)?;
    Ok((read == 4 && MACH_O.contains(&head)) || head.starts_with(b"#!"))
}

#[cfg(test)]
mod tests {
    use stoneform_tact::config::CdnKind;
    use stoneform_tact::keys::EncodingKey;

    use super::*;
    use stoneform_app::install::plan::Source;

    /// The simplest BLTE container: no chunk table, one plain (`N`) block.
    fn blte(content: &[u8]) -> Bytes {
        let mut blob = b"BLTE\0\0\0\0N".to_vec();
        blob.extend_from_slice(content);
        Bytes::from(blob)
    }

    fn loose(path: &str, content: &[u8]) -> LooseFile {
        LooseFile {
            path: path.to_owned(),
            content_key: ContentKey::of(content),
            encoding_key: EncodingKey::from_slice(&[7; 16]).unwrap(),
            size: content.len() as u64,
            encoded_size: 0,
            source: Source::File(CdnKind::Data),
        }
    }

    #[tokio::test]
    async fn stages_then_moves_files_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let script = loose("World of Warcraft.app/Contents/run.sh", b"#!/bin/sh\necho hi\n");
        let data = loose("World of Warcraft.app/Contents/Info.plist", b"<plist/>");
        let store = DiskLooseFiles;

        for (file, content) in [(&script, &b"#!/bin/sh\necho hi\n"[..]), (&data, &b"<plist/>"[..])] {
            assert!(!store.is_staged(dir.path(), file));
            store.stage(dir.path(), file, blte(content)).await.unwrap();
            assert!(store.is_staged(dir.path(), file));
        }
        store.commit(dir.path(), &[script.clone(), data.clone()], &[]).unwrap();

        let script_path = dir.path().join(&script.path);
        assert_eq!(fs::read(&script_path).unwrap(), b"#!/bin/sh\necho hi\n");
        assert_eq!(fs::metadata(&script_path).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(dir.path().join(&data.path)).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(!dir.path().join(STAGING).exists());
    }

    #[tokio::test]
    async fn mach_o_binaries_become_executable() {
        let dir = tempfile::tempdir().unwrap();
        let binary = [0xcf, 0xfa, 0xed, 0xfe, 1, 2, 3, 4];
        let file = loose("World of Warcraft.app/Contents/MacOS/World of Warcraft", &binary);
        DiskLooseFiles.stage(dir.path(), &file, blte(&binary)).await.unwrap();
        DiskLooseFiles.commit(dir.path(), std::slice::from_ref(&file), &[]).unwrap();
        let mode = fs::metadata(dir.path().join(&file.path)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[tokio::test]
    async fn refuses_content_that_does_not_match_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = loose("a.txt", b"expected");
        let error = DiskLooseFiles.stage(dir.path(), &file, blte(b"tampered")).await.unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        assert!(!DiskLooseFiles.is_staged(dir.path(), &file));
    }

    #[tokio::test]
    async fn leaves_current_files_alone_and_removes_obsolete_ones() {
        let dir = tempfile::tempdir().unwrap();
        let kept = loose("World of Warcraft.app/Contents/Info.plist", b"<plist/>");
        DiskLooseFiles.stage(dir.path(), &kept, blte(b"<plist/>")).await.unwrap();
        DiskLooseFiles.commit(dir.path(), std::slice::from_ref(&kept), &[]).unwrap();
        fs::create_dir_all(dir.path().join("Old/Nested")).unwrap();
        fs::write(dir.path().join("Old/Nested/gone.txt"), b"old").unwrap();

        assert!(DiskLooseFiles.is_current(dir.path(), &kept));
        assert!(!DiskLooseFiles.is_current(dir.path(), &loose(&kept.path, b"<plist version=2/>")));
        assert!(!DiskLooseFiles.is_current(dir.path(), &loose("missing.txt", b"x")));

        DiskLooseFiles.commit(dir.path(), &[], &["Old/Nested/gone.txt".to_owned(), "../escape".to_owned()]).unwrap();

        assert!(!dir.path().join("Old").exists(), "emptied folders go too");
        assert!(dir.path().join(&kept.path).exists());
    }

    #[tokio::test]
    async fn writes_shared_content_to_every_path() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (loose("a/one.txt", b"same"), loose("b/two.txt", b"same"));
        DiskLooseFiles.stage(dir.path(), &first, blte(b"same")).await.unwrap();
        DiskLooseFiles.commit(dir.path(), &[first.clone(), second.clone()], &[]).unwrap();
        assert_eq!(fs::read(dir.path().join(&first.path)).unwrap(), b"same");
        assert_eq!(fs::read(dir.path().join(&second.path)).unwrap(), b"same");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stages_one_content_many_times_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![7u8; 1 << 20];
        let file = loose("same.bin", &content);
        let stagings: Vec<_> = (0..8)
            .map(|_| {
                let (root, file, blob) = (dir.path().to_owned(), file.clone(), blte(&content));
                tokio::spawn(async move { DiskLooseFiles.stage(&root, &file, blob).await })
            })
            .collect();
        for staging in stagings {
            staging.await.unwrap().unwrap();
        }
        assert!(DiskLooseFiles.is_staged(dir.path(), &file));
        let leftovers = fs::read_dir(dir.path().join(STAGING)).unwrap().count();
        assert_eq!(leftovers, 1, "only the staged file remains");
    }

    #[test]
    fn refuses_paths_that_leave_the_game_folder() {
        let root = Path::new("/games/wow/_forever_");
        assert!(safe_join(root, "World of Warcraft.app/Contents/Info.plist").is_ok());
        for path in ["", "/etc/passwd", "../outside", "a/../../b", "./a"] {
            assert!(safe_join(root, path).is_err(), "{path:?}");
        }
    }
}
