//! KARON3 ETERNITY FIRE - Kill Switch 3종
//! H0: 런타임 안전장치

use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// 킬스위치 3종
pub static TRADE_ENABLED: AtomicBool = AtomicBool::new(false);   // 거래 허용
pub static DRY_RUN: AtomicBool = AtomicBool::new(true);          // 제출 안 함
pub static STAGE_FREEZE: AtomicBool = AtomicBool::new(true);     // Stage 변경 금지

/// 거래 실행 전 반드시 체크
#[inline]
pub fn can_trade() -> bool {
    TRADE_ENABLED.load(Ordering::SeqCst)
        && !DRY_RUN.load(Ordering::SeqCst)
        && !STAGE_FREEZE.load(Ordering::SeqCst)
}

/// DRY_RUN 모드에서도 파싱/분석은 진행
#[inline]
pub fn is_dry_run() -> bool {
    DRY_RUN.load(Ordering::SeqCst)
}

/// 거래 활성화 여부
#[inline]
pub fn is_trade_enabled() -> bool {
    TRADE_ENABLED.load(Ordering::SeqCst)
}

/// 안전 정지 - 모든 거래 중단
pub fn emergency_stop(reason: &str) {
    warn!("🛑 EMERGENCY STOP: {}", reason);
    TRADE_ENABLED.store(false, Ordering::SeqCst);
    STAGE_FREEZE.store(true, Ordering::SeqCst);
}

/// SIGHUP 핸들러용 토글 함수
pub fn toggle_trade_enabled() {
    let current = TRADE_ENABLED.load(Ordering::SeqCst);
    let new_val = !current;
    TRADE_ENABLED.store(new_val, Ordering::SeqCst);
    info!("🔄 TRADE_ENABLED toggled: {} -> {}", current, new_val);
}

/// 런타임 상태 로그
pub fn log_runtime_state() {
    info!(
        "⚙️ Runtime: trade_enabled={} dry_run={} stage_freeze={} can_trade={}",
        TRADE_ENABLED.load(Ordering::SeqCst),
        DRY_RUN.load(Ordering::SeqCst),
        STAGE_FREEZE.load(Ordering::SeqCst),
        can_trade()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_trade_default_false() {
        // 기본값: 거래 불가
        assert!(!can_trade());
    }

    #[test]
    fn test_emergency_stop() {
        TRADE_ENABLED.store(true, Ordering::SeqCst);
        DRY_RUN.store(false, Ordering::SeqCst);
        STAGE_FREEZE.store(false, Ordering::SeqCst);
        
        emergency_stop("test");
        
        assert!(!TRADE_ENABLED.load(Ordering::SeqCst));
        assert!(STAGE_FREEZE.load(Ordering::SeqCst));
    }
}
