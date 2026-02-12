//! KARON3 ETERNITY FIRE - Prometheus Metrics
//! H5: /metrics 엔드포인트

use std::sync::atomic::Ordering;
use crate::metrics::counters::COUNTERS;
use crate::config::runtime::{TRADE_ENABLED, DRY_RUN, STAGE_FREEZE};

/// Prometheus 형식 메트릭스 생성
pub fn generate_prometheus_metrics() -> String {
    let c = &COUNTERS;

    let mut output = String::with_capacity(4096);

    // Parser metrics
    output.push_str("# HELP karon3_parse_ok Successful parses\n");
    output.push_str("# TYPE karon3_parse_ok counter\n");
    output.push_str(&format!("karon3_parse_ok {}\n", c.pump_parse_ok.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_parse_fail Failed parses\n");
    output.push_str("# TYPE karon3_parse_fail counter\n");
    output.push_str(&format!("karon3_parse_fail {}\n", c.pump_parse_fail.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_fake_mint_blocked Fake mints blocked\n");
    output.push_str("# TYPE karon3_fake_mint_blocked counter\n");
    output.push_str(&format!("karon3_fake_mint_blocked {}\n", c.fake_mint_blocked.load(Ordering::Relaxed)));

    // TX fetch metrics
    output.push_str("# HELP karon3_tx_fetch_ok Successful TX fetches\n");
    output.push_str("# TYPE karon3_tx_fetch_ok counter\n");
    output.push_str(&format!("karon3_tx_fetch_ok {}\n", c.tx_fetch_ok.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_tx_fetch_fail Failed TX fetches\n");
    output.push_str("# TYPE karon3_tx_fetch_fail counter\n");
    output.push_str(&format!("karon3_tx_fetch_fail {}\n", c.tx_fetch_fail.load(Ordering::Relaxed)));

    // Jito metrics
    output.push_str("# HELP jito_requests_total Jito requests by HTTP code class\n");
    output.push_str("# TYPE jito_requests_total counter\n");
    output.push_str(&format!("jito_requests_total{{code=\"2xx\"}} {}\n", c.jito_requests_2xx.load(Ordering::Relaxed)));
    output.push_str(&format!("jito_requests_total{{code=\"400\"}} {}\n", c.jito_requests_400.load(Ordering::Relaxed)));
    output.push_str(&format!("jito_requests_total{{code=\"429\"}} {}\n", c.jito_requests_429.load(Ordering::Relaxed)));
    output.push_str(&format!("jito_requests_total{{code=\"5xx\"}} {}\n", c.jito_requests_5xx.load(Ordering::Relaxed)));
    output.push_str(&format!("jito_requests_total{{code=\"other\"}} {}\n", c.jito_requests_other.load(Ordering::Relaxed)));

    output.push_str("# HELP jito_429_total Jito 429 errors\n");
    output.push_str("# TYPE jito_429_total counter\n");
    output.push_str(&format!("jito_429_total {}\n", c.jito_429.load(Ordering::Relaxed)));

    output.push_str("# HELP jito_400_total Jito 400 errors\n");
    output.push_str("# TYPE jito_400_total counter\n");
    output.push_str(&format!("jito_400_total {}\n", c.jito_400.load(Ordering::Relaxed)));

    output.push_str("# HELP limiter_cooldown_active Limiter cooldown state\n");
    output.push_str("# TYPE limiter_cooldown_active gauge\n");
    output.push_str(&format!("limiter_cooldown_active {}\n", c.limiter_cooldown_active.load(Ordering::Relaxed)));

    output.push_str("# HELP inflight_requests In-flight Jito requests\n");
    output.push_str("# TYPE inflight_requests gauge\n");
    output.push_str(&format!("inflight_requests {}\n", c.inflight_requests.load(Ordering::Relaxed)));

    output.push_str("# HELP queue_depth Pending queue depth before limiter gate\n");
    output.push_str("# TYPE queue_depth gauge\n");
    output.push_str(&format!("queue_depth {}\n", c.queue_depth.load(Ordering::Relaxed)));

    // Backward-compatible names
    output.push_str("# HELP karon3_jito_ok Successful Jito submits\n");
    output.push_str("# TYPE karon3_jito_ok counter\n");
    output.push_str(&format!("karon3_jito_ok {}\n", c.jito_submit_ok.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_jito_429 Jito 429 errors\n");
    output.push_str("# TYPE karon3_jito_429 counter\n");
    output.push_str(&format!("karon3_jito_429 {}\n", c.jito_429.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_jito_400 Jito 400 errors\n");
    output.push_str("# TYPE karon3_jito_400 counter\n");
    output.push_str(&format!("karon3_jito_400 {}\n", c.jito_400.load(Ordering::Relaxed)));

    // Trade metrics
    output.push_str("# HELP karon3_trade_attempt Trade attempts\n");
    output.push_str("# TYPE karon3_trade_attempt counter\n");
    output.push_str(&format!("karon3_trade_attempt {}\n", c.trade_attempt.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_trade_skip Trades skipped\n");
    output.push_str("# TYPE karon3_trade_skip counter\n");
    output.push_str(&format!("karon3_trade_skip {}\n", c.trade_skip.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_trade_success Successful trades\n");
    output.push_str("# TYPE karon3_trade_success counter\n");
    output.push_str(&format!("karon3_trade_success {}\n", c.trade_success.load(Ordering::Relaxed)));

    output.push_str("# HELP karon3_trade_fail Failed trades\n");
    output.push_str("# TYPE karon3_trade_fail counter\n");
    output.push_str(&format!("karon3_trade_fail {}\n", c.trade_fail.load(Ordering::Relaxed)));

    // Runtime state gauges
    output.push_str("# HELP karon3_trade_enabled Trade enabled state\n");
    output.push_str("# TYPE karon3_trade_enabled gauge\n");
    output.push_str(&format!("karon3_trade_enabled {}\n", if TRADE_ENABLED.load(Ordering::SeqCst) { 1 } else { 0 }));

    output.push_str("# HELP karon3_dry_run Dry run mode state\n");
    output.push_str("# TYPE karon3_dry_run gauge\n");
    output.push_str(&format!("karon3_dry_run {}\n", if DRY_RUN.load(Ordering::SeqCst) { 1 } else { 0 }));

    output.push_str("# HELP karon3_stage_freeze Stage freeze state\n");
    output.push_str("# TYPE karon3_stage_freeze gauge\n");
    output.push_str(&format!("karon3_stage_freeze {}\n", if STAGE_FREEZE.load(Ordering::SeqCst) { 1 } else { 0 }));

    output.push_str("# HELP karon3_consecutive_failures Consecutive failures\n");
    output.push_str("# TYPE karon3_consecutive_failures gauge\n");
    output.push_str(&format!("karon3_consecutive_failures {}\n", c.consecutive_failures.load(Ordering::Relaxed)));

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prometheus_output_format() {
        let output = generate_prometheus_metrics();
        assert!(output.contains("jito_requests_total{code=\"429\"}"));
        assert!(output.contains("inflight_requests"));
        assert!(output.contains("# HELP"));
        assert!(output.contains("# TYPE"));
    }
}
