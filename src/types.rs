use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

/// Represents a newly detected liquidity pool
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    pub amm_id: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub open_orders: Pubkey,
    pub target_orders: Pubkey,
    pub market_id: Pubkey,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub detected_at: DateTime<Utc>,
    pub slot: u64,
}

/// Represents a newly detected Pump.fun token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PumpFunToken {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub user: Pubkey,
    pub initial_buy_amount_lamports: u64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub detected_at: DateTime<Utc>,
    pub slot: u64,
    pub signature: Option<String>,
    /// Decoded from create_v2 instruction data (borsh)
    pub name: Option<String>,
    pub symbol: Option<String>,
    pub uri: Option<String>,
    /// Creator SOL spend in lamports (pre_balance - post_balance)
    pub creator_spend_lamports: Option<u64>,
    /// Transaction fee payer (first account key)
    pub fee_payer: Option<Pubkey>,
}

impl PumpFunToken {
    /// Derive bonding curve PDA and its associated token account from mint.
    /// seeds = ["bonding-curve", mint] under the Pump.fun program.
    pub fn derive_bonding_curve(mint: &Pubkey, pump_program: &Pubkey) -> (Pubkey, Pubkey) {
        let (bonding_curve, _bump) =
            Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], pump_program);
        // Pump.fun tokens use Token-2022
        let associated =
            spl_associated_token_account::get_associated_token_address_with_program_id(
                &bonding_curve,
                mint,
                &spl_token_2022::id(),
            );
        (bonding_curve, associated)
    }
}

/// Safety check results for a token
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SafetyCheck {
    pub mint_authority_revoked: bool,
    pub freeze_authority_revoked: bool,
    pub lp_burned_percent: f64,
    pub lp_locked_percent: f64,
    pub top10_holder_percent: f64,
    pub liquidity_sol: f64,
    pub is_safe: bool,
    pub risk_score: u32,
    pub warnings: Vec<String>,
}

/// A trading position (paper or real)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub id: String,
    pub pool: Pool,
    pub entry_price_sol: f64,
    pub entry_amount_sol: f64,
    pub entry_time: DateTime<Utc>,
    pub current_price_sol: f64,
    pub unrealized_pnl_percent: f64,
    pub status: PositionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PositionStatus {
    Open,
    ClosedTakeProfit,
    ClosedStopLoss,
    ClosedManual,
    ClosedTimeout,
}

/// Result of a simulated or real trade
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeResult {
    pub position_id: String,
    pub action: TradeAction,
    pub price_sol: f64,
    pub amount_sol: f64,
    pub pnl_percent: f64,
    pub pnl_sol: f64,
    pub timestamp: DateTime<Utc>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TradeAction {
    Buy,
    Sell,
}

/// Event from the streaming layer
#[derive(Debug, Clone)]
pub enum StreamEvent {
    NewPool(Pool, LatencyEvent),
    NewPumpFunToken(PumpFunToken, LatencyEvent),
    PriceUpdate {
        pool_id: Pubkey,
        price_sol: f64,
    },
    TradeExecuted(TradeResult),
    TradeFailed {
        reason: String,
        pool_id: Option<Pubkey>,
    },
    Error(String),
}

/// Latency metrics for pool detection and trading
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LatencyEvent {
    pub event_id: String,
    pub ws_recv_ts_us: u64,
    pub parse_done_ts_us: u64,
    pub intent_enqueued_ts_us: u64,
    pub exec_start_ts_us: u64,
    pub exec_end_ts_us: u64,
    pub ws_receive_us: u64,
    pub pool_parsed_us: u64,
    pub channel_receive_us: u64,
    pub rugcheck_start_us: u64,
    pub rugcheck_end_us: u64,
    pub tx_build_start_us: u64,
    pub tx_signed_us: u64,
    pub jito_send_start_us: u64,
    pub jito_send_end_us: u64,
    pub rpc_send_start_us: u64,
    pub rpc_send_end_us: u64,
    pub backoff_sleep_start_ts_us: u64,
    pub backoff_sleep_end_ts_us: u64,
    pub detection_latency_us: u64,
    pub execution_latency_us: u64,
    pub backoff_latency_us: u64,
    pub confirmation_us: Option<u64>,
    pub slot_drift: Option<i64>,
}

/// Metrics for analysis
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub total_pnl_sol: f64,
    pub best_trade_pnl: f64,
    pub worst_trade_pnl: f64,
    pub avg_hold_time_secs: f64,
    pub pools_detected: u64,
    pub pools_passed_safety: u64,
    pub wallet_balance_sol: f64,
}
