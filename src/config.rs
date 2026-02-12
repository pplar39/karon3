use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub rpc: RpcConfig,
    pub trading: TradingConfig,
    pub safety: SafetyConfig,
    pub logging: LoggingConfig,
    pub dashboard: DashboardConfig,
    pub notifications: Option<NotificationsConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcConfig {
    pub http_url: String,
    pub ws_url: String,
    pub grpc_url: Option<String>,
    pub grpc_api_key: Option<String>,
    pub backup_rpc_urls: Option<Vec<String>>,
    pub failover_threshold_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradingConfig {
    pub mode: String,
    pub buy_amount_sol: f64,
    pub max_sol_spend: Option<f64>,
    pub max_trades: Option<usize>,
    pub min_balance_sol: Option<f64>,
    pub error_rate_threshold: Option<f64>,
    pub take_profit_percent: f64,
    pub stop_loss_percent: f64,
    pub max_position_age_secs: u64,
    pub slippage_bps: Option<u64>,
    pub jito_url: Option<String>,
    pub jito_auth_uuid: Option<String>,
    pub jito_auth_keypair: Option<String>,
    pub use_siege_engine: Option<bool>, // Sidecar Toggle
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
    /// Shadow mode: simulate trades without sending real transactions
    /// Perfect for strategy testing with real market data
    pub shadow_mode: Option<bool>,
    /// Max intent age in milliseconds before exec-worker drops it as stale (default 5000, range 500-10000)
    pub max_intent_age_ms: Option<u64>,
    /// Kill-switch: set to false to disable all buys (sell-only mode)
    pub buy_enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    pub max_daily_loss_sol: Option<f64>,
    pub omega_trinity: Option<OmegaTrinityConfigToml>,
    pub pumpfun_filter: Option<PumpFunFilterConfigToml>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    pub enabled: Option<bool>,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreakerConfigToml {
    pub max_consecutive_losses: Option<u32>,
    pub max_drawdown_percent: Option<f64>,
    pub window_secs: Option<u64>,
    pub cooldown_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmegaTrinityConfigToml {
    pub min_safety_score: Option<u8>,
    pub budget_sol: Option<f64>,
    pub max_rps: Option<u32>,
    pub max_drawdown_percent: Option<f64>,
    pub cooldown_secs: Option<u64>,
    pub window_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub trades_file: String,
    pub metrics_file: String,
    pub log_level: String,
    /// SQLite database path for trade logging
    pub trades_db: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub port: u16,
    pub username: String,
    pub password: String,
}

// ─── Pump.fun Filter Config ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PumpFunFilterConfigToml {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub default_on_error: Option<String>,
    pub max_total_budget_ms: Option<u64>,
    pub min_score: Option<u32>,
    pub emit_reason_log: Option<bool>,
    pub reason_log_sample_rate: Option<f64>,
    pub dedup: Option<DedupFilterConfig>,
    pub creator_rate_limit: Option<CreatorRateLimitConfig>,
    pub lists: Option<ListsFilterConfig>,
    pub meta: Option<MetaFilterConfig>,
    pub uri: Option<UriFilterConfig>,
    pub creator_commitment: Option<CreatorCommitmentConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct DedupFilterConfig {
    pub max_seen: Option<usize>,
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreatorRateLimitConfig {
    pub window_secs: Option<u64>,
    pub max_creates: Option<u32>,
    pub on_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListsFilterConfig {
    pub creator_allowlist: Option<String>,
    pub creator_denylist: Option<String>,
    pub on_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetaFilterConfig {
    pub require_decode: Option<bool>,
    pub on_decode_error: Option<String>,
    pub name_len_min: Option<usize>,
    pub name_len_max: Option<usize>,
    pub symbol_len_min: Option<usize>,
    pub symbol_len_max: Option<usize>,
    pub ascii_only: Option<bool>,
    pub deny_unicode_controls: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UriFilterConfig {
    pub require_uri: Option<bool>,
    pub on_missing: Option<String>,
    pub allow_ipfs: Option<bool>,
    pub allow_arweave: Option<bool>,
    pub allow_https_hosts: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreatorCommitmentConfig {
    pub min_creator_spend_sol: Option<f64>,
    pub require_fee_payer_is_creator: Option<bool>,
    pub on_missing_meta: Option<String>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
