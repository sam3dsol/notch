# UPONLY Holder Guide

Plain-language guide to what actually happens when you buy, hold, and sell an UPONLY token.

## What you are buying

An UPONLY token has no liquidity pool. It trades against its own vault, a program account that holds all the SOL ever paid in. Two numbers matter:

- **Price**: what you pay to mint new tokens. It only moves up.
- **Floor**: what every token is guaranteed to redeem for (vault divided by supply). It also only moves up.

Your worst case is capped: the floor is never allowed below 93.5% of the price, and fees total ~8.8% round trip, so the most you can possibly lose, buying the exact top and selling one second later, is 14.75%. The program enforces this; it is not a promise.

## When you buy

Say you buy with 1 SOL:

1. 3% (0.03 SOL) goes straight into the vault. This raises the floor for everyone, including you.
2. 97% mints you tokens at the current curve price.
3. The price ratchets up a little because of your buy. It will never come back down.

You cannot get front-run into a dump: there is no pool, and the price you see is the price the program gives you (slippage-protected with `min_out`).

## While you hold

Every event makes your floor rise:

- Someone buys: the price and the floor both rise.
- Someone sells: 5% of their exit value stays in the vault. The floor rises. The price does not move.
- Someone donates SOL to the vault (anyone can): the floor rises.

There is no event that lowers your floor. None. The randomized on-chain test suite asserts this: across every combination of buys and sells, the floor never decreased once.

## When you sell

You redeem against the vault at the floor price:

1. Your tokens are burned.
2. You receive 94% of their floor value in SOL, instantly, from the vault. No counterparty needed, no liquidity dependence.
3. 5% stays in the vault (the holders after you say thank you), 1% goes to the creator.

Selling never crashes the chart. In testing, 81 out of 100 holders dumped everything: the price did not move down a single tick, the floor ended HIGHER than the price, and the next buyer printed a new all-time high.

## What early vs late entry looks like

From the exact math, 100 people buying 1 SOL each:

- Buyer 1 enters at 0.00001000 and is immediately over-backed (floor above entry). After one wave of volume he exits at +22% to +30% AT THE FLOOR, not on paper.
- Buyer 100 enters at 0.00001558. If he panic-sells instantly he takes the worst case, about -14.7%. If he waits, any future volume in either direction pulls him toward profit, and nothing can pull him away from it.

That last part is the key mental model: **your exit value is a one-way ratchet**. Time and volume can only improve it.

## Honest FAQ

**Is this risk-free?** No. If you buy and the token dies with zero further volume, you redeem at the floor and take up to a 14.75% loss. That is the maximum, and it shrinks as volume accrues.

**Where does the floor growth come from?** From fees and from the gap between price and floor, paid by traders. The token redistributes from churners to holders. High volume means a fast-rising floor.

**Can the team touch the vault?** The program has no admin functions. No withdraw, no pause, no parameter change. Vault SOL leaves only through holder redemptions. Read `program/src/lib.rs`, it is about 600 lines.

**What if everyone sells?** The vault pays every holder out in full at the floor. The price stays exactly where it was, it never resets. A later buyer starts fresh at that same price. While the token is alive and anyone is still holding, every sell leaves its floor share behind and lifts the floor for the remaining holders. Only on the very last exit, when nobody is left to benefit, that final share is released with the redemption instead of being stranded.

**Why not just list on a DEX?** A DEX price can go down. This one cannot. If someone lists it externally anyway, arbitrage pins the external price between the floor and the curve price.
