use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A shared download speed limit across every request of a `BlizzardHttp`
/// and its clones. It can change at any time; the next chunk read obeys it.
#[derive(Debug, Clone, Default)]
pub struct Throttle(Arc<State>);

#[derive(Debug, Default)]
struct State {
    /// Bytes per second; 0 means no limit.
    rate: AtomicU64,
    bucket: std::sync::Mutex<Bucket>,
}

/// Bytes allowed so far minus bytes taken. Negative means in debt: the
/// taker waits until the rate has paid it back.
#[derive(Debug)]
struct Bucket {
    balance: f64,
    updated: Instant,
}

impl Default for Bucket {
    fn default() -> Self {
        Self { balance: 0.0, updated: Instant::now() }
    }
}

/// How much unused allowance carries over: a quarter second's worth, so a
/// pause in reading doesn't turn into a burst above the limit.
const CARRY: f64 = 0.25;

impl Throttle {
    /// `None` lifts the limit.
    pub fn set_limit(&self, bytes_per_second: Option<u64>) {
        self.0.rate.store(bytes_per_second.unwrap_or(0), Ordering::SeqCst);
    }

    pub fn limit(&self) -> Option<u64> {
        Some(self.0.rate.load(Ordering::SeqCst)).filter(|&rate| rate > 0)
    }

    /// Accounts for `bytes` just read, waiting if the limit asks for it.
    pub(crate) async fn take(&self, bytes: usize) {
        let Some(rate) = self.limit() else { return };
        let rate = rate as f64;
        let wait = {
            let mut bucket = self.0.bucket.lock().expect("never poisoned");
            let now = Instant::now();
            let earned = now.duration_since(bucket.updated).as_secs_f64() * rate;
            bucket.balance = (bucket.balance + earned).min(rate * CARRY) - bytes as f64;
            bucket.updated = now;
            (bucket.balance < 0.0).then(|| Duration::from_secs_f64(-bucket.balance / rate))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn holds_reads_to_the_limit() {
        let throttle = Throttle::default();
        throttle.set_limit(Some(200_000));
        let started = Instant::now();
        for _ in 0..3 {
            throttle.take(20_000).await;
        }
        // 60 kB at 200 kB/s: about 0.3 s.
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(600), "{elapsed:?}");
    }

    #[tokio::test]
    async fn without_a_limit_never_waits() {
        let throttle = Throttle::default();
        let started = Instant::now();
        throttle.take(100_000_000).await;
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(throttle.limit(), None);
    }
}
