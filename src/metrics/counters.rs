//! KARON3 ETERNITY FIRE - Counter System
//! H0: AtomicU64 카운터 시스템

use std::sync::atomic::{AtomicU64, Ordering};
use tracing::info;

/// 전역 카운터 구조체
pub struct Counters {
    // 파서 관련
    pub pump_parse_ok: AtomicU64,
    pub pump_parse_fail: AtomicU64,
    pub fake_mint_blocked: AtomicU64,

    // TX fetch 관련
    pub tx_fetch_ok: AtomicU64,
    pub tx_fetch_fail: AtomicU64,

    // Jito 관련
    pub jito_submit_ok: AtomicU64,
    pub jito_429: AtomicU64,
    pub jito_400: AtomicU64,
    pub jito_5xx: AtomicU64,
    pub jito_other: AtomicU64,

    // 라벨 분해 카운터 (Prometheus)
    pub jito_requests_2xx: AtomicU64,
    pub jito_requests_400: AtomicU64,
    pub jito_requests_429: AtomicU64,
    pub jito_requests_5xx: AtomicU64,
    pub jito_requests_other: AtomicU64,

    // 가드레일/백프레셔 관측용 게이지
    pub inflight_requests: AtomicU64,
    pub queue_depth: AtomicU64,
    pub limiter_cooldown_active: AtomicU64,

    // 거래 관련
    pub trade_attempt: AtomicU64,
    pub trade_skip: AtomicU64,
    pub trade_success: AtomicU64,
    pub trade_fail: AtomicU64,

    // 연속 실패 카운터
    pub consecutive_failures: AtomicU64,
}

impl Counters {
    pub const fn new() -> Self {
        Self {
            pump_parse_ok: AtomicU64::new(0),
            pump_parse_fail: AtomicU64::new(0),
            fake_mint_blocked: AtomicU64::new(0),
            tx_fetch_ok: AtomicU64::new(0),
            tx_fetch_fail: AtomicU64::new(0),
            jito_submit_ok: AtomicU64::new(0),
            jito_429: AtomicU64::new(0),
            jito_400: AtomicU64::new(0),
            jito_5xx: AtomicU64::new(0),
            jito_other: AtomicU64::new(0),
            jito_requests_2xx: AtomicU64::new(0),
            jito_requests_400: AtomicU64::new(0),
            jito_requests_429: AtomicU64::new(0),
            jito_requests_5xx: AtomicU64::new(0),
            jito_requests_other: AtomicU64::new(0),
            inflight_requests: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            limiter_cooldown_active: AtomicU64::new(0),
            trade_attempt: AtomicU64::new(0),
            trade_skip: AtomicU64::new(0),
            trade_success: AtomicU64::new(0),
            trade_fail: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
        }
    }

    /// 리셋 (테스트용)
    pub fn reset(&self) {
        self.pump_parse_ok.store(0, Ordering::Relaxed);
        self.pump_parse_fail.store(0, Ordering::Relaxed);
        self.fake_mint_blocked.store(0, Ordering::Relaxed);
        self.tx_fetch_ok.store(0, Ordering::Relaxed);
        self.tx_fetch_fail.store(0, Ordering::Relaxed);
        self.jito_submit_ok.store(0, Ordering::Relaxed);
        self.jito_429.store(0, Ordering::Relaxed);
        self.jito_400.store(0, Ordering::Relaxed);
        self.jito_5xx.store(0, Ordering::Relaxed);
        self.jito_other.store(0, Ordering::Relaxed);
        self.jito_requests_2xx.store(0, Ordering::Relaxed);
        self.jito_requests_400.store(0, Ordering::Relaxed);
        self.jito_requests_429.store(0, Ordering::Relaxed);
        self.jito_requests_5xx.store(0, Ordering::Relaxed);
        self.jito_requests_other.store(0, Ordering::Relaxed);
        self.inflight_requests.store(0, Ordering::Relaxed);
        self.queue_depth.store(0, Ordering::Relaxed);
        self.limiter_cooldown_active.store(0, Ordering::Relaxed);
        self.trade_attempt.store(0, Ordering::Relaxed);
        self.trade_skip.store(0, Ordering::Relaxed);
        self.trade_success.store(0, Ordering::Relaxed);
        self.trade_fail.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }
}

/// 전역 카운터 인스턴스
pub static COUNTERS: Counters = Counters::new();

/// 1초 간격 샘플 로그 (스팸 금지 — 1줄/초)
pub fn log_snapshot() {
    let c = &COUNTERS;
    info!(
        "📊 STATS: parse={}/{} fetch={}/{} jito={}/{}/{}/{}/{} req={}/{}/{}/{}/{} q={} in={} cd={} trade={}/{}/{}/{}",
        c.pump_parse_ok.load(Ordering::Relaxed),
        c.pump_parse_fail.load(Ordering::Relaxed),
        c.tx_fetch_ok.load(Ordering::Relaxed),
        c.tx_fetch_fail.load(Ordering::Relaxed),
        c.jito_submit_ok.load(Ordering::Relaxed),
        c.jito_429.load(Ordering::Relaxed),
        c.jito_400.load(Ordering::Relaxed),
        c.jito_5xx.load(Ordering::Relaxed),
        c.jito_other.load(Ordering::Relaxed),
        c.jito_requests_2xx.load(Ordering::Relaxed),
        c.jito_requests_400.load(Ordering::Relaxed),
        c.jito_requests_429.load(Ordering::Relaxed),
        c.jito_requests_5xx.load(Ordering::Relaxed),
        c.jito_requests_other.load(Ordering::Relaxed),
        c.queue_depth.load(Ordering::Relaxed),
        c.inflight_requests.load(Ordering::Relaxed),
        c.limiter_cooldown_active.load(Ordering::Relaxed),
        c.trade_attempt.load(Ordering::Relaxed),
        c.trade_skip.load(Ordering::Relaxed),
        c.trade_success.load(Ordering::Relaxed),
        c.trade_fail.load(Ordering::Relaxed),
    );
}

/// JSON 형식 스냅샷 (모니터링용)
pub fn get_snapshot_json() -> String {
    let c = &COUNTERS;
    format!(
        r#"{{"pump_parse_ok":{},"pump_parse_fail":{},"fake_mint_blocked":{},"tx_fetch_ok":{},"tx_fetch_fail":{},"jito_submit_ok":{},"jito_429":{},"jito_400":{},"jito_5xx":{},"jito_other":{},"jito_requests_2xx":{},"jito_requests_400":{},"jito_requests_429":{},"jito_requests_5xx":{},"jito_requests_other":{},"inflight_requests":{},"queue_depth":{},"limiter_cooldown_active":{},"trade_attempt":{},"trade_skip":{},"trade_success":{},"trade_fail":{},"consecutive_failures":{}}}"#,
        c.pump_parse_ok.load(Ordering::Relaxed),
        c.pump_parse_fail.load(Ordering::Relaxed),
        c.fake_mint_blocked.load(Ordering::Relaxed),
        c.tx_fetch_ok.load(Ordering::Relaxed),
        c.tx_fetch_fail.load(Ordering::Relaxed),
        c.jito_submit_ok.load(Ordering::Relaxed),
        c.jito_429.load(Ordering::Relaxed),
        c.jito_400.load(Ordering::Relaxed),
        c.jito_5xx.load(Ordering::Relaxed),
        c.jito_other.load(Ordering::Relaxed),
        c.jito_requests_2xx.load(Ordering::Relaxed),
        c.jito_requests_400.load(Ordering::Relaxed),
        c.jito_requests_429.load(Ordering::Relaxed),
        c.jito_requests_5xx.load(Ordering::Relaxed),
        c.jito_requests_other.load(Ordering::Relaxed),
        c.inflight_requests.load(Ordering::Relaxed),
        c.queue_depth.load(Ordering::Relaxed),
        c.limiter_cooldown_active.load(Ordering::Relaxed),
        c.trade_attempt.load(Ordering::Relaxed),
        c.trade_skip.load(Ordering::Relaxed),
        c.trade_success.load(Ordering::Relaxed),
        c.trade_fail.load(Ordering::Relaxed),
        c.consecutive_failures.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counters_increment() {
        COUNTERS.reset();
        COUNTERS.pump_parse_ok.fetch_add(1, Ordering::Relaxed);
        assert_eq!(COUNTERS.pump_parse_ok.load(Ordering::Relaxed), 1);
    }
}
