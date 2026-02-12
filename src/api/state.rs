use crate::config::DashboardConfig;
use crate::logging::TradeLogger;
use crate::trading::TradingMode;
use crate::types::{Metrics, Position, StreamEvent};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, RwLock};

#[derive(Clone)]
pub struct AppState {
    pub trading_mode: Arc<RwLock<TradingMode>>,
    pub trade_logger: Arc<RwLock<TradeLogger>>,
    pub event_tx: broadcast::Sender<StreamEvent>,
    pub metrics_cache: Arc<RwLock<Metrics>>,
    pub positions_cache: Arc<RwLock<Vec<Position>>>,
    pub config: DashboardConfig,
    pub start_time: Instant,
}

impl AppState {
    pub fn new(
        trading_mode: Arc<RwLock<TradingMode>>,
        trade_logger: Arc<RwLock<TradeLogger>>,
        event_tx: broadcast::Sender<StreamEvent>,
        config: DashboardConfig,
    ) -> Self {
        Self {
            trading_mode,
            trade_logger,
            event_tx,
            metrics_cache: Arc::new(RwLock::new(Metrics::default())),
            positions_cache: Arc::new(RwLock::new(Vec::new())),
            config,
            start_time: Instant::now(),
        }
    }

    pub async fn cached_metrics(&self) -> Metrics {
        self.metrics_cache.read().await.clone()
    }

    pub async fn cached_positions(&self) -> Vec<Position> {
        self.positions_cache.read().await.clone()
    }

    pub async fn set_cached_metrics(&self, metrics: Metrics) {
        *self.metrics_cache.write().await = metrics;
    }

    pub async fn set_cached_positions(&self, positions: Vec<Position>) {
        *self.positions_cache.write().await = positions;
    }

    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    pub fn uptime_formatted(&self) -> String {
        let secs = self.uptime_secs();
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        let secs = secs % 60;
        format!("{:02}:{:02}:{:02}", hours, mins, secs)
    }
}
