//! Whether the disk can take an install.

use std::path::Path;

use crate::error::{At, Error, Result};

/// What an install leaves free beyond what it writes, as Waypoint does: the
/// game, the system and the index files' next versions need room too.
pub const FREE_SPACE_MARGIN: u64 = 1 << 30;

/// Fails with [`Error::NotEnoughSpace`] unless `available` bytes cover
/// `needed` (what is left to write, after what a resumed run journaled)
/// plus [`FREE_SPACE_MARGIN`]. Checked before anything is written, so a full
/// disk is reported up front instead of hours into a download.
pub fn ensure_free_space(needed: u64, available: u64) -> Result<()> {
    if available > needed.saturating_add(FREE_SPACE_MARGIN) {
        Ok(())
    } else {
        Err(Error::NotEnoughSpace { needed, available })
    }
}

/// The bytes an unprivileged process can still write on `path`'s volume
/// (`statvfs`). `path` needn't exist yet: its nearest existing ancestor
/// decides.
///
/// On macOS this leaves out purgeable space (local snapshots, caches) that
/// the system frees for an important download; the app's own measurement
/// (`NSURLVolumeAvailableCapacityForImportantUsageKey`, as Waypoint uses)
/// counts it, and is the better input to [`ensure_free_space`] there.
pub fn available_space(path: &Path) -> Result<u64> {
    let existing = path.ancestors().find(|ancestor| ancestor.exists()).unwrap_or(Path::new("/"));
    let stats = rustix::fs::statvfs(existing).map_err(std::io::Error::from).at(existing)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_a_gibibyte_to_spare() {
        assert!(ensure_free_space(10, FREE_SPACE_MARGIN + 11).is_ok());
        let short = ensure_free_space(10, FREE_SPACE_MARGIN + 10);
        assert!(
            matches!(short, Err(Error::NotEnoughSpace { needed: 10, available }) if available == FREE_SPACE_MARGIN + 10)
        );
        assert!(ensure_free_space(u64::MAX, u64::MAX).is_err());
    }

    #[test]
    fn measures_the_volume_of_a_path_to_be() {
        let dir = tempfile::tempdir().unwrap();
        let here = available_space(dir.path()).unwrap();
        assert!(here > 0);
        let later = available_space(&dir.path().join("World of Warcraft/Data/data")).unwrap();
        // Same volume; other processes may write meanwhile.
        assert!(later.abs_diff(here) < 1 << 30);
    }
}
