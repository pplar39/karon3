use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::time::Instant;

use parking_lot::RwLock;
use tracing::error;

use crate::config::CircuitBreakerConfigToml;

const STATE_CLOSED: u8 = 0;
const STATE_OPEN: u8 = 1;
const STATE_HALF_OPEN: u8 = 2;

#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub max_consecutive_losses: u32,
    pub max_drawdown_percent: f64,
    pub window_secs: u64,
    pub cooldown_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_consecutive_losses: 5,
            max_drawdown_percent: 10.0,
            window_secs: 300,
            cooldown_secs: 300,
        }
    }
}

impl CircuitBreakerConfig {
    pub fn from_toml(toml: &CircuitBreakerConfigToml) -> Self {
        let d = Self::default();
        Self {
            max_consecutive_losses: toml
                .max_consecutive_losses
                .unwrap_or(d.max_consecutive_losses),
            max_drawdown_percent: toml.max_drawdown_percent.unwrap_or(d.max_drawdown_percent),
            window_secs: toml.window_secs.unwrap_or(d.window_secs),
            cooldown_secs: toml.cooldown_secs.unwrap_or(d.cooldown_secs),
        }
    }
}

struct TradeRecord {
    pnl_percent: f64,
    timestamp: Instant,
}

pub struct CircuitBreaker {
    state: AtomicU8,
    config: CircuitBreakerConfig,
    recent_trades: RwLock<VecDeque<TradeRecord>>,
    last_triggered: RwLock<Option<Instant>>,
    consecutive_losses: AtomicU32,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            state: AtomicU8::new(STATE_CLOSED),
            config,
            recent_trades: RwLock::new(VecDeque::new()),
            last_triggered: RwLock::new(None),
            consecutive_losses: AtomicU32::new(0),
        }
    }

    pub fn record_trade(&self, pnl_percent: f64) {
        let now = Instant::now();

        {
            let mut trades = self.recent_trades.write();
            trades.push_back(TradeRecord {
                pnl_percent,
                timestamp: now,
            });

            let cutoff = now
                .checked_sub(std::time::Duration::from_secs(self.config.window_secs))
                .unwrap_or(now);
            while let Some(front) = trades.front() {
                if front.timestamp < cutoff {
                    trades.pop_front();
                } else {
                    break;
                }
            }
        }

        if pnl_percent < 0.0 {
            self.consecutive_losses.fetch_add(1, Ordering::SeqCst);
        } else {
            self.consecutive_losses.store(0, Ordering::SeqCst);
        }

        self.check_and_trip();
    }

    pub fn is_triggered(&self) -> bool {
        let state = self.state.load(Ordering::SeqCst);
        state == STATE_OPEN || state == STATE_HALF_OPEN
    }

    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::SeqCst) == STATE_OPEN
    }

    pub fn check_and_trip(&self) -> bool {
        if self.state.load(Ordering::SeqCst) == STATE_OPEN {
            return true;
        }

        let losses = self.consecutive_losses.load(Ordering::SeqCst);
        if losses >= self.config.max_consecutive_losses {
            self.trip();
            return true;
        }

        let drawdown = self.calculate_drawdown();
        if drawdown >= self.config.max_drawdown_percent {
            self.trip();
            return true;
        }

        false
    }

    pub fn try_reset(&self) -> bool {
        let state = self.state.load(Ordering::SeqCst);
        if state != STATE_OPEN {
            return state == STATE_CLOSED;
        }

        let last_triggered = self.last_triggered.read();
        if let Some(triggered_at) = *last_triggered {
            let elapsed = triggered_at.elapsed().as_secs();
            if elapsed >= self.config.cooldown_secs {
                drop(last_triggered);
                self.state.store(STATE_HALF_OPEN, Ordering::SeqCst);
                return true;
            }
        }

        false
    }

    pub fn confirm_recovery(&self) {
        if self
            .state
            .compare_exchange(
                STATE_HALF_OPEN,
                STATE_CLOSED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            self.consecutive_losses.store(0, Ordering::SeqCst);
            let mut trades = self.recent_trades.write();
            trades.clear();
        }
    }

    pub fn state_name(&self) -> &'static str {
        match self.state.load(Ordering::SeqCst) {
            STATE_CLOSED => "closed",
            STATE_OPEN => "open",
            STATE_HALF_OPEN => "half-open",
            _ => "unknown",
        }
    }

    fn trip(&self) {
        self.state.store(STATE_OPEN, Ordering::SeqCst);
        let mut last = self.last_triggered.write();
        *last = Some(Instant::now());
    }

    fn calculate_drawdown(&self) -> f64 {
        let trades = self.recent_trades.read();
        if trades.is_empty() {
            return 0.0;
        }

        trades
            .iter()
            .map(|t| t.pnl_percent)
            .filter(|&p| p < 0.0)
            .sum::<f64>()
            .abs()
    }
}

impl std::fmt::Debug for CircuitBreaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitBreaker")
            .field("state", &self.state_name())
            .field(
                "consecutive_losses",
                &self.consecutive_losses.load(Ordering::Relaxed),
            )
            .field("config", &self.config)
            .finish()
    }
}

#[derive(Debug)]
pub struct SafetyGuardrails {
    pub max_sol_spend: f64,
    pub max_trades: usize,
    pub min_balance_threshold: f64,
    pub error_rate_threshold: f64,

    // Atomic Safety State
    pub current_spend: AtomicU64,
    pub trade_count: AtomicUsize,
    pub error_count: AtomicUsize,
    pub total_attempts: AtomicUsize,

    // NEW: OMEGA Safety Primitives
    pub kill_switch: AtomicBool, // The "Toad Switch" (Failure-proof latch)
    pub active_positions: AtomicUsize, // Concurrent position counter
    pub total_exposure_lamports: AtomicU64, // Real-time value at risk

    pub max_daily_loss_sol: Option<f64>,
    pub current_daily_loss_lamports: AtomicU64,
    circuit_breaker: Option<CircuitBreaker>,
}

impl SafetyGuardrails {
    pub fn new(
        max_sol_spend: f64,
        max_trades: usize,
        min_balance_threshold: f64,
        error_rate_threshold: f64,
    ) -> Self {
        Self {
            max_sol_spend,
            max_trades,
            min_balance_threshold,
            error_rate_threshold,
            current_spend: AtomicU64::new(0),
            trade_count: AtomicUsize::new(0),
            error_count: AtomicUsize::new(0),
            total_attempts: AtomicUsize::new(0),

            // Initial Safety State
            kill_switch: AtomicBool::new(false),
            active_positions: AtomicUsize::new(0),
            total_exposure_lamports: AtomicU64::new(0),

            max_daily_loss_sol: None,
            current_daily_loss_lamports: AtomicU64::new(0),
            circuit_breaker: None,
        }
    }

    pub fn with_max_daily_loss(mut self, max_loss: f64) -> Self {
        self.max_daily_loss_sol = Some(max_loss);
        self
    }

    pub fn hydrate_daily_loss(&self, loss_sol: f64) {
        let lamports = (loss_sol * 1_000_000_000.0) as u64;
        self.current_daily_loss_lamports
            .store(lamports, Ordering::SeqCst);
    }

    pub fn with_circuit_breaker(mut self, breaker: CircuitBreaker) -> Self {
        self.circuit_breaker = Some(breaker);
        self
    }

    // --- KILL SWITCH LOGIC (The Toad Switch) ---

    pub fn is_killed(&self) -> bool {
        self.kill_switch.load(Ordering::SeqCst)
    }

    pub fn trip_kill_switch(&self, reason: &str) {
        // Latch to TRUE. Once set, only explicit reset (admin) can clear it.
        if !self.kill_switch.swap(true, Ordering::SeqCst) {
            error!("🚨 KILL SWITCH TRIPPED: {}", reason);
        }
    }

    // --- POSITION & EXPOSURE LOGIC ---

    pub fn can_open_position(
        &self,
        estimated_cost_sol: f64,
        current_balance_sol: f64,
    ) -> Result<(), String> {
        // 1. Kill Switch Check (Zero Latency)
        if self.is_killed() {
            return Err("⛔ KILL SWITCH ACTIVE".to_string());
        }

        // 2. Circuit Breaker Check
        if let Some(breaker) = &self.circuit_breaker {
            if breaker.is_triggered() {
                return Err(format!(
                    "⛔ CIRCUIT BREAKER ACTIVE ({})",
                    breaker.state_name()
                ));
            }
        }

        // 3. Position Limit Check (Atomic)
        // Hard limits should be config-driven, but for now we enforce logical sanity
        let current_active = self.active_positions.load(Ordering::Relaxed);
        if current_active >= self.max_trades {
            // reusing max_trades as max_concurrent for now or config
            return Err(format!(
                "Max concurrent positions reached: {}",
                current_active
            ));
        }

        // 4. Balance & Spend Checks
        let cost_lamports = (estimated_cost_sol * 1_000_000_000.0) as u64;
        let current_spend = self.current_spend.load(Ordering::Relaxed);
        let max_spend_lamports = (self.max_sol_spend * 1_000_000_000.0) as u64;

        if current_spend + cost_lamports > max_spend_lamports {
            return Err(format!(
                "Spend cap reached. Current: {}, Max: {}",
                current_spend, max_spend_lamports
            ));
        }

        if current_balance_sol < self.min_balance_threshold {
            return Err(format!(
                "Balance low: {} < threshold {}",
                current_balance_sol, self.min_balance_threshold
            ));
        }

        Ok(())
    }

    pub fn check_circuit_breaker(&self) -> Result<(), String> {
        let Some(breaker) = &self.circuit_breaker else {
            return Ok(());
        };

        if breaker.is_open() {
            // Attempt a safe transition to half-open after cooldown.
            breaker.try_reset();
        }

        if breaker.is_open() {
            return Err("Circuit breaker tripped".to_string());
        }

        Ok(())
    }

    pub fn record_pnl_percent(&self, pnl_percent: f64) {
        let Some(breaker) = &self.circuit_breaker else {
            return;
        };

        breaker.record_trade(pnl_percent);

        // If we were probing recovery (half-open) and this trade wasn't a loss,
        // confirm recovery and return to closed.
        if pnl_percent >= 0.0 {
            breaker.confirm_recovery();
        }
    }

    // Backward-compatible pre-trade guard used by legacy call sites.
    pub fn check_pre_trade(
        &self,
        estimated_cost_sol: f64,
        current_balance_sol: f64,
    ) -> Result<(), String> {
        self.can_open_position(estimated_cost_sol, current_balance_sol)
    }

    // Backward-compatible spend tracker used by legacy live_trader.
    pub fn record_spend(&self, amount_sol: f64) {
        let amount_lamports = (amount_sol * 1_000_000_000.0) as u64;
        self.current_spend
            .fetch_add(amount_lamports, Ordering::Relaxed);
    }

    // Backward-compatible trade counter used by legacy live_trader.
    pub fn record_trade(&self) {
        self.trade_count.fetch_add(1, Ordering::Relaxed);
        self.total_attempts.fetch_add(1, Ordering::Relaxed);
    }

    // --- TRACKING METHODS ---

    pub fn record_entry(&self, cost_sol: f64) {
        let lamports = (cost_sol * 1_000_000_000.0) as u64;
        self.active_positions.fetch_add(1, Ordering::SeqCst);
        self.total_exposure_lamports
            .fetch_add(lamports, Ordering::SeqCst);
        self.current_spend.fetch_add(lamports, Ordering::Relaxed);
        self.total_attempts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_exit(&self, original_cost_sol: f64) {
        let lamports = (original_cost_sol * 1_000_000_000.0) as u64;
        // Prevent underflow with saturating sub (Safety)
        if self.active_positions.load(Ordering::SeqCst) > 0 {
            self.active_positions.fetch_sub(1, Ordering::SeqCst);
        }
        if self.total_exposure_lamports.load(Ordering::SeqCst) >= lamports {
            self.total_exposure_lamports
                .fetch_sub(lamports, Ordering::SeqCst);
        } else {
            // Reset to 0 if tracking drifted
            self.total_exposure_lamports.store(0, Ordering::SeqCst);
        }

        self.trade_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        // Note: Failed entries should not increment active_positions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(max_losses: u32, max_drawdown: f64) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_consecutive_losses: max_losses,
            max_drawdown_percent: max_drawdown,
            window_secs: 300,
            cooldown_secs: 0,
        }
    }

    #[test]
    fn circuit_breaker_starts_closed() {
        //#given
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());

        //#then
        assert!(!cb.is_triggered());
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn no_trip_on_winning_trades() {
        //#given
        let cb = CircuitBreaker::new(make_config(3, 10.0));

        //#when
        cb.record_trade(5.0);
        cb.record_trade(10.0);
        cb.record_trade(2.0);

        //#then
        assert!(!cb.is_triggered());
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn trips_on_consecutive_losses() {
        //#given
        let cb = CircuitBreaker::new(make_config(3, 100.0));

        //#when
        cb.record_trade(-1.0);
        cb.record_trade(-2.0);
        assert!(!cb.is_triggered());

        cb.record_trade(-3.0);

        //#then
        assert!(cb.is_triggered());
        assert_eq!(cb.state_name(), "open");
    }

    #[test]
    fn winning_trade_resets_consecutive_losses() {
        //#given
        let cb = CircuitBreaker::new(make_config(3, 100.0));

        //#when
        cb.record_trade(-1.0);
        cb.record_trade(-2.0);
        cb.record_trade(5.0);
        cb.record_trade(-1.0);
        cb.record_trade(-2.0);

        //#then
        assert!(!cb.is_triggered());
    }

    #[test]
    fn trips_on_drawdown() {
        //#given
        let cb = CircuitBreaker::new(make_config(100, 10.0));

        //#when
        cb.record_trade(-3.0);
        cb.record_trade(-4.0);
        assert!(!cb.is_triggered());

        cb.record_trade(-4.0);

        //#then
        assert!(cb.is_triggered());
        assert_eq!(cb.state_name(), "open");
    }

    #[test]
    fn recovery_after_cooldown() {
        //#given
        let cb = CircuitBreaker::new(make_config(1, 100.0));
        cb.record_trade(-5.0);
        assert!(cb.is_triggered());

        //#when
        let reset_ok = cb.try_reset();

        //#then
        assert!(reset_ok);
        assert_eq!(cb.state_name(), "half-open");
    }

    #[test]
    fn confirm_recovery_closes_circuit() {
        //#given
        let cb = CircuitBreaker::new(make_config(1, 100.0));
        cb.record_trade(-5.0);
        cb.try_reset();
        assert_eq!(cb.state_name(), "half-open");

        //#when
        cb.confirm_recovery();

        //#then
        assert!(!cb.is_triggered());
        assert_eq!(cb.state_name(), "closed");
    }

    #[test]
    fn safety_guardrails_check_pre_trade() {
        //#given
        let sg = SafetyGuardrails::new(1.0, 10, 0.5, 0.5);

        //#when
        let result = sg.check_pre_trade(0.1, 1.0);

        //#then
        assert!(result.is_ok());
    }

    #[test]
    fn safety_guardrails_rejects_over_spend() {
        //#given
        let sg = SafetyGuardrails::new(1.0, 10, 0.5, 0.5);
        sg.record_spend(0.9);

        //#when
        let result = sg.check_pre_trade(0.2, 1.0);

        //#then
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Spend cap"));
    }

    #[test]
    fn guardrails_blocks_when_circuit_breaker_open() {
        //#given
        let mut config = make_config(1, 100.0);
        config.cooldown_secs = 300;
        let cb = CircuitBreaker::new(config);

        cb.record_trade(-1.0);
        let sg = SafetyGuardrails::new(10_000.0, 10_000, 0.0, 1.0).with_circuit_breaker(cb);

        //#then
        assert!(sg.check_circuit_breaker().is_err());
    }
}
