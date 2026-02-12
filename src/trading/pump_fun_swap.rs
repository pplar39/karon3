use crate::types::PumpFunToken;
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use spl_token_2022;
use std::str::FromStr;

lazy_static::lazy_static! {
    pub static ref PUMP_FUN_PROGRAM: Pubkey =
        Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").expect("PUMP_FUN_PROGRAM pubkey");
    pub static ref GLOBAL_ACCOUNT: Pubkey =
        Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").expect("GLOBAL_ACCOUNT pubkey");
    pub static ref FEE_RECIPIENT: Pubkey =
        Pubkey::from_str("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV").expect("FEE_RECIPIENT pubkey");
    pub static ref EVENT_AUTHORITY: Pubkey =
        Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1").expect("EVENT_AUTHORITY pubkey");
    pub static ref FEE_PROGRAM: Pubkey =
        Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").expect("FEE_PROGRAM pubkey");

    // Pre-derived constant PDAs (program-global, never change)
    pub static ref GLOBAL_VOLUME_ACCUMULATOR: Pubkey = {
        let (pda, _) = Pubkey::find_program_address(
            &[b"global_volume_accumulator"],
            &PUMP_FUN_PROGRAM,
        );
        pda
    };
    pub static ref FEE_CONFIG: Pubkey = {
        // 32-byte constant seed from IDL fee_config PDA definition
        let const_seed: [u8; 32] = [
            1, 86, 224, 246, 147, 102, 90, 207,
            68, 219, 21, 104, 191, 23, 91, 170,
            81, 137, 203, 151, 245, 210, 255, 59,
            101, 93, 43, 182, 253, 109, 24, 176,
        ];
        let (pda, _) = Pubkey::find_program_address(
            &[b"fee_config", &const_seed],
            &FEE_PROGRAM,
        );
        pda
    };
}

/// Derive creator_vault PDA: seeds = ["creator-vault", creator_pubkey]
pub fn derive_creator_vault(creator: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &PUMP_FUN_PROGRAM,
    );
    pda
}

/// Derive user_volume_accumulator PDA: seeds = ["user_volume_accumulator", user_wallet]
pub fn derive_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &PUMP_FUN_PROGRAM,
    );
    pda
}

pub struct PumpFunSwapBuilder {
    token: PumpFunToken,
}

impl PumpFunSwapBuilder {
    pub fn new(token: PumpFunToken) -> Self {
        Self { token }
    }

    // Zero-RPC Math
    // Virtual Reserves for a fresh Pump.fun pool
    pub const INITIAL_VIRTUAL_SOL_RESERVES: u128 = 30_000_000_000; // 30 SOL
    pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u128 = 1_073_000_000_000_000; // 1.073B tokens (decimals=6)

    pub fn calculate_tokens_for_sol(sol_amount_lamports: u64) -> u64 {
        let x_old = Self::INITIAL_VIRTUAL_SOL_RESERVES;
        let y_old = Self::INITIAL_VIRTUAL_TOKEN_RESERVES;
        let k = x_old * y_old;

        let x_new = x_old + (sol_amount_lamports as u128);
        let y_new = k / x_new; // Integer division acts as floor

        let tokens_out = y_old - y_new;
        tokens_out as u64
    }

    /// Build Buy instruction — 16 accounts per official IDL
    /// token.user = token creator (from create_v2 instruction)
    /// user = our wallet (signer)
    pub fn build_buy_instruction(
        &self,
        token_amount: u64,
        max_sol_cost: u64,
        user: &Pubkey,
        token_account: &Pubkey,
    ) -> Result<Instruction> {
        let discriminator: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];

        // Buy args: amount(u64) + max_sol_cost(u64) + track_volume(OptionBool = 1 byte)
        let mut data = Vec::with_capacity(25);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&max_sol_cost.to_le_bytes());
        data.push(0x00); // track_volume = OptionBool(false)

        let creator_vault = derive_creator_vault(&self.token.user);
        let user_volume_acc = derive_user_volume_accumulator(user);

        let accounts = vec![
            AccountMeta::new_readonly(*GLOBAL_ACCOUNT, false),              // [0]  global
            AccountMeta::new(*FEE_RECIPIENT, false),                        // [1]  fee_recipient
            AccountMeta::new_readonly(self.token.mint, false),              // [2]  mint
            AccountMeta::new(self.token.bonding_curve, false),              // [3]  bonding_curve
            AccountMeta::new(self.token.associated_bonding_curve, false),   // [4]  associated_bonding_curve
            AccountMeta::new(*token_account, false),                        // [5]  associated_user
            AccountMeta::new(*user, true),                                  // [6]  user (signer)
            AccountMeta::new_readonly(system_program::id(), false),         // [7]  system_program
            AccountMeta::new_readonly(spl_token_2022::id(), false),         // [8]  token_program ← CRITICAL: must be [8]
            AccountMeta::new(creator_vault, false),                         // [9]  creator_vault
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),             // [10] event_authority
            AccountMeta::new_readonly(*PUMP_FUN_PROGRAM, false),            // [11] program
            AccountMeta::new_readonly(*GLOBAL_VOLUME_ACCUMULATOR, false),   // [12] global_volume_accumulator
            AccountMeta::new(user_volume_acc, false),                       // [13] user_volume_accumulator
            AccountMeta::new_readonly(*FEE_CONFIG, false),                  // [14] fee_config
            AccountMeta::new_readonly(*FEE_PROGRAM, false),                 // [15] fee_program
        ];

        Ok(Instruction {
            program_id: *PUMP_FUN_PROGRAM,
            accounts,
            data,
        })
    }

    /// Build Sell instruction — 14 accounts per official IDL
    /// Note: Sell has creator_vault at [8], token_program at [9] (reversed vs Buy)
    pub fn build_sell_args(
        &self,
        token_amount: u64,
        min_sol_output: u64,
        user: &Pubkey,
        token_account: &Pubkey,
    ) -> Result<Instruction> {
        let discriminator: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&min_sol_output.to_le_bytes());

        let creator_vault = derive_creator_vault(&self.token.user);

        let accounts = vec![
            AccountMeta::new_readonly(*GLOBAL_ACCOUNT, false),              // [0]  global
            AccountMeta::new(*FEE_RECIPIENT, false),                        // [1]  fee_recipient
            AccountMeta::new_readonly(self.token.mint, false),              // [2]  mint
            AccountMeta::new(self.token.bonding_curve, false),              // [3]  bonding_curve
            AccountMeta::new(self.token.associated_bonding_curve, false),   // [4]  associated_bonding_curve
            AccountMeta::new(*token_account, false),                        // [5]  associated_user
            AccountMeta::new(*user, true),                                  // [6]  user (signer)
            AccountMeta::new_readonly(system_program::id(), false),         // [7]  system_program
            AccountMeta::new(creator_vault, false),                         // [8]  creator_vault
            AccountMeta::new_readonly(spl_token_2022::id(), false),         // [9]  token_program (Token-2022)
            AccountMeta::new_readonly(*EVENT_AUTHORITY, false),             // [10] event_authority
            AccountMeta::new_readonly(*PUMP_FUN_PROGRAM, false),            // [11] program
            AccountMeta::new_readonly(*FEE_CONFIG, false),                  // [12] fee_config
            AccountMeta::new_readonly(*FEE_PROGRAM, false),                 // [13] fee_program
        ];

        Ok(Instruction {
            program_id: *PUMP_FUN_PROGRAM,
            accounts,
            data,
        })
    }
}
