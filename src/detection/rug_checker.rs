use super::{Blacklist, HoneypotChecker, HoneypotResult, Token2022Checker, Token2022Result};
use crate::config::SafetyConfig;
use crate::constants::{BURN_ADDRESS, SMITHII_LOCKER, UNCX_LOCKER};
use crate::types::{LatencyEvent, Pool, SafetyCheck};
use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tracing::{info, warn};

pub struct RugChecker {
    rpc_client: Arc<RpcClient>,
    config: SafetyConfig,
    token2022_checker: Token2022Checker,
    honeypot_checker: HoneypotChecker,
    blacklist: Option<Arc<Blacklist>>,
}

#[derive(Debug, Default)]
struct LpStatus {
    burned_percent: f64,
    locked_percent: f64,
    _unlocked_percent: f64,
}

impl RugChecker {
    pub fn new(rpc_url: &str, config: SafetyConfig, blacklist: Option<Arc<Blacklist>>) -> Self {
        let rpc_client = Arc::new(RpcClient::new(rpc_url.to_string()));
        Self {
            rpc_client: Arc::clone(&rpc_client),
            config,
            token2022_checker: Token2022Checker::new(),
            honeypot_checker: HoneypotChecker::new(rpc_client),
            blacklist,
        }
    }

    async fn check_token2022(&self, mint: &Pubkey) -> Result<Token2022Result> {
        let mint = *mint;
        let client = Arc::clone(&self.rpc_client);
        let account = tokio::task::spawn_blocking(move || client.get_account(&mint)).await??;
        Ok(self.token2022_checker.check_mint(&account.data))
    }

    pub async fn check_pool(&self, pool: &Pool, latency: &mut LatencyEvent) -> Result<SafetyCheck> {
        latency.rugcheck_start_us = crate::time::monotonic_now_us();

        let mut warnings = Vec::new();
        let mut risk_score = 0u32;

        if let Some(blacklist) = &self.blacklist {
            if blacklist.is_blacklisted(&pool.base_mint) {
                warnings.push("Token is blacklisted".to_string());
                risk_score += 1000;
            }
        }

        let (
            mint_revoked,
            freeze_revoked,
            lp_status,
            top10,
            liquidity_sol,
            token2022_res,
            honeypot_res,
        ) = tokio::join!(
            self.check_mint_authority(&pool.base_mint),
            self.check_freeze_authority(&pool.base_mint),
            self.check_lp_status(&pool.lp_mint),
            self.check_top_holders(&pool.base_mint),
            self.check_liquidity(pool),
            self.check_token2022(&pool.base_mint),
            self.honeypot_checker.check(pool)
        );

        let mint_revoked = mint_revoked?;
        let freeze_revoked = freeze_revoked?;
        let lp_status = lp_status.unwrap_or_default();
        let top10 = top10.unwrap_or(100.0);
        let liquidity_sol = liquidity_sol.unwrap_or(0.0);
        let token2022_res = token2022_res.unwrap_or_default();
        let honeypot_res = honeypot_res.unwrap_or_default();

        if !mint_revoked && self.config.require_mint_revoked {
            warnings.push("Mint authority not revoked".to_string());
            risk_score += 500;
        }

        if !freeze_revoked && self.config.require_freeze_revoked {
            warnings.push("Freeze authority not revoked".to_string());
            risk_score += 300;
        }

        // LP Status Scoring
        let lp_burned = lp_status.burned_percent;
        let lp_locked = lp_status.locked_percent;
        let lp_combined = lp_burned + lp_locked;

        if lp_burned >= 90.0 {
        } else if lp_locked >= 90.0 {
            warnings.push(format!("LP Locked ({:.1}%) but not burned", lp_locked));
            risk_score += 50;
        } else if lp_combined >= 90.0 {
            warnings.push(format!("LP Combined ({:.1}%) burned+locked", lp_combined));
            risk_score += 25;
        } else {
            warnings.push(format!(
                "LP status risky: Burned={:.1}%, Locked={:.1}%",
                lp_burned, lp_locked
            ));
            risk_score += 200;
        }

        if top10 > self.config.max_top10_holder_percent {
            warnings.push(format!("Top 10 holders own {:.1}%", top10));
            risk_score += 100;
        }

        if liquidity_sol < self.config.min_liquidity_sol {
            warnings.push(format!(
                "Liquidity {:.2} SOL below minimum {:.2}",
                liquidity_sol, self.config.min_liquidity_sol
            ));
            risk_score += 400;
        }

        if token2022_res.is_token_2022 {
            risk_score += token2022_res.risk_score;
            for w in token2022_res.warnings {
                warnings.push(format!("Token2022: {}", w));
            }
        }

        if honeypot_res.is_honeypot {
            risk_score += 1000;
            if let Some(msg) = honeypot_res.error_message {
                warnings.push(format!("Honeypot: {}", msg));
            }
        }

        let is_safe = risk_score == 0;

        if is_safe {
            info!("✅ Pool {} passed safety checks", pool.amm_id);
        } else {
            warn!(
                "⚠️ Pool {} failed safety checks (score={}): {:?}",
                pool.amm_id, risk_score, warnings
            );
        }

        latency.rugcheck_end_us = crate::time::monotonic_now_us();

        Ok(SafetyCheck {
            mint_authority_revoked: mint_revoked,
            freeze_authority_revoked: freeze_revoked,
            lp_burned_percent: lp_burned,
            lp_locked_percent: lp_locked,
            top10_holder_percent: top10,
            liquidity_sol,
            is_safe,
            risk_score,
            warnings,
        })
    }

    /// SPL Token mint layout: mintAuthority at offset 0-36 (Option<Pubkey>).
    /// First byte: 0 = None (revoked), 1 = Some (active).
    async fn check_mint_authority(&self, mint: &Pubkey) -> Result<bool> {
        let mint = *mint;
        let client = Arc::clone(&self.rpc_client);

        let result = tokio::task::spawn_blocking(move || client.get_account(&mint)).await??;

        if result.data.len() >= 4 {
            let has_authority = result.data[0] == 1;
            return Ok(!has_authority);
        }

        Ok(false)
    }

    /// SPL Token mint layout: freezeAuthority at offset 36-72 (Option<Pubkey>).
    /// Byte 36: 0 = None (revoked), 1 = Some (active).
    async fn check_freeze_authority(&self, mint: &Pubkey) -> Result<bool> {
        let mint = *mint;
        let client = Arc::clone(&self.rpc_client);

        let result = tokio::task::spawn_blocking(move || client.get_account(&mint)).await??;

        if result.data.len() >= 40 {
            let has_authority = result.data[36] == 1;
            return Ok(!has_authority);
        }

        Ok(false)
    }

    fn is_known_locker(&self, owner: &Pubkey) -> bool {
        *owner == *UNCX_LOCKER || *owner == *SMITHII_LOCKER
    }

    async fn check_lp_status(&self, lp_mint: &Pubkey) -> Result<LpStatus> {
        let lp_mint = *lp_mint;
        let client = Arc::clone(&self.rpc_client);

        let supply = tokio::task::spawn_blocking({
            let client = Arc::clone(&client);
            let lp_mint = lp_mint;
            move || client.get_token_supply(&lp_mint)
        })
        .await??;

        let total: f64 = supply.ui_amount.unwrap_or(0.0);

        if total == 0.0 {
            return Ok(LpStatus::default());
        }

        let holders = tokio::task::spawn_blocking({
            let client = Arc::clone(&client);
            let lp_mint = lp_mint;
            move || client.get_token_largest_accounts(&lp_mint)
        })
        .await??;

        let mut burned = 0.0;
        let mut locked = 0.0;
        let mut unlocked = 0.0;

        let holder_pubkeys: Vec<Pubkey> = holders
            .iter()
            .filter_map(|h| h.address.parse::<Pubkey>().ok())
            .collect();

        if !holder_pubkeys.is_empty() {
            let client = Arc::clone(&self.rpc_client);
            let accounts =
                tokio::task::spawn_blocking(move || client.get_multiple_accounts(&holder_pubkeys))
                    .await??;

            for (i, account_opt) in accounts.into_iter().enumerate() {
                if let Some(account) = account_opt {
                    // SPL Token account layout: owner is at offset 32-64
                    if account.data.len() >= 64 {
                        let owner_bytes: [u8; 32] =
                            account.data[32..64].try_into().unwrap_or([0u8; 32]);
                        let owner = Pubkey::new_from_array(owner_bytes);
                        let amount = holders[i].amount.ui_amount.unwrap_or(0.0);

                        if owner == *BURN_ADDRESS {
                            burned += amount;
                        } else if self.is_known_locker(&owner) {
                            locked += amount;
                        } else {
                            unlocked += amount;
                        }
                    }
                }
            }
        }

        Ok(LpStatus {
            burned_percent: (burned / total) * 100.0,
            locked_percent: (locked / total) * 100.0,
            _unlocked_percent: (unlocked / total) * 100.0,
        })
    }

    async fn check_top_holders(&self, mint: &Pubkey) -> Result<f64> {
        let mint = *mint;
        let client = Arc::clone(&self.rpc_client);

        let supply = tokio::task::spawn_blocking({
            let client = Arc::clone(&client);
            let mint = mint;
            move || client.get_token_supply(&mint)
        })
        .await??;

        let total: f64 = supply.ui_amount.unwrap_or(0.0);

        if total == 0.0 {
            return Ok(100.0);
        }

        let holders = tokio::task::spawn_blocking({
            let client = Arc::clone(&client);
            let mint = mint;
            move || client.get_token_largest_accounts(&mint)
        })
        .await??;

        let top10_sum: f64 = holders
            .iter()
            .take(10)
            .map(|h| h.amount.ui_amount.unwrap_or(0.0))
            .sum();

        Ok((top10_sum / total) * 100.0)
    }

    async fn check_liquidity(&self, pool: &Pool) -> Result<f64> {
        let quote_vault = pool.quote_vault;
        let client = Arc::clone(&self.rpc_client);

        let balance =
            tokio::task::spawn_blocking(move || client.get_token_account_balance(&quote_vault))
                .await??;

        Ok(balance.ui_amount.unwrap_or(0.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> SafetyConfig {
        SafetyConfig {
            min_lp_burn_percent: 90.0,
            max_top10_holder_percent: 30.0,
            require_mint_revoked: true,
            require_freeze_revoked: true,
            min_liquidity_sol: 5.0,
            parallel_rug_check: None,
            blacklist_file: None,
            auto_blacklist_on_rug: None,
            circuit_breaker: None,
            rpc_rate_limit: None,
            max_daily_loss_sol: None,
            omega_trinity: None,
            pumpfun_filter: None,
        }
    }

    #[test]
    fn test_rug_checker_creation() {
        let config = test_config();
        let checker = RugChecker::new("https://api.mainnet-beta.solana.com", config, None);

        assert_eq!(checker.config.min_lp_burn_percent, 90.0);
        assert_eq!(checker.config.max_top10_holder_percent, 30.0);
        assert!(checker.config.require_mint_revoked);
        assert!(checker.config.require_freeze_revoked);
        assert_eq!(checker.config.min_liquidity_sol, 5.0);
    }

    #[test]
    fn test_is_known_locker() {
        let config = test_config();
        let checker = RugChecker::new("https://api.mainnet-beta.solana.com", config, None);

        assert!(checker.is_known_locker(&UNCX_LOCKER));
        assert!(checker.is_known_locker(&SMITHII_LOCKER));
        assert!(!checker.is_known_locker(&Pubkey::new_unique()));
        assert!(!checker.is_known_locker(&BURN_ADDRESS));
    }
}
