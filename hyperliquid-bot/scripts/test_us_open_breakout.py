import json
import datetime


def run_confirmed_simulation(
    sl_bps=35.0, min_obi=0.15, trail_threshold_bps=50.0, trail_dist_bps=30.0
):
    with open("hl_history_native_btc.jsonl") as f:
        bars = [json.loads(l) for l in f]

    bars.sort(key=lambda b: b["timestamp_ms"])
    notional_usdc = 175.0
    maker_fee = 0.000144
    taker_fee = 0.000432
    stop_slippage = 0.000050

    trades = []
    business_days = [
        "2026-10-22",
        "2026-10-23",
        "2026-10-26",
        "2026-10-27",
        "2026-10-28",
        "2026-10-29",
        "2026-10-30",
        "2026-11-02",
        "2026-11-03",
        "2026-11-04",
        "2026-11-05",
    ]

    for d_str in business_days:
        dt_day = datetime.datetime.strptime(d_str, "%Y-%m-%d").replace(
            tzinfo=datetime.timezone.utc
        )
        open_hour = (
            13
            if dt_day < datetime.datetime(2026, 11, 1, tzinfo=datetime.timezone.utc)
            else 14
        )

        t_open = int(dt_day.replace(hour=open_hour, minute=30).timestamp() * 1000)
        t_or_end = int(dt_day.replace(hour=open_hour, minute=35).timestamp() * 1000)
        t_session_end = int(
            dt_day.replace(hour=open_hour + 2, minute=30).timestamp() * 1000
        )

        or_bars = [b for b in bars if t_open <= b["timestamp_ms"] < t_or_end]
        session_bars = [
            b for b in bars if t_or_end <= b["timestamp_ms"] <= t_session_end
        ]

        if len(or_bars) < 5 or len(session_bars) < 20:
            continue

        or_high = max(b["high"] for b in or_bars)
        or_low = min(b["low"] for b in or_bars)

        in_pos = False
        side = None
        entry_px = 0.0
        stop_px = 0.0
        best_px = 0.0

        for b in session_bars:
            if not in_pos:
                # Require real taker flow confirmation (OBI)
                long_trigger = b["high"] > or_high and (
                    b["obi_instant"] >= min_obi or b["obi_1s"] >= min_obi
                )
                short_trigger = b["low"] < or_low and (
                    b["obi_instant"] <= -min_obi or b["obi_1s"] <= -min_obi
                )

                if long_trigger:
                    in_pos = True
                    side = "LONG"
                    entry_px = or_high
                    stop_px = entry_px * (1.0 - sl_bps * 1e-4)
                    best_px = entry_px
                elif short_trigger:
                    in_pos = True
                    side = "SHORT"
                    entry_px = or_low
                    stop_px = entry_px * (1.0 + sl_bps * 1e-4)
                    best_px = entry_px
            else:
                # Trailing stop ratchet
                if side == "LONG":
                    if b["high"] > best_px:
                        best_px = b["high"]
                    gain_bps = (best_px - entry_px) / entry_px * 10000.0
                    if gain_bps >= trail_threshold_bps:
                        ratchet_stop = best_px * (1.0 - trail_dist_bps * 1e-4)
                        stop_px = max(stop_px, ratchet_stop)
                else:
                    if b["low"] < best_px:
                        best_px = b["low"]
                    gain_bps = (entry_px - best_px) / entry_px * 10000.0
                    if gain_bps >= trail_threshold_bps:
                        ratchet_stop = best_px * (1.0 + trail_dist_bps * 1e-4)
                        stop_px = min(stop_px, ratchet_stop)

                exit_px = None
                exit_reason = None
                is_stop = False

                if side == "LONG":
                    if b["low"] <= stop_px:
                        exit_px = stop_px * (1.0 - stop_slippage)
                        exit_reason = "STOP" if stop_px < entry_px else "PROFIT_TRAIL"
                        is_stop = True
                else:
                    if b["high"] >= stop_px:
                        exit_px = stop_px * (1.0 + stop_slippage)
                        exit_reason = "STOP" if stop_px > entry_px else "PROFIT_TRAIL"
                        is_stop = True

                if exit_px is None and b == session_bars[-1]:
                    exit_px = b["close"]
                    exit_reason = "SESSION_TIMEOUT"

                if exit_px is not None:
                    entry_fee_usd = notional_usdc * taker_fee
                    exit_fee_usd = notional_usdc * (taker_fee if is_stop else maker_fee)
                    total_fees = entry_fee_usd + exit_fee_usd

                    gross_return = (
                        (exit_px - entry_px) / entry_px
                        if side == "LONG"
                        else (entry_px - exit_px) / entry_px
                    )
                    gross_pnl_usd = notional_usdc * gross_return
                    net_pnl_usd = gross_pnl_usd - total_fees
                    net_bps = (net_pnl_usd / notional_usdc) * 10000.0

                    trades.append(
                        {
                            "date": d_str,
                            "side": side,
                            "entry": entry_px,
                            "exit": exit_px,
                            "gross_bps": gross_return * 10000.0,
                            "net_bps": net_bps,
                            "net_usd": net_pnl_usd,
                            "reason": exit_reason,
                        }
                    )
                    break

    wins = [t for t in trades if t["net_usd"] > 0]
    losses = [t for t in trades if t["net_usd"] <= 0]
    total_pnl = sum(t["net_usd"] for t in trades)
    total_bps = sum(t["net_bps"] for t in trades)
    wr = len(wins) / len(trades) * 100.0 if trades else 0.0

    print(
        f"=== US OPEN ORB + OBI CONFIRMATION (min_obi={min_obi:.2f}, SL={sl_bps:.0f} bps) ==="
    )
    print(
        f"Trades: {len(trades)} | Wins: {len(wins)} | Losses: {len(losses)} | Win Rate: {wr:.1f}%"
    )
    print(f"Net PnL: ${total_pnl:+.2f} USDC ({total_bps:+.1f} bps) on $175 notional")
    print("Trade Log:")
    for t in trades:
        print(
            f"  {t['date']} | {t['side']:5s} | In: ${t['entry']:.1f} -> Out: ${t['exit']:.1f} | Net: {t['net_bps']:+6.1f} bps (${t['net_usd']:+.2f}) | {t['reason']}"
        )
    print()


if __name__ == "__main__":
    for obi in [0.05, 0.10, 0.15, 0.20]:
        for sl in [35.0, 45.0]:
            run_confirmed_simulation(
                sl_bps=sl, min_obi=obi, trail_threshold_bps=50.0, trail_dist_bps=30.0
            )
