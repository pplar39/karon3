pub mod intent_pipeline;
pub mod jito;
pub mod omega_trinity; // OMEGA TRINITY 3-Rule Architecture
pub mod paper_trader;
pub mod position;
pub mod pump_fun_swap;
pub mod runtime_gate; // Re-enabled for OMEGA Overmind

pub mod blockhash_cache;
pub mod hot_blockhash;
pub mod live_trader;
pub mod overmind_edge;
pub mod overmind_master;
pub mod overmind_protocol;
pub mod safety;
pub mod siege_engine; // Sidecar (renamed from siege_trader)

pub use blockhash_cache::BlockhashCache;
pub use jito::{
    record_preflight_fail, validate_bundle_tip_lock, BundleStatus, CompetitionLevel, JitoSender,
    MultiRegionJitoSender, ProfitBasedTipCalculator, TipUrgency,
};
pub use live_trader::{load_keypair_from_env, load_keypair_from_file, LiveTrader};
pub use paper_trader::{PaperTrader, TradingStats};
pub use position::PositionManager;
pub use runtime_gate::{
    is_trade_enabled, latency_snapshot as runtime_latency_snapshot, record_backoff_latency,
    record_detection_latency, record_execution_latency, record_intent_drop,
    record_stale_intent_drop, runtime_prometheus_metrics, set_gate_mode, set_gate_reason_code,
    set_latest_error_rates, set_trade_enabled,
};
pub use safety::SafetyGuardrails;
pub use siege_engine::SiegeTrader; // Re-export for main.rs scope

pub enum TradingMode {
    Paper(PaperTrader),
    Live(LiveTrader),
}
