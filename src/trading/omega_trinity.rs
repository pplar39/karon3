//! OMEGA TRINITY: 3-Rule HFT Architecture
//!
//! Replaces the legacy 12-rule system with 3 core rules:
//! 1. THE WALL (EntryGuard) - Blocks bad entries before they happen
//! 2. THE PULSE (ResourcePulse) - Budget and rate management
//! 3. THE CIRCUIT (OmegaBreaker) - Drawdown-based circuit breaker

use parking_lot::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Rejection reasons from OMEGA TRINITY
#[derive(Debug, Clone, PartialEq)]
pub enum RejectionReason {
    /// THE WALL rejection
    LowSafetyScore { score: u8, required: u8 },
    /// THE PULSE rejection - budget
    BudgetExhausted { spent: f64, limit: f64 },
    /// THE PULSE rejection - rate limit
    RateLimited,
    /// THE CIRCUIT rejection
    CircuitOpen {
        drawdown_pct: f64,
        cooldown_remaining_secs: u64,
    },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::LowSafetyScore { score, required } => {
                write!(f, "Safety score {} < required {}", score, required)
            }
            RejectionReason::BudgetExhausted { spent, limit } => {
                write!(f, "Budget exhausted: {:.4} / {:.4} SOL", spent, limit)
            }
            RejectionReason::RateLimited => {
                write!(f, "Rate limited")
            }
            RejectionReason::CircuitOpen {
                drawdown_pct,
                cooldown_remaining_secs,
            } => {
                write!(
                    f,
                    "Circuit open: {:.2}% drawdown, {}s cooldown remaining",
                    drawdown_pct, cooldown_remaining_secs
                )
            }
        }
    }
}

/// Trade context for rule evaluation
#[derive(Debug, Clone)]
pub struct TradeContext {
    /// Safety score from rug detection (0-100)
    pub safety_score: u8,
    /// Estimated cost in SOL
    pub estimated_cost_sol: f64,
    /// Current balance in SOL
    pub current_balance_sol: f64,
}

// =============================================================================
// RULE 1: THE WALL (EntryGuard)
// =============================================================================

/// THE WALL: Unified entry filter
///
/// "들어오지 못하게 하는 것이 나가게 하는 것보다 100배 저렴하다"
#[derive(Debug, Clone)]
pub struct EntryGuard {
    /// Minimum safety score required (0-100)
    pub min_safety_score: u8,
}

impl Default for EntryGuard {
    fn default() -> Self {
        Self {
            min_safety_score: 70,
        }
    }
}

impl EntryGuard {
    pub fn new(min_safety_score: u8) -> Self {
        Self { min_safety_score }
    }

    pub fn check(&self, context: &TradeContext) -> Result<(), RejectionReason> {
        if context.safety_score < self.min_safety_score {
            return Err(RejectionReason::LowSafetyScore {
                score: context.safety_score,
                required: self.min_safety_score,
            });
        }
        Ok(())
    }
}

// =============================================================================
// RULE 2: THE PULSE (ResourcePulse)
// =============================================================================

/// THE PULSE: Budget and rate management
///
/// "느려지면 죽는다. 멈추면 부활한다."
pub struct ResourcePulse {
    /// Total budget in SOL
    pub budget_sol: f64,
    /// Maximum requests per second
    pub max_rps: u32,

    // Internal state
    spent_lamports: AtomicU64,
    request_count: AtomicU64,
    window_start: RwLock<Instant>,
}

impl ResourcePulse {
    pub fn new(budget_sol: f64, max_rps: u32) -> Self {
        Self {
            budget_sol,
            max_rps,
            spent_lamports: AtomicU64::new(0),
            request_count: AtomicU64::new(0),
            window_start: RwLock::new(Instant::now()),
        }
    }

    pub fn check(&self, context: &TradeContext) -> Result<(), RejectionReason> {
        // Check budget
        let spent = self.spent_lamports.load(Ordering::Relaxed) as f64 / 1_000_000_000.0;
        let new_spend = spent + context.estimated_cost_sol;

        if new_spend > self.budget_sol {
            return Err(RejectionReason::BudgetExhausted {
                spent,
                limit: self.budget_sol,
            });
        }

        // Check rate limit
        self.maybe_reset_window();
        let current_count = self.request_count.load(Ordering::Relaxed);
        if current_count >= self.max_rps as u64 {
            return Err(RejectionReason::RateLimited);
        }

        Ok(())
    }

    pub fn record_spend(&self, amount_sol: f64) {
        let lamports = (amount_sol * 1_000_000_000.0) as u64;
        self.spent_lamports.fetch_add(lamports, Ordering::Relaxed);
    }

    /// Count a trade-send attempt for max_rps gating (independent of spend).
    pub fn record_attempt(&self) {
        self.maybe_reset_window();
        self.request_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_spent(&self) -> f64 {
        self.spent_lamports.load(Ordering::Relaxed) as f64 / 1_000_000_000.0
    }

    fn maybe_reset_window(&self) {
        let mut start = self.window_start.write();
        if start.elapsed().as_secs() >= 1 {
            self.request_count.store(0, Ordering::Relaxed);
            *start = Instant::now();
        }
    }
}

impl std::fmt::Debug for ResourcePulse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResourcePulse")
            .field("budget_sol", &self.budget_sol)
            .field("max_rps", &self.max_rps)
            .field("spent", &self.get_spent())
            .finish()
    }
}

// =============================================================================
// RULE 3: THE CIRCUIT (OmegaBreaker)
// =============================================================================

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;

/// THE CIRCUIT: Drawdown-based circuit breaker
///
/// "피가 5% 빠지면 자동으로 지혈한다"
pub struct OmegaBreaker {
    /// Maximum drawdown percentage before tripping
    pub max_drawdown_pct: f64,
    /// Cooldown period in seconds
    pub cooldown_secs: u64,
    /// Measurement window in seconds
    pub window_secs: u64,

    // Internal state
    state: std::sync::atomic::AtomicU8,
    baseline_value: AtomicU64, // Scaled by 1e9, restored after cooldown reset
    peak_value: AtomicU64,     // Scaled by 1e9
    current_value: AtomicU64,
    last_triggered: RwLock<Option<Instant>>,
}

impl OmegaBreaker {
    pub fn new(max_drawdown_pct: f64, cooldown_secs: u64, window_secs: u64) -> Self {
        Self {
            max_drawdown_pct,
            cooldown_secs,
            window_secs,
            state: std::sync::atomic::AtomicU8::new(STATE_CLOSED),
            baseline_value: AtomicU64::new(0),
            peak_value: AtomicU64::new(0),
            current_value: AtomicU64::new(0),
            last_triggered: RwLock::new(None),
        }
    }

    /// Seed baseline equity so drawdown works before first profit,
    /// and so cooldown reset restores a non-zero baseline.
    pub fn seed_equity(&self, equity_sol: f64) {
        let scaled = (equity_sol.max(0.0) * 1e9) as u64;
        self.baseline_value.store(scaled, Ordering::SeqCst);
        self.current_value.store(scaled, Ordering::SeqCst);
        self.peak_value.store(scaled, Ordering::SeqCst);
    }

    pub fn check(&self, _context: &TradeContext) -> Result<(), RejectionReason> {
        if self.is_open() {
            // Try to reset if cooldown has passed
            if !self.try_reset() {
                let elapsed = self
                    .last_triggered
                    .read()
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0);
                let remaining = self.cooldown_secs.saturating_sub(elapsed);

                return Err(RejectionReason::CircuitOpen {
                    drawdown_pct: self.calculate_drawdown(),
                    cooldown_remaining_secs: remaining,
                });
            }
        }
        Ok(())
    }

    /// Record a trade result (positive or negative PnL)
    pub fn record_pnl(&self, pnl_sol: f64) {
        // Update current value
        let current = self.current_value.load(Ordering::Relaxed) as f64 / 1e9;
        let new_value = current + pnl_sol;
        let new_scaled = (new_value.max(0.0) * 1e9) as u64;
        self.current_value.store(new_scaled, Ordering::Relaxed);

        // Update peak if new high
        if new_value > 0.0 {
            let peak = self.peak_value.load(Ordering::Relaxed) as f64 / 1e9;
            if new_value > peak {
                self.peak_value.store(new_scaled, Ordering::SeqCst);
            }
        }

        // Check if we should trip
        self.check_and_trip();
    }

    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_OPEN
    }

    pub fn calculate_drawdown(&self) -> f64 {
        let peak = self.peak_value.load(Ordering::Relaxed) as f64 / 1e9;
        let current = self.current_value.load(Ordering::Relaxed) as f64 / 1e9;

        if peak <= 0.0 {
            return 0.0;
        }

        ((peak - current) / peak * 100.0).max(0.0)
    }

    fn check_and_trip(&self) {
        let drawdown = self.calculate_drawdown();
        if drawdown >= self.max_drawdown_pct {
            self.trip();
        }
    }

    fn trip(&self) {
        self.state.store(STATE_OPEN, Ordering::SeqCst);
        let mut last = self.last_triggered.write();
        *last = Some(Instant::now());
    }

    fn try_reset(&self) -> bool {
        let last = self.last_triggered.read();
        if let Some(triggered_at) = *last {
            if triggered_at.elapsed().as_secs() >= self.cooldown_secs {
                drop(last);
                self.state.store(STATE_CLOSED, Ordering::SeqCst);
                // Reset tracking to baseline (not zero) so losses can trip again.
                let base = self.baseline_value.load(Ordering::Relaxed);
                self.peak_value.store(base, Ordering::Relaxed);
                self.current_value.store(base, Ordering::Relaxed);
                return true;
            }
        }
        false
    }
}

impl std::fmt::Debug for OmegaBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmegaBreaker")
            .field("max_drawdown_pct", &self.max_drawdown_pct)
            .field("cooldown_secs", &self.cooldown_secs)
            .field("state", &if self.is_open() { "OPEN" } else { "CLOSED" })
            .field("current_drawdown", &self.calculate_drawdown())
            .finish()
    }
}

// =============================================================================
// OMEGA TRINITY: The Unified System
// =============================================================================

/// OMEGA TRINITY: The 3-Rule HFT Architecture
///
/// "규칙이 12개인 시스템은 12가지 방법으로 실패한다.
///  규칙이 3개인 시스템은 3가지 방법으로 성공한다."
pub struct OmegaTrinity {
    pub entry_guard: EntryGuard,
    pub resource_pulse: ResourcePulse,
    pub circuit_breaker: OmegaBreaker,
}

impl OmegaTrinity {
    pub fn new(
        min_safety_score: u8,
        budget_sol: f64,
        max_rps: u32,
        max_drawdown_pct: f64,
        cooldown_secs: u64,
        window_secs: u64,
    ) -> Self {
        let breaker = OmegaBreaker::new(max_drawdown_pct, cooldown_secs, window_secs);
        breaker.seed_equity(budget_sol);
        Self {
            entry_guard: EntryGuard::new(min_safety_score),
            resource_pulse: ResourcePulse::new(budget_sol, max_rps),
            circuit_breaker: breaker,
        }
    }

    /// Default configuration
    pub fn default_config() -> Self {
        Self::new(
            70,   // min_safety_score
            10.0, // budget_sol
            50,   // max_rps
            5.0,  // max_drawdown_pct
            300,  // cooldown_secs
            600,  // window_secs
        )
    }

    /// Single entry point for all rule checks
    ///
    /// This is intentionally a single function to enforce simplicity.
    pub fn should_trade(&self, context: &TradeContext) -> Result<(), RejectionReason> {
        // THE WALL - Entry Filter
        self.entry_guard.check(context)?;

        // THE PULSE - Resource Check
        self.resource_pulse.check(context)?;

        // THE CIRCUIT - Drawdown Check
        self.circuit_breaker.check(context)?;

        Ok(())
    }

    /// Record a completed trade
    pub fn record_trade(&self, cost_sol: f64, pnl_sol: f64) {
        self.resource_pulse.record_spend(cost_sol);
        self.circuit_breaker.record_pnl(pnl_sol);
    }
}

impl std::fmt::Debug for OmegaTrinity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmegaTrinity")
            .field("entry_guard", &self.entry_guard)
            .field("resource_pulse", &self.resource_pulse)
            .field("circuit_breaker", &self.circuit_breaker)
            .finish()
    }
}

// =============================================================================
// TESTS
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_guard_passes_good_score() {
        let guard = EntryGuard::new(70);
        let context = TradeContext {
            safety_score: 85,
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };
        assert!(guard.check(&context).is_ok());
    }

    #[test]
    fn test_entry_guard_rejects_low_score() {
        let guard = EntryGuard::new(70);
        let context = TradeContext {
            safety_score: 50,
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };
        let result = guard.check(&context);
        assert!(matches!(
            result,
            Err(RejectionReason::LowSafetyScore { .. })
        ));
    }

    #[test]
    fn test_resource_pulse_budget_tracking() {
        let pulse = ResourcePulse::new(1.0, 100);
        let context = TradeContext {
            safety_score: 80,
            estimated_cost_sol: 0.5,
            current_balance_sol: 10.0,
        };

        // Should pass initially
        assert!(pulse.check(&context).is_ok());

        // Record spend
        pulse.record_spend(0.6);

        // Now should fail (0.6 + 0.5 > 1.0)
        let result = pulse.check(&context);
        assert!(matches!(
            result,
            Err(RejectionReason::BudgetExhausted { .. })
        ));
    }

    #[test]
    fn test_omega_breaker_trips_on_drawdown() {
        let breaker = OmegaBreaker::new(5.0, 0, 600); // 0 cooldown for testing

        // Record positive PnL to establish peak
        breaker.record_pnl(1.0);

        // Record loss that exceeds 5% drawdown
        breaker.record_pnl(-0.1); // Now at 0.9, drawdown = 10%

        assert!(breaker.is_open());
    }

    #[test]
    fn test_trinity_full_flow() {
        let trinity = OmegaTrinity::default_config();

        let good_context = TradeContext {
            safety_score: 80,
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };

        // Should pass
        assert!(trinity.should_trade(&good_context).is_ok());

        let bad_context = TradeContext {
            safety_score: 30, // Low safety
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };

        // Should fail on entry guard
        let result = trinity.should_trade(&bad_context);
        assert!(matches!(
            result,
            Err(RejectionReason::LowSafetyScore { .. })
        ));
    }

    #[test]
    fn resource_pulse_rate_limit_blocks_after_max_rps_attempts() {
        let pulse = ResourcePulse::new(10.0, 2);
        let ctx = TradeContext {
            safety_score: 100,
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };
        pulse.record_attempt();
        pulse.record_attempt();
        let res = pulse.check(&ctx);
        assert!(matches!(res, Err(RejectionReason::RateLimited)));
    }

    #[test]
    fn breaker_seed_equity_enables_drawdown_before_first_profit() {
        let breaker = OmegaBreaker::new(5.0, 0, 600);
        breaker.seed_equity(10.0);

        // Loss of 1.0 SOL on seeded 10.0 = 10% drawdown > 5% threshold
        breaker.record_pnl(-1.0);
        assert!(breaker.is_open());
    }

    #[test]
    fn breaker_reset_restores_baseline_not_zero() {
        let breaker = OmegaBreaker::new(5.0, 0, 600); // 0 cooldown for testing
        breaker.seed_equity(10.0);

        // Trip it
        breaker.record_pnl(-1.0); // 10% drawdown
        assert!(breaker.is_open());

        // Reset (0 cooldown means immediate)
        let ctx = TradeContext {
            safety_score: 100,
            estimated_cost_sol: 0.1,
            current_balance_sol: 10.0,
        };
        let _ = breaker.check(&ctx); // triggers try_reset

        // After reset, peak and current should be baseline (10.0), not 0
        let peak = breaker.peak_value.load(Ordering::Relaxed) as f64 / 1e9;
        let current = breaker.current_value.load(Ordering::Relaxed) as f64 / 1e9;
        assert!((peak - 10.0).abs() < 0.001);
        assert!((current - 10.0).abs() < 0.001);
    }
}
