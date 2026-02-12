use crate::constants::PUMP_FUN_PROGRAM;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// Pump.fun Global state account
pub const PUMP_GLOBAL: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
// Pump.fun Fee recipient
pub const PUMP_FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
// Pump.fun Event Authority
pub const PUMP_EVENT_AUTHORITY: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";

pub struct BlindSniper;

pub struct BlindContext {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
}

impl BlindSniper {
    /// Create blind context by deriving PDAs from mint
    pub fn create_blind_context(mint: &Pubkey) -> BlindContext {
        // Derive bonding curve PDA
        let (bonding_curve, _) = Pubkey::find_program_address(
            &[b"bonding-curve", mint.as_ref()],
            &*PUMP_FUN_PROGRAM,
        );
        
        // Derive associated bonding curve (ATA for WSOL)
        let associated_bonding_curve = spl_associated_token_account::get_associated_token_address(
            &bonding_curve,
            mint,
        );
        
        BlindContext {
            mint: *mint,
            bonding_curve,
            associated_bonding_curve,
        }
    }

    /// Build Pump.fun buy instruction
    /// Instruction Layout:
    /// - Discriminator: [102, 6, 61, 18, 1, 218, 235, 234] (buy)
    /// - amount: u64 (token amount to buy)
    /// - max_sol_cost: u64 (maximum SOL to spend with slippage)
    pub fn build_buy_instruction(
        ctx: &BlindContext,
        user: &Pubkey,
        sol_amount: u64,    // Amount of SOL to spend (in lamports)
        slippage_bps: u64,  // Slippage tolerance in basis points
    ) -> Instruction {
        let global = Pubkey::from_str(PUMP_GLOBAL).unwrap();
        let fee_recipient = Pubkey::from_str(PUMP_FEE_RECIPIENT).unwrap();
        let event_authority = Pubkey::from_str(PUMP_EVENT_AUTHORITY).unwrap();
        
        // User's token account for the mint
        let user_token_account = spl_associated_token_account::get_associated_token_address(
            user,
            &ctx.mint,
        );
        
        // Calculate max_sol_cost with slippage
        let max_sol_cost = sol_amount + (sol_amount * slippage_bps / 10000);
        
        // For buy, we estimate token amount based on a rough initial price
        // The actual amount doesn't matter much as pump.fun uses max_sol_cost for the limit
        let token_amount = sol_amount * 1_000_000; // Rough estimate
        
        // Build instruction data
        // Discriminator for "buy" in Pump.fun: [102, 6, 61, 18, 1, 218, 235, 234]
        let mut data = vec![102, 6, 61, 18, 1, 218, 235, 234];
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&max_sol_cost.to_le_bytes());
        
        // Account layout for Pump.fun buy (2026 version - 16 accounts)
        let accounts = vec![
            AccountMeta::new_readonly(global, false),                         // 0: Global
            AccountMeta::new(fee_recipient, false),                           // 1: Fee recipient
            AccountMeta::new_readonly(ctx.mint, false),                       // 2: Mint
            AccountMeta::new(ctx.bonding_curve, false),                       // 3: Bonding curve
            AccountMeta::new(ctx.associated_bonding_curve, false),            // 4: Associated bonding curve
            AccountMeta::new(user_token_account, false),                      // 5: User token account
            AccountMeta::new(*user, true),                                    // 6: User (signer)
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false), // 7: System program
            AccountMeta::new_readonly(spl_token::id(), false),                // 8: Token program
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::id(), false), // 9: Rent
            AccountMeta::new_readonly(event_authority, false),                // 10: Event authority
            AccountMeta::new_readonly(*PUMP_FUN_PROGRAM, false),              // 11: Program (for CPI)
        ];
        
        Instruction {
            program_id: *PUMP_FUN_PROGRAM,
            accounts,
            data,
        }
    }
    
    /// Build Pump.fun sell instruction
    pub fn build_sell_instruction(
        ctx: &BlindContext,
        user: &Pubkey,
        token_amount: u64,  // Amount of tokens to sell
        min_sol_out: u64,   // Minimum SOL to receive (with slippage)
    ) -> Instruction {
        let global = Pubkey::from_str(PUMP_GLOBAL).unwrap();
        let fee_recipient = Pubkey::from_str(PUMP_FEE_RECIPIENT).unwrap();
        let event_authority = Pubkey::from_str(PUMP_EVENT_AUTHORITY).unwrap();
        
        let user_token_account = spl_associated_token_account::get_associated_token_address(
            user,
            &ctx.mint,
        );
        
        // Build instruction data
        // Discriminator for "sell" in Pump.fun: [51, 230, 133, 164, 1, 127, 131, 173]
        let mut data = vec![51, 230, 133, 164, 1, 127, 131, 173];
        data.extend_from_slice(&token_amount.to_le_bytes());
        data.extend_from_slice(&min_sol_out.to_le_bytes());
        
        let accounts = vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(ctx.mint, false),
            AccountMeta::new(ctx.bonding_curve, false),
            AccountMeta::new(ctx.associated_bonding_curve, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(spl_associated_token_account::id(), false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(*PUMP_FUN_PROGRAM, false),
        ];
        
        Instruction {
            program_id: *PUMP_FUN_PROGRAM,
            accounts,
            data,
        }
    }
}
