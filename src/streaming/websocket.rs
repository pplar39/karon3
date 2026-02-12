use crate::streaming::parsing_worker::{spawn_parser_workers, ParseJob};
use crate::time::monotonic_now_us;
use crate::types::StreamEvent;
use anyhow::{Context, Result};
use chrono::Utc;
use crossbeam_channel as cbc;
use solana_client::pubsub_client::PubsubClient;
use solana_client::rpc_config::{RpcTransactionLogsConfig, RpcTransactionLogsFilter};
use solana_sdk::commitment_config::CommitmentConfig;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Parse channel capacity — bounded, drop-newest on overflow.
const PARSE_CHANNEL_CAPACITY: usize = 4096;

/// If no WS frame for this many seconds, break inner loop and reconnect.
/// Pump.fun only mode: token creation interval can be sparse,
/// so 120s prevents false stall detection.
const STALL_THRESHOLD_SECS: u64 = 120;

/// Polling interval for shutdown check + stall detection inside recv_timeout.
const RECV_POLL_SECS: u64 = 5;

/// Maximum backoff between reconnection attempts.
const MAX_RECONNECT_BACKOFF_SECS: u64 = 30;

/// Number of RPC parser worker threads.
const PARSER_WORKER_COUNT: usize = 2;

pub struct WebSocketMonitor {
    ws_url: String,
    rpc_url: String,
    event_tx: mpsc::Sender<StreamEvent>,
    shutdown: Arc<AtomicBool>,
    reconnect_count: Arc<AtomicU64>,
}

impl WebSocketMonitor {
    pub fn new(ws_url: String, rpc_url: String, event_tx: mpsc::Sender<StreamEvent>) -> Self {
        Self {
            ws_url,
            rpc_url,
            event_tx,
            shutdown: Arc::new(AtomicBool::new(false)),
            reconnect_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        info!("WebSocket monitor shutdown signaled");
    }

    pub fn reconnect_count(&self) -> u64 {
        self.reconnect_count.load(Ordering::Relaxed)
    }

    /// Outer reconnect loop.  Never returns unless shutdown is signaled.
    ///
    /// On stall / EOF → reconnect immediately (backoff reset to 1s).
    /// On connect error → exponential backoff 1s → 30s.
    pub async fn start(&self) -> Result<()> {
        let mut backoff_secs: u64 = 1;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                info!("WS monitor: shutdown requested before connect, exiting");
                return Ok(());
            }

            info!("Connecting to WebSocket: {}", self.ws_url);

            let ws_url = self.ws_url.clone();
            let rpc_url = self.rpc_url.clone();
            let event_tx = self.event_tx.clone();
            let shutdown = self.shutdown.clone();

            let result = tokio::task::spawn_blocking(move || {
                Self::run_subscription(ws_url, rpc_url, event_tx, shutdown)
            })
            .await
            .context("WebSocket task panicked")?;

            // If user-requested shutdown, exit cleanly
            if self.shutdown.load(Ordering::SeqCst) {
                info!("WS monitor: clean shutdown after subscription exit");
                return Ok(());
            }

            // Determine reconnect behavior
            match &result {
                Ok(()) => {
                    // Connected OK, ran for a while, exited due to stall/EOF.
                    // Reset backoff — this is a recoverable situation.
                    backoff_secs = 1;
                    self.reconnect_count.fetch_add(1, Ordering::Relaxed);
                    let n = self.reconnect_count.load(Ordering::Relaxed);
                    warn!(
                        "WS subscription ended (stall/EOF). Reconnect #{} in {}s...",
                        n, backoff_secs
                    );
                }
                Err(e) => {
                    // Connection failed — exponential backoff
                    self.reconnect_count.fetch_add(1, Ordering::Relaxed);
                    let n = self.reconnect_count.load(Ordering::Relaxed);
                    error!(
                        "WS connection error: {}. Reconnect #{} in {}s...",
                        e, n, backoff_secs
                    );
                    // Increase backoff only on connect errors
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = std::cmp::min(backoff_secs * 2, MAX_RECONNECT_BACKOFF_SECS);
                    continue;
                }
            }

            // Short sleep before reconnect on stall/EOF (non-error case)
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Single subscription lifecycle.
    ///
    /// Returns `Ok(())` on stall/EOF (caller should reconnect).
    /// Returns `Err(...)` on connect/subscribe failure.
    fn run_subscription(
        ws_url: String,
        rpc_url: String,
        event_tx: mpsc::Sender<StreamEvent>,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        use crate::constants::PUMP_FUN_PROGRAM;

        // NOTE: Solana logsSubscribe only supports 1 address in Mentions filter.
        // Detection plane is Pump.fun only.
        let filter = RpcTransactionLogsFilter::Mentions(vec![PUMP_FUN_PROGRAM.to_string()]);
        let config = RpcTransactionLogsConfig {
            commitment: Some(CommitmentConfig::processed()),
        };

        let (mut subscription, receiver) = PubsubClient::logs_subscribe(&ws_url, filter, config)
            .context("Failed to subscribe to logs")?;

        info!("Subscribed to Program logs (Pump.fun only)");

        // ═══════════════════════════════════════════════════════
        // Bounded MPMC parse channel (crossbeam).
        // Drop-newest policy: if queue is full, newest job is
        // dropped — the read loop NEVER blocks.
        // ═══════════════════════════════════════════════════════
        let (parse_tx, parse_rx) = cbc::bounded::<ParseJob>(PARSE_CHANNEL_CAPACITY);

        // Spawn N parser workers on dedicated threads.
        let worker_handles = spawn_parser_workers(
            PARSER_WORKER_COUNT,
            rpc_url,
            parse_rx,
            event_tx,
            shutdown.clone(),
        );

        // ═══════════════════════════════════════════════════════
        // HOT PATH: WebSocket read loop with integrated stall detection.
        //
        // Uses recv_timeout(5s) instead of blocking for-loop so we can:
        //   1. Check shutdown flag every 5s (responsive to ctrl-c)
        //   2. Detect stalls (no frame for 120s) without a separate thread
        //   3. Handle EOF (Disconnected) for clean reconnection
        //
        // ZERO RPC calls in this loop. Only:
        //   1. Pattern match on log strings (~200ns)
        //   2. Extract signature + slot (string copy)
        //   3. Non-blocking enqueue to parse channel
        // ═══════════════════════════════════════════════════════
        let recv_timeout = Duration::from_secs(RECV_POLL_SECS);
        let stall_threshold = Duration::from_secs(STALL_THRESHOLD_SECS);
        let mut last_frame = Instant::now();

        loop {
            // Check shutdown before blocking on recv
            if shutdown.load(Ordering::SeqCst) {
                info!("Shutdown signal received, closing WebSocket");
                break;
            }

            match receiver.recv_timeout(recv_timeout) {
                Ok(response) => {
                    // Update stall tracker
                    last_frame = Instant::now();

                    let recv_time = Utc::now();
                    let ws_receive_us = monotonic_now_us();

                    let logs = &response.value.logs;
                    let signature_str = &response.value.signature;
                    let slot = response.context.slot;

                    // ── Pump.fun: pattern match ONLY ──
                    let is_pump_create = logs
                        .iter()
                        .any(|log: &String| log.contains("Program log: Instruction: Create"));

                    if is_pump_create {
                        info!(
                            "New Pump.fun token detected! Signature: {}, Slot: {}",
                            signature_str, slot
                        );
                        let job = ParseJob::PumpFun {
                            signature: signature_str.clone(),
                            slot,
                            recv_time,
                            ws_receive_us,
                        };
                        if let Err(cbc::TrySendError::Full(_)) = parse_tx.try_send(job) {
                            warn!("Parse channel full — dropping Pump.fun job (drop-newest)");
                        }
                    }
                }
                Err(cbc::RecvTimeoutError::Timeout) => {
                    // No message within 5s — check for stall
                    if last_frame.elapsed() >= stall_threshold {
                        error!(
                            "WS_STALL_DETECTED: no frame for {}s — breaking for reconnect",
                            last_frame.elapsed().as_secs()
                        );
                        break;
                    }
                    // Otherwise just loop back and check shutdown again
                    continue;
                }
                Err(cbc::RecvTimeoutError::Disconnected) => {
                    warn!("WS receiver disconnected (EOF) — breaking for reconnect");
                    break;
                }
            }
        }

        // ── Cleanup: close subscription, drain parser workers ──
        drop(parse_tx); // Signals parser workers to exit
        if let Err(e) = subscription.shutdown() {
            warn!("Error during subscription shutdown: {:?}", e);
        }
        for (i, handle) in worker_handles.into_iter().enumerate() {
            if let Err(e) = handle.join() {
                warn!("Parser worker {} join error: {:?}", i, e);
            }
        }
        info!("WebSocket subscription closed (cleanup done)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::PUMP_FUN_PROGRAM;
    use solana_client::rpc_config::RpcTransactionLogsFilter;
    use tokio::sync::mpsc;

    #[test]
    fn test_websocket_monitor_new() {
        let (tx, _rx) = mpsc::channel(100);
        let monitor = WebSocketMonitor::new(
            "wss://test.solana.com".to_string(),
            "https://api.mainnet-beta.solana.com".to_string(),
            tx,
        );

        assert_eq!(monitor.ws_url, "wss://test.solana.com");
        assert!(!monitor.shutdown.load(Ordering::SeqCst));
        assert_eq!(monitor.reconnect_count(), 0);
    }

    #[test]
    fn test_shutdown_signal() {
        let (tx, _rx) = mpsc::channel(100);
        let monitor = WebSocketMonitor::new(
            "wss://test.solana.com".to_string(),
            "https://api.mainnet-beta.solana.com".to_string(),
            tx,
        );

        assert!(!monitor.shutdown.load(Ordering::SeqCst));
        monitor.shutdown();
        assert!(monitor.shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn stall_threshold_is_120_seconds() {
        assert_eq!(STALL_THRESHOLD_SECS, 120);
    }

    #[test]
    fn max_reconnect_backoff_is_30s() {
        assert_eq!(MAX_RECONNECT_BACKOFF_SECS, 30);
    }

    #[test]
    fn recv_poll_interval_is_5s() {
        assert_eq!(RECV_POLL_SECS, 5);
    }

    #[test]
    fn pump_fun_filter_single_address() {
        let filter = RpcTransactionLogsFilter::Mentions(vec![PUMP_FUN_PROGRAM.to_string()]);
        match filter {
            RpcTransactionLogsFilter::Mentions(addrs) => assert_eq!(addrs.len(), 1),
            _ => panic!("expected Mentions filter"),
        }
    }

    #[test]
    fn parse_channel_capacity_positive() {
        assert!(PARSE_CHANNEL_CAPACITY > 0);
    }

    #[test]
    fn reconnect_count_increments() {
        let (tx, _rx) = mpsc::channel(100);
        let monitor = WebSocketMonitor::new(
            "wss://test.solana.com".to_string(),
            "https://api.mainnet-beta.solana.com".to_string(),
            tx,
        );
        assert_eq!(monitor.reconnect_count(), 0);
        monitor.reconnect_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(monitor.reconnect_count(), 1);
        monitor.reconnect_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(monitor.reconnect_count(), 2);
    }

    #[test]
    fn shutdown_prevents_reconnect() {
        // Verifies that the shutdown flag is checked before each connect attempt.
        // If shutdown is set, start() should return Ok(()) immediately.
        let (tx, _rx) = mpsc::channel(100);
        let monitor = WebSocketMonitor::new(
            "wss://test.solana.com".to_string(),
            "https://api.mainnet-beta.solana.com".to_string(),
            tx,
        );
        monitor.shutdown();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(monitor.start());
        assert!(result.is_ok());
        // No reconnect attempts were made
        assert_eq!(monitor.reconnect_count(), 0);
    }
}
