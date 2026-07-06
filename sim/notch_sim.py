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
BUY_FEE_CREATOR_BPS = 0           # buys: creator share
BUY_FEE_FLOOR_BPS   = 300         # buys: donated straight to the vault
SELL_FEE_CREATOR_BPS = 100        # sells: creator share of gross
SELL_FEE_FLOOR_BPS   = 500        # sells: stays in the vault
MIN_BACKING_BPS      = 9_350      # governor: price <= NAV * 10000 / this (0 = off)
# ---------------------------------------------------------------------------

E18 = 10**18
FP = 10**9
LN2 = 693_147_181
SOL = 10**9
CHUNK = max(DOUBLE_VOL // 32, 1)


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
        rem, p, out = net, self.price, 0
        v_run, s0 = self.vault + dn, self.supply
        while rem > 0:
            c = min(rem, CHUNK)
            p0 = p
            p = p + p * (LN2 * c // DOUBLE_VOL) // FP        # schedule advance
            s_run = s0 + out
            if s_run > 0:
                nav = v_run * E18 // s_run
                if MIN_BACKING_BPS:
                    cap = nav * 10_000 // MIN_BACKING_BPS    # governor
                    p = max(min(p, cap), p0)                 # never down
                pe = (p0 + p) // 2
                if pe <= nav:
                    pe = nav + nav // 10_000 + 1             # never mint below NAV
            else:
                pe = (p0 + p) // 2
            out += c * E18 // pe
            v_run += c
            rem -= c
        self.price, self.vault, self.supply = p, v_run, s0 + out
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
