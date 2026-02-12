use parking_lot::Mutex;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

const DEFAULT_WINDOW_SECS: u64 = 600;
const MAX_SAMPLES_PER_SERIES: usize = 50_000;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RuntimeLatencySnapshot {
    pub detection_samples: u64,
    pub detection_p50_us: u64,
    pub detection_p99_us: u64,
    pub detection_jitter_us: u64,
    pub execution_samples: u64,
    pub execution_p99_us: u64,
    pub backoff_samples: u64,
    pub backoff_p99_us: u64,
    pub dropped_intents_total: u64,
    pub stale_drop_total: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RuntimeGateSnapshot {
    pub trade_enabled: bool,
    pub gate_mode: u8,
    pub gate_transitions_total: u64,
    pub gate_reason_code: u64,
    pub effective_success_milli_pct: u64,
    pub rate_limit_milli_pct: u64,
    pub client_error_milli_pct: u64,
    pub server_error_milli_pct: u64,
}

#[derive(Clone, Copy)]
struct TimedSample {
    ts: u64,
    value: u64,
}

#[derive(Default)]
struct RuntimeState {
    detection_latency_us: VecDeque<TimedSample>,
    execution_latency_us: VecDeque<TimedSample>,
    backoff_latency_us: VecDeque<TimedSample>,
    dropped_intents_total: u64,
    stale_drop_total: u64,
}

lazy_static::lazy_static! {
    static ref RUNTIME_STATE: Mutex<RuntimeState> = Mutex::new(RuntimeState::default());
}

// 0=normal, 1=throttled, 2=half_open, 3=blocked
static GATE_MODE: AtomicU8 = AtomicU8::new(0);
static TRADE_ENABLED: AtomicBool = AtomicBool::new(true);
static GATE_TRANSITIONS_TOTAL: AtomicU64 = AtomicU64::new(0);
static GATE_REASON_CODE: AtomicU64 = AtomicU64::new(0);

// Latest control-plane rates (milli-percent precision: 100% = 100_000)
static EFFECTIVE_SUCCESS_MILLI_PCT: AtomicU64 = AtomicU64::new(0);
static RATE_LIMIT_MILLI_PCT: AtomicU64 = AtomicU64::new(0);
static CLIENT_ERROR_MILLI_PCT: AtomicU64 = AtomicU64::new(0);
static SERVER_ERROR_MILLI_PCT: AtomicU64 = AtomicU64::new(0);

fn unix_secs() -> u64 {
    crate::time::monotonic_now_secs()
}

fn prune_window(samples: &mut VecDeque<TimedSample>, cutoff_secs: u64) {
    while let Some(front) = samples.front() {
        if front.ts < cutoff_secs {
            samples.pop_front();
        } else {
            break;
        }
    }

    if samples.len() > MAX_SAMPLES_PER_SERIES {
        let overflow = samples.len() - MAX_SAMPLES_PER_SERIES;
        for _ in 0..overflow {
            samples.pop_front();
        }
    }
}

fn percentile(values: &[u64], percentile_milli: u64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() - 1) as u64 * percentile_milli / 1000) as usize;
    sorted[idx]
}

pub fn record_detection_latency(latency_us: u64) {
    let mut state = RUNTIME_STATE.lock();
    state.detection_latency_us.push_back(TimedSample {
        ts: unix_secs(),
        value: latency_us,
    });
}

pub fn record_execution_latency(latency_us: u64) {
    let mut state = RUNTIME_STATE.lock();
    state.execution_latency_us.push_back(TimedSample {
        ts: unix_secs(),
        value: latency_us,
    });
}

pub fn record_backoff_latency(latency_us: u64) {
    let mut state = RUNTIME_STATE.lock();
    state.backoff_latency_us.push_back(TimedSample {
        ts: unix_secs(),
        value: latency_us,
    });
}

pub fn record_intent_drop() {
    let mut state = RUNTIME_STATE.lock();
    state.dropped_intents_total = state.dropped_intents_total.saturating_add(1);
}

pub fn record_stale_intent_drop() {
    let mut state = RUNTIME_STATE.lock();
    state.stale_drop_total = state.stale_drop_total.saturating_add(1);
    state.dropped_intents_total = state.dropped_intents_total.saturating_add(1);
}

pub fn latency_snapshot(window_secs: Option<u64>) -> RuntimeLatencySnapshot {
    let window_secs = window_secs.unwrap_or(DEFAULT_WINDOW_SECS);
    let now = unix_secs();
    let cutoff = now.saturating_sub(window_secs);

    let mut state = RUNTIME_STATE.lock();
    prune_window(&mut state.detection_latency_us, cutoff);
    prune_window(&mut state.execution_latency_us, cutoff);
    prune_window(&mut state.backoff_latency_us, cutoff);

    let detection_vals: Vec<u64> = state.detection_latency_us.iter().map(|s| s.value).collect();
    let execution_vals: Vec<u64> = state.execution_latency_us.iter().map(|s| s.value).collect();
    let backoff_vals: Vec<u64> = state.backoff_latency_us.iter().map(|s| s.value).collect();

    let detection_p50 = percentile(&detection_vals, 500);
    let detection_p99 = percentile(&detection_vals, 990);

    RuntimeLatencySnapshot {
        detection_samples: detection_vals.len() as u64,
        detection_p50_us: detection_p50,
        detection_p99_us: detection_p99,
        detection_jitter_us: detection_p99.saturating_sub(detection_p50),
        execution_samples: execution_vals.len() as u64,
        execution_p99_us: percentile(&execution_vals, 990),
        backoff_samples: backoff_vals.len() as u64,
        backoff_p99_us: percentile(&backoff_vals, 990),
        dropped_intents_total: state.dropped_intents_total,
        stale_drop_total: state.stale_drop_total,
    }
}

pub fn set_trade_enabled(enabled: bool) {
    TRADE_ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_trade_enabled() -> bool {
    TRADE_ENABLED.load(Ordering::SeqCst)
}

pub fn set_gate_mode(mode: u8) {
    let previous = GATE_MODE.swap(mode, Ordering::SeqCst);
    if previous != mode {
        GATE_TRANSITIONS_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn gate_mode() -> u8 {
    GATE_MODE.load(Ordering::SeqCst)
}

pub fn set_gate_reason_code(code: u64) {
    GATE_REASON_CODE.store(code, Ordering::Relaxed);
}

pub fn set_latest_error_rates(
    effective_success_milli_pct: u64,
    rate_limit_milli_pct: u64,
    client_error_milli_pct: u64,
    server_error_milli_pct: u64,
) {
    EFFECTIVE_SUCCESS_MILLI_PCT.store(effective_success_milli_pct, Ordering::Relaxed);
    RATE_LIMIT_MILLI_PCT.store(rate_limit_milli_pct, Ordering::Relaxed);
    CLIENT_ERROR_MILLI_PCT.store(client_error_milli_pct, Ordering::Relaxed);
    SERVER_ERROR_MILLI_PCT.store(server_error_milli_pct, Ordering::Relaxed);
}

pub fn gate_snapshot() -> RuntimeGateSnapshot {
    RuntimeGateSnapshot {
        trade_enabled: is_trade_enabled(),
        gate_mode: gate_mode(),
        gate_transitions_total: GATE_TRANSITIONS_TOTAL.load(Ordering::Relaxed),
        gate_reason_code: GATE_REASON_CODE.load(Ordering::Relaxed),
        effective_success_milli_pct: EFFECTIVE_SUCCESS_MILLI_PCT.load(Ordering::Relaxed),
        rate_limit_milli_pct: RATE_LIMIT_MILLI_PCT.load(Ordering::Relaxed),
        client_error_milli_pct: CLIENT_ERROR_MILLI_PCT.load(Ordering::Relaxed),
        server_error_milli_pct: SERVER_ERROR_MILLI_PCT.load(Ordering::Relaxed),
    }
}

pub fn runtime_prometheus_metrics() -> String {
    let latency = latency_snapshot(None);
    let gate = gate_snapshot();
    format!(
        concat!(
            "# HELP trade_enabled Runtime trade gate state (0/1)\n",
            "# TYPE trade_enabled gauge\n",
            "trade_enabled {}\n",
            "# HELP trade_gate_mode Runtime gate mode (0=normal,1=throttled,2=half_open,3=blocked)\n",
            "# TYPE trade_gate_mode gauge\n",
            "trade_gate_mode {}\n",
            "# HELP trade_gate_transitions_total Runtime gate transitions\n",
            "# TYPE trade_gate_transitions_total counter\n",
            "trade_gate_transitions_total {}\n",
            "# HELP trade_gate_reason_code Last gate reason code\n",
            "# TYPE trade_gate_reason_code gauge\n",
            "trade_gate_reason_code {}\n",
            "# HELP detection_latency_p50_us Detection latency p50 over rolling window\n",
            "# TYPE detection_latency_p50_us gauge\n",
            "detection_latency_p50_us {}\n",
            "# HELP detection_latency_p99_us Detection latency p99 over rolling window\n",
            "# TYPE detection_latency_p99_us gauge\n",
            "detection_latency_p99_us {}\n",
            "# HELP detection_jitter_us Detection jitter (p99-p50) over rolling window\n",
            "# TYPE detection_jitter_us gauge\n",
            "detection_jitter_us {}\n",
            "# HELP execution_latency_p99_us Execution latency p99 over rolling window\n",
            "# TYPE execution_latency_p99_us gauge\n",
            "execution_latency_p99_us {}\n",
            "# HELP backoff_latency_p99_us Backoff latency p99 over rolling window\n",
            "# TYPE backoff_latency_p99_us gauge\n",
            "backoff_latency_p99_us {}\n",
            "# HELP detection_samples_window Detection samples in rolling window\n",
            "# TYPE detection_samples_window gauge\n",
            "detection_samples_window {}\n",
            "# HELP execution_samples_window Execution samples in rolling window\n",
            "# TYPE execution_samples_window gauge\n",
            "execution_samples_window {}\n",
            "# HELP dropped_intents_total Dropped intents (queue_full + stale)\n",
            "# TYPE dropped_intents_total counter\n",
            "dropped_intents_total {}\n",
            "# HELP stale_drop_total Stale intents dropped by exec-worker\n",
            "# TYPE stale_drop_total counter\n",
            "stale_drop_total {}\n",
            "# HELP effective_success_milli_pct Effective success rate x1000 percent\n",
            "# TYPE effective_success_milli_pct gauge\n",
            "effective_success_milli_pct {}\n",
            "# HELP rate_limit_milli_pct HTTP 429 rate x1000 percent\n",
            "# TYPE rate_limit_milli_pct gauge\n",
            "rate_limit_milli_pct {}\n",
            "# HELP client_error_milli_pct 4xx (excluding 429) rate x1000 percent\n",
            "# TYPE client_error_milli_pct gauge\n",
            "client_error_milli_pct {}\n",
            "# HELP server_error_milli_pct 5xx rate x1000 percent\n",
            "# TYPE server_error_milli_pct gauge\n",
            "server_error_milli_pct {}\n",
        ),
        if gate.trade_enabled { 1 } else { 0 },
        gate.gate_mode,
        gate.gate_transitions_total,
        gate.gate_reason_code,
        latency.detection_p50_us,
        latency.detection_p99_us,
        latency.detection_jitter_us,
        latency.execution_p99_us,
        latency.backoff_p99_us,
        latency.detection_samples,
        latency.execution_samples,
        latency.dropped_intents_total,
        latency.stale_drop_total,
        gate.effective_success_milli_pct,
        gate.rate_limit_milli_pct,
        gate.client_error_milli_pct,
        gate.server_error_milli_pct,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset_runtime_state() {
        let mut state = RUNTIME_STATE.lock();
        state.detection_latency_us.clear();
        state.execution_latency_us.clear();
        state.backoff_latency_us.clear();
        state.dropped_intents_total = 0;
        state.stale_drop_total = 0;
        drop(state);

        TRADE_ENABLED.store(true, Ordering::SeqCst);
        GATE_MODE.store(0, Ordering::SeqCst);
        GATE_TRANSITIONS_TOTAL.store(0, Ordering::SeqCst);
        GATE_REASON_CODE.store(0, Ordering::SeqCst);
        EFFECTIVE_SUCCESS_MILLI_PCT.store(0, Ordering::SeqCst);
        RATE_LIMIT_MILLI_PCT.store(0, Ordering::SeqCst);
        CLIENT_ERROR_MILLI_PCT.store(0, Ordering::SeqCst);
        SERVER_ERROR_MILLI_PCT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn record_and_snapshot_detection_latency() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runtime_state();

        record_detection_latency(100);
        let snap = latency_snapshot(Some(DEFAULT_WINDOW_SECS));
        assert_eq!(snap.detection_samples, 1);
        assert_eq!(snap.detection_p50_us, 100);
    }

    #[test]
    fn percentile_calculation_correct() {
        let _guard = TEST_LOCK.lock().unwrap();
        let values = [10_u64, 20, 30, 40, 50];
        assert_eq!(percentile(&values, 500), 30);
        assert_eq!(percentile(&values, 1000), 50);
    }

    #[test]
    fn prune_window_removes_old_samples() {
        let _guard = TEST_LOCK.lock().unwrap();
        let mut samples = VecDeque::new();
        samples.push_back(TimedSample { ts: 10, value: 100 });
        samples.push_back(TimedSample { ts: 20, value: 200 });
        samples.push_back(TimedSample { ts: 30, value: 300 });

        prune_window(&mut samples, 20);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples.front().unwrap().value, 200);
    }

    #[test]
    fn gate_mode_transitions_counted() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runtime_state();

        set_gate_mode(1);
        set_gate_mode(1);
        set_gate_mode(2);
        let snap = gate_snapshot();
        assert_eq!(snap.gate_transitions_total, 2);
        assert_eq!(snap.gate_mode, 2);
    }

    #[test]
    fn stale_drop_increments_both_counters() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runtime_state();

        record_stale_intent_drop();
        record_stale_intent_drop();
        let snap = latency_snapshot(Some(DEFAULT_WINDOW_SECS));
        assert_eq!(snap.stale_drop_total, 2);
        assert_eq!(snap.dropped_intents_total, 2);

        // queue_full drop only increments dropped_intents_total
        record_intent_drop();
        let snap = latency_snapshot(Some(DEFAULT_WINDOW_SECS));
        assert_eq!(snap.stale_drop_total, 2);
        assert_eq!(snap.dropped_intents_total, 3);
    }

    #[test]
    fn runtime_prometheus_metrics_format_valid() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_runtime_state();
        record_detection_latency(123);
        let metrics = runtime_prometheus_metrics();
        assert!(metrics.contains("# HELP trade_enabled"));
        assert!(metrics.contains("detection_latency_p99_us"));
    }
}
