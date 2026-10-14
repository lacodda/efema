//! How often one access key may ask: a bucket of requests per key, refilled
//! evenly over a minute.
//!
//! A key starts with a full minute's worth, so a device that syncs after a
//! night away is not held back; past that, requests come at the rate the
//! operator set. The buckets live in memory: a restart refills them, which
//! costs nothing a minute would not.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) struct Rates {
    per_minute: u32,
    buckets: Mutex<HashMap<[u8; 32], Bucket>>,
}

struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Rates {
    pub(crate) fn new(per_minute: u32) -> Self {
        Self { per_minute, buckets: Mutex::new(HashMap::new()) }
    }

    /// Takes one request from the bucket of the key with `digest`, or says
    /// how long until there is one.
    pub(crate) fn take(&self, digest: &[u8; 32]) -> Result<(), Duration> {
        self.take_at(digest, Instant::now())
    }

    fn take_at(&self, digest: &[u8; 32], now: Instant) -> Result<(), Duration> {
        let capacity = f64::from(self.per_minute);
        let per_second = capacity / 60.0;
        // Nothing in the map is left half-updated by a panic.
        let mut buckets = self.buckets.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket = buckets.entry(*digest).or_insert(Bucket { tokens: capacity, at: now });
        let elapsed = now.saturating_duration_since(bucket.at).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * per_second).min(capacity);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / per_second))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minute_at_once_then_the_rate() {
        let rates = Rates::new(60);
        let key = [1; 32];
        let start = Instant::now();
        for _ in 0..60 {
            rates.take_at(&key, start).unwrap();
        }
        let wait = rates.take_at(&key, start).unwrap_err();
        assert!(wait > Duration::from_millis(900) && wait <= Duration::from_secs(1), "{wait:?}");
        // Another key has a bucket of its own.
        rates.take_at(&[2; 32], start).unwrap();
        // A second later, one more.
        let later = start + Duration::from_secs(1);
        rates.take_at(&key, later).unwrap();
        assert!(rates.take_at(&key, later).is_err());
        // A long pause refills the bucket, and no further.
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..60 {
            rates.take_at(&key, much_later).unwrap();
        }
        assert!(rates.take_at(&key, much_later).is_err());
    }
}
