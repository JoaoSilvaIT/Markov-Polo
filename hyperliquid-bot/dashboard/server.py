#!/usr/bin/env python3
"""
Lightweight Quantitative Dashboard Server for Hyperliquid Dual-Engine Bot.
Zero external dependencies (pure Python standard library).
"""

import http.server
import socketserver
import json
import os
import glob
import time
import urllib.parse

PORT = int(os.environ.get("DASHBOARD_PORT", 8088))

# Potential data directories to search
SEARCH_DIRS = [
    "/app/data",                      # Docker container mount
    "/app",                           # Alternative Docker mount
    "./hl_bot_data",                  # Local dev inside hyperliquid-bot
    ".",                              # Current directory
    "../hl_bot_data",
]

def get_telemetry_data():
    results = {
        "server_time_ms": int(time.time() * 1000),
        "data_dir": "/app/data",
        "assets": {},
        "cluster_lease": None,
        "circuit_breaker": None,
    }

    valid_dirs = [d for d in SEARCH_DIRS if os.path.isdir(d)]
    if not valid_dirs:
        valid_dirs = ["."]

    # Load telemetry for all symbols across search directories
    for d in valid_dirs:
        for path in glob.glob(os.path.join(d, "hl_telemetry_*.json")):
            base = os.path.basename(path)
            sym = base.replace("hl_telemetry_", "").replace(".json", "")
            if sym not in results["assets"]:
                try:
                    with open(path, "r") as f:
                        results["assets"][sym] = json.load(f)
                        results["data_dir"] = d
                except Exception:
                    pass

    # Load cluster lease if present
    for d in valid_dirs:
        lease_path = os.path.join(d, "cluster_lease.json")
        if os.path.exists(lease_path):
            try:
                with open(lease_path, "r") as f:
                    results["cluster_lease"] = json.load(f)
                    break
            except Exception:
                pass

    # Load circuit breaker if present
    for d in valid_dirs:
        cb_path = os.path.join(d, "hl_circuit_breaker.json")
        if os.path.exists(cb_path):
            try:
                with open(cb_path, "r") as f:
                    results["circuit_breaker"] = json.load(f)
                    break
            except Exception:
                pass

    # Load counterfactual records if present
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
        "touch_only": 0,
        "touch_only_count": 0,
        "fill_strict_through": 0,
        "trade_through_count": 0,
        "fill_ratio_pct": 0.0,
        "veto_breakdown": {},
        "recent_records": [],
    }

    if cf_path and os.path.exists(cf_path):
        try:
            with open(cf_path, "r") as f:
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
                        cf_stats["touch_only"] += 1
                    if is_strict:
                        cf_stats["trade_through_count"] += 1
                        cf_stats["fill_strict_through"] += 1

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
                    cf_stats["fill_ratio_pct"] = (cf_stats["fill_strict_through"] / total_touches) * 100.0

                cf_stats["recent_records"] = parsed_records[-15:][::-1]
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

if __name__ == "__main__":
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("0.0.0.0", PORT), DashboardHandler) as httpd:
        print(f"🚀 [DASHBOARD] Quantitative Web Terminal listening on http://0.0.0.0:{PORT}")
        httpd.serve_forever()

