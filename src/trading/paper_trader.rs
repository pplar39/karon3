use crate::config::{SafetyConfig, TradingConfig};
use crate::constants::PUMP_FUN_PROGRAM;
use crate::detection::RugChecker;
use crate::trading::pump_fun_swap::PumpFunSwapBuilder;
use crate::trading::{JitoSender, PositionManager};
use crate::types::{LatencyEvent, Pool, Position, PumpFunToken, TradeAction, TradeResult};
use anyhow::Result;
use chrono::Utc;
use solana_client::rpc_client::RpcClient;
use solana_sdk::compute_budget::ComputeBudgetInstruction;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::Keypair;
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, warn};

pub struct PaperTrader {
    position_manager: PositionManager,
    rug_checker: RugChecker,
    rpc_client: Arc<RpcClient>,
    jito_sender: JitoSender,
    trades: Vec<TradeResult>,
    trading_config: TradingConfig,
    dummy_payer: Keypair,
    cached_blockhash: Option<Hash>,
    last_blockhash_fetch: Option<Instant>,
    pub rpc_call_count: u64,
}

impl PaperTrader {
    pub fn new(rpc_url: &str, trading_config: TradingConfig, safety_config: SafetyConfig) -> Self {
        let rpc_client = Arc::new(RpcClient::new(rpc_url.to_string()));
        // Match live path: Jito HTTP auth uses UUID header (x-jito-auth).
        let mut jito_sender = JitoSender::new(
            trading_config.jito_url.clone(),
            trading_config.tip_amount,
            trading_config.jito_auth_uuid.clone(),
        );
        if let Some(dont_front) = trading_config.jito_dont_front {
            jito_sender.set_dont_front(dont_front);
        }
        Self {
            position_manager: PositionManager::new(trading_config.clone()),
            rug_checker: RugChecker::new(rpc_url, safety_config, None),
            rpc_client,
            jito_sender,
            trades: Vec::new(),
            trading_config,
            dummy_payer: Keypair::new(),
            cached_blockhash: None,
            last_blockhash_fetch: None,
            rpc_call_count: 0,
        }
    }

    pub async fn on_new_pool(
        &mut self,
        pool: Pool,
        latency: &mut LatencyEvent,
    ) -> Result<Option<Position>> {
        info!("Evaluating pool: {:?}", pool.amm_id);

        let safety = if self.trading_config.skip_rug_check.unwrap_or(false) {
            crate::types::SafetyCheck {
                mint_authority_revoked: true,
                freeze_authority_revoked: true,
                lp_burned_percent: 100.0,
                lp_locked_percent: 0.0,
                top10_holder_percent: 10.0,
                liquidity_sol: 100.0,
                is_safe: true,
                risk_score: 0,
                warnings: vec![],
            }
        } else {
            self.rug_checker.check_pool(&pool, latency).await?
        };

        if !safety.is_safe {
            warn!("Pool failed safety checks: {:?}, skipping", safety.warnings);
            return Ok(None);
        }

        info!(
            "Pool passed safety checks (risk score: {})",
            safety.risk_score
        );

        // --- EXPERIMENT SIMULATION START ---
        latency.tx_build_start_us = crate::time::monotonic_now_us();

        // Dynamic CU Price Fetching (Exp 06)
        let mut computed_cu_price = self.trading_config.compute_unit_price.unwrap_or(100_000);
        if self.trading_config.dynamic_cu.unwrap_or(false) {
            let accounts = vec![pool.amm_id];
            if let Ok(fees) = self.rpc_client.get_recent_prioritization_fees(&accounts) {
                if !fees.is_empty() {
                    let mut prices: Vec<u64> =
                        fees.into_iter().map(|f| f.prioritization_fee).collect();
                    prices.sort_unstable();
                    let mid = prices.len() / 2;
                    computed_cu_price = if prices.len() % 2 == 0 {
                        (prices[mid - 1] + prices[mid]) / 2
                    } else {
                        prices[mid]
                    };
                    debug!(
                        "Dynamic CU Price (median of {} blocks): {} microlamports",
                        prices.len(),
                        computed_cu_price
                    );
                }
            }
        }

        // Build Swap IX
        let token = Self::pool_as_pumpfun_token(&pool);
        let builder = PumpFunSwapBuilder::new(token.clone());
        let token_account = Pubkey::new_unique();
        let user_owner = self.dummy_payer.pubkey();

        // --- EXPERIMENT 16: ATA Creation Check Bottleneck ---
        if !self.trading_config.precreate_ata.unwrap_or(false) {
            // Simulate sequential get_account RPC call
            let _ = self.rpc_client.get_account(&token_account);
            self.rpc_call_count += 1;
        }

        let token_amount = PumpFunSwapBuilder::calculate_tokens_for_sol(100_000_000);
        let swap_ix = builder
            .build_buy_instruction(token_amount, 100_000_000, &user_owner, &token_account)
            .unwrap_or_else(|_| {
                solana_sdk::instruction::Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    vec![],
                )
            });

        // Add Compute Budget
        let mut instructions = vec![];
        let mut cu_instructions = vec![];

        if let Some(cu_limit) = self.trading_config.compute_unit_limit {
            cu_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        }

        if computed_cu_price > 0 {
            cu_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                computed_cu_price,
            ));
        }

        let swap_cu_order = self.trading_config.swap_cu_order.unwrap_or(false);

        if !swap_cu_order {
            instructions.extend(cu_instructions.clone());
        }

        // Add Tip (Exp 09)
        if let Some(tip) = self.trading_config.tip_amount {
            instructions.push(solana_sdk::system_instruction::transfer(
                &user_owner,
                &Pubkey::new_unique(),
                tip,
            ));
        }

        instructions.push(swap_ix);

        if swap_cu_order {
            instructions.extend(cu_instructions);
        }

        // Build Transaction
        // Exp 10: Persistent Client Toggle
        let rpc_client = if self.trading_config.persistent_client.unwrap_or(true) {
            self.rpc_client.clone()
        } else {
            Arc::new(RpcClient::new(self.rpc_client.url()))
        };

        let latest_blockhash = if self.trading_config.skip_rug_check.unwrap_or(false) {
            let now = Instant::now();
            let cache_ms = self.trading_config.blockhash_cache_ms.unwrap_or(0);
            let should_refresh = match self.last_blockhash_fetch {
                Some(last) => now.duration_since(last).as_millis() >= cache_ms as u128,
                None => true,
            };
            if should_refresh {
                self.rpc_call_count += 1;
                self.last_blockhash_fetch = Some(now);
                self.cached_blockhash = Some(Hash::default());
                Hash::default()
            } else {
                self.cached_blockhash.unwrap_or_default()
            }
        } else if self.trading_config.blockhash_cache_ms.is_some() {
            let cache_ms = self.trading_config.blockhash_cache_ms.unwrap();
            let now = Instant::now();

            let should_refresh = match self.last_blockhash_fetch {
                Some(last) => now.duration_since(last).as_millis() >= cache_ms as u128,
                None => true,
            };

            if should_refresh {
                let bh = rpc_client.get_latest_blockhash().unwrap_or_default();
                self.cached_blockhash = Some(bh);
                self.last_blockhash_fetch = Some(now);
                self.rpc_call_count += 1;
                bh
            } else {
                self.cached_blockhash.unwrap_or_default()
            }
        } else {
            self.rpc_call_count += 1;
            rpc_client.get_latest_blockhash().unwrap_or_default()
        };

        let _tx = if self.trading_config.parallel_signing.unwrap_or(false) {
            let payer_bytes = self.dummy_payer.to_bytes();
            let instructions = instructions.clone();
            let user_owner = user_owner.clone();
            let latest_blockhash = latest_blockhash.clone();

            tokio::task::spawn_blocking(move || {
                let payer = Keypair::from_bytes(&payer_bytes).unwrap();
                Transaction::new_signed_with_payer(
                    &instructions,
                    Some(&user_owner),
                    &[&payer],
                    latest_blockhash,
                )
            })
            .await?
        } else {
            Transaction::new_signed_with_payer(
                &instructions,
                Some(&user_owner),
                &[&self.dummy_payer],
                latest_blockhash,
            )
        };

        latency.tx_signed_us = crate::time::monotonic_now_us();

        // Preflight Simulation (Exp 04)
        if self.trading_config.preflight_check.unwrap_or(false) {
            let _ = rpc_client.simulate_transaction(&_tx);
        }

        if self.trading_config.use_bundles.unwrap_or(true) && self.trading_config.jito_url.is_some()
        {
            latency.jito_send_start_us = crate::time::monotonic_now_us();

            // Real network measurement (non-blocking simulation of Jito roundtrip)
            let _ = self.jito_sender.get_bundle_status("dummy").await;

            latency.jito_send_end_us = crate::time::monotonic_now_us();
            latency.slot_drift = Some(0);
        } else {
            latency.rpc_send_start_us = crate::time::monotonic_now_us();

            let overhead = 15000 + (latency.tx_signed_us % 10000);
            latency.rpc_send_end_us = latency.rpc_send_start_us + overhead;
            latency.slot_drift = Some(1);
        }

        // --- EXPERIMENT SIMULATION END ---

        let price = self.estimate_token_price(&pool);
        info!("Estimated token price: {} SOL", price);

        let position = self.position_manager.open_position(pool, price);

        let buy_trade = TradeResult {
            position_id: position.id.clone(),
            action: TradeAction::Buy,
            price_sol: price,
            amount_sol: position.entry_amount_sol,
            pnl_percent: 0.0,
            pnl_sol: 0.0,
            timestamp: Utc::now(),
            reason: "New safe pool detected".to_string(),
        };
        self.trades.push(buy_trade);

        Ok(Some(position))
    }

    /// PumpFunToken → Pool 변환 (Position 추적용 bridge)
    /// Vault fields are Pubkey::default() — Pump.fun uses bonding curve PDA, not vaults.
    fn pumpfun_token_as_pool(token: &PumpFunToken) -> Pool {
        Pool {
            amm_id: token.bonding_curve,
            base_mint: token.mint,
            quote_mint: Pubkey::default(),
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

    /// Pool → PumpFunToken 변환 (on_new_pool 경로 전용)
    /// bonding_curve는 mint에서 PDA 파생 (직접 매핑 금지)
    fn pool_as_pumpfun_token(pool: &Pool) -> PumpFunToken {
        let (bonding_curve, associated_bonding_curve) =
            PumpFunToken::derive_bonding_curve(&pool.base_mint, &PUMP_FUN_PROGRAM);
        PumpFunToken {
            mint: pool.base_mint,
            bonding_curve,
            associated_bonding_curve,
            user: Pubkey::default(),
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

    pub async fn on_new_pumpfun_token(
        &mut self,
        token: PumpFunToken,
        latency: &mut LatencyEvent,
    ) -> Result<Option<Position>> {
        info!("Evaluating Pump.fun token: {:?}", token.mint);

        latency.tx_build_start_us = crate::time::monotonic_now_us();

        // Swap instruction 빌드 (시뮬레이션)
        let builder = PumpFunSwapBuilder::new(token.clone());
        let token_account = Pubkey::new_unique();
        let user_owner = self.dummy_payer.pubkey();

        let token_amount = PumpFunSwapBuilder::calculate_tokens_for_sol(100_000_000);
        let swap_ix = builder
            .build_buy_instruction(token_amount, 100_000_000, &user_owner, &token_account)
            .unwrap_or_else(|_| {
                solana_sdk::instruction::Instruction::new_with_bytes(
                    Pubkey::new_unique(),
                    &[],
                    vec![],
                )
            });

        let mut instructions = vec![];
        if let Some(cu_limit) = self.trading_config.compute_unit_limit {
            instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));
        }
        instructions.push(swap_ix);

        let latest_blockhash = Hash::default();
        let _tx = Transaction::new_signed_with_payer(
            &instructions,
            Some(&user_owner),
            &[&self.dummy_payer],
            latest_blockhash,
        );

        latency.tx_signed_us = crate::time::monotonic_now_us();

        // Jito roundtrip 시뮬레이션
        if self.trading_config.use_bundles.unwrap_or(true) && self.trading_config.jito_url.is_some()
        {
            latency.jito_send_start_us = crate::time::monotonic_now_us();
            let _ = self.jito_sender.get_bundle_status("dummy").await;
            latency.jito_send_end_us = crate::time::monotonic_now_us();
            latency.slot_drift = Some(0);
        } else {
            latency.rpc_send_start_us = crate::time::monotonic_now_us();
            let overhead = 15000 + (latency.tx_signed_us % 10000);
            latency.rpc_send_end_us = latency.rpc_send_start_us + overhead;
            latency.slot_drift = Some(1);
        }

        // 가격 추정 (virtual reserves 기반)
        let price = if token.virtual_token_reserves > 0 {
            (token.virtual_sol_reserves as f64) / (token.virtual_token_reserves as f64)
        } else {
            0.001
        };
        info!("Pump.fun token price: {} SOL", price);

        // Position 생성 (Pool bridge)
        let pool = Self::pumpfun_token_as_pool(&token);
        let position = self.position_manager.open_position(pool, price);

        let buy_trade = TradeResult {
            position_id: position.id.clone(),
            action: TradeAction::Buy,
            price_sol: price,
            amount_sol: position.entry_amount_sol,
            pnl_percent: 0.0,
            pnl_sol: 0.0,
            timestamp: Utc::now(),
            reason: "Pump.fun token detected".to_string(),
        };
        self.trades.push(buy_trade);

        Ok(Some(position))
    }

    pub fn update_positions(&mut self, price_updates: Vec<(String, f64)>) -> Vec<TradeResult> {
        let mut closed_trades = Vec::new();

        for (position_id, new_price) in price_updates {
            self.position_manager.update_price(&position_id, new_price);

            if let Some(trade) = self.position_manager.check_exit_conditions(&position_id) {
                info!(
                    "Position {} closed: {} with {:.2}% PnL",
                    trade.position_id, trade.reason, trade.pnl_percent
                );
                self.trades.push(trade.clone());
                closed_trades.push(trade);
            }
        }

        closed_trades
    }

    pub fn get_all_trades(&self) -> &[TradeResult] {
        &self.trades
    }

    pub fn get_open_positions(&self) -> Vec<&Position> {
        self.position_manager.get_open_positions()
    }

    pub fn position_manager(&self) -> &PositionManager {
        &self.position_manager
    }

    pub fn position_manager_mut(&mut self) -> &mut PositionManager {
        &mut self.position_manager
    }

    pub fn get_stats(&self) -> TradingStats {
        let sells: Vec<_> = self
            .trades
            .iter()
            .filter(|t| matches!(t.action, TradeAction::Sell))
            .collect();

        let total_pnl_sol: f64 = sells.iter().map(|t| t.pnl_sol).sum();
        let winning_trades = sells.iter().filter(|t| t.pnl_sol > 0.0).count();
        let losing_trades = sells.iter().filter(|t| t.pnl_sol < 0.0).count();

        let win_rate = if !sells.is_empty() {
            (winning_trades as f64 / sells.len() as f64) * 100.0
        } else {
            0.0
        };

        TradingStats {
            total_trades: sells.len(),
            winning_trades,
            losing_trades,
            total_pnl_sol,
            win_rate,
            open_positions: self.position_manager.get_open_positions().len(),
        }
    }

    fn estimate_token_price(&self, pool: &Pool) -> f64 {
        let base_balance = self.fetch_vault_balance(&pool.base_vault, pool.base_decimals);
        let quote_balance = self.fetch_vault_balance(&pool.quote_vault, pool.quote_decimals);

        if base_balance <= 0.0 {
            return 0.001;
        }

        quote_balance / base_balance
    }

    fn fetch_vault_balance(&self, vault: &solana_sdk::pubkey::Pubkey, _decimals: u8) -> f64 {
        if self.trading_config.skip_rug_check.unwrap_or(false) {
            return 10.0;
        }
        match self.rpc_client.get_token_account_balance(vault) {
            Ok(balance) => balance.ui_amount.unwrap_or(0.0),
            Err(e) => {
                warn!("Failed to fetch vault balance: {}", e);
                0.0
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TradingStats {
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub total_pnl_sol: f64,
    pub win_rate: f64,
    pub open_positions: usize,
}

impl std::fmt::Display for TradingStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Trades: {} (W: {}, L: {}) | Win Rate: {:.1}% | PnL: {:.4} SOL | Open: {}",
            self.total_trades,
            self.winning_trades,
            self.losing_trades,
            self.win_rate,
            self.total_pnl_sol,
            self.open_positions
        )
    }
}
