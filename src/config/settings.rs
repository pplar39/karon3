use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub rpc: RpcConfig,
    pub trading: TradingConfig,
    pub safety: SafetyConfig,
    pub logging: LoggingConfig,
    pub dashboard: DashboardConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
    pub grpc_url: Option<String>,
    pub grpc_api_key: Option<String>,
    pub backup_rpc_urls: Option<Vec<String>>,
    pub failover_threshold_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    pub mode: String,
    pub buy_amount_sol: f64,
    pub take_profit_percent: f64,
    pub stop_loss_percent: f64,
    pub max_position_age_secs: u64,
    pub slippage_bps: Option<u64>,
    pub jito_url: Option<String>,
    pub jito_auth_keypair: Option<String>,
    pub compute_unit_price: Option<u64>,
    pub compute_unit_limit: Option<u32>,
    pub tip_amount: Option<u64>,
    pub preflight_check: Option<bool>,
    pub dynamic_cu: Option<bool>,
    pub persistent_client: Option<bool>,
    pub use_bundles: Option<bool>,
    pub jito_dont_front: Option<bool>,
    pub blockhash_cache_ms: Option<u64>,
    pub parallel_signing: Option<bool>,
    pub skip_rug_check: Option<bool>,
    pub precreate_ata: Option<bool>,
    pub swap_cu_order: Option<bool>,

    /// Enable Pump.fun token trading (default: true)
    pub pump_fun_enabled: Option<bool>,

    /// Buy amount for Pump.fun tokens (defaults to buy_amount_sol if not set)
    pub pump_fun_buy_amount_sol: Option<f64>,

    /// Slippage for Pump.fun trades in bps (defaults to slippage_bps if not set)  
    pub pump_fun_slippage_bps: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SafetyConfig {
    pub min_lp_burn_percent: f64,
    pub max_top10_holder_percent: f64,
    pub require_mint_revoked: bool,
    pub require_freeze_revoked: bool,
    pub min_liquidity_sol: f64,
    pub parallel_rug_check: Option<bool>,
    pub blacklist_file: Option<String>,
    pub auto_blacklist_on_rug: Option<bool>,
    pub circuit_breaker: Option<CircuitBreakerConfigToml>,
    pub rpc_rate_limit: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfigToml {
    pub max_consecutive_losses: Option<u32>,
    pub max_drawdown_percent: Option<f64>,
    pub window_secs: Option<u64>,
    pub cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub trades_file: String,
    pub metrics_file: String,
    pub log_level: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
