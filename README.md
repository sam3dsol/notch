# UPONLY

**A Solana token standard where the chart price can never go down. Not as a promise, as a property of the code.**

UPONLY is a ratchet-curve token vault. Every token trades against its own on-chain vault instead of a DEX pool. Buys advance the price. Sells redeem against the vault floor and leave a fee behind that raises the floor for everyone still holding. The price is mathematically incapable of printing a red candle, and the worst possible outcome for any buyer is bounded on-chain at 15% all-in.

Proven with 34/34 on-chain tests, including a randomized fuzz that asserts price and floor never decreased once.

## The five properties

1. **Price only goes up.** Buys move it up on a curve. Sells do not touch it. There is no pool to dump into.
2. **Sells make the token stronger.** 5% of every exit stays in the vault, so every seller raises the floor under every remaining holder. In testing, a wave of sells pushed the floor above the last printed price, and the next buy printed a new all-time high.
3. **Hard loss cap, enforced on-chain.** A governor keeps the vault backing at 93.5% of the price minimum. Worst case for any buyer at any time, buying the top and dumping instantly: 14.75% loss, fees included. Verified live in the test suite at 14.7%.
4. **Non-custodial by construction.** The program has no admin instructions. No withdraw, no pause, no config change. Vault SOL can only leave through holder redemptions. The creator receives flow fees only and can never touch the vault.
5. **Always redeemable.** Every token can always be sold back to the vault at the floor price, in one transaction, with no counterparty and no liquidity dependence.

## How it works

```
BUY 1 SOL                                 SELL tokens
  3% -> vault (raises floor instantly)      redeemed at NAV (vault / supply)
 97% -> mints tokens at curve price         94% -> seller
        price ratchets up, capped            5% -> stays in vault (floor rises)
        at NAV / 0.935 by the governor       1% -> creator
```

NAV (the floor) is the vault balance divided by token supply. It rises on every buy (buyers mint above NAV) and on every sell (the 5% stays). The curve price rides at most 6.5% above it. Both numbers are monotone: they never go down, no matter what any holder does.

## Reference numbers (from the exact program math)

100 buyers of 1 SOL each, then 81 of them dump everything:

| event | price | floor (NAV) | note |
|---|---|---|---|
| launch | 0.00001000 | 0 | pool starts empty, no LP needed |
| after 100 buys | 0.00001560 | 0.00001459 | half the holders in profit at the floor |
| 81 holders dump (80 SOL out) | 0.00001560 | 0.00001594 | price unchanged through the dump (sells never move it), floor ends ABOVE price |
| next 1 SOL buy | 0.00001602 | | new all-time high, after an 80% exodus |

Seller outcomes in that dump wave: seller 1 exits at +30.6%, seller 50 at -5.5%, seller 81 at -5.2%. Nobody comes close to the -14.75% bound, because the floor rises during the wave itself.

## The closed-form math

Once the governor engages (a few SOL after launch), everything is one power law. With x = cumulative SOL bought:

```
NAV(x)        = C * x^beta            beta = 1 - (1 - buy_fee) * backing
price(x)      = NAV(x) / backing            = 1 - 0.97 * 0.935 = 0.09305
sell_price(x) = NAV(x) * (1 - sell_fee)
round trip    = (1-buy_fee) * backing * (1-sell_fee) = 0.8525 constant
```

Fitted against the chain-exact simulation over 500 buys: max error 0.02%. Full derivation in [docs/TOKENOMICS.md](docs/TOKENOMICS.md).

## Repository layout

```
program/   the on-chain program (native Solana, no Anchor, ~600 lines)
client/    Rust client: instruction builders + the 34-test suite
sim/       Python simulator mirroring the exact integer math
data/      full datasets: every 1 SOL buy from 1 to 500, and 400 sells
docs/      TOKENOMICS.md, GUIDE.md, TESTING.md, DATASETS.md
```

## Quick start

```bash
# build the program (needs solana-cli + cargo-build-sbf)
cd program && cargo-build-sbf

# run the full test suite against a local validator
solana-test-validator --reset &
solana program deploy program/target/deploy/uponly.so --program-id <your-keypair>
cd client && cargo build
RPC=http://127.0.0.1:8899 PROGRAM=<program-id> PAYER=<payer.json> ./target/debug/curve-test
```

Expected output: 34 PASS, 0 FAIL. Details in [docs/TESTING.md](docs/TESTING.md).

## Launch configuration

Every launch picks its own personality at `Initialize`. One deployment serves unlimited tokens (curve PDA per mint).

| parameter | reference value | meaning |
|---|---|---|
| `start_price_fp` | 0.00001 SOL | launch price per token |
| `double_vol` | 25 SOL | schedule: price 2x per 25 SOL (governor tempers it) |
| `buy_fee_creator_bps` | 0 | buys pay the creator nothing |
| `buy_fee_floor_bps` | 300 | 3% of every buy goes straight to the floor |
| `sell_fee_creator_bps` | 100 | 1% of sells to the creator |
| `sell_fee_floor_bps` | 500 | 5% of sells stays in the floor |
| `min_backing_bps` | 9350 | governor: floor is never below 93.5% of price |

Hard caps in the program prevent degenerate configs (creator fees max 5% per side, backing 10% to 99%, and more).

## Status

- Program compiles and passes 34/34 tests on a local validator, including exact integer-math mirrors of every trade.
- **Not audited. Not yet deployed to mainnet.** Do not put real money on it before an independent review.

## License

MIT
