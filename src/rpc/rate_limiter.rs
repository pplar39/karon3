use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct RateLimiter {
    tokens: AtomicU64, // Current tokens (scaled by 1000)
    max_tokens: u64,   // Bucket capacity
    refill_rate: u64,  // Tokens per second
    last_refill: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(max_tokens: u64, refill_rate: u64) -> Self {
        Self {
            tokens: AtomicU64::new(max_tokens * 1000),
            max_tokens,
            refill_rate,
            last_refill: Mutex::new(Instant::now()),
        }
    }

    fn refill(&self) {
        let mut last = self.last_refill.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(*last);
        let millis = elapsed.as_millis() as u64;

        if millis > 0 {
            // 1 token = 1000 units.
            // Rate = R tokens/sec = R units/ms.
            let to_add = millis * self.refill_rate;

            if to_add > 0 {
                let max_scaled = self.max_tokens * 1000;
                let mut current = self.tokens.load(Ordering::Relaxed);

                // CAS loop to safely update tokens
                loop {
                    if current >= max_scaled {
                        break;
                    }
                    let new_val = std::cmp::min(current + to_add, max_scaled);
                    match self.tokens.compare_exchange(
                        current,
                        new_val,
                        Ordering::SeqCst,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(x) => current = x,
                    }
                }
                *last = now;
            }
        }
    }

    /// Try to acquire tokens. Returns true if successful.
    pub fn try_acquire(&self, tokens: u64) -> bool {
        self.refill();
        let requested = tokens * 1000;

        let mut current = self.tokens.load(Ordering::Relaxed);
        loop {
            if current < requested {
                return false;
            }

            match self.tokens.compare_exchange(
                current,
                current - requested,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(x) => current = x,
            }
        }
    }

    /// Blocking acquire - waits until tokens available
    pub async fn acquire(&self, tokens: u64) {
        while !self.try_acquire(tokens) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_initial_capacity() {
        let rl = RateLimiter::new(10, 1);
        assert!(rl.try_acquire(10));
        assert!(!rl.try_acquire(1));
    }

    #[test]
    fn test_refill() {
        let rl = RateLimiter::new(10, 10); // 10 tokens/sec
        assert!(rl.try_acquire(10)); // Empty it
        assert!(!rl.try_acquire(1));

        // Wait 100ms -> should gain ~1 token (10 * 0.1 = 1)
        thread::sleep(Duration::from_millis(110));

        assert!(rl.try_acquire(1));
    }

    #[tokio::test]
    async fn test_blocking_acquire() {
        let rl = RateLimiter::new(1, 10); // 1 token max, 10/sec refill rate
        assert!(rl.try_acquire(1)); // Empty

        let start = Instant::now();
        rl.acquire(1).await; // Should wait ~100ms
        let elapsed = start.elapsed();

        assert!(elapsed.as_millis() >= 90);
    }
}
