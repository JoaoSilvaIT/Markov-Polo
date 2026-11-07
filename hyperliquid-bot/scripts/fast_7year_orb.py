#!/usr/bin/env python3
"""
Fast 7-Year Bitcoin US Open Opening Range Breakout (ORB) Simulation (2020 - 2026)
Loads the 1,782 US trading days into memory once and evaluates all parameter variations in milliseconds.
"""

import json
import datetime
import time


def is_us_dst(dt_utc):
    year = dt_utc.year
    mar1 = datetime.datetime(year, 3, 1, tzinfo=datetime.timezone.utc)
    dst_start = mar1 + datetime.timedelta(days=(6 - mar1.weekday() + 7) % 7 + 7)
    nov1 = datetime.datetime(year, 11, 1, tzinfo=datetime.timezone.utc)
    dst_end = nov1 + datetime.timedelta(days=(6 - nov1.weekday()) % 7)
    return dst_start <= dt_utc < dst_end


print(
    "⏳ [DATA PRE-FILTER] Loading 7-year BTC US Open sessions (13:00-16:30 UTC) into memory..."
)
t0 = time.time()

# Pre-extract only the US open sessions from the 3.6M bars
sessions = []  # List of (sample_dt, or_bars, session_bars)

current_day = None
day_bars = []


def extract_session(d_bars):
    if not d_bars:
        return
    sample_dt = datetime.datetime.fromtimestamp(
        d_bars[0]["timestamp_ms"] / 1000.0, datetime.timezone.utc
    )
    if sample_dt.weekday() >= 5:  # Skip weekends
        return

    open_hour = 13 if is_us_dst(sample_dt) else 14
    t_open = int(
        sample_dt.replace(
            hour=open_hour, minute=30, second=0, microsecond=0
        ).timestamp()
        * 1000
    )
    t_or_end = int(
        sample_dt.replace(
            hour=open_hour, minute=35, second=0, microsecond=0
        ).timestamp()
        * 1000
    )
    t_session_end = int(
        sample_dt.replace(
            hour=open_hour + 2, minute=30, second=0, microsecond=0
        ).timestamp()
        * 1000
    )

    or_bars = [b for b in d_bars if t_open <= b["timestamp_ms"] < t_or_end]
    session_bars = [b for b in d_bars if t_or_end <= b["timestamp_ms"] <= t_session_end]

    if len(or_bars) >= 5 and len(session_bars) >= 20:
        sessions.append((sample_dt, or_bars, session_bars))


with open("hl_history_btc.jsonl") as f:
    for line in f:
        if not line.strip():
            continue
        b = json.loads(line)
        dt = datetime.datetime.fromtimestamp(
            b["timestamp_ms"] / 1000.0, datetime.timezone.utc
        )
        d_key = dt.strftime("%Y-%m-%d")
        if current_day != d_key:
            if day_bars:
                extract_session(day_bars)
            current_day = d_key
            day_bars = [b]
        else:
            # Only keep bars between 13:00 and 17:00 UTC to save RAM
            if 13 <= dt.hour <= 17:
                day_bars.append(b)

if day_bars:
    extract_session(day_bars)

print(
    f"✅ [LOADED] {len(sessions)} US Open trading sessions pre-cached in {time.time() - t0:.1f}s!\n"
)


def simulate_orb(
    is_maker_entry=False,
    min_or_bps=35.0,
    sl_bps=35.0,
    tp_target_mult=2.5,
    trail_after_r=1.5,
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

        if or_range_bps < min_or_bps:
            continue

        in_pos = False
        side = None
        entry_px = 0.0
        stop_px = 0.0
        tp_px = 0.0
        best_px = 0.0

        for b in session_bars:
            if not in_pos:
                if b["high"] > or_high:
                    in_pos = True
                    side = "LONG"
                    entry_px = or_high
                    stop_px = entry_px * (1.0 - sl_bps * 1e-4)
                    tp_px = entry_px * (1.0 + (sl_bps * tp_target_mult) * 1e-4)
                    best_px = entry_px
                elif b["low"] < or_low:
                    in_pos = True
                    side = "SHORT"
                    entry_px = or_low
                    stop_px = entry_px * (1.0 + sl_bps * 1e-4)
                    tp_px = entry_px * (1.0 - (sl_bps * tp_target_mult) * 1e-4)
                    best_px = entry_px
            else:
                # Ratchet / Trailing stop
                if side == "LONG":
                    if b["high"] > best_px:
                        best_px = b["high"]
                    gain_bps = (best_px - entry_px) / entry_px * 10000.0
                    if gain_bps >= sl_bps * trail_after_r:
                        stop_px = max(stop_px, best_px * (1.0 - sl_bps * 1e-4))
                else:
                    if b["low"] < best_px:
                        best_px = b["low"]
                    gain_bps = (entry_px - best_px) / entry_px * 10000.0
                    if gain_bps >= sl_bps * trail_after_r:
                        stop_px = min(stop_px, best_px * (1.0 + sl_bps * 1e-4))

                exit_px = None
                exit_reason = None
                is_maker_exit = False

                if side == "LONG":
                    if b["low"] <= stop_px:
                        exit_px = stop_px * (1.0 - stop_slippage)
                        exit_reason = "STOP" if stop_px < entry_px else "TRAIL_WIN"
                    elif b["high"] >= tp_px:
                        exit_px = tp_px
                        exit_reason = "TP_TARGET"
                        is_maker_exit = True
                else:
                    if b["high"] >= stop_px:
                        exit_px = stop_px * (1.0 + stop_slippage)
                        exit_reason = "STOP" if stop_px > entry_px else "TRAIL_WIN"
                    elif b["low"] <= tp_px:
                        exit_px = tp_px
                        exit_reason = "TP_TARGET"
                        is_maker_exit = True

                if exit_px is None and b == session_bars[-1]:
                    exit_px = b["close"]
                    exit_reason = "TIMEOUT"

                if exit_px is not None:
                    ent_fee = notional_usdc * (
                        maker_fee if is_maker_entry else taker_fee
                    )
                    ext_fee = notional_usdc * (
                        maker_fee if is_maker_exit else taker_fee
                    )
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

    tp_hits = len([t for t in trades if t["reason"] == "TP_TARGET"])
    trail_wins = len([t for t in trades if t["reason"] == "TRAIL_WIN"])
    stops = len([t for t in trades if t["reason"] == "STOP"])

    return (
        len(trades),
        len(wins),
        len(losses),
        wr,
        net_usd,
        gross_usd,
        fees_usd,
        avg_win,
        avg_loss,
        tp_hits,
        trail_wins,
        stops,
    )


print("=" * 105)
print("🔬 [7-YEAR BITCOIN US OPEN ORB RESULTS] 2020 - 2026 (1,782 Sessions)")
print(
    "   Notional: $175 USDC | Leverage: 3x | VIP0 Fees: 1.44bps maker / 4.32bps taker"
)
print("=" * 105)

for maker_entry in [False, True]:
    entry_mode = "MAKER RETEST" if maker_entry else "TAKER BREAKOUT"
    print(f"\n--- {entry_mode} EXECUTION ---")
    for min_or in [0.0, 30.0, 40.0, 50.0]:
        for sl in [30.0, 40.0]:
            for tp_mult in [2.0, 3.0]:
                n, w, l, wr, net_usd, gross_usd, fees_usd, avg_w, avg_l, tp, tr, st = (
                    simulate_orb(
                        is_maker_entry=maker_entry,
                        min_or_bps=min_or,
                        sl_bps=sl,
                        tp_target_mult=tp_mult,
                        trail_after_r=1.5,
                    )
                )
                payoff = abs(avg_w / avg_l) if avg_l != 0 else 0
                print(
                    f"OR>={min_or:2.0f}bps | SL={sl:2.0f}bps | TP={tp_mult:.1f}R -> {n:4d} tr ({wr:4.1f}% WR) | Net: ${net_usd:+7.2f} | Gross: ${gross_usd:+7.2f} | Fees: ${fees_usd:6.2f} | Payoff: {payoff:.2f} (TP:{tp:3d} Trail:{tr:3d} Stop:{st:3d})"
                )
