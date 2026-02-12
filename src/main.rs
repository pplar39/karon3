use aether_sniper::api::{create_router, AppState};
use aether_sniper::logging::TradeLogger;
use aether_sniper::streaming::WebSocketMonitor;
use aether_sniper::trading::intent_pipeline::{
    mark_detection_enqueue, now_us, try_enqueue_drop_newest, INTENT_QUEUE_CAPACITY,
};
use aether_sniper::trading::jito::{
    clear_jito_emergency_stop, jito_metrics_snapshot, trigger_jito_emergency_stop,
};
use aether_sniper::trading::runtime_gate::{
    is_trade_enabled, latency_snapshot as runtime_latency_snapshot, record_backoff_latency,
    record_detection_latency, record_execution_latency, record_intent_drop,
    record_stale_intent_drop, set_gate_mode, set_gate_reason_code, set_latest_error_rates,
    set_trade_enabled,
};
use aether_sniper::trading::{
    load_keypair_from_env, load_keypair_from_file, LiveTrader, PaperTrader, SiegeTrader,
    TradingMode,
};
use aether_sniper::{Config, LatencyEvent, Pool, PumpFunToken, StreamEvent};
use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use solana_sdk::signature::Keypair;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser, Debug)]
#[command(author, version, about = "Aether Sniper - Solana MEV Bot")]
struct Args {
    /// Send a test Telegram alert and exit
    #[arg(long)]
    test_alert: bool,
}

#[derive(Debug)]
enum TradeIntent {
    NewPool(Pool, LatencyEvent),
    NewPumpFunToken(PumpFunToken, LatencyEvent),
}

#[derive(Clone, Copy, Debug)]
struct JitoDelta {
    success_2xx: u64,
    rate_limited_429: u64,
    client_4xx_non429: u64,
    server_5xx: u64,
    other: u64,
}

struct NotificationService;

impl NotificationService {
    fn new(_bot_token: Option<String>, _chat_id: Option<i64>) -> Self {
        Self
    }

    fn is_enabled(&self) -> bool {
        false
    }

    async fn send_direct(&self, _message: &str) -> Result<()> {
        Ok(())
    }

    async fn start(&self, _rx: broadcast::Receiver<StreamEvent>) {}
}

fn milli_pct(numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    numerator
        .saturating_mul(100_000)
        .saturating_div(denominator)
}

fn compute_jito_delta(
    current: aether_sniper::trading::jito::JitoMetricsSnapshot,
    previous: aether_sniper::trading::jito::JitoMetricsSnapshot,
) -> JitoDelta {
    JitoDelta {
        success_2xx: current
            .request_2xx_total
            .saturating_sub(previous.request_2xx_total),
        rate_limited_429: current
            .request_429_total
            .saturating_sub(previous.request_429_total),
        client_4xx_non429: current
            .request_4xx_non429_total
            .saturating_sub(previous.request_4xx_non429_total),
        server_5xx: current
            .request_5xx_total
            .saturating_sub(previous.request_5xx_total),
        other: current
            .request_other_total
            .saturating_sub(previous.request_other_total),
    }
}

/// Parse isolated CPU cores from /sys/devices/system/cpu/isolated.
/// Returns the list of isolated core IDs (e.g., [8, 9, 10, 11]).
fn get_isolated_cores() -> Vec<usize> {
    let isolated_str =
        std::fs::read_to_string("/sys/devices/system/cpu/isolated").unwrap_or_default();
    let trimmed = isolated_str.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    // Parse "8-11" or "8,9,10,11" or "8-11,14-15"
    let mut cores = Vec::new();
    for part in trimmed.split(',') {
        if part.contains('-') {
            let bounds: Vec<&str> = part.split('-').collect();
            if bounds.len() == 2 {
                let start: usize = bounds[0].parse().unwrap_or(0);
                let end: usize = bounds[1].parse().unwrap_or(0);
                cores.extend(start..=end);
            }
        } else if let Ok(c) = part.parse::<usize>() {
            cores.push(c);
        }
    }
    cores
}

fn main() -> Result<()> {
    // ═══════════════════════════════════════════════════════
    // IGNIS FIX #3: Explicit runtime builder.
    // Worker thread count = isolated core count.
    // Each worker thread pinned to a different isolated core
    // via round-robin in on_thread_start.
    // ═══════════════════════════════════════════════════════
    let isolated_cores = get_isolated_cores();
    let worker_threads = if isolated_cores.is_empty() {
        4 // default fallback if no isolated cores
    } else {
        isolated_cores.len()
    };

    // Pin main thread to first isolated core
    if let Some(core_ids) = core_affinity::get_core_ids() {
        let target = if !isolated_cores.is_empty() {
            core_ids
                .iter()
                .find(|c| isolated_cores.contains(&c.id))
                .unwrap_or(&core_ids[0])
        } else {
            &core_ids[0]
        };
        if core_affinity::set_for_current(*target) {
            eprintln!("Main thread pinned to core {}", target.id);
        }
    }

    // Build tokio runtime with per-worker core pinning
    let isolated_for_rt = isolated_cores.clone();
    let worker_idx = std::sync::atomic::AtomicUsize::new(0);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .on_thread_start(move || {
            if !isolated_for_rt.is_empty() {
                let idx = worker_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let target_core_id = isolated_for_rt[idx % isolated_for_rt.len()];
                if let Some(core_ids) = core_affinity::get_core_ids() {
                    if let Some(core) = core_ids.iter().find(|c| c.id == target_core_id) {
                        core_affinity::set_for_current(*core);
                    }
                }
            }
        })
        .enable_all()
        .build()
        .expect("Failed to build tokio runtime");

    eprintln!(
        "Tokio runtime: {} workers, isolated cores: {:?}",
        worker_threads, isolated_cores
    );

    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    // Parse CLI arguments FIRST
    let args = Args::parse();

    let isolated_cores = get_isolated_cores();
    info!(
        "Isolated cores: {:?}, tokio workers: {}",
        isolated_cores,
        isolated_cores.len().max(2)
    );

    info!("AETHER-SNIPER v0.1.0 Starting...");

    dotenvy::dotenv().ok();

    let config = Config::load("config.toml")?;
    info!("Configuration loaded");

    // Handle --test-alert
    if args.test_alert {
        let (notif_token, notif_chat_id) = if let Some(ref n) = config.notifications {
            (n.telegram_bot_token.clone(), n.telegram_chat_id)
        } else {
            (None, None)
        };

        let notification_service = NotificationService::new(notif_token, notif_chat_id);

        if !notification_service.is_enabled() {
            error!("Notifications not configured. Set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID");
            return Err(anyhow::anyhow!("Notifications not configured"));
        }

        info!("Sending test alert...");
        match notification_service
            .send_direct("<b>Test Alert</b>\n\nAether Sniper is configured correctly!")
            .await
        {
            Ok(_) => {
                info!("Test alert sent successfully!");
                return Ok(());
            }
            Err(e) => {
                error!("Failed to send test alert: {}", e);
                return Err(e);
            }
        }
    }

    // Tokyo RPC first, then Helius, then config fallback
    let rpc_url = std::env::var("TOKYO_RPC_URL")
        .or_else(|_| std::env::var("HELIUS_RPC_URL"))
        .unwrap_or_else(|_| config.rpc.http_url.clone());
    let ws_url = std::env::var("HELIUS_WS_URL").unwrap_or_else(|_| config.rpc.ws_url.clone());

    info!("RPC: {}", rpc_url);

    // Create trade logger and hydrate daily loss
    let mut trade_logger_inner = TradeLogger::new(&config.logging.trades_file)?;

    // Calculate today's PnL so far for daily loss hydration
    let today = Utc::now().date_naive();
    let daily_pnl_so_far = trade_logger_inner.calculate_daily_pnl(today).unwrap_or(0.0);
    if daily_pnl_so_far != 0.0 {
        info!("Today's PnL so far: {:.4} SOL", daily_pnl_so_far);
    }

    let trading_mode_inner = if config.trading.mode == "live" {
        info!("Mode: LIVE TRADING");

        let wallet = if let Ok(keypair_path) = std::env::var("WALLET_KEYPAIR_PATH") {
            load_keypair_from_file(&keypair_path)?
        } else if std::env::var("WALLET_KEYPAIR").is_ok() {
            load_keypair_from_env("WALLET_KEYPAIR")?
        } else {
            error!(
                "No wallet keypair configured. Set WALLET_KEYPAIR_PATH or WALLET_KEYPAIR env var."
            );
            return Err(anyhow::anyhow!("Wallet keypair required for live trading"));
        };

        let mut live_trader = LiveTrader::new(
            &rpc_url,
            config.trading.clone(),
            config.safety.clone(),
            wallet,
        );

        // Hydrate daily loss if negative PnL today
        if daily_pnl_so_far < 0.0 {
            live_trader.hydrate_daily_loss(daily_pnl_so_far.abs());
        }

        info!("Wallet: {}", live_trader.get_wallet_pubkey());
        match live_trader.get_sol_balance() {
            Ok(balance) => info!("Balance: {:.4} SOL", balance),
            Err(e) => warn!("Could not fetch balance: {}", e),
        }

        if !config.trading.buy_enabled.unwrap_or(true) {
            info!("buy_enabled=false — SELL-ONLY MODE. Dumping all existing positions...");
            let (sold, failed) = live_trader.dump_all_positions().await;
            info!("Dump complete: {} sold, {} failed", sold, failed);
            match live_trader.get_sol_balance() {
                Ok(balance) => info!("Post-dump balance: {:.4} SOL", balance),
                Err(e) => warn!("Could not fetch post-dump balance: {}", e),
            }
        }

        TradingMode::Live(live_trader)
    } else {
        info!("Mode: Paper Trading");
        TradingMode::Paper(PaperTrader::new(
            &rpc_url,
            config.trading.clone(),
            config.safety.clone(),
        ))
    };

    // SIEGE ENGINE INITIALIZATION (SIDECAR)
    // We initialize it regardless of the flag if we are in Live mode,
    // so we can switch on the fly via config reload (future proofing) or just simpler static logic.
    let siege_engine = if config.trading.mode == "live" {
        let wallet = if let Ok(keypair_path) = std::env::var("WALLET_KEYPAIR_PATH") {
            load_keypair_from_file(&keypair_path)?
        } else {
            // If we are here, we likely already loaded it for LiveTrader, but we need a clone or reload.
            // Keypair doesn't clone easily if not careful. Let's re-load from env/file.
            // Efficiency note: This is done once at startup.
            load_keypair_from_env("WALLET_KEYPAIR").unwrap_or_else(|_| Keypair::new())
        };

        Some(SiegeTrader::new(
            rpc_url.clone(),
            config.trading.clone(),
            config.safety.clone(),
            wallet,
        ))
    } else {
        None
    };

    // Wrap in Arc for shared access if needed (though mostly main loop uses it)
    // We don't need RwLock because it's stateless/read-only mostly or handles its own state
    let siege_engine = Arc::new(siege_engine);

    let trade_logger = Arc::new(RwLock::new(trade_logger_inner));
    let trading_mode = Arc::new(RwLock::new(trading_mode_inner));
    let (broadcast_tx, _) = broadcast::channel::<StreamEvent>(100);

    // Initialize Notification Service
    let (notif_token, notif_chat_id) = if let Some(ref n) = config.notifications {
        if n.enabled.unwrap_or(false) {
            (n.telegram_bot_token.clone(), n.telegram_chat_id)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let notification_service = Arc::new(NotificationService::new(notif_token, notif_chat_id));

    if notification_service.is_enabled() {
        let service = notification_service.clone();
        let rx = broadcast_tx.subscribe();
        tokio::spawn(async move {
            service.start(rx).await;
        });
        info!("Telegram notifications enabled");
    }

    // Dashboard State
    let app_state = AppState::new(
        trading_mode.clone(),
        trade_logger.clone(),
        broadcast_tx.clone(),
        config.dashboard.clone(),
    );

    // Keep dashboard snapshots refreshed without making API handlers contend on
    // long-lived trading_mode write locks.
    let app_state_snapshot = app_state.clone();
    let trading_mode_snapshot = trading_mode.clone();
    let trade_logger_snapshot = trade_logger.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            interval.tick().await;

            if let Ok(mode) = trading_mode_snapshot.try_read() {
                let positions = match &*mode {
                    TradingMode::Paper(trader) => trader.get_open_positions(),
                    TradingMode::Live(trader) => trader.get_open_positions(),
                };
                let positions_owned: Vec<_> = positions.into_iter().cloned().collect();
                drop(mode);
                app_state_snapshot
                    .set_cached_positions(positions_owned)
                    .await;
            }

            if let Ok(logger) = trade_logger_snapshot.try_read() {
                let metrics = logger.get_metrics().clone();
                drop(logger);
                app_state_snapshot.set_cached_metrics(metrics).await;
            }
        }
    });

    // Start HTTP server always so /metrics remains available even when dashboard UI is disabled.
    let app_state_http = app_state.clone();
    let port = config.dashboard.port;
    let dashboard_enabled = config.dashboard.enabled;
    tokio::spawn(async move {
        let app = create_router(app_state_http);
        let addr = format!("0.0.0.0:{}", port);
        if dashboard_enabled {
            info!("Dashboard running at http://{}", addr);
        } else {
            info!(
                "Metrics/control API running at http://{} (dashboard UI disabled)",
                addr
            );
        }
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        if let Err(e) = axum::serve(listener, app).await {
            error!("Dashboard server error: {}", e);
        }
    });

    if config.dashboard.enabled {
        let trading_mode_clone = trading_mode.clone();
        let trade_logger_clone = trade_logger.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                let mode = trading_mode_clone.read().await;
                let balance = match &*mode {
                    TradingMode::Live(trader) => trader.get_sol_balance().unwrap_or(0.0),
                    TradingMode::Paper(_) => 0.0,
                };
                drop(mode);
                let mut logger = trade_logger_clone.write().await;
                logger.set_wallet_balance(balance);
            }
        });
    }

    let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(100);

    let monitor = WebSocketMonitor::new(ws_url, rpc_url.clone(), event_tx);

    let monitor_handle = tokio::spawn(async move {
        if let Err(e) = monitor.start().await {
            error!("WebSocket monitor error: {}", e);
        }
    });

    let (intent_tx, mut intent_rx) = mpsc::channel::<TradeIntent>(INTENT_QUEUE_CAPACITY);
    let trading_mode_worker = trading_mode.clone();
    let trade_logger_worker = trade_logger.clone();
    let broadcast_tx_worker = broadcast_tx.clone();
    let siege_engine_worker = siege_engine.clone();
    let use_siege_engine = config.trading.use_siege_engine.unwrap_or(false);

    let max_intent_age_us: u64 = config
        .trading
        .max_intent_age_ms
        .unwrap_or(5000)
        .clamp(500, 10_000)
        * 1000;

    tokio::spawn(async move {
        info!("[exec-worker] started, waiting for intents");
        while let Some(intent) = intent_rx.recv().await {
            let (label, enqueued_at) = match &intent {
                TradeIntent::NewPool(_, lat) => ("NewPool", lat.intent_enqueued_ts_us),
                TradeIntent::NewPumpFunToken(_, lat) => {
                    ("NewPumpFunToken", lat.intent_enqueued_ts_us)
                }
            };
            let age_us = now_us().saturating_sub(enqueued_at);
            if age_us > max_intent_age_us {
                warn!(
                    "[exec-worker] STALE intent dropped: {} age={}ms q_pending={}",
                    label,
                    age_us / 1000,
                    intent_rx.len()
                );
                record_stale_intent_drop();
                continue;
            }
            debug!(
                "[exec-worker] recv intent: {} age={}ms",
                label,
                age_us / 1000
            );
            if let Err(e) = process_trade_intent(
                intent,
                &trading_mode_worker,
                &trade_logger_worker,
                &broadcast_tx_worker,
                &siege_engine_worker,
                use_siege_engine,
            )
            .await
            {
                error!("Intent worker error: {}", e);
            }
            debug!("[exec-worker] done: {}", label);
        }
        warn!("[exec-worker] channel closed, exiting");
    });

    set_trade_enabled(true);
    set_gate_mode(0);
    set_gate_reason_code(0);

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        let mut previous = jito_metrics_snapshot();
        let mut unstable_windows = 0_u32;
        let mut stable_windows = 0_u32;
        let mut half_open_windows_remaining = 0_u32;

        loop {
            interval.tick().await;

            let latency = runtime_latency_snapshot(Some(600));
            let current = jito_metrics_snapshot();
            let delta = compute_jito_delta(current, previous);
            previous = current;

            let total = delta
                .success_2xx
                .saturating_add(delta.client_4xx_non429)
                .saturating_add(delta.rate_limited_429)
                .saturating_add(delta.server_5xx)
                .saturating_add(delta.other);

            let effective_success = milli_pct(delta.success_2xx, total);
            let rate_limit = milli_pct(delta.rate_limited_429, total);
            let client_error = milli_pct(delta.client_4xx_non429, total);
            let server_error = milli_pct(delta.server_5xx, total);
            set_latest_error_rates(effective_success, rate_limit, client_error, server_error);

            let enough_detection_samples = latency.detection_samples >= 1000;
            let enough_exec_samples = total >= 500;
            let detection_good =
                latency.detection_p99_us <= 10_000 && latency.detection_jitter_us <= 5_000;
            let execution_good = rate_limit <= 3_000 && server_error <= 1_000;
            let can_evaluate_window = enough_detection_samples && enough_exec_samples;
            let window_good = detection_good && execution_good;

            let mode = aether_sniper::trading::runtime_gate::gate_mode();
            match mode {
                // normal
                0 => {
                    if can_evaluate_window && !window_good {
                        unstable_windows = unstable_windows.saturating_add(1);
                    } else if can_evaluate_window {
                        unstable_windows = 0;
                    }
                    if unstable_windows >= 2 {
                        set_gate_mode(1);
                        set_gate_reason_code(1001);
                        set_trade_enabled(false);
                        trigger_jito_emergency_stop("auto gate: degraded SLO/error window");
                        stable_windows = 0;
                        info!(
                            "AUTO_GATE: normal->throttled, det_p99={}us jitter={}us 429={}‰ 5xx={}‰ samples(det={},exec={})",
                            latency.detection_p99_us,
                            latency.detection_jitter_us,
                            rate_limit,
                            server_error,
                            latency.detection_samples,
                            total
                        );
                    }
                }
                // throttled
                1 => {
                    if can_evaluate_window && window_good {
                        stable_windows = stable_windows.saturating_add(1);
                    } else if can_evaluate_window {
                        stable_windows = 0;
                    }
                    if stable_windows >= 3 {
                        set_gate_mode(2);
                        set_gate_reason_code(2001);
                        set_trade_enabled(true);
                        clear_jito_emergency_stop();
                        half_open_windows_remaining = 3;
                        info!("AUTO_GATE: throttled->half_open");
                    }
                }
                // half-open
                2 => {
                    if can_evaluate_window && !window_good {
                        set_gate_mode(1);
                        set_gate_reason_code(2002);
                        set_trade_enabled(false);
                        trigger_jito_emergency_stop("auto gate: half_open regression");
                        stable_windows = 0;
                        info!("AUTO_GATE: half_open->throttled (regression)");
                    } else if can_evaluate_window && half_open_windows_remaining > 0 {
                        half_open_windows_remaining -= 1;
                        if half_open_windows_remaining == 0 {
                            set_gate_mode(0);
                            set_gate_reason_code(0);
                            set_trade_enabled(true);
                            info!("AUTO_GATE: half_open->normal");
                        }
                    }
                }
                // blocked fallback
                _ => {
                    set_trade_enabled(false);
                }
            }

            info!(
                target: "control_plane_metrics",
                "{{\"trade_enabled\":{},\"gate_mode\":{},\"effective_success_milli_pct\":{},\"rate_limit_milli_pct\":{},\"client_error_milli_pct\":{},\"server_error_milli_pct\":{},\"detection_p99_us\":{},\"detection_jitter_us\":{},\"execution_p99_us\":{},\"backoff_p99_us\":{},\"detection_samples\":{},\"exec_samples\":{},\"dropped_intents_total\":{},\"stale_drop_total\":{}}}",
                if is_trade_enabled() { 1 } else { 0 },
                aether_sniper::trading::runtime_gate::gate_mode(),
                effective_success,
                rate_limit,
                client_error,
                server_error,
                latency.detection_p99_us,
                latency.detection_jitter_us,
                latency.execution_p99_us,
                latency.backoff_p99_us,
                latency.detection_samples,
                total,
                latency.dropped_intents_total,
                latency.stale_drop_total
            );
        }
    });

    info!("👀 Watching for new Pump.fun tokens...");
    info!("Press Ctrl+C to stop");

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                match event {
                    StreamEvent::NewPool(pool, mut latency) => {
                        latency.channel_receive_us = now_us();
                        mark_detection_enqueue(&mut latency);
                        record_detection_latency(latency.detection_latency_us);
                        let _ = broadcast_tx.send(StreamEvent::NewPool(pool.clone(), latency.clone()));
                        if let Ok(json) = serde_json::to_string(&latency) {
                            info!(target: "latency_detection_metrics", "{}", json);
                        }

                        if !try_enqueue_drop_newest(&intent_tx, TradeIntent::NewPool(pool, latency)) {
                            record_intent_drop();
                            warn!("Intent queue full. drop-newest policy applied for NewPool");
                        }
                    }
                    StreamEvent::NewPumpFunToken(token, mut latency) => {
                        latency.channel_receive_us = now_us();
                        mark_detection_enqueue(&mut latency);
                        record_detection_latency(latency.detection_latency_us);
                        let _ = broadcast_tx.send(StreamEvent::NewPumpFunToken(token.clone(), latency.clone()));
                        if let Ok(json) = serde_json::to_string(&latency) {
                            info!(target: "latency_detection_metrics", "{}", json);
                        }

                        if !try_enqueue_drop_newest(&intent_tx, TradeIntent::NewPumpFunToken(token, latency)) {
                            record_intent_drop();
                            warn!("Intent queue full. drop-newest policy applied for NewPumpFunToken");
                        } else {
                            debug!("[main-loop] PumpFun intent enqueued, q_pending={}", INTENT_QUEUE_CAPACITY - intent_tx.capacity());
                        }
                    }
                    StreamEvent::PriceUpdate { pool_id, price_sol } => {
                        let _ = broadcast_tx.send(StreamEvent::PriceUpdate { pool_id, price_sol });

                        // Phase 1: Brief WRITE lock — compute position updates (synchronous).
                        // Collect sell orders and paper trades, then DROP the lock.
                        let (paper_trades, live_sells) = {
                            let mut mode = trading_mode.write().await;
                            match &mut *mode {
                                TradingMode::Paper(trader) => {
                                    let closed = trader.update_positions(vec![(pool_id.to_string(), price_sol)]);
                                    (closed, vec![])
                                }
                                TradingMode::Live(trader) => {
                                    let to_sell = trader.update_positions(vec![(pool_id.to_string(), price_sol)]);
                                    (vec![], to_sell)
                                }
                            }
                        }; // write lock DROPPED — dashboard reads unblocked

                        // Paper trades: log immediately (no network calls)
                        if !paper_trades.is_empty() {
                            let mut logger = trade_logger.write().await;
                            for trade in paper_trades {
                                logger.log_trade(&trade)?;
                                let _ = broadcast_tx.send(StreamEvent::TradeExecuted(trade));
                            }
                        }

                        // Live sells: execute with READ lock (doesn't block dashboard)
                        for (position, trade_result) in live_sells {
                            info!("Executing sell for position: {}", position.id);
                            let sell_result = {
                                let mode = trading_mode.read().await;
                                match &*mode {
                                    TradingMode::Live(trader) => {
                                        let res = trader.execute_sell(&position).await;
                                        // Feed sell outcome into OmegaTrinity circuit
                                        match &res {
                                            Ok(_) => trader.on_sell_executed(&trade_result),
                                            Err(_) => trader.on_trade_error(),
                                        }
                                        res
                                    }
                                    _ => unreachable!("mode changed mid-sell"),
                                }
                            }; // read lock dropped
                            match sell_result {
                                Ok(bundle_id) => {
                                    info!("Sell executed: {}", bundle_id);
                                    let mut logger = trade_logger.write().await;
                                    logger.log_trade(&trade_result)?;
                                    let _ = broadcast_tx.send(StreamEvent::TradeExecuted(trade_result));
                                }
                                Err(e) => {
                                    error!("Sell execution failed: {}", e);
                                    let _ = broadcast_tx.send(StreamEvent::TradeFailed {
                                        reason: format!("Sell failed: {}", e),
                                        pool_id: Some(pool_id)
                                    });
                                }
                            }
                        }
                    }
                    StreamEvent::Error(msg) => {
                        let _ = broadcast_tx.send(StreamEvent::Error(msg.clone()));
                        error!("Stream error: {}", msg);
                    }
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                break;
            }
        }
    }

    trade_logger.read().await.print_summary();

    monitor_handle.abort();
    info!("AETHER-SNIPER stopped");

    Ok(())
}

async fn process_trade_intent(
    intent: TradeIntent,
    trading_mode: &Arc<RwLock<TradingMode>>,
    trade_logger: &Arc<RwLock<TradeLogger>>,
    broadcast_tx: &broadcast::Sender<StreamEvent>,
    siege_engine: &Arc<Option<SiegeTrader>>,
    use_siege_engine: bool,
) -> Result<()> {
    match intent {
        TradeIntent::NewPool(pool, mut latency) => {
            latency.exec_start_ts_us = now_us();

            // Hold trading_mode write lock for the trade, but NOT logger.
            // Logger is acquired briefly only when needed for log_trade().
            let (result, trades_to_log) = {
                let mut mode = trading_mode.write().await;
                match &mut *mode {
                    TradingMode::Paper(trader) => {
                        let r = trader.on_new_pool(pool.clone(), &mut latency).await;
                        let trades: Vec<_> = if r.is_ok() && r.as_ref().unwrap().is_some() {
                            trader
                                .get_all_trades()
                                .iter()
                                .rev()
                                .take(1)
                                .cloned()
                                .collect()
                        } else {
                            vec![]
                        };
                        (r, trades)
                    }
                    TradingMode::Live(trader) => {
                        if !is_trade_enabled() {
                            info!("AUTO_GATE skip live buy (NewPool): trade_enabled=false");
                            latency.exec_end_ts_us = now_us();
                            latency.execution_latency_us = latency
                                .exec_end_ts_us
                                .saturating_sub(latency.intent_enqueued_ts_us);
                            record_execution_latency(latency.execution_latency_us);
                            return Ok(());
                        }
                        if use_siege_engine {
                            if let Some(siege) = &**siege_engine {
                                info!("[Siege] Routing to Siege Engine...");
                                match siege.on_new_pool(pool.clone(), &mut latency).await {
                                    Ok(Some(pos)) => {
                                        info!("[Siege] EXECUTION TRIGGERED: {}", pos.id)
                                    }
                                    Ok(None) => {}
                                    Err(e) => error!("[Siege] Error: {}", e),
                                }
                            } else {
                                error!("Siege Engine enabled but not initialized!");
                            }
                            (Ok(None), vec![])
                        } else {
                            let r = trader.on_new_pool(pool.clone(), &mut latency).await;
                            let trades: Vec<_> = if r.is_ok() && r.as_ref().unwrap().is_some() {
                                trader
                                    .get_all_trades()
                                    .iter()
                                    .rev()
                                    .take(1)
                                    .cloned()
                                    .collect()
                            } else {
                                vec![]
                            };
                            (r, trades)
                        }
                    }
                }
            }; // trading_mode write lock DROPPED

            match result {
                Ok(Some(position)) => {
                    info!("Position opened: {}", position.id);
                    let mut logger = trade_logger.write().await; // brief lock
                    for trade in &trades_to_log {
                        logger.log_trade(trade)?;
                        let _ = broadcast_tx.send(StreamEvent::TradeExecuted(trade.clone()));
                    }
                }
                Ok(None) => {
                    debug!("Pool skipped (failed safety checks or no action)");
                }
                Err(e) => {
                    error!("Error processing pool: {}", e);
                    let _ = broadcast_tx.send(StreamEvent::TradeFailed {
                        reason: e.to_string(),
                        pool_id: Some(pool.amm_id),
                    });
                }
            }

            latency.exec_end_ts_us = now_us();
            latency.execution_latency_us = latency
                .exec_end_ts_us
                .saturating_sub(latency.intent_enqueued_ts_us);
            record_execution_latency(latency.execution_latency_us);
            if latency.backoff_latency_us > 0 {
                record_backoff_latency(latency.backoff_latency_us);
            }
            if let Ok(json) = serde_json::to_string(&latency) {
                info!(target: "latency_execution_metrics", "{}", json);
            }
        }
        TradeIntent::NewPumpFunToken(token, mut latency) => {
            latency.exec_start_ts_us = now_us();

            // Hold trading_mode write lock for the trade, but NOT logger.
            let (result, trades_to_log) = {
                let mut mode = trading_mode.write().await;
                match &mut *mode {
                    TradingMode::Paper(trader) => {
                        let r = trader
                            .on_new_pumpfun_token(token.clone(), &mut latency)
                            .await;
                        let trades: Vec<_> = if r.is_ok() && r.as_ref().unwrap().is_some() {
                            trader
                                .get_all_trades()
                                .iter()
                                .rev()
                                .take(1)
                                .cloned()
                                .collect()
                        } else {
                            vec![]
                        };
                        (r, trades)
                    }
                    TradingMode::Live(trader) => {
                        if !is_trade_enabled() {
                            info!("AUTO_GATE skip live buy (NewPumpFunToken): trade_enabled=false");
                            latency.exec_end_ts_us = now_us();
                            latency.execution_latency_us = latency
                                .exec_end_ts_us
                                .saturating_sub(latency.intent_enqueued_ts_us);
                            record_execution_latency(latency.execution_latency_us);
                            return Ok(());
                        }
                        let r = trader
                            .on_new_pumpfun_token(token.clone(), &mut latency)
                            .await;
                        let trades: Vec<_> = if r.is_ok() && r.as_ref().unwrap().is_some() {
                            trader
                                .get_all_trades()
                                .iter()
                                .rev()
                                .take(1)
                                .cloned()
                                .collect()
                        } else {
                            vec![]
                        };
                        (r, trades)
                    }
                }
            }; // trading_mode write lock DROPPED

            match result {
                Ok(Some(position)) => {
                    info!("Pump.fun position opened: {}", position.id);
                    let mut logger = trade_logger.write().await; // brief lock
                    for trade in &trades_to_log {
                        logger.log_trade(trade)?;
                        let _ = broadcast_tx.send(StreamEvent::TradeExecuted(trade.clone()));
                    }
                }
                Ok(None) => {
                    debug!("Pump.fun token skipped: no action taken");
                }
                Err(e) => {
                    error!("Error executing Pump.fun trade: {}", e);
                }
            }

            latency.exec_end_ts_us = now_us();
            latency.execution_latency_us = latency
                .exec_end_ts_us
                .saturating_sub(latency.intent_enqueued_ts_us);
            record_execution_latency(latency.execution_latency_us);
            if latency.backoff_latency_us > 0 {
                record_backoff_latency(latency.backoff_latency_us);
            }
            if let Ok(json) = serde_json::to_string(&latency) {
                info!(target: "latency_execution_metrics", "{}", json);
            }
        }
    }

    Ok(())
}
