//! KARON3 ETERNITY FIRE - Jito Rate Limiter
//! H3: 429 대응 - 상태 머신 + 자동 정지

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn, error};

use crate::config::runtime::{TRADE_ENABLED, emergency_stop};
use crate::metrics::counters::COUNTERS;

/// Jito Rate Limiter
pub struct JitoRateLimiter {
    min_interval: Duration,          // 1000ms 하한
    last_submit: Instant,
    consecutive_429: AtomicU32,
    max_consecutive_429: u32,        // 5회 → 자동 정지
}

impl JitoRateLimiter {
    pub fn new() -> Self {
        Self {
            min_interval: Duration::from_millis(1000),
            last_submit: Instant::now() - Duration::from_secs(10),
            consecutive_429: AtomicU32::new(0),
            max_consecutive_429: 5,
        }
    }

    /// 다음 슬롯까지 대기
    pub async fn wait_for_slot(&mut self) {
        let elapsed = self.last_submit.elapsed();
        if elapsed < self.min_interval {
            let wait_time = self.min_interval - elapsed;
            tokio::time::sleep(wait_time).await;
        }
    }

    /// 성공 시 호출
    pub fn on_success(&mut self) {
        self.last_submit = Instant::now();
        self.consecutive_429.store(0, Ordering::Relaxed);
        COUNTERS.jito_submit_ok.fetch_add(1, Ordering::Relaxed);
        info!("✅ Jito submit OK");
    }

    /// 429 에러 시 호출
    pub async fn on_429(&mut self) {
        let count = self.consecutive_429.fetch_add(1, Ordering::Relaxed) + 1;
        COUNTERS.jito_429.fetch_add(1, Ordering::Relaxed);

        // 지수 백오프: 1s, 2s, 4s, 8s, 16s
        let backoff_ms = 1000 * 2u64.pow(count.min(4));
        let backoff = Duration::from_millis(backoff_ms);
        warn!("⚠️ Jito 429 #{} — backoff {}ms", count, backoff_ms);
        
        tokio::time::sleep(backoff).await;

        // 연속 5회 → 자동 안전 정지
        if count >= self.max_consecutive_429 {
            error!("🛑 Jito 429 x{} — TRADE_ENABLED → false", count);
            emergency_stop("Jito 429 rate limit exceeded");
        }

        self.last_submit = Instant::now();
    }

    /// 400 에러 시 호출 (구조 오류)
    pub fn on_400(&self, error_msg: &str) {
        COUNTERS.jito_400.fetch_add(1, Ordering::Relaxed);
        error!("🛑 Jito 400 — TRADE_ENABLED → false (구조 오류): {}", error_msg);
        emergency_stop("Jito 400 structural error");
    }

    /// 연속 429 카운트 조회
    pub fn consecutive_429_count(&self) -> u32 {
        self.consecutive_429.load(Ordering::Relaxed)
    }

    /// 리셋
    pub fn reset(&mut self) {
        self.consecutive_429.store(0, Ordering::Relaxed);
        self.last_submit = Instant::now() - Duration::from_secs(10);
    }
}

impl Default for JitoRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_consecutive_429_count() {
        let mut limiter = JitoRateLimiter::new();
        limiter.min_interval = Duration::from_millis(10); // 테스트용 짧은 간격
        
        // 3번 429
        for _ in 0..3 {
            limiter.on_429().await;
        }
        
        assert_eq!(limiter.consecutive_429_count(), 3);
    }
}
