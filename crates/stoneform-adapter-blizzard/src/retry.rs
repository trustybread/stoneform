use std::future::Future;
use std::time::Duration;

use crate::HttpError;

/// How often, and after which pauses, a GET is repeated after a transient
/// failure. Permanent failures return at once.
#[derive(Debug, Clone)]
pub struct Retry {
    delays: Vec<Duration>,
}

impl Default for Retry {
    /// 1, 2, 4, 8, 16 and 30 seconds: about a minute of patience, as Waypoint.
    fn default() -> Self {
        Self::with_delays([1, 2, 4, 8, 16, 30].map(Duration::from_secs))
    }
}

impl Retry {
    pub fn with_delays(delays: impl IntoIterator<Item = Duration>) -> Self {
        Self { delays: delays.into_iter().collect() }
    }

    /// No retries at all.
    pub fn never() -> Self {
        Self::with_delays([])
    }

    pub(crate) async fn run<T, F, Fut>(&self, mut attempt: F) -> Result<T, HttpError>
    where
        F: FnMut(usize) -> Fut,
        Fut: Future<Output = Result<T, HttpError>>,
    {
        let mut number = 0;
        loop {
            let error = match attempt(number).await {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            let Some(delay) = self.delays.get(number).filter(|_| error.is_transient()) else { return Err(error) };
            tracing::info!(attempt = number + 2, ?delay, %error, "retrying");
            tokio::time::sleep(*delay).await;
            number += 1;
        }
    }
}
