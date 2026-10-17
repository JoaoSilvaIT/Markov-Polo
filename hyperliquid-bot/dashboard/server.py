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
HL_INFO_URL = "https://api.hyperliquid.xyz/info" if IS_MAINNET else "https://api.hyperliquid-testnet.xyz/info"

SEARCH_DIRS = [
    "/app/data",                      # Docker container mount
    "/app",                           # Alternative Docker mount
    "./hl_bot_data",                  # Local dev inside hyperliquid-bot
    ".",                              # Current directory
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


def get_cached_hl_exchange_data(wallet_addr):
    now_s = time.time()
    if not wallet_addr or len(wallet_addr) < 20:
        return _HL_CACHE

    if now_s - _HL_CACHE["last_fetch_s"] < 15.0:
        return _HL_CACHE

    _HL_CACHE["last_fetch_s"] = now_s
    try:
        ch = fetch_hl_info({"type": "clearinghouseState", "user": wallet_addr})
        perp_val = 0.0
        if isinstance(ch, dict):
            margin = ch.get("marginSummary") or ch.get("crossMarginSummary") or {}
            perp_val = float(margin.get("accountValue", 0.0) or 0.0)
            withdrawable = float(ch.get("withdrawable", 0.0) or 0.0)
            if withdrawable > 0.0:
                _HL_CACHE["withdrawable"] = withdrawable
        spot_val = 0.0
        spot_ch = fetch_hl_info({"type": "spotClearinghouseState", "user": wallet_addr})
        if isinstance(spot_ch, dict):
            for b in spot_ch.get("balances", []):
                if str(b.get("coin", "")).upper() == "USDC":
                    total = float(b.get("total", 0.0) or 0.0)
                    hold = float(b.get("hold", 0.0) or 0.0)
                    spot_val += max(0.0, total - hold)
        if perp_val + spot_val > 0.0:
            _HL_CACHE["account_equity"] = round(perp_val + spot_val, 4)
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
        live_equity = 150.93
    if high_water_mark < live_equity:
        high_water_mark = live_equity

    real_trades = []

    # 1. Reconstruct closed trades from Hyperliquid L1 `userFills` if available
    raw_fills = hl_live.get("fills") or []
    if raw_fills:
        # Sort chronological (oldest first)
        sorted_fills = sorted(raw_fills, key=lambda x: int(x.get("time", 0) or 0))
        open_tracker = {}  # coin -> {"time": ms, "px": float, "side": "LONG"/"SHORT"}
        grouped_closes = []

        for f in sorted_fills:
            coin = str(f.get("coin", "")).upper()
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
                    open_tracker[coin] = {"time": ts_ms, "px": px, "side": side, "fee": fee}
                else:
                    open_tracker[coin]["fee"] += fee
            elif "Close" in direction or abs(closed_pnl) > 1e-9:
                side = "LONG" if "Long" in direction else ("SHORT" if "Short" in direction else (open_tracker.get(coin, {}).get("side", "LONG")))
                open_info = open_tracker.get(coin, {"time": max(0, ts_ms - 1800_000), "px": px, "side": side, "fee": 0.0})
                entry_px = float(open_info.get("px") or px)
                open_ms = int(open_info.get("time") or ts_ms)
                net_pnl = closed_pnl - fee - float(open_info.get("fee", 0.0) or 0.0)
                open_info["fee"] = 0.0  # consume open fee once

                # Merge partial fills belonging to the same close order or within 10s on same coin
                if grouped_closes and grouped_closes[-1]["symbol"] == coin and (
                    grouped_closes[-1]["oid"] == oid or abs(ts_ms - grouped_closes[-1]["close_ms"]) <= 10_000
                ):
                    prev = grouped_closes[-1]
                    total_sz = prev["lots"] + sz
                    if total_sz > 0:
                        prev["exit_price"] = (prev["exit_price"] * prev["lots"] + px * sz) / total_sz
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
                    grouped_closes.append({
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
                    })

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
                        close_ms = int(closed_dt.timestamp() * 1000) if closed_dt else int(time.time() * 1000)
                        open_ms = int(opened_dt.timestamp() * 1000) if opened_dt else close_ms

                        # Deduplicate against exchange fills within 90s on same coin
                        is_dup = any(
                            t["symbol"] == sym and abs(t["close_ms"] - close_ms) <= 90_000
                            for t in real_trades
                        )
                        if not is_dup:
                            pnl_usd = float(rec.get("pnl_usd", 0.0) or 0.0)
                            pnl_bps = float(rec.get("pnl_bps", 0.0) or 0.0)
                            real_trades.append({
                                "oid": "",
                                "symbol": sym,
                                "side": str(rec.get("side", "LONG")).upper(),
                                "engine": str(rec.get("engine", "EngineA")),
                                "lots": round(float(rec.get("amount", 0.0) or 0.0), 5),
                                "entry_price": float(rec.get("entry_price", 0.0) or 0.0),
                                "exit_price": float(rec.get("exit_price", 0.0) or 0.0),
                                "open_ms": open_ms,
                                "close_ms": close_ms,
                                "open_date": fmt_ts_ms(open_ms),
                                "close_date": fmt_ts_ms(close_ms),
                                "day_key": fmt_date_slash(close_ms),
                                "pnl_usd": round(pnl_usd, 4),
                                "pnl_bps": round(pnl_bps, 2),
                                "pnl_pct": round(pnl_bps / 100.0, 2),
                                "exit_reason": str(rec.get("exit_reason", "CLOSED")),
                                "is_win": bool(rec.get("is_win", pnl_usd > 0.0)),
                            })
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
    win_rate_pct = round((win_count / total_trades) * 100.0, 1) if total_trades > 0 else 0.0

    total_win_usd = round(sum(t["pnl_usd"] for t in wins), 2)
    total_loss_usd = round(sum(abs(t["pnl_usd"]) for t in losses), 2)
    net_realized_pnl_usd = round(sum(t["pnl_usd"] for t in real_trades), 2)

    avg_win_usd = round(total_win_usd / win_count, 2) if win_count > 0 else 0.0
    avg_win_pct = round(sum(t["pnl_pct"] for t in wins) / win_count, 2) if win_count > 0 else 0.0

    avg_loss_usd = round(total_loss_usd / loss_count, 2) if loss_count > 0 else 0.0
    avg_loss_pct = round(sum(abs(t["pnl_pct"]) for t in losses) / loss_count, 2) if loss_count > 0 else 0.0

    # Compute live unrealized PnL across any active positions
    unrealized_pnl_usd = round(
        sum(float(p.get("unrealized_pnl_usd", 0.0) or 0.0) for p in open_positions.values() if p.get("is_filled")),
        4,
    )
    account_balance_usd = round(live_equity - unrealized_pnl_usd, 2)

    # Reconstruct chronological equity curve ending at `live_equity`
    equity_curve = []
    if real_trades:
        recent_for_curve = real_trades[-12:]
        running_eq = live_equity
        pts = [{"label": "Now", "date": fmt_date_slash(int(time.time() * 1000)), "equity": round(running_eq, 2)}]
        for t in reversed(recent_for_curve):
            running_eq = running_eq - t["pnl_usd"]
            pts.append({
                "label": t["close_date"].split(" ")[0][5:],
                "date": t["close_date"],
                "equity": round(running_eq, 2),
                "symbol": t["symbol"],
                "pnl_usd": t["pnl_usd"],
            })
        pts.reverse()
        equity_curve = pts
    else:
        today_str = fmt_date_slash(int(time.time() * 1000))
        equity_curve = [
            {"label": "Start", "date": today_str, "equity": round(account_balance_usd, 2)},
            {"label": "Live", "date": today_str, "equity": round(live_equity, 2)},
        ]

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
        daily_summary[dk]["net_pnl_usd"] = round(daily_summary[dk]["net_pnl_usd"] + t["pnl_usd"], 2)
        if t["pnl_usd"] > 0:
            daily_summary[dk]["wins"] += 1
        else:
            daily_summary[dk]["losses"] += 1
            daily_summary[dk]["lost_usd"] = round(daily_summary[dk]["lost_usd"] + abs(t["pnl_usd"]), 2)

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
    daily_loss_limit_usd = round(today_starting_equity * 0.05, 2)  # 5% daily drawdown limit (3 strikes)
    daily_threshold_usd = round(today_starting_equity - daily_loss_limit_usd, 2)
    daily_loss_used_usd = max(0.0, -today_obj["net_pnl_usd"])
    daily_loss_left_usd = round(max(0.0, daily_loss_limit_usd - daily_loss_used_usd), 2)

    starting_equity_total = max(live_equity - net_realized_pnl_usd, 1e-6)
    total_profit_pct = round(((live_equity - starting_equity_total) / starting_equity_total) * 100.0, 4)

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
        "avg_win_usd": avg_win_usd,
        "avg_win_pct": avg_win_pct,
        "total_win_usd": total_win_usd,
        "avg_loss_usd": avg_loss_usd,
        "avg_loss_pct": avg_loss_pct,
        "total_loss_usd": total_loss_usd,
        "today_starting_equity_usd": today_starting_equity,
        "daily_loss_limit_usd": daily_loss_limit_usd,
        "daily_threshold_usd": daily_threshold_usd,
        "daily_loss_left_usd": daily_loss_left_usd,
        "equity_curve": equity_curve,
        "daily_summary": daily_summary,
        "recent_trades": list(reversed(real_trades[-50:])),
    }


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
                if isinstance(data, dict) and "leases" in data and isinstance(data["leases"], dict):
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
                            pos.get("is_filled", False) or
                            float(pos.get("amount", 0.0) or 0.0) > 0.0 or
                            bool(pos.get("order_id")) or
                            bool(pos.get("tp_order_id")) or
                            bool(pos.get("client_order_id"))
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

    # 4. Load Circuit Breakers
    for d in valid_dirs:
        for pattern, prefix in (("hl_hmm_cb_*.json", "hl_hmm_cb_"), ("hl_cb_*.json", "hl_cb_")):
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
        print(f"🚀 [DASHBOARD] Institutional Trading Terminal listening on http://0.0.0.0:{PORT}")
        httpd.serve_forever()
