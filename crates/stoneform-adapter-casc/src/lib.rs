//! The game's local CASC storage on disk (`<install root>/Data/data`),
//! written exactly the way the Battle.net Agent writes it, so the game takes
//! it as its own.
//!
//! ```text
//! Data/config/xx/yy/{hash}  build and CDN configs                       DataDir
//! Data/indices/*.index      CDN archive indexes and archive groups      DataDir
//! Data/data/data.###        archives: segment headers, then [entry header][BLTE blob]
//! Data/data/*.idx           16 index files, one version each            CascStorage,
//! Data/data/shmem           index versions and free-space table         StorageWriter
//! Data/data/.waypoint-journal   what an unfinished write has appended
//! ```
//!
//! A [`StorageWriter`] appends blobs after the end of the last archive (new
//! archives when one is full) and journals each one, so a crash or pause at
//! any point resumes without losing or duplicating a blob. Finishing writes
//! the next version of every index file, then `shmem`, then removes the old
//! versions and the journal, with each step on disk before the next: a
//! storage interrupted anywhere is one the game accepts, plus a journal the
//! next run resumes from. One writer at a time holds the folder's
//! [`StorageLock`] (WoW's flavors share one storage).
//!
//! [`CascStorage`] reads what a storage holds without the lock: its keys for
//! planning, blobs back by key, what an interrupted run journaled, and the
//! repairs that change no game data (index update sections, lost archive
//! heads). The byte formats are `stoneform_tact::casc`'s; this crate is
//! their file I/O. Everything is synchronous std I/O, for a blocking thread.
//!
//! Ported from Waypoint (0BSD): `CASCStorageWriter`, `StorageLock`,
//! `StoredKeys` and the storage parts of `CASCInstaller`. Not ported: the
//! cleaner (old builds' data, hole punching), the verify-and-replace repair
//! mode, and anything Windows-specific.

mod data_dir;
mod error;
mod fsutil;
mod journal;
mod lock;
mod port;
mod space;
mod storage;
mod writer;

#[cfg(test)]
mod tests;

pub use data_dir::DataDir;
pub use error::{Error, Result};
pub use journal::{JOURNAL_FILE_NAME, UnfinishedSession};
pub use lock::StorageLock;
pub use port::{CascGameStorage, CascWriter, FreeSpace};
pub use space::{FREE_SPACE_MARGIN, available_space, ensure_free_space};
pub use storage::{CascStorage, IndexRepair, StorageState, StoredKeys};
pub use writer::{Finished, StorageWriter};
