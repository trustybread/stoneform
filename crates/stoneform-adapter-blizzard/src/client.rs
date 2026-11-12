use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::path::Path;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, RANGE};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::{HttpError, Retry, Throttle};

/// The two tables the version service publishes per product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionTable {
    /// The live build per region.
    Versions,
    /// The CDN hosts and path per region.
    Cdns,
}

impl VersionTable {
    fn name(self) -> &'static str {
        match self {
            VersionTable::Versions => "versions",
            VersionTable::Cdns => "cdns",
        }
    }
}

/// HTTPS access to Blizzard's version service and CDN mirrors.
#[derive(Debug, Clone)]
pub struct BlizzardHttp {
    http: reqwest::Client,
    retry: Retry,
    version_base: Option<String>,
    throttle: Throttle,
}

impl BlizzardHttp {
    pub fn new() -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("StoneForm/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            // Per read, not per request: a large download may take hours.
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { http, retry: Retry::default(), version_base: None, throttle: Throttle::default() })
    }

    /// The download speed limit this client and its clones share.
    pub fn throttle(&self) -> &Throttle {
        &self.throttle
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// Sends version-service requests to `base` instead of
    /// `https://<region>.version.battle.net`. For tests.
    pub fn with_version_base(mut self, base: impl Into<String>) -> Self {
        self.version_base = Some(base.into());
        self
    }

    /// One of the version service's pipe-separated tables for `product`,
    /// e.g. `https://eu.version.battle.net/v2/products/wowf/versions`.
    pub async fn version_table(&self, region: &str, product: &str, table: VersionTable) -> Result<String, HttpError> {
        let base = match &self.version_base {
            Some(base) => base.clone(),
            None => format!("https://{region}.version.battle.net"),
        };
        let url = format!("{base}/v2/products/{product}/{}", table.name());
        self.retry
            .run(|_| async {
                let response = self.send(&url, None).await?;
                let response = check_status(response, &url, false)?;
                response.text().await.map_err(|source| HttpError::Network { url: url.clone(), source })
            })
            .await
    }

    /// The file at `path` on the first mirror that has it; with `range`, only
    /// those bytes (inclusive, as HTTP ranges are).
    pub async fn fetch(
        &self,
        mirrors: &[String],
        path: &str,
        range: Option<RangeInclusive<u64>>,
    ) -> Result<Bytes, HttpError> {
        self.with_mirrors(mirrors, path, |url| {
            let range = range.clone();
            async move {
                let response = self.send(&url, range.as_ref().map(|r| (*r.start(), Some(*r.end())))).await?;
                let response = check_status(response, &url, range.is_some())?;
                if let Some(range) = &range {
                    check_content_range(&response, &url, *range.start(), *range.end())?;
                }
                let body = self.read_body(response, &url).await?;
                if let Some(range) = &range {
                    let expected = range.end() - range.start() + 1;
                    if body.len() as u64 != expected {
                        let reason = format!("{} bytes for a {expected}-byte range", body.len());
                        return Err(HttpError::Malformed { url, reason });
                    }
                }
                Ok(body)
            }
        })
        .await
    }

    /// Downloads the whole file at `path` into `dest`, continuing a partial
    /// `dest` left by an earlier, interrupted call. `progress` gets the number
    /// of bytes written after each chunk. Returns the file's size.
    ///
    /// Resuming from another mirror is safe: CDN paths are content hashes.
    pub async fn fetch_to_file(
        &self,
        mirrors: &[String],
        path: &str,
        dest: &Path,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<u64, HttpError> {
        let io_error = |source| HttpError::Io { path: dest.to_owned(), source };
        self.with_mirrors(mirrors, path, |url| async move {
            let have = match tokio::fs::metadata(dest).await {
                Ok(metadata) => metadata.len(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => return Err(io_error(error)),
            };
            let mut response = self.send(&url, (have > 0).then_some((have, None))).await?;
            if response.status() == StatusCode::RANGE_NOT_SATISFIABLE {
                // The partial file is not a prefix of this one; start over.
                tracing::info!(%url, have, "cannot resume; downloading again");
                tokio::fs::remove_file(dest).await.map_err(io_error)?;
                response = self.send(&url, None).await?;
            }
            // 200: the server sent the whole file (start over); 206: the rest.
            let resuming = match response.status() {
                StatusCode::OK => false,
                StatusCode::PARTIAL_CONTENT if have > 0 => {
                    check_content_range_start(&response, &url, have)?;
                    true
                }
                status => return Err(status_error(status, &url)),
            };
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .append(resuming)
                .truncate(!resuming)
                .open(dest)
                .await
                .map_err(io_error)?;
            let mut written = if resuming { have } else { 0 };
            while let Some(chunk) =
                response.chunk().await.map_err(|source| HttpError::Network { url: url.clone(), source })?
            {
                self.throttle.take(chunk.len()).await;
                file.write_all(&chunk).await.map_err(io_error)?;
                written += chunk.len() as u64;
                progress(written);
            }
            file.flush().await.map_err(io_error)?;
            Ok(written)
        })
        .await
    }

    /// The whole body, read in chunks so the speed limit can pace it.
    async fn read_body(&self, mut response: reqwest::Response, url: &str) -> Result<Bytes, HttpError> {
        let mut body = BytesMut::with_capacity(usize::try_from(response.content_length().unwrap_or(0)).unwrap_or(0));
        while let Some(chunk) =
            response.chunk().await.map_err(|source| HttpError::Network { url: url.to_owned(), source })?
        {
            self.throttle.take(chunk.len()).await;
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }

    async fn send(&self, url: &str, range: Option<(u64, Option<u64>)>) -> Result<reqwest::Response, HttpError> {
        let mut request = self.http.get(url);
        if let Some((start, end)) = range {
            let value = match end {
                Some(end) => format!("bytes={start}-{end}"),
                None => format!("bytes={start}-"),
            };
            request = request.header(RANGE, value);
        }
        request.send().await.map_err(|source| HttpError::Network { url: url.to_owned(), source })
    }

    /// Tries each mirror in turn; a mirror that answers with a permanent
    /// failure is skipped for the rest of the call. When a whole round ends in
    /// a transient failure, the round is retried after a pause.
    async fn with_mirrors<T, F, Fut>(&self, mirrors: &[String], path: &str, request: F) -> Result<T, HttpError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, HttpError>>,
    {
        let unavailable = std::sync::Mutex::new(HashSet::<usize>::new());
        let result = self
            .retry
            .run(|attempt| {
                let unavailable = &unavailable;
                let request = &request;
                async move {
                    let mut transient = None;
                    let mut last = HttpError::NoMirrors;
                    for (index, mirror) in mirrors.iter().enumerate() {
                        if unavailable.lock().expect("never poisoned").contains(&index) {
                            continue;
                        }
                        let url = format!("{}/{}", mirror.trim_end_matches('/'), path.trim_start_matches('/'));
                        match request(url.clone()).await {
                            Ok(value) => return Ok(value),
                            Err(error) => {
                                tracing::warn!(%url, attempt = attempt + 1, %error, "CDN request failed");
                                if error.is_transient() {
                                    transient = Some(error);
                                } else if error.is_mirror_failure() {
                                    unavailable.lock().expect("never poisoned").insert(index);
                                    last = error;
                                } else {
                                    // Local failures won't improve on another mirror.
                                    return Err(error);
                                }
                            }
                        }
                    }
                    Err(transient.unwrap_or(last))
                }
            })
            .await;
        if let Err(error) = &result {
            tracing::error!(path, %error, "every CDN mirror failed");
        }
        result
    }
}

/// 404 is "not found"; anything but 200 (or 206 for a range) is a status error.
fn check_status(response: reqwest::Response, url: &str, partial: bool) -> Result<reqwest::Response, HttpError> {
    let expected = if partial { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK };
    match response.status() {
        status if status == expected => Ok(response),
        status => Err(status_error(status, url)),
    }
}

fn status_error(status: StatusCode, url: &str) -> HttpError {
    match status {
        StatusCode::NOT_FOUND => HttpError::NotFound { url: url.to_owned() },
        status => HttpError::Status { url: url.to_owned(), status: status.as_u16() },
    }
}

fn check_content_range(response: &reqwest::Response, url: &str, start: u64, end: u64) -> Result<(), HttpError> {
    let value = response.headers().get(CONTENT_RANGE).and_then(|value| value.to_str().ok()).unwrap_or_default();
    if value.starts_with(&format!("bytes {start}-{end}/")) {
        Ok(())
    } else {
        Err(HttpError::Malformed { url: url.to_owned(), reason: format!("Content-Range {value:?} for {start}-{end}") })
    }
}

fn check_content_range_start(response: &reqwest::Response, url: &str, start: u64) -> Result<(), HttpError> {
    let value = response.headers().get(CONTENT_RANGE).and_then(|value| value.to_str().ok()).unwrap_or_default();
    if value.starts_with(&format!("bytes {start}-")) {
        Ok(())
    } else {
        Err(HttpError::Malformed {
            url: url.to_owned(),
            reason: format!("Content-Range {value:?} resuming at {start}"),
        })
    }
}
