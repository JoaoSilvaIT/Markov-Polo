#!/usr/bin/env python3
"""
Institutional Trading Terminal Server for Hyperliquid Dual-Engine Bot.
Pure Python standard library — zero external dependencies, robust and fast.
"""

import http.server
import socketserver
import json
import os
import glob
import time
import urllib.parse
from datetime import datetime, timezone

PORT = int(os.environ.get("DASHBOARD_PORT", 8088))
MAX_CONCURRENT = int(os.environ.get("HL_MAX_CONCURRENT_POSITIONS", 2))

SEARCH_DIRS = [
    "/app/data",                      # Docker container mount
    "/app",                           # Alternative Docker mount
    "./hl_bot_data",                  # Local dev inside hyperliquid-bot
    ".",                              # Current directory
    "../hl_bot_data",
]

def parse_iso_time(ts_str):
    try:
        if not ts_str:
            return None
        # Replace trailing Z with +00:00 if present
        cleaned = ts_str.replace("Z", "+00:00")
        return datetime.fromisoformat(cleaned)
    except Exception:
        return None

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
    }

    valid_dirs = [d for d in SEARCH_DIRS if os.path.isdir(d)]
    if not valid_dirs:
        valid_dirs = ["."]

    # 1. Load telemetry, armed signals, and controller states for all symbols (BTC, ETH, SOL, AVAX)
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

    # 2. Load cluster leases (supports multi-lease map and legacy single lease)
    active_leases = []
    seen_lease_symbols = set()

    for d in valid_dirs:
        lock_file = os.path.join(d, "hl_portfolio_cluster.lock")
        if os.path.exists(lock_file):
            try:
                with open(lock_file, "r", encoding="utf-8") as f:
                    data = json.load(f)
                
                # Check if multi-lease map: {"leases": { "BTC": {...}, ... }}
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
                # Check if legacy single lease: {"owner": "BTC", ...}
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

        # Also search for cluster_lease.json or hl_cluster_lease_*.json
        fallback_files = glob.glob(os.path.join(d, "cluster_lease*.json")) + glob.glob(os.path.join(d, "hl_cluster_lease_*.json"))
        for fb in fallback_files:
            try:
                with open(fb, "r", encoding="utf-8") as f:
                    fb_data = json.load(f)
                if isinstance(fb_data, dict) and "owner" in fb_data:
                    sym = fb_data.get("owner")
                    if sym and sym not in seen_lease_symbols:
                        exp_dt = parse_iso_time(fb_data.get("expires_at"))
                        is_active = (exp_dt is None) or (now_utc < exp_dt)
                        fb_data["is_active"] = is_active
                        fb_data["symbol"] = sym
                        if is_active:
                            active_leases.append(fb_data)
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
                        # Verify occupancy: is_filled or amount > 0 or has active order
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
                            # Compute live unrealized PnL if spot price available
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

    # 4. Load Order Intents (hl_order_intent_*.json)
    for d in valid_dirs:
        for path in glob.glob(os.path.join(d, "hl_order_intent_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_order_intent_", "").replace(".json", "")
            if sym not in results["order_intents"]:
                try:
                    with open(path, "r", encoding="utf-8") as f:
                        intent = json.load(f)
                    status = intent.get("status", "")
                    if status not in ("Completed", "Aborted"):
                        results["order_intents"][sym] = intent
                except Exception:
                    pass

    # 5. Load Circuit Breakers (hl_hmm_cb_*.json, hl_cb_*.json, or hl_circuit_breaker.json)
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
        legacy_cb = os.path.join(d, "hl_circuit_breaker.json")
        if os.path.exists(legacy_cb) and "GLOBAL" not in results["circuit_breakers"]:
            try:
                with open(legacy_cb, "r", encoding="utf-8") as f:
                    results["circuit_breakers"]["GLOBAL"] = json.load(f)
            except Exception:
                pass

    # 6. Load counterfactual records
    cf_path = None
    for d in valid_dirs:
        cand = os.path.join(d, "hl_counterfactual_trades.jsonl")
        if os.path.exists(cand):
            cf_path = cand
            break

    cf_stats = {
        "total_evaluated": 0,
        "total_records": 0,
        "alpha_protective_count": 0,
        "alpha_protective_avoided_bps": 0.0,
        "alpha_strangling_count": 0,
        "alpha_strangling_missed_bps": 0.0,
        "neutral_noise_count": 0,
        "touch_only_count": 0,
        "trade_through_count": 0,
        "fill_ratio_pct": 0.0,
        "veto_breakdown": {},
        "recent_records": [],
    }

    if cf_path and os.path.exists(cf_path):
        try:
            with open(cf_path, "r", encoding="utf-8") as f:
                lines = [line.strip() for line in f if line.strip()]
            cf_stats["total_records"] = len(lines)
            cf_stats["total_evaluated"] = len(lines)

            parsed_records = []
            for line in lines[-1000:]:
                try:
                    rec = json.loads(line)
                    parsed_records.append(rec)
                except Exception:
                    pass

            for rec in parsed_records:
                outcome = rec.get("outcome", {}) or {}
                v_class = str(outcome.get("veto_classification", "")).upper()
                pnl = float(outcome.get("counterfactual_net_pnl_bps", 0.0) or 0.0)
                fill_m = rec.get("fill_model", {}) or {}

                is_strict = bool(fill_m.get("trade_through_filled") or (outcome.get("fill_status") == "trade_through_confirmed"))
                is_touch = bool(fill_m.get("touch_only") or (outcome.get("fill_status") in ("touch_only", "trade_through_confirmed")) or is_strict)

                if is_touch:
                    cf_stats["touch_only_count"] += 1
                if is_strict:
                    cf_stats["trade_through_count"] += 1

                if "PROTECTIVE" in v_class:
                    cf_stats["alpha_protective_count"] += 1
                    cf_stats["alpha_protective_avoided_bps"] += abs(pnl)
                elif "STRANGLING" in v_class:
                    cf_stats["alpha_strangling_count"] += 1
                    cf_stats["alpha_strangling_missed_bps"] += pnl
                else:
                    cf_stats["neutral_noise_count"] += 1

                veto_reason = rec.get("decision", {}).get("primary_veto", "OTHER")
                if veto_reason not in cf_stats["veto_breakdown"]:
                    cf_stats["veto_breakdown"][veto_reason] = {"count": 0, "protective": 0, "strangling": 0, "net_bps": 0.0}
                cf_stats["veto_breakdown"][veto_reason]["count"] += 1
                if "PROTECTIVE" in v_class:
                    cf_stats["veto_breakdown"][veto_reason]["protective"] += 1
                elif "STRANGLING" in v_class:
                    cf_stats["veto_breakdown"][veto_reason]["strangling"] += 1
                cf_stats["veto_breakdown"][veto_reason]["net_bps"] += pnl

            total_touches = cf_stats["touch_only_count"]
            if total_touches > 0:
                cf_stats["fill_ratio_pct"] = round((cf_stats["trade_through_count"] / total_touches) * 100.0, 1)

            cf_stats["recent_records"] = parsed_records[-20:][::-1]
        except Exception as e:
            print(f"Error reading counterfactual records from {cf_path}: {e}")

    results["counterfactual"] = cf_stats
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

        # Serve index.html for root
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
