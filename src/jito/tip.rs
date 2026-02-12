//! KARON3 ETERNITY FIRE - Jito Tip Configuration
//! H3: Jito 400 방지 - 규격 고정

use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Frankfurt 리전 Jito tip accounts (공식 목록)
pub const TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiNPLniS7",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUC5Dqv3djYbRFGhi2cvzMFrYtb1yJgVkN8",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL91KN",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

static TIP_INDEX: AtomicUsize = AtomicUsize::new(0);

/// 라운드로빈으로 다음 tip account 선택
pub fn next_tip_account() -> Pubkey {
    let idx = TIP_INDEX.fetch_add(1, Ordering::Relaxed) % TIP_ACCOUNTS.len();
    Pubkey::from_str(TIP_ACCOUNTS[idx]).expect("Invalid tip account")
}

/// Tip 설정
#[derive(Clone, Debug)]
pub struct TipConfig {
    pub min_lamports: u64,      // 10_000 (0.00001 SOL)
    pub default_lamports: u64,  // 10_000
    pub max_lamports: u64,      // 100_000 (0.0001 SOL)
}

impl Default for TipConfig {
    fn default() -> Self {
        Self {
            min_lamports: 10_000,
            default_lamports: 10_000,
            max_lamports: 100_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tip_accounts_valid() {
        for account in TIP_ACCOUNTS.iter() {
            let result = Pubkey::from_str(account);
            assert!(result.is_ok(), "Invalid tip account: {}", account);
        }
    }

    #[test]
    fn test_next_tip_round_robin() {
        let first = next_tip_account();
        let second = next_tip_account();
        // 서로 다른 계정이어야 함 (라운드로빈)
        assert_ne!(first, second);
    }
}
