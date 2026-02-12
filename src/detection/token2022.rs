//! Token-2022 extension detection for identifying dangerous token features.
//!
//! Detects extensions like PermanentDelegate, NonTransferable, and TransferHook
//! that can make tokens unsafe to trade (honeypots, rug pulls, hidden fees).

use spl_token_2022::{
    extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensions},
    state::Mint,
};
use tracing::warn;

/// Token-2022 program ID
pub const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

/// Risk scores for each extension type
const RISK_PERMANENT_DELEGATE: u32 = 1000;
const RISK_NON_TRANSFERABLE: u32 = 1000;
const RISK_TRANSFER_HOOK: u32 = 1000;
const RISK_TRANSFER_FEE: u32 = 500;
const RISK_CONFIDENTIAL_TRANSFER: u32 = 500;
const RISK_MINT_CLOSE_AUTHORITY: u32 = 200;

/// Result of Token-2022 extension analysis
#[derive(Debug, Clone)]
pub struct Token2022Result {
    /// Whether this is a Token-2022 mint
    pub is_token_2022: bool,
    /// List of detected extensions
    pub extensions: Vec<ExtensionType>,
    /// Cumulative risk score based on extensions
    pub risk_score: u32,
    /// Human-readable warnings
    pub warnings: Vec<String>,
    /// Whether to automatically reject (critical extensions found)
    pub auto_reject: bool,
}

impl Default for Token2022Result {
    fn default() -> Self {
        Self {
            is_token_2022: false,
            extensions: Vec::new(),
            risk_score: 0,
            warnings: Vec::new(),
            auto_reject: false,
        }
    }
}

/// Checker for Token-2022 mint extensions
pub struct Token2022Checker;

impl Token2022Checker {
    /// Create a new Token2022Checker instance
    pub fn new() -> Self {
        Self
    }

    /// Check mint account data for dangerous Token-2022 extensions.
    ///
    /// # Arguments
    /// * `account_data` - Raw account data bytes from the mint account
    ///
    /// # Returns
    /// * `Token2022Result` with detected extensions and risk assessment
    pub fn check_mint(&self, account_data: &[u8]) -> Token2022Result {
        // Standard SPL Token mint is 82 bytes, Token-2022 is larger
        if account_data.len() <= 82 {
            return Token2022Result::default();
        }

        // Try to unpack as Token-2022 mint with extensions
        let mint_with_extensions = match StateWithExtensions::<Mint>::unpack(account_data) {
            Ok(m) => m,
            Err(_) => return Token2022Result::default(),
        };

        let mut result = Token2022Result {
            is_token_2022: true,
            extensions: Vec::new(),
            risk_score: 0,
            warnings: Vec::new(),
            auto_reject: false,
        };

        // Get all extension types present on this mint
        let extension_types = mint_with_extensions
            .get_extension_types()
            .unwrap_or_default();

        for ext_type in extension_types {
            result.extensions.push(ext_type);

            match ext_type {
                // CRITICAL: Auto-reject (+1000 risk each)
                ExtensionType::PermanentDelegate => {
                    result.risk_score += RISK_PERMANENT_DELEGATE;
                    result.auto_reject = true;
                    result.warnings.push(
                        "PermanentDelegate: Token issuer can seize tokens from any wallet"
                            .to_string(),
                    );
                    warn!("🚨 Token has PermanentDelegate - can seize tokens!");
                }
                ExtensionType::NonTransferable => {
                    result.risk_score += RISK_NON_TRANSFERABLE;
                    result.auto_reject = true;
                    result.warnings.push(
                        "NonTransferable: Soulbound token, cannot sell (honeypot)".to_string(),
                    );
                    warn!("🚨 Token is NonTransferable - honeypot!");
                }
                ExtensionType::TransferHook => {
                    result.risk_score += RISK_TRANSFER_HOOK;
                    result.auto_reject = true;
                    result.warnings.push(
                        "TransferHook: Arbitrary code runs on transfer, can block sales"
                            .to_string(),
                    );
                    warn!("🚨 Token has TransferHook - can execute arbitrary code!");
                }

                // HIGH RISK (+500 each)
                ExtensionType::TransferFeeConfig => {
                    result.risk_score += RISK_TRANSFER_FEE;
                    result
                        .warnings
                        .push("TransferFeeConfig: Hidden transfer tax may apply".to_string());
                    warn!("⚠️ Token has TransferFee - hidden tax");
                }
                ExtensionType::ConfidentialTransferMint => {
                    result.risk_score += RISK_CONFIDENTIAL_TRANSFER;
                    result
                        .warnings
                        .push("ConfidentialTransfer: Transfer amounts are hidden".to_string());
                    warn!("⚠️ Token has ConfidentialTransfer - hidden amounts");
                }

                // MEDIUM RISK (+200 each)
                ExtensionType::MintCloseAuthority => {
                    result.risk_score += RISK_MINT_CLOSE_AUTHORITY;
                    result
                        .warnings
                        .push("MintCloseAuthority: Mint can be closed by authority".to_string());
                }

                // Low risk or benign extensions
                ExtensionType::MetadataPointer
                | ExtensionType::TokenMetadata
                | ExtensionType::InterestBearingConfig
                | ExtensionType::DefaultAccountState
                | ExtensionType::ImmutableOwner
                | ExtensionType::MemoTransfer
                | ExtensionType::CpiGuard
                | ExtensionType::TransferHookAccount
                | ExtensionType::GroupPointer
                | ExtensionType::TokenGroup
                | ExtensionType::GroupMemberPointer
                | ExtensionType::TokenGroupMember
                | ExtensionType::ConfidentialTransferAccount
                | ExtensionType::ConfidentialTransferFeeConfig
                | ExtensionType::ConfidentialTransferFeeAmount => {
                    // These are generally safe or account-level extensions
                }

                // Uninitialized or unknown
                ExtensionType::Uninitialized => {}

                // Catch any future extensions we haven't categorized
                #[allow(unreachable_patterns)]
                _ => {
                    result.risk_score += 100;
                    result
                        .warnings
                        .push(format!("Unknown extension: {:?}", ext_type));
                }
            }
        }

        result
    }

    /// Check if the given program ID is the Token-2022 program
    pub fn is_token_2022_program(program_id: &str) -> bool {
        program_id == TOKEN_2022_PROGRAM_ID
    }
}

impl Default for Token2022Checker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_standard_token_not_detected() {
        let checker = Token2022Checker::new();
        let standard_mint_data = vec![0u8; 82];
        let result = checker.check_mint(&standard_mint_data);

        assert!(!result.is_token_2022);
        assert!(result.extensions.is_empty());
        assert_eq!(result.risk_score, 0);
        assert!(!result.auto_reject);
    }

    #[test]
    fn test_empty_data() {
        let checker = Token2022Checker::new();
        let result = checker.check_mint(&[]);

        assert!(!result.is_token_2022);
        assert_eq!(result.risk_score, 0);
    }

    #[test]
    fn test_short_data() {
        let checker = Token2022Checker::new();
        let result = checker.check_mint(&[0u8; 50]);

        assert!(!result.is_token_2022);
        assert_eq!(result.risk_score, 0);
    }

    #[test]
    fn test_invalid_token2022_data() {
        let checker = Token2022Checker::new();
        let invalid_data = vec![0u8; 200];
        let result = checker.check_mint(&invalid_data);

        assert!(!result.is_token_2022);
    }

    #[test]
    fn test_token2022_program_detection() {
        assert!(Token2022Checker::is_token_2022_program(
            TOKEN_2022_PROGRAM_ID
        ));
        assert!(!Token2022Checker::is_token_2022_program(
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        ));
    }

    #[test]
    fn test_checker_default() {
        let checker = Token2022Checker::default();
        let result = checker.check_mint(&[]);
        assert!(!result.is_token_2022);
    }

    #[test]
    fn test_result_default() {
        let result = Token2022Result::default();
        assert!(!result.is_token_2022);
        assert!(result.extensions.is_empty());
        assert_eq!(result.risk_score, 0);
        assert!(result.warnings.is_empty());
        assert!(!result.auto_reject);
    }

    #[test]
    fn test_risk_score_constants() {
        assert_eq!(RISK_PERMANENT_DELEGATE, 1000);
        assert_eq!(RISK_NON_TRANSFERABLE, 1000);
        assert_eq!(RISK_TRANSFER_HOOK, 1000);
        assert_eq!(RISK_TRANSFER_FEE, 500);
        assert_eq!(RISK_CONFIDENTIAL_TRANSFER, 500);
        assert_eq!(RISK_MINT_CLOSE_AUTHORITY, 200);
    }
}
