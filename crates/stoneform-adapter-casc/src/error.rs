use std::io;
use std::path::{Path, PathBuf};

/// What went wrong with a local storage or the files around it.
///
/// The variants say what the caller can do about it: wait for another writer
/// ([`Error::Busy`]), resume first ([`Error::Unfinished`]), free space
/// ([`Error::NotEnoughSpace`]), or send the player to Battle.net's Scan and
/// Repair ([`Error::Damaged`]).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file operation failed: a full disk, missing permissions.
    #[error("could not access {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A storage file doesn't parse: an index file, `shmem`, an archive index.
    #[error("could not read {}: {source}", path.display())]
    Format {
        path: PathBuf,
        #[source]
        source: stoneform_tact::Error,
    },
    /// The storage isn't in a state this writer can safely continue from.
    #[error("the storage in {} is damaged: {reason}", path.display())]
    Damaged { path: PathBuf, reason: String },
    /// Another writer holds the storage, in this process or another (WoW's
    /// flavors share one storage).
    #[error("another install or update is writing to {}", .0.display())]
    Busy(PathBuf),
    /// Asked to start a new storage where a finished one is.
    #[error("{} already holds a finished storage", .0.display())]
    AlreadyExists(PathBuf),
    /// An interrupted install or update has to be resumed first: its writer
    /// rewrites the index files itself.
    #[error("an unfinished install or update needs to finish in {}", .0.display())]
    Unfinished(PathBuf),
    /// A blob bigger than an archive can hold.
    #[error("a {0}-byte blob does not fit in a storage archive")]
    BlobTooLarge(usize),
    /// Every archive number is used.
    #[error("the storage in {} has no room for another archive", .0.display())]
    StorageFull(PathBuf),
    /// The volume can't take what is left to write and the margin on top.
    #[error("not enough disk space: {needed} bytes needed and 1 GiB to spare, {available} available")]
    NotEnoughSpace { needed: u64, available: u64 },
    /// A config or CDN index name that isn't a 32-digit hex hash, which
    /// would put the file somewhere else than where the game looks.
    #[error("{0:?} is not a 32-digit hex name")]
    InvalidName(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn damaged(path: &Path, reason: impl Into<String>) -> Self {
        Error::Damaged { path: path.to_owned(), reason: reason.into() }
    }
}

/// Attaches the path an operation was on to its error.
pub(crate) trait At<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> At<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io { path: path.to_owned(), source })
    }
}

impl<T> At<T> for stoneform_tact::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| match source {
            stoneform_tact::Error::Io(source) => Error::Io { path: path.to_owned(), source },
            source => Error::Format { path: path.to_owned(), source },
        })
    }
}
