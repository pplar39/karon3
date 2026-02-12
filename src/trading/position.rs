use crate::config::TradingConfig;
use crate::types::{Pool, Position, PositionStatus, TradeAction, TradeResult};
use chrono::Utc;
use std::collections::HashMap;
use tracing::info;
use uuid::Uuid;

/// Manages trading positions with automatic exit condition checking
pub struct PositionManager {
    positions: HashMap<String, Position>,
    config: TradingConfig,
}

impl PositionManager {
    /// Create a new position manager with the given trading configuration
    pub fn new(config: TradingConfig) -> Self {
        Self {
            positions: HashMap::new(),
            config,
        }
    }

    /// Open a new position for the given pool at the specified entry price
    pub fn open_position(&mut self, pool: Pool, entry_price: f64) -> Position {
        let id = Uuid::new_v4().to_string();
        let position = Position {
            id: id.clone(),
            pool,
            entry_price_sol: entry_price,
            entry_amount_sol: self.config.buy_amount_sol,
            entry_time: Utc::now(),
            current_price_sol: entry_price,
            unrealized_pnl_percent: 0.0,
            status: PositionStatus::Open,
        };

        info!("📈 Opened position {} at {} SOL", id, entry_price);
        self.positions.insert(id, position.clone());
        position
    }

    /// Update the current price for a position and recalculate PnL
    pub fn update_price(&mut self, position_id: &str, new_price: f64) -> Option<&Position> {
        if let Some(pos) = self.positions.get_mut(position_id) {
            pos.current_price_sol = new_price;
            pos.unrealized_pnl_percent =
                ((new_price - pos.entry_price_sol) / pos.entry_price_sol) * 100.0;
        }
        self.positions.get(position_id)
    }

    /// Check if a position should be closed based on take profit, stop loss, or timeout
    pub fn check_exit_conditions(&mut self, position_id: &str) -> Option<TradeResult> {
        let pos = self.positions.get(position_id)?;

        // Take profit check
        if pos.unrealized_pnl_percent >= self.config.take_profit_percent {
            return Some(self.close_position(position_id, PositionStatus::ClosedTakeProfit));
        }

        // Stop loss check
        if pos.unrealized_pnl_percent <= -self.config.stop_loss_percent {
            return Some(self.close_position(position_id, PositionStatus::ClosedStopLoss));
        }

        // Timeout check
        let age = Utc::now().signed_duration_since(pos.entry_time);
        if age.num_seconds() > self.config.max_position_age_secs as i64 {
            return Some(self.close_position(position_id, PositionStatus::ClosedTimeout));
        }

        None
    }

    /// Close a position with the specified status and generate a trade result
    fn close_position(&mut self, position_id: &str, status: PositionStatus) -> TradeResult {
        let pos = self.positions.get_mut(position_id).unwrap();
        pos.status = status.clone();

        let pnl_percent = pos.unrealized_pnl_percent;
        let pnl_sol = pos.entry_amount_sol * (pnl_percent / 100.0);

        let reason = match status {
            PositionStatus::ClosedTakeProfit => "Take profit triggered".to_string(),
            PositionStatus::ClosedStopLoss => "Stop loss triggered".to_string(),
            PositionStatus::ClosedTimeout => "Position timed out".to_string(),
            _ => "Manual close".to_string(),
        };

        info!(
            "📉 Closed position {} with {:.2}% PnL ({})",
            position_id, pnl_percent, reason
        );

        TradeResult {
            position_id: position_id.to_string(),
            action: TradeAction::Sell,
            price_sol: pos.current_price_sol,
            amount_sol: pos.entry_amount_sol,
            pnl_percent,
            pnl_sol,
            timestamp: Utc::now(),
            reason,
        }
    }

    /// Get all currently open positions
    pub fn get_open_positions(&self) -> Vec<&Position> {
        self.positions
            .values()
            .filter(|p| p.status == PositionStatus::Open)
            .collect()
    }

    /// Get a specific position by ID
    pub fn get_position(&self, position_id: &str) -> Option<&Position> {
        self.positions.get(position_id)
    }

    /// Get total count of all positions (open and closed)
    pub fn total_positions(&self) -> usize {
        self.positions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn make_test_pool() -> Pool {
        Pool {
            amm_id: Pubkey::new_unique(),
            base_mint: Pubkey::new_unique(),
            quote_mint: Pubkey::new_unique(),
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            lp_mint: Pubkey::new_unique(),
            open_orders: Pubkey::new_unique(),
            target_orders: Pubkey::new_unique(),
            market_id: Pubkey::new_unique(),
            base_decimals: 9,
            quote_decimals: 9,
            detected_at: Utc::now(),
            slot: 12345,
        }
    }

    fn make_test_config() -> TradingConfig {
        TradingConfig {
            mode: "paper".to_string(),
            buy_amount_sol: 0.1,
            max_sol_spend: None,
            max_trades: None,
            min_balance_sol: None,
            error_rate_threshold: None,
            take_profit_percent: 50.0,
            stop_loss_percent: 20.0,
            max_position_age_secs: 300,
            slippage_bps: Some(500),
            jito_url: None,
            jito_auth_uuid: None,
            jito_auth_keypair: None,
            use_siege_engine: None,
            compute_unit_price: None,
            compute_unit_limit: None,
            tip_amount: None,
            preflight_check: None,
            dynamic_cu: None,
            persistent_client: None,
            use_bundles: None,
            jito_dont_front: None,
            precreate_ata: None,
            swap_cu_order: None,
            blockhash_cache_ms: None,
            parallel_signing: None,
            skip_rug_check: None,
            shadow_mode: None,
            max_intent_age_ms: None,
            buy_enabled: None,
        }
    }

    #[test]
    fn test_open_position() {
        let config = make_test_config();
        let mut manager = PositionManager::new(config);
        let pool = make_test_pool();

        let position = manager.open_position(pool, 0.001);

        assert_eq!(position.entry_price_sol, 0.001);
        assert_eq!(position.entry_amount_sol, 0.1);
        assert_eq!(position.status, PositionStatus::Open);
        assert_eq!(manager.get_open_positions().len(), 1);
    }

    #[test]
    fn test_update_price_calculates_pnl() {
        let config = make_test_config();
        let mut manager = PositionManager::new(config);
        let pool = make_test_pool();

        let position = manager.open_position(pool, 0.001);
        manager.update_price(&position.id, 0.0015); // 50% gain

        let updated = manager.get_position(&position.id).unwrap();
        assert!((updated.unrealized_pnl_percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn test_take_profit_exit() {
        let config = make_test_config();
        let mut manager = PositionManager::new(config);
        let pool = make_test_pool();

        let position = manager.open_position(pool, 0.001);
        manager.update_price(&position.id, 0.0015); // 50% gain triggers TP

        let result = manager.check_exit_conditions(&position.id);
        assert!(result.is_some());

        let trade = result.unwrap();
        assert!(matches!(trade.action, TradeAction::Sell));
        assert!(trade.reason.contains("Take profit"));
    }

    #[test]
    fn test_stop_loss_exit() {
        let config = make_test_config();
        let mut manager = PositionManager::new(config);
        let pool = make_test_pool();

        let position = manager.open_position(pool, 0.001);
        manager.update_price(&position.id, 0.0008); // 20% loss triggers SL

        let result = manager.check_exit_conditions(&position.id);
        assert!(result.is_some());

        let trade = result.unwrap();
        assert!(trade.reason.contains("Stop loss"));
    }
}
