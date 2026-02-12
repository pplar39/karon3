//! HELL MARCH: Dynamic Jito Tip Calculator

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use parking_lot::RwLock;
use tracing::debug;

const TIP_HISTORY_SIZE: usize = 100;
const MIN_TIP_LAMPORTS: u64 = 100_000;
const MAX_TIP_LAMPORTS: u64 = 10_000_000;
const TARGET_SUCCESS_RATE: f64 = 0.80;

#[derive(Debug, Clone, Copy)]
pub struct TipOutcome {
    pub tip_lamports: u64,
    pub success: bool,
    pub timestamp_us: u64,
}

pub struct DynamicTipCalculator {
    current_tip: AtomicU64,
    history: Arc<RwLock<VecDeque<TipOutcome>>>,
    multiplier: AtomicU64,
}

impl DynamicTipCalculator {
    pub fn new(initial_tip: u64) -> Self {
        Self {
            current_tip: AtomicU64::new(initial_tip.max(MIN_TIP_LAMPORTS).min(MAX_TIP_LAMPORTS)),
            history: Arc::new(RwLock::new(VecDeque::with_capacity(TIP_HISTORY_SIZE))),
            multiplier: AtomicU64::new(1000),
        }
    }

    #[inline]
    pub fn get_tip(&self) -> u64 {
        let base = self.current_tip.load(Ordering::Relaxed);
        let mult = self.multiplier.load(Ordering::Relaxed);
        let adjusted = (base as u128 * mult as u128 / 1000) as u64;
        adjusted.max(MIN_TIP_LAMPORTS).min(MAX_TIP_LAMPORTS)
    }

    pub fn record_outcome(&self, tip_lamports: u64, success: bool) {
        let outcome = TipOutcome {
            tip_lamports,
            success,
            timestamp_us: crate::time::monotonic_now_us(),
        };

        let mut history = self.history.write();
        if history.len() >= TIP_HISTORY_SIZE {
            history.pop_front();
        }
        history.push_back(outcome);
        self.recalculate(&history);
    }

    fn recalculate(&self, history: &VecDeque<TipOutcome>) {
        if history.len() < 10 {
            return;
        }

        let recent: Vec<_> = history.iter().rev().take(20).collect();
        let successes = recent.iter().filter(|o| o.success).count();
        let success_rate = successes as f64 / recent.len() as f64;

        let current_mult = self.multiplier.load(Ordering::Relaxed);
        let new_mult = if success_rate < TARGET_SUCCESS_RATE - 0.1 {
            (current_mult * 110 / 100).min(2000)
        } else if success_rate > TARGET_SUCCESS_RATE + 0.1 {
            (current_mult * 95 / 100).max(800)
        } else {
            current_mult
        };

        if new_mult != current_mult {
            self.multiplier.store(new_mult, Ordering::Relaxed);
            debug!("Tip multiplier: {}x -> {}x", current_mult as f64 / 1000.0, new_mult as f64 / 1000.0);
        }

        let successful_tips: Vec<u64> = history.iter().filter(|o| o.success).map(|o| o.tip_lamports).collect();
        if successful_tips.len() >= 5 {
            let mut sorted = successful_tips.clone();
            sorted.sort_unstable();
            let p25_idx = sorted.len() / 4;
            let new_base = sorted[p25_idx];
            let old_base = self.current_tip.load(Ordering::Relaxed);
            let adjusted_base = (old_base * 7 + new_base * 3) / 10;
            self.current_tip.store(adjusted_base.max(MIN_TIP_LAMPORTS).min(MAX_TIP_LAMPORTS), Ordering::Relaxed);
        }
    }

    pub fn get_urgent_tip(&self) -> u64 {
        (self.get_tip() * 15 / 10).min(MAX_TIP_LAMPORTS)
    }
}

impl Default for DynamicTipCalculator {
    fn default() -> Self {
        Self::new(MIN_TIP_LAMPORTS)
    }
}
