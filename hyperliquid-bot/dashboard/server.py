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

PORT = int(os.environ.get("DASHBOARD_PORT", 8080))

# Potential data directories to search
SEARCH_DIRS = [
    "/app/data",                      # Docker container mount
    "/app",                           # Alternative Docker mount
    "./hl_bot_data",                  # Local dev inside hyperliquid-bot
    ".",                              # Current directory
    "../hl_bot_data",
]

def find_data_dir():
    for d in SEARCH_DIRS:
        if os.path.exists(d) and (glob.glob(os.path.join(d, "hl_telemetry_*.json")) or glob.glob(os.path.join(d, "hl_controller_state_*.json"))):
            return d
    return SEARCH_DIRS[0]

def get_telemetry_data():
    data_dir = find_data_dir()
    results = {
        "server_time_ms": int(time.time() * 1000),
        "data_dir": data_dir,
        "assets": {},
        "cluster_lease": None,
        "circuit_breaker": None,
    }

    # Load telemetry for all symbols
    for path in glob.glob(os.path.join(data_dir, "hl_telemetry_*.json")):
        base = os.path.basename(path)
        sym = base.replace("hl_telemetry_", "").replace(".json", "")
        try:
            with open(path, "r") as f:
                results["assets"][sym] = json.load(f)
        except Exception:
            pass

    # Load cluster lease if present
    lease_path = os.path.join(data_dir, "cluster_lease.json")
    if os.path.exists(lease_path):
        try:
            with open(lease_path, "r") as f:
                results["cluster_lease"] = json.load(f)
        except Exception:
            pass

    # Load circuit breaker if present
    cb_path = os.path.join(data_dir, "hl_circuit_breaker.json")
    if os.path.exists(cb_path):
        try:
            with open(cb_path, "r") as f:
                results["circuit_breaker"] = json.load(f)
        except Exception:
            pass

    return results

class DashboardHandler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/api/telemetry":
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
        if self.path == "/" or self.path == "/index.html":
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
