use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// Lazy static pubkeys
lazy_static::lazy_static! {
    // Token Program
    pub static ref TOKEN_PROGRAM: Pubkey =
        Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();

    // Wrapped SOL
    pub static ref WSOL_MINT: Pubkey =
        Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();

    // USDC
    pub static ref USDC_MINT: Pubkey =
        Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();

    // Burn address (for LP burn check)
    pub static ref BURN_ADDRESS: Pubkey =
        Pubkey::from_str("1nc1nerator11111111111111111111111111111111").unwrap();

    // LP Locker Programs
    pub static ref UNCX_LOCKER: Pubkey =
        Pubkey::from_str("GsSCS3vPWrtJ5Y9aEVVT65fmrex5P5RGHXdZvsdbWgfo").unwrap();
    pub static ref SMITHII_LOCKER: Pubkey =
        Pubkey::from_str("vesFcnNXtfS9JMtspbe9SkMJiRiSPwsywuWMjYwxQ2K").unwrap();
}

// Jito Block Engine Endpoints
pub const JITO_FRANKFURT: &str = "frankfurt.mainnet.block-engine.jito.wtf";
pub const JITO_TOKYO: &str = "tokyo.mainnet.block-engine.jito.wtf";
pub const JITO_AMSTERDAM: &str = "amsterdam.mainnet.block-engine.jito.wtf";
pub const JITO_NY: &str = "ny.mainnet.block-engine.jito.wtf";

// Pump.fun constants
lazy_static::lazy_static! {
    pub static ref PUMP_FUN_PROGRAM: Pubkey =
        Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();
}
pub const PUMP_FUN_CREATE_V2_DISCRIMINATOR: u64 = 0xd6904cec5f8b31b4;

// Timing constants
pub const SLOT_DURATION_MS: u64 = 400;
pub const MAX_DETECTION_LATENCY_MS: u64 = 500;

// Trading defaults
pub const DEFAULT_SLIPPAGE_BPS: u64 = 100; // 1%
pub const DEFAULT_PRIORITY_FEE: u64 = 100_000; // 0.0001 SOL
pub const DEFAULT_JITO_TIP: u64 = 30_000_000; // 0.03 SOL
