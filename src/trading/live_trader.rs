use crate::config::{SafetyConfig, TradingConfig};
use crate::constants::{DEFAULT_JITO_TIP, PUMP_FUN_PROGRAM, WSOL_MINT};
use crate::detection::{PumpFunFilterEngine, RugChecker};
use crate::trading::omega_trinity::{OmegaTrinity, TradeContext};
use crate::trading::jito::{
    is_jito_emergency_stop_active, record_preflight_fail, validate_bundle_tip_lock,
};
use crate::trading::pump_fun_swap::PumpFunSwapBuilder;
use crate::trading::safety::{CircuitBreaker, CircuitBreakerConfig};
use crate::trading::{BlockhashCache, JitoSender, PositionManager, SafetyGuardrails};
use crate::types::{LatencyEvent, Pool, Position, PumpFunToken, TradeAction, TradeResult};
use anyhow::{Context, Result};
use chrono::Utc;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::message::{v0, VersionedMessage};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::system_instruction;
use solana_sdk::transaction::VersionedTransaction;
use spl_associated_token_account::instruction as ata_instruction;
use spl_token::instruction as token_instruction;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

const WSOL_ACCOUNT_RENT: u64 = 2_039_280;
const COMPUTE_UNIT_LIMIT: u32 = 200_000;
const COMPUTE_UNIT_PRICE: u64 = 100_000;

pub struct LiveTrader {
    position_manager: PositionManager,
    rug_checker: RugChecker,
    rpc_client: Arc<RpcClient>,
    jito_sender: JitoSender,
    wallet: Arc<Keypair>,
    trades: Vec<TradeResult>,
    buy_amount_lamports: u64,
    slippage_bps: u64,
    trading_config: TradingConfig,
    // Shadow mode: simulate trades without sending real transactions
    shadow_mode: bool,
    guardrails: SafetyGuardrails,
    /// Blockhash cache - eliminates RPC call in hot path
    blockhash_cache: Arc<BlockhashCache>,
    /// Helius RPC URL for direct sendTransaction (Option 3: parallel fan-out)
    rpc_url: String,
    /// OMEGA TRINITY: admission (budget/rps/circuit) + accounting
    omega_trinity: OmegaTrinity,
    /// Pump.fun token quality filter (Phase 1)
    pumpfun_filter: PumpFunFilterEngine,
}

impl LiveTrader {
    pub fn new(
        rpc_url: &str,
        trading_config: TradingConfig,
        safety_config: SafetyConfig,
        wallet: Keypair,
    ) -> Self {
        let breaker_toml = safety_config.circuit_breaker.clone();
        let shadow_mode = trading_config.shadow_mode.unwrap_or(false);

        let rpc_client = Arc::new(RpcClient::new_with_commitment(
            rpc_url.to_string(),
            CommitmentConfig::confirmed(),
        ));

        // Initialize blockhash cache with background refresh
        let blockhash_cache = BlockhashCache::new(rpc_url);
        if shadow_mode {
            info!("👻 SHADOW MODE ENABLED - No real transactions will be sent");
        } else {
            info!("BlockhashCache initialized with background refresh");
        }

        // Critical: Jito HTTP auth uses `x-jito-auth` UUID (not a keypair path).
        // Also do not hardcode a tiny tip here; let config override, otherwise use DEFAULT_JITO_TIP.
        let mut jito_sender = JitoSender::new(
            trading_config.jito_url.clone(),
            trading_config.tip_amount,
            trading_config.jito_auth_uuid.clone(),
        );

        if let Some(dont_front) = trading_config.jito_dont_front {
            jito_sender.set_dont_front(dont_front);
        }

        let buy_amount_lamports = (trading_config.buy_amount_sol * 1_000_000_000.0) as u64;
        let slippage_bps = trading_config.slippage_bps.unwrap_or(500);

        let mut guardrails = SafetyGuardrails::new(
            trading_config.max_sol_spend.unwrap_or(10.0),
            trading_config.max_trades.unwrap_or(5),
            trading_config.min_balance_sol.unwrap_or(0.5),
            trading_config.error_rate_threshold.unwrap_or(0.5),
        );

        // Apply max daily loss from safety config
        if let Some(limit) = safety_config.max_daily_loss_sol {
            guardrails = guardrails.with_max_daily_loss(limit);
        }

        if let Some(toml) = breaker_toml {
            let cfg = CircuitBreakerConfig::from_toml(&toml);
            guardrails = guardrails.with_circuit_breaker(CircuitBreaker::new(cfg));
        }

        // OMEGA TRINITY: budget/rps/circuit admission gate.
        // min_safety_score=0 recommended (rugcheck is authoritative via EntryGuard bypass).
        let omega_trinity = if let Some(ref cfg) = safety_config.omega_trinity {
            OmegaTrinity::new(
                cfg.min_safety_score.unwrap_or(0),
                cfg.budget_sol
                    .unwrap_or(trading_config.max_sol_spend.unwrap_or(10.0)),
                cfg.max_rps.unwrap_or(10),
                cfg.max_drawdown_percent.unwrap_or(5.0),
                cfg.cooldown_secs.unwrap_or(300),
                cfg.window_secs.unwrap_or(600),
            )
        } else {
            OmegaTrinity::new(
                0,
                trading_config.max_sol_spend.unwrap_or(10.0),
                10,
                5.0,
                300,
                600,
            )
        };
        info!(
            "OmegaTrinity armed: budget_sol={:.4} max_rps={} max_drawdown_pct={:.1}%",
            omega_trinity.resource_pulse.budget_sol,
            omega_trinity.resource_pulse.max_rps,
            omega_trinity.circuit_breaker.max_drawdown_pct,
        );

        let pumpfun_filter =
            PumpFunFilterEngine::new(safety_config.pumpfun_filter.as_ref());

        Self {
            position_manager: PositionManager::new(trading_config.clone()),
            rug_checker: RugChecker::new(rpc_url, safety_config, None),
            rpc_client,
            jito_sender,
            wallet: Arc::new(wallet),
            trades: Vec::new(),
            buy_amount_lamports,
            slippage_bps,
            trading_config,
            guardrails,
            blockhash_cache,
            shadow_mode,
            rpc_url: rpc_url.to_string(),
            omega_trinity,
            pumpfun_filter,
        }
    }

    pub fn is_shadow_mode(&self) -> bool {
        self.shadow_mode
    }

    pub fn hydrate_daily_loss(&self, initial_loss: f64) {
        self.guardrails.hydrate_daily_loss(initial_loss);
    }

    /// Estimated total cost for one buy: buy_amount + tip + base fee.
    fn omega_estimated_cost_sol(&self) -> f64 {
        let buy_sol = self.trading_config.buy_amount_sol;
        let tip_lamports = self.trading_config.tip_amount.unwrap_or(DEFAULT_JITO_TIP);
        let tip_sol = tip_lamports as f64 / 1_000_000_000.0;
        buy_sol + tip_sol
    }

    pub async fn on_new_pool(
        &mut self,
        pool: Pool,
        latency: &mut LatencyEvent,
    ) -> Result<Option<Position>> {
        if let Err(msg) = self.guardrails.check_circuit_breaker() {
            anyhow::bail!(msg);
        }

        if !self.trading_config.buy_enabled.unwrap_or(true) {
            anyhow::bail!("buy_enabled=false; buy blocked (sell-only mode)");
        }

        // OMEGA TRINITY BUY admission (budget/rps/circuit)
        let est_cost_sol = self.omega_estimated_cost_sol();
        let omega_ctx = TradeContext {
            safety_score: 100, // rugcheck is authoritative; EntryGuard disabled via min_safety_score=0
            estimated_cost_sol: est_cost_sol,
            current_balance_sol: 0.0,
        };
        if let Err(reason) = self.omega_trinity.should_trade(&omega_ctx) {
            warn!(
                "OMEGA TRINITY REJECTED (NewPool): {} (cost_sol={:.6})",
                reason, est_cost_sol
            );
            return Ok(None);
        }
        self.omega_trinity.resource_pulse.record_attempt();

        info!("Evaluating pool for live trade: {:?}", pool.amm_id);

        let safety = self.rug_checker.check_pool(&pool, latency).await?;

        if !safety.is_safe {
            warn!("Pool failed safety checks: {:?}", safety.warnings);
            return Ok(None);
        }

        info!("Pool passed safety. Executing live buy...");

        match self.execute_buy(&pool, latency).await {
            Ok(bundle_id) => {
                info!("Buy bundle accepted: {}", bundle_id);

                // Verify bundle actually landed on-chain
                let landed = self.verify_bundle_landed(&bundle_id).await;
                if !landed {
                    warn!("Bundle {} accepted but NOT landed on-chain — no position opened", bundle_id);
                    return Ok(None);
                }

                info!("Bundle {} confirmed on-chain", bundle_id);
                let price = self.estimate_token_price(&pool);
                let position = self.position_manager.open_position(pool.clone(), price);

                let trade = TradeResult {
                    position_id: position.id.clone(),
                    action: TradeAction::Buy,
                    price_sol: price,
                    amount_sol: self.buy_amount_lamports as f64 / 1_000_000_000.0,
                    pnl_percent: 0.0,
                    pnl_sol: 0.0,
                    timestamp: Utc::now(),
                    reason: format!("Live buy via Jito: {}", bundle_id),
                };
                self.trades.push(trade);

                self.guardrails.record_trade();
                self.guardrails.record_spend(est_cost_sol);
                self.omega_trinity.resource_pulse.record_spend(est_cost_sol);

                Ok(Some(position))
            }
            Err(e) => {
                error!("Buy execution failed: {}", e);
                self.guardrails.record_error();
                Err(e)
            }
        }
    }

    pub async fn on_new_pumpfun_token(
        &mut self,
        token: PumpFunToken,
        latency: &mut LatencyEvent,
    ) -> Result<Option<Position>> {
        // ── PUMP.FUN QUALITY FILTER (runs even in sell-only mode for shadow calibration) ──
        use crate::detection::pumpfun_filter::FilterAction;
        let filter_result = self.pumpfun_filter.evaluate(&token);
        if filter_result.action == FilterAction::Deny {
            let reason = filter_result
                .reasons
                .first()
                .map(|r| format!("{}:{}", r.filter, r.detail))
                .unwrap_or_default();
            warn!(
                "PUMPFUN_FILTER_DENIED: mint={} creator={} reason={} latency_us={}",
                token.mint, token.user, reason, filter_result.latency_us
            );
            return Ok(None);
        }

        if let Err(msg) = self.guardrails.check_circuit_breaker() {
            anyhow::bail!(msg);
        }
        if is_jito_emergency_stop_active() {
            anyhow::bail!("Jito emergency_stop active; buy blocked");
        }
        if !self.trading_config.buy_enabled.unwrap_or(true) {
            anyhow::bail!("buy_enabled=false; buy blocked (sell-only mode)");
        }

        // OMEGA TRINITY BUY admission (budget/rps/circuit)
        let est_cost_sol = self.omega_estimated_cost_sol();
        let omega_ctx = TradeContext {
            safety_score: 100,
            estimated_cost_sol: est_cost_sol,
            current_balance_sol: 0.0,
        };
        if let Err(reason) = self.omega_trinity.should_trade(&omega_ctx) {
            warn!(
                "OMEGA TRINITY REJECTED (NewPumpFunToken): {} (cost_sol={:.6})",
                reason, est_cost_sol
            );
            return Ok(None);
        }
        self.omega_trinity.resource_pulse.record_attempt();

        info!("Evaluating Pump.fun token: {:?}", token.mint);

        match self.execute_pump_buy(&token, latency).await {
            Ok(bundle_id) => {
                info!("Pump.fun buy bundle accepted: {}", bundle_id);

                // Verify bundle actually landed on-chain
                let landed = self.verify_bundle_landed(&bundle_id).await;
                if !landed {
                    warn!("Bundle {} accepted but NOT landed on-chain — no position opened", bundle_id);
                    return Ok(None);
                }

                info!("Bundle {} confirmed on-chain", bundle_id);
                let price = Self::estimate_pump_price(&token);
                let pool = Self::pumpfun_token_as_pool(&token);
                let position = self.position_manager.open_position(pool, price);

                let trade = TradeResult {
                    position_id: position.id.clone(),
                    action: TradeAction::Buy,
                    price_sol: price,
                    amount_sol: self.buy_amount_lamports as f64 / 1_000_000_000.0,
                    pnl_percent: 0.0,
                    pnl_sol: 0.0,
                    timestamp: Utc::now(),
                    reason: format!("Pump.fun buy via Jito: {}", bundle_id),
                };
                self.trades.push(trade);

                self.guardrails.record_trade();
                self.guardrails.record_spend(est_cost_sol);
                self.omega_trinity.resource_pulse.record_spend(est_cost_sol);

                Ok(Some(position))
            }
            Err(e) => {
                error!("Pump.fun buy failed: {}", e);
                self.guardrails.record_error();
                Err(e)
            }
        }
    }

    pub fn on_sell_executed(&self, trade_result: &TradeResult) {
        self.guardrails.record_pnl_percent(trade_result.pnl_percent);
        self.guardrails.record_trade();
        // Feed sell PnL into OmegaBreaker circuit
        self.omega_trinity.circuit_breaker.record_pnl(trade_result.pnl_sol);
    }

    pub fn on_trade_error(&self) {
        self.guardrails.record_error();
    }

    /// PumpFunToken → Pool 변환 (Position 추적용 bridge)
    /// Vault fields are Pubkey::default() — Pump.fun uses bonding curve PDA, not vaults.
    fn pumpfun_token_as_pool(token: &PumpFunToken) -> Pool {
        Pool {
            amm_id: token.bonding_curve,
            base_mint: token.mint,
            quote_mint: *WSOL_MINT,
            base_vault: Pubkey::default(),
            quote_vault: Pubkey::default(),
            lp_mint: Pubkey::default(),
            open_orders: Pubkey::default(),
            target_orders: Pubkey::default(),
            market_id: Pubkey::default(),
            base_decimals: 6,
            quote_decimals: 9,
            detected_at: token.detected_at,
            slot: token.slot,
        }
    }

    /// Pump.fun bonding curve 가격 계산 (virtual reserves 기반)
    fn estimate_pump_price(token: &PumpFunToken) -> f64 {
        if token.virtual_token_reserves == 0 {
            return 0.001;
        }
        // SOL per token = virtual_sol_reserves / virtual_token_reserves
        (token.virtual_sol_reserves as f64) / (token.virtual_token_reserves as f64)
    }

    /// PumpFunToken에서 직접 buy transaction 빌드 + Jito 전송
    async fn execute_pump_buy(
        &self,
        token: &PumpFunToken,
        latency: &mut LatencyEvent,
    ) -> Result<String> {
        // GUARDRAIL: check spend cap, max trades, balance BEFORE building tx
        let balance_sol = self.get_sol_balance().unwrap_or(0.0);
        if let Err(msg) = self.guardrails.can_open_position(
            self.trading_config.buy_amount_sol,
            balance_sol,
        ) {
            anyhow::bail!("Guardrail blocked: {}", msg);
        }

        latency.tx_build_start_us = crate::time::monotonic_now_us();

        let recent_blockhash = self
            .blockhash_cache
            .get_blockhash()
            .await
            .ok_or_else(|| anyhow::anyhow!("Blockhash cache not ready"))?;

        let wsol_keypair = Keypair::new();
        let wsol_account = wsol_keypair.pubkey();
        let token_account = self.get_ata_address(&token.mint);

        let mut instructions = vec![];

        let cu_limit = self
            .trading_config
            .compute_unit_limit
            .unwrap_or(COMPUTE_UNIT_LIMIT);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));

        let cu_price = self.get_cu_price(&[token.mint]);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(cu_price));

        // ATA 생성 (idempotent) — Pump.fun Token-2022 mint
        instructions.push(
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &self.wallet.pubkey(),
                &self.wallet.pubkey(),
                &token.mint,
                &spl_token_2022::id(),
            ),
        );

        // WSOL 계정 생성
        let total_sol_needed = self.buy_amount_lamports + WSOL_ACCOUNT_RENT;
        instructions.push(system_instruction::create_account(
            &self.wallet.pubkey(),
            &wsol_account,
            total_sol_needed,
            165,
            &spl_token::id(),
        ));

        instructions.push(token_instruction::initialize_account(
            &spl_token::id(),
            &wsol_account,
            &WSOL_MINT,
            &self.wallet.pubkey(),
        )?);

        // Pump.fun swap instruction (PumpFunToken 직접 사용)
        let swap_builder = PumpFunSwapBuilder::new(token.clone());
        let raw_token_amount = PumpFunSwapBuilder::calculate_tokens_for_sol(self.buy_amount_lamports);
        // Reduce min expected tokens: creator initial buy + protocol fees + slippage
        // Initial reserves assume empty pool, but creator already bought => price is higher
        let effective_slippage = self.slippage_bps.max(1500); // at least 15% for sniping
        let token_amount = raw_token_amount * (10000u64.saturating_sub(effective_slippage)) / 10000;
        let swap_ix = swap_builder.build_buy_instruction(
            token_amount,
            self.buy_amount_lamports,
            &self.wallet.pubkey(),
            &token_account,
        )?;
        instructions.push(swap_ix);

        // WSOL 계정 닫기
        instructions.push(token_instruction::close_account(
            &spl_token::id(),
            &wsol_account,
            &self.wallet.pubkey(),
            &self.wallet.pubkey(),
            &[],
        )?);

        // Jito tip
        instructions.push(
            self.jito_sender
                .create_tip_instruction(&self.wallet.pubkey()),
        );

        let message =
            v0::Message::try_compile(&self.wallet.pubkey(), &instructions, &[], recent_blockhash)?;

        let tx = if self.trading_config.parallel_signing.unwrap_or(false) {
            let wallet = self.wallet.clone();
            let wsol_keypair_bytes = wsol_keypair.to_bytes();

            tokio::task::spawn_blocking(move || {
                let wsol_keypair = Keypair::from_bytes(&wsol_keypair_bytes).unwrap();
                VersionedTransaction::try_new(
                    VersionedMessage::V0(message),
                    &[&*wallet, &wsol_keypair],
                )
            })
            .await?
            .context("Failed to sign transaction in parallel")?
        } else {
            VersionedTransaction::try_new(
                VersionedMessage::V0(message),
                &[&*self.wallet, &wsol_keypair],
            )?
        };

        latency.tx_signed_us = crate::time::monotonic_now_us();

        // Shadow mode
        if self.shadow_mode {
            let shadow_id = format!("SHADOW_PUMP_{}", uuid::Uuid::new_v4());
            info!(
                "Shadow mode: Would send Pump.fun buy bundle, simulated ID: {}",
                shadow_id
            );
            latency.jito_send_start_us = latency.tx_signed_us;
            latency.jito_send_end_us = latency.tx_signed_us + 1;
            return Ok(shadow_id);
        }

        if !validate_bundle_tip_lock(&tx) {
            record_preflight_fail();
            anyhow::bail!(
                "Tip write-lock preflight failed: no Jito tip account writable in pump buy tx"
            );
        }

        latency.jito_send_start_us = crate::time::monotonic_now_us();

        let tx_id = self.fan_out_send(&tx).await?;

        latency.jito_send_end_us = crate::time::monotonic_now_us();

        Ok(tx_id)
    }

    /// Pool → PumpFunToken 변환 (on_new_pool 경로 전용)
    /// bonding_curve는 mint에서 PDA 파생 (직접 매핑 금지)
    fn pool_as_pumpfun_token(&self, pool: &Pool) -> PumpFunToken {
        let (bonding_curve, associated_bonding_curve) =
            PumpFunToken::derive_bonding_curve(&pool.base_mint, &PUMP_FUN_PROGRAM);
        PumpFunToken {
            mint: pool.base_mint,
            bonding_curve,
            associated_bonding_curve,
            user: self.wallet.pubkey(),
            initial_buy_amount_lamports: 0,
            virtual_sol_reserves: 0,
            virtual_token_reserves: 0,
            detected_at: pool.detected_at,
            slot: pool.slot,
            signature: None,
            name: None,
            symbol: None,
            uri: None,
            creator_spend_lamports: None,
            fee_payer: None,
        }
    }

    async fn execute_buy(&self, pool: &Pool, latency: &mut LatencyEvent) -> Result<String> {
        if is_jito_emergency_stop_active() {
            anyhow::bail!("Jito emergency_stop active; buy blocked");
        }

        // GUARDRAIL: check spend cap, max trades, balance BEFORE building tx
        let balance_sol = self.get_sol_balance().unwrap_or(0.0);
        if let Err(msg) = self.guardrails.can_open_position(
            self.trading_config.buy_amount_sol,
            balance_sol,
        ) {
            anyhow::bail!("Guardrail blocked: {}", msg);
        }

        latency.tx_build_start_us = crate::time::monotonic_now_us();

        let recent_blockhash = self
            .blockhash_cache
            .get_blockhash()
            .await
            .ok_or_else(|| anyhow::anyhow!("Blockhash cache not ready"))?;

        debug!("Using cached blockhash: {}", recent_blockhash);

        let wsol_keypair = Keypair::new();
        let wsol_account = wsol_keypair.pubkey();
        let token = self.pool_as_pumpfun_token(pool);
        let token_account = self.get_ata_address(&token.mint);

        let mut instructions = vec![];

        let cu_limit = self
            .trading_config
            .compute_unit_limit
            .unwrap_or(COMPUTE_UNIT_LIMIT);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));

        let cu_price = self.get_cu_price(&[token.mint]);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(cu_price));

        let use_precreate = self.trading_config.precreate_ata.unwrap_or(true); // Default true for latency

        if use_precreate {
            instructions.push(spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &self.wallet.pubkey(),
                &self.wallet.pubkey(),
                &token.mint,
                &spl_token_2022::id(),
            ));
        } else {
            if self.rpc_client.get_account(&token_account).is_err() {
                instructions.push(ata_instruction::create_associated_token_account(
                    &self.wallet.pubkey(),
                    &self.wallet.pubkey(),
                    &token.mint,
                    &spl_token_2022::id(),
                ));
            }
        }

        let total_sol_needed = self.buy_amount_lamports + WSOL_ACCOUNT_RENT;

        instructions.push(system_instruction::create_account(
            &self.wallet.pubkey(),
            &wsol_account,
            total_sol_needed,
            165,
            &spl_token::id(),
        ));

        instructions.push(token_instruction::initialize_account(
            &spl_token::id(),
            &wsol_account,
            &WSOL_MINT,
            &self.wallet.pubkey(),
        )?);

        let swap_builder = PumpFunSwapBuilder::new(token);
        let raw_token_amount = PumpFunSwapBuilder::calculate_tokens_for_sol(self.buy_amount_lamports);
        let effective_slippage = self.slippage_bps.max(1500);
        let token_amount = raw_token_amount * (10000u64.saturating_sub(effective_slippage)) / 10000;
        let swap_ix = swap_builder.build_buy_instruction(
            token_amount,
            self.buy_amount_lamports,
            &self.wallet.pubkey(),
            &token_account,
        )?;
        instructions.push(swap_ix);

        instructions.push(token_instruction::close_account(
            &spl_token::id(),
            &wsol_account,
            &self.wallet.pubkey(),
            &self.wallet.pubkey(),
            &[],
        )?);

        instructions.push(
            self.jito_sender
                .create_tip_instruction(&self.wallet.pubkey()),
        );

        let message =
            v0::Message::try_compile(&self.wallet.pubkey(), &instructions, &[], recent_blockhash)?;

        let tx = if self.trading_config.parallel_signing.unwrap_or(false) {
            let wallet = self.wallet.clone();
            let wsol_keypair_bytes = wsol_keypair.to_bytes();

            tokio::task::spawn_blocking(move || {
                let wsol_keypair = Keypair::from_bytes(&wsol_keypair_bytes).unwrap();
                VersionedTransaction::try_new(
                    VersionedMessage::V0(message),
                    &[&*wallet, &wsol_keypair],
                )
            })
            .await?
            .context("Failed to sign transaction in parallel")?
        } else {
            VersionedTransaction::try_new(
                VersionedMessage::V0(message),
                &[&*self.wallet, &wsol_keypair],
            )?
        };

        latency.tx_signed_us = crate::time::monotonic_now_us();

        // Shadow mode
        if self.shadow_mode {
            let shadow_id = format!("SHADOW_{}", uuid::Uuid::new_v4());
            info!(
                "Shadow mode: Would send bundle, simulated ID: {}",
                shadow_id
            );
            latency.jito_send_start_us = latency.tx_signed_us;
            latency.jito_send_end_us = latency.tx_signed_us + 1;
            return Ok(shadow_id);
        }

        if !validate_bundle_tip_lock(&tx) {
            record_preflight_fail();
            anyhow::bail!(
                "Tip write-lock preflight failed: no Jito tip account writable in buy tx"
            );
        }

        latency.jito_send_start_us = crate::time::monotonic_now_us();

        let tx_id = self.fan_out_send(&tx).await?;

        latency.jito_send_end_us = crate::time::monotonic_now_us();

        Ok(tx_id)
    }

    /// Read bonding curve account to extract the token creator pubkey.
    /// Layout: 8(disc) + 5×u64(40) + 1(bool) = offset 49, then 32 bytes = creator.
    fn fetch_bonding_curve_creator(&self, mint: &Pubkey) -> Result<Pubkey> {
        let (bonding_curve, _) =
            PumpFunToken::derive_bonding_curve(mint, &PUMP_FUN_PROGRAM);
        let account = self
            .rpc_client
            .get_account(&bonding_curve)
            .context("Failed to read bonding curve account")?;
        if account.data.len() < 81 {
            anyhow::bail!(
                "Bonding curve account too short: {} bytes (need >=81)",
                account.data.len()
            );
        }
        let creator_bytes: [u8; 32] = account.data[49..81]
            .try_into()
            .context("Failed to parse creator from bonding curve")?;
        Ok(Pubkey::new_from_array(creator_bytes))
    }

    pub async fn execute_sell(&self, position: &Position) -> Result<String> {
        self.execute_sell_mint(&position.pool.base_mint, 0, true).await
    }

    /// Unified sell: works for both position-based and dump-all flows.
    /// min_sol_output=0 means fire-sale (accept any price).
    /// include_tip=false saves 0.03 SOL per sell (for dump-all fire sales).
    pub async fn execute_sell_mint(&self, mint: &Pubkey, min_sol_output: u64, include_tip: bool) -> Result<String> {
        if let Err(msg) = self.guardrails.check_circuit_breaker() {
            anyhow::bail!(msg);
        }
        if is_jito_emergency_stop_active() {
            anyhow::bail!("Jito emergency_stop active; sell blocked");
        }

        // Fetch creator from on-chain bonding curve
        let creator = self.fetch_bonding_curve_creator(mint)?;
        let (bonding_curve, associated_bonding_curve) =
            PumpFunToken::derive_bonding_curve(mint, &PUMP_FUN_PROGRAM);

        let token = PumpFunToken {
            mint: *mint,
            bonding_curve,
            associated_bonding_curve,
            user: creator,
            initial_buy_amount_lamports: 0,
            virtual_sol_reserves: 0,
            virtual_token_reserves: 0,
            detected_at: Utc::now(),
            slot: 0,
            signature: None,
            name: None,
            symbol: None,
            uri: None,
            creator_spend_lamports: None,
            fee_payer: None,
        };

        let recent_blockhash = self
            .blockhash_cache
            .get_blockhash()
            .await
            .ok_or_else(|| anyhow::anyhow!("Blockhash cache not ready"))?;

        let token_account = self.get_ata_address(mint);

        let token_balance = self.fetch_token_balance(&token_account)?;
        if token_balance == 0 {
            anyhow::bail!("No tokens to sell");
        }

        let mut instructions = vec![];

        let cu_limit = self
            .trading_config
            .compute_unit_limit
            .unwrap_or(COMPUTE_UNIT_LIMIT);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));

        let cu_price = self.get_cu_price(&[*mint]);
        instructions.push(ComputeBudgetInstruction::set_compute_unit_price(cu_price));

        let swap_builder = PumpFunSwapBuilder::new(token);
        let swap_ix = swap_builder.build_sell_args(
            token_balance,
            min_sol_output,
            &self.wallet.pubkey(),
            &token_account,
        )?;
        instructions.push(swap_ix);

        if include_tip {
            instructions.push(
                self.jito_sender
                    .create_tip_instruction(&self.wallet.pubkey()),
            );
        }

        let message =
            v0::Message::try_compile(&self.wallet.pubkey(), &instructions, &[], recent_blockhash)?;

        let tx = VersionedTransaction::try_new(
            VersionedMessage::V0(message),
            &[&*self.wallet],
        )?;

        // Shadow mode
        if self.shadow_mode {
            let shadow_id = format!("SHADOW_SELL_{}", uuid::Uuid::new_v4());
            info!(
                "Shadow mode: Would send sell bundle, simulated ID: {}",
                shadow_id
            );
            return Ok(shadow_id);
        }

        if include_tip && !validate_bundle_tip_lock(&tx) {
            record_preflight_fail();
            anyhow::bail!(
                "Tip write-lock preflight failed: no Jito tip account writable in sell tx"
            );
        }

        // For tipless fire-sales, send only via direct RPC (no Jito)
        let tx_id = if include_tip {
            self.fan_out_send(&tx).await?
        } else {
            let serialized = bincode::serialize(&tx)
                .map_err(|e| anyhow::anyhow!("Failed to serialize tx: {}", e))?;
            let base64_tx = {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode(&serialized)
            };
            let client = self.jito_sender.get_client().clone();
            let rpc_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "sendTransaction",
                "params": [base64_tx, {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 3
                }]
            });
            let resp = client
                .post(&self.rpc_url)
                .header("Content-Type", "application/json")
                .json(&rpc_body)
                .send()
                .await
                .context("RPC sendTransaction failed")?;
            let body: serde_json::Value = resp
                .json()
                .await
                .context("Failed to parse RPC response")?;
            if let Some(err) = body.get("error") {
                anyhow::bail!("RPC error: {}", err);
            }
            body["result"]
                .as_str()
                .unwrap_or("unknown")
                .to_string()
        };

        Ok(tx_id)
    }

    /// Dump all Token-2022 positions (fire-sale). Used for emergency liquidation.
    /// Scans wallet for all Token-2022 token accounts, sells any with non-zero balance.
    pub async fn dump_all_positions(&self) -> (usize, usize) {
        use solana_client::rpc_request::TokenAccountsFilter;

        info!("DUMP ALL: Scanning wallet for Token-2022 positions...");

        let accounts = match self.rpc_client.get_token_accounts_by_owner(
            &self.wallet.pubkey(),
            TokenAccountsFilter::ProgramId(spl_token_2022::id()),
        ) {
            Ok(a) => a,
            Err(e) => {
                error!("Failed to fetch token accounts: {}", e);
                return (0, 0);
            }
        };

        // Collect mints with non-zero balances
        let mut mints_to_sell: Vec<Pubkey> = Vec::new();
        for keyed_account in &accounts {
            let ata_pubkey: Pubkey = match keyed_account.pubkey.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            match self.rpc_client.get_token_account(&ata_pubkey) {
                Ok(Some(ta)) => {
                    let balance = ta.token_amount.amount.parse::<u64>().unwrap_or(0);
                    if balance == 0 {
                        continue;
                    }
                    if let Ok(mint) = ta.mint.parse::<Pubkey>() {
                        info!(
                            "Found position: mint={} balance={}",
                            mint,
                            ta.token_amount.ui_amount_string
                        );
                        mints_to_sell.push(mint);
                    }
                }
                _ => continue,
            }
        }

        info!("Found {} non-zero Token-2022 positions to dump", mints_to_sell.len());

        let mut success = 0usize;
        let mut failed = 0usize;
        let total = mints_to_sell.len();

        for (i, mint) in mints_to_sell.iter().enumerate() {
            info!("DUMP [{}/{}]: Selling {}", i + 1, total, mint);

            match self.execute_sell_mint(mint, 0, false).await {
                Ok(tx_id) => {
                    info!("SOLD [{}/{}] {} -> tx {}", i + 1, total, mint, tx_id);
                    success += 1;
                }
                Err(e) => {
                    warn!("FAIL [{}/{}] {}: {}", i + 1, total, mint, e);
                    failed += 1;
                }
            }

            // 500ms between sells to avoid rate limiting
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }

        info!(
            "DUMP ALL complete: {} sold, {} failed out of {} positions",
            success, failed, total
        );
        (success, failed)
    }

    fn get_cu_price(&self, accounts: &[Pubkey]) -> u64 {
        let mut price = self
            .trading_config
            .compute_unit_price
            .unwrap_or(COMPUTE_UNIT_PRICE);

        if self.trading_config.dynamic_cu.unwrap_or(false) {
            if let Ok(fees) = self.rpc_client.get_recent_prioritization_fees(accounts) {
                if !fees.is_empty() {
                    let mut prices: Vec<u64> =
                        fees.into_iter().map(|f| f.prioritization_fee).collect();
                    prices.sort_unstable();
                    let mid = prices.len() / 2;
                    price = if prices.len() % 2 == 0 {
                        (prices[mid - 1] + prices[mid]) / 2
                    } else {
                        prices[mid]
                    };
                    debug!(
                        "Dynamic CU Price (median of {} blocks): {} microlamports",
                        prices.len(),
                        price
                    );
                }
            }
        }
        price
    }

    pub fn update_positions(
        &mut self,
        price_updates: Vec<(String, f64)>,
    ) -> Vec<(Position, TradeResult)> {
        let mut to_sell = vec![];

        for (position_id, new_price) in price_updates {
            self.position_manager.update_price(&position_id, new_price);

            if let Some(trade_result) = self.position_manager.check_exit_conditions(&position_id) {
                if let Some(position) = self.position_manager.get_position(&position_id).cloned() {
                    to_sell.push((position, trade_result));
                }
            }
        }

        to_sell
    }

    fn get_ata_address(&self, mint: &Pubkey) -> Pubkey {
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &self.wallet.pubkey(),
            mint,
            &spl_token_2022::id(),
        )
    }

    fn fetch_token_balance(&self, token_account: &Pubkey) -> Result<u64> {
        match self.rpc_client.get_token_account_balance(token_account) {
            Ok(balance) => {
                let amount = balance.amount.parse::<u64>().unwrap_or(0);
                Ok(amount)
            }
            Err(_) => Ok(0),
        }
    }

    fn estimate_token_price(&self, pool: &Pool) -> f64 {
        let base_balance = self.fetch_vault_balance(&pool.base_vault);
        let quote_balance = self.fetch_vault_balance(&pool.quote_vault);

        if base_balance <= 0.0 {
            return 0.001;
        }

        quote_balance / base_balance
    }

    fn fetch_vault_balance(&self, vault: &Pubkey) -> f64 {
        match self.rpc_client.get_token_account_balance(vault) {
            Ok(balance) => balance.ui_amount.unwrap_or(0.0),
            Err(_) => 0.0,
        }
    }

    /// Poll RPC getSignatureStatuses to confirm on-chain landing.
    /// Works for both Jito sendTransaction and direct RPC sendTransaction paths.
    async fn verify_bundle_landed(&self, tx_sig: &str) -> bool {
        use solana_sdk::signature::Signature;
        let sig = match tx_sig.parse::<Signature>() {
            Ok(s) => s,
            Err(_) => {
                warn!("Invalid tx signature format: {}", tx_sig);
                return false;
            }
        };

        // Poll up to 10 times over ~15 seconds (1.5s intervals)
        for attempt in 1..=10 {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            match self.rpc_client.get_signature_status(&sig) {
                Ok(Some(Ok(()))) => {
                    info!(
                        "TX {} confirmed on-chain (attempt {}/10)",
                        tx_sig, attempt
                    );
                    return true;
                }
                Ok(Some(Err(e))) => {
                    warn!(
                        "TX {} failed on-chain: {} (attempt {}/10)",
                        tx_sig, e, attempt
                    );
                    return false;
                }
                Ok(None) => {
                    // Not yet confirmed — keep polling
                }
                Err(e) => {
                    warn!(
                        "Signature status check failed (attempt {}/10): {}",
                        attempt, e
                    );
                }
            }
        }
        warn!("TX {} not confirmed after 15s — treating as NOT landed", tx_sig);
        false
    }

    /// Parallel fan-out: send same signed TX via multiple paths simultaneously.
    /// Path A: Jito sendTransaction?bundleOnly=true (separate rate limit bucket)
    /// Path B: Helius direct RPC sendTransaction (no Jito rate limit at all)
    /// Returns first success. Solana deduplicates identical TX (same signature).
    async fn fan_out_send(&self, tx: &VersionedTransaction) -> Result<String> {
        let serialized = bincode::serialize(tx)
            .map_err(|e| anyhow::anyhow!("Failed to serialize tx: {}", e))?;
        let base64_tx = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&serialized)
        };

        // Path A: Jito sendTransaction?bundleOnly=true
        let jito_fut = self.jito_sender.send_transaction_jito(tx);

        // Path B: Helius direct RPC sendTransaction
        let rpc_url = self.rpc_url.clone();
        let client = self.jito_sender.get_client().clone();
        let rpc_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [base64_tx, {
                "encoding": "base64",
                "skipPreflight": true,
                "maxRetries": 0
            }]
        });
        let rpc_fut = async {
            let resp = client
                .post(&rpc_url)
                .header("Content-Type", "application/json")
                .json(&rpc_body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Helius RPC send error: {}", e))?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("Helius RPC {}: {}", status, body);
            }
            let resp: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if let Some(err) = resp.get("error") {
                anyhow::bail!("Helius RPC error: {}", err);
            }
            let sig = resp["result"].as_str().unwrap_or("unknown").to_string();
            info!("Helius direct RPC sendTransaction OK: {}", sig);
            Ok::<String, anyhow::Error>(sig)
        };

        // Race both paths — first success wins
        let (jito_result, rpc_result) = tokio::join!(jito_fut, rpc_fut);

        // Return first success
        match (jito_result, rpc_result) {
            (Ok(sig), _) => {
                info!("Fan-out: Jito path succeeded: {}", sig);
                Ok(sig)
            }
            (_, Ok(sig)) => {
                info!("Fan-out: Helius direct path succeeded: {}", sig);
                Ok(sig)
            }
            (Err(e1), Err(e2)) => {
                warn!("Fan-out: both paths failed — Jito: {} | Helius: {}", e1, e2);
                Err(anyhow::anyhow!(
                    "Fan-out failed — Jito: {} | Helius: {}",
                    e1,
                    e2
                ))
            }
        }
    }

    pub fn get_all_trades(&self) -> &[TradeResult] {
        &self.trades
    }

    pub fn get_wallet_pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    pub fn get_sol_balance(&self) -> Result<f64> {
        let balance = self.rpc_client.get_balance(&self.wallet.pubkey())?;
        Ok(balance as f64 / 1_000_000_000.0)
    }

    pub fn get_open_positions(&self) -> Vec<&Position> {
        self.position_manager.get_open_positions()
    }

    /// Log blockhash cache stats
    pub fn log_cache_stats(&self) {
        self.blockhash_cache.log_stats();
    }
}

pub fn load_keypair_from_file(path: &str) -> Result<Keypair> {
    let content = std::fs::read_to_string(path).context("Failed to read keypair file")?;

    let bytes: Vec<u8> = serde_json::from_str(&content).context("Failed to parse keypair JSON")?;

    Keypair::from_bytes(&bytes).context("Failed to create keypair from bytes")
}

pub fn load_keypair_from_env(env_var: &str) -> Result<Keypair> {
    let content = std::env::var(env_var).context("Environment variable not set")?;

    let bytes: Vec<u8> =
        serde_json::from_str(&content).context("Failed to parse keypair JSON from env")?;

    Keypair::from_bytes(&bytes).context("Failed to create keypair from bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wsol_account_creation() {
        let wallet = Keypair::new();
        let seed = format!("wsol_{}", 123);
        let wsol_account =
            Pubkey::create_with_seed(&wallet.pubkey(), &seed, &spl_token::id()).unwrap();

        assert_ne!(wsol_account, wallet.pubkey());
    }

    #[test]
    fn test_tip_instruction_is_last() {
        let mut instructions = vec![];
        instructions.push(
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(200_000),
        );

        let tip_account = solana_sdk::pubkey::Pubkey::new_unique();
        let payer = solana_sdk::pubkey::Pubkey::new_unique();
        let tip_lamports = 100_000;

        let tip_ix = solana_sdk::system_instruction::transfer(&payer, &tip_account, tip_lamports);
        instructions.push(tip_ix.clone());

        assert_eq!(
            instructions.last().unwrap().program_id,
            solana_sdk::system_program::id()
        );
        assert_eq!(instructions.last().unwrap().accounts[1].pubkey, tip_account);
    }
}
