//! Pump.fun token quality filter engine (Phase 1).
//!
//! Filters F0-F8 applied in fast-reject order.
//! All decisions logged for shadow calibration.

use crate::config::PumpFunFilterConfigToml;
use crate::types::PumpFunToken;
use parking_lot::Mutex;
use serde::Serialize;
use solana_sdk::pubkey::Pubkey;
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

// ═══════════════════════════════════════════════════════
//  Result Types
// ═══════════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterAction {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub struct FilterReason {
    pub filter: &'static str,
    pub action: FilterAction,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct FilterResult {
    pub action: FilterAction,
    pub reasons: Vec<FilterReason>,
    pub latency_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnError {
    Allow,
    Deny,
}

impl OnError {
    fn parse(s: Option<&str>, default: OnError) -> OnError {
        match s {
            Some("allow") => OnError::Allow,
            Some("deny") => OnError::Deny,
            _ => default,
        }
    }

    fn to_action(self) -> FilterAction {
        match self {
            OnError::Allow => FilterAction::Allow,
            OnError::Deny => FilterAction::Deny,
        }
    }
}

// ═══════════════════════════════════════════════════════
//  F0: Dedup Cache
// ═══════════════════════════════════════════════════════

struct DedupCache {
    entries: HashMap<String, Instant>,
    order: VecDeque<(Instant, String)>,
    max_size: usize,
    ttl: Duration,
}

impl DedupCache {
    fn new(max_size: usize, ttl_secs: u64) -> Self {
        Self {
            entries: HashMap::with_capacity(max_size.min(65536)),
            order: VecDeque::with_capacity(max_size.min(65536)),
            max_size,
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Returns true if this key is a duplicate (already seen and not expired).
    fn check_and_insert(&mut self, key: &str) -> bool {
        let now = Instant::now();

        // Evict expired entries (amortized: check front only)
        while let Some((ts, _)) = self.order.front() {
            if now.duration_since(*ts) > self.ttl {
                if let Some((_, k)) = self.order.pop_front() {
                    self.entries.remove(&k);
                }
            } else {
                break;
            }
        }

        // Check for duplicate
        if self.entries.contains_key(key) {
            return true;
        }

        // Evict oldest if at capacity
        while self.entries.len() >= self.max_size {
            if let Some((_, k)) = self.order.pop_front() {
                self.entries.remove(&k);
            }
        }

        self.entries.insert(key.to_string(), now);
        self.order.push_back((now, key.to_string()));
        false
    }
}

// ═══════════════════════════════════════════════════════
//  F2: Creator Rate Limiter
// ═══════════════════════════════════════════════════════

struct CreatorRateLimiter {
    creates: HashMap<Pubkey, VecDeque<Instant>>,
    window: Duration,
    max_creates: u32,
}

impl CreatorRateLimiter {
    fn new(window_secs: u64, max_creates: u32) -> Self {
        Self {
            creates: HashMap::new(),
            window: Duration::from_secs(window_secs),
            max_creates,
        }
    }

    /// Returns true if this creator has exceeded the rate limit.
    fn is_rate_limited(&mut self, creator: &Pubkey) -> bool {
        let now = Instant::now();
        let window_start = now - self.window;
        let timestamps = self.creates.entry(*creator).or_default();

        // Remove expired entries
        while let Some(ts) = timestamps.front() {
            if *ts < window_start {
                timestamps.pop_front();
            } else {
                break;
            }
        }

        let exceeded = timestamps.len() >= self.max_creates as usize;
        timestamps.push_back(now);

        // Periodic cleanup: remove creators with no recent activity
        if self.creates.len() > 100_000 {
            self.creates.retain(|_, v| {
                v.back().map_or(false, |ts| now.duration_since(*ts) < self.window)
            });
        }

        exceeded
    }
}

// ═══════════════════════════════════════════════════════
//  F4: create_v2 Args Decode
// ═══════════════════════════════════════════════════════

/// Decode name, symbol, uri from create_v2 instruction data (borsh encoding).
/// Layout after 8-byte discriminator: name(String) + symbol(String) + uri(String).
/// Borsh String = 4-byte LE u32 length + UTF-8 bytes.
pub fn decode_create_v2_args(data: &[u8]) -> Option<(String, String, String)> {
    if data.len() < 12 {
        return None;
    }
    let mut pos = 8; // skip discriminator
    let name = read_borsh_string(data, &mut pos)?;
    let symbol = read_borsh_string(data, &mut pos)?;
    let uri = read_borsh_string(data, &mut pos)?;
    Some((name, symbol, uri))
}

fn read_borsh_string(data: &[u8], pos: &mut usize) -> Option<String> {
    if *pos + 4 > data.len() {
        return None;
    }
    let len = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?) as usize;
    *pos += 4;
    if len > 512 || *pos + len > data.len() {
        return None; // sanity: reject absurdly long strings
    }
    let s = String::from_utf8(data[*pos..*pos + len].to_vec()).ok()?;
    *pos += len;
    Some(s)
}

// ═══════════════════════════════════════════════════════
//  F5/F6: Validation helpers
// ═══════════════════════════════════════════════════════

fn is_valid_name(name: &str, min: usize, max: usize, ascii_only: bool, deny_controls: bool) -> bool {
    let len = name.len();
    if len < min || len > max {
        return false;
    }
    if ascii_only && !name.is_ascii() {
        return false;
    }
    if deny_controls && name.chars().any(|c| c.is_control()) {
        return false;
    }
    // Spam pattern: same char repeated 6+ times
    let bytes = name.as_bytes();
    if bytes.len() >= 6 {
        let mut run = 1u32;
        for i in 1..bytes.len() {
            if bytes[i] == bytes[i - 1] {
                run += 1;
                if run >= 6 {
                    return false;
                }
            } else {
                run = 1;
            }
        }
    }
    true
}

fn extract_uri_host(uri: &str) -> Option<&str> {
    if let Some(rest) = uri.strip_prefix("https://").or_else(|| uri.strip_prefix("http://")) {
        let end = rest.find('/').unwrap_or(rest.len());
        let end = rest[..end].find('?').map_or(end, |q| q.min(end));
        let end = rest[..end].find('#').map_or(end, |h| h.min(end));
        if end > 0 {
            return Some(&rest[..end]);
        }
    }
    None
}

fn is_ipfs_uri(uri: &str) -> bool {
    uri.starts_with("ipfs://") || uri.contains("/ipfs/")
}

fn is_arweave_uri(uri: &str) -> bool {
    uri.starts_with("ar://") || uri.contains("arweave.net/")
}

// ═══════════════════════════════════════════════════════
//  Metrics
// ═══════════════════════════════════════════════════════

pub struct FilterMetrics {
    pub total_evaluated: AtomicU64,
    pub total_allowed: AtomicU64,
    pub total_denied: AtomicU64,
    pub denied_dedup: AtomicU64,
    pub denied_rate_limit: AtomicU64,
    pub denied_denylist: AtomicU64,
    pub allowed_allowlist: AtomicU64,
    pub denied_meta: AtomicU64,
    pub denied_uri: AtomicU64,
    pub denied_commitment: AtomicU64,
    pub errors: AtomicU64,
}

impl FilterMetrics {
    fn new() -> Self {
        Self {
            total_evaluated: AtomicU64::new(0),
            total_allowed: AtomicU64::new(0),
            total_denied: AtomicU64::new(0),
            denied_dedup: AtomicU64::new(0),
            denied_rate_limit: AtomicU64::new(0),
            denied_denylist: AtomicU64::new(0),
            allowed_allowlist: AtomicU64::new(0),
            denied_meta: AtomicU64::new(0),
            denied_uri: AtomicU64::new(0),
            denied_commitment: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> FilterMetricsSnapshot {
        FilterMetricsSnapshot {
            total_evaluated: self.total_evaluated.load(Ordering::Relaxed),
            total_allowed: self.total_allowed.load(Ordering::Relaxed),
            total_denied: self.total_denied.load(Ordering::Relaxed),
            denied_dedup: self.denied_dedup.load(Ordering::Relaxed),
            denied_rate_limit: self.denied_rate_limit.load(Ordering::Relaxed),
            denied_denylist: self.denied_denylist.load(Ordering::Relaxed),
            denied_meta: self.denied_meta.load(Ordering::Relaxed),
            denied_uri: self.denied_uri.load(Ordering::Relaxed),
            denied_commitment: self.denied_commitment.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FilterMetricsSnapshot {
    pub total_evaluated: u64,
    pub total_allowed: u64,
    pub total_denied: u64,
    pub denied_dedup: u64,
    pub denied_rate_limit: u64,
    pub denied_denylist: u64,
    pub denied_meta: u64,
    pub denied_uri: u64,
    pub denied_commitment: u64,
}

// ═══════════════════════════════════════════════════════
//  Filter Engine
// ═══════════════════════════════════════════════════════

struct MetaQualityConfig {
    require_decode: bool,
    on_decode_error: OnError,
    name_len_min: usize,
    name_len_max: usize,
    symbol_len_min: usize,
    symbol_len_max: usize,
    ascii_only: bool,
    deny_unicode_controls: bool,
}

pub struct PumpFunFilterEngine {
    enabled: bool,
    default_on_error: OnError,
    max_budget: Duration,
    emit_log: bool,
    log_sample_counter: AtomicU64,
    log_sample_rate_inv: u64, // 1/rate as integer, e.g. 10 = 10% sampling

    // F0: Dedup
    sig_dedup: Mutex<DedupCache>,
    mint_dedup: Mutex<DedupCache>,

    // F2: Creator rate limit
    creator_rate: Mutex<CreatorRateLimiter>,
    creator_rate_on_error: OnError,

    // F3: Lists
    creator_denylist: HashSet<Pubkey>,
    creator_allowlist: HashSet<Pubkey>,

    // F5: Meta quality
    meta_config: MetaQualityConfig,

    // F6: URI allowlist
    uri_require: bool,
    uri_on_missing: OnError,
    uri_allow_ipfs: bool,
    uri_allow_arweave: bool,
    uri_https_hosts: HashSet<String>,

    // F7: Creator commitment
    min_creator_spend_lamports: u64,
    require_fee_payer_is_creator: bool,
    commitment_on_missing: OnError,

    // Metrics
    pub metrics: FilterMetrics,
}

impl PumpFunFilterEngine {
    pub fn new(config: Option<&PumpFunFilterConfigToml>) -> Self {
        let cfg = config.cloned().unwrap_or_default();
        let enabled = cfg.enabled.unwrap_or(false);
        let default_on_error =
            OnError::parse(cfg.default_on_error.as_deref(), OnError::Deny);

        // Dedup
        let dedup_cfg = cfg.dedup.unwrap_or_default();
        let max_seen = dedup_cfg.max_seen.unwrap_or(200_000);
        let ttl_secs = dedup_cfg.ttl_secs.unwrap_or(600);

        // Creator rate limit
        let rl_cfg = cfg.creator_rate_limit.unwrap_or_default();
        let rl_window = rl_cfg.window_secs.unwrap_or(600);
        let rl_max = rl_cfg.max_creates.unwrap_or(2);
        let rl_on_error = OnError::parse(rl_cfg.on_error.as_deref(), default_on_error);

        // Lists
        let lists_cfg = cfg.lists.unwrap_or_default();
        let creator_denylist = lists_cfg
            .creator_denylist
            .as_deref()
            .map(load_pubkey_list)
            .unwrap_or_default();
        let creator_allowlist = lists_cfg
            .creator_allowlist
            .as_deref()
            .map(load_pubkey_list)
            .unwrap_or_default();

        // Meta quality
        let meta_cfg = cfg.meta.unwrap_or_default();
        let meta_config = MetaQualityConfig {
            require_decode: meta_cfg.require_decode.unwrap_or(true),
            on_decode_error: OnError::parse(meta_cfg.on_decode_error.as_deref(), default_on_error),
            name_len_min: meta_cfg.name_len_min.unwrap_or(2),
            name_len_max: meta_cfg.name_len_max.unwrap_or(32),
            symbol_len_min: meta_cfg.symbol_len_min.unwrap_or(2),
            symbol_len_max: meta_cfg.symbol_len_max.unwrap_or(10),
            ascii_only: meta_cfg.ascii_only.unwrap_or(true),
            deny_unicode_controls: meta_cfg.deny_unicode_controls.unwrap_or(true),
        };

        // URI
        let uri_cfg = cfg.uri.unwrap_or_default();
        let uri_require = uri_cfg.require_uri.unwrap_or(true);
        let uri_on_missing = OnError::parse(uri_cfg.on_missing.as_deref(), default_on_error);
        let uri_allow_ipfs = uri_cfg.allow_ipfs.unwrap_or(true);
        let uri_allow_arweave = uri_cfg.allow_arweave.unwrap_or(true);
        let uri_https_hosts: HashSet<String> = uri_cfg
            .allow_https_hosts
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.to_lowercase())
            .collect();

        // Creator commitment
        let commit_cfg = cfg.creator_commitment.unwrap_or_default();
        let min_spend_sol = commit_cfg.min_creator_spend_sol.unwrap_or(0.0);
        let min_creator_spend_lamports = (min_spend_sol * 1_000_000_000.0) as u64;
        let require_fee_payer_is_creator =
            commit_cfg.require_fee_payer_is_creator.unwrap_or(false);
        let commitment_on_missing =
            OnError::parse(commit_cfg.on_missing_meta.as_deref(), OnError::Allow);

        // Logging
        let emit_log = cfg.emit_reason_log.unwrap_or(true);
        let sample_rate = cfg.reason_log_sample_rate.unwrap_or(0.10);
        let log_sample_rate_inv = if sample_rate <= 0.0 {
            u64::MAX
        } else {
            (1.0 / sample_rate) as u64
        };

        let max_budget_ms = cfg.max_total_budget_ms.unwrap_or(15);

        if enabled {
            info!(
                "PumpFun filter armed: dedup={}, rate_limit={}/{} creators/{}s, denylist={}, allowlist={}, meta_ascii={}, uri_hosts={}, min_spend={:.4} SOL",
                max_seen, rl_max, rl_window, rl_window,
                creator_denylist.len(), creator_allowlist.len(),
                meta_config.ascii_only, uri_https_hosts.len(),
                min_spend_sol
            );
        }

        Self {
            enabled,
            default_on_error,
            max_budget: Duration::from_millis(max_budget_ms),
            emit_log,
            log_sample_counter: AtomicU64::new(0),
            log_sample_rate_inv,
            sig_dedup: Mutex::new(DedupCache::new(max_seen, ttl_secs)),
            mint_dedup: Mutex::new(DedupCache::new(max_seen, ttl_secs)),
            creator_rate: Mutex::new(CreatorRateLimiter::new(rl_window, rl_max)),
            creator_rate_on_error: rl_on_error,
            creator_denylist,
            creator_allowlist,
            meta_config,
            uri_require,
            uri_on_missing,
            uri_allow_ipfs,
            uri_allow_arweave,
            uri_https_hosts,
            min_creator_spend_lamports,
            require_fee_payer_is_creator,
            commitment_on_missing,
            metrics: FilterMetrics::new(),
        }
    }

    /// Evaluate a PumpFunToken through all Phase 1 filters.
    /// Returns Allow or Deny with reasons.
    pub fn evaluate(&self, token: &PumpFunToken) -> FilterResult {
        if !self.enabled {
            return FilterResult {
                action: FilterAction::Allow,
                reasons: vec![],
                latency_us: 0,
            };
        }

        let start = Instant::now();
        let deadline = start + self.max_budget;
        let mut reasons = Vec::with_capacity(4);
        self.metrics.total_evaluated.fetch_add(1, Ordering::Relaxed);

        // ── F0: Signature dedup ──
        if let Some(sig) = &token.signature {
            if self.sig_dedup.lock().check_and_insert(sig) {
                let r = FilterReason {
                    filter: "F0_sig_dedup",
                    action: FilterAction::Deny,
                    detail: "duplicate signature".into(),
                };
                reasons.push(r);
                self.metrics.denied_dedup.fetch_add(1, Ordering::Relaxed);
                return self.finalize(FilterAction::Deny, reasons, start);
            }
        }

        // ── F0: Mint dedup ──
        let mint_str = token.mint.to_string();
        if self.mint_dedup.lock().check_and_insert(&mint_str) {
            let r = FilterReason {
                filter: "F0_mint_dedup",
                action: FilterAction::Deny,
                detail: "duplicate mint".into(),
            };
            reasons.push(r);
            self.metrics.denied_dedup.fetch_add(1, Ordering::Relaxed);
            return self.finalize(FilterAction::Deny, reasons, start);
        }

        // ── F1: Structural sanity ──
        if token.bonding_curve == Pubkey::default() || token.mint == Pubkey::default() {
            let r = FilterReason {
                filter: "F1_structural",
                action: FilterAction::Deny,
                detail: "invalid pubkey (default)".into(),
            };
            reasons.push(r);
            return self.finalize(FilterAction::Deny, reasons, start);
        }

        // ── Budget check ──
        if Instant::now() > deadline {
            self.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return self.finalize(self.default_on_error.to_action(), reasons, start);
        }

        // ── F2: Creator rate limit ──
        if self.creator_rate.lock().is_rate_limited(&token.user) {
            let r = FilterReason {
                filter: "F2_creator_rate",
                action: FilterAction::Deny,
                detail: "creator exceeded create rate limit".into(),
            };
            reasons.push(r);
            self.metrics
                .denied_rate_limit
                .fetch_add(1, Ordering::Relaxed);
            return self.finalize(FilterAction::Deny, reasons, start);
        }

        // ── F3: Creator denylist ──
        if self.creator_denylist.contains(&token.user) {
            let r = FilterReason {
                filter: "F3_denylist",
                action: FilterAction::Deny,
                detail: "creator is on denylist".into(),
            };
            reasons.push(r);
            self.metrics.denied_denylist.fetch_add(1, Ordering::Relaxed);
            return self.finalize(FilterAction::Deny, reasons, start);
        }

        // ── F3: Creator allowlist (hard pass) ──
        if self.creator_allowlist.contains(&token.user) {
            let r = FilterReason {
                filter: "F3_allowlist",
                action: FilterAction::Allow,
                detail: "creator is on allowlist".into(),
            };
            reasons.push(r);
            self.metrics
                .allowed_allowlist
                .fetch_add(1, Ordering::Relaxed);
            return self.finalize(FilterAction::Allow, reasons, start);
        }

        // ── F5: Metadata quality ──
        if let (Some(name), Some(symbol)) = (&token.name, &token.symbol) {
            if !is_valid_name(
                name,
                self.meta_config.name_len_min,
                self.meta_config.name_len_max,
                self.meta_config.ascii_only,
                self.meta_config.deny_unicode_controls,
            ) {
                let r = FilterReason {
                    filter: "F5_meta_name",
                    action: FilterAction::Deny,
                    detail: format!("name failed quality: {:?}", name),
                };
                reasons.push(r);
                self.metrics.denied_meta.fetch_add(1, Ordering::Relaxed);
                return self.finalize(FilterAction::Deny, reasons, start);
            }
            if !is_valid_name(
                symbol,
                self.meta_config.symbol_len_min,
                self.meta_config.symbol_len_max,
                self.meta_config.ascii_only,
                self.meta_config.deny_unicode_controls,
            ) {
                let r = FilterReason {
                    filter: "F5_meta_symbol",
                    action: FilterAction::Deny,
                    detail: format!("symbol failed quality: {:?}", symbol),
                };
                reasons.push(r);
                self.metrics.denied_meta.fetch_add(1, Ordering::Relaxed);
                return self.finalize(FilterAction::Deny, reasons, start);
            }
        } else if self.meta_config.require_decode {
            // Metadata not available
            let action = self.meta_config.on_decode_error.to_action();
            if action == FilterAction::Deny {
                let r = FilterReason {
                    filter: "F4_meta_decode",
                    action: FilterAction::Deny,
                    detail: "name/symbol not decoded".into(),
                };
                reasons.push(r);
                self.metrics.denied_meta.fetch_add(1, Ordering::Relaxed);
                return self.finalize(FilterAction::Deny, reasons, start);
            }
        }

        // ── F6: URI check ──
        if let Some(uri) = &token.uri {
            if !uri.is_empty() {
                let uri_ok = is_ipfs_uri(uri) && self.uri_allow_ipfs
                    || is_arweave_uri(uri) && self.uri_allow_arweave
                    || {
                        if let Some(host) = extract_uri_host(uri) {
                            self.uri_https_hosts.contains(&host.to_lowercase())
                        } else {
                            false
                        }
                    };
                if !uri_ok {
                    let r = FilterReason {
                        filter: "F6_uri",
                        action: FilterAction::Deny,
                        detail: format!("URI host not allowed: {:?}", uri),
                    };
                    reasons.push(r);
                    self.metrics.denied_uri.fetch_add(1, Ordering::Relaxed);
                    return self.finalize(FilterAction::Deny, reasons, start);
                }
            } else if self.uri_require {
                let action = self.uri_on_missing.to_action();
                if action == FilterAction::Deny {
                    let r = FilterReason {
                        filter: "F6_uri_empty",
                        action: FilterAction::Deny,
                        detail: "URI is empty".into(),
                    };
                    reasons.push(r);
                    self.metrics.denied_uri.fetch_add(1, Ordering::Relaxed);
                    return self.finalize(FilterAction::Deny, reasons, start);
                }
            }
        } else if self.uri_require {
            let action = self.uri_on_missing.to_action();
            if action == FilterAction::Deny {
                let r = FilterReason {
                    filter: "F6_uri_missing",
                    action: FilterAction::Deny,
                    detail: "URI not present".into(),
                };
                reasons.push(r);
                self.metrics.denied_uri.fetch_add(1, Ordering::Relaxed);
                return self.finalize(FilterAction::Deny, reasons, start);
            }
        }

        // ── F7: Creator commitment ──
        if self.min_creator_spend_lamports > 0 {
            if let Some(spend) = token.creator_spend_lamports {
                if spend < self.min_creator_spend_lamports {
                    let r = FilterReason {
                        filter: "F7_commitment",
                        action: FilterAction::Deny,
                        detail: format!(
                            "creator spend {} < min {}",
                            spend, self.min_creator_spend_lamports
                        ),
                    };
                    reasons.push(r);
                    self.metrics
                        .denied_commitment
                        .fetch_add(1, Ordering::Relaxed);
                    return self.finalize(FilterAction::Deny, reasons, start);
                }
            } else {
                let action = self.commitment_on_missing.to_action();
                if action == FilterAction::Deny {
                    let r = FilterReason {
                        filter: "F7_commitment_missing",
                        action: FilterAction::Deny,
                        detail: "creator spend data not available".into(),
                    };
                    reasons.push(r);
                    self.metrics
                        .denied_commitment
                        .fetch_add(1, Ordering::Relaxed);
                    return self.finalize(FilterAction::Deny, reasons, start);
                }
            }
        }

        // ── F7b: Fee payer = creator check ──
        if self.require_fee_payer_is_creator {
            if let Some(payer) = token.fee_payer {
                if payer != token.user {
                    let r = FilterReason {
                        filter: "F7_fee_payer",
                        action: FilterAction::Deny,
                        detail: "fee payer != creator".into(),
                    };
                    reasons.push(r);
                    self.metrics
                        .denied_commitment
                        .fetch_add(1, Ordering::Relaxed);
                    return self.finalize(FilterAction::Deny, reasons, start);
                }
            }
        }

        // All filters passed
        let r = FilterReason {
            filter: "PASS",
            action: FilterAction::Allow,
            detail: "all filters passed".into(),
        };
        reasons.push(r);
        self.finalize(FilterAction::Allow, reasons, start)
    }

    fn finalize(
        &self,
        action: FilterAction,
        reasons: Vec<FilterReason>,
        start: Instant,
    ) -> FilterResult {
        let latency_us = start.elapsed().as_micros() as u64;

        match action {
            FilterAction::Allow => self.metrics.total_allowed.fetch_add(1, Ordering::Relaxed),
            FilterAction::Deny => self.metrics.total_denied.fetch_add(1, Ordering::Relaxed),
        };

        // Sampled logging
        if self.emit_log {
            let counter = self.log_sample_counter.fetch_add(1, Ordering::Relaxed);
            if counter % self.log_sample_rate_inv == 0 || action == FilterAction::Deny {
                let reason_str: Vec<String> = reasons
                    .iter()
                    .map(|r| format!("{}:{:?}", r.filter, r.action))
                    .collect();
                if action == FilterAction::Deny {
                    debug!(
                        "PUMPFUN_FILTER_DENY: reasons=[{}] latency_us={}",
                        reason_str.join(","),
                        latency_us
                    );
                } else {
                    debug!(
                        "PUMPFUN_FILTER_ALLOW: reasons=[{}] latency_us={}",
                        reason_str.join(","),
                        latency_us
                    );
                }
            }
        }

        FilterResult {
            action,
            reasons,
            latency_us,
        }
    }
}

// ═══════════════════════════════════════════════════════
//  Utility: Load pubkey list from file
// ═══════════════════════════════════════════════════════

fn load_pubkey_list(path: &str) -> HashSet<Pubkey> {
    let mut set = HashSet::new();
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            for line in contents.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                match Pubkey::from_str(trimmed) {
                    Ok(pk) => {
                        set.insert(pk);
                    }
                    Err(_) => {
                        warn!("Invalid pubkey in {}: {}", path, trimmed);
                    }
                }
            }
            info!("Loaded {} pubkeys from {}", set.len(), path);
        }
        Err(e) => {
            if path != "configs/pumpfun_creator_deny.txt"
                && path != "configs/pumpfun_creator_allow.txt"
            {
                warn!("Could not load pubkey list {}: {}", path, e);
            } else {
                debug!("Pubkey list not found (OK): {}", path);
            }
        }
    }
    set
}

// ═══════════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use solana_sdk::pubkey::Pubkey;

    fn make_token() -> PumpFunToken {
        PumpFunToken {
            mint: Pubkey::new_unique(),
            bonding_curve: Pubkey::new_unique(),
            associated_bonding_curve: Pubkey::new_unique(),
            user: Pubkey::new_unique(),
            initial_buy_amount_lamports: 0,
            virtual_sol_reserves: 0,
            virtual_token_reserves: 0,
            detected_at: Utc::now(),
            slot: 12345,
            signature: Some("5abc123def456".to_string()),
            name: Some("TestCoin".to_string()),
            symbol: Some("TEST".to_string()),
            uri: Some("https://arweave.net/abc123".to_string()),
            creator_spend_lamports: Some(1_000_000_000), // 1 SOL
            fee_payer: None,
        }
    }

    fn make_enabled_config() -> PumpFunFilterConfigToml {
        PumpFunFilterConfigToml {
            enabled: Some(true),
            mode: Some("hard_gate".into()),
            default_on_error: Some("deny".into()),
            max_total_budget_ms: Some(100),
            min_score: None,
            emit_reason_log: Some(false),
            reason_log_sample_rate: Some(1.0),
            dedup: Some(crate::config::DedupFilterConfig {
                max_seen: Some(1000),
                ttl_secs: Some(60),
            }),
            creator_rate_limit: Some(crate::config::CreatorRateLimitConfig {
                window_secs: Some(600),
                max_creates: Some(2),
                on_error: Some("deny".into()),
            }),
            lists: None,
            meta: Some(crate::config::MetaFilterConfig {
                require_decode: Some(true),
                on_decode_error: Some("deny".into()),
                name_len_min: Some(2),
                name_len_max: Some(32),
                symbol_len_min: Some(2),
                symbol_len_max: Some(10),
                ascii_only: Some(true),
                deny_unicode_controls: Some(true),
            }),
            uri: Some(crate::config::UriFilterConfig {
                require_uri: Some(true),
                on_missing: Some("deny".into()),
                allow_ipfs: Some(true),
                allow_arweave: Some(true),
                allow_https_hosts: Some(vec![
                    "arweave.net".into(),
                    "pump.fun".into(),
                    "ipfs.io".into(),
                ]),
            }),
            creator_commitment: Some(crate::config::CreatorCommitmentConfig {
                min_creator_spend_sol: Some(0.5),
                require_fee_payer_is_creator: Some(false),
                on_missing_meta: Some("allow".into()),
            }),
        }
    }

    // ── Dedup tests ──

    #[test]
    fn dedup_detects_duplicate() {
        let mut cache = DedupCache::new(100, 60);
        assert!(!cache.check_and_insert("sig1"));
        assert!(cache.check_and_insert("sig1")); // duplicate
        assert!(!cache.check_and_insert("sig2"));
    }

    #[test]
    fn dedup_capacity_eviction() {
        let mut cache = DedupCache::new(3, 600);
        assert!(!cache.check_and_insert("a"));
        assert!(!cache.check_and_insert("b"));
        assert!(!cache.check_and_insert("c"));
        // At capacity, next insert evicts oldest
        assert!(!cache.check_and_insert("d"));
        // "a" was evicted, should not be a duplicate now
        assert!(!cache.check_and_insert("a"));
    }

    // ── Creator rate limit tests ──

    #[test]
    fn rate_limit_allows_under_threshold() {
        let mut rl = CreatorRateLimiter::new(600, 3);
        let creator = Pubkey::new_unique();
        assert!(!rl.is_rate_limited(&creator));
        assert!(!rl.is_rate_limited(&creator));
        assert!(!rl.is_rate_limited(&creator));
        // 4th call exceeds limit of 3
        assert!(rl.is_rate_limited(&creator));
    }

    #[test]
    fn rate_limit_different_creators_independent() {
        let mut rl = CreatorRateLimiter::new(600, 1);
        let c1 = Pubkey::new_unique();
        let c2 = Pubkey::new_unique();
        assert!(!rl.is_rate_limited(&c1));
        assert!(!rl.is_rate_limited(&c2));
        // Both hit limit independently
        assert!(rl.is_rate_limited(&c1));
        assert!(rl.is_rate_limited(&c2));
    }

    // ── Borsh decode tests ──

    #[test]
    fn decode_create_v2_args_valid() {
        let mut data = vec![0u8; 8]; // discriminator
        // name = "ABC"
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(b"ABC");
        // symbol = "XY"
        data.extend_from_slice(&2u32.to_le_bytes());
        data.extend_from_slice(b"XY");
        // uri = "https://example.com"
        let uri = b"https://example.com";
        data.extend_from_slice(&(uri.len() as u32).to_le_bytes());
        data.extend_from_slice(uri);

        let result = decode_create_v2_args(&data);
        assert!(result.is_some());
        let (name, symbol, uri) = result.unwrap();
        assert_eq!(name, "ABC");
        assert_eq!(symbol, "XY");
        assert_eq!(uri, "https://example.com");
    }

    #[test]
    fn decode_create_v2_args_truncated() {
        let data = vec![0u8; 8]; // discriminator only
        assert!(decode_create_v2_args(&data).is_none());
    }

    #[test]
    fn decode_create_v2_args_invalid_utf8() {
        let mut data = vec![0u8; 8];
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&[0xFF, 0xFE, 0xFD]); // invalid UTF-8
        assert!(decode_create_v2_args(&data).is_none());
    }

    // ── Name validation tests ──

    #[test]
    fn valid_name_passes() {
        assert!(is_valid_name("TestCoin", 2, 32, true, true));
        assert!(is_valid_name("AB", 2, 32, true, true));
    }

    #[test]
    fn name_too_short_fails() {
        assert!(!is_valid_name("A", 2, 32, true, true));
    }

    #[test]
    fn name_too_long_fails() {
        assert!(!is_valid_name("ABCDEFGHIJK", 2, 10, true, true));
    }

    #[test]
    fn name_non_ascii_fails_when_required() {
        assert!(!is_valid_name("Test\u{200B}Coin", 2, 32, true, true)); // zero-width space
        assert!(!is_valid_name("Tester\u{4e16}", 2, 32, true, true)); // CJK char
    }

    #[test]
    fn name_control_char_fails() {
        assert!(!is_valid_name("Test\x00Coin", 2, 32, false, true));
    }

    #[test]
    fn name_spam_pattern_fails() {
        assert!(!is_valid_name("AAAAAA", 2, 32, true, true)); // 6 repeated
        assert!(is_valid_name("AAAAA", 2, 32, true, true)); // 5 is OK
    }

    // ── URI tests ──

    #[test]
    fn ipfs_uri_detected() {
        assert!(is_ipfs_uri("ipfs://QmABC123"));
        assert!(is_ipfs_uri("https://gateway.pinata.cloud/ipfs/QmABC123"));
        assert!(!is_ipfs_uri("https://example.com"));
    }

    #[test]
    fn arweave_uri_detected() {
        assert!(is_arweave_uri("ar://abc123"));
        assert!(is_arweave_uri("https://arweave.net/abc123"));
        assert!(!is_arweave_uri("https://example.com"));
    }

    #[test]
    fn extract_host_works() {
        assert_eq!(extract_uri_host("https://arweave.net/abc"), Some("arweave.net"));
        assert_eq!(extract_uri_host("https://pump.fun/token/abc"), Some("pump.fun"));
        assert_eq!(extract_uri_host("https://example.com?q=1"), Some("example.com"));
        assert_eq!(extract_uri_host("ipfs://abc"), None);
    }

    // ── Full pipeline tests ──

    #[test]
    fn disabled_filter_allows_everything() {
        let engine = PumpFunFilterEngine::new(None);
        let token = make_token();
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Allow);
    }

    #[test]
    fn good_token_passes_all_filters() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let token = make_token();
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Allow);
    }

    #[test]
    fn duplicate_signature_denied() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let token = make_token();
        let r1 = engine.evaluate(&token);
        assert_eq!(r1.action, FilterAction::Allow);
        let r2 = engine.evaluate(&token); // same sig
        assert_eq!(r2.action, FilterAction::Deny);
        assert_eq!(r2.reasons[0].filter, "F0_sig_dedup");
    }

    #[test]
    fn creator_rate_limit_triggers() {
        let mut cfg = make_enabled_config();
        cfg.creator_rate_limit = Some(crate::config::CreatorRateLimitConfig {
            window_secs: Some(600),
            max_creates: Some(1),
            on_error: Some("deny".into()),
        });
        let engine = PumpFunFilterEngine::new(Some(&cfg));

        let creator = Pubkey::new_unique();

        let mut t1 = make_token();
        t1.user = creator;
        t1.signature = Some("sig1".into());
        let r1 = engine.evaluate(&t1);
        assert_eq!(r1.action, FilterAction::Allow);

        let mut t2 = make_token();
        t2.user = creator;
        t2.mint = Pubkey::new_unique();
        t2.signature = Some("sig2".into());
        let r2 = engine.evaluate(&t2);
        assert_eq!(r2.action, FilterAction::Deny);
        assert_eq!(r2.reasons[0].filter, "F2_creator_rate");
    }

    #[test]
    fn missing_name_denied_when_required() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let mut token = make_token();
        token.name = None;
        token.symbol = None;
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Deny);
        assert_eq!(result.reasons[0].filter, "F4_meta_decode");
    }

    #[test]
    fn bad_uri_host_denied() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let mut token = make_token();
        token.uri = Some("https://evil-site.com/token.json".into());
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Deny);
        assert_eq!(result.reasons[0].filter, "F6_uri");
    }

    #[test]
    fn low_creator_spend_denied() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let mut token = make_token();
        token.creator_spend_lamports = Some(100_000_000); // 0.1 SOL < 0.5 min
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Deny);
        assert_eq!(result.reasons[0].filter, "F7_commitment");
    }

    #[test]
    fn structural_default_pubkey_denied() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let mut token = make_token();
        token.bonding_curve = Pubkey::default();
        let result = engine.evaluate(&token);
        assert_eq!(result.action, FilterAction::Deny);
        assert_eq!(result.reasons[0].filter, "F1_structural");
    }

    #[test]
    fn metrics_count_correctly() {
        let cfg = make_enabled_config();
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let t1 = make_token();
        engine.evaluate(&t1);
        let snap = engine.metrics.snapshot();
        assert_eq!(snap.total_evaluated, 1);
        assert_eq!(snap.total_allowed, 1);
        assert_eq!(snap.total_denied, 0);

        // duplicate
        engine.evaluate(&t1);
        let snap = engine.metrics.snapshot();
        assert_eq!(snap.total_evaluated, 2);
        assert_eq!(snap.total_denied, 1);
        assert_eq!(snap.denied_dedup, 1);
    }

    #[test]
    fn budget_exceeded_uses_default_on_error_deny() {
        // max_total_budget_ms=0 forces immediate deadline expiry → default_on_error="deny"
        let mut cfg = make_enabled_config();
        cfg.max_total_budget_ms = Some(0);
        cfg.default_on_error = Some("deny".into());
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let token = make_token();
        let result = engine.evaluate(&token);
        // F0 dedup/F1 structural run before budget check, so if those pass
        // and budget is blown, default_on_error kicks in.
        // With budget=0ms the deadline is already expired by the time we reach the check.
        assert_eq!(result.action, FilterAction::Deny);
        assert!(engine.metrics.errors.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn budget_exceeded_uses_default_on_error_allow() {
        let mut cfg = make_enabled_config();
        cfg.max_total_budget_ms = Some(0);
        cfg.default_on_error = Some("allow".into());
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let token = make_token();
        let result = engine.evaluate(&token);
        // With default_on_error="allow", budget exceeded → Allow
        assert_eq!(result.action, FilterAction::Allow);
        assert!(engine.metrics.errors.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn list_file_missing_does_not_panic() {
        let mut cfg = make_enabled_config();
        cfg.lists = Some(crate::config::ListsFilterConfig {
            creator_denylist: Some("/nonexistent/path/deny.txt".into()),
            creator_allowlist: Some("/nonexistent/path/allow.txt".into()),
            on_error: Some("deny".into()),
        });
        // Must not panic — returns engine with empty sets
        let engine = PumpFunFilterEngine::new(Some(&cfg));
        let token = make_token();
        let result = engine.evaluate(&token);
        // Token should still pass (empty denylist = no block from F3)
        assert_eq!(result.action, FilterAction::Allow);
    }
}
