//! NOTCH curve client: state mirror + instruction builders + SPL helpers.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction, system_program,
};
use std::str::FromStr;

pub const CURVE_SEED: &[u8] = b"curve";
pub const CURVE_SIZE: usize = 147;
pub const MINT_SIZE: usize = 82;
pub const FP: u128 = 1_000_000_000;

pub fn token_program() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}

pub fn ata_program() -> Pubkey {
    Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap()
}

#[derive(BorshSerialize, BorshDeserialize, Debug, Default, Clone)]
pub struct Curve {
    pub mint: Pubkey,
    pub buy_creator: Pubkey,
    pub sell_creator: Pubkey,
    pub price_fp: u128,
    pub double_vol: u64,
    pub buy_fee_creator_bps: u16,
    pub buy_fee_floor_bps: u16,
    pub sell_fee_creator_bps: u16,
    pub sell_fee_floor_bps: u16,
    pub min_backing_bps: u16,
    pub cum_vol: u128,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum CurveInstruction {
    Initialize {
        buy_creator: Pubkey,
        sell_creator: Pubkey,
        start_price_fp: u128,
        double_vol: u64,
        buy_fee_creator_bps: u16,
        buy_fee_floor_bps: u16,
        sell_fee_creator_bps: u16,
        sell_fee_floor_bps: u16,
        min_backing_bps: u16,
    },
    Buy { lamports: u64, min_out: u64 },
    Sell { units: u64, min_out: u64 },
}

/// Launch configuration bundle (mirrors Initialize params).
#[derive(Clone, Copy, Debug)]
pub struct LaunchCfg {
    pub start_price_fp: u128,
    pub double_vol: u64,
    pub buy_fee_creator_bps: u16,
    pub buy_fee_floor_bps: u16,
    pub sell_fee_creator_bps: u16,
    pub sell_fee_floor_bps: u16,
    pub min_backing_bps: u16,
}

pub fn curve_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CURVE_SEED, mint.as_ref()], program_id)
}

pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program().as_ref(), mint.as_ref()],
        &ata_program(),
    )
    .0
}

// ---------------------------------------------------------------------------
// Program instructions
// ---------------------------------------------------------------------------

pub fn initialize(
    program_id: &Pubkey,
    payer: &Pubkey,
    buy_creator: &Pubkey,
    sell_creator: &Pubkey,
    mint: &Pubkey,
    cfg: &LaunchCfg,
) -> Instruction {
    let (pda, _) = curve_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: borsh::to_vec(&CurveInstruction::Initialize {
            buy_creator: *buy_creator,
            sell_creator: *sell_creator,
            start_price_fp: cfg.start_price_fp,
            double_vol: cfg.double_vol,
            buy_fee_creator_bps: cfg.buy_fee_creator_bps,
            buy_fee_floor_bps: cfg.buy_fee_floor_bps,
            sell_fee_creator_bps: cfg.sell_fee_creator_bps,
            sell_fee_floor_bps: cfg.sell_fee_floor_bps,
            min_backing_bps: cfg.min_backing_bps,
        })
        .unwrap(),
    }
}

pub fn buy(
    program_id: &Pubkey,
    buyer: &Pubkey,
    mint: &Pubkey,
    buy_creator: &Pubkey,
    lamports: u64,
    min_out: u64,
) -> Instruction {
    let (pda, _) = curve_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*buyer, true),
            AccountMeta::new(pda, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new(ata(buyer, mint), false),
            AccountMeta::new(*buy_creator, false),
            AccountMeta::new_readonly(token_program(), false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
        data: borsh::to_vec(&CurveInstruction::Buy { lamports, min_out }).unwrap(),
    }
}

pub fn sell(
    program_id: &Pubkey,
    seller: &Pubkey,
    mint: &Pubkey,
    sell_creator: &Pubkey,
    units: u64,
    min_out: u64,
) -> Instruction {
    let (pda, _) = curve_pda(program_id, mint);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*seller, true),
            AccountMeta::new(pda, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new(ata(seller, mint), false),
            AccountMeta::new(*sell_creator, false),
            AccountMeta::new_readonly(token_program(), false),
        ],
        data: borsh::to_vec(&CurveInstruction::Sell { units, min_out }).unwrap(),
    }
}

/// Sell that appends the payout PDA + system program (accounts 7 & 8), so the
/// program pays the seller/creator via visible System transfer CPIs.
pub fn sell_via_payout(
    program_id: &Pubkey,
    seller: &Pubkey,
    mint: &Pubkey,
    sell_creator: &Pubkey,
    units: u64,
    min_out: u64,
) -> Instruction {
    let (pda, _) = curve_pda(program_id, mint);
    let (payout, _) = Pubkey::find_program_address(&[b"payout"], program_id);
    Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(*seller, true),
            AccountMeta::new(pda, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new(ata(seller, mint), false),
            AccountMeta::new(*sell_creator, false),
            AccountMeta::new_readonly(token_program(), false),
            AccountMeta::new(payout, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data: borsh::to_vec(&CurveInstruction::Sell { units, min_out }).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// SPL setup helpers (mint creation + ATA)
// ---------------------------------------------------------------------------

/// create_account for the mint (payer funds, mint keypair signs) +
/// InitializeMint2 { decimals 9, mint_authority = curve PDA, no freeze }.
pub fn create_mint_ixs(
    program_id: &Pubkey,
    payer: &Pubkey,
    mint: &Pubkey,
    mint_rent: u64,
) -> Vec<Instruction> {
    let (pda, _) = curve_pda(program_id, mint);
    let mut data = Vec::with_capacity(35);
    data.push(20u8); // InitializeMint2
    data.push(9u8); // decimals
    data.extend_from_slice(pda.as_ref()); // mint_authority
    data.push(0u8); // freeze_authority = None
    vec![
        system_instruction::create_account(payer, mint, mint_rent, MINT_SIZE as u64, &token_program()),
        Instruction {
            program_id: token_program(),
            accounts: vec![AccountMeta::new(*mint, false)],
            data,
        },
    ]
}

/// Associated token account CreateIdempotent.
pub fn create_ata_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ata_program(),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(token_program(), false),
        ],
        data: vec![1u8], // CreateIdempotent
    }
}

// ---------------------------------------------------------------------------
// Account readers
// ---------------------------------------------------------------------------

pub fn parse_curve(data: &[u8]) -> Option<Curve> {
    Curve::try_from_slice(data).ok()
}

/// SPL mint supply (u64 at [36..44]).
pub fn mint_supply(data: &[u8]) -> u64 {
    if data.len() < 44 {
        return 0;
    }
    u64::from_le_bytes(data[36..44].try_into().unwrap())
}

/// SPL token account amount (u64 at [64..72]).
pub fn token_amount(data: &[u8]) -> u64 {
    if data.len() < 72 {
        return 0;
    }
    u64::from_le_bytes(data[64..72].try_into().unwrap())
}
