//! Speed limiter: a token bucket that any number of connections and downloads share.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

pub struct Limiter {
    /// Bytes per second; 0 means unlimited.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Limiter {
    pub fn new(rate: u64) -> Self {
        Limiter { rate: AtomicU64::new(rate), bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }) }
    }

    /// Changes the rate at once for every download using this limiter; 0 lifts the limit.
    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Accounts for `n` received bytes and says how long to wait before reading on, if they
    /// went over the rate. Reading slower makes TCP slow the sender down, so the network use
    /// follows the limit.
    pub fn reserve(&self, n: u64) -> Option<Duration> {
        let rate = self.rate();
        if rate == 0 {
            return None;
        }
        let mut b = self.bucket.lock().unwrap();
        let now = Instant::now();
        // A quarter second of burst keeps the rate smooth without long stalls.
        let burst = (rate as f64 / 4.0).max(64.0 * 1024.0);
        b.tokens = (b.tokens + (now - b.last).as_secs_f64() * rate as f64).min(burst);
        b.last = now;
        b.tokens -= n as f64;
        // Going into debt and sleeping it off keeps the total right with many takers.
        (b.tokens < 0.0).then(|| Duration::from_secs_f64(-b.tokens / rate as f64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u64 = 1 << 20;

    /// Nothing is free at the start, and takers share one debt: each waits for all that was
    /// taken before it too, so together they stay at the rate.
    #[tokio::test(start_paused = true)]
    async fn what_is_taken_is_waited_for_at_the_rate() {
        let limiter = Limiter::new(RATE);
        assert_eq!(limiter.reserve(RATE / 2), Some(Duration::from_millis(500)));
        assert_eq!(limiter.reserve(RATE / 2), Some(Duration::from_secs(1)));
        tokio::time::advance(Duration::from_millis(750)).await;
        assert_eq!(limiter.reserve(0), Some(Duration::from_millis(250)), "a quarter of the debt is left");
        tokio::time::advance(Duration::from_millis(250)).await;
        assert_eq!(limiter.reserve(0), None, "paid off");
    }

    /// Time without traffic saves up a quarter second of the rate, no more (64 KiB at
    /// least, for low rates).
    #[tokio::test(start_paused = true)]
    async fn idle_time_saves_up_a_quarter_second() {
        let limiter = Limiter::new(RATE);
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(limiter.reserve(RATE / 4), None, "the burst");
        assert_eq!(limiter.reserve(RATE / 4), Some(Duration::from_millis(250)), "more than the burst was saved up");

        let slow = Limiter::new(4096);
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(slow.reserve(64 * 1024), None, "the least burst");
        assert_eq!(slow.reserve(4096), Some(Duration::from_secs(1)));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_changes_at_once_and_zero_lifts_the_limit() {
        let limiter = Limiter::new(0);
        assert_eq!(limiter.reserve(u64::MAX), None);
        limiter.set_rate(RATE);
        assert_eq!(limiter.reserve(RATE), Some(Duration::from_secs(1)));
        limiter.set_rate(2 * RATE);
        assert_eq!(limiter.reserve(0), Some(Duration::from_millis(500)), "the debt is paid at the new rate");
        limiter.set_rate(0);
        assert_eq!(limiter.reserve(RATE), None);
    }
}
