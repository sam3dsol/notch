# NOTCH Tokenomics

This document is the complete economic specification: where every basis point flows, the exact price and floor formulas, the conservation law that governs all designs of this kind, and the honest trade-offs.

## 1. The two prices

NOTCH tracks two numbers per token, and both are monotone non-decreasing:

- **Curve price P**: what buyers pay, what the chart shows. Only buys move it, only upward. Sells never touch it.
- **Floor / NAV**: vault balance divided by token supply. The guaranteed redemption value. Rises on every buy AND every sell.

The governor chains them together: `P <= NAV / min_backing`. With the reference 93.5% backing, the price can never run more than ~6.95% ahead of what the vault actually holds.

## 2. Fee flows (reference config)

Every buy of B SOL:

| destination | amount | effect |
|---|---|---|
| vault (donation) | 3% of B | floor rises instantly, benefits all holders including the buyer |
| vault (mint) | 97% of B | mints tokens at the curve price |
| creator | 0 | buys pay the creator nothing |

Every sell with gross floor value G (tokens x NAV):

| destination | amount | effect |
|---|---|---|
| seller | 94% of G | the redemption payout |
| vault (kept) | 5% of G | floor rises, price frozen, headroom banked |
| creator | 1% of G | the only creator revenue |

A full round trip therefore returns `0.97 x 0.935 x 0.94 = 0.8525` in the worst case: a constant 14.75% all-in maximum loss, enforced by the governor, independent of size or timing. The test suite verifies this live (measured 14.7%).

## 3. The exact formulas

### Phase 1: launch (roughly the first 5 SOL)

The price follows the configured schedule `P(x) = P0 * 2^(0.97x / double_vol)`. The 3% buy donation initially over-backs the token: buyer 1 lands with a floor ABOVE the launch price (102% backing measured). Backing decays toward the governor line as the schedule runs.

### Phase 2: governed (everything after, forever)

Once `P = NAV / 0.935` binds, one power law describes the whole system. Let x = cumulative SOL of buys:

```
beta          = 1 - (1 - buy_floor_fee) * backing_ratio
              = 1 - 0.97 * 0.935 = 0.09305

NAV(x)        = C * x^beta        (C = 9.5077e-6 for the reference launch)
price(x)      = NAV(x) / 0.935
sell_price(x) = NAV(x) * 0.94
```

Fitted against the chain-exact simulation at x = 50, 100, 250, 500: maximum error 0.02%.

**Derivation.** In the governed regime each buy dx puts all of dx into the vault (3% donation + 97% minted-against) but mints only `0.97 / 0.935 = 0.90695` of proportional floor claims, because tokens are minted at a price 1/0.935 above NAV from just 97% of the money. The unminted remainder accretes to the floor:

```
d(NAV)/NAV = beta * dx / V,   V = pool
```

Integrating with V = x (pure buying) gives the power law.

### Sells

A sell of gross value g pays out 95% of g from the vault (94 seller + 1 creator) and burns 100% of g in claims:

```
d(NAV)/NAV = +0.05 * g / V
```

Always positive: every sell raises the floor. The price does not move on sells (trading continues normally, sells just never touch it), but the governor cap `NAV/0.935` rises, so the next buy prints higher. Measured: after 81 of 100 holders dumped, the floor ended ABOVE the frozen price and the next 1 SOL buy set a new all-time high.

### Mixed flow (the general law)

```
d(ln NAV) = 0.09305 * (buy volume)/V + 0.05 * (sell gross)/V
```

A round-tripped SOL contributes ~0.143/V, about 1.5x more than a one-way buy. Churn is the strongest floor fuel in this configuration.

## 4. Volume milestones (reference config, pure buying)

Inverting the price formula from launch:

| price multiple | cumulative buys needed |
|---|---|
| 1.25x | ~11 SOL |
| 1.5x | ~65 SOL |
| 1.75x | ~343 SOL |
| 2x | ~1,437 SOL |

The early leg is fast; the curve then flattens by design. This is the direct cost of the 15% loss cap, explained next.

## 5. The conservation law (read this before configuring a launch)

The floor rises exactly as fast as traders collectively pay in (fees plus the price-floor gap). Nothing else funds it. Therefore, in any design of this kind:

```
maximum instant loss  ==  the fuel rate  ==  the speed
```

They are the same number wearing different hats. Cap everyone's worst case at 15% and each round trip can contribute at most ~15% of its size to the floor. Loosen the cap and the chart runs faster but the worst case worsens in lockstep. There is no configuration that is both fast at scale and fully backed; a fully backed up-only token grows only as fast as fee inflow relative to market cap.

NOTCH does not escape this law. It makes the trade-off explicit, configurable per launch, and enforced on-chain instead of implied by trust.

## 6. The configuration dial

`min_backing_bps` is the single most important launch parameter:

| backing | max all-in loss | beta (speed exponent) | character |
|---|---|---|---|
| 9800 (98%) | ~10.6% | 0.049 | savings-grade, very slow chart |
| 9350 (93.5%) | ~14.75% | 0.093 | reference: bounded pain, steady climb |
| 9000 (90%) | ~17.9% | 0.127 | tradeable, faster chart |
| 7500 (75%) | ~31% | 0.273 | degen leaning, fast |
| 0 (off) | unbounded | schedule-only | pure ratchet curve, price detaches from backing at scale |

All other knobs (fees, their creator/floor split, launch price, schedule) are also per-launch `Initialize` parameters. The program enforces sanity caps so no launch can be configured abusively (creator fees max 5% per side).

## 7. Comparison to fee-accrual LSTs

Volume-fee LSTs put a transfer fee (6.9% in the best-known case) into backing, so the redemption rate only rises. That design is 100% backed at all times, which also makes it slow: a doubling needs roughly 10x the market cap in transfer volume.

NOTCH keeps the part that works (sells strengthen the token, value is volume-fed, exit is always guaranteed) and changes two things: the chart price itself is up-only rather than just the redemption rate, and the safety-speed trade-off is a configurable, on-chain-enforced parameter instead of a fixed 100% backing. At 93.5% backing the early phase moves roughly 10x faster per SOL of volume; at scale the conservation law applies to everyone equally.

## 8. Worst cases, measured

From the exact program math, buying 1 SOL at the top of a wave and dumping instantly:

| after n buyers | instant round trip |
|---|---|
| 20 | -14.4% |
| 50 | -14.6% |
| 100 | -14.7% |

And realistic dump waves are much gentler: in an 80% holder exodus, sellers lost between +30.6% (first out, in profit) and -6% (worst mid-wave), because the floor rises during the wave itself.
