/// What went wrong reading or writing a Blizzard format.
///
/// The variants say what kind of problem it is, so callers can decide what
/// to do (retry another CDN mirror on a checksum mismatch, give up on an
/// unsupported format); the message says where.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bytes don't follow the format: bad magic, truncated, inconsistent sizes.
    #[error("malformed {0}")]
    Malformed(String),
    /// Well-formed, but not something this crate handles (encrypted BLTE, another index version).
    #[error("unsupported {0}")]
    Unsupported(String),
    /// An MD5 or lookup3 checksum doesn't match: corrupt data, or not the file asked for.
    #[error("checksum mismatch for {0}")]
    ChecksumMismatch(String),
    /// A table or config lacks the row or value asked for.
    #[error("not found: {0}")]
    NotFound(String),
    /// The reader or writer of a streaming codec failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn malformed(what: impl Into<String>) -> Self {
        Error::Malformed(what.into())
    }

    pub(crate) fn unsupported(what: impl Into<String>) -> Self {
        Error::Unsupported(what.into())
    }

    pub(crate) fn not_found(what: impl Into<String>) -> Self {
        Error::NotFound(what.into())
    }
}
