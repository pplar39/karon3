//! RPC failover pool with health monitoring.
//!
//! Provides automatic failover between multiple RPC endpoints based on
//! health checks, latency, and priority.

use parking_lot::RwLock;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

/// Default health check interval.
const DEFAULT_HEALTH_CHECK_INTERVAL_SECS: u64 = 10;

/// Default latency threshold for failover (ms).
const DEFAULT_FAILOVER_THRESHOLD_MS: u64 = 500;

/// Number of consecutive errors before marking unhealthy.
const ERROR_THRESHOLD: u32 = 3;

/// Exponential moving average weight for latency updates.
const LATENCY_EMA_WEIGHT: f64 = 0.3;

/// A single RPC endpoint with health metrics.
pub struct RpcEndpoint {
    /// The RPC URL.
    pub url: String,
    /// Priority level (lower = higher priority).
    pub priority: u8,
    /// The underlying RPC client.
    client: Arc<RpcClient>,
    /// Exponential moving average of latency in milliseconds.
    avg_latency_ms: RwLock<f64>,
    /// Consecutive error count.
    error_count: AtomicU32,
    /// Last successful call timestamp.
    last_success: RwLock<Instant>,
    /// Whether endpoint is healthy.
    healthy: AtomicBool,
}

impl RpcEndpoint {
    /// Create a new endpoint with the given URL and priority.
    pub fn new(url: String, priority: u8) -> Self {
        let client = Arc::new(RpcClient::new_with_commitment(
            url.clone(),
            CommitmentConfig::confirmed(),
        ));
        Self {
            url,
            priority,
            client,
            avg_latency_ms: RwLock::new(0.0),
            error_count: AtomicU32::new(0),
            last_success: RwLock::new(Instant::now()),
            healthy: AtomicBool::new(true),
        }
    }

    /// Get the average latency in milliseconds.
    pub fn avg_latency(&self) -> f64 {
        *self.avg_latency_ms.read()
    }

    /// Check if endpoint is currently healthy.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Get current error count.
    pub fn errors(&self) -> u32 {
        self.error_count.load(Ordering::Relaxed)
    }

    /// Update average latency using EMA.
    fn update_latency(&self, latency_ms: f64) {
        let mut avg = self.avg_latency_ms.write();
        if *avg == 0.0 {
            *avg = latency_ms;
        } else {
            *avg = LATENCY_EMA_WEIGHT * latency_ms + (1.0 - LATENCY_EMA_WEIGHT) * *avg;
        }
    }

    /// Record successful call with latency.
    fn record_success(&self, latency_ms: f64) {
        self.update_latency(latency_ms);
        self.error_count.store(0, Ordering::Relaxed);
        self.healthy.store(true, Ordering::Relaxed);
        *self.last_success.write() = Instant::now();
    }

    /// Record failed call.
    fn record_failure(&self) {
        let count = self.error_count.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= ERROR_THRESHOLD {
            self.healthy.store(false, Ordering::Relaxed);
        }
    }
}

/// RPC connection pool with automatic failover.
pub struct RpcPool {
    /// All configured endpoints.
    endpoints: Vec<RpcEndpoint>,
    /// Index of current active endpoint.
    current: AtomicUsize,
    /// Health check interval.
    health_check_interval: Duration,
    /// Latency threshold for failover (ms).
    failover_threshold_ms: u64,
    /// Notify when shutdown is requested.
    shutdown: Notify,
}

impl RpcPool {
    /// Create a new pool from a list of (url, priority) pairs.
    ///
    /// URLs with lower priority values are preferred.
    pub fn new(urls: Vec<(String, u8)>) -> Self {
        Self::with_config(urls, None, None)
    }

    /// Create a new pool with custom configuration.
    pub fn with_config(
        urls: Vec<(String, u8)>,
        health_check_interval_secs: Option<u64>,
        failover_threshold_ms: Option<u64>,
    ) -> Self {
        let mut endpoints: Vec<RpcEndpoint> = urls
            .into_iter()
            .map(|(url, priority)| RpcEndpoint::new(url, priority))
            .collect();

        // Sort by priority (lower = higher priority)
        endpoints.sort_by_key(|e| e.priority);

        let interval = Duration::from_secs(
            health_check_interval_secs.unwrap_or(DEFAULT_HEALTH_CHECK_INTERVAL_SECS),
        );

        Self {
            endpoints,
            current: AtomicUsize::new(0),
            health_check_interval: interval,
            failover_threshold_ms: failover_threshold_ms.unwrap_or(DEFAULT_FAILOVER_THRESHOLD_MS),
            shutdown: Notify::new(),
        }
    }

    /// Get the current healthy client.
    ///
    /// Returns the highest priority healthy endpoint. Falls back to
    /// the first endpoint if none are healthy.
    pub fn get_client(&self) -> Arc<RpcClient> {
        let idx = self.current.load(Ordering::Relaxed);
        if idx < self.endpoints.len() && self.endpoints[idx].is_healthy() {
            return Arc::clone(&self.endpoints[idx].client);
        }

        // Find first healthy endpoint
        for (i, ep) in self.endpoints.iter().enumerate() {
            if ep.is_healthy() {
                self.current.store(i, Ordering::Relaxed);
                return Arc::clone(&ep.client);
            }
        }

        // Fallback: return first endpoint even if unhealthy
        Arc::clone(&self.endpoints[0].client)
    }

    /// Get the current endpoint URL.
    pub fn current_url(&self) -> &str {
        let idx = self.current.load(Ordering::Relaxed);
        &self.endpoints[idx].url
    }

    /// Record a successful RPC call with latency.
    pub fn record_success(&self, latency_ms: f64) {
        let idx = self.current.load(Ordering::Relaxed);
        if idx < self.endpoints.len() {
            self.endpoints[idx].record_success(latency_ms);

            // Check if latency exceeds threshold
            if latency_ms > self.failover_threshold_ms as f64 {
                debug!(
                    "Latency {}ms exceeds threshold {}ms, considering failover",
                    latency_ms, self.failover_threshold_ms
                );
                self.try_failover_for_latency();
            }
        }
    }

    /// Record a failed RPC call - may trigger failover.
    pub fn record_failure(&self) {
        let idx = self.current.load(Ordering::Relaxed);
        if idx < self.endpoints.len() {
            self.endpoints[idx].record_failure();

            if self.endpoints[idx].errors() >= ERROR_THRESHOLD {
                warn!(
                    "Endpoint {} reached error threshold",
                    self.endpoints[idx].url
                );
                self.failover();
            }
        }
    }

    /// Attempt to fail over to a better endpoint due to latency.
    fn try_failover_for_latency(&self) {
        let current_idx = self.current.load(Ordering::Relaxed);
        let current_latency = self.endpoints[current_idx].avg_latency();

        // Find a healthy endpoint with lower latency
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i == current_idx || !ep.is_healthy() {
                continue;
            }

            // Switch if significantly better latency (at least 20% better)
            if ep.avg_latency() > 0.0 && ep.avg_latency() < current_latency * 0.8 {
                info!(
                    "Switching from {} ({}ms) to {} ({}ms) due to latency",
                    self.endpoints[current_idx].url,
                    current_latency,
                    ep.url,
                    ep.avg_latency()
                );
                self.current.store(i, Ordering::Relaxed);
                return;
            }
        }
    }

    /// Switch to the next healthy endpoint.
    fn failover(&self) {
        let current_idx = self.current.load(Ordering::Relaxed);

        // Try to find next healthy endpoint by priority
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i != current_idx && ep.is_healthy() {
                info!(
                    "Failing over from {} to {}",
                    self.endpoints[current_idx].url, ep.url
                );
                self.current.store(i, Ordering::Relaxed);
                return;
            }
        }

        // If no healthy endpoints, wrap around
        let next_idx = (current_idx + 1) % self.endpoints.len();
        if next_idx != current_idx {
            warn!(
                "No healthy endpoints, rotating to {} (may be unhealthy)",
                self.endpoints[next_idx].url
            );
            self.current.store(next_idx, Ordering::Relaxed);
        }
    }

    /// Start the background health monitoring task.
    ///
    /// This spawns an async task that periodically checks all endpoints.
    pub async fn start_health_monitor(self: Arc<Self>) {
        info!(
            "Starting RPC health monitor with {}s interval",
            self.health_check_interval.as_secs()
        );

        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.health_check_interval) => {
                    self.run_health_checks().await;
                }
                _ = self.shutdown.notified() => {
                    info!("Health monitor shutting down");
                    break;
                }
            }
        }
    }

    /// Stop the health monitor.
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }

    /// Run health checks on all endpoints.
    async fn run_health_checks(&self) {
        for (i, endpoint) in self.endpoints.iter().enumerate() {
            let healthy = self.health_check(endpoint).await;
            debug!(
                "Health check for {} [{}]: healthy={}, latency={:.1}ms, errors={}",
                endpoint.url,
                i,
                healthy,
                endpoint.avg_latency(),
                endpoint.errors()
            );
        }

        // After health checks, try to switch to better endpoint
        self.maybe_switch_to_better_endpoint();
    }

    /// Check health of a single endpoint.
    async fn health_check(&self, endpoint: &RpcEndpoint) -> bool {
        let client = Arc::clone(&endpoint.client);
        let start = Instant::now();

        let result = tokio::task::spawn_blocking(move || client.get_health()).await;

        match result {
            Ok(Ok(_)) => {
                let latency = start.elapsed().as_millis() as f64;
                endpoint.record_success(latency);
                true
            }
            Ok(Err(e)) => {
                debug!("Health check failed for {}: {}", endpoint.url, e);
                endpoint.record_failure();
                false
            }
            Err(e) => {
                debug!("Health check task failed for {}: {}", endpoint.url, e);
                endpoint.record_failure();
                false
            }
        }
    }

    /// Try to switch to a better endpoint based on priority and health.
    fn maybe_switch_to_better_endpoint(&self) {
        let current_idx = self.current.load(Ordering::Relaxed);
        let current_priority = self.endpoints[current_idx].priority;

        // Find a healthy endpoint with higher priority (lower number)
        for (i, ep) in self.endpoints.iter().enumerate() {
            if i == current_idx {
                continue;
            }

            if ep.is_healthy() && ep.priority < current_priority {
                info!(
                    "Switching to higher priority endpoint {} (priority {})",
                    ep.url, ep.priority
                );
                self.current.store(i, Ordering::Relaxed);
                return;
            }
        }
    }

    /// Get the number of healthy endpoints.
    pub fn healthy_count(&self) -> usize {
        self.endpoints.iter().filter(|e| e.is_healthy()).count()
    }

    /// Get the total number of endpoints.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    /// Get status information about all endpoints.
    pub fn status(&self) -> Vec<EndpointStatus> {
        let current_idx = self.current.load(Ordering::Relaxed);
        self.endpoints
            .iter()
            .enumerate()
            .map(|(i, ep)| EndpointStatus {
                url: ep.url.clone(),
                priority: ep.priority,
                healthy: ep.is_healthy(),
                avg_latency_ms: ep.avg_latency(),
                error_count: ep.errors(),
                is_active: i == current_idx,
            })
            .collect()
    }
}

/// Status information for a single endpoint.
#[derive(Debug, Clone)]
pub struct EndpointStatus {
    pub url: String,
    pub priority: u8,
    pub healthy: bool,
    pub avg_latency_ms: f64,
    pub error_count: u32,
    pub is_active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_creation() {
        let pool = RpcPool::new(vec![
            ("https://api.mainnet-beta.solana.com".to_string(), 1),
            ("https://rpc.ankr.com/solana".to_string(), 2),
        ]);

        assert_eq!(pool.endpoint_count(), 2);
        assert_eq!(pool.healthy_count(), 2);
    }

    #[test]
    fn test_priority_ordering() {
        let pool = RpcPool::new(vec![
            ("https://low-priority.com".to_string(), 10),
            ("https://high-priority.com".to_string(), 1),
            ("https://mid-priority.com".to_string(), 5),
        ]);

        // First endpoint should be highest priority (lowest number)
        assert!(pool.endpoints[0].url.contains("high-priority"));
        assert!(pool.endpoints[1].url.contains("mid-priority"));
        assert!(pool.endpoints[2].url.contains("low-priority"));
    }

    #[test]
    fn test_endpoint_health_state() {
        let endpoint = RpcEndpoint::new("https://test.com".to_string(), 1);

        assert!(endpoint.is_healthy());
        assert_eq!(endpoint.errors(), 0);

        // Record failures up to threshold
        endpoint.record_failure();
        assert!(endpoint.is_healthy());
        endpoint.record_failure();
        assert!(endpoint.is_healthy());
        endpoint.record_failure(); // 3rd failure = threshold
        assert!(!endpoint.is_healthy());
        assert_eq!(endpoint.errors(), 3);

        // Success resets
        endpoint.record_success(50.0);
        assert!(endpoint.is_healthy());
        assert_eq!(endpoint.errors(), 0);
    }

    #[test]
    fn test_latency_ema() {
        let endpoint = RpcEndpoint::new("https://test.com".to_string(), 1);

        endpoint.record_success(100.0);
        assert_eq!(endpoint.avg_latency(), 100.0);

        endpoint.record_success(50.0);
        // EMA: 0.3 * 50 + 0.7 * 100 = 15 + 70 = 85
        assert!((endpoint.avg_latency() - 85.0).abs() < 0.01);
    }

    #[test]
    fn test_failover_on_errors() {
        let pool = RpcPool::new(vec![
            ("https://primary.com".to_string(), 1),
            ("https://backup.com".to_string(), 2),
        ]);

        assert_eq!(pool.current.load(Ordering::Relaxed), 0);

        // Trigger failover with 3 errors
        pool.record_failure();
        pool.record_failure();
        pool.record_failure();

        assert_eq!(pool.current.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_get_client_fallback() {
        let pool = RpcPool::new(vec![
            ("https://primary.com".to_string(), 1),
            ("https://backup.com".to_string(), 2),
        ]);

        // Mark first endpoint unhealthy
        pool.endpoints[0].healthy.store(false, Ordering::Relaxed);

        // get_client should return backup
        let _client = pool.get_client();
        // Internally switches to index 1
        assert_eq!(pool.current.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_status_reporting() {
        let pool = RpcPool::new(vec![
            ("https://primary.com".to_string(), 1),
            ("https://backup.com".to_string(), 2),
        ]);

        let status = pool.status();
        assert_eq!(status.len(), 2);
        assert!(status[0].is_active);
        assert!(!status[1].is_active);
        assert!(status[0].healthy);
        assert!(status[1].healthy);
    }

    #[test]
    fn test_custom_config() {
        let pool = RpcPool::with_config(
            vec![("https://test.com".to_string(), 1)],
            Some(30),
            Some(1000),
        );

        assert_eq!(pool.health_check_interval, Duration::from_secs(30));
        assert_eq!(pool.failover_threshold_ms, 1000);
    }
}
