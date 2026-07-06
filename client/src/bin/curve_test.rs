//! UPONLY v2 curve test suite, against a local validator.
//! Reference launch config (user spec 2026-07-05):
//!   buy fee 3% (all to floor), sell fee 6% (1% creator + 5% floor),
//!   governor min backing 93.5% => worst-case instant round trip ~ -14.8%
//!   (<= 15% all-in), start 0.00001 SOL/token, schedule 2x per 25 SOL.
//! Proves on-chain: price monotone, NAV monotone on buys AND sells, backing
//! ratio never below the governor, exact integer math, dump-pump behavior,
//! and the <=15% all-in round-trip bound.

use rand::{rngs::StdRng, Rng, SeedableRng};
use solana_sdk::{
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use uponly_client::{curve, curve::LaunchCfg, rpc::Rpc};

const SOL: u64 = 1_000_000_000;
const FP: u128 = 1_000_000_000;
const LN2_FP: u128 = 693_147_181;
const CHUNK_DIV: u128 = 32;
const E18: u128 = 1_000_000_000_000_000_000;

const CFG: LaunchCfg = LaunchCfg {
    start_price_fp: 1_000_000_000 * FP, // 1 SOL / token
    double_vol: 25 * SOL,        // schedule: 2x per 25 SOL (governor tempers it)
    buy_fee_creator_bps: 100,    // buys: 1% to buy_creator (platform wallet)
    buy_fee_floor_bps: 200,      // buys: 2% into the floor
    sell_fee_creator_bps: 100,   // sells: 1% to sell_creator (deployer)
    sell_fee_floor_bps: 500,     // sells: 5% stays in the floor
    min_backing_bps: 9_350,      // main token: backing 93.5% => ~15% max loss (platform FLOOR is 82.5%/25% for other launches)
};

fn load_kp(path: &str) -> Keypair {
    let bytes: Vec<u8> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    Keypair::from_bytes(&bytes).unwrap()
}

fn cu_limit(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: Pubkey::from_str("ComputeBudget111111111111111111111111111111").unwrap(),
        accounts: vec![],
        data,
    }
}

/// Exact mirror of the program's buy math (running NAV + governor).
/// Returns (new_price_fp, out_units).
fn expected_buy(price_fp: u128, lamports: u64, v_pre: u128, s0: u128) -> (u128, u128) {
    let d = CFG.double_vol as u128;
    let creator_fee = lamports as u128 * CFG.buy_fee_creator_bps as u128 / 10_000;
    let donation = lamports as u128 * CFG.buy_fee_floor_bps as u128 / 10_000;
    let net = lamports as u128 - creator_fee - donation;
    let chunk = std::cmp::max(d / CHUNK_DIV, 1);
    let mut remaining = net;
    let mut v_run = v_pre + donation;
    let mut p = price_fp;
    let mut out: u128 = 0;
    while remaining > 0 {
        let c = std::cmp::min(remaining, chunk);
        let p0 = p;
        p += p * (LN2_FP * c / d) / FP;
        let s_run = s0 + out;
        let pe;
        if s_run > 0 {
            let nav = v_run * E18 / s_run;
            if CFG.min_backing_bps > 0 {
                let cap = nav
                    .checked_mul(10_000)
                    .map(|x| x / CFG.min_backing_bps as u128)
                    .unwrap_or(u128::MAX);
                if p > cap {
                    p = cap;
                }
                if p < p0 {
                    p = p0;
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
        out += c * E18 / pe;
        v_run += c;
        remaining -= c;
    }
    (p, out)
}

/// Exact mirror of the program's sell math: (to_seller, vault_out, creator_fee).
fn expected_sell(units: u64, v: u128, supply: u64) -> (u64, u64, u64) {
    let gross = units as u128 * v / supply as u128;
    let creator_fee = gross * CFG.sell_fee_creator_bps as u128 / 10_000;
    let floor_keep = gross * CFG.sell_fee_floor_bps as u128 / 10_000;
    let to_seller = (gross - creator_fee - floor_keep) as u64;
    (to_seller, to_seller + creator_fee as u64, creator_fee as u64)
}

struct Ctx {
    rpc: Rpc,
    program: Pubkey,
    mint: Pubkey,
    curve_rent: u64,
}

impl Ctx {
    async fn curve(&self) -> Option<curve::Curve> {
        let (pda, _) = curve::curve_pda(&self.program, &self.mint);
        curve::parse_curve(&self.rpc.account_data(&pda).await?)
    }
    async fn backing(&self) -> u64 {
        let (pda, _) = curve::curve_pda(&self.program, &self.mint);
        self.rpc.balance(&pda).await.saturating_sub(self.curve_rent)
    }
    async fn supply(&self) -> u64 {
        curve::mint_supply(&self.rpc.account_data(&self.mint).await.unwrap_or_default())
    }
    async fn bag(&self, owner: &Pubkey) -> u64 {
        match self.rpc.account_data(&curve::ata(owner, &self.mint)).await {
            Some(d) => curve::token_amount(&d),
            None => 0,
        }
    }
    /// backing ratio holds: price * min_backing <= nav * 10000 (+rounding slack)
    async fn ratio_ok(&self) -> bool {
        let s = self.supply().await;
        if s == 0 {
            return true;
        }
        let nav = self.backing().await as u128 * E18 / s as u128;
        let p = self.curve().await.unwrap().price_fp;
        p * CFG.min_backing_bps as u128 <= nav * 10_000 + nav / 100
    }
}

async fn send(rpc: &Rpc, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) -> Result<String, String> {
    let bh = rpc.blockhash().await;
    let msg = solana_sdk::message::Message::new(ixs, Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    tx.sign(signers, bh);
    rpc.send(&tx).await
}

#[tokio::main]
async fn main() {
    let url = std::env::var("RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let program = Pubkey::from_str(&std::env::var("PROGRAM").expect("PROGRAM env required")).unwrap();
    let rpc = Rpc::new(&url);

    let payer = load_kp(&std::env::var("PAYER").expect("PAYER env required"));
    let creator = Keypair::new();
    let buyer = Keypair::new();
    let whale = Keypair::new();
    let mint_kp = Keypair::new();
    let mint = mint_kp.pubkey();

    let mut pass = 0u32;
    let mut fail = 0u32;
    macro_rules! check {
        ($name:expr, $cond:expr) => {
            if $cond { pass += 1; println!("PASS  {}", $name); } else { fail += 1; println!("FAIL  {}", $name); }
        };
    }

    // --- setup -------------------------------------------------------------
    if rpc.balance(&payer.pubkey()).await < 700 * SOL {
        rpc.airdrop(&payer.pubkey(), 1_000 * SOL).await.expect("airdrop payer");
    }
    for (kp, amt) in [(&creator, 5 * SOL), (&buyer, 50 * SOL), (&whale, 400 * SOL)] {
        send(&rpc, &[solana_sdk::system_instruction::transfer(&payer.pubkey(), &kp.pubkey(), amt)], &payer, &[&payer])
            .await
            .expect("fund");
    }
    let mint_rent = rpc.min_balance(curve::MINT_SIZE).await;
    let curve_rent = rpc.min_balance(curve::CURVE_SIZE).await;
    let ctx = Ctx { rpc, program, mint, curve_rent };
    let rpc = &ctx.rpc;
    send(rpc, &curve::create_mint_ixs(&program, &payer.pubkey(), &mint, mint_rent), &payer, &[&payer, &mint_kp])
        .await
        .expect("create mint");

    // --- 1) Initialize (main mint: creator plays payer + both fee roles) ----
    match send(rpc, &[curve::initialize(&program, &creator.pubkey(), &creator.pubkey(), &creator.pubkey(), &mint, &CFG)], &creator, &[&creator]).await {
        Ok(_) => {
            let c = ctx.curve().await;
            check!("Initialize creates curve PDA", c.is_some());
            if let Some(c) = c {
                check!("curve params correct",
                    c.mint == mint && c.buy_creator == creator.pubkey() && c.sell_creator == creator.pubkey()
                        && c.price_fp == CFG.start_price_fp && c.double_vol == CFG.double_vol
                        && c.buy_fee_creator_bps == 100 && c.buy_fee_floor_bps == 200
                        && c.sell_fee_creator_bps == 100 && c.sell_fee_floor_bps == 500
                        && c.min_backing_bps == 9_350 && c.cum_vol == 0);
            }
        }
        Err(e) => { check!("Initialize", false); println!("      err: {}", e); }
    }

    // --- 2) Re-initialize rejected ------------------------------------------
    let reinit = send(rpc, &[curve::initialize(&program, &creator.pubkey(), &creator.pubkey(), &creator.pubkey(), &mint, &CFG)], &creator, &[&creator]).await;
    check!("re-Initialize rejected", reinit.is_err());

    // --- 2b) PLATFORM RULE: backing must be 82.5%+, governor is mandatory ----
    for (bps, ok, name) in [
        (4000u16, false, "PLATFORM: backing 40% rejected (< 82.5% floor)"),
        (0u16, false, "PLATFORM: ungoverned (0) rejected"),
        (8000u16, false, "PLATFORM: backing 80% rejected (< 82.5% floor)"),
        (8500u16, true, "PLATFORM: backing 85% accepted"),
    ] {
        let mk = Keypair::new();
        let m = mk.pubkey();
        send(rpc, &curve::create_mint_ixs(&program, &payer.pubkey(), &m, mint_rent), &payer, &[&payer, &mk]).await.expect("mint");
        let mut cfg = CFG;
        cfg.min_backing_bps = bps;
        let r = send(rpc, &[curve::initialize(&program, &creator.pubkey(), &creator.pubkey(), &creator.pubkey(), &m, &cfg)], &creator, &[&creator]).await;
        check!(name, r.is_ok() == ok);
    }

    // --- 2c) SPLIT FEES: buy 1% -> buy_creator, sell 1% -> sell_creator ------
    {
        let bc = Keypair::new();
        let sc = Keypair::new();
        let sniper = &buyer; // a buyer for this split-test mint
        let mk = Keypair::new();
        let m2 = mk.pubkey();
        send(rpc, &curve::create_mint_ixs(&program, &payer.pubkey(), &m2, mint_rent), &payer, &[&payer, &mk]).await.expect("split mint");
        send(rpc, &[curve::initialize(&program, &creator.pubkey(), &bc.pubkey(), &sc.pubkey(), &m2, &CFG)], &creator, &[&creator]).await.expect("split init");
        send(rpc, &[curve::create_ata_ix(&sniper.pubkey(), &sniper.pubkey(), &m2)], sniper, &[sniper]).await.expect("split ata");
        // buy: 1% must land on buy_creator (bc), NOT sell_creator (sc)
        send(rpc, &[curve::buy(&program, &sniper.pubkey(), &m2, &bc.pubkey(), SOL, 0)], sniper, &[sniper]).await.expect("split buy");
        check!("SPLIT: buy 1% went to buy_creator", ctx.rpc.balance(&bc.pubkey()).await == SOL / 100);
        check!("SPLIT: sell_creator got nothing from a buy", ctx.rpc.balance(&sc.pubkey()).await == 0);
        // buy with the WRONG creator account (sc) must be rejected
        let bag2 = match ctx.rpc.account_data(&curve::ata(&sniper.pubkey(), &m2)).await { Some(d) => curve::token_amount(&d), None => 0 };
        let wrong = send(rpc, &[curve::buy(&program, &sniper.pubkey(), &m2, &sc.pubkey(), SOL, 0)], sniper, &[sniper]).await;
        check!("SPLIT: buy with wrong buy_creator rejected", wrong.is_err());
        // sell: 1% must land on sell_creator (sc)
        let sc_before = ctx.rpc.balance(&sc.pubkey()).await;
        send(rpc, &[curve::sell(&program, &sniper.pubkey(), &m2, &sc.pubkey(), bag2 / 2, 0)], sniper, &[sniper]).await.expect("split sell");
        check!("SPLIT: sell 1% went to sell_creator", ctx.rpc.balance(&sc.pubkey()).await > sc_before);
    }

    // --- 3) First buy: exact out, 3% to floor, creator gets NOTHING on buys ---
    send(rpc, &[curve::create_ata_ix(&buyer.pubkey(), &buyer.pubkey(), &mint)], &buyer, &[&buyer]).await.expect("buyer ata");
    let creator_bal0 = rpc.balance(&creator.pubkey()).await;
    let (exp_p1, exp_out1) = expected_buy(CFG.start_price_fp, SOL, 0, 0);
    match send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &creator.pubkey(), SOL, 0)], &buyer, &[&buyer]).await {
        Ok(_) => {
            check!("buy#1 exact token out", ctx.bag(&buyer.pubkey()).await as u128 == exp_out1);
            check!("buy#1 vault got net + 2% donation (0.99 SOL)", ctx.backing().await == SOL - SOL / 100);
            check!("buy#1 buy_creator got exact 1%", rpc.balance(&creator.pubkey()).await == creator_bal0 + SOL / 100);
            let c = ctx.curve().await.unwrap();
            check!("buy#1 exact price advance", c.price_fp == exp_p1 && c.price_fp > CFG.start_price_fp);
            check!("buy#1 backing ratio >= 93.5%", ctx.ratio_ok().await);
        }
        Err(e) => { check!("buy#1", false); println!("      err: {}", e); }
    }

    // --- 4) Second buy: exact + fewer tokens ---------------------------------
    let p1 = ctx.curve().await.unwrap().price_fp;
    let bag1 = ctx.bag(&buyer.pubkey()).await;
    let (exp_p2, exp_out2) = expected_buy(p1, SOL, ctx.backing().await as u128, ctx.supply().await as u128);
    match send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &creator.pubkey(), SOL, 0)], &buyer, &[&buyer]).await {
        Ok(_) => {
            let got = ctx.bag(&buyer.pubkey()).await - bag1;
            check!("buy#2 exact token out", got as u128 == exp_out2);
            check!("buy#2 fewer tokens than buy#1 (price ratcheted)", got < bag1);
            check!("buy#2 price monotone up", ctx.curve().await.unwrap().price_fp == exp_p2 && exp_p2 > p1);
        }
        Err(e) => { check!("buy#2", false); println!("      err: {}", e); }
    }

    // --- 5) Sell half: exact 94% payout, 1% to creator, NAV up, price frozen --
    let p_before_sell = ctx.curve().await.unwrap().price_fp;
    let v = ctx.backing().await as u128;
    let s = ctx.supply().await;
    let sell_units = ctx.bag(&buyer.pubkey()).await / 2;
    let (exp_recv, exp_vault_out, exp_cf) = expected_sell(sell_units, v, s);
    let buyer_bal_before = rpc.balance(&buyer.pubkey()).await;
    let creator_before = rpc.balance(&creator.pubkey()).await;
    match send(rpc, &[curve::sell(&program, &buyer.pubkey(), &mint, &creator.pubkey(), sell_units, 0)], &buyer, &[&buyer]).await {
        Ok(_) => {
            let delta = rpc.balance(&buyer.pubkey()).await + 5_000 - buyer_bal_before;
            check!("sell exact payout (94% of gross)", delta == exp_recv);
            check!("sell creator got exact 1% of gross", rpc.balance(&creator.pubkey()).await == creator_before + exp_cf);
            let v2 = ctx.backing().await as u128;
            let s2 = ctx.supply().await;
            check!("sell burned units", s2 == s - sell_units);
            check!("sell vault out == seller + creator", v2 == v - exp_vault_out as u128);
            check!("sell RAISED NAV (5% floor share stays)", v2 * s as u128 > v * s2 as u128);
            check!("sell did NOT move curve price", ctx.curve().await.unwrap().price_fp == p_before_sell);
        }
        Err(e) => { check!("sell", false); println!("      err: {}", e); }
    }

    // --- 6) Guards -----------------------------------------------------------
    let too_high = send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &creator.pubkey(), SOL, u64::MAX)], &buyer, &[&buyer]).await;
    check!("buy min_out too high rejected", too_high.is_err());
    let sell_high = send(rpc, &[curve::sell(&program, &buyer.pubkey(), &mint, &creator.pubkey(), 1_000, u64::MAX)], &buyer, &[&buyer]).await;
    check!("sell min_out too high rejected", sell_high.is_err());
    let bag_now = ctx.bag(&buyer.pubkey()).await;
    let oversell = send(rpc, &[curve::sell(&program, &buyer.pubkey(), &mint, &creator.pubkey(), bag_now + 1, 0)], &buyer, &[&buyer]).await;
    check!("oversell rejected", oversell.is_err());
    let imposter = Keypair::new();
    let bad_buy = send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &imposter.pubkey(), SOL, 0)], &buyer, &[&buyer]).await;
    check!("buy with wrong creator account rejected", bad_buy.is_err());
    send(rpc, &[curve::create_ata_ix(&whale.pubkey(), &whale.pubkey(), &mint)], &whale, &[&whale]).await.expect("whale ata");
    let huge = send(rpc, &[cu_limit(600_000), curve::buy(&program, &whale.pubkey(), &mint, &creator.pubkey(), 210 * SOL, 0)], &whale, &[&whale]).await;
    check!("buy > 8 doublings rejected (split required)", huge.is_err());

    // --- 7) Whale buy 100 SOL: exact under governor ----------------------------
    let pw = ctx.curve().await.unwrap().price_fp;
    let (exp_pw, exp_outw) = expected_buy(pw, 100 * SOL, ctx.backing().await as u128, ctx.supply().await as u128);
    let wbag0 = ctx.bag(&whale.pubkey()).await;
    match send(rpc, &[cu_limit(600_000), curve::buy(&program, &whale.pubkey(), &mint, &creator.pubkey(), 100 * SOL, 0)], &whale, &[&whale]).await {
        Ok(_) => {
            check!("whale 100 SOL exact out under governor", (ctx.bag(&whale.pubkey()).await - wbag0) as u128 == exp_outw);
            let pa = ctx.curve().await.unwrap().price_fp;
            check!("whale exact governed price", pa == exp_pw);
            check!("whale: backing ratio >= 93.5% held", ctx.ratio_ok().await);
            println!("      100 SOL moved price {:.3}x (governed; ungoverned would be ~15x)", pa as f64 / pw as f64);
        }
        Err(e) => { check!("whale buy", false); println!("      err: {}", e); }
    }

    // --- 8) All-in round-trip bound: buy then immediately sell all ------------
    let fresh = Keypair::new();
    send(rpc, &[solana_sdk::system_instruction::transfer(&payer.pubkey(), &fresh.pubkey(), 3 * SOL)], &payer, &[&payer]).await.expect("fund fresh");
    send(rpc, &[curve::create_ata_ix(&fresh.pubkey(), &fresh.pubkey(), &mint)], &fresh, &[&fresh]).await.expect("fresh ata");
    send(rpc, &[curve::buy(&program, &fresh.pubkey(), &mint, &creator.pubkey(), SOL, 0)], &fresh, &[&fresh]).await.expect("fresh buy");
    let fbag = ctx.bag(&fresh.pubkey()).await;
    let (exp_r, _, _) = expected_sell(fbag, ctx.backing().await as u128, ctx.supply().await);
    send(rpc, &[curve::sell(&program, &fresh.pubkey(), &mint, &creator.pubkey(), fbag, 0)], &fresh, &[&fresh]).await.expect("fresh sell");
    let loss = 1.0 - exp_r as f64 / SOL as f64;
    check!(&format!("main token round trip loss {:.1}% <= 15% all-in", loss * 100.0), loss <= 0.15);

    // --- 9) Dump-pump: dumps raise NAV, price frozen, next buy prints higher ---
    let p_pre_dump = ctx.curve().await.unwrap().price_fp;
    let v0 = ctx.backing().await as u128;
    let s0 = ctx.supply().await as u128;
    let dump_units = ctx.bag(&whale.pubkey()).await / 2;
    send(rpc, &[curve::sell(&program, &whale.pubkey(), &mint, &creator.pubkey(), dump_units, 0)], &whale, &[&whale]).await.expect("dump");
    let v1 = ctx.backing().await as u128;
    let s1 = ctx.supply().await as u128;
    check!("big dump RAISED NAV", v1 * s0 > v0 * s1);
    check!("big dump did not move price", ctx.curve().await.unwrap().price_fp == p_pre_dump);
    send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &creator.pubkey(), SOL, 0)], &buyer, &[&buyer]).await.expect("post-dump buy");
    check!("next buy prints HIGHER than pre-dump price (dump built headroom)",
        ctx.curve().await.unwrap().price_fp > p_pre_dump);

    // --- 10) Randomized monotonicity + governor fuzz ---------------------------
    let mut rng = StdRng::seed_from_u64(42);
    let mut last_p = ctx.curve().await.unwrap().price_fp;
    let (mut last_v, mut last_s) = (ctx.backing().await as u128, ctx.supply().await as u128);
    let mut mono_ok = true;
    let mut ops = 0;
    for i in 0..20 {
        let do_buy = rng.gen_bool(0.5);
        let res = if do_buy {
            let amt = rng.gen_range(SOL / 10..=2 * SOL);
            send(rpc, &[curve::buy(&program, &whale.pubkey(), &mint, &creator.pubkey(), amt, 0)], &whale, &[&whale]).await
        } else {
            let bag = ctx.bag(&whale.pubkey()).await;
            if bag < 100 { continue; }
            let units = rng.gen_range(bag / 10..=bag / 2);
            send(rpc, &[curve::sell(&program, &whale.pubkey(), &mint, &creator.pubkey(), units, 0)], &whale, &[&whale]).await
        };
        if let Err(e) = res {
            println!("      fuzz op {} err: {}", i, e);
            mono_ok = false;
            break;
        }
        ops += 1;
        let p = ctx.curve().await.unwrap().price_fp;
        let (v, s) = (ctx.backing().await as u128, ctx.supply().await as u128);
        if p < last_p { println!("      PRICE DOWN at op {}", i); mono_ok = false; }
        if s > 0 && last_s > 0 && v * last_s < last_v * s { println!("      NAV DOWN at op {}", i); mono_ok = false; }
        if !ctx.ratio_ok().await { println!("      RATIO BROKEN at op {}", i); mono_ok = false; }
        last_p = p; last_v = v; last_s = s;
    }
    check!(&format!("fuzz {} ops: price+NAV monotone, ratio >= 93.5%", ops), mono_ok && ops >= 10);

    // --- 11) Full exit + restart (S=0 edge; audit fix: no stranded floor) ------
    // Exit everyone. The LAST seller triggers units==supply, so the floor share
    // is paid out instead of stranded: supply==0 must imply backing==0, closing
    // the NAV-floor-bypass path where a reviver could capture orphaned backing.
    for kp in [&buyer, &whale] {
        let bag = ctx.bag(&kp.pubkey()).await;
        if bag > 0 {
            send(rpc, &[curve::sell(&program, &kp.pubkey(), &mint, &creator.pubkey(), bag, 0)], kp, &[kp]).await.expect("exit");
        }
    }
    check!("everyone exited: supply == 0", ctx.supply().await == 0);
    let leftover = ctx.backing().await;
    check!("AUDIT FIX: full exit strands NO backing (supply==0 => backing==0)", leftover == 0);
    println!("      leftover backing after full exit: {} lamports (expect 0)", leftover);
    let p_final = ctx.curve().await.unwrap().price_fp;
    match send(rpc, &[curve::buy(&program, &buyer.pubkey(), &mint, &creator.pubkey(), SOL, 0)], &buyer, &[&buyer]).await {
        Ok(_) => {
            check!("restart buy after full exit works (S=0 edge)", ctx.bag(&buyer.pubkey()).await > 0);
            check!("price never reset", ctx.curve().await.unwrap().price_fp >= p_final);
            // The reviver must NOT be able to instantly round-trip for a profit
            // (i.e. no stranded value to capture): exit is still bounded by the cap.
            let rbag = ctx.bag(&buyer.pubkey()).await;
            let (exp_rv, _, _) = expected_sell(rbag, ctx.backing().await as u128, ctx.supply().await);
            check!("AUDIT FIX: revival buy cannot be dumped for profit",
                (exp_rv as f64 / SOL as f64) <= 1.0);
        }
        Err(e) => { check!("restart buy", false); println!("      err: {}", e); }
    }

    println!("\n==== {} passed, {} failed ====", pass, fail);
    if fail > 0 {
        std::process::exit(1);
    }
}
