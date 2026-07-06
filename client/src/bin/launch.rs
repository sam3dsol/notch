//! NOTCH atomic launcher.
//!
//! Launches a new NOTCH token in a SINGLE transaction:
//!   [ create mint account, InitializeMint2 (authority = curve PDA, no freeze),
//!     NOTCH Initialize ]
//! Because mint-creation, authority assignment, and Initialize all land in one
//! atomic tx, the mint never exists on-chain before Initialize commits, so the
//! permissionless-Initialize front-run (audit: creator-fee-role hijack) has zero
//! window. Signed by the payer/creator + the fresh mint keypair.
//!
//! Env:
//!   RPC        JSON-RPC url (default http://127.0.0.1:8899)
//!   PROGRAM    NOTCH program id
//!   PAYER      creator/deployer keypair path (the vanity wallet); pays + becomes creator
//!   MINT       optional mint keypair path (e.g. a vanity mint); else a fresh random mint
//!   DRY        if set, prints the plan and the derived curve PDA, sends nothing
//! Launch params (reference config defaults):
//!   START_PRICE_FP (1e13) DOUBLE_VOL_SOL (25) BUY_FEE_CREATOR_BPS (0)
//!   BUY_FEE_FLOOR_BPS (300) SELL_FEE_CREATOR_BPS (100) SELL_FEE_FLOOR_BPS (500)
//!   MIN_BACKING_BPS (9350)

use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;
use notch_client::{curve, curve::LaunchCfg, rpc::Rpc};

const SOL: u64 = 1_000_000_000;

fn load_kp(path: &str) -> Keypair {
    let bytes: Vec<u8> = serde_json::from_str(&std::fs::read_to_string(path).expect("read keypair"))
        .expect("parse keypair json");
    Keypair::from_bytes(&bytes).expect("keypair bytes")
}

fn env_u128(k: &str, d: u128) -> u128 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
fn env_u64(k: &str, d: u64) -> u64 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }
fn env_u16(k: &str, d: u16) -> u16 { std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d) }

#[tokio::main]
async fn main() {
    let url = std::env::var("RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".into());
    let program = Pubkey::from_str(&std::env::var("PROGRAM").expect("PROGRAM env required")).unwrap();
    let payer = load_kp(&std::env::var("PAYER").expect("PAYER env required"));
    let mint_kp = match std::env::var("MINT") {
        Ok(p) => load_kp(&p),
        Err(_) => Keypair::new(),
    };
    let mint = mint_kp.pubkey();
    let rpc = Rpc::new(&url);

    // Fee recipients. Default buy-creator = platform fee wallet; default
    // sell-creator = the deployer/payer. Both overridable per launch.
    let buy_creator = Pubkey::from_str(
        &std::env::var("BUY_CREATOR").unwrap_or_else(|_| "Bj6kYwqS7Le5SkwYepMTDUpDZNgmYTfXW9FPAvRq7vsY".into()),
    ).expect("BUY_CREATOR pubkey");
    let sell_creator = match std::env::var("SELL_CREATOR") {
        Ok(s) => Pubkey::from_str(&s).expect("SELL_CREATOR pubkey"),
        Err(_) => payer.pubkey(),
    };

    let cfg = LaunchCfg {
        start_price_fp: env_u128("START_PRICE_FP", 1_000_000_000_000_000_000),
        double_vol: env_u64("DOUBLE_VOL_SOL", 25) * SOL,
        buy_fee_creator_bps: env_u16("BUY_FEE_CREATOR_BPS", 100),
        buy_fee_floor_bps: env_u16("BUY_FEE_FLOOR_BPS", 200),
        sell_fee_creator_bps: env_u16("SELL_FEE_CREATOR_BPS", 100),
        sell_fee_floor_bps: env_u16("SELL_FEE_FLOOR_BPS", 500),
        min_backing_bps: env_u16("MIN_BACKING_BPS", 9350),
    };

    let (curve_pda, _) = curve::curve_pda(&program, &mint);
    let mint_rent = rpc.min_balance(curve::MINT_SIZE).await;

    println!("NOTCH atomic launch");
    println!("  program     : {}", program);
    println!("  payer       : {}", payer.pubkey());
    println!("  buy_creator : {}  (gets 1% of buys)", buy_creator);
    println!("  sell_creator: {}  (gets 1% of sells)", sell_creator);
    println!("  mint        : {}", mint);
    println!("  curvePDA    : {}", curve_pda);
    println!(
        "  config      : start={} 2x_per={} SOL buy=3%(1%cr+2%fl) sell=6%(1%cr+5%fl) backing>={}bps (main token ~15%; platform floor 82.5%/25%)",
        cfg.start_price_fp, cfg.double_vol / SOL, cfg.min_backing_bps
    );

    // ONE atomic transaction: create+init mint (authority = curve PDA), then Initialize.
    let mut ixs = curve::create_mint_ixs(&program, &payer.pubkey(), &mint, mint_rent);
    ixs.push(curve::initialize(&program, &payer.pubkey(), &buy_creator, &sell_creator, &mint, &cfg));
    println!("  tx      : {} instructions (create_account, InitializeMint2, Initialize) in ONE tx", ixs.len());

    if std::env::var("DRY").is_ok() {
        println!("DRY set: not sending. Front-run-safe because all {} ixs are atomic.", ixs.len());
        return;
    }

    let bh = rpc.blockhash().await;
    let msg = solana_sdk::message::Message::new(&ixs, Some(&payer.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    tx.sign(&[&payer, &mint_kp], bh);
    match rpc.send(&tx).await {
        Ok(sig) => {
            // verify the curve exists and reads back correctly
            match rpc.account_data(&curve_pda).await.and_then(|d| curve::parse_curve(&d)) {
                Some(c) if c.mint == mint && c.buy_creator == buy_creator && c.sell_creator == sell_creator => {
                    println!("LAUNCHED  sig={}", sig);
                    println!("  curve OK: buy_creator={} sell_creator={} price_fp={} backing>={}bps", c.buy_creator, c.sell_creator, c.price_fp, c.min_backing_bps);
                    println!("  token is live. Buy: notch Buy ix against mint {}", mint);
                }
                _ => println!("SENT but curve readback failed (sig={})", sig),
            }
        }
        Err(e) => {
            println!("LAUNCH FAILED: {}", e);
            std::process::exit(1);
        }
    }
}
