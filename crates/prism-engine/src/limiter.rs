//! Live-adjustable token-bucket rate limiter (GCRA formulation).
//!
//! Downloads acquire from a chain of limiters (`[download, queue…, global]`). All callers share
//! one timeline: each acquire reserves the next slot (at most [`MAX_ACQUIRE`] bytes) under a
//! short lock and then sleeps — once — until that slot, outside the lock. This makes the
//! limiter FIFO-fair across connections and robust against timer imprecision: slots are
//! computed from the monotonic clock, never from how long a sleep lasted, and a burst
//! tolerance of 250 ms (at least 64 KiB) credits oversleeping instead of losing the time.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// Minimum burst tolerance in bytes.
const BURST_MIN: f64 = 64.0 * 1024.0;

/// Largest number of bytes reserved by a single internal acquire.
pub const MAX_ACQUIRE: u64 = 64 * 1024;

/// A token-bucket limiter shared by any number of tasks. `0` bytes/s = unlimited.
#[derive(Debug)]
pub struct RateLimiter {
    rate: AtomicU64,
    /// Theoretical arrival time of the next byte (GCRA "TAT").
    tat: Mutex<Option<Instant>>,
    changed: Notify,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::unlimited()
    }
}

impl RateLimiter {
    /// A limiter allowing `bytes_per_sec` (0 = unlimited).
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            tat: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    /// An unlimited limiter (can be limited later with [`RateLimiter::set_rate`]).
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// Current limit in bytes/s (0 = unlimited).
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Changes the limit immediately; waiting tasks re-evaluate their wait.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        // Settings are re-applied periodically: the same rate must not restart the schedule
        // (that would forgive the waiting connections and let the speed overshoot the cap).
        if self.rate.swap(bytes_per_sec, Ordering::Relaxed) == bytes_per_sec {
            return;
        }
        *self.tat.lock().expect("limiter lock") = None;
        self.changed.notify_waiters();
    }

    /// Waits until `bytes` may be consumed.
    pub async fn acquire(&self, bytes: u64) {
        let mut left = bytes;
        while left > 0 {
            if self.rate() == 0 {
                return;
            }
            let n = left.min(MAX_ACQUIRE);
            self.acquire_piece(n).await;
            left -= n;
        }
    }

    async fn acquire_piece(&self, n: u64) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let rate = self.rate();
            if rate == 0 {
                return;
            }
            let wake_at = {
                let mut tat = self.tat.lock().expect("limiter lock");
                let now = Instant::now();
                // Burst tolerance: 250 ms of traffic, at least 64 KiB.
                let tolerance =
                    Duration::from_secs_f64((rate as f64 / 4.0).max(BURST_MIN) / rate as f64);
                let base = match *tat {
                    Some(t) if t > now => t,
                    _ => now,
                };
                let new_tat = base + Duration::from_secs_f64(n as f64 / rate as f64);
                *tat = Some(new_tat);
                new_tat.checked_sub(tolerance).filter(|w| *w > now)
            };
            let Some(wake_at) = wake_at else { return };
            tokio::select! {
                _ = tokio::time::sleep_until(wake_at) => return,
                // Rate changed: the reservation was discarded, compute a new one.
                _ = &mut notified => continue,
            }
        }
    }
}

/// Whether any limiter of a chain currently caps the rate. Racing a slow tail with a second
/// connection only helps when bandwidth is free; under a cap the duplicate bytes would eat
/// the budget the other connections need.
pub(crate) fn any_limited(limiters: &[std::sync::Arc<RateLimiter>]) -> bool {
    limiters.iter().any(|l| l.rate() != 0)
}

/// Acquires `bytes` from every limiter of a chain, in order.
pub(crate) async fn acquire_all(limiters: &[std::sync::Arc<RateLimiter>], bytes: u64) {
    for l in limiters {
        if l.rate() != 0 {
            l.acquire(bytes).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Achieved rate excluding the initial burst allowance. `extra_sleep` simulates coarse
    /// timers (e.g. macOS timer coalescing): every acquire that had to wait oversleeps by it.
    async fn measure(
        limiter: Arc<RateLimiter>,
        workers: usize,
        dur: Duration,
        extra_sleep: Duration,
    ) -> f64 {
        let burst = (limiter.rate() as f64 / 4.0).max(BURST_MIN);
        let start = Instant::now();
        let total = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..workers {
            let l = limiter.clone();
            let t = total.clone();
            tasks.push(tokio::spawn(async move {
                while start.elapsed() < dur {
                    let before = Instant::now();
                    l.acquire(16384).await;
                    if !extra_sleep.is_zero() && before.elapsed() > Duration::from_millis(1) {
                        tokio::time::sleep(extra_sleep).await;
                    }
                    t.fetch_add(16384, Ordering::Relaxed);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        (total.load(Ordering::Relaxed) as f64 - burst) / start.elapsed().as_secs_f64()
    }

    #[tokio::test]
    async fn accuracy_single_and_many_workers() {
        let rate = 2u64 << 20;
        for workers in [1usize, 8] {
            let l = Arc::new(RateLimiter::new(rate));
            let got = measure(l, workers, Duration::from_millis(1500), Duration::ZERO).await;
            let err = (got - rate as f64).abs() / rate as f64;
            assert!(err < 0.05, "workers={workers} rate={got} err={err}");
        }
    }

    #[tokio::test]
    async fn oversleeping_does_not_lose_bandwidth() {
        // 16 KiB at 2 MiB/s is an 8 ms slot; waking 12 ms late every time would cut the rate
        // to 40% if the limiter trusted sleep lengths instead of the clock.
        let rate = 2u64 << 20;
        for workers in [1usize, 4] {
            let l = Arc::new(RateLimiter::new(rate));
            let got = measure(
                l,
                workers,
                Duration::from_millis(2000),
                Duration::from_millis(12),
            )
            .await;
            let err = (got - rate as f64).abs() / rate as f64;
            assert!(
                err < 0.05,
                "{workers} workers with coarse timers: rate={got} err={err}"
            );
        }
    }

    #[tokio::test]
    async fn fairness_across_workers() {
        let l = Arc::new(RateLimiter::new(1 << 20));
        let start = Instant::now();
        let counts: Vec<Arc<AtomicU64>> = (0..4).map(|_| Arc::new(AtomicU64::new(0))).collect();
        let mut tasks = Vec::new();
        for c in &counts {
            let (l, c) = (l.clone(), c.clone());
            tasks.push(tokio::spawn(async move {
                while start.elapsed() < Duration::from_millis(2500) {
                    l.acquire(16384).await;
                    c.fetch_add(16384, Ordering::Relaxed);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let v: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        let (min, max) = (*v.iter().min().unwrap(), *v.iter().max().unwrap());
        // Whoever starts first may take the initial burst (250 ms); the rest is FIFO.
        let burst = (1u64 << 20) / 4;
        assert!(max - min <= burst + 2 * 16384, "unfair split: {v:?}");
    }

    #[tokio::test]
    async fn live_adjustment_and_unlimited() {
        let l = Arc::new(RateLimiter::new(64 * 1024));
        // Unlimited is instant.
        let u = RateLimiter::unlimited();
        let t = Instant::now();
        u.acquire(1 << 30).await;
        assert!(t.elapsed() < Duration::from_millis(50));
        // A waiter on a slow limiter is released quickly when the limit is lifted.
        let l2 = l.clone();
        let waiter = tokio::spawn(async move {
            let t = Instant::now();
            l2.acquire(4 << 20).await; // would take ~64 s at 64 KiB/s
            t.elapsed()
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        l.set_rate(0);
        let waited = waiter.await.unwrap();
        assert!(waited < Duration::from_secs(2), "waited {waited:?}");
    }

    #[tokio::test]
    async fn setting_the_same_rate_again_keeps_the_cap() {
        let rate = 256 * 1024;
        let l = Arc::new(RateLimiter::new(rate));
        let again = l.clone();
        let reapply = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                again.set_rate(rate);
            }
        });
        let start = Instant::now();
        let mut got = 0u64;
        while start.elapsed() < Duration::from_secs(1) {
            l.acquire(16384).await;
            got += 16384;
        }
        reapply.abort();
        // One second at the rate plus the initial burst (250 ms) and one chunk of slack.
        assert!(got <= rate + rate / 4 + 2 * 16384, "got {got} bytes in 1 s");
    }
}
