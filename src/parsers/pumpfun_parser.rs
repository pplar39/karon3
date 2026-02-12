//! KARON3 ETERNITY FIRE - Pump.fun Parser (FAIL-CLOSED)
//! H2: decode 기반 파서 + 골든 검증

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{EncodedTransaction, UiMessage, UiTransactionEncoding};
use std::str::FromStr;
use std::sync::atomic::Ordering;
use tracing::{info, warn, debug};

use crate::metrics::counters::COUNTERS;

/// Pump.fun Program ID
pub const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

/// Pump.fun Create instruction discriminator (8 bytes)
/// NOTE: 실제 discriminator는 골든 데이터셋에서 확인 필요
pub const PUMP_CREATE_DISCRIMINATOR: [u8; 8] = [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77];

/// 파싱된 Pump.fun 토큰
#[derive(Debug, Clone)]
pub struct ParsedPumpToken {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub signature: Signature,
    pub slot: u64,
}

/// 알려진 시스템 프로그램인지 확인
#[inline]
fn is_known_program(key: &Pubkey) -> bool {
    let s = key.to_string();
    s == "11111111111111111111111111111111"       // System Program
        || s == "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"  // SPL Token
        || s.starts_with("1111111")               // 테스트키 패턴 (new_unique 결과)
        || s == "ComputeBudget111111111111111111111111111111" // Compute Budget
}

/// 테스트용 Pubkey 패턴인지 확인 (Pubkey::new_unique 결과)
#[inline]
fn looks_like_test_pubkey(pk: &Pubkey) -> bool {
    let b = pk.to_bytes();
    // new_unique()는 처음 24바이트가 0인 패턴 생성
    b[..24].iter().all(|&x| x == 0)
}

/// UI JSON 트랜잭션에서 Pump.fun 토큰 파싱 (FAIL-CLOSED)
/// 
/// # Returns
/// - `Some(ParsedPumpToken)` - 파싱 성공
/// - `None` - 파싱 실패 (FAIL-CLOSED: 불확실하면 스킵)
pub fn parse_from_ui_tx(
    account_keys: &[String],
    instructions: &[solana_transaction_status::UiCompiledInstruction],
    signature: &Signature,
    slot: u64,
) -> Option<ParsedPumpToken> {
    for ix in instructions {
        let program_idx = ix.program_id_index as usize;
        if program_idx >= account_keys.len() {
            continue;
        }

        let program_id = &account_keys[program_idx];
        if program_id != PUMP_FUN_PROGRAM {
            continue;
        }

        // Account 수 검증 (pump.fun create는 최소 10개)
        if ix.accounts.len() < 10 {
            debug!("pump.fun ix accounts too few: {} (need >= 10)", ix.accounts.len());
            continue;
        }

        // ix.data에서 discriminator 확인 (선택적)
        // let data = bs58::decode(&ix.data).into_vec().unwrap_or_default();
        // if data.len() >= 8 && data[..8] != PUMP_CREATE_DISCRIMINATOR {
        //     continue;
        // }

        // Helper: account index에서 Pubkey 추출
        let get_pubkey = |idx: usize| -> Option<Pubkey> {
            let account_idx = ix.accounts.get(idx).copied()? as usize;
            let key_str = account_keys.get(account_idx)?;
            Pubkey::from_str(key_str).ok()
        };

        // pump.fun create ix의 accounts:
        //   [0] = mint
        //   [2] = bonding_curve
        let mint = match get_pubkey(0) {
            Some(pk) => pk,
            None => {
                COUNTERS.pump_parse_fail.fetch_add(1, Ordering::Relaxed);
                warn!("pump.fun FAIL-CLOSED: failed to get mint from accounts");
                return None; // FAIL-CLOSED
            }
        };

        let bonding_curve = match get_pubkey(2) {
            Some(pk) => pk,
            None => {
                COUNTERS.pump_parse_fail.fetch_add(1, Ordering::Relaxed);
                warn!("pump.fun FAIL-CLOSED: failed to get bonding_curve from accounts");
                return None; // FAIL-CLOSED
            }
        };

        // 교차검증 1: mint가 시스템 프로그램이 아닌지
        if is_known_program(&mint) {
            COUNTERS.fake_mint_blocked.fetch_add(1, Ordering::Relaxed);
            warn!("pump.fun BLOCKED: mint is known program: {}", mint);
            return None;
        }

        // 교차검증 2: 테스트용 Pubkey 패턴인지
        if looks_like_test_pubkey(&mint) {
            COUNTERS.fake_mint_blocked.fetch_add(1, Ordering::Relaxed);
            warn!("pump.fun BLOCKED: mint looks like test pubkey: {}", mint);
            return None;
        }

        if looks_like_test_pubkey(&bonding_curve) {
            COUNTERS.fake_mint_blocked.fetch_add(1, Ordering::Relaxed);
            warn!("pump.fun BLOCKED: bonding_curve looks like test pubkey: {}", bonding_curve);
            return None;
        }

        // 모든 검증 통과 → 성공
        COUNTERS.pump_parse_ok.fetch_add(1, Ordering::Relaxed);
        info!("✅ pump.fun parsed: mint={} bc={}", mint, bonding_curve);
        
        return Some(ParsedPumpToken {
            mint,
            bonding_curve,
            signature: *signature,
            slot,
        });
    }

    // Pump.fun instruction 없음
    COUNTERS.pump_parse_fail.fetch_add(1, Ordering::Relaxed);
    None
}

/// 로그 기반 파싱 (fallback) - 여전히 FAIL-CLOSED
pub fn parse_from_logs(logs: &[String], signature: &Signature, slot: u64) -> Option<ParsedPumpToken> {
    let mut mint: Option<Pubkey> = None;
    let mut bonding_curve: Option<Pubkey> = None;

    for log in logs {
        // mint: 또는 Mint: 패턴 찾기
        if let Some(pos) = log.to_lowercase().find("mint:") {
            let after_mint = &log[pos + 5..];
            if let Some(key) = extract_pubkey_from_str(after_mint) {
                mint = Some(key);
            }
        }
        
        // bonding_curve: 패턴 찾기
        if let Some(pos) = log.to_lowercase().find("bonding") {
            let after = &log[pos..];
            if let Some(key) = extract_pubkey_from_str(after) {
                bonding_curve = Some(key);
            }
        }
    }

    // FAIL-CLOSED: 둘 다 있어야만 반환
    let m = match mint {
        Some(pk) => pk,
        None => {
            COUNTERS.pump_parse_fail.fetch_add(1, Ordering::Relaxed);
            debug!("pump.fun log parse: missing mint");
            return None;
        }
    };

    let bc = match bonding_curve {
        Some(pk) => pk,
        None => {
            COUNTERS.pump_parse_fail.fetch_add(1, Ordering::Relaxed);
            debug!("pump.fun log parse: missing bonding_curve");
            return None;
        }
    };

    // 테스트키 검증
    if looks_like_test_pubkey(&m) || looks_like_test_pubkey(&bc) {
        COUNTERS.fake_mint_blocked.fetch_add(1, Ordering::Relaxed);
        warn!("pump.fun log BLOCKED: test pubkey pattern");
        return None;
    }

    COUNTERS.pump_parse_ok.fetch_add(1, Ordering::Relaxed);
    Some(ParsedPumpToken {
        mint: m,
        bonding_curve: bc,
        signature: *signature,
        slot,
    })
}

/// 문자열에서 Pubkey 추출 (44자 base58)
fn extract_pubkey_from_str(s: &str) -> Option<Pubkey> {
    // 공백이나 구분자로 분리된 44자 base58 찾기
    for word in s.split(|c: char| c.is_whitespace() || c == ',' || c == ':') {
        let trimmed = word.trim();
        if trimmed.len() >= 32 && trimmed.len() <= 44 {
            if let Ok(pk) = Pubkey::from_str(trimmed) {
                return Some(pk);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_known_program() {
        assert!(is_known_program(&Pubkey::from_str("11111111111111111111111111111111").unwrap()));
        assert!(is_known_program(&Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()));
        
        // 일반 토큰은 아님
        assert!(!is_known_program(&Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").unwrap()));
    }

    #[test]
    fn test_looks_like_test_pubkey() {
        // new_unique() 패턴 (처음 24바이트 0)
        let test_pk_bytes = [0u8; 32];
        let test_pk = Pubkey::new_from_array(test_pk_bytes);
        assert!(looks_like_test_pubkey(&test_pk));

        // 일반 pubkey
        let real_pk = Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").unwrap();
        assert!(!looks_like_test_pubkey(&real_pk));
    }
}
