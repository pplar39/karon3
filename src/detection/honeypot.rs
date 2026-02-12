//! Honeypot detection for Solana tokens.
//!
//! Honeypot = token that can be bought but NOT sold.
//!
//! Detection methods:
//! 1. RugCheck API (primary, fast) - returns risk analysis
//! 2. Transaction simulation (fallback) - simulate buy/sell
//!
//! For MVP, we use the RugCheck API as the primary method.

use crate::types::Pool;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

/// Result of honeypot detection
#[derive(Debug, Clone)]
pub struct HoneypotResult {
    /// Whether the token appears to be a honeypot
    pub is_honeypot: bool,
    /// Whether buy simulation succeeded (if simulation was used)
    pub buy_simulation_ok: bool,
    /// Whether sell simulation succeeded (if simulation was used)
    pub sell_simulation_ok: bool,
    /// Risk score (0-10000, +1000 if honeypot detected)
    pub risk_score: u32,
    /// Error or warning message
    pub error_message: Option<String>,
}

impl Default for HoneypotResult {
    fn default() -> Self {
        Self {
            is_honeypot: false,
            buy_simulation_ok: true,
            sell_simulation_ok: true,
            risk_score: 0,
            error_message: None,
        }
    }
}

/// RugCheck API response structures
#[derive(Debug, Deserialize)]
struct RugCheckReport {
    /// Risk score from 0-10000
    score: Option<u32>,
    /// Risk level: "Good", "Low", "Medium", "High", "Extreme"
    #[serde(default)]
    risks: Vec<RugCheckRisk>,
    /// Token information
    #[serde(rename = "tokenMeta")]
    token_meta: Option<TokenMeta>,
}

#[derive(Debug, Deserialize)]
struct RugCheckRisk {
    /// Risk name
    name: String,
    /// Risk level
    level: String,
    /// Risk description
    description: String,
}

#[derive(Debug, Deserialize)]
struct TokenMeta {
    /// Token symbol
    symbol: Option<String>,
    /// Token name
    name: Option<String>,
}

/// Honeypot detection checker
pub struct HoneypotChecker {
    /// RPC client for on-chain simulation (reserved for future use)
    _rpc_client: Arc<RpcClient>,
    /// HTTP client for API calls
    http_client: reqwest::Client,
    /// RugCheck API base URL
    api_base_url: String,
}

impl HoneypotChecker {
    /// Create a new HoneypotChecker
    ///
    /// # Arguments
    /// * `rpc_client` - Solana RPC client for transaction simulation
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            _rpc_client: rpc_client,
            http_client,
            api_base_url: "https://api.rugcheck.xyz/v1".to_string(),
        }
    }

    /// Create with custom API base URL (for testing)
    #[cfg(test)]
    pub fn with_api_url(rpc_client: Arc<RpcClient>, api_base_url: String) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            _rpc_client: rpc_client,
            http_client,
            api_base_url,
        }
    }

    /// Check if a pool's base token is a honeypot
    ///
    /// Uses RugCheck API as primary detection method.
    /// Returns HoneypotResult with risk assessment.
    pub async fn check(&self, pool: &Pool) -> Result<HoneypotResult> {
        self.check_via_api(&pool.base_mint).await
    }

    /// Check a specific mint address for honeypot characteristics
    pub async fn check_mint(&self, mint: &Pubkey) -> Result<HoneypotResult> {
        self.check_via_api(mint).await
    }

    /// Primary detection: Use RugCheck API
    async fn check_via_api(&self, mint: &Pubkey) -> Result<HoneypotResult> {
        let url = format!("{}/tokens/{}/report", self.api_base_url, mint);

        debug!("Checking honeypot via RugCheck API: {}", url);

        let response = self
            .http_client
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await;

        match response {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    warn!(
                        "RugCheck API returned {}: {}",
                        status,
                        body.chars().take(200).collect::<String>()
                    );
                    return Ok(HoneypotResult {
                        error_message: Some(format!("API returned status {}", status)),
                        ..Default::default()
                    });
                }

                let report: RugCheckReport = resp
                    .json()
                    .await
                    .map_err(|e| anyhow!("Failed to parse RugCheck response: {}", e))?;

                self.analyze_report(report)
            }
            Err(e) => {
                warn!("RugCheck API request failed: {}", e);
                Ok(HoneypotResult {
                    error_message: Some(format!("API request failed: {}", e)),
                    ..Default::default()
                })
            }
        }
    }

    fn analyze_report(&self, report: RugCheckReport) -> Result<HoneypotResult> {
        let mut result = HoneypotResult::default();
        result.risk_score = report.score.unwrap_or(0);

        let honeypot_keywords = [
            "honeypot",
            "cannot sell",
            "sell disabled",
            "transfer disabled",
            "blacklist",
            "can't sell",
            "locked",
        ];

        for risk in &report.risks {
            let risk_lower = risk.name.to_lowercase();
            let desc_lower = risk.description.to_lowercase();

            let is_honeypot_risk = honeypot_keywords
                .iter()
                .any(|kw| risk_lower.contains(kw) || desc_lower.contains(kw));

            if is_honeypot_risk {
                result.is_honeypot = true;
                result.sell_simulation_ok = false;
                result.risk_score = result.risk_score.saturating_add(1000);
                result.error_message = Some(format!(
                    "Honeypot risk: {} - {}",
                    risk.name, risk.description
                ));
                break;
            }

            if risk.level == "High" || risk.level == "Extreme" {
                result.risk_score = result.risk_score.saturating_add(200);
            }
        }

        let extremely_high_risk = result.risk_score >= 8000 && !result.is_honeypot;
        if extremely_high_risk {
            result.is_honeypot = true;
            result.error_message =
                Some(format!("Extremely high risk score: {}", result.risk_score));
        }

        if let Some(meta) = &report.token_meta {
            debug!(
                "Token: {} ({}), Risk Score: {}, Honeypot: {}",
                meta.name.as_deref().unwrap_or("Unknown"),
                meta.symbol.as_deref().unwrap_or("???"),
                result.risk_score,
                result.is_honeypot
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_rpc_client() -> Arc<RpcClient> {
        Arc::new(RpcClient::new(
            "https://api.mainnet-beta.solana.com".to_string(),
        ))
    }

    #[test]
    fn test_honeypot_checker_creation() {
        let rpc_client = mock_rpc_client();
        let checker = HoneypotChecker::new(rpc_client);

        assert_eq!(checker.api_base_url, "https://api.rugcheck.xyz/v1");
    }

    #[test]
    fn test_honeypot_result_default() {
        let result = HoneypotResult::default();

        assert!(!result.is_honeypot);
        assert!(result.buy_simulation_ok);
        assert!(result.sell_simulation_ok);
        assert_eq!(result.risk_score, 0);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn test_analyze_report_clean_token() {
        let rpc_client = mock_rpc_client();
        let checker = HoneypotChecker::new(rpc_client);

        let report = RugCheckReport {
            score: Some(500),
            risks: vec![],
            token_meta: Some(TokenMeta {
                symbol: Some("SAFE".to_string()),
                name: Some("Safe Token".to_string()),
            }),
        };

        let result = checker.analyze_report(report).unwrap();

        assert!(!result.is_honeypot);
        assert_eq!(result.risk_score, 500);
        assert!(result.error_message.is_none());
    }

    #[test]
    fn test_analyze_report_honeypot_detected() {
        let rpc_client = mock_rpc_client();
        let checker = HoneypotChecker::new(rpc_client);

        let report = RugCheckReport {
            score: Some(3000),
            risks: vec![RugCheckRisk {
                name: "Honeypot Detected".to_string(),
                level: "Extreme".to_string(),
                description: "Token cannot sell - honeypot mechanism active".to_string(),
            }],
            token_meta: None,
        };

        let result = checker.analyze_report(report).unwrap();

        assert!(result.is_honeypot);
        assert!(!result.sell_simulation_ok);
        assert!(result.risk_score >= 4000);
        assert!(result.error_message.is_some());
    }

    #[test]
    fn test_analyze_report_high_risk_score() {
        let rpc_client = mock_rpc_client();
        let checker = HoneypotChecker::new(rpc_client);

        let report = RugCheckReport {
            score: Some(9000),
            risks: vec![],
            token_meta: None,
        };

        let result = checker.analyze_report(report).unwrap();

        assert!(result.is_honeypot);
        assert!(result.error_message.is_some());
    }

    #[test]
    fn test_analyze_report_blacklist_risk() {
        let rpc_client = mock_rpc_client();
        let checker = HoneypotChecker::new(rpc_client);

        let report = RugCheckReport {
            score: Some(2000),
            risks: vec![RugCheckRisk {
                name: "Blacklist Function".to_string(),
                level: "High".to_string(),
                description: "Token has blacklist mechanism that can block transfers".to_string(),
            }],
            token_meta: None,
        };

        let result = checker.analyze_report(report).unwrap();

        assert!(result.is_honeypot);
        assert!(result.risk_score >= 3000);
    }
}
