#!/usr/bin/env python3
"""
Institutional Trading Terminal Server for Hyperliquid Dual-Engine Bot.
Pure Python standard library — zero external dependencies, robust and fast.
Focuses on REAL executed trades, real win rate, real average win/loss, and live L1 equity.
"""

import http.server
import socketserver
import json
import os
import glob
import time
import urllib.parse
import urllib.request
from datetime import datetime, timezone

PORT = int(os.environ.get("DASHBOARD_PORT", 8088))
MAX_CONCURRENT = int(os.environ.get("HL_MAX_CONCURRENT_POSITIONS", 2))
WALLET_ADDRESS = os.environ.get("HL_WALLET_ADDRESS", "").strip()
IS_MAINNET = os.environ.get("HL_IS_MAINNET", "true").strip().lower() != "false"
HL_INFO_URL = (
    "https://api.hyperliquid.xyz/info"
    if IS_MAINNET
    else "https://api.hyperliquid-testnet.xyz/info"
)
# Clean-slate cutoff: 2026-10-29 15:30 UTC major quant architecture upgrade (5m Kalman O-U + Excursion Caps + Ratchet)
CLEAN_SLATE_EPOCH_MS = int(os.environ.get("HL_CLEAN_SLATE_EPOCH_MS", "1793287800000"))
# Scale-up cutoff: 2026-11-01 19:00 UTC ($534 USDC capital scale-up epoch)
SCALEUP_EPOCH_MS = int(os.environ.get("HL_SCALEUP_EPOCH_MS", "1793559600000"))

SEARCH_DIRS = [
    "/app/data",  # Docker container mount
    "/app",  # Alternative Docker mount
    "./hl_bot_data",  # Local dev inside hyperliquid-bot
    ".",  # Current directory
    "../hl_bot_data",
]

# 15-second in-memory cache for Hyperliquid L1 REST queries (userFills + clearinghouseState)
_HL_CACHE = {
    "last_fetch_s": 0.0,
    "account_equity": None,
    "withdrawable": None,
    "fills": [],
}


def parse_iso_time(ts_str):
    try:
        if not ts_str:
            return None
        cleaned = str(ts_str).replace("Z", "+00:00")
        return datetime.fromisoformat(cleaned)
    except Exception:
        return None


def fmt_ts_ms(ms):
    try:
        dt = datetime.fromtimestamp(float(ms) / 1000.0, tz=timezone.utc)
        return dt.strftime("%Y-%m-%d %H:%M")
    except Exception:
        return "--"


def fmt_date_slash(ms):
    try:
        dt = datetime.fromtimestamp(float(ms) / 1000.0, tz=timezone.utc)
        return dt.strftime("%Y/%m/%d")
    except Exception:
        return "--"


def fetch_hl_info(payload):
    req = urllib.request.Request(
        HL_INFO_URL,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=4.0) as resp:
        return json.loads(resp.read().decode("utf-8"))


def normalize_hl_symbol(raw):
    s = str(raw or "").strip()
    if ":" in s:
        dex, coin = s.split(":", 1)
        return f"{dex.lower()}:{coin.upper()}"
    return s.upper()


def get_cached_hl_exchange_data(wallet_addr):
    now_s = time.time()
    if not wallet_addr or len(wallet_addr) < 20:
        return _HL_CACHE

    if now_s - _HL_CACHE["last_fetch_s"] < 30.0:
        return _HL_CACHE

    _HL_CACHE["last_fetch_s"] = now_s
    try:
        ch = fetch_hl_info({"type": "clearinghouseState", "user": wallet_addr})
        nat_acct = 0.0
        nat_raw = 0.0
        nat_upnl = 0.0
        if isinstance(ch, dict):
            margin = ch.get("marginSummary") or ch.get("crossMarginSummary") or {}
            nat_acct = float(margin.get("accountValue", 0.0) or 0.0)
            nat_raw = float(margin.get("totalRawUsd", 0.0) or 0.0)
            for ap in ch.get("assetPositions") or []:
                nat_upnl += float(
                    (ap.get("position") or {}).get("unrealizedPnl", 0.0) or 0.0
                )
            withdrawable = float(ch.get("withdrawable", 0.0) or 0.0)
            if withdrawable > 0.0:
                _HL_CACHE["withdrawable"] = withdrawable

        xyz_acct = 0.0
        xyz_raw = 0.0
        xyz_upnl = 0.0
        try:
            xyz_ch = fetch_hl_info(
                {"type": "clearinghouseState", "user": wallet_addr, "dex": "xyz"}
            )
            if isinstance(xyz_ch, dict):
                xm = (
                    xyz_ch.get("marginSummary")
                    or xyz_ch.get("crossMarginSummary")
                    or {}
                )
                xyz_acct = float(xm.get("accountValue", 0.0) or 0.0)
                xyz_raw = float(xm.get("totalRawUsd", 0.0) or 0.0)
                for ap in xyz_ch.get("assetPositions") or []:
                    xyz_upnl += float(
                        (ap.get("position") or {}).get("unrealizedPnl", 0.0) or 0.0
                    )
        except Exception:
            pass

        spot_total = 0.0
        spot_hold = 0.0
        spot_ch = fetch_hl_info({"type": "spotClearinghouseState", "user": wallet_addr})
        if isinstance(spot_ch, dict):
            for b in spot_ch.get("balances", []):
                if str(b.get("coin", "")).upper() == "USDC":
                    spot_total += float(b.get("total", 0.0) or 0.0)
                    spot_hold += float(b.get("hold", 0.0) or 0.0)

        # Mirror reconcile_unified_account_equity() from src/hyperliquid.rs
        if spot_total <= 1.0 and (nat_acct > 1.0 or xyz_acct > 1.0):
            total_eq = max(0.0, nat_acct + xyz_acct)
        else:
            if nat_acct > spot_total + 1.0:
                base_eq = nat_acct
            elif (
                nat_raw > 0.0
                and spot_hold < nat_raw * 0.5
                and nat_acct < spot_total * 0.85
            ):
                base_eq = spot_total + nat_acct
            else:
                base_eq = max(spot_total + nat_upnl, nat_acct)
            xyz_contrib = (
                xyz_acct if (xyz_raw > 0.0 and spot_hold < xyz_raw * 0.5) else xyz_upnl
            )
            total_eq = max(0.0, base_eq + xyz_contrib)

        if total_eq > 0.0:
            _HL_CACHE["account_equity"] = round(total_eq, 4)
    except Exception:
        pass

    try:
        fills = fetch_hl_info({"type": "userFills", "user": wallet_addr})
        if isinstance(fills, list):
            _HL_CACHE["fills"] = fills
    except Exception:
        pass

    return _HL_CACHE


def build_real_trades_and_stats(valid_dirs, assets_telemetry, open_positions):
    """
    Builds unified real closed trade history and performance metrics from:
    1. Live Hyperliquid `userFills` API (when HL_WALLET_ADDRESS is set),
    2. Local `hl_real_trades.jsonl` persisted by the Rust bot containers,
    3. Live `account_equity` from clearinghouseState or `hl_telemetry_*.json`.
    """
    # Resolve live account equity
    live_equity = 0.0
    high_water_mark = 0.0
    for sym, t in assets_telemetry.items():
        eq = float(t.get("account_equity", 0.0) or 0.0)
        hwm = float(t.get("high_water_mark", 0.0) or 0.0)
        if eq > live_equity:
            live_equity = eq
        if hwm > high_water_mark:
            high_water_mark = hwm

    hl_live = get_cached_hl_exchange_data(WALLET_ADDRESS)
    if hl_live.get("account_equity") and float(hl_live["account_equity"]) > 0.0:
        live_equity = float(hl_live["account_equity"])

    if live_equity <= 0.0:
        live_equity = 0.0
    if high_water_mark < live_equity:
        high_water_mark = live_equity

    real_trades = []

    # 1. Reconstruct closed trades from Hyperliquid L1 `userFills` if available (post-clean-slate only)
    raw_fills = [
        f
        for f in (hl_live.get("fills") or [])
        if int(f.get("time", 0) or 0) >= CLEAN_SLATE_EPOCH_MS
    ]
    if raw_fills:
        # Sort chronological (oldest first)
        sorted_fills = sorted(raw_fills, key=lambda x: int(x.get("time", 0) or 0))
        open_tracker = {}  # coin -> {"time": ms, "px": float, "side": "LONG"/"SHORT"}
        grouped_closes = []

        for f in sorted_fills:
            coin = normalize_hl_symbol(f.get("coin", ""))
            direction = str(f.get("dir", ""))
            px = float(f.get("px", 0.0) or 0.0)
            sz = float(f.get("sz", 0.0) or 0.0)
            ts_ms = int(f.get("time", 0) or 0)
            closed_pnl = float(f.get("closedPnl", 0.0) or 0.0)
            fee = float(f.get("fee", 0.0) or 0.0)
            oid = str(f.get("oid", ""))

            if "Open" in direction:
                side = "LONG" if "Long" in direction else "SHORT"
                if coin not in open_tracker:
                    open_tracker[coin] = {
                        "time": ts_ms,
                        "px_sz": px * sz,
                        "sz": sz,
                        "side": side,
                        "fee": fee,
                    }
                else:
                    open_tracker[coin]["px_sz"] += px * sz
                    open_tracker[coin]["sz"] += sz
                    open_tracker[coin]["fee"] += fee
            elif "Close" in direction or abs(closed_pnl) > 1e-9:
                side = (
                    "LONG"
                    if "Long" in direction
                    else (
                        "SHORT"
                        if "Short" in direction
                        else (open_tracker.get(coin, {}).get("side", "LONG"))
                    )
                )
                open_info = open_tracker.get(
                    coin,
                    {
                        "time": max(0, ts_ms - 1800_000),
                        "px_sz": px * sz,
                        "sz": sz,
                        "side": side,
                        "fee": 0.0,
                    },
                )
                tot_op_sz = max(float(open_info.get("sz", 0.0) or 0.0), 1e-9)
                entry_px = (
                    float(open_info.get("px_sz", 0.0) or (px * sz)) / tot_op_sz
                    if open_info.get("sz", 0.0) > 0
                    else px
                )
                open_ms = int(open_info.get("time") or ts_ms)
                net_pnl = closed_pnl - fee - float(open_info.get("fee", 0.0) or 0.0)
                open_info["fee"] = 0.0  # consume open fee once
                if coin in open_tracker:
                    open_tracker[coin]["sz"] -= sz
                    open_tracker[coin]["px_sz"] -= entry_px * sz
                    if open_tracker[coin]["sz"] <= 1e-5:
                        del open_tracker[coin]

                # Merge partial fills belonging to the same close order or within 10s on same coin
                if (
                    grouped_closes
                    and grouped_closes[-1]["symbol"] == coin
                    and (
                        grouped_closes[-1]["oid"] == oid
                        or abs(ts_ms - grouped_closes[-1]["close_ms"]) <= 10_000
                    )
                ):
                    prev = grouped_closes[-1]
                    total_sz = prev["lots"] + sz
                    if total_sz > 0:
                        prev["exit_price"] = (
                            prev["exit_price"] * prev["lots"] + px * sz
                        ) / total_sz
                    prev["lots"] = round(total_sz, 5)
                    prev["pnl_usd"] = round(prev["pnl_usd"] + net_pnl, 4)
                    prev["close_ms"] = ts_ms
                    prev["close_date"] = fmt_ts_ms(ts_ms)
                    prev["day_key"] = fmt_date_slash(ts_ms)
                    notional = max(entry_px * prev["lots"], 1e-6)
                    prev["pnl_bps"] = round((prev["pnl_usd"] / notional) * 10000.0, 2)
                    prev["pnl_pct"] = round((prev["pnl_usd"] / notional) * 100.0, 2)
                    prev["is_win"] = prev["pnl_usd"] > 0.0
                else:
                    notional = max(entry_px * sz, 1e-6)
                    pnl_bps = (net_pnl / notional) * 10000.0
                    grouped_closes.append(
                        {
                            "oid": oid,
                            "symbol": coin,
                            "side": side,
                            "engine": "Live L1",
                            "lots": round(sz, 5),
                            "entry_price": entry_px,
                            "exit_price": px,
                            "open_ms": open_ms,
                            "close_ms": ts_ms,
                            "open_date": fmt_ts_ms(open_ms),
                            "close_date": fmt_ts_ms(ts_ms),
                            "day_key": fmt_date_slash(ts_ms),
                            "pnl_usd": round(net_pnl, 4),
                            "pnl_bps": round(pnl_bps, 2),
                            "pnl_pct": round(pnl_bps / 100.0, 2),
                            "exit_reason": "EXCHANGE_FILL",
                            "is_win": net_pnl > 0.0,
                        }
                    )

        real_trades.extend(grouped_closes)

    # 2. Load local `hl_real_trades.jsonl` written by the Rust bot
    for d in valid_dirs:
        p = os.path.join(d, "hl_real_trades.jsonl")
        if os.path.exists(p):
            try:
                with open(p, "r", encoding="utf-8") as f:
                    for line in f:
                        line = line.strip()
                        if not line:
                            continue
                        rec = json.loads(line)
                        sym = str(rec.get("symbol", "")).upper()
                        closed_dt = parse_iso_time(rec.get("closed_at"))
                        opened_dt = parse_iso_time(rec.get("opened_at"))
                        close_ms = (
                            int(closed_dt.timestamp() * 1000)
                            if closed_dt
                            else int(time.time() * 1000)
                        )
                        open_ms = (
                            int(opened_dt.timestamp() * 1000) if opened_dt else close_ms
                        )
                        if close_ms < CLEAN_SLATE_EPOCH_MS:
                            continue

                        # Deduplicate against exchange fills within 90s on same coin
                        is_dup = any(
                            t["symbol"] == sym
                            and abs(t["close_ms"] - close_ms) <= 90_000
                            for t in real_trades
                        )
                        if not is_dup:
                            pnl_usd = float(rec.get("pnl_usd", 0.0) or 0.0)
                            pnl_bps = float(rec.get("pnl_bps", 0.0) or 0.0)
                            real_trades.append(
                                {
                                    "oid": "",
                                    "symbol": sym,
                                    "side": str(rec.get("side", "LONG")).upper(),
                                    "engine": str(rec.get("engine", "EngineA")),
                                    "lots": round(
                                        float(rec.get("amount", 0.0) or 0.0), 5
                                    ),
                                    "entry_price": float(
                                        rec.get("entry_price", 0.0) or 0.0
                                    ),
                                    "exit_price": float(
                                        rec.get("exit_price", 0.0) or 0.0
                                    ),
                                    "open_ms": open_ms,
                                    "close_ms": close_ms,
                                    "open_date": fmt_ts_ms(open_ms),
                                    "close_date": fmt_ts_ms(close_ms),
                                    "day_key": fmt_date_slash(close_ms),
                                    "pnl_usd": round(pnl_usd, 4),
                                    "pnl_bps": round(pnl_bps, 2),
                                    "pnl_pct": round(pnl_bps / 100.0, 2),
                                    "exit_reason": str(
                                        rec.get("exit_reason", "CLOSED")
                                    ),
                                    "is_win": bool(rec.get("is_win", pnl_usd > 0.0)),
                                }
                            )
            except Exception:
                pass
            break

    # Sort all real trades chronologically (oldest first) to compute equity curve & stats
    real_trades.sort(key=lambda x: x["close_ms"])

    total_trades = len(real_trades)
    wins = [t for t in real_trades if t["pnl_usd"] > 0.0]
    losses = [t for t in real_trades if t["pnl_usd"] <= 0.0]
    win_count = len(wins)
    loss_count = len(losses)
    win_rate_pct = (
        round((win_count / total_trades) * 100.0, 1) if total_trades > 0 else 0.0
    )

    total_win_usd = round(sum(t["pnl_usd"] for t in wins), 2)
    total_loss_usd = round(sum(abs(t["pnl_usd"]) for t in losses), 2)
    net_realized_pnl_usd = round(sum(t["pnl_usd"] for t in real_trades), 2)

    avg_win_usd = round(total_win_usd / win_count, 2) if win_count > 0 else 0.0
    avg_win_pct = (
        round(sum(t["pnl_pct"] for t in wins) / win_count, 2) if win_count > 0 else 0.0
    )
    avg_win_bps = (
        round(sum(t["pnl_bps"] for t in wins) / win_count, 1) if win_count > 0 else 0.0
    )

    avg_loss_usd = round(total_loss_usd / loss_count, 2) if loss_count > 0 else 0.0
    avg_loss_pct = (
        round(sum(abs(t["pnl_pct"]) for t in losses) / loss_count, 2)
        if loss_count > 0
        else 0.0
    )
    avg_loss_bps = (
        round(sum(abs(t["pnl_bps"]) for t in losses) / loss_count, 1)
        if loss_count > 0
        else 0.0
    )

    profit_factor = (
        round(total_win_usd / total_loss_usd, 2)
        if total_loss_usd > 1e-4
        else (99.9 if total_win_usd > 0 else 0.0)
    )
    expectancy_bps = (
        round(sum(t["pnl_bps"] for t in real_trades) / total_trades, 1)
        if total_trades > 0
        else 0.0
    )

    # Also compute Post-$534 Scale-Up Epoch stats (Nov 01, 2026+)
    scaleup_trades = [t for t in real_trades if t["close_ms"] >= SCALEUP_EPOCH_MS]
    su_wins = [t for t in scaleup_trades if t["pnl_usd"] > 0.0]
    su_losses = [t for t in scaleup_trades if t["pnl_usd"] <= 0.0]
    su_total = len(scaleup_trades)
    scaleup_stats = {
        "total_trades_done": su_total,
        "win_count": len(su_wins),
        "loss_count": len(su_losses),
        "win_rate_pct": round((len(su_wins) / su_total) * 100.0, 1)
        if su_total > 0
        else 0.0,
        "net_realized_pnl_usd": round(sum(t["pnl_usd"] for t in scaleup_trades), 2),
        "avg_win_usd": round(sum(t["pnl_usd"] for t in su_wins) / len(su_wins), 2)
        if su_wins
        else 0.0,
        "avg_win_bps": round(sum(t["pnl_bps"] for t in su_wins) / len(su_wins), 1)
        if su_wins
        else 0.0,
        "avg_loss_usd": round(
            sum(abs(t["pnl_usd"]) for t in su_losses) / len(su_losses), 2
        )
        if su_losses
        else 0.0,
        "avg_loss_bps": round(
            sum(abs(t["pnl_bps"]) for t in su_losses) / len(su_losses), 1
        )
        if su_losses
        else 0.0,
    }

    # Compute live unrealized PnL across any active positions
    unrealized_pnl_usd = round(
        sum(
            float(p.get("unrealized_pnl_usd", 0.0) or 0.0)
            for p in open_positions.values()
            if p.get("is_filled")
        ),
        4,
    )
    account_balance_usd = round(live_equity - unrealized_pnl_usd, 2)

    # Reconstruct chronological equity curve across all real trades up to `live_equity`
    equity_curve = []
    today_str = fmt_date_slash(int(time.time() * 1000))
    if real_trades:
        start_eq = max(0.0, live_equity - sum(t["pnl_usd"] for t in real_trades))
        running_eq = start_eq
        cum_pnl = 0.0
        first_open_ms = real_trades[0]["open_ms"]
        pts = [
            {
                "time_ms": first_open_ms,
                "date": real_trades[0]["open_date"],
                "label": "Start",
                "equity": round(running_eq, 2),
                "cum_pnl_usd": 0.0,
                "pnl_usd": 0.0,
                "symbol": "START",
                "side": "--",
            }
        ]
        for t in real_trades:
            running_eq += t["pnl_usd"]
            cum_pnl += t["pnl_usd"]
            pts.append(
                {
                    "time_ms": t["close_ms"],
                    "date": t["close_date"],
                    "label": t["close_date"].split(" ")[-1]
                    if " " in t["close_date"]
                    else t["close_date"],
                    "equity": round(running_eq, 2),
                    "cum_pnl_usd": round(cum_pnl, 2),
                    "pnl_usd": t["pnl_usd"],
                    "pnl_bps": t.get("pnl_bps", 0.0),
                    "symbol": t["symbol"],
                    "side": t["side"],
                }
            )
        now_ms = int(time.time() * 1000)
        pts.append(
            {
                "time_ms": now_ms,
                "date": fmt_ts_ms(now_ms),
                "label": "Now",
                "equity": round(live_equity, 2),
                "cum_pnl_usd": round(cum_pnl, 2),
                "pnl_usd": 0.0,
                "symbol": "LIVE",
                "side": "--",
            }
        )
        equity_curve = pts
    else:
        now_ms = int(time.time() * 1000)
        equity_curve = [
            {
                "time_ms": now_ms - 86400000,
                "label": "Start",
                "date": today_str,
                "equity": round(account_balance_usd, 2),
                "cum_pnl_usd": 0.0,
                "pnl_usd": 0.0,
                "symbol": "START",
                "side": "--",
            },
            {
                "time_ms": now_ms,
                "label": "Live",
                "date": today_str,
                "equity": round(live_equity, 2),
                "cum_pnl_usd": 0.0,
                "pnl_usd": 0.0,
                "symbol": "LIVE",
                "side": "--",
            },
        ]

    high_water_mark = max(
        [live_equity] + [float(p.get("equity", 0.0) or 0.0) for p in equity_curve]
    )

    # Daily breakdown for Calendar + "Summary of the day"
    daily_summary = {}
    running_bal = live_equity - net_realized_pnl_usd
    for t in real_trades:
        dk = t["day_key"]
        running_bal += t["pnl_usd"]
        if dk not in daily_summary:
            daily_summary[dk] = {
                "date": dk,
                "ending_balance": round(running_bal, 2),
                "trades_done": 0,
                "wins": 0,
                "losses": 0,
                "net_pnl_usd": 0.0,
                "lost_usd": 0.0,
            }
        daily_summary[dk]["ending_balance"] = round(running_bal, 2)
        daily_summary[dk]["trades_done"] += 1
        daily_summary[dk]["net_pnl_usd"] = round(
            daily_summary[dk]["net_pnl_usd"] + t["pnl_usd"], 2
        )
        if t["pnl_usd"] > 0:
            daily_summary[dk]["wins"] += 1
        else:
            daily_summary[dk]["losses"] += 1
            daily_summary[dk]["lost_usd"] = round(
                daily_summary[dk]["lost_usd"] + abs(t["pnl_usd"]), 2
            )

    today_key = fmt_date_slash(int(time.time() * 1000))
    if today_key not in daily_summary:
        daily_summary[today_key] = {
            "date": today_key,
            "ending_balance": round(live_equity, 2),
            "trades_done": 0,
            "wins": 0,
            "losses": 0,
            "net_pnl_usd": round(unrealized_pnl_usd, 2),
            "lost_usd": 0.0,
        }

    today_obj = daily_summary[today_key]
    today_starting_equity = round(live_equity - today_obj["net_pnl_usd"], 2)
    daily_loss_limit_usd = (
        round(today_starting_equity * 0.0275, 2) if today_starting_equity > 0.0 else 0.0
    )  # 2.75% Cross-Margin Daily Drawdown Cap (Rank 1)
    daily_threshold_usd = (
        round(today_starting_equity - daily_loss_limit_usd, 2)
        if today_starting_equity > 0.0
        else 0.0
    )
    daily_loss_used_usd = max(0.0, -today_obj["net_pnl_usd"])
    daily_loss_left_usd = round(max(0.0, daily_loss_limit_usd - daily_loss_used_usd), 2)

    starting_equity_total = max(live_equity - net_realized_pnl_usd, 0.0)
    total_profit_pct = (
        round(
            ((live_equity - starting_equity_total) / starting_equity_total) * 100.0, 4
        )
        if starting_equity_total > 0.0
        else 0.0
    )

    return {
        "account_balance_usd": round(account_balance_usd, 2),
        "account_equity_usd": round(live_equity, 2),
        "high_water_mark_usd": round(high_water_mark, 2),
        "unrealized_pnl_usd": round(unrealized_pnl_usd, 2),
        "net_realized_pnl_usd": net_realized_pnl_usd,
        "total_profit_pct": total_profit_pct,
        "total_trades_done": total_trades,
        "win_count": win_count,
        "loss_count": loss_count,
        "win_rate_pct": win_rate_pct,
        "profit_factor": profit_factor,
        "expectancy_bps": expectancy_bps,
        "avg_win_usd": avg_win_usd,
        "avg_win_pct": avg_win_pct,
        "avg_win_bps": avg_win_bps,
        "total_win_usd": total_win_usd,
        "avg_loss_usd": avg_loss_usd,
        "avg_loss_pct": avg_loss_pct,
        "avg_loss_bps": avg_loss_bps,
        "total_loss_usd": total_loss_usd,
        "scaleup_stats": scaleup_stats,
        "today_starting_equity_usd": today_starting_equity,
        "daily_loss_limit_usd": daily_loss_limit_usd,
        "daily_threshold_usd": daily_threshold_usd,
        "daily_loss_left_usd": daily_loss_left_usd,
        "equity_curve": equity_curve,
        "daily_summary": daily_summary,
        "recent_trades": list(reversed(real_trades[-50:])),
    }


FOMC_DATES = {
    (2025, 1, 29),
    (2025, 3, 19),
    (2025, 5, 7),
    (2025, 6, 18),
    (2025, 7, 30),
    (2025, 9, 17),
    (2025, 10, 29),
    (2025, 12, 10),
    (2026, 1, 28),
    (2026, 3, 18),
    (2026, 4, 29),
    (2026, 6, 17),
    (2026, 7, 29),
    (2026, 9, 16),
    (2026, 11, 4),
    (2026, 12, 9),
    (2027, 1, 27),
    (2027, 3, 17),
    (2027, 4, 28),
    (2027, 6, 9),
    (2027, 7, 28),
    (2027, 9, 15),
    (2027, 10, 27),
    (2027, 12, 8),
}

CPI_DATES = {
    (2025, 1, 15),
    (2025, 2, 12),
    (2025, 3, 12),
    (2025, 4, 10),
    (2025, 5, 13),
    (2025, 6, 11),
    (2025, 7, 15),
    (2025, 8, 12),
    (2025, 9, 11),
    (2025, 10, 15),
    (2025, 11, 13),
    (2025, 12, 10),
    (2026, 1, 13),
    (2026, 2, 11),
    (2026, 3, 11),
    (2026, 4, 14),
    (2026, 5, 12),
    (2026, 6, 10),
    (2026, 7, 14),
    (2026, 8, 12),
    (2026, 9, 11),
    (2026, 10, 14),
    (2026, 11, 12),
    (2026, 12, 10),
    (2027, 1, 13),
    (2027, 2, 10),
    (2027, 3, 10),
    (2027, 4, 13),
    (2027, 5, 12),
    (2027, 6, 10),
    (2027, 7, 14),
    (2027, 8, 11),
    (2027, 9, 14),
    (2027, 10, 13),
    (2027, 11, 10),
    (2027, 12, 14),
}


def is_us_dst(year, month, day):
    if 4 <= month <= 10:
        return True
    if month < 3 or month > 11:
        return False
    if month == 3:
        mar1_wd = (datetime(year, 3, 1).weekday() + 1) % 7
        second_sun = 1 + ((7 - mar1_wd) % 7) + 7
        return day >= second_sun
    if month == 11:
        nov1_wd = (datetime(year, 11, 1).weekday() + 1) % 7
        first_sun = 1 + ((7 - nov1_wd) % 7)
        return day < first_sun
    return False


def evaluate_macro_shield_status(now_ms):
    import math

    dt = datetime.fromtimestamp(now_ms / 1000.0, tz=timezone.utc)
    y, m, d = dt.year, dt.month, dt.day
    wd = dt.weekday()  # 0=Mon .. 4=Fri
    edt = is_us_dst(y, m, d)

    lambda_exo = 0.0
    is_blackout = False
    event_name = "CLEAR"
    scheduled_today = None

    def check_ev(rel_hour, rel_min, hold_m, hl_m, name):
        nonlocal lambda_exo, is_blackout, event_name, scheduled_today
        rel_dt = datetime(y, m, d, rel_hour, rel_min, 0, tzinfo=timezone.utc)
        rel_ms = int(rel_dt.timestamp() * 1000)
        diff_m = (now_ms - rel_ms) / 60000.0
        scheduled_today = f"{name} ({rel_hour:02d}:{rel_min:02d} UTC)"
        lam = 0.0
        if -30.0 <= diff_m < -15.0:
            lam = 0.60 * ((diff_m + 30.0) / 15.0)
        elif -15.0 <= diff_m <= hold_m:
            lam = 1.0
        elif hold_m < diff_m <= hold_m + 5.0 * hl_m:
            raw = math.exp(-math.log(2.0) * (diff_m - hold_m) / hl_m)
            lam = raw if raw >= 0.05 else 0.0
        if lam > lambda_exo:
            lambda_exo = round(lam, 3)
            is_blackout = lam >= 0.60
            event_name = name

    if (y, m, d) in FOMC_DATES:
        check_ev(18 if edt else 19, 0, 60.0, 20.0, "US_FOMC_DECISION")
    print_hr = 12 if edt else 13
    if (y, m, d) in CPI_DATES:
        check_ev(print_hr, 30, 15.0, 15.0, "US_CPI_PRINT")
    elif wd == 4 and d <= 7:
        check_ev(print_hr, 30, 15.0, 15.0, "US_NFP_PAYROLLS")
    elif wd == 4 and d >= 24:
        check_ev(print_hr, 30, 15.0, 15.0, "US_CORE_PCE")

    return {
        "lambda_exo": lambda_exo,
        "is_hard_blackout": is_blackout,
        "event_name": event_name,
        "scheduled_today": scheduled_today or "None Today",
    }


def load_counterfactual_summary(valid_dirs):
    summary = {
        "total_evaluated": 0,
        "avoided_losses_count": 0,
        "avoided_losses_bps": 0.0,
        "missed_profits_count": 0,
        "missed_profits_bps": 0.0,
        "recent_events": [],
    }
    for d in valid_dirs:
        p = os.path.join(d, "hl_counterfactual_trades.jsonl")
        if os.path.exists(p):
            try:
                with open(p, "r", encoding="utf-8") as f:
                    lines = f.readlines()
                for line in lines:
                    line = line.strip()
                    if not line:
                        continue
                    rec = json.loads(line)
                    summary["total_evaluated"] += 1
                    out = rec.get("outcome", {})
                    dec = rec.get("decision", {})
                    v_class = str(out.get("veto_classification", "NEUTRAL_NOISE"))
                    net_bps = float(out.get("counterfactual_net_pnl_bps", 0.0) or 0.0)
                    if v_class == "ALPHA_PROTECTIVE":
                        summary["avoided_losses_count"] += 1
                        summary["avoided_losses_bps"] = round(
                            summary["avoided_losses_bps"] + abs(net_bps), 1
                        )
                    elif v_class == "ALPHA_STRANGLING":
                        summary["missed_profits_count"] += 1
                        summary["missed_profits_bps"] = round(
                            summary["missed_profits_bps"] + max(0.0, net_bps), 1
                        )

                # Keep last 25 resolved events in reverse chronological order
                recent = []
                for line in reversed(lines[-40:]):
                    line = line.strip()
                    if not line:
                        continue
                    rec = json.loads(line)
                    out = rec.get("outcome", {})
                    dec = rec.get("decision", {})
                    mkt = rec.get("market_at_decision", {})
                    recent.append(
                        {
                            "ts_utc": str(rec.get("ts_utc", ""))[:16].replace("T", " "),
                            "symbol": str(rec.get("asset", "PERP")),
                            "side": str(rec.get("side", "BUY")),
                            "engine": str(rec.get("engine", "EngineA")),
                            "primary_veto": str(dec.get("primary_veto", "VETO")),
                            "ref_price": float(mkt.get("reference_price", 0.0) or 0.0),
                            "exit_price": float(out.get("exit_price", 0.0) or 0.0),
                            "net_bps": round(
                                float(
                                    out.get("counterfactual_net_pnl_bps", 0.0) or 0.0
                                ),
                                1,
                            ),
                            "mfe_bps": round(float(out.get("mfe_bps", 0.0) or 0.0), 1),
                            "mae_bps": round(float(out.get("mae_bps", 0.0) or 0.0), 1),
                            "classification": str(
                                out.get("veto_classification", "NEUTRAL_NOISE")
                            ),
                        }
                    )
                    if len(recent) >= 25:
                        break
                summary["recent_events"] = recent
            except Exception:
                pass
            break
    return summary


def load_recent_candles(valid_dirs, symbol, limit=140):
    """
    Loads the last `limit` 1m candles from `hl_history_{symbol}.jsonl` on the local NAS/SSD.
    Enforces strictly ascending unique UTCTimestamp in integer seconds as required by TradingView Lightweight Charts.
    """
    sym_low = str(symbol or "BTC").strip().lower()
    for d in valid_dirs:
        p = os.path.join(d, f"hl_history_{sym_low}.jsonl")
        if os.path.exists(p):
            try:
                with open(p, "rb") as f:
                    f.seek(0, os.SEEK_END)
                    fsize = f.tell()
                    read_sz = min(fsize, limit * 520)
                    f.seek(max(0, fsize - read_sz), os.SEEK_SET)
                    raw_lines = f.read().decode("utf-8", errors="ignore").splitlines()
                by_sec = {}
                for line in raw_lines[-limit * 2 :]:
                    line = line.strip()
                    if not line.startswith("{"):
                        continue
                    rec = json.loads(line)
                    ts_sec = int(int(rec.get("timestamp_ms", 0) or 0) // 1000)
                    o = float(rec.get("open", 0.0) or 0.0)
                    h = float(rec.get("high", 0.0) or 0.0)
                    l = float(rec.get("low", 0.0) or 0.0)
                    c = float(rec.get("close", 0.0) or 0.0)
                    if ts_sec > 0 and c > 0 and h >= l > 0:
                        by_sec[ts_sec] = {
                            "time": ts_sec,
                            "open": o,
                            "high": h,
                            "low": l,
                            "close": c,
                            "volume": float(rec.get("volume", 0.0) or 0.0),
                        }
                sorted_bars = [by_sec[k] for k in sorted(by_sec.keys())]
                return sorted_bars[-limit:]
            except Exception:
                pass
    return []


def format_prometheus_metrics(data):
    """
    Exposes Prometheus / OpenMetrics text format with strictly bounded low-cardinality labels
    (symbol, state, horizon, stage) per `Open-Source Trading Dashboard Research.md` Section 3.1.
    """
    lines = [
        "# HELP markov_polo_account_equity_usd Total account equity in USDC",
        "# TYPE markov_polo_account_equity_usd gauge",
    ]
    perf = data.get("real_performance", {})
    lines.append(
        f"markov_polo_account_equity_usd {float(perf.get('account_equity_usd', 0.0))}"
    )
    lines.append(
        f"markov_polo_high_water_mark_usd {float(perf.get('high_water_mark_usd', 0.0))}"
    )
    lines.append(
        f"markov_polo_net_realized_pnl_usd {float(perf.get('net_realized_pnl_usd', 0.0))}"
    )
    lines.append(f"markov_polo_win_rate_pct {float(perf.get('win_rate_pct', 0.0))}")
    lines.append(
        f'markov_polo_trades_total{{outcome="win"}} {int(perf.get("win_count", 0))}'
    )
    lines.append(
        f'markov_polo_trades_total{{outcome="loss"}} {int(perf.get("loss_count", 0))}'
    )

    macro = data.get("macro_shield", {})
    lines.append(
        f"markov_polo_macro_hazard_lambda {float(macro.get('lambda_exo', 0.0))}"
    )
    lines.append(
        f"markov_polo_macro_blackout_active {1 if macro.get('is_hard_blackout') else 0}"
    )

    ccb = data.get("cluster_circuit_breaker", {})
    lines.append(
        f"markov_polo_cluster_consecutive_losses {int(ccb.get('cluster_consecutive_losses', 0))}"
    )

    for sym, a in (data.get("assets") or {}).items():
        if sym not in (
            "BTC",
            "ETH",
            "SOL",
            "AVAX",
            "HYPE",
            "NEAR",
            "XRP",
            "SUI",
            "LINK",
            "ZEC",
            "xyz:NVDA",
            "xyz:TSLA",
            "xyz:XYZ100",
            "xyz:GOLD",
            "xyz:SILVER",
        ):
            continue
        lines.append(
            f'markov_polo_asset_price{{symbol="{sym}"}} {float(a.get("price", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_ou_half_life_minutes{{symbol="{sym}"}} {float(a.get("half_life_m", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_z_score{{symbol="{sym}",window="15m"}} {float(a.get("z_15", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_z_score{{symbol="{sym}",window="60m"}} {float(a.get("z_60", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_l2_obi{{symbol="{sym}",horizon="instant"}} {float(a.get("obi_instant", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_l2_obi{{symbol="{sym}",horizon="1s"}} {float(a.get("obi_1s", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_l2_obi{{symbol="{sym}",horizon="5s"}} {float(a.get("obi_5s", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_hmm_probability{{symbol="{sym}",state="consolidation"}} {float(a.get("p_consolidation", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_hmm_probability{{symbol="{sym}",state="trend"}} {float(a.get("p_trend", 0.0) or 0.0)}'
        )
        lines.append(
            f'markov_polo_hmm_probability{{symbol="{sym}",state="shock"}} {float(a.get("p_shock", 0.0) or 0.0)}'
        )

    return "\n".join(lines) + "\n"


def get_telemetry_data():
    now_utc = datetime.now(timezone.utc)
    now_ms = int(time.time() * 1000)

    results = {
        "server_time_ms": now_ms,
        "max_concurrent": MAX_CONCURRENT,
        "data_dir": "/app/data",
        "assets": {},
        "armed_signals": {},
        "controllers": {},
        "positions": {},
        "order_intents": {},
        "cluster_leases": [],
        "cluster_lease": None,
        "circuit_breakers": {},
        "funnels": {},
        "real_performance": {},
    }

    valid_dirs = [d for d in SEARCH_DIRS if os.path.isdir(d)]
    if not valid_dirs:
        valid_dirs = ["."]

    # 1. Load telemetry, armed signals, controllers, and funnels for all symbols (BTC, ETH, SOL, AVAX)
    for d in valid_dirs:
        for path in glob.glob(os.path.join(d, "hl_telemetry_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_telemetry_", "").replace(".json", "")
            if sym not in results["assets"]:
                try:
                    with open(path, "r", encoding="utf-8") as f:
                        results["assets"][sym] = json.load(f)
                        results["data_dir"] = d
                except Exception:
                    pass
        for path in glob.glob(os.path.join(d, "hl_hmm_arm_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_hmm_arm_", "").replace(".json", "")
            if sym not in results["armed_signals"]:
                try:
                    with open(path, "r", encoding="utf-8") as f:
                        results["armed_signals"][sym] = json.load(f)
                except Exception:
                    pass
        for path in glob.glob(os.path.join(d, "hl_controller_state_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_controller_state_", "").replace(".json", "")
            if sym not in results["controllers"]:
                try:
                    with open(path, "r", encoding="utf-8") as f:
                        results["controllers"][sym] = json.load(f)
                except Exception:
                    pass
        for path in glob.glob(os.path.join(d, "hl_funnel_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_funnel_", "").replace(".json", "")
            if sym not in results["funnels"]:
                try:
                    with open(path, "r", encoding="utf-8") as f:
                        results["funnels"][sym] = json.load(f)
                except Exception:
                    pass

    # 2. Load cluster leases
    active_leases = []
    seen_lease_symbols = set()

    for d in valid_dirs:
        lock_file = os.path.join(d, "hl_portfolio_cluster.lock")
        if os.path.exists(lock_file):
            try:
                with open(lock_file, "r", encoding="utf-8") as f:
                    data = json.load(f)
                if (
                    isinstance(data, dict)
                    and "leases" in data
                    and isinstance(data["leases"], dict)
                ):
                    for sym, l in data["leases"].items():
                        if sym not in seen_lease_symbols:
                            exp_dt = parse_iso_time(l.get("expires_at"))
                            is_active = (exp_dt is None) or (now_utc < exp_dt)
                            l["is_active"] = is_active
                            l["symbol"] = sym
                            if is_active:
                                active_leases.append(l)
                                seen_lease_symbols.add(sym)
                elif isinstance(data, dict) and "owner" in data:
                    sym = data.get("owner")
                    if sym and sym not in seen_lease_symbols:
                        exp_dt = parse_iso_time(data.get("expires_at"))
                        is_active = (exp_dt is None) or (now_utc < exp_dt)
                        data["is_active"] = is_active
                        data["symbol"] = sym
                        if is_active:
                            active_leases.append(data)
                            seen_lease_symbols.add(sym)
            except Exception:
                pass

    results["cluster_leases"] = active_leases
    results["cluster_lease"] = active_leases[0] if active_leases else None

    # 3. Load active position states (hl_hmm_state_*.json)
    for d in valid_dirs:
        for path in glob.glob(os.path.join(d, "hl_hmm_state_*.json")):
            try:
                with open(path, "r", encoding="utf-8") as f:
                    state_map = json.load(f)
                if isinstance(state_map, dict):
                    for sym, pos in state_map.items():
                        if not isinstance(pos, dict):
                            continue
                        lifecycle = pos.get("lifecycle", "Active")
                        if lifecycle != "Active":
                            continue
                        is_occupied = (
                            pos.get("is_filled", False)
                            or float(pos.get("amount", 0.0) or 0.0) > 0.0
                            or bool(pos.get("order_id"))
                            or bool(pos.get("tp_order_id"))
                            or bool(pos.get("client_order_id"))
                        )
                        if is_occupied and sym not in results["positions"]:
                            asset_telem = results["assets"].get(sym, {})
                            cur_px = asset_telem.get("price")
                            entry_px = float(pos.get("entry_price", 0.0) or 0.0)
                            amount = float(pos.get("amount", 0.0) or 0.0)
                            side = str(pos.get("side", "LONG")).upper()

                            pos_copy = dict(pos)
                            pos_copy["symbol"] = sym
                            if cur_px and entry_px > 0 and amount > 0:
                                cur_px = float(cur_px)
                                if side == "LONG":
                                    pnl_usd = (cur_px - entry_px) * amount
                                    pnl_bps = ((cur_px - entry_px) / entry_px) * 10000.0
                                else:
                                    pnl_usd = (entry_px - cur_px) * amount
                                    pnl_bps = ((entry_px - cur_px) / entry_px) * 10000.0
                                pos_copy["unrealized_pnl_usd"] = round(pnl_usd, 4)
                                pos_copy["unrealized_pnl_bps"] = round(pnl_bps, 2)
                                pos_copy["current_price"] = cur_px
                            results["positions"][sym] = pos_copy
            except Exception:
                pass

    # 4. Load Circuit Breakers & Cross-Margin Cluster Circuit Breaker
    results["cluster_circuit_breaker"] = {}
    for d in valid_dirs:
        ccb_path = os.path.join(d, "hl_cluster_circuit_breaker.json")
        if os.path.exists(ccb_path) and not results["cluster_circuit_breaker"]:
            try:
                with open(ccb_path, "r", encoding="utf-8") as f:
                    results["cluster_circuit_breaker"] = json.load(f)
            except Exception:
                pass
        for pattern, prefix in (
            ("hl_hmm_cb_*.json", "hl_hmm_cb_"),
            ("hl_cb_*.json", "hl_cb_"),
        ):
            for path in glob.glob(os.path.join(d, pattern)):
                base = os.path.basename(path)
                sym = base.replace(prefix, "").replace(".json", "")
                if sym not in results["circuit_breakers"]:
                    try:
                        with open(path, "r", encoding="utf-8") as f:
                            results["circuit_breakers"][sym] = json.load(f)
                    except Exception:
                        pass

    # 5. Compute Real Trade History, Real Win Rate, Real Avg Win/Loss & Real Equity Curve
    results["real_performance"] = build_real_trades_and_stats(
        valid_dirs, results["assets"], results["positions"]
    )

    # 6. Compute Macro Shield & Counterfactual Summary
    results["macro_shield"] = evaluate_macro_shield_status(now_ms)
    results["counterfactual_summary"] = load_counterfactual_summary(valid_dirs)

    return results


class DashboardHandler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        clean_path = parsed.path

        if clean_path == "/api/telemetry":
            data = get_telemetry_data()
            content = json.dumps(data).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Cache-Control", "no-cache, no-store, must-revalidate")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)
            return

        if clean_path == "/api/candles":
            qs = urllib.parse.parse_qs(parsed.query)
            sym = (qs.get("symbol") or ["BTC"])[0].upper()
            valid_dirs = [d for d in SEARCH_DIRS if os.path.isdir(d)] or ["."]
            candles = load_recent_candles(valid_dirs, sym, limit=140)
            content = json.dumps({"symbol": sym, "candles": candles}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Access-Control-Allow-Origin", "*")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)
            return

        if clean_path == "/metrics":
            data = get_telemetry_data()
            content = format_prometheus_metrics(data).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Content-Length", str(len(content)))
            self.end_headers()
            self.wfile.write(content)
            return

        if clean_path in ("/", "/index.html"):
            html_paths = [
                "/app/dashboard/index.html",
                "./dashboard/index.html",
                os.path.join(os.path.dirname(__file__), "index.html"),
            ]
            for p in html_paths:
                if os.path.exists(p):
                    with open(p, "rb") as f:
                        content = f.read()
                    self.send_response(200)
                    self.send_header("Content-Type", "text/html; charset=utf-8")
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("Content-Length", str(len(content)))
                    self.end_headers()
                    self.wfile.write(content)
                    return

        super().do_GET()


class ThreadingHTTPServer(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == "__main__":
    with ThreadingHTTPServer(("0.0.0.0", PORT), DashboardHandler) as httpd:
        print(
            f"🚀 [DASHBOARD] Institutional Trading Terminal listening on http://0.0.0.0:{PORT}"
        )
        httpd.serve_forever()
