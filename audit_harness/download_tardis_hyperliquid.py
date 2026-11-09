#!/usr/bin/env python3
"""
Tardis.dev Free Dataset Downloader for Hyperliquid L1 Perpetuals.
Downloads open-access full-day datasets (1st of every month, no API key needed):
- trades (tick-by-tick trades with microsecond timestamps)
- book_snapshot_5 (5-level L2 depth snapshots)
- derivative_ticker (funding rates, open interest, index/mark prices)
"""

import os
import sys
import urllib.request
import urllib.error
from concurrent.futures import ThreadPoolExecutor, as_completed

BASE_URL = "https://datasets.tardis.dev/v1/hyperliquid"
ASSETS = ["BTC", "ETH", "SOL", "AVAX", "HYPE", "NEAR", "XRP", "SUI", "LINK"]
CHANNELS = ["trades", "book_snapshot_5", "derivative_ticker"]
DATES = [
    ("2025", "01", "01"),
    ("2025", "02", "01"),
]

DATA_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "data")


def download_file(channel: str, year: str, month: str, day: str, symbol: str) -> str:
    filename = f"hyperliquid_{channel}_{year}-{month}-{day}_{symbol}.csv.gz"
    out_path = os.path.join(DATA_DIR, filename)

    if os.path.exists(out_path) and os.path.getsize(out_path) > 1024:
        return f"ALREADY_EXISTS: {filename}"

    url = f"{BASE_URL}/{channel}/{year}/{month}/{day}/{symbol}.csv.gz"
    req = urllib.request.Request(url, headers={"User-Agent": "NautilusAuditBot/1.0"})

    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            if resp.status == 200:
                with open(out_path, "wb") as f:
                    f.write(resp.read())
                size_mb = os.path.getsize(out_path) / (1024 * 1024)
                return f"DOWNLOADED: {filename} ({size_mb:.2f} MB)"
            else:
                return f"FAILED (Status {resp.status}): {url}"
    except urllib.error.HTTPError as e:
        return f"HTTP_ERROR {e.code}: {url}"
    except Exception as e:
        return f"ERROR: {url} -> {e}"


def main():
    os.makedirs(DATA_DIR, exist_ok=True)
    tasks = []
    print(
        f"📥 [TARDIS DOWNLOADER] Preparing downloads for {len(ASSETS)} assets across {len(DATES)} monthly dates..."
    )

    with ThreadPoolExecutor(max_workers=8) as executor:
        for y, m, d in DATES:
            for asset in ASSETS:
                for ch in CHANNELS:
                    tasks.append(executor.submit(download_file, ch, y, m, d, asset))

        total = len(tasks)
        success = 0
        for i, future in enumerate(as_completed(tasks), 1):
            res = future.result()
            if "DOWNLOADED" in res or "ALREADY_EXISTS" in res:
                success += 1
                if i % 5 == 0 or i == total:
                    print(f"   [{i}/{total}] {res}")
            else:
                print(f"   [{i}/{total}] {res}")

    print(
        f"\n✅ [TARDIS DOWNLOAD COMPLETE] Successfully acquired {success}/{total} datasets in {DATA_DIR}"
    )


if __name__ == "__main__":
    main()
