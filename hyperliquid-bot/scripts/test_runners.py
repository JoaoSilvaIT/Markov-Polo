#!/usr/bin/env python3
"""
Test Pure Runner Trailing Stop on 7-Year BTC US Open (2020 - 2026)
Lets massive macro trend runners (100 to 500 bps) run without capping at 2R or 3R.
"""

import json
import datetime
import time
from fast_7year_orb import sessions


def test_pure_runners(
    min_or=35.0, sl_bps=30.0, trail_activation_bps=60.0, trail_distance_bps=35.0
):
    notional_usdc = 175.0
    taker_fee = 0.000432
    maker_fee = 0.000144
    stop_slippage = 0.000050

    trades = []

    for sample_dt, or_bars, session_bars in sessions:
        or_high = max(b["high"] for b in or_bars)
        or_low = min(b["low"] for b in or_bars)
        or_range_bps = (or_high - or_low) / or_low * 10000.0

        if or_range_bps < min_or:
            continue

        in_pos = False
        side = None
        entry_px = 0.0
        stop_px = 0.0
        best_px = 0.0

        for b in session_bars:
            if not in_pos:
                if b["high"] > or_high:
                    in_pos = True
                    side = "LONG"
                    entry_px = or_high
                    stop_px = entry_px * (1.0 - sl_bps * 1e-4)
                    best_px = entry_px
                elif b["low"] < or_low:
                    in_pos = True
                    side = "SHORT"
                    entry_px = or_low
                    stop_px = entry_px * (1.0 + sl_bps * 1e-4)
                    best_px = entry_px
            else:
                # Pure Runner Trailing Ratchet
                if side == "LONG":
                    if b["high"] > best_px:
                        best_px = b["high"]
                    gain_bps = (best_px - entry_px) / entry_px * 10000.0
                    if gain_bps >= trail_activation_bps:
                        ratchet_stop = best_px * (1.0 - trail_distance_bps * 1e-4)
                        stop_px = max(stop_px, ratchet_stop)
                else:
                    if b["low"] < best_px:
                        best_px = b["low"]
                    gain_bps = (entry_px - best_px) / entry_px * 10000.0
                    if gain_bps >= trail_activation_bps:
                        ratchet_stop = best_px * (1.0 + trail_distance_bps * 1e-4)
                        stop_px = min(stop_px, ratchet_stop)

                exit_px = None
                exit_reason = None
                is_stop = False

                if side == "LONG":
                    if b["low"] <= stop_px:
                        exit_px = stop_px * (1.0 - stop_slippage)
                        exit_reason = "STOP" if stop_px < entry_px else "TRAIL_WIN"
                        is_stop = True
                else:
                    if b["high"] >= stop_px:
                        exit_px = stop_px * (1.0 + stop_slippage)
                        exit_reason = "STOP" if stop_px > entry_px else "TRAIL_WIN"
                        is_stop = True

                if exit_px is None and b == session_bars[-1]:
                    exit_px = b["close"]
                    exit_reason = "SESSION_TIMEOUT"

                if exit_px is not None:
                    ent_fee = notional_usdc * taker_fee
                    ext_fee = notional_usdc * (taker_fee if is_stop else maker_fee)
                    total_fees = ent_fee + ext_fee

                    gross_ret = (
                        (exit_px - entry_px) / entry_px
                        if side == "LONG"
                        else (entry_px - exit_px) / entry_px
                    )
                    gross_usd = notional_usdc * gross_ret
                    net_usd = gross_usd - total_fees
                    net_bps = (net_usd / notional_usdc) * 10000.0

                    trades.append(
                        {
                            "date": sample_dt.strftime("%Y-%m-%d"),
                            "year": sample_dt.year,
                            "gross_bps": gross_ret * 10000.0,
                            "net_bps": net_bps,
                            "net_usd": net_usd,
                            "reason": exit_reason,
                        }
                    )
                    break

    wins = [t for t in trades if t["net_usd"] > 0]
    losses = [t for t in trades if t["net_usd"] <= 0]
    wr = len(wins) / len(trades) * 100.0 if trades else 0.0
    net_usd = sum(t["net_usd"] for t in trades)
    gross_usd = sum(t["gross_bps"] * notional_usdc / 10000.0 for t in trades)
    fees_usd = gross_usd - net_usd
    avg_win = sum(t["net_bps"] for t in wins) / len(wins) if wins else 0.0
    avg_loss = sum(t["net_bps"] for t in losses) / len(losses) if losses else 0.0
    payoff = abs(avg_win / avg_loss) if avg_loss != 0 else 0

    # Count huge runners (> +100 bps)
    huge_runners = [t for t in trades if t["net_bps"] >= 100.0]

    print(
        f"MinOR={min_or:2.0f} | SL={sl_bps:2.0f} | Act={trail_activation_bps:2.0f} | Dist={trail_distance_bps:2.0f} -> {len(trades):4d} tr ({wr:4.1f}% WR) | Net: ${net_usd:+7.2f} | Gross: ${gross_usd:+7.2f} | Fees: ${fees_usd:6.2f} | Payoff: {payoff:.2f} (Runners >100bps: {len(huge_runners)})"
    )


if __name__ == "__main__":
    print("=" * 110)
    print("🚀 [7-YEAR BITCOIN PURE RUNNER TRAILING MATRIX] (2020 - 2026)")
    print("=" * 110)
    for min_or in [0.0, 30.0, 40.0, 50.0]:
        for sl in [25.0, 30.0, 40.0]:
            for act in [50.0, 70.0, 90.0]:
                for dist in [30.0, 45.0]:
                    test_pure_runners(
                        min_or=min_or,
                        sl_bps=sl,
                        trail_activation_bps=act,
                        trail_distance_bps=dist,
                    )
