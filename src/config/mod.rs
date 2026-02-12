//! KARON3 Config Module
pub mod runtime;
pub mod settings;

// Re-export settings for backwards compatibility
pub use settings::{Config, TradingConfig, SafetyConfig, RpcConfig, LoggingConfig, DashboardConfig, CircuitBreakerConfigToml};
