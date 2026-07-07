//! NOTCH — notch-curve token vault. The quoted price only NOTCHES up.
//!
//! v2 tokenomics (per-launch config, immutable after Initialize):
//!   * BUY fee (e.g. 3%): `buy_fee_creator_bps` to the creator wallet,
//!     `buy_fee_floor_bps` donated straight into the vault (lifts NAV for
//!     everyone, including the buyer). The remainder mints tokens at the
//!     governor price = NAV / backing.
//!   * SELL fee (e.g. 6%): seller redeems at NAV; `sell_fee_creator_bps` of
//!     gross to the creator, `sell_fee_floor_bps` STAYS in the vault (sells
//!     raise the floor). Sells never move the price.
//!   * MINT (path-independent): tokens are minted at price = NAV / backing via
//!     the exact power law `S1 = S0 * (Vf/V0)^backing` (integer fixed-point,
//!     see pow_ratio_q). Composable => buy(a) then buy(b) == buy(a+b), so the
//!     minted amount does NOT depend on how a buy is split. (Replaces the old
//!     chunked `2^(net/double_vol)` curve, which carried a laggy price and was
//!     path-dependent: splitting a buy farmed free tokens. `double_vol`/
//!     `price_fp` remain in state but are vestigial; price_fp = reported price.)
//!   * GOVERNOR: `min_backing_bps` (e.g. 9350 = 93.5%) pins price at NAV/ratio,
//!     bounding the price-vs-floor gap forever, so the worst instant round trip
//!     is fees + gap (3%/6% fees, 93.5% backing: ~-14.7% all-in; platform floor
//!     82.5% => <=25%). PLATFORM RULE: min_backing_bps must be 8250..=9900.
//!
//! Invariants (enforced by construction, proven in curve_test):
//!   * curve price is monotone nondecreasing; NAV is monotone nondecreasing
//!     on both buys and sells; backing ratio never drops below the governor.
//!   * NO admin instructions. Vault SOL only leaves through Sell redemptions.
//!     The creator only ever receives flow fees. Non-custodial by construction.
//!   * One deployment serves many launches: curve PDA seeds = ["curve", mint];
//!     the mint's authority must be the curve PDA (verified at Initialize).

use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const CURVE_SEED: &[u8] = b"curve";
/// mint(32) buy_creator(32) sell_creator(32) price_fp(16) double_vol(8)
/// buy_fee_creator(2) buy_fee_floor(2) sell_fee_creator(2) sell_fee_floor(2)
/// min_backing(2) cum_vol(16) bump(1)
pub const CURVE_SIZE: usize = 32 + 32 + 32 + 16 + 8 + 2 + 2 + 2 + 2 + 2 + 16 + 1; // = 147 (mint + buy_creator + sell_creator + ...)

const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Fixed-point scale. `price_fp` = lamports per WHOLE token (1e9 base units),
/// scaled by FP. So lamports per base unit = price_fp / 1e18.
pub const FP: u128 = 1_000_000_000;
const E18: u128 = 1_000_000_000_000_000_000;
/// Fixed-point (Q48) scale for the path-independent power-law mint. Buys mint
/// `S1 = S0 * (Vf/V0)^backing` exactly in integer math (see `pow_ratio_q`),
/// which is composable — buy(a) then buy(b) == buy(a+b) — so the minted amount
/// no longer depends on how a buy is split. (The old chunked `2^(net/double_vol)`
/// curve carried a laggy price and was path-dependent: splitting a buy farmed
/// free tokens. `double_vol`/`price_fp` remain in state but no longer drive the
/// mint; `price_fp` is now just the reported governor price NAV/backing.)
const POW_F: u32 = 48;
const POW_ONE: u128 = 1u128 << POW_F; // 2^48 == 1.0 in Q48
/// TWO_POW_LUT[i] = round(2^(2^-i) * 2^48), i = 1..=48 (index 0 unused).
const TWO_POW_LUT: [u128; 49] = [
    0, 398065729532861, 334732044999537, 306950638654744, 293936938588305,
    287638476118103, 284540038248454, 283003357999923, 282238132792268,
    281856296460737, 281665572056717, 281570258256901, 281522613452764,
    281498794074042, 281486885140443, 281480930862574, 281477953770871,
    281476465236828, 281475720972758, 281475348841461, 281475162775997,
    281475069743311, 281475023226980, 281474999968817, 281474988339736,
    281474982525196, 281474979617926, 281474978164291, 281474977437473,
    281474977074065, 281474976892360, 281474976801508, 281474976756082,
    281474976733369, 281474976722013, 281474976716334, 281474976713495,
    281474976712076, 281474976711366, 281474976711011, 281474976710833,
    281474976710745, 281474976710700, 281474976710678, 281474976710667,
    281474976710662, 281474976710659, 281474976710657, 281474976710657,
];

/// Hard caps so a launch can't be configured degenerately.
pub const MAX_CREATOR_FEE_BPS: u16 = 500; // 5% per side
pub const MAX_BUY_FLOOR_BPS: u16 = 1_000; // 10%
pub const MAX_SELL_FLOOR_BPS: u16 = 2_000; // 20%
pub const MIN_BACKING_FLOOR_BPS: u16 = 8_250; // PLATFORM RULE: backing >= 82.5% => max round-trip loss <= 25% at 3%/6% fees (no ungoverned/degen launches)
pub const MAX_BACKING_BPS: u16 = 9_900; // and at most 99% (100% = zero speed)
pub const MIN_DOUBLE_VOL: u64 = 1_000_000_000; // 1 SOL per 2x minimum
pub const MAX_DOUBLE_VOL: u64 = 10_000_000_000_000; // 10k SOL per 2x maximum
pub const MIN_START_PRICE_FP: u128 = 1_000; // 1e-15 SOL/token
pub const MAX_PRICE_FP: u128 = 1_000_000_000_000_000_000_000_000_000_000; // 1e30

// Custom errors.
const E_BAD_PARAMS: u32 = 1;
const E_BAD_PDA: u32 = 2;
const E_BAD_MINT: u32 = 3;
const E_BAD_CREATOR: u32 = 4;
const E_SLIPPAGE: u32 = 5;
const E_BUY_TOO_LARGE: u32 = 6;
const E_INSUFFICIENT_VAULT: u32 = 7;
const E_OVERFLOW: u32 = 8;
const E_ALREADY_INIT: u32 = 9;
const E_ZERO_AMOUNT: u32 = 10;
const E_BAD_TOKEN_ACCOUNT: u32 = 11;
const E_CURVE_SATURATED: u32 = 12;

fn err(code: u32) -> ProgramError {
    ProgramError::Custom(code)
}

// ---------------------------------------------------------------------------
// Fixed-point power (Q48): computes ratio^backing with no floats, for the
// path-independent mint. log2 by repeated squaring; exp2 by the 2^(2^-i) table.
// ---------------------------------------------------------------------------

/// log2(x / 2^48) in Q48, for x >= 2^48 (i.e. value >= 1).
fn log2_q(mut x: u128) -> u128 {
    let mut e: u128 = 0;
    while x >= (POW_ONE << 1) {
        x >>= 1;
        e += 1;
    }
    // x is now in [2^48, 2^49): the mantissa is in [1, 2).
    let mut result = e << POW_F;
    let mut i: u32 = 1;
    while i <= POW_F {
        x = (x * x) >> POW_F; // x < 2^49 => x*x < 2^98, safe in u128
        if x >= (POW_ONE << 1) {
            result += POW_ONE >> i;
            x >>= 1;
        }
        i += 1;
    }
    result
}

/// 2^(y / 2^48) in Q48. Saturates to u128::MAX on overflow (the caller then
/// fails the buy as too large; safe because minting is now split-invariant).
fn exp2_q(y: u128) -> u128 {
    let int_part = y >> POW_F;
    let frac = y & (POW_ONE - 1);
    let mut r = POW_ONE; // 1.0 in Q48
    let mut i: u32 = 1;
    while i <= POW_F {
        if frac & (POW_ONE >> i) != 0 {
            r = (r * TWO_POW_LUT[i as usize]) >> POW_F; // r,LUT < 2^49 => < 2^98
        }
        i += 1;
    }
    if int_part >= 128 {
        return u128::MAX;
    }
    let ip = int_part as u32;
    if r > (u128::MAX >> ip) {
        u128::MAX
    } else {
        r << ip
    }
}

/// (num / den)^(bps / 10000) in Q48. Requires num >= den >= 1. Returns None on
/// overflow (buy too large relative to the pool — split it; split-invariant).
fn pow_ratio_q(num: u128, den: u128, bps: u16) -> Option<u128> {
    if den == 0 || num < den {
        return None;
    }
    let ratio = num.checked_mul(POW_ONE)? / den; // >= 2^48
    let l = log2_q(ratio);
    let b = (bps as u128 * POW_ONE) / 10_000;
    let bl = l.checked_mul(b)? >> POW_F;
    Some(exp2_q(bl))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(BorshSerialize, BorshDeserialize, Debug, Default)]
pub struct Curve {
    pub mint: Pubkey,
    /// Receives the buy creator fee (buy_fee_creator_bps of each buy).
    pub buy_creator: Pubkey,
    /// Receives the sell creator fee (sell_fee_creator_bps of each sell gross).
    pub sell_creator: Pubkey,
    /// Lamports per whole token, scaled by FP. Monotone nondecreasing.
    pub price_fp: u128,
    /// Net buy lamports per price doubling (the schedule speed dial).
    pub double_vol: u64,
    /// Buy fee to creator (bps of the buy amount).
    pub buy_fee_creator_bps: u16,
    /// Buy fee donated to the vault (bps of the buy amount) — instant floor lift.
    pub buy_fee_floor_bps: u16,
    /// Sell fee to creator (bps of gross NAV value).
    pub sell_fee_creator_bps: u16,
    /// Sell fee that stays in the vault (bps of gross) — sells raise NAV.
    pub sell_fee_floor_bps: u16,
    /// Governor: price <= NAV * 10000 / min_backing_bps. Always 5000..=9900.
    pub min_backing_bps: u16,
    /// Cumulative net buy lamports (stats).
    pub cum_vol: u128,
    pub bump: u8,
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub enum CurveInstruction {
    /// Create the curve for `mint`. Mint must be: decimals 9, supply 0,
    /// mint_authority == curve PDA, freeze_authority == None. `buy_creator`
    /// and `sell_creator` are the fee-recipient addresses (may differ, may be
    /// the payer); they are recorded, not signed, at init.
    /// Accounts: [payer (signer, writable), curve PDA (writable),
    ///            mint, system_program]
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
    /// Pay `lamports`, receive tokens at the (advancing, governed) curve price.
    /// Accounts: [buyer (signer, writable), curve PDA (writable),
    ///            mint (writable), buyer_token_account (writable),
    ///            buy_creator (writable), token_program, system_program]
    Buy { lamports: u64, min_out: u64 },
    /// Burn `units`, redeem at NAV minus sell fees (floor share stays).
    /// Accounts: [seller (signer, writable), curve PDA (writable),
    ///            mint (writable), seller_token_account (writable),
    ///            sell_creator (writable), token_program]
    Sell { units: u64, min_out: u64 },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn curve_pda(program_id: &Pubkey, mint: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[CURVE_SEED, mint.as_ref()], program_id)
}

/// Parse the SPL mint fields we care about.
struct MintView {
    authority: Option<Pubkey>,
    supply: u64,
    decimals: u8,
    initialized: bool,
    freeze_authority: Option<Pubkey>,
}

fn parse_mint(data: &[u8]) -> Result<MintView, ProgramError> {
    if data.len() < 82 {
        return Err(err(E_BAD_MINT));
    }
    let opt = |tag_off: usize, key_off: usize| -> Option<Pubkey> {
        let tag = u32::from_le_bytes(data[tag_off..tag_off + 4].try_into().unwrap());
        if tag == 1 {
            Some(Pubkey::new_from_array(data[key_off..key_off + 32].try_into().unwrap()))
        } else {
            None
        }
    };
    Ok(MintView {
        authority: opt(0, 4),
        supply: u64::from_le_bytes(data[36..44].try_into().unwrap()),
        decimals: data[44],
        initialized: data[45] == 1,
        freeze_authority: opt(46, 50),
    })
}

/// SPL token account mint field ([0..32]).
fn token_account_mint(data: &[u8]) -> Result<Pubkey, ProgramError> {
    if data.len() < 72 {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }
    Ok(Pubkey::new_from_array(data[0..32].try_into().unwrap()))
}

fn rent_floor() -> Result<u64, ProgramError> {
    Ok(Rent::get()?.minimum_balance(CURVE_SIZE))
}

/// Vault backing = curve PDA lamports above its rent-exempt floor.
/// (Donations to the PDA count as backing — anyone may pump the floor.)
fn backing(curve_ai: &AccountInfo) -> Result<u64, ProgramError> {
    Ok(curve_ai.lamports().saturating_sub(rent_floor()?))
}

fn load_curve(
    program_id: &Pubkey,
    curve_ai: &AccountInfo,
    mint_ai: &AccountInfo,
) -> Result<Curve, ProgramError> {
    if curve_ai.owner != program_id {
        return Err(err(E_BAD_PDA));
    }
    let curve = Curve::try_from_slice(&curve_ai.data.borrow())?;
    let (expect, _) = curve_pda(program_id, &curve.mint);
    if expect != *curve_ai.key || curve.mint != *mint_ai.key {
        return Err(err(E_BAD_PDA));
    }
    Ok(curve)
}

fn store_curve(curve: &Curve, curve_ai: &AccountInfo) -> ProgramResult {
    curve.serialize(&mut &mut curve_ai.data.borrow_mut()[..])?;
    Ok(())
}

fn spl_mint_to(mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(7u8); // MintTo
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

fn spl_burn(account: &Pubkey, mint: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(8u8); // Burn
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*mint, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match CurveInstruction::try_from_slice(data)? {
        CurveInstruction::Initialize {
            buy_creator,
            sell_creator,
            start_price_fp,
            double_vol,
            buy_fee_creator_bps,
            buy_fee_floor_bps,
            sell_fee_creator_bps,
            sell_fee_floor_bps,
            min_backing_bps,
        } => initialize(
            program_id,
            accounts,
            buy_creator,
            sell_creator,
            start_price_fp,
            double_vol,
            buy_fee_creator_bps,
            buy_fee_floor_bps,
            sell_fee_creator_bps,
            sell_fee_floor_bps,
            min_backing_bps,
        ),
        CurveInstruction::Buy { lamports, min_out } => buy(program_id, accounts, lamports, min_out),
        CurveInstruction::Sell { units, min_out } => sell(program_id, accounts, units, min_out),
    }
}

// ---------------------------------------------------------------------------
// Initialize
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn initialize(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    buy_creator: Pubkey,
    sell_creator: Pubkey,
    start_price_fp: u128,
    double_vol: u64,
    buy_fee_creator_bps: u16,
    buy_fee_floor_bps: u16,
    sell_fee_creator_bps: u16,
    sell_fee_floor_bps: u16,
    min_backing_bps: u16,
) -> ProgramResult {
    let ai = &mut accounts.iter();
    let payer_ai = next_account_info(ai)?;
    let curve_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let system_ai = next_account_info(ai)?;

    if !payer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_ai.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if !(MIN_START_PRICE_FP..=MAX_PRICE_FP).contains(&start_price_fp)
        || !(MIN_DOUBLE_VOL..=MAX_DOUBLE_VOL).contains(&double_vol)
        || buy_fee_creator_bps > MAX_CREATOR_FEE_BPS
        || sell_fee_creator_bps > MAX_CREATOR_FEE_BPS
        || buy_fee_floor_bps > MAX_BUY_FLOOR_BPS
        || sell_fee_floor_bps > MAX_SELL_FLOOR_BPS
        || !(MIN_BACKING_FLOOR_BPS..=MAX_BACKING_BPS).contains(&min_backing_bps)
    {
        return Err(err(E_BAD_PARAMS));
    }

    let (pda, bump) = curve_pda(program_id, mint_ai.key);
    if pda != *curve_ai.key {
        return Err(err(E_BAD_PDA));
    }
    if !curve_ai.data_is_empty() || curve_ai.owner == program_id {
        return Err(err(E_ALREADY_INIT));
    }

    // The mint must be fully under the curve's control before any supply exists.
    if *mint_ai.owner != TOKEN_PROGRAM {
        return Err(err(E_BAD_MINT));
    }
    let mint = parse_mint(&mint_ai.data.borrow())?;
    if !mint.initialized
        || mint.decimals != 9
        || mint.supply != 0
        || mint.authority != Some(pda)
        || mint.freeze_authority.is_some()
    {
        return Err(err(E_BAD_MINT));
    }

    let rent = Rent::get()?.minimum_balance(CURVE_SIZE);
    let seeds: &[&[u8]] = &[CURVE_SEED, mint_ai.key.as_ref(), &[bump]];
    if curve_ai.lamports() == 0 {
        invoke_signed(
            &system_instruction::create_account(payer_ai.key, curve_ai.key, rent, CURVE_SIZE as u64, program_id),
            &[payer_ai.clone(), curve_ai.clone(), system_ai.clone()],
            &[seeds],
        )?;
    } else {
        // PDA was pre-funded (griefing attempt or donation): allocate+assign path.
        if curve_ai.lamports() < rent {
            invoke(
                &system_instruction::transfer(payer_ai.key, curve_ai.key, rent - curve_ai.lamports()),
                &[payer_ai.clone(), curve_ai.clone(), system_ai.clone()],
            )?;
        }
        invoke_signed(
            &system_instruction::allocate(curve_ai.key, CURVE_SIZE as u64),
            &[curve_ai.clone(), system_ai.clone()],
            &[seeds],
        )?;
        invoke_signed(
            &system_instruction::assign(curve_ai.key, program_id),
            &[curve_ai.clone(), system_ai.clone()],
            &[seeds],
        )?;
    }

    let curve = Curve {
        mint: *mint_ai.key,
        buy_creator,
        sell_creator,
        price_fp: start_price_fp,
        double_vol,
        buy_fee_creator_bps,
        buy_fee_floor_bps,
        sell_fee_creator_bps,
        sell_fee_floor_bps,
        min_backing_bps,
        cum_vol: 0,
        bump,
    };
    store_curve(&curve, curve_ai)?;
    msg!(
        "notch: init mint={} buy_creator={} sell_creator={} price_fp={} backing>={}",
        mint_ai.key,
        buy_creator,
        sell_creator,
        start_price_fp,
        min_backing_bps
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Buy
// ---------------------------------------------------------------------------

fn buy(program_id: &Pubkey, accounts: &[AccountInfo], lamports: u64, min_out: u64) -> ProgramResult {
    let ai = &mut accounts.iter();
    let buyer_ai = next_account_info(ai)?;
    let curve_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let buyer_ta_ai = next_account_info(ai)?;
    let buy_creator_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;
    let system_ai = next_account_info(ai)?;

    if !buyer_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM || *system_ai.key != system_program::ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    if lamports == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }
    let mut curve = load_curve(program_id, curve_ai, mint_ai)?;
    if curve.buy_creator != *buy_creator_ai.key {
        return Err(err(E_BAD_CREATOR));
    }
    if token_account_mint(&buyer_ta_ai.data.borrow())? != curve.mint {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }

    let creator_fee = (lamports as u128 * curve.buy_fee_creator_bps as u128 / 10_000) as u64;
    let donation = (lamports as u128 * curve.buy_fee_floor_bps as u128 / 10_000) as u64;
    let net = lamports - creator_fee - donation;
    if net == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }

    // Pre-transfer snapshot; the floor donation counts as landed before minting.
    let v_pre = backing(curve_ai)? as u128;
    let s0 = parse_mint(&mint_ai.data.borrow())?.supply as u128;

    // Move the money: (net + donation) -> vault, creator fee -> creator.
    invoke(
        &system_instruction::transfer(buyer_ai.key, curve_ai.key, net + donation),
        &[buyer_ai.clone(), curve_ai.clone(), system_ai.clone()],
    )?;
    if creator_fee > 0 {
        invoke(
            &system_instruction::transfer(buyer_ai.key, buy_creator_ai.key, creator_fee),
            &[buyer_ai.clone(), buy_creator_ai.clone(), system_ai.clone()],
        )?;
    }

    // Path-independent mint at the governor price. The floor donation lands
    // first (lifts NAV for existing holders), then the net buy mints tokens at
    // price = NAV / backing. Integrated exactly this is a power law:
    //   S1 = S0 * (Vf / V0)^backing,   V0 = v_pre + donation,   Vf = V0 + net.
    // Composable => splitting a buy across txs can't change the result, so no
    // one can farm free tokens by chunking (the old exponential's bug). Minting
    // rounds DOWN (>> POW_F, integer div), so NAV only ever rounds up: the
    // floor is monotone nondecreasing by construction.
    let v0 = v_pre.checked_add(donation as u128).ok_or(err(E_OVERFLOW))?;
    let vf = v0.checked_add(net as u128).ok_or(err(E_OVERFLOW))?;
    let out: u128 = if s0 == 0 {
        // Genesis: no NAV yet. Mint at the configured start price.
        (net as u128)
            .checked_mul(E18)
            .ok_or(err(E_OVERFLOW))?
            / curve.price_fp
    } else {
        // S1 = S0 * (Vf/V0)^backing, then out = S1 - S0. None => buy too large
        // relative to the pool; split it (result is now split-invariant).
        let factor = pow_ratio_q(vf, v0, curve.min_backing_bps).ok_or(err(E_BUY_TOO_LARGE))?;
        let s1 = s0.checked_mul(factor).ok_or(err(E_BUY_TOO_LARGE))? >> POW_F;
        s1.checked_sub(s0).ok_or(err(E_OVERFLOW))?
    };
    let out64 = u64::try_from(out).map_err(|_| err(E_OVERFLOW))?;
    if out64 == 0 || out64 < min_out {
        return Err(err(E_SLIPPAGE));
    }

    // Reported price = NAV / backing after the buy (never decreases).
    let s_new = s0.checked_add(out).ok_or(err(E_OVERFLOW))?;
    let nav_new = vf.checked_mul(E18).ok_or(err(E_OVERFLOW))? / s_new;
    let price_new = nav_new
        .checked_mul(10_000)
        .map(|x| x / curve.min_backing_bps as u128)
        .unwrap_or(u128::MAX);
    let price_before = curve.price_fp;
    let p = core::cmp::max(price_new, curve.price_fp);
    if p > MAX_PRICE_FP {
        return Err(err(E_CURVE_SATURATED));
    }
    curve.price_fp = p;
    curve.cum_vol = curve.cum_vol.saturating_add(net as u128);
    store_curve(&curve, curve_ai)?;

    invoke_signed(
        &spl_mint_to(mint_ai.key, buyer_ta_ai.key, curve_ai.key, out64),
        &[mint_ai.clone(), buyer_ta_ai.clone(), curve_ai.clone(), token_ai.clone()],
        &[&[CURVE_SEED, curve.mint.as_ref(), &[curve.bump]]],
    )?;

    msg!("notch: buy {} lamports -> {} units, price_fp {} -> {}", lamports, out64, price_before, p);
    Ok(())
}

// ---------------------------------------------------------------------------
// Sell
// ---------------------------------------------------------------------------

fn sell(program_id: &Pubkey, accounts: &[AccountInfo], units: u64, min_out: u64) -> ProgramResult {
    let ai = &mut accounts.iter();
    let seller_ai = next_account_info(ai)?;
    let curve_ai = next_account_info(ai)?;
    let mint_ai = next_account_info(ai)?;
    let seller_ta_ai = next_account_info(ai)?;
    let sell_creator_ai = next_account_info(ai)?;
    let token_ai = next_account_info(ai)?;

    if !seller_ai.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *token_ai.key != TOKEN_PROGRAM {
        return Err(ProgramError::IncorrectProgramId);
    }
    if units == 0 {
        return Err(err(E_ZERO_AMOUNT));
    }
    let curve = load_curve(program_id, curve_ai, mint_ai)?;
    if curve.sell_creator != *sell_creator_ai.key {
        return Err(err(E_BAD_CREATOR));
    }
    if token_account_mint(&seller_ta_ai.data.borrow())? != curve.mint {
        return Err(err(E_BAD_TOKEN_ACCOUNT));
    }

    let supply = parse_mint(&mint_ai.data.borrow())?.supply;
    if supply == 0 || units > supply {
        return Err(err(E_ZERO_AMOUNT));
    }
    let v = backing(curve_ai)? as u128;

    // gross = units * NAV. Floor share stays in the vault (raises NAV);
    // creator share goes to the creator; the rest to the seller.
    let gross = (units as u128).checked_mul(v).ok_or(err(E_OVERFLOW))? / supply as u128;
    let creator_fee = gross * curve.sell_fee_creator_bps as u128 / 10_000;
    // On a FULL exit (units == supply) the floor share would have no remaining
    // holders to benefit and would strand in the vault as ownerless backing.
    // With supply == 0 the buy() NAV floor is undefined (0/0) and skipped, so a
    // later reviver could mint ~100% of a fresh supply cheaply and redeem that
    // stranded value. We therefore pay the floor share out with the final
    // redemption, keeping the invariant: supply == 0  =>  backing == 0.
    let floor_keep = if units == supply {
        0
    } else {
        gross * curve.sell_fee_floor_bps as u128 / 10_000
    };
    let to_seller = u64::try_from(gross - creator_fee - floor_keep).map_err(|_| err(E_OVERFLOW))?;
    let creator_fee = u64::try_from(creator_fee).map_err(|_| err(E_OVERFLOW))?;
    let vault_out = to_seller + creator_fee;

    if to_seller < min_out {
        return Err(err(E_SLIPPAGE));
    }
    if vault_out as u128 > v {
        return Err(err(E_INSUFFICIENT_VAULT));
    }

    // Burn first (seller signs the burn), then pay out via direct lamport moves
    // (curve PDA is program-owned so the program may debit it).
    invoke(
        &spl_burn(seller_ta_ai.key, mint_ai.key, seller_ai.key, units),
        &[seller_ta_ai.clone(), mint_ai.clone(), seller_ai.clone(), token_ai.clone()],
    )?;

    **curve_ai.try_borrow_mut_lamports()? -= vault_out;
    **seller_ai.try_borrow_mut_lamports()? += to_seller;
    if creator_fee > 0 {
        **sell_creator_ai.try_borrow_mut_lamports()? += creator_fee;
    }
    if curve_ai.lamports() < rent_floor()? {
        return Err(err(E_INSUFFICIENT_VAULT));
    }

    msg!("notch: sell {} units -> {} lamports (floor kept {})", units, to_seller, floor_keep as u64);
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for the path-independent mint math (native `cargo test`).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod mint_tests {
    use super::*;

    /// Pure model of buy()'s mint, mirroring the on-chain math exactly.
    struct Sim {
        v: u128,          // vault (lamports)
        s: u128,          // supply (base units)
        price_fp: u128,   // start price
        backing_bps: u16,
        buy_creator_bps: u16,
        buy_floor_bps: u16,
    }
    impl Sim {
        fn new(backing_bps: u16, buy_floor_bps: u16) -> Self {
            Sim { v: 0, s: 0, price_fp: 10_000_000_000_000, backing_bps, buy_creator_bps: 100, buy_floor_bps }
        }
        fn buy(&mut self, lamports: u128) -> u128 {
            let creator = lamports * self.buy_creator_bps as u128 / 10_000;
            let donation = lamports * self.buy_floor_bps as u128 / 10_000;
            let net = lamports - creator - donation;
            let v0 = self.v + donation;
            let vf = v0 + net;
            let out = if self.s == 0 {
                net * E18 / self.price_fp
            } else {
                let factor = pow_ratio_q(vf, v0, self.backing_bps).unwrap();
                (self.s * factor >> POW_F) - self.s
            };
            self.v = vf;
            self.s += out;
            out
        }
        fn floor_fp(&self) -> u128 { if self.s > 0 { self.v * E18 / self.s } else { 0 } }
        // net lamports a holder of `units` gets on sell (6% = 1% creator + 5% floor)
        fn sell_net(&self, units: u128) -> u128 {
            let gross = units * self.v / self.s;
            gross - gross * 100 / 10_000 - gross * 500 / 10_000
        }
    }

    #[test]
    fn pow_matches_reference() {
        // (21)^0.935 ~= 17.22959, (60)^0.825 ~= 29.30723  (verified vs float 4e-14)
        let a = pow_ratio_q(21_000_000_000, 1_000_000_000, 9350).unwrap() as f64 / POW_ONE as f64;
        let b = pow_ratio_q(60_000_000_000, 1_000_000_000, 8250).unwrap() as f64 / POW_ONE as f64;
        assert!((a - 17.229_593_87).abs() < 1e-6, "got {a}");
        assert!((b - 29.307_230_60).abs() < 1e-6, "got {b}");
    }

    #[test]
    fn mint_is_path_independent() {
        // No buy-floor donation => the power law is EXACTLY split-invariant.
        // Seed 1 SOL, then 100 SOL of follow-on volume split 4 ways; supply must match.
        let sol = 1_000_000_000u128;
        let run = |parts: u128| -> u128 {
            let mut sim = Sim::new(8500, 0);
            sim.buy(sol);
            let chunk = 100 * sol / parts;
            for _ in 0..parts { sim.buy(chunk); }
            sim.s
        };
        let base = run(1);
        for parts in [20u128, 200, 2000] {
            let got = run(parts);
            let rel = (got as i128 - base as i128).unsigned_abs() as f64 / base as f64;
            assert!(rel < 1e-6, "split {parts}: supply {got} vs {base} (rel {rel:.2e})");
        }
    }

    #[test]
    fn floor_never_falls() {
        // With the real 2% donation + governor, NAV must be monotone nondecreasing.
        let sol = 1_000_000_000u128;
        let mut sim = Sim::new(8250, 200);
        sim.buy(sol);
        let mut last = sim.floor_fp();
        for i in 0..200u128 {
            sim.buy(sol + (i % 7) * sol / 3); // varied buy sizes
            let f = sim.floor_fp();
            assert!(f >= last, "floor fell at step {i}: {f} < {last}");
            last = f;
        }
    }

    #[test]
    fn max_loss_within_25pct_at_floor_backing() {
        // 82.5% backing + 3%/6% fees => worst instant round trip must be <= 25%.
        let sol = 1_000_000_000u128;
        let mut sim = Sim::new(8250, 200);
        sim.buy(100 * sol); // establish a pool
        let spend = sol / 100; // small buy
        let got = sim.buy(spend);
        let back = sim.sell_net(got) as f64 / spend as f64;
        let loss = (1.0 - back) * 100.0;
        assert!(loss <= 25.0, "max loss {loss:.2}% exceeds 25%");
        assert!(loss > 20.0, "sanity: expected ~22-25%, got {loss:.2}%");
    }
}
