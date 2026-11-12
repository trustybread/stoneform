use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("{url} was not found")]
    NotFound { url: String },
    #[error("{url} returned HTTP {status}")]
    Status { url: String, status: u16 },
    #[error("could not reach {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("unexpected response from {url}: {reason}")]
    Malformed { url: String, reason: String },
    #[error("could not write {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("no download server is available")]
    NoMirrors,
}

impl HttpError {
    /// Worth asking again after a pause: dropped connections, timeouts and
    /// the statuses servers send when they are busy.
    pub fn is_transient(&self) -> bool {
        match self {
            HttpError::Status { status, .. } => matches!(status, 408 | 429 | 500 | 502 | 503 | 504),
            HttpError::Network { source, .. } => {
                source.is_timeout() || source.is_connect() || source.is_request() || source.is_body()
            }
            _ => false,
        }
    }

    /// This mirror cannot serve the request; another one might.
    pub(crate) fn is_mirror_failure(&self) -> bool {
        matches!(self, HttpError::NotFound { .. } | HttpError::Status { .. } | HttpError::Malformed { .. })
    }
}
