# NOTCH Security Audit

Internal adversarial audit of `program/src/lib.rs`. Eight independent reviewers each took one attack surface (arithmetic and rounding, account and signer validation, SPL byte parsing, economic and game-theoretic invariants, CPI and lamport handling, initialization and lifecycle, denial of service and griefing, value conservation and accounting). Every raw finding was then handed to a separate adversarial verifier whose job was to refute it by tracing the actual guards.

Result: 21 raw findings, 18 refuted as already-guarded or non-exploitable, 1 genuine defect (surfaced by three reviewers at different severities, one root cause). The defect is fixed; details below.

This is an internal review, not a substitute for a professional third-party audit before significant value is deposited.

## Finding (fixed): NAV-floor bypass at zero supply

**Severity:** medium. **Location:** `buy()` pricing branch and `sell()` full-exit path.

**Root cause.** In `buy()`, both the NAV floor (mint price can never be below NAV) and the governor cap live inside the `if s_run > 0` block. When mint supply is zero, that block is skipped and the first chunk is priced at the bare schedule average. If the vault holds backing while supply is zero, the first buyer mints almost all of a fresh supply at the low schedule price and can then redeem the entire backing.

**How zero-supply-with-backing was reachable.** `sell()` allowed a full exit (`units == supply`) that retained the seller's floor share in the vault. After a 100% exit this left `supply == 0` with `backing > 0`: ownerless value that the next buyer could capture. (The value at risk is only the residual floor share present at a full exit, and only on small or single-holder curves that actually empty; no vault with live holders is ever exposed, because the NAV floor applies whenever supply is positive.)

**Fix.** On a full exit (`units == supply`) the floor share is paid out with the final redemption instead of retained, because there are no remaining holders to benefit from it. This restores the invariant **supply == 0 implies backing == 0**, so the zero-supply pricing branch can never mint against stranded value. A regression test asserts that a full exit leaves zero backing and that a subsequent revival buy cannot be round-tripped for a profit.

**Residual, documented.** A third party can still transfer SOL directly to a curve PDA while supply is zero (a donation to an empty vault). That donation would go to whoever mints the first share. This is an external actor gifting funds to a vault with no holders, not a loss of any deposited or protocol-retained value, and it is not part of the standard launch flow (a normal launch starts empty and the first buy establishes supply and backing together). No on-chain guard can both prevent this and still allow an empty curve to be revived without creating a one-lamport griefing lock, so it is documented rather than blocked.

## Operational requirement: launch atomically

`Initialize` is permissionless by design (one deployment serves many tokens, pump.fun style). It authorizes on mint state, not on caller identity. If a deployer creates the mint, sets its authority to the curve PDA, and calls `Initialize` in three separate landed transactions, a front-runner could call `Initialize` first and set themselves as the fee recipient. This steals only a prospective fee stream on an unpromoted token and locks no user funds, but to avoid it entirely, **perform mint creation, authority assignment, and `Initialize` in a single atomic transaction.** The launch tooling must do this.

## Surfaces reviewed and cleared

The following were probed and confirmed safe (representative, not exhaustive):

- **Value conservation.** Buy and sell lamport bookkeeping were traced; the vault's real balance always equals what the NAV math assumes. `cum_vol` is write-only telemetry and cannot affect pricing.
- **Rounding direction.** Every division floors toward the vault (minted units and gross redemption both round down), so rounding can never favor a user into draining value.
- **Price and NAV monotonicity.** Price never decreases (the only downward clamp is followed by a `p < p0` guard). NAV never decreases on buys (mint price is floored above NAV for all positive-supply states) or on sells (the retained floor share raises it).
- **Reentrancy.** The only external calls are to the pinned legacy SPL Token program and the System program; neither re-enters arbitrary code. Token-2022 (with its transfer hooks) is excluded because `Initialize` requires the mint to be owned by the legacy token program.
- **Direct lamport payout in `sell()`.** Legal because the PDA is program-owned; the debit cannot underflow (`vault_out <= v` checked), and a rent-floor recheck follows.
- **CPI signing.** `mint_to` signs with the correct PDA seeds; `burn` requires the seller to sign as owner or delegate, so no one can burn another holder's tokens.
- **Token-account and mint substitution.** Only the mint field is byte-checked, but any confusion or foreign account causes the pinned SPL Token CPI to revert the whole transaction before value moves; minting or paying to one's own foreign account is self-directed and harmless.
- **Governor overflow fallback.** The `unwrap_or(u128::MAX)` in the governor cap fails open only in the safe direction (higher price, fewer tokens, more backing per token) and is practically unreachable.
- **Configuration bricks.** A creator can pick an in-range but absurd start price or schedule that makes their own launch untradeable; this is self-inflicted, harms no other party, and locks no funds.
- **DoS and fund lock.** No external actor can push a well-configured live vault into a permanent failure state; sells always remain solvent (`units <= supply` bounds gross by backing).
