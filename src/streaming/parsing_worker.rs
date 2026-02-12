// ═══════════════════════════════════════════════════════════
// IGNIS FIX #1: Parsing Worker — Decoupled RPC fetch pool.
// All get_transaction() / retry / sleep happens HERE,
// never on the WebSocket read thread.
// ═══════════════════════════════════════════════════════════

use crate::constants::{PUMP_FUN_CREATE_V2_DISCRIMINATOR, PUMP_FUN_PROGRAM};
use crate::types::{LatencyEvent, PumpFunToken, StreamEvent};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use crossbeam_channel::Receiver;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{EncodedTransaction, UiMessage, UiTransactionEncoding};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ─── Metrics (atomic, lock-free) ───────────────────────
static PARSE_TIMEOUT_TOTAL: AtomicU64 = AtomicU64::new(0);
static PARSE_SUCCESS_TOTAL: AtomicU64 = AtomicU64::new(0);
static PARSE_FAIL_TOTAL: AtomicU64 = AtomicU64::new(0);

pub fn parse_metrics_snapshot() -> (u64, u64, u64) {
    (
        PARSE_SUCCESS_TOTAL.load(Ordering::Relaxed),
        PARSE_FAIL_TOTAL.load(Ordering::Relaxed),
        PARSE_TIMEOUT_TOTAL.load(Ordering::Relaxed),
    )
}

// ─── Parse Job ─────────────────────────────────────────
#[derive(Debug)]
pub enum ParseJob {
    PumpFun {
        signature: String,
        slot: u64,
        recv_time: DateTime<Utc>,
        ws_receive_us: u64,
    },
}

// ─── Worker Config ─────────────────────────────────────
const MAX_RETRIES: usize = 5;
const RETRY_SLEEP_BASE_MS: u64 = 200;
const RETRY_SLEEP_CAP_MS: u64 = 2000;
const TOTAL_BUDGET_MS: u64 = 5000;

/// Spawn N parser worker threads.
/// Each worker pulls jobs from the shared crossbeam MPMC receiver,
/// performs the RPC fetch, and sends results to event_tx.
pub fn spawn_parser_workers(
    worker_count: usize,
    rpc_url: String,
    parse_rx: Receiver<ParseJob>,
    event_tx: mpsc::Sender<StreamEvent>,
    shutdown: Arc<AtomicBool>,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::with_capacity(worker_count);

    for id in 0..worker_count {
        let rx = parse_rx.clone();
        let rpc_url = rpc_url.clone();
        let event_tx = event_tx.clone();
        let shutdown = shutdown.clone();

        let handle = std::thread::Builder::new()
            .name(format!("rpc-parser-{}", id))
            .spawn(move || {
                let rpc_client = RpcClient::new(rpc_url);
                info!("[parser-{}] Worker started", id);

                for job in rx.iter() {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }

                    match job {
                        ParseJob::PumpFun { signature, slot, recv_time, ws_receive_us } => {
                            match fetch_and_parse_pumpfun_budgeted(&rpc_client, &signature, slot, recv_time) {
                                Ok(token) => {
                                    let parse_done_us = now_us();
                                    PARSE_SUCCESS_TOTAL.fetch_add(1, Ordering::Relaxed);
                                    info!("[parser-{}] Pump.fun Token parsed: Mint={}", id, token.mint);

                                    let mut latency = LatencyEvent::default();
                                    latency.event_id = signature;
                                    latency.ws_receive_us = ws_receive_us;
                                    latency.ws_recv_ts_us = ws_receive_us;
                                    latency.pool_parsed_us = parse_done_us;
                                    latency.parse_done_ts_us = parse_done_us;

                                    match event_tx.try_send(StreamEvent::NewPumpFunToken(token, latency)) {
                                        Ok(_) => {}
                                        Err(mpsc::error::TrySendError::Full(_)) => {
                                            warn!("[parser-{}] Intent queue full — dropping NewPumpFunToken (drop-newest)", id);
                                        }
                                        Err(mpsc::error::TrySendError::Closed(_)) => {
                                            error!("[parser-{}] Event channel closed", id);
                                        }
                                    }
                                }
                                Err(e) => {
                                    PARSE_FAIL_TOTAL.fetch_add(1, Ordering::Relaxed);
                                    warn!("[parser-{}] Failed to parse Pump.fun token: {}", id, e);
                                }
                            }
                        }
                    }
                }

                info!("[parser-{}] Worker exiting", id);
            })
            .expect("Failed to spawn parser thread");

        handles.push(handle);
    }

    handles
}

// ─── Budgeted RPC Fetch (Pump.fun) ─────────────────────
fn fetch_and_parse_pumpfun_budgeted(
    client: &RpcClient,
    signature_str: &str,
    slot: u64,
    detected_at: DateTime<Utc>,
) -> Result<PumpFunToken> {
    let signature = Signature::from_str(signature_str).context("Invalid signature")?;
    let deadline = Instant::now() + Duration::from_millis(TOTAL_BUDGET_MS);
    let mut retries = 0;

    let match_discriminator = |data: &[u8]| -> bool {
        if data.len() < 8 {
            return false;
        }
        let disc = &data[0..8];
        if disc == PUMP_FUN_CREATE_V2_DISCRIMINATOR.to_be_bytes() {
            return true;
        }
        if disc == [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77] {
            return true;
        }
        false
    };

    loop {
        let tx_config = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        match client.get_transaction_with_config(&signature, tx_config) {
            Ok(tx) => {
                if let EncodedTransaction::Json(ui_tx) = tx.transaction.transaction {
                    if let UiMessage::Raw(message) = ui_tx.message {
                        let account_keys = message.account_keys;
                        for ix in message.instructions {
                            let program_idx = ix.program_id_index as usize;
                            if program_idx >= account_keys.len() {
                                continue;
                            }
                            if account_keys[program_idx] != PUMP_FUN_PROGRAM.to_string() {
                                continue;
                            }

                            let data = bs58::decode(&ix.data).into_vec().unwrap_or_default();
                            if !match_discriminator(&data) {
                                continue;
                            }

                            let get_pubkey = |idx: usize| -> Result<Pubkey> {
                                let acc_idx =
                                    ix.accounts.get(idx).context("Missing account")?.clone()
                                        as usize;
                                if acc_idx >= account_keys.len() {
                                    anyhow::bail!("Account index bound");
                                }
                                Pubkey::from_str(&account_keys[acc_idx]).context("Bad Pubkey")
                            };

                            let mint = get_pubkey(0)?;
                            let bonding_curve = get_pubkey(2)?;
                            let associated_bonding_curve = get_pubkey(3)?;
                            let user = get_pubkey(5)?;

                            // Decode create_v2 args: name/symbol/uri (borsh)
                            let (name, symbol, uri) = crate::detection::pumpfun_filter::decode_create_v2_args(&data)
                                .map(|(n, s, u)| (Some(n), Some(s), Some(u)))
                                .unwrap_or((None, None, None));

                            // Fee payer = first account key
                            let fee_payer = Pubkey::from_str(&account_keys[0]).ok();

                            // Creator balance delta from tx meta
                            let creator_spend_lamports = {
                                let creator_acc_idx = ix.accounts.get(5).copied().unwrap_or(0) as usize;
                                tx.transaction.meta.as_ref().and_then(|meta| {
                                    let pre = meta.pre_balances.get(creator_acc_idx)?;
                                    let post = meta.post_balances.get(creator_acc_idx)?;
                                    Some(pre.saturating_sub(*post))
                                })
                            };

                            return Ok(PumpFunToken {
                                mint,
                                bonding_curve,
                                associated_bonding_curve,
                                user,
                                initial_buy_amount_lamports: 0,
                                virtual_sol_reserves: 0,
                                virtual_token_reserves: 0,
                                detected_at,
                                slot,
                                signature: Some(signature_str.to_string()),
                                name,
                                symbol,
                                uri,
                                creator_spend_lamports,
                                fee_payer,
                            });
                        }
                    }
                }
                anyhow::bail!("Pump.fun instruction not found");
            }
            Err(e) => {
                retries += 1;
                if retries > MAX_RETRIES || Instant::now() >= deadline {
                    PARSE_TIMEOUT_TOTAL.fetch_add(1, Ordering::Relaxed);
                    anyhow::bail!(
                        "Failed fetch pumpfun after {} retries (budget {}ms): {}",
                        retries,
                        TOTAL_BUDGET_MS,
                        e
                    );
                }
                let sleep_ms =
                    (RETRY_SLEEP_BASE_MS * 2_u64.pow(retries as u32 - 1)).min(RETRY_SLEEP_CAP_MS);
                debug!(
                    "Retry {} fetching pumpfun tx (backoff {}ms): {}",
                    retries, sleep_ms, e
                );
                std::thread::sleep(Duration::from_millis(sleep_ms));
            }
        }
    }
}

fn now_us() -> u64 {
    crate::time::monotonic_now_us()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn now_us_is_monotonic_non_decreasing() {
        let t1 = now_us();
        let t2 = now_us();
        assert!(t2 >= t1);
    }

    #[test]
    fn parse_job_has_pumpfun_variant() {
        let job = ParseJob::PumpFun {
            signature: "sig".to_string(),
            slot: 1,
            recv_time: Utc::now(),
            ws_receive_us: 42,
        };
        match job {
            ParseJob::PumpFun {
                slot,
                ws_receive_us,
                ..
            } => {
                assert_eq!(slot, 1);
                assert_eq!(ws_receive_us, 42);
            }
        }
    }

    #[test]
    fn parse_metrics_snapshot_starts_from_zero_or_higher() {
        let first = parse_metrics_snapshot();
        let second = parse_metrics_snapshot();
        assert_eq!(first, second);
    }
}
