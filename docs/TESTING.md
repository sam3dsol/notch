# Building and Testing UPONLY

## Toolchain

- Rust (stable) with cargo
- Solana CLI 4.x with `cargo-build-sbf` (program builds against `solana-program` 2.1)
- A local `solana-test-validator` for the suite

## Build

```bash
# on-chain program -> program/target/deploy/uponly.so (+ a program keypair)
cd program
cargo-build-sbf

# client + test binary
cd ../client
cargo build
```

Note: `cargo-build-sbf` generates `program/target/deploy/uponly-keypair.json`. That keypair IS your program address on every cluster. Keep it out of the repo and back it up.

## Run the suite

```bash
solana-test-validator --reset --quiet &

# fund a payer and deploy
solana airdrop 1200 <payer-pubkey> --url http://127.0.0.1:8899
solana program deploy program/target/deploy/uponly.so \
  --program-id program/target/deploy/uponly-keypair.json \
  --keypair <payer.json> --url http://127.0.0.1:8899

RPC=http://127.0.0.1:8899 PROGRAM=<program-id> PAYER=<payer.json> \
  ./client/target/debug/curve-test
```

Expected: `34 passed, 0 failed`.

## What the 34 tests prove

The test client re-implements the program's integer math exactly (same chunk loop, same governor, same rounding), so most assertions are exact equality against on-chain results, not tolerances.

Setup and config:

- Initialize creates the curve PDA with the exact reference parameters
- re-Initialize is rejected

Buy path:

- exact token output for the 1st and 2nd buys (integer-identical to the mirror)
- the full buy amount lands in the vault (97% mint + 3% donation), creator receives nothing on buys
- exact price advance, price strictly monotone
- backing ratio at or above 93.5% after every buy
- oversized buys (more than 8 price doublings) are rejected

Sell path:

- exact 94% payout, exact 1% creator fee
- units burned, vault debited by exactly seller + creator amounts
- the 5% floor share stays: NAV strictly rises on sells
- sells never move the curve price

Safety and guards:

- `min_out` slippage rejection on both sides
- overselling a balance is rejected
- a wrong creator account is rejected
- a 100 SOL whale buy matches the mirror exactly under the governor and holds the backing ratio

Economic properties, measured live:

- instant buy-then-dump round trip loses 14.7%, inside the 15% all-in cap
- a large dump raises NAV, does not move the price, and the next buy prints HIGHER than the pre-dump price
- 20 randomized buys and sells: price monotone, NAV monotone, backing ratio never below 93.5%
- full exit of all holders: supply reaches zero, the vault keeps the accumulated fees, a restart buy works, and the price never resets

## The Python simulator

`sim/uponly_sim.py` mirrors the same integer math and generated the datasets in `data/`. Use it to test any launch configuration before deploying:

```bash
python3 sim/uponly_sim.py            # prints the reference tables
```

Edit the constants at the top (fees, backing, schedule, start price) to model your own launch.
