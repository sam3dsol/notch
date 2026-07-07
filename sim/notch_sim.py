#!/usr/bin/env python3
"""NOTCH simulator.

Mirrors the on-chain program's integer math exactly (same chunk loop, same
governor, same rounding), so its outputs match the deployed program to the
lamport. Edit CONFIG to model any launch before deploying it.

Usage:
    python3 notch_sim.py             # prints reference tables
    python3 notch_sim.py --csv DIR   # also writes the two datasets to DIR
"""

import sys

# ---------------------------------------------------------------- CONFIG ---
START_PRICE_FP = 10_000 * 10**9   # 0.00001 SOL per token (lamports/token * 1e9)
DOUBLE_VOL     = 25 * 10**9       # schedule: price 2x per 25 SOL of buys
BUY_FEE_CREATOR_BPS = 100         # buys: 1% to the platform wallet
BUY_FEE_FLOOR_BPS   = 200         # buys: 2% donated straight to the vault
SELL_FEE_CREATOR_BPS = 100        # sells: creator share of gross
SELL_FEE_FLOOR_BPS   = 500        # sells: stays in the vault
MIN_BACKING_BPS      = 9_350      # governor: price <= NAV * 10000 / this (0 = off)
# ---------------------------------------------------------------------------

E18 = 10**18
FP = 10**9
SOL = 10**9

# Q48 fixed-point power (mirrors program/src/lib.rs) for the path-independent mint.
POW_F = 48
POW_ONE = 1 << POW_F
TWO_POW_LUT = [0, 398065729532861, 334732044999537, 306950638654744, 293936938588305, 287638476118103, 284540038248454, 283003357999923, 282238132792268, 281856296460737, 281665572056717, 281570258256901, 281522613452764, 281498794074042, 281486885140443, 281480930862574, 281477953770871, 281476465236828, 281475720972758, 281475348841461, 281475162775997, 281475069743311, 281475023226980, 281474999968817, 281474988339736, 281474982525196, 281474979617926, 281474978164291, 281474977437473, 281474977074065, 281474976892360, 281474976801508, 281474976756082, 281474976733369, 281474976722013, 281474976716334, 281474976713495, 281474976712076, 281474976711366, 281474976711011, 281474976710833, 281474976710745, 281474976710700, 281474976710678, 281474976710667, 281474976710662, 281474976710659, 281474976710657, 281474976710657]


def _log2q(x):
    e = 0
    while x >= (POW_ONE << 1):
        x >>= 1; e += 1
    r = e << POW_F
    for i in range(1, POW_F + 1):
        x = (x * x) >> POW_F
        if x >= (POW_ONE << 1):
            r += POW_ONE >> i; x >>= 1
    return r


def _exp2q(y):
    ip = y >> POW_F; fr = y & (POW_ONE - 1); r = POW_ONE
    for i in range(1, POW_F + 1):
        if fr & (POW_ONE >> i):
            r = (r * TWO_POW_LUT[i]) >> POW_F
    return r << ip


def pow_ratio_q(num, den, bps):
    if den == 0 or num < den:
        return None
    ratio = (num * POW_ONE) // den
    bl = (_log2q(ratio) * ((bps * POW_ONE) // 10000)) >> POW_F
    return _exp2q(bl)


class Curve:
    def __init__(self):
        self.price = START_PRICE_FP  # lamports per whole token, x1e9
        self.vault = 0               # lamports
        self.supply = 0              # token base units (1e9 = 1 whole token)
        self.creator = 0             # lamports earned by the creator

    # -- exact mirror of the program's Buy ---------------------------------
    def buy(self, lamports):
        cf = lamports * BUY_FEE_CREATOR_BPS // 10_000
        dn = lamports * BUY_FEE_FLOOR_BPS // 10_000
        net = lamports - cf - dn
        self.creator += cf
        # Path-independent mint at the governor price. Donation lands first, then
        # net mints at price = NAV/backing => S1 = S0 * (Vf/V0)^backing. Splitting
        # a buy can't change the result (replaces the old path-dependent chunk loop).
        v0 = self.vault + dn
        vf = v0 + net
        s0 = self.supply
        if s0 == 0:
            out = net * E18 // self.price            # genesis: mint at start price
        else:
            factor = pow_ratio_q(vf, v0, MIN_BACKING_BPS)
            out = (s0 * factor >> POW_F) - s0
        self.vault, self.supply = vf, s0 + out
        # reported price = NAV / backing (never decreases)
        nav = vf * E18 // self.supply
        price_new = nav * 10_000 // MIN_BACKING_BPS if MIN_BACKING_BPS else nav
        self.price = max(price_new, self.price)
        return out  # token units minted

    # -- exact mirror of the program's Sell --------------------------------
    def sell(self, units):
        gross = units * self.vault // self.supply
        cf = gross * SELL_FEE_CREATOR_BPS // 10_000
        keep = gross * SELL_FEE_FLOOR_BPS // 10_000
        to_seller = gross - cf - keep
        self.vault -= to_seller + cf
        self.supply -= units
        self.creator += cf
        return to_seller  # lamports paid to the seller

    # -- views --------------------------------------------------------------
    def price_sol(self):
        return self.price / 1e18

    def nav_sol(self):
        return self.vault / self.supply if self.supply else 0.0

    def backing_pct(self):
        return self.nav_sol() / self.price_sol() * 100 if self.supply else 0.0


def buys_dataset(n=500):
    c = Curve()
    rows = []
    for i in range(1, n + 1):
        entry = c.price_sol()
        out = c.buy(SOL)
        nav = c.nav_sol()
        rt = out * nav * (10_000 - SELL_FEE_CREATOR_BPS - SELL_FEE_FLOOR_BPS) / 10_000 / 1e9
        rows.append((i, entry, out / 1e9, 1e9 / out, c.price_sol(), nav,
                     nav * (10_000 - SELL_FEE_CREATOR_BPS - SELL_FEE_FLOOR_BPS) / 10_000,
                     c.backing_pct(), c.vault / 1e9, rt, (rt - 1) * 100))
    return c, rows


def sells_dataset(c, n=400):
    rows = []
    for m in range(1, n + 1):
        units = int(SOL * c.supply / c.vault)  # about 1 SOL gross
        got = c.sell(units)
        rows.append((m, units / 1e9, got / 1e9, c.nav_sol(),
                     c.nav_sol() * (10_000 - SELL_FEE_CREATOR_BPS - SELL_FEE_FLOOR_BPS) / 10_000,
                     c.price_sol(), c.vault / 1e9))
    return rows


def main():
    c, rows = buys_dataset()
    print("BUYS (1 SOL each):")
    print(f"{'cum':>4} {'entry':>11} {'price':>11} {'NAV':>11} {'redeem':>11} {'back%':>6} {'roundtrip':>9}")
    for r in rows:
        if r[0] in (1, 5, 10, 20, 50, 100, 250, 500):
            print(f"{r[0]:>4} {r[1]:>11.8f} {r[4]:>11.8f} {r[5]:>11.8f} {r[6]:>11.8f} {r[7]:>6.1f} {r[9]:>9.4f}")
    sells = sells_dataset(c)
    print("\nSELLS after (each ~1 SOL gross):")
    for r in sells:
        if r[0] in (1, 100, 200, 400):
            print(f"sell {r[0]:>3}: receives {r[2]:.4f}  NAV {r[3]:.8f}  price(frozen) {r[5]:.8f}  pool {r[6]:.1f}")

    if "--csv" in sys.argv:
        outdir = sys.argv[sys.argv.index("--csv") + 1]
        with open(f"{outdir}/notch-dataset-buys-1to500.csv", "w") as f:
            f.write("cum_buys_sol,entry_price_sol,tokens_for_1sol,eff_cost_sol,price_after_sol,"
                    "nav_sol,redeem_price_after_fees_sol,backing_pct,pool_sol,"
                    "instant_roundtrip_sol,instant_pnl_pct\n")
            for r in rows:
                f.write(",".join(f"{x:.10g}" for x in r) + "\n")
        with open(f"{outdir}/notch-dataset-sells-after500.csv", "w") as f:
            f.write("sell_n,tokens_sold,seller_receives_sol,nav_sol,redeem_price_sol,"
                    "price_frozen_sol,pool_sol\n")
            for r in sells:
                f.write(",".join(f"{x:.10g}" for x in r) + "\n")
        print(f"\nCSVs written to {outdir}/")


if __name__ == "__main__":
    main()
