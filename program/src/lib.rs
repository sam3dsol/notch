//! NOTCH — ratchet-curve token vault. The quoted price can only go UP.
//!
//! v2 tokenomics (per-launch config, immutable after Initialize):
//!   * BUY fee (e.g. 3%): `buy_fee_creator_bps` to the creator wallet,
//!     `buy_fee_floor_bps` donated straight into the vault (lifts NAV for
//!     everyone, including the buyer). The remainder mints tokens at the
//!     curve price, which advances 2^(net/double_vol) — the speed dial.
//!   * SELL fee (e.g. 6%): seller redeems at NAV; `sell_fee_creator_bps` of
//!     gross to the creator, `sell_fee_floor_bps` STAYS in the vault (sells
//!     raise the floor). Sells never move the price.
//!   * GOVERNOR: `min_backing_bps` (e.g. 9350 = 93.5%) caps the price at
//!     NAV / ratio, chunk-by-chunk with running NAV. This bounds the
//!     price-vs-floor gap forever, so the worst possible instant round trip
//!     is fees + gap (with 3%/6% fees and 93.5% backing: ~-14.8% all-in).
//!     PLATFORM RULE: min_backing_bps must be 5000..=9900 (50% to 99%), so
//!     the governor is always on and no launch can drop below 50% backing.
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
/// ln(2) * FP.
const LN2_FP: u128 = 693_147_181;
/// Price advance is compounded in chunks of double_vol/32 for accuracy
/// (~0.7% conservative per doubling vs exact 2^x).
const CHUNK_DIV: u128 = 32;
/// A single Buy may advance the curve at most 8 doublings (bounds the chunk
/// loop at 256 iterations). Bigger buys must be split across transactions.
const MAX_BUY_DOUBLINGS: u128 = 8;

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
    let d = curve.double_vol as u128;
    if net as u128 > MAX_BUY_DOUBLINGS * d {
        // Bounds the price-advance loop; split absurdly large buys.
        return Err(err(E_BUY_TOO_LARGE));
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

    // Advance the curve in chunks with RUNNING NAV: each chunk mints at its
    // trapezoid-average price, never below NAV, and the governor caps the
    // price at NAV * 10000 / min_backing_bps so the gap is bounded forever.
    let chunk = core::cmp::max(d / CHUNK_DIV, 1);
    let mut remaining = net as u128;
    let mut v_run = v_pre.checked_add(donation as u128).ok_or(err(E_OVERFLOW))?;
    let mut p = curve.price_fp;
    let mut out: u128 = 0;
    while remaining > 0 {
        let c = core::cmp::min(remaining, chunk);
        let p0 = p;
        // dp = p * ln2 * c / d  (compounded per chunk => ~2^(net/d) overall)
        let step_fp = LN2_FP * c / d;
        p = p.checked_add(p.checked_mul(step_fp).ok_or(err(E_OVERFLOW))? / FP).ok_or(err(E_OVERFLOW))?;
        let s_run = s0 + out;
        let pe;
        if s_run > 0 {
            let nav = v_run.checked_mul(E18).ok_or(err(E_OVERFLOW))? / s_run;
            if curve.min_backing_bps > 0 {
                // cap = nav / backing_ratio; on mul-overflow the backing is so
                // rich that no clamp is needed.
                let cap = nav
                    .checked_mul(10_000)
                    .map(|x| x / curve.min_backing_bps as u128)
                    .unwrap_or(u128::MAX);
                if p > cap {
                    p = cap;
                }
                if p < p0 {
                    p = p0; // price never goes down
                }
            }
            let mut pe_ = (p0 + p) / 2;
            if pe_ <= nav {
                pe_ = nav + nav / 10_000 + 1;
            }
            pe = pe_;
        } else {
            pe = (p0 + p) / 2;
        }
        if p > MAX_PRICE_FP {
            return Err(err(E_CURVE_SATURATED));
        }
        // units = c lamports * 1e18 / price_fp
        out = out
            .checked_add(c.checked_mul(E18).ok_or(err(E_OVERFLOW))? / pe)
            .ok_or(err(E_OVERFLOW))?;
        v_run += c;
        remaining -= c;
    }
    let out64 = u64::try_from(out).map_err(|_| err(E_OVERFLOW))?;
    if out64 == 0 || out64 < min_out {
        return Err(err(E_SLIPPAGE));
    }

    let price_before = curve.price_fp;
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
