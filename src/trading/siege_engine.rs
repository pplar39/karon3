use crate::config::{SafetyConfig, TradingConfig};
use crate::detection::RugChecker;
use crate::trading::jito::JitoSender;
use crate::trading::position::PositionManager;
use crate::types::{LatencyEvent, Pool, Position};
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

/// SiegeTrader: High-Performance Async Trading Engine
/// "Sidecar" module designed to run in parallel with legacy code.
/// Focus: Speed, Async I/O, and Strict Latency Budgeting (Admission Gate).
pub struct SiegeTrader {
    rpc_client: Arc<RpcClient>,
    position_manager: PositionManager,
    rug_checker: RugChecker,
    jito_sender: Option<JitoSender>,
    wallet: Keypair,
    config: TradingConfig,
    latency_budget_us: u64, // The Admission Gate Limit
}

impl SiegeTrader {
    pub fn new(
        rpc_url: String,
        config: TradingConfig,
        safety_config: SafetyConfig,
        wallet: Keypair,
    ) -> Self {
        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        // Jito Sender Initialization (Async-ready)
        let jito_sender = match (config.jito_url.clone(), config.jito_auth_uuid.clone()) {
            (Some(url), Some(uuid)) => {
                Some(JitoSender::new(Some(url), config.tip_amount, Some(uuid)))
            }
            (Some(_), None) => {
                warn!("[Siege] Jito UUID missing, disabling Jito");
                None
            }
            _ => None,
        };

        // Use a default latency budget of 200ms (200,000us) for now if not in config
        // This is aggressive but necessary to beat the 0.5s baseline
        let latency_budget_us = 200_000;

        info!(
            "[Siege] Engine Initialized. ADMISSION_GATE budget: {}us",
            latency_budget_us
        );

        Self {
            rpc_client,
            position_manager: PositionManager::new(config.clone()),
            rug_checker: RugChecker::new(&rpc_url, safety_config, None), // Use passed rpc_url directly
            jito_sender,
            wallet,
            config,
            latency_budget_us,
        }
    }

    /// The Hotpath Entry Point
    /// Returns: Result<Option<Position>> - Some(Position) if trade executed, None if skipped
    pub async fn on_new_pool(
        &self,
        pool: Pool,
        latency: &mut LatencyEvent,
    ) -> Result<Option<Position>> {
        let start = Instant::now();

        // 1. ADMISSION GATE (Gatekeeper)
        // Rejects opportunities that are already too old (RPC + WS latency)
        // Calculated as: Now - DetectedAt
        let detection_age_us = if latency.ws_recv_ts_us > 0 {
            crate::time::monotonic_now_us().saturating_sub(latency.ws_recv_ts_us)
        } else {
            0
        };

        if detection_age_us > self.latency_budget_us {
            warn!(
                "[Siege] ADMISSION DENIED: Latency {}us > Budget {}us. Pool: {}",
                detection_age_us, self.latency_budget_us, pool.amm_id
            );
            return Ok(None);
        }

        debug!(
            "[Siege] ADMISSION GRANTED: Latency {}us. Processing Pool: {}",
            detection_age_us, pool.amm_id
        );

        // 2. Parallel Safety Checks & Balance Fetch (The Async Advantage)
        // RugChecker is currently sync internal, so we might wrap it if needed,
        // but for now we assume it's fast enough or we'll refactor it later.
        // For true speed, we should rely on local cache, but let's do a basic balance check async.

        let balance = self.rpc_client.get_balance(&self.wallet.pubkey()).await?;
        let balance_sol = balance as f64 / 1_000_000_000.0; // Convert lamports to SOL
        let safety = self.rug_checker.check_pool(&pool, latency).await?;

        // 3. Decision Logic
        if !safety.is_safe {
            info!("[Siege] Pool Unsafe: {:?}", safety.warnings);
            return Ok(None);
        }

        const MIN_SOL_BALANCE: f64 = 0.05;
        if balance_sol < MIN_SOL_BALANCE {
            warn!(
                "[Siege] Insufficient Balance: {} < {}",
                balance_sol, MIN_SOL_BALANCE
            );
            return Ok(None);
        }

        // 4. Execution (Buy)
        // This is where we would call Jito or RPC to send the bundle.
        // For now, we'll confirm we got here and just log "Simulated Buy" to prove the engine works
        // without risking funds until fully verified.
        // TODO: Implement actual Swap Logic here reusing `pump_fun_swap` or `swap` modules but async.

        // ... (Buy Logic Placeholder) ...
        info!(
            "[Siege] 🚀 TRIGGER BUY (Simulated) for {} at {}us latency",
            pool.amm_id, detection_age_us
        );

        // Update Latency Metrics
        // latency.total_us not available in struct, just logging for now.
        let total_us = start.elapsed().as_micros() as u64 + detection_age_us;
        info!("[Siege] Total Operation Latency: {}us", total_us);

        Ok(None) // Return None essentially acting as Paper Trader for this first pass
    }

    pub fn get_wallet_pubkey(&self) -> String {
        self.wallet.pubkey().to_string()
    }
}
