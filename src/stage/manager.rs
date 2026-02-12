//! KARON3 ETERNITY FIRE - Stage Manager
//! H6: 수동 승격 + 자동 강등

use std::sync::atomic::Ordering;
use tracing::{info, warn, error};

use crate::config::runtime::{TRADE_ENABLED, STAGE_FREEZE, emergency_stop};
use crate::metrics::counters::COUNTERS;

/// 거래 단계
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Conservative,  // Stage 1: 0.1 SOL max
    Moderate,      // Stage 2: 0.3 SOL max
    Aggressive,    // Stage 3: 0.5 SOL max
}

impl Stage {
    pub fn risk_score(&self) -> f64 {
        match self {
            Stage::Conservative => 6.0,
            Stage::Moderate => 7.5,
            Stage::Aggressive => 8.0,
        }
    }

    pub fn risk_score_x10(&self) -> u32 {
        (self.risk_score() * 10.0) as u32
    }

    pub fn max_position_sol(&self) -> f64 {
        match self {
            Stage::Conservative => 0.1,
            Stage::Moderate => 0.3,
            Stage::Aggressive => 0.5,
        }
    }

    pub fn max_concurrent_trades(&self) -> u32 {
        match self {
            Stage::Conservative => 1,
            Stage::Moderate => 2,
            Stage::Aggressive => 3,
        }
    }

    pub fn stop_loss_pct(&self) -> f64 {
        match self {
            Stage::Conservative => 5.0,
            Stage::Moderate => 7.0,
            Stage::Aggressive => 10.0,
        }
    }
}

/// 라이브 메트릭스 (강등 판단용)
pub struct LiveMetrics {
    pub jito_400_total: u64,
    pub consecutive_parse_failures: u64,
    pub session_pnl_sol: f64,
}

impl LiveMetrics {
    pub fn from_counters() -> Self {
        Self {
            jito_400_total: COUNTERS.jito_400.load(Ordering::Relaxed),
            consecutive_parse_failures: COUNTERS.consecutive_failures.load(Ordering::Relaxed),
            session_pnl_sol: 0.0, // 별도 계산 필요
        }
    }
}

/// Stage Manager
pub struct StageManager {
    current: Stage,
}

impl StageManager {
    pub fn new() -> Self {
        Self {
            current: Stage::Conservative,
        }
    }

    pub fn current_stage(&self) -> Stage {
        self.current
    }

    /// 자동 강등 체크 (이건 자동이어야 함 — 위험 감지 시 즉시)
    pub fn check_auto_demotion(&mut self, metrics: &LiveMetrics) -> bool {
        // 400 발생 → 즉시 Stage 1로 강등 + trade_enabled=false
        if metrics.jito_400_total > 0 {
            error!("🛑 Jito 400 detected → Stage Conservative + trade_enabled=false");
            self.force_stage(Stage::Conservative);
            emergency_stop("Jito 400 structural error");
            return true;
        }

        // 연속 파서 실패 > 10 → Stage 1로 강등
        if metrics.consecutive_parse_failures > 10 {
            warn!("⚠️ Consecutive parse failures > 10 → Stage Conservative");
            self.force_stage(Stage::Conservative);
            return true;
        }

        // PnL 급락 (라이브) → Stage 1
        if metrics.session_pnl_sol < -0.05 {
            error!("🛑 Session PnL < -0.05 SOL → Stage Conservative + trade_enabled=false");
            self.force_stage(Stage::Conservative);
            emergency_stop("Session PnL exceeded loss limit");
            return true;
        }

        false
    }

    /// 강제 스테이지 설정
    fn force_stage(&mut self, target: Stage) {
        self.current = target;
        info!("📊 Stage forced to {:?}", target);
    }

    /// 수동 승격 (CLI 명령으로만)
    pub fn manual_promote(&mut self, target: Stage) -> Result<(), &'static str> {
        if STAGE_FREEZE.load(Ordering::SeqCst) {
            return Err("stage frozen");
        }

        // 현재 스테이지보다 낮으면 안됨
        if target.risk_score() < self.current.risk_score() {
            return Err("cannot demote via promote command, use force_stage");
        }

        self.current = target;
        info!("📈 Stage promoted to {:?}", target);
        Ok(())
    }

    /// 수동 강등
    pub fn manual_demote(&mut self, target: Stage) {
        self.current = target;
        info!("📉 Stage demoted to {:?}", target);
    }
}

impl Default for StageManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stage_defaults() {
        let manager = StageManager::new();
        assert_eq!(manager.current_stage(), Stage::Conservative);
    }

    #[test]
    fn test_auto_demotion_on_400() {
        let mut manager = StageManager::new();
        manager.current = Stage::Aggressive;

        let metrics = LiveMetrics {
            jito_400_total: 1,
            consecutive_parse_failures: 0,
            session_pnl_sol: 0.0,
        };

        let demoted = manager.check_auto_demotion(&metrics);
        assert!(demoted);
        assert_eq!(manager.current_stage(), Stage::Conservative);
    }
}
