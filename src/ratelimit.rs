//! A sliding-window counter per key, in process memory: one process serves a handful of
//! users, so no shared store is needed.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct RateLimiter {
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a hit for the key and says whether it is over `limit` hits in `window`; when it
    /// is, the seconds until the oldest hit leaves the window.
    pub fn hit(&self, key: &str, limit: usize, window: Duration) -> Result<(), u64> {
        self.hit_at(key, limit, window, Instant::now())
    }

    pub fn hit_at(
        &self,
        key: &str,
        limit: usize,
        window: Duration,
        now: Instant,
    ) -> Result<(), u64> {
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = hits.entry(key.to_string()).or_default();
        while let Some(oldest) = entry.front() {
            if now.duration_since(*oldest) >= window {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() >= limit {
            let oldest = *entry.front().expect("a non-empty window");
            let retry = window
                .saturating_sub(now.duration_since(oldest))
                .as_secs()
                .max(1);
            return Err(retry);
        }
        entry.push_back(now);
        Ok(())
    }

    /// Forgets a key, as a successful login does for its e-mail.
    pub fn clear(&self, key: &str) {
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        hits.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eleventh_hit_in_the_window_is_refused_and_the_window_slides() {
        let limiter = RateLimiter::new();
        let start = Instant::now();
        for _ in 0..10 {
            assert!(
                limiter
                    .hit_at("k", 10, Duration::from_secs(900), start)
                    .is_ok()
            );
        }
        assert_eq!(
            limiter.hit_at(
                "k",
                10,
                Duration::from_secs(900),
                start + Duration::from_secs(1)
            ),
            Err(899)
        );
        assert!(
            limiter
                .hit_at(
                    "k",
                    10,
                    Duration::from_secs(900),
                    start + Duration::from_secs(901)
                )
                .is_ok()
        );
        limiter.clear("k");
        assert!(
            limiter
                .hit_at(
                    "k",
                    1,
                    Duration::from_secs(900),
                    start + Duration::from_secs(902)
                )
                .is_ok()
        );
    }
}
