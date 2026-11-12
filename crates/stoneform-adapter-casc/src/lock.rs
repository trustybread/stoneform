//! One writer per storage folder.

use std::fs::File;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::FlockOperation;
use rustix::io::Errno;

use crate::error::{At, Error, Result};

/// How long taking the lock waits for another holder to let go.
const LOCK_WAIT: Duration = Duration::from_secs(1);
const RETRY_EVERY: Duration = Duration::from_millis(10);

/// A writer's hold on a storage folder, across processes too: WoW's flavors
/// share one `Data/data`, and two installs writing to it at once would
/// interleave their archives and lose each other's index entries.
///
/// It is an exclusive `flock` on the folder itself, so no lock file is left
/// behind after a crash. Released when dropped.
#[derive(Debug)]
pub struct StorageLock {
    /// Holding the descriptor holds the lock; std opens it close-on-exec, so
    /// a process started meanwhile (the game) doesn't inherit it.
    _folder: File,
}

impl StorageLock {
    /// Takes the lock on `dir` (which must exist), waiting up to a second:
    /// while any process is being started, it briefly holds a copy of every
    /// descriptor, close-on-exec or not, so only a hold that lasts is a
    /// writer. Fails with [`Error::Busy`] then.
    pub fn acquire(dir: &Path) -> Result<Self> {
        Self::acquire_within(dir, LOCK_WAIT)
    }

    pub(crate) fn acquire_within(dir: &Path, wait: Duration) -> Result<Self> {
        let folder = File::open(dir).at(dir)?;
        let deadline = Instant::now() + wait;
        loop {
            match rustix::fs::flock(&folder, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Self { _folder: folder }),
                Err(Errno::WOULDBLOCK) if Instant::now() < deadline => thread::sleep(RETRY_EVERY),
                Err(Errno::WOULDBLOCK) => return Err(Error::Busy(dir.to_owned())),
                Err(Errno::INTR) => {}
                // A filesystem without locks is still written, as Waypoint does.
                Err(error) => {
                    tracing::warn!(%error, path = %dir.display(), "storage folder can't be locked");
                    return Ok(Self { _folder: folder });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_a_second_holder_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let first = StorageLock::acquire_within(dir.path(), Duration::ZERO).unwrap();
        assert!(matches!(StorageLock::acquire_within(dir.path(), Duration::from_millis(50)), Err(Error::Busy(_))));
        drop(first);
        StorageLock::acquire_within(dir.path(), Duration::ZERO).unwrap();
        assert!(matches!(StorageLock::acquire(&dir.path().join("missing")), Err(Error::Io { .. })));
    }

    /// Port of Waypoint's `storageLockWaitsOutAShortHold`: a hold that ends
    /// within the wait isn't another writer.
    #[test]
    fn waits_out_a_short_hold() {
        let dir = tempfile::tempdir().unwrap();
        let first = StorageLock::acquire(dir.path()).unwrap();
        let started = Instant::now();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            drop(first);
        });
        let second = StorageLock::acquire(dir.path()).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(150));
        release.join().unwrap();
        drop(second);
    }
}
