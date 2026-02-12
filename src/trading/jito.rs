use crate::constants::{DEFAULT_JITO_TIP, JITO_AMSTERDAM, JITO_FRANKFURT, JITO_NY, JITO_TOKYO};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use parking_lot::Mutex;
use reqwest::header::RETRY_AFTER;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, info, warn};

const MAX_TX_SIZE: usize = 1232;
const DEFAULT_RETRY_ATTEMPTS: u32 = 2;
const DEFAULT_MAX_CONCURRENCY: usize = 2;
const DEFAULT_MAX_QUEUE_DEPTH: u64 = 128;
const DEFAULT_MAX_RPS: f64 = 4.9;
const DEFAULT_MIN_RPS: f64 = 1.0;
const DEFAULT_MAX_RPS_DYNAMIC: f64 = 4.9;
const DEFAULT_BURST_TOKENS: f64 = 2.0;
const DEFAULT_MIN_CONCURRENCY: usize = 1;
const DEFAULT_MAX_CONCURRENCY_DYNAMIC: usize = 6;
const SUCCESS_STEP_FOR_INCREASE: u32 = 32;
const MAX_CONSECUTIVE_429_BEFORE_STOP: u32 = 50;
const EMERGENCY_STOP_AUTO_CLEAR_SECS: u64 = 60;

static EMERGENCY_STOP_ACTIVATED_US: AtomicU64 = AtomicU64::new(0);
static EMERGENCY_COUNTER_RESET: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    pub static ref JITO_TIP_ACCOUNTS: Vec<Pubkey> = vec![
        Pubkey::from_str("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5").unwrap(),
        Pubkey::from_str("HFqU5x63VTqvQss8hp11i4bVmkdzGvxmQJGhPr9J12rW").unwrap(),
        Pubkey::from_str("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY").unwrap(),
        Pubkey::from_str("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49").unwrap(),
        Pubkey::from_str("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh").unwrap(),
        Pubkey::from_str("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt").unwrap(),
        Pubkey::from_str("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL").unwrap(),
        Pubkey::from_str("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT").unwrap(),
    ];
    static ref JITO_METRICS: JitoMetrics = JitoMetrics::default();
    static ref JITO_EMERGENCY_STOP: AtomicBool = AtomicBool::new(false);
}

#[derive(Debug, Error)]
pub enum JitoSendError {
    #[error("emergency stop active: {0}")]
    EmergencyStop(String),
    #[error("request dropped due to queue pressure (depth={0})")]
    QueueFull(u64),
    #[error("rate limited (429), retry_after_ms={retry_after_ms:?}: {body}")]
    RateLimited {
        body: String,
        retry_after_ms: Option<u64>,
    },
    #[error("bad request (400): {0}")]
    BadRequest(String),
    #[error("http status {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("response parse error: {0}")]
    ResponseParse(String),
}

#[derive(Default)]
struct JitoMetrics {
    requests_total: AtomicU64,
    request_2xx_total: AtomicU64,
    request_400_total: AtomicU64,
    request_4xx_non429_total: AtomicU64,
    request_4xx_retryable_total: AtomicU64,
    request_429_total: AtomicU64,
    request_5xx_total: AtomicU64,
    request_other_total: AtomicU64,
    jito_400_total: AtomicU64,
    jito_429_total: AtomicU64,
    inflight_requests: AtomicU64,
    queue_depth: AtomicU64,
    limiter_cooldown_active: AtomicU64,
    retries_total: AtomicU64,
    backoff_sleep_events_total: AtomicU64,
    backoff_sleep_total_us: AtomicU64,
    dropped_total: AtomicU64,
    emergency_stop_total: AtomicU64,
    tokens_available_milli: AtomicU64,
    limiter_dynamic_rps_milli: AtomicU64,
    limiter_dynamic_concurrency: AtomicU64,
    preflight_fail_total: AtomicU64,
}

fn unix_time_us() -> u64 {
    crate::time::monotonic_now_us()
}

pub fn trigger_jito_emergency_stop(reason: &str) {
    if !JITO_EMERGENCY_STOP.swap(true, Ordering::SeqCst) {
        EMERGENCY_STOP_ACTIVATED_US.store(unix_time_us(), Ordering::SeqCst);
        JITO_METRICS
            .emergency_stop_total
            .fetch_add(1, Ordering::Relaxed);
        error!("Jito emergency_stop activated: {}", reason);
    }
}

pub fn clear_jito_emergency_stop() {
    if JITO_EMERGENCY_STOP.swap(false, Ordering::SeqCst) {
        EMERGENCY_STOP_ACTIVATED_US.store(0, Ordering::SeqCst);
        EMERGENCY_COUNTER_RESET.store(true, Ordering::Relaxed);
        warn!("Jito emergency_stop cleared");
    }
}

pub fn is_jito_emergency_stop_active() -> bool {
    if !JITO_EMERGENCY_STOP.load(Ordering::SeqCst) {
        return false;
    }
    let activated = EMERGENCY_STOP_ACTIVATED_US.load(Ordering::SeqCst);
    if activated > 0 {
        let elapsed_us = unix_time_us().saturating_sub(activated);
        if elapsed_us >= EMERGENCY_STOP_AUTO_CLEAR_SECS * 1_000_000 {
            JITO_EMERGENCY_STOP.store(false, Ordering::SeqCst);
            EMERGENCY_STOP_ACTIVATED_US.store(0, Ordering::SeqCst);
            EMERGENCY_COUNTER_RESET.store(true, Ordering::Relaxed);
            warn!(
                "Jito emergency_stop auto-cleared after {}s cooldown",
                EMERGENCY_STOP_AUTO_CLEAR_SECS
            );
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct JitoMetricsSnapshot {
    pub request_2xx_total: u64,
    pub request_4xx_non429_total: u64,
    pub request_429_total: u64,
    pub request_5xx_total: u64,
    pub request_other_total: u64,
    pub retries_total: u64,
    pub dropped_total: u64,
}

pub fn jito_metrics_snapshot() -> JitoMetricsSnapshot {
    JitoMetricsSnapshot {
        request_2xx_total: JITO_METRICS.request_2xx_total.load(Ordering::Relaxed),
        request_4xx_non429_total: JITO_METRICS
            .request_4xx_non429_total
            .load(Ordering::Relaxed),
        request_429_total: JITO_METRICS.request_429_total.load(Ordering::Relaxed),
        request_5xx_total: JITO_METRICS.request_5xx_total.load(Ordering::Relaxed),
        request_other_total: JITO_METRICS.request_other_total.load(Ordering::Relaxed),
        retries_total: JITO_METRICS.retries_total.load(Ordering::Relaxed),
        dropped_total: JITO_METRICS.dropped_total.load(Ordering::Relaxed),
    }
}

pub fn jito_prometheus_metrics() -> String {
    format!(
        concat!(
            "# HELP jito_requests_attempt_total Total Jito request attempts (all statuses)\n",
            "# TYPE jito_requests_attempt_total counter\n",
            "jito_requests_attempt_total {}\n",
            "# HELP jito_requests_total Total Jito request attempts\n",
            "# TYPE jito_requests_total counter\n",
            "jito_requests_total{{code=\"2xx\"}} {}\n",
            "jito_requests_total{{code=\"400\"}} {}\n",
            "jito_requests_total{{code=\"4xx_non429\"}} {}\n",
            "jito_requests_total{{code=\"4xx_retryable\"}} {}\n",
            "jito_requests_total{{code=\"429\"}} {}\n",
            "jito_requests_total{{code=\"5xx\"}} {}\n",
            "jito_requests_total{{code=\"other\"}} {}\n",
            "# HELP jito_429_total HTTP 429 responses from Jito\n",
            "# TYPE jito_429_total counter\n",
            "jito_429_total {}\n",
            "# HELP jito_400_total HTTP 400 responses from Jito\n",
            "# TYPE jito_400_total counter\n",
            "jito_400_total {}\n",
            "# HELP limiter_cooldown_active Limiter cooldown state (0/1)\n",
            "# TYPE limiter_cooldown_active gauge\n",
            "limiter_cooldown_active {}\n",
            "# HELP inflight_requests Current in-flight Jito requests\n",
            "# TYPE inflight_requests gauge\n",
            "inflight_requests {}\n",
            "# HELP queue_depth Current Jito request queue depth\n",
            "# TYPE queue_depth gauge\n",
            "queue_depth {}\n",
            "# HELP jito_retries_total Jito retry attempts\n",
            "# TYPE jito_retries_total counter\n",
            "jito_retries_total {}\n",
            "# HELP jito_backoff_sleep_events_total Jito backoff sleep events\n",
            "# TYPE jito_backoff_sleep_events_total counter\n",
            "jito_backoff_sleep_events_total {}\n",
            "# HELP jito_backoff_sleep_total_us Jito accumulated backoff sleep in microseconds\n",
            "# TYPE jito_backoff_sleep_total_us counter\n",
            "jito_backoff_sleep_total_us {}\n",
            "# HELP jito_dropped_total Dropped Jito requests under pressure\n",
            "# TYPE jito_dropped_total counter\n",
            "jito_dropped_total {}\n",
            "# HELP jito_tokens_available_milli Available limiter tokens x1000\n",
            "# TYPE jito_tokens_available_milli gauge\n",
            "jito_tokens_available_milli {}\n",
            "# HELP jito_limiter_dynamic_rps_milli Adaptive limiter current RPS x1000\n",
            "# TYPE jito_limiter_dynamic_rps_milli gauge\n",
            "jito_limiter_dynamic_rps_milli {}\n",
            "# HELP jito_limiter_dynamic_concurrency Adaptive limiter current concurrency\n",
            "# TYPE jito_limiter_dynamic_concurrency gauge\n",
            "jito_limiter_dynamic_concurrency {}\n",
            "# HELP jito_emergency_stop Jito emergency stop state (0/1)\n",
            "# TYPE jito_emergency_stop gauge\n",
            "jito_emergency_stop {}\n",
            "# HELP jito_preflight_fail_total Tip write-lock preflight failures\n",
            "# TYPE jito_preflight_fail_total counter\n",
            "jito_preflight_fail_total {}\n",
        ),
        JITO_METRICS.requests_total.load(Ordering::Relaxed),
        JITO_METRICS.request_2xx_total.load(Ordering::Relaxed),
        JITO_METRICS.request_400_total.load(Ordering::Relaxed),
        JITO_METRICS
            .request_4xx_non429_total
            .load(Ordering::Relaxed),
        JITO_METRICS
            .request_4xx_retryable_total
            .load(Ordering::Relaxed),
        JITO_METRICS.request_429_total.load(Ordering::Relaxed),
        JITO_METRICS.request_5xx_total.load(Ordering::Relaxed),
        JITO_METRICS.request_other_total.load(Ordering::Relaxed),
        JITO_METRICS.jito_429_total.load(Ordering::Relaxed),
        JITO_METRICS.jito_400_total.load(Ordering::Relaxed),
        JITO_METRICS.limiter_cooldown_active.load(Ordering::Relaxed),
        JITO_METRICS.inflight_requests.load(Ordering::Relaxed),
        JITO_METRICS.queue_depth.load(Ordering::Relaxed),
        JITO_METRICS.retries_total.load(Ordering::Relaxed),
        JITO_METRICS
            .backoff_sleep_events_total
            .load(Ordering::Relaxed),
        JITO_METRICS.backoff_sleep_total_us.load(Ordering::Relaxed),
        JITO_METRICS.dropped_total.load(Ordering::Relaxed),
        JITO_METRICS.tokens_available_milli.load(Ordering::Relaxed),
        JITO_METRICS
            .limiter_dynamic_rps_milli
            .load(Ordering::Relaxed),
        JITO_METRICS
            .limiter_dynamic_concurrency
            .load(Ordering::Relaxed),
        if is_jito_emergency_stop_active() {
            1
        } else {
            0
        },
        JITO_METRICS.preflight_fail_total.load(Ordering::Relaxed),
    )
}

struct LimiterState {
    tokens: f64,
    last_refill: Instant,
    cooldown_until: Option<Instant>,
    current_rps: f64,
    min_rps: f64,
    max_rps: f64,
    current_concurrency: usize,
    min_concurrency: usize,
    max_concurrency: usize,
    success_streak: u32,
}

struct JitoRateLimiter {
    state: Mutex<LimiterState>,
    burst_tokens: f64,
    max_queue_depth: u64,
    semaphore: Arc<Semaphore>,
    consecutive_429: AtomicU32,
}

struct JitoSendPermit {
    _permit: OwnedSemaphorePermit,
}

impl Drop for JitoSendPermit {
    fn drop(&mut self) {
        JITO_METRICS
            .inflight_requests
            .fetch_sub(1, Ordering::Relaxed);
    }
}

impl JitoRateLimiter {
    fn new(
        min_rps: f64,
        max_rps: f64,
        burst_tokens: f64,
        min_concurrency: usize,
        max_concurrency: usize,
        max_queue_depth: u64,
    ) -> Self {
        let bounded_min_rps = min_rps.max(0.5);
        let bounded_max_rps = max_rps.max(bounded_min_rps);
        let bounded_min_conc = min_concurrency.max(1);
        let bounded_max_conc = max_concurrency.max(bounded_min_conc);
        JITO_METRICS
            .limiter_dynamic_rps_milli
            .store((bounded_max_rps * 1000.0) as u64, Ordering::Relaxed);
        JITO_METRICS
            .limiter_dynamic_concurrency
            .store(bounded_max_conc as u64, Ordering::Relaxed);
        Self {
            state: Mutex::new(LimiterState {
                tokens: burst_tokens,
                last_refill: Instant::now(),
                cooldown_until: None,
                current_rps: bounded_max_rps,
                min_rps: bounded_min_rps,
                max_rps: bounded_max_rps,
                current_concurrency: bounded_max_conc,
                min_concurrency: bounded_min_conc,
                max_concurrency: bounded_max_conc,
                success_streak: 0,
            }),
            burst_tokens: burst_tokens.max(1.0),
            max_queue_depth,
            semaphore: Arc::new(Semaphore::new(bounded_max_conc)),
            consecutive_429: AtomicU32::new(0),
        }
    }

    async fn acquire_slot(&self) -> std::result::Result<JitoSendPermit, JitoSendError> {
        if is_jito_emergency_stop_active() {
            return Err(JitoSendError::EmergencyStop(
                "send path blocked by emergency_stop".to_string(),
            ));
        }

        let depth = JITO_METRICS.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        if depth > self.max_queue_depth {
            JITO_METRICS.queue_depth.fetch_sub(1, Ordering::Relaxed);
            JITO_METRICS.dropped_total.fetch_add(1, Ordering::Relaxed);
            return Err(JitoSendError::QueueFull(depth));
        }

        if let Err(e) = self.wait_for_token().await {
            JITO_METRICS.queue_depth.fetch_sub(1, Ordering::Relaxed);
            return Err(e);
        }

        self.wait_for_concurrency_budget().await?;

        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| JitoSendError::Transport("semaphore closed".to_string()))?;

        JITO_METRICS.queue_depth.fetch_sub(1, Ordering::Relaxed);
        JITO_METRICS
            .inflight_requests
            .fetch_add(1, Ordering::Relaxed);

        Ok(JitoSendPermit { _permit: permit })
    }

    async fn wait_for_concurrency_budget(&self) -> std::result::Result<(), JitoSendError> {
        loop {
            if is_jito_emergency_stop_active() {
                return Err(JitoSendError::EmergencyStop(
                    "concurrency gate interrupted by emergency_stop".to_string(),
                ));
            }
            let (limit, inflight) = {
                let state = self.state.lock();
                (
                    state.current_concurrency,
                    JITO_METRICS.inflight_requests.load(Ordering::Relaxed) as usize,
                )
            };
            if inflight < limit {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    async fn wait_for_token(&self) -> std::result::Result<(), JitoSendError> {
        loop {
            if is_jito_emergency_stop_active() {
                return Err(JitoSendError::EmergencyStop(
                    "rate limiter interrupted by emergency_stop".to_string(),
                ));
            }

            let wait_time = {
                let mut state = self.state.lock();
                let now = Instant::now();
                let elapsed = now.duration_since(state.last_refill).as_secs_f64();
                if elapsed > 0.0 {
                    state.tokens =
                        (state.tokens + elapsed * state.current_rps).min(self.burst_tokens);
                    state.last_refill = now;
                }

                if let Some(cooldown_until) = state.cooldown_until {
                    if now < cooldown_until {
                        JITO_METRICS
                            .limiter_cooldown_active
                            .store(1, Ordering::Relaxed);
                        cooldown_until.saturating_duration_since(now)
                    } else {
                        state.cooldown_until = None;
                        JITO_METRICS
                            .limiter_cooldown_active
                            .store(0, Ordering::Relaxed);
                        Duration::ZERO
                    }
                } else {
                    Duration::ZERO
                }
            };

            if !wait_time.is_zero() {
                tokio::time::sleep(wait_time.min(Duration::from_secs(2))).await;
                continue;
            }

            let maybe_wait = {
                let mut state = self.state.lock();
                if state.tokens >= 1.0 {
                    state.tokens -= 1.0;
                    JITO_METRICS
                        .tokens_available_milli
                        .store((state.tokens * 1000.0).max(0.0) as u64, Ordering::Relaxed);
                    JITO_METRICS.limiter_dynamic_rps_milli.store(
                        (state.current_rps * 1000.0).max(0.0) as u64,
                        Ordering::Relaxed,
                    );
                    JITO_METRICS
                        .limiter_dynamic_concurrency
                        .store(state.current_concurrency as u64, Ordering::Relaxed);
                    None
                } else {
                    let secs = ((1.0 - state.tokens) / state.current_rps).max(0.005);
                    Some(Duration::from_secs_f64(secs))
                }
            };

            if let Some(wait) = maybe_wait {
                tokio::time::sleep(wait).await;
                continue;
            }

            return Ok(());
        }
    }

    fn on_success(&self) {
        self.consecutive_429.store(0, Ordering::Relaxed);
        let mut state = self.state.lock();
        state.success_streak = state.success_streak.saturating_add(1);
        if state.success_streak >= SUCCESS_STEP_FOR_INCREASE {
            state.success_streak = 0;
            state.current_rps = (state.current_rps + 0.5).min(state.max_rps);
            if state.current_concurrency < state.max_concurrency {
                state.current_concurrency += 1;
            }
        }
        JITO_METRICS.limiter_dynamic_rps_milli.store(
            (state.current_rps * 1000.0).max(0.0) as u64,
            Ordering::Relaxed,
        );
        JITO_METRICS
            .limiter_dynamic_concurrency
            .store(state.current_concurrency as u64, Ordering::Relaxed);
    }

    fn on_429(&self, retry_after: Option<Duration>) {
        if EMERGENCY_COUNTER_RESET.swap(false, Ordering::Relaxed) {
            self.consecutive_429.store(0, Ordering::Relaxed);
        }
        let count = self.consecutive_429.fetch_add(1, Ordering::Relaxed) + 1;
        let exp_backoff_ms = 2000_u64.saturating_mul(2_u64.pow(count.saturating_sub(1).min(4)));
        let exp_backoff = Duration::from_millis(exp_backoff_ms.min(30_000));
        let cooldown = retry_after.unwrap_or(exp_backoff).max(exp_backoff);

        {
            let mut state = self.state.lock();
            let target = Instant::now() + cooldown;
            state.cooldown_until = Some(match state.cooldown_until {
                Some(current) if current > target => current,
                _ => target,
            });
            state.tokens = state.tokens.min(1.0);
            state.success_streak = 0;
            state.current_rps = (state.current_rps * 0.7).max(state.min_rps);
            if state.current_concurrency > state.min_concurrency {
                state.current_concurrency -= 1;
            }
            JITO_METRICS.limiter_dynamic_rps_milli.store(
                (state.current_rps * 1000.0).max(0.0) as u64,
                Ordering::Relaxed,
            );
            JITO_METRICS
                .limiter_dynamic_concurrency
                .store(state.current_concurrency as u64, Ordering::Relaxed);
        }

        JITO_METRICS
            .limiter_cooldown_active
            .store(1, Ordering::Relaxed);

        if count >= MAX_CONSECUTIVE_429_BEFORE_STOP {
            trigger_jito_emergency_stop("consecutive 429 threshold exceeded");
        }
    }

    fn on_server_error(&self) {
        let mut state = self.state.lock();
        state.success_streak = 0;
        state.current_rps = (state.current_rps * 0.9).max(state.min_rps);
        JITO_METRICS.limiter_dynamic_rps_milli.store(
            (state.current_rps * 1000.0).max(0.0) as u64,
            Ordering::Relaxed,
        );
    }

    fn on_client_error_non429(&self) {
        let mut state = self.state.lock();
        state.success_streak = 0;
        if state.current_concurrency > state.min_concurrency {
            state.current_concurrency -= 1;
        }
        JITO_METRICS
            .limiter_dynamic_concurrency
            .store(state.current_concurrency as u64, Ordering::Relaxed);
    }
}

fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let raw = value?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(secs.min(300)))
}

fn is_retryable_client_status(status_code: u16) -> bool {
    matches!(status_code, 408 | 409 | 425)
}

fn classify_status(status: reqwest::StatusCode) {
    if status.is_success() {
        JITO_METRICS
            .request_2xx_total
            .fetch_add(1, Ordering::Relaxed);
    } else if status.as_u16() == 400 {
        JITO_METRICS
            .request_400_total
            .fetch_add(1, Ordering::Relaxed);
        JITO_METRICS
            .request_4xx_non429_total
            .fetch_add(1, Ordering::Relaxed);
    } else if status.as_u16() == 429 {
        JITO_METRICS
            .request_429_total
            .fetch_add(1, Ordering::Relaxed);
    } else if status.is_client_error() {
        let status_code = status.as_u16();
        JITO_METRICS
            .request_4xx_non429_total
            .fetch_add(1, Ordering::Relaxed);
        if is_retryable_client_status(status_code) {
            JITO_METRICS
                .request_4xx_retryable_total
                .fetch_add(1, Ordering::Relaxed);
        }
    } else if status.is_server_error() {
        JITO_METRICS
            .request_5xx_total
            .fetch_add(1, Ordering::Relaxed);
    } else {
        JITO_METRICS
            .request_other_total
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn build_rate_limiter() -> Arc<JitoRateLimiter> {
    let max_rps = std::env::var("JITO_MAX_RPS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MAX_RPS);
    let min_rps = std::env::var("JITO_MIN_RPS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_MIN_RPS)
        .min(max_rps)
        .min(DEFAULT_MAX_RPS_DYNAMIC);
    let bounded_max_rps = max_rps.max(min_rps).min(DEFAULT_MAX_RPS_DYNAMIC);
    let burst = std::env::var("JITO_BURST_TOKENS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(DEFAULT_BURST_TOKENS);
    let max_concurrency = std::env::var("JITO_MAX_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
        .min(DEFAULT_MAX_CONCURRENCY_DYNAMIC);
    let min_concurrency = std::env::var("JITO_MIN_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MIN_CONCURRENCY)
        .min(max_concurrency);
    let max_queue_depth = std::env::var("JITO_MAX_QUEUE_DEPTH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_QUEUE_DEPTH);

    Arc::new(JitoRateLimiter::new(
        min_rps,
        bounded_max_rps,
        burst,
        min_concurrency,
        max_concurrency,
        max_queue_depth,
    ))
}

const TOTAL_SEND_TIMEOUT_MS: u64 = 2500;

struct EndpointEntry {
    name: String,
    bundle_url: String,
    health: AtomicU32,
}

struct EndpointPool {
    entries: Vec<EndpointEntry>,
    current: AtomicUsize,
}

impl EndpointPool {
    fn extract_host(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }

        let parsed = if trimmed.contains("://") {
            Url::parse(trimmed).ok()
        } else {
            Url::parse(&format!("https://{}", trimmed.trim_start_matches('/'))).ok()
        }?;

        parsed.host_str().map(|h| h.to_ascii_lowercase())
    }

    fn to_bundle_url(raw: &str) -> String {
        let base = if raw.contains("://") {
            raw.trim().to_string()
        } else {
            format!("https://{}", raw.trim().trim_start_matches('/'))
        };
        if base.ends_with("/api/v1/bundles") {
            base
        } else {
            format!("{}/api/v1/bundles", base.trim_end_matches('/'))
        }
    }

    fn new(primary_url: Option<&str>) -> Self {
        let known_hosts: &[(&str, &str)] = &[
            ("frankfurt", JITO_FRANKFURT),
            ("amsterdam", JITO_AMSTERDAM),
        ];
        let mut entries: Vec<EndpointEntry> = known_hosts
            .iter()
            .map(|(name, host)| EndpointEntry {
                name: name.to_string(),
                bundle_url: format!("https://{}/api/v1/bundles", host),
                health: AtomicU32::new(100),
            })
            .collect();

        // If primary_url host does not exactly match a known region host,
        // insert it as entry[0] so it becomes the preferred primary.
        let start = if let Some(url) = primary_url {
            let maybe_host = Self::extract_host(url);
            if let Some(pos) = maybe_host
                .as_deref()
                .and_then(|h| known_hosts.iter().position(|(_, host)| h == *host))
            {
                pos
            } else {
                // Custom endpoint — prepend as primary
                let custom_bundle_url = Self::to_bundle_url(url);
                entries.insert(
                    0,
                    EndpointEntry {
                        name: "custom".to_string(),
                        bundle_url: custom_bundle_url,
                        health: AtomicU32::new(100),
                    },
                );
                0
            }
        } else {
            0
        };

        Self {
            entries,
            current: AtomicUsize::new(start),
        }
    }

    fn pick(&self) -> (usize, &str, &str) {
        let base = self.current.load(Ordering::Relaxed);
        let len = self.entries.len();
        let mut best_idx = base % len;
        let mut best_score = self.entries[best_idx].health.load(Ordering::Relaxed);
        for i in 1..len {
            let idx = (base + i) % len;
            let s = self.entries[idx].health.load(Ordering::Relaxed);
            if s > best_score {
                best_score = s;
                best_idx = idx;
            }
        }
        let e = &self.entries[best_idx];
        (best_idx, &e.name, &e.bundle_url)
    }

    fn on_success(&self, idx: usize) {
        if idx < self.entries.len() {
            let h = &self.entries[idx].health;
            let _ = h.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(10).min(100))
            });
        }
    }

    fn on_429(&self, idx: usize) {
        if idx < self.entries.len() {
            let h = &self.entries[idx].health;
            let _ = h.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(30))
            });
            self.current
                .store((idx + 1) % self.entries.len(), Ordering::Relaxed);
        }
    }

    fn on_error(&self, idx: usize) {
        if idx < self.entries.len() {
            let h = &self.entries[idx].health;
            let _ = h.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(10))
            });
        }
    }

    fn health_summary(&self) -> String {
        self.entries
            .iter()
            .map(|e| format!("{}:{}", e.name, e.health.load(Ordering::Relaxed)))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub struct JitoSender {
    endpoint: String,
    bundle_url: String,
    tip_lamports: u64,
    auth_uuid: Option<String>,
    dont_front: bool,
    client: reqwest::Client,
    rate_limiter: Arc<JitoRateLimiter>,
    endpoint_pool: EndpointPool,
}

impl JitoSender {
    pub fn new(
        endpoint: Option<String>,
        tip_lamports: Option<u64>,
        auth_uuid: Option<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("Failed to build reqwest client");

        let endpoint = endpoint.unwrap_or_else(|| format!("https://{}", JITO_FRANKFURT));
        let bundle_url = format!("{}/api/v1/bundles", endpoint);
        let endpoint_pool = EndpointPool::new(Some(&endpoint));

        Self {
            endpoint,
            bundle_url,
            tip_lamports: tip_lamports.unwrap_or(DEFAULT_JITO_TIP),
            auth_uuid,
            dont_front: false,
            client,
            rate_limiter: build_rate_limiter(),
            endpoint_pool,
        }
    }

    pub fn with_region(region: &str) -> Self {
        let endpoint_host = match region.to_lowercase().as_str() {
            "frankfurt" | "eu" => JITO_FRANKFURT,
            "tokyo" | "asia" => JITO_TOKYO,
            "amsterdam" => JITO_AMSTERDAM,
            "ny" | "us" => JITO_NY,
            _ => JITO_FRANKFURT,
        };

        let endpoint = format!("https://{}", endpoint_host);
        let bundle_url = format!("{}/api/v1/bundles", endpoint);
        let endpoint_pool = EndpointPool::new(Some(&endpoint));

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("Failed to build reqwest client");

        Self {
            endpoint,
            bundle_url,
            tip_lamports: DEFAULT_JITO_TIP,
            auth_uuid: None,
            dont_front: false,
            client,
            rate_limiter: build_rate_limiter(),
            endpoint_pool,
        }
    }

    pub fn set_auth_uuid(&mut self, uuid: String) {
        self.auth_uuid = Some(uuid);
    }

    pub fn set_tip(&mut self, lamports: u64) {
        self.tip_lamports = lamports;
    }

    pub fn set_dont_front(&mut self, dont_front: bool) {
        self.dont_front = dont_front;
    }

    pub fn create_tip_instruction(&self, payer: &Pubkey) -> Instruction {
        let tip_account = self.select_tip_account();
        system_instruction::transfer(payer, &tip_account, self.tip_lamports)
    }

    fn select_tip_account(&self) -> Pubkey {
        let index = crate::time::monotonic_now_us() as usize % JITO_TIP_ACCOUNTS.len();
        JITO_TIP_ACCOUNTS[index]
    }

    pub async fn send_bundle(&self, transactions: Vec<VersionedTransaction>) -> Result<String> {
        let encoded_txs = self.encode_transactions(&transactions)?;
        self.send_bundle_encoded_with_retry(encoded_txs, DEFAULT_RETRY_ATTEMPTS)
            .await
    }

    pub async fn send_bundle_encoded(&self, encoded_transactions: Vec<String>) -> Result<String> {
        self.send_bundle_encoded_with_retry(encoded_transactions, DEFAULT_RETRY_ATTEMPTS)
            .await
    }

    fn encode_transactions(&self, transactions: &[VersionedTransaction]) -> Result<Vec<String>> {
        if transactions.is_empty() {
            anyhow::bail!("Bundle cannot be empty");
        }

        if transactions.len() > 5 {
            anyhow::bail!("Bundle cannot contain more than 5 transactions");
        }

        let mut encoded_txs = Vec::with_capacity(transactions.len());
        for (i, tx) in transactions.iter().enumerate() {
            let serialized =
                bincode::serialize(tx).context(format!("Failed to serialize transaction {}", i))?;

            if serialized.len() > MAX_TX_SIZE {
                anyhow::bail!(
                    "Transaction {} exceeds max size: {} > {} bytes",
                    i,
                    serialized.len(),
                    MAX_TX_SIZE
                );
            }

            encoded_txs.push(BASE64.encode(&serialized));
        }

        Ok(encoded_txs)
    }

    async fn send_bundle_encoded_with_retry(
        &self,
        encoded_transactions: Vec<String>,
        max_attempts: u32,
    ) -> Result<String> {
        if encoded_transactions.is_empty() {
            anyhow::bail!("Bundle cannot be empty");
        }

        if encoded_transactions.len() > 5 {
            anyhow::bail!("Bundle cannot contain more than 5 transactions");
        }

        let attempts = max_attempts.max(1);
        let mut last_error: Option<anyhow::Error> = None;
        let deadline = Instant::now() + Duration::from_millis(TOTAL_SEND_TIMEOUT_MS);

        for attempt in 1..=attempts {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                debug!(
                    "send_bundle time cap reached ({}ms), aborting at attempt {}/{}",
                    TOTAL_SEND_TIMEOUT_MS, attempt, attempts
                );
                break;
            }

            let (ep_idx, ep_name, ep_url) = self.endpoint_pool.pick();
            let send_result = tokio::time::timeout(
                remaining,
                self.send_bundle_encoded_once(encoded_transactions.clone(), ep_url, ep_name),
            )
            .await;

            match send_result {
                Ok(Ok(bundle_id)) => {
                    self.endpoint_pool.on_success(ep_idx);
                    return Ok(bundle_id);
                }
                Err(_elapsed) => {
                    // tokio::time::timeout fired — treat as transport error
                    self.endpoint_pool.on_error(ep_idx);
                    warn!(
                        "Bundle attempt {}/{} timed out via {} after {}ms (pool: {})",
                        attempt,
                        attempts,
                        ep_name,
                        TOTAL_SEND_TIMEOUT_MS,
                        self.endpoint_pool.health_summary()
                    );
                    last_error = Some(anyhow::anyhow!(
                        "send_bundle timed out ({}ms cap)",
                        TOTAL_SEND_TIMEOUT_MS
                    ));
                    break; // deadline exhausted, no point retrying
                }
                Ok(Err(err)) => {
                    let mut retry_after = None;
                    let mut retryable = false;
                    let mut rely_on_limiter_cooldown = false;

                    if let Some(send_err) = err.downcast_ref::<JitoSendError>() {
                        match send_err {
                            JitoSendError::RateLimited { retry_after_ms, .. } => {
                                retryable = true;
                                retry_after = retry_after_ms.map(Duration::from_millis);
                                rely_on_limiter_cooldown = true;
                                self.endpoint_pool.on_429(ep_idx);
                            }
                            JitoSendError::HttpStatus { status, .. } => {
                                retryable = *status >= 500 || is_retryable_client_status(*status);
                                self.endpoint_pool.on_error(ep_idx);
                            }
                            JitoSendError::Transport(_) | JitoSendError::QueueFull(_) => {
                                retryable = true;
                                self.endpoint_pool.on_error(ep_idx);
                            }
                            JitoSendError::EmergencyStop(_) | JitoSendError::BadRequest(_) => {
                                retryable = false;
                            }
                            JitoSendError::ResponseParse(_) => {
                                retryable = false;
                            }
                        }
                    }

                    if !retryable || attempt == attempts {
                        return Err(err);
                    }

                    let exp_backoff_ms = 150_u64.saturating_mul(2_u64.pow((attempt - 1).min(6)));
                    let jitter_ms = ((attempt as u64) * 37) % 97;
                    let delay = if rely_on_limiter_cooldown {
                        let base_ms = exp_backoff_ms.max(2000) + jitter_ms;
                        retry_after.unwrap_or(Duration::from_millis(base_ms))
                    } else {
                        retry_after.unwrap_or(Duration::from_millis(exp_backoff_ms + jitter_ms))
                    };

                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let clamped_delay = delay.min(remaining);

                    JITO_METRICS.retries_total.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        "Bundle attempt {}/{} failed via {}: {}. retrying in {}ms (pool: {})",
                        attempt,
                        attempts,
                        ep_name,
                        err,
                        clamped_delay.as_millis(),
                        self.endpoint_pool.health_summary()
                    );

                    last_error = Some(err);
                    if !clamped_delay.is_zero() {
                        let backoff_sleep_start_ts_us = unix_time_us();
                        tokio::time::sleep(clamped_delay).await;
                        let backoff_sleep_end_ts_us = unix_time_us();
                        let backoff_us =
                            backoff_sleep_end_ts_us.saturating_sub(backoff_sleep_start_ts_us);
                        JITO_METRICS
                            .backoff_sleep_events_total
                            .fetch_add(1, Ordering::Relaxed);
                        JITO_METRICS
                            .backoff_sleep_total_us
                            .fetch_add(backoff_us, Ordering::Relaxed);
                        debug!(
                            target: "latency_backoff_metrics",
                            "{{\"backoff_sleep_start_ts_us\":{},\"backoff_sleep_end_ts_us\":{},\"backoff_latency_us\":{}}}",
                            backoff_sleep_start_ts_us,
                            backoff_sleep_end_ts_us,
                            backoff_us
                        );
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Max retries exceeded")))
    }

    async fn send_bundle_encoded_once(
        &self,
        encoded_transactions: Vec<String>,
        bundle_url: &str,
        ep_name: &str,
    ) -> Result<String> {
        if is_jito_emergency_stop_active() {
            return Err(
                JitoSendError::EmergencyStop("send blocked by emergency_stop".to_string()).into(),
            );
        }

        let _permit = self
            .rate_limiter
            .acquire_slot()
            .await
            .map_err(anyhow::Error::new)?;

        JITO_METRICS.requests_total.fetch_add(1, Ordering::Relaxed);

        debug!(
            "Sending encoded bundle with {} txs to {} ({})",
            encoded_transactions.len(),
            ep_name,
            bundle_url
        );

        let request = JitoBundleRequestV2 {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "sendBundle".to_string(),
            params: (
                encoded_transactions,
                JitoEncoding {
                    encoding: "base64".to_string(),
                    jitodontfront: if self.dont_front { Some(true) } else { None },
                },
            ),
        };

        let mut request_builder = self
            .client
            .post(bundle_url)
            .header("Content-Type", "application/json");

        if let Some(ref uuid) = self.auth_uuid {
            debug!("Using authenticated Jito API with UUID");
            request_builder = request_builder.header("x-jito-auth", uuid);
        }

        let response = request_builder
            .json(&request)
            .send()
            .await
            .map_err(|e| JitoSendError::Transport(e.to_string()))?;

        let status = response.status();
        let retry_after = parse_retry_after(
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
        );
        let body = response
            .text()
            .await
            .map_err(|e| JitoSendError::Transport(e.to_string()))?;

        classify_status(status);
        debug!("Jito response status: {}, body: {}", status, body);

        if status.as_u16() == 429 {
            JITO_METRICS.jito_429_total.fetch_add(1, Ordering::Relaxed);
            self.rate_limiter.on_429(retry_after);
            return Err(JitoSendError::RateLimited {
                body,
                retry_after_ms: retry_after.map(|d| d.as_millis() as u64),
            }
            .into());
        }

        if status.as_u16() == 400 {
            JITO_METRICS.jito_400_total.fetch_add(1, Ordering::Relaxed);
            warn!("Jito 400 via {}: {}", ep_name, body);
            return Err(JitoSendError::BadRequest(body).into());
        }

        if !status.is_success() {
            if status.is_server_error() {
                self.rate_limiter.on_server_error();
            } else if status.is_client_error() && status.as_u16() != 429 {
                self.rate_limiter.on_client_error_non429();
            }
            return Err(JitoSendError::HttpStatus {
                status: status.as_u16(),
                body,
            }
            .into());
        }

        let response: JitoBundleResponse =
            serde_json::from_str(&body).map_err(|e| JitoSendError::ResponseParse(e.to_string()))?;

        if let Some(error) = response.error {
            if error.code == 429 {
                JITO_METRICS.jito_429_total.fetch_add(1, Ordering::Relaxed);
                self.rate_limiter.on_429(retry_after);
                return Err(JitoSendError::RateLimited {
                    body: error.message,
                    retry_after_ms: retry_after.map(|d| d.as_millis() as u64),
                }
                .into());
            }

            if error.code == 400 {
                JITO_METRICS.jito_400_total.fetch_add(1, Ordering::Relaxed);
                self.rate_limiter.on_client_error_non429();
                warn!("Jito RPC-400 via {}: {}", ep_name, error.message);
                return Err(JitoSendError::BadRequest(error.message).into());
            }

            if (500..=599).contains(&(error.code as u16)) {
                self.rate_limiter.on_server_error();
            } else if (400..=499).contains(&(error.code as u16)) {
                self.rate_limiter.on_client_error_non429();
            }

            return Err(JitoSendError::HttpStatus {
                status: status.as_u16(),
                body: format!("Jito bundle error {}: {}", error.code, error.message),
            }
            .into());
        }

        self.rate_limiter.on_success();
        let bundle_id = response.result.unwrap_or_else(|| "unknown".to_string());
        info!("Bundle submitted via {}: {}", ep_name, bundle_id);

        Ok(bundle_id)
    }

    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<BundleStatus> {
        let request = JitoStatusRequest {
            jsonrpc: "2.0".to_string(),
            id: 1,
            method: "getBundleStatuses".to_string(),
            params: vec![vec![bundle_id.to_string()]],
        };

        let response = self
            .client
            .post(&self.bundle_url)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let body = response.text().await?;
        let response: JitoBundleStatusResponse = serde_json::from_str(&body)?;

        if let Some(statuses) = response.result {
            if let Some(status) = statuses.value.first() {
                return Ok(status.clone());
            }
        }

        Ok(BundleStatus {
            bundle_id: bundle_id.to_string(),
            transactions: vec![],
            slot: 0,
            confirmation_status: Some("unknown".to_string()),
            err: None,
        })
    }

    pub async fn send_bundle_with_retry(
        &self,
        transactions: Vec<VersionedTransaction>,
        max_retries: u32,
    ) -> Result<String> {
        let encoded_txs = self.encode_transactions(&transactions)?;
        self.send_bundle_encoded_with_retry(encoded_txs, max_retries)
            .await
    }

    /// Option 1: Jito sendTransaction endpoint (/api/v1/transactions?bundleOnly=true)
    /// Separate rate limit bucket from sendBundle. Single TX with revert protection.
    pub async fn send_transaction_jito(
        &self,
        tx: &VersionedTransaction,
    ) -> Result<String> {
        if is_jito_emergency_stop_active() {
            anyhow::bail!("send blocked by emergency_stop");
        }

        let _permit = self
            .rate_limiter
            .acquire_slot()
            .await
            .map_err(anyhow::Error::new)?;

        JITO_METRICS.requests_total.fetch_add(1, Ordering::Relaxed);

        let serialized = bincode::serialize(tx).context("Failed to serialize tx")?;
        let encoded = bs58::encode(&serialized).into_string();

        let (ep_idx, ep_name, ep_bundle_url) = self.endpoint_pool.pick();
        // /api/v1/bundles → /api/v1/transactions?bundleOnly=true
        let tx_url = ep_bundle_url
            .replace("/api/v1/bundles", "/api/v1/transactions?bundleOnly=true");

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [encoded, {"encoding": "base58"}]
        });

        let mut request_builder = self
            .client
            .post(&tx_url)
            .header("Content-Type", "application/json");

        if let Some(ref uuid) = self.auth_uuid {
            request_builder = request_builder.header("x-jito-auth", uuid);
        }

        let result = tokio::time::timeout(
            Duration::from_millis(TOTAL_SEND_TIMEOUT_MS),
            request_builder.json(&request).send(),
        )
        .await;

        let response = match result {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                self.endpoint_pool.on_error(ep_idx);
                anyhow::bail!("Jito sendTransaction transport error via {}: {}", ep_name, e);
            }
            Err(_) => {
                self.endpoint_pool.on_error(ep_idx);
                anyhow::bail!("Jito sendTransaction timed out via {} ({}ms)", ep_name, TOTAL_SEND_TIMEOUT_MS);
            }
        };

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if status.as_u16() == 429 {
            JITO_METRICS.jito_429_total.fetch_add(1, Ordering::Relaxed);
            self.rate_limiter.on_429(None);
            self.endpoint_pool.on_429(ep_idx);
            anyhow::bail!("Jito sendTransaction 429 via {}: {}", ep_name, body);
        }

        if !status.is_success() {
            self.endpoint_pool.on_error(ep_idx);
            anyhow::bail!("Jito sendTransaction {} via {}: {}", status, ep_name, body);
        }

        self.endpoint_pool.on_success(ep_idx);
        self.rate_limiter.on_success();

        // Response: {"jsonrpc":"2.0","result":"<tx_signature>","id":1}
        let resp: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let sig = resp["result"].as_str().unwrap_or("unknown").to_string();
        info!("Jito sendTransaction OK via {}: {}", ep_name, sig);
        Ok(sig)
    }

    pub fn get_endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn get_tip_lamports(&self) -> u64 {
        self.tip_lamports
    }

    pub fn get_client(&self) -> &reqwest::Client {
        &self.client
    }
}

pub fn record_preflight_fail() {
    JITO_METRICS
        .preflight_fail_total
        .fetch_add(1, Ordering::Relaxed);
}

pub fn preflight_fail_total() -> u64 {
    JITO_METRICS.preflight_fail_total.load(Ordering::Relaxed)
}

/// Validate that at least one Jito tip account is write-locked in the transaction.
/// Returns false if no tip account is writable — Jito will reject with 400.
pub fn validate_bundle_tip_lock(tx: &VersionedTransaction) -> bool {
    use solana_sdk::message::VersionedMessage;
    let (keys, header) = match &tx.message {
        VersionedMessage::Legacy(msg) => (&msg.account_keys, &msg.header),
        VersionedMessage::V0(msg) => (&msg.account_keys, &msg.header),
    };
    let num_sigs = header.num_required_signatures as usize;
    let num_ro_signed = header.num_readonly_signed_accounts as usize;
    let num_ro_unsigned = header.num_readonly_unsigned_accounts as usize;
    let writable_signer_end = num_sigs.saturating_sub(num_ro_signed);
    let writable_nonsigner_end = keys.len().saturating_sub(num_ro_unsigned);
    for i in 0..writable_signer_end {
        if JITO_TIP_ACCOUNTS.contains(&keys[i]) {
            return true;
        }
    }
    for i in num_sigs..writable_nonsigner_end {
        if JITO_TIP_ACCOUNTS.contains(&keys[i]) {
            return true;
        }
    }
    // Diagnostic: log which accounts are writable and check tip presence anywhere
    let has_tip_anywhere = keys.iter().any(|k| JITO_TIP_ACCOUNTS.contains(k));
    warn!(
        "tip_lock_fail: keys={} sigs={} ro_signed={} ro_unsigned={} writable_signer=[0..{}) writable_nonsigner=[{}..{}) tip_in_keys={}",
        keys.len(), num_sigs, num_ro_signed, num_ro_unsigned,
        writable_signer_end, num_sigs, writable_nonsigner_end, has_tip_anywhere
    );
    false
}

#[derive(Debug, Serialize)]
struct JitoBundleRequestV2 {
    jsonrpc: String,
    id: u64,
    method: String,
    params: (Vec<String>, JitoEncoding),
}

#[derive(Debug, Serialize)]
struct JitoEncoding {
    encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    jitodontfront: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct JitoBundleResponse {
    result: Option<String>,
    error: Option<JitoError>,
}

#[derive(Debug, Deserialize)]
struct JitoError {
    code: i32,
    message: String,
}

#[derive(Debug, Serialize)]
struct JitoStatusRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    params: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct JitoBundleStatusResponse {
    result: Option<BundleStatusResult>,
}

#[derive(Debug, Deserialize)]
struct BundleStatusResult {
    value: Vec<BundleStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleStatus {
    pub bundle_id: String,
    pub transactions: Vec<String>,
    pub slot: u64,
    pub confirmation_status: Option<String>,
    pub err: Option<serde_json::Value>,
}

pub struct DynamicTipCalculator {
    rpc_client: Arc<RpcClient>,
    min_tip: u64,
    max_tip: u64,
    cached_tip: Mutex<Option<(u64, Instant)>>,
    cache_duration: Duration,
}

impl DynamicTipCalculator {
    pub fn new(rpc_url: &str, min_tip: u64, max_tip: u64) -> Self {
        Self {
            rpc_client: Arc::new(RpcClient::new(rpc_url.to_string())),
            min_tip,
            max_tip,
            cached_tip: Mutex::new(None),
            cache_duration: Duration::from_secs(5),
        }
    }

    pub fn calculate_tip(&self, urgency: TipUrgency) -> u64 {
        {
            let cache = self.cached_tip.lock();
            if let Some((tip, timestamp)) = *cache {
                if timestamp.elapsed() < self.cache_duration {
                    return self.adjust_for_urgency(tip, urgency);
                }
            }
        }

        let fees = match self.rpc_client.get_recent_prioritization_fees(&[]) {
            Ok(f) => f,
            Err(_) => return self.min_tip,
        };

        if fees.is_empty() {
            return self.min_tip;
        }

        let mut values: Vec<u64> = fees.iter().map(|f| f.prioritization_fee).collect();
        values.sort();
        let median = values[values.len() / 2];

        {
            let mut cache = self.cached_tip.lock();
            *cache = Some((median, Instant::now()));
        }

        self.adjust_for_urgency(median, urgency)
    }

    fn adjust_for_urgency(&self, base: u64, urgency: TipUrgency) -> u64 {
        let adjusted = match urgency {
            TipUrgency::Low => base / 2,
            TipUrgency::Normal => base,
            TipUrgency::High => base * 2,
            TipUrgency::Critical => base * 5,
        };
        adjusted.clamp(self.min_tip, self.max_tip)
    }
}

#[derive(Clone, Copy)]
pub enum TipUrgency {
    Low,
    Normal,
    High,
    Critical,
}

/// Multi-region Jito sender for concurrent bundle submission.
/// Sends bundles to all regions simultaneously to maximize landing rate.
pub struct MultiRegionJitoSender {
    senders: Vec<(String, JitoSender)>,
}

impl MultiRegionJitoSender {
    /// Create a new multi-region sender with all Jito regions.
    pub fn new(tip_lamports: Option<u64>, auth_uuid: Option<String>) -> Self {
        let regions = vec![
            ("frankfurt", JITO_FRANKFURT),
            ("amsterdam", JITO_AMSTERDAM),
        ];

        let senders = regions
            .into_iter()
            .map(|(name, _)| {
                let sender = JitoSender::new(
                    Some(format!(
                        "https://{}",
                        match name {
                            "frankfurt" => JITO_FRANKFURT,
                            "amsterdam" => JITO_AMSTERDAM,
                            _ => JITO_FRANKFURT,
                        }
                    )),
                    tip_lamports,
                    auth_uuid.clone(),
                );
                (name.to_string(), sender)
            })
            .collect();

        Self { senders }
    }

    /// Send bundle to all regions concurrently.
    /// Returns the first successful bundle ID and the region that landed it.
    pub async fn send_bundle_all_regions(
        &self,
        transactions: Vec<VersionedTransaction>,
    ) -> Result<Vec<(String, String)>> {
        if transactions.is_empty() {
            anyhow::bail!("Bundle cannot be empty");
        }

        // Serialize transactions once
        let mut encoded_txs = Vec::with_capacity(transactions.len());
        for (i, tx) in transactions.iter().enumerate() {
            let serialized =
                bincode::serialize(tx).context(format!("Failed to serialize transaction {}", i))?;

            if serialized.len() > MAX_TX_SIZE {
                anyhow::bail!(
                    "Transaction {} exceeds max size: {} > {} bytes",
                    i,
                    serialized.len(),
                    MAX_TX_SIZE
                );
            }

            encoded_txs.push(BASE64.encode(&serialized));
        }

        // Send to all regions concurrently
        let futures: Vec<_> = self
            .senders
            .iter()
            .map(|(region, sender)| {
                let region = region.clone();
                let encoded = encoded_txs.clone();
                async move {
                    match sender.send_bundle_encoded(encoded).await {
                        Ok(bundle_id) => {
                            info!("[JITO] Bundle landed via {}: {}", region, bundle_id);
                            Ok((region, bundle_id))
                        }
                        Err(e) => {
                            warn!("[JITO] Failed to send to {}: {}", region, e);
                            Err(e)
                        }
                    }
                }
            })
            .collect();

        let results = futures::future::join_all(futures).await;

        let successful: Vec<_> = results.into_iter().filter_map(|r| r.ok()).collect();

        if successful.is_empty() {
            anyhow::bail!("Failed to send bundle to any Jito region");
        }

        info!("[JITO] Bundle sent to {} regions", successful.len());
        Ok(successful)
    }

    /// Get all configured regions.
    pub fn regions(&self) -> Vec<&str> {
        self.senders.iter().map(|(r, _)| r.as_str()).collect()
    }
}

/// Profit-based tip calculator (Claude document optimization).
/// Calculates optimal tip as a percentage of expected profit.
pub struct ProfitBasedTipCalculator {
    base_ratio: f64, // Default: 0.15 (15% of profit)
    min_tip: u64,    // Minimum tip in lamports
    max_ratio: f64,  // Maximum ratio: 0.40 (40% of profit)
}

impl ProfitBasedTipCalculator {
    pub fn new(base_ratio: f64, min_tip: u64, max_ratio: f64) -> Self {
        Self {
            base_ratio: base_ratio.clamp(0.05, 0.50),
            min_tip,
            max_ratio: max_ratio.clamp(0.10, 0.50),
        }
    }

    /// Calculate optimal tip based on expected profit and competition level.
    pub fn calculate(&self, expected_profit_lamports: u64, competition: CompetitionLevel) -> u64 {
        let competition_multiplier = match competition {
            CompetitionLevel::Low => 0.8,
            CompetitionLevel::Medium => 1.0,
            CompetitionLevel::High => 1.3,
            CompetitionLevel::Extreme => 1.5,
        };

        let ratio = (self.base_ratio * competition_multiplier).min(self.max_ratio);
        let tip = (expected_profit_lamports as f64 * ratio) as u64;

        tip.max(self.min_tip)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CompetitionLevel {
    Low,     // < 5 competing bundles
    Medium,  // 5-15 bundles
    High,    // 15-30 bundles
    Extreme, // > 30 bundles
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;
    use solana_sdk::message::Message;
    use solana_sdk::signature::Keypair;
    use solana_sdk::signer::Signer;
    use solana_sdk::transaction::Transaction;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset_jito_state() {
        clear_jito_emergency_stop();
        JITO_METRICS.queue_depth.store(0, Ordering::Relaxed);
        JITO_METRICS.inflight_requests.store(0, Ordering::Relaxed);
        JITO_METRICS.dropped_total.store(0, Ordering::Relaxed);
        JITO_METRICS
            .limiter_cooldown_active
            .store(0, Ordering::Relaxed);
    }

    fn make_test_versioned_tx() -> VersionedTransaction {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let ix = system_instruction::transfer(&payer.pubkey(), &recipient, 1);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new(&[&payer], msg, Hash::new_unique());
        VersionedTransaction::from(tx)
    }

    #[test]
    fn test_jito_sender_new() {
        let sender = JitoSender::new(None, None, None);
        assert!(sender.endpoint.contains("frankfurt"));
        assert!(sender.bundle_url.contains("frankfurt"));
        assert!(sender.bundle_url.ends_with("/api/v1/bundles"));
        assert_eq!(sender.tip_lamports, DEFAULT_JITO_TIP);
        assert!(sender.auth_uuid.is_none());
    }

    #[test]
    fn test_jito_sender_with_auth() {
        let sender = JitoSender::new(None, None, Some("test-uuid-123".to_string()));
        assert!(sender.auth_uuid.is_some());
        assert_eq!(sender.auth_uuid.unwrap(), "test-uuid-123");
    }

    #[test]
    fn test_jito_sender_with_region() {
        let sender = JitoSender::with_region("tokyo");
        assert!(sender.endpoint.contains("tokyo"));
        assert!(sender.bundle_url.contains("tokyo"));
        assert!(sender.bundle_url.ends_with("/api/v1/bundles"));

        let sender = JitoSender::with_region("ny");
        assert!(sender.endpoint.contains("ny"));
        assert!(sender.bundle_url.contains("ny"));
    }

    #[test]
    fn test_create_tip_instruction() {
        let sender = JitoSender::new(None, Some(100_000), None);
        let payer = Pubkey::new_unique();
        let ix = sender.create_tip_instruction(&payer);

        assert_eq!(ix.program_id, solana_sdk::system_program::id());
        assert_eq!(ix.accounts.len(), 2);
        assert_eq!(ix.accounts[0].pubkey, payer);
    }

    #[test]
    fn test_tip_account_selection() {
        let sender = JitoSender::new(None, None, None);
        let tip_account = sender.select_tip_account();
        assert!(JITO_TIP_ACCOUNTS.contains(&tip_account));
    }

    #[test]
    fn test_consecutive_429_count() {
        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 2, 32);
        assert_eq!(limiter.consecutive_429.load(Ordering::Relaxed), 0);
        limiter.on_429(None);
        assert_eq!(limiter.consecutive_429.load(Ordering::Relaxed), 1);
        limiter.on_success();
        assert_eq!(limiter.consecutive_429.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_retryable_client_status_mapping() {
        assert!(is_retryable_client_status(408));
        assert!(is_retryable_client_status(409));
        assert!(is_retryable_client_status(425));
        assert!(!is_retryable_client_status(400));
        assert!(!is_retryable_client_status(404));
    }

    #[test]
    fn rate_limiter_starts_at_max_rps() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 2, 32);
        let state = limiter.state.lock();
        assert_eq!(state.current_rps, 4.0);
        assert_eq!(state.current_concurrency, 2);
    }

    #[test]
    fn rate_limiter_decreases_on_429() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 2, 32);
        let before = limiter.state.lock().current_rps;
        limiter.on_429(None);
        let after = limiter.state.lock().current_rps;
        assert!(after < before);
        assert!(after >= 1.0);
    }

    #[test]
    fn rate_limiter_increases_on_success_streak() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 3, 32);
        limiter.on_429(None);
        let before = limiter.state.lock().current_rps;
        for _ in 0..64 {
            limiter.on_success();
        }
        let after = limiter.state.lock().current_rps;
        assert!(after > before);
    }

    #[test]
    fn emergency_stop_activates_on_consecutive_429() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 2, 32);
        for _ in 0..50 {
            limiter.on_429(None);
        }
        assert!(is_jito_emergency_stop_active());
    }

    #[test]
    fn emergency_stop_can_be_cleared() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        trigger_jito_emergency_stop("test");
        assert!(is_jito_emergency_stop_active());
        clear_jito_emergency_stop();
        assert!(!is_jito_emergency_stop_active());
    }

    #[tokio::test]
    async fn queue_full_drops_request() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let limiter = JitoRateLimiter::new(1.0, 4.0, 4.0, 1, 2, 0);
        match limiter.acquire_slot().await {
            Err(JitoSendError::QueueFull(_)) => {}
            _ => panic!("expected QueueFull"),
        }
    }

    #[test]
    fn encode_transactions_rejects_empty_and_oversized_batch() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_jito_state();

        let sender = JitoSender::new(None, None, None);
        let err = sender.encode_transactions(&[]).unwrap_err();
        assert!(err.to_string().contains("Bundle cannot be empty"));

        let tx = make_test_versioned_tx();
        let too_many = vec![
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx.clone(),
            tx,
        ];
        let err = sender.encode_transactions(&too_many).unwrap_err();
        assert!(err.to_string().contains("more than 5"));
    }

    #[test]
    fn prometheus_metrics_format_valid() {
        let _guard = TEST_LOCK.lock().unwrap();
        let metrics = jito_prometheus_metrics();
        assert!(metrics.contains("# HELP jito_requests_total"));
        assert!(metrics.contains("jito_emergency_stop"));
    }

    #[test]
    fn regression_retry_attempts_must_be_two() {
        assert_eq!(
            DEFAULT_RETRY_ATTEMPTS, 2,
            "DEFAULT_RETRY_ATTEMPTS must be 2 — never regress to 3 or 4"
        );
    }

    #[test]
    fn validate_tip_lock_passes_with_jito_tip() {
        let payer = Keypair::new();
        let tip_account = JITO_TIP_ACCOUNTS[0];
        let ix = system_instruction::transfer(&payer.pubkey(), &tip_account, 100_000);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new(&[&payer], msg, Hash::new_unique());
        let vtx = VersionedTransaction::from(tx);
        assert!(
            validate_bundle_tip_lock(&vtx),
            "tx with Jito tip account must pass write-lock check"
        );
    }

    #[test]
    fn validate_tip_lock_fails_without_jito_tip() {
        let payer = Keypair::new();
        let random_recipient = Pubkey::new_unique();
        let ix = system_instruction::transfer(&payer.pubkey(), &random_recipient, 100_000);
        let msg = Message::new(&[ix], Some(&payer.pubkey()));
        let tx = Transaction::new(&[&payer], msg, Hash::new_unique());
        let vtx = VersionedTransaction::from(tx);
        assert!(
            !validate_bundle_tip_lock(&vtx),
            "tx without Jito tip account must fail write-lock check"
        );
    }

    #[test]
    fn endpoint_pool_rotates_on_429() {
        let pool = EndpointPool::new(None);
        let (idx, name, _) = pool.pick();
        assert_eq!(name, "frankfurt"); // default primary
        pool.on_429(idx);
        let (_, name2, _) = pool.pick();
        // After 429 on frankfurt (health 70), other endpoints at 100 should be picked
        assert_ne!(name2, "frankfurt");
    }

    #[test]
    fn endpoint_pool_preserves_custom_url() {
        let pool = EndpointPool::new(Some("https://my-private-relay.example.com"));
        assert_eq!(pool.entries.len(), 5); // custom + 4 known
        let (_, name, url) = pool.pick();
        assert_eq!(name, "custom");
        assert!(url.contains("my-private-relay.example.com"));
        assert!(url.ends_with("/api/v1/bundles"));
    }

    #[test]
    fn endpoint_pool_known_url_no_duplicate() {
        let pool = EndpointPool::new(Some("https://frankfurt.mainnet.block-engine.jito.wtf"));
        assert_eq!(pool.entries.len(), 4); // no extra entry
        let (_, name, _) = pool.pick();
        assert_eq!(name, "frankfurt");
    }

    #[test]
    fn endpoint_pool_no_substring_false_positive() {
        // "company" contains "ny" — must NOT match as known region
        let pool = EndpointPool::new(Some("https://company-relay.example.com"));
        assert_eq!(pool.entries.len(), 5); // custom + 4 known
        let (_, name, _) = pool.pick();
        assert_eq!(name, "custom");
    }

    #[test]
    fn endpoint_pool_no_path_query_false_positive() {
        // Known host string in path/query must not be treated as known region host.
        let pool = EndpointPool::new(Some(
            "https://my-relay.example.com/path/ny.mainnet.block-engine.jito.wtf?x=tokyo.mainnet.block-engine.jito.wtf",
        ));
        assert_eq!(pool.entries.len(), 5); // custom + 4 known
        let (_, name, url) = pool.pick();
        assert_eq!(name, "custom");
        assert!(url.starts_with("https://my-relay.example.com/"));
    }
}
