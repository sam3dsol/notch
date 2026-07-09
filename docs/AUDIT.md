# NOTCH Security Audit

## The short version

We stress-tested our own program before anyone else could.

Eight independent reviewers were each given one way to try to break NOTCH: the math, the account checks, the byte parsing, the economics, the money movement, the launch flow, denial of service, and the accounting. Everything they claimed was then handed to a separate adversarial verifier whose only job was to prove them wrong by tracing the actual code.

**The result: 21 claimed problems. 18 turned out to be already guarded or impossible. 1 was a real bug. It is fixed, and a regression test now proves it stays fixed.** The remaining 2 items are operational notes, documented below.

**What the bug meant for users: nothing, for any token with holders.** It only affected a curve that had been fully emptied, where a leftover crumb of value could be scooped by the next buyer. No vault with live holders was ever exposed. The fix makes an emptied curve hold exactly zero, so there is nothing to scoop.

This was a rigorous internal review, not a review by an external firm. Read it, then read the code: everything below is checkable in `program/src/lib.rs`.

## How the audit worked

Each reviewer owned one risk area:

| Surface | The question they tried to answer |
|---|---|
| Arithmetic and rounding | Can rounding errors leak value out of the vault? |
| Accounts and signers | Can someone pass a fake account and act as someone else? |
| Byte parsing | Can malformed token data confuse the program? |
| Economics | Can trading patterns game the curve for free money? |
| Money movement | Does every lamport end up exactly where the math says? |
| Launch and lifecycle | Can a launch be hijacked or corrupted? |
| Denial of service | Can anyone freeze a token or lock funds? |
| Value conservation | Does the vault always cover every holder at the floor? |

Every finding then went to a second, independent check that tried to refute it against the real code paths. Only what survived both passes counts.

## The one real bug, in plain words

Think of a token where every single holder has sold: the vault should be empty, because nobody is owed anything.

Before the fix, the very last seller left a small tip behind in the vault (the usual 5% floor share, which normally benefits the remaining holders). But after a 100% exit there are no remaining holders. That crumb sat in an empty vault, and the pricing rule that protects holders was skipped when supply was zero. So the next person to buy into the dead token could mint a fresh supply cheaply and pocket the crumb.

**Scope:** only the final seller's floor share, only on a curve that fully emptied, which in practice means tiny or single-holder tokens. A token with anyone still holding was never exposed, because the price floor applies whenever supply is positive.

**Fix:** when the last holder sells, they are paid the floor share too, since there is nobody left for it to benefit. An emptied curve now holds exactly zero. This restores a simple rule the whole design leans on:

> **If nobody holds the token, the vault holds nothing.** (supply == 0 implies backing == 0)

A regression test asserts both halves: a full exit leaves zero backing, and reviving a dead token cannot be round-tripped for a profit.

### The same bug, precisely

**Severity:** medium. **Location:** `buy()` pricing branch and `sell()` full-exit path.

In `buy()`, both the NAV floor (mint price can never be below NAV) and the governor cap lived inside the `if s_run > 0` block. With supply zero the block was skipped and the first chunk was priced at the bare schedule average. `sell()` allowed a full exit (`units == supply`) that retained the seller's floor share, leaving `supply == 0` with `backing > 0`: ownerless value the next buyer could capture by minting nearly all of a fresh supply at the schedule price and redeeming the backing. The fix pays the floor share out with the final redemption, so the zero-supply pricing branch can never mint against stranded value. (The code excerpts describe the program as it stood at audit time, before the mint was rewritten as a single path-independent power law; the fixed invariant is unchanged and remains regression-tested in the current suite.)

## Two things to know, documented rather than blocked

**1. Donations to a dead token belong to its reviver.** Anyone can transfer SOL directly to a curve address. If they do that while the token has zero holders, whoever buys in first captures the gift. This is an outside party donating to an empty vault, not a loss of anything deposited by users or held by the protocol, and it is not part of any normal launch. Blocking it would require a griefing-prone lock, so it is documented instead.

**2. Launches must be atomic, and the tooling makes them atomic.** `Initialize` is permissionless by design: one deployment serves every token, and it authorizes on mint state, not caller identity. If someone launched in three separate transactions, a front-runner could slip in and claim the creator fee stream for that token (no user funds at risk, only prospective fees on an unpromoted token). The launch tooling therefore creates the mint, assigns authority, and initializes the curve in a single transaction, which closes the window completely.

## Everything else that was probed and cleared

Plain question first, technical answer after.

**Can rounding leak money out of the vault?** No. Every division rounds in the vault's favor: minted units and gross redemptions both round down, so rounding can only leave dust behind for holders, never drain value.

**Can the price or the floor ever go down?** No. Price never decreases (the only downward clamp is followed by a `p < p0` guard). The floor never decreases on buys (the mint price is floored at or above NAV for every positive-supply state) or on sells (the retained floor share raises it).

**Does the vault's real balance always match the math?** Yes. Buy and sell lamport bookkeeping was traced end to end; the balance always equals what the NAV math assumes. `cum_vol` is write-only telemetry and cannot affect pricing.

**Can a malicious program re-enter and drain funds?** No. The only external calls are to the pinned legacy SPL Token program and the System program, neither of which re-enters arbitrary code. Token-2022, with its transfer hooks, is excluded: `Initialize` requires the mint to be owned by the legacy token program.

**Can someone burn or sell another holder's tokens?** No. `burn` requires the seller to sign as owner or delegate, and `mint_to` signs with the correct program address seeds only.

**Can fake or substituted accounts trick the program?** No. Any confused or foreign account causes the pinned SPL Token call to revert the whole transaction before value moves; directing a payout at your own foreign account is self-directed and harmless.

**Is the direct lamport payout in `sell()` safe?** Yes. The vault account is program-owned, the debit cannot underflow (`vault_out <= v` is checked), and a rent-floor recheck follows the payout.

**What if the governor math overflows?** It fails open only in the safe direction (`unwrap_or(u128::MAX)` means a higher price, fewer tokens minted, more backing per token) and is practically unreachable.

**Can a creator brick their own launch?** They can pick an in-range but absurd start price that makes their own token untradeable. That harms nobody else and locks no funds.

**Can anyone freeze a healthy token or trap funds?** No. No external actor can push a well-configured live vault into a permanent failure state, and sells always remain solvent (`units <= supply` bounds every redemption by the backing).

## Related

- The Robinhood Chain port ([notch-evm](https://github.com/sam3dsol/notch-evm)) re-implements the same math and re-proves the same invariants in its own test suite, including bit-for-bit parity of the fixed-point power law, the full-exit invariant above, and EVM-specific concerns (reentrancy guards, fee recipients that refuse payment).
- Verify the deployed code yourself: the on-chain program builds reproducibly from this repository, and the EVM contracts are source-verified on Blockscout.
