#!/usr/bin/env python3
"""
Institutional Historical Market Data Ingestion from Binance Data Vision.
Downloads 1-minute OHLCV klines from the earliest available inception date (or a chosen start year)
up to the latest available month for all portfolio perpetual assets, converts them into
Hyperliquid-compatible `HistoricalMarketBarRecord` JSONL format, merges seamlessly with native
Hyperliquid L1 records, and deduplicates chronologically.
"""

import argparse
import csv
import datetime
import io
import json
import math
import os
import sys
import time
import urllib.request
import xml.etree.ElementTree as ET
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed

BASE_S3_URL = "https://s3-ap-northeast-1.amazonaws.com/data.binance.vision"

SZ_DECIMALS_MAP = {
    "BTC": 5,
    "ETH": 4,
    "SOL": 2,
    "AVAX": 2,
    "XRP": 0,
    "NEAR": 1,
    "SUI": 1,
    "LINK": 1,
}

TICK_SIZES = {
    "BTC": 1.0,
    "ETH": 0.10,
    "SOL": 0.01,
    "AVAX": 0.001,
    "XRP": 0.0001,
    "NEAR": 0.0001,
    "SUI": 0.00001,
    "LINK": 0.001,
}


def eff_tick(price, sz_dec):
    if price <= 0:
        return 0.01
    p_dec = max(0, 6 - sz_dec)
    mag = int(math.floor(math.log10(abs(price))))
    sig_dec = 5 - mag - 1
    eff_dec = max(0, min(p_dec, sig_dec))
    return 10.0 ** (-eff_dec)


def list_available_monthly_zips(asset_pair):
    prefix = f"data/futures/um/monthly/klines/{asset_pair}/1m/"
    url = f"{BASE_S3_URL}?prefix={prefix}"
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            root = ET.fromstring(r.read())
            keys = [
                elem.text
                for elem in root.findall(
                    ".//{http://s3.amazonaws.com/doc/2006-03-01/}Key"
                )
                if elem.text and elem.text.endswith(".zip")
            ]
            return sorted(keys)
    except Exception as e:
        print(f"❌ Failed to list S3 prefix for {asset_pair}: {e}", file=sys.stderr)
        return []


def download_and_parse_zip(s3_key, symbol, sz_dec, retries=3):
    url = f"{BASE_S3_URL}/{s3_key}"
    fname = s3_key.split("/")[-1]
    req = urllib.request.Request(url, headers={"User-Agent": "Mozilla/5.0"})
    content = None

    for attempt in range(retries):
        try:
            with urllib.request.urlopen(req, timeout=25) as r:
                content = r.read()
                break
        except Exception as e:
            if attempt == retries - 1:
                print(f"❌ [{symbol}] Failed to download {fname}: {e}", file=sys.stderr)
                return []
            time.sleep(1.0 + attempt * 1.5)

    if not content:
        return []

    records = []
    try:
        with zipfile.ZipFile(io.BytesIO(content)) as z:
            csv_name = z.namelist()[0]
            with z.open(csv_name) as f:
                reader = csv.reader(io.TextIOWrapper(f, encoding="utf-8"))
                for row in reader:
                    if not row or not row[0].isdigit():
                        continue  # Skip header or empty rows
                    ts = int(row[0])
                    o = float(row[1])
                    h = float(row[2])
                    l = float(row[3])
                    c = float(row[4])
                    v = float(row[5])
                    if c > 0 and h >= l > 0:
                        tick = eff_tick(c, sz_dec)
                        records.append(
                            {
                                "symbol": symbol,
                                "timestamp_ms": ts,
                                "open": o,
                                "high": h,
                                "low": l,
                                "close": c,
                                "volume": v,
                                "best_bid": round(c - 0.5 * tick, 6),
                                "best_ask": round(c + 0.5 * tick, 6),
                                "obi_instant": 0.0,
                                "obi_1s": 0.0,
                                "obi_5s": 0.0,
                                "funding_1h": 0.0000125,
                                "open_interest": 0.0,
                                "mark_px": c,
                                "oracle_px": c,
                            }
                        )
    except Exception as e:
        print(f"⚠️ [{symbol}] Error parsing {fname}: {e}", file=sys.stderr)

    return records


def get_existing_file_bounds(filepath):
    """Returns (min_ts, max_ts, line_count) from an existing sorted JSONL file

    without loading all records into RAM.
    """
    if not os.path.exists(filepath) or os.path.getsize(filepath) == 0:
        return None, None, 0

    min_ts = None
    max_ts = None
    count = 0
    with open(filepath, "r", encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            count += 1
            if min_ts is None:
                try:
                    min_ts = int(json.loads(line)["timestamp_ms"])
                except Exception:
                    pass

    if min_ts is None:
        return None, None, 0

    with open(filepath, "rb") as f:
        f.seek(0, os.SEEK_END)
        size = f.tell()
        buffer_size = min(size, 8192)
        f.seek(size - buffer_size)
        lines = f.read().decode("utf-8", errors="ignore").splitlines()
        for line in reversed(lines):
            if line.strip():
                try:
                    max_ts = int(json.loads(line)["timestamp_ms"])
                    break
                except Exception:
                    continue

    return min_ts, max_ts, count


def process_asset(symbol, start_year, max_workers=10, force=False):
    binance_pair = f"{symbol}USDT"
    sz_dec = SZ_DECIMALS_MAP.get(symbol, 2)
    output_file = f"hl_history_{symbol.lower()}.jsonl"

    print(
        f"\n🚀 [{symbol}] Querying Binance Data Vision archives for {binance_pair}..."
    )
    all_keys = list_available_monthly_zips(binance_pair)
    if not all_keys:
        print(f"⚠️ [{symbol}] No archive files found on Binance Data Vision.")
        return 0

    existing_min_ts, existing_max_ts, existing_count = get_existing_file_bounds(
        output_file
    )
    if existing_min_ts is not None:
        first_dt = datetime.datetime.fromtimestamp(
            existing_min_ts / 1000, datetime.timezone.utc
        )
        last_dt = datetime.datetime.fromtimestamp(
            existing_max_ts / 1000, datetime.timezone.utc
        )
        print(
            f"📂 [{symbol}] Found existing archive: {existing_count:,} bars ({first_dt.strftime('%Y-%m-%d')} -> {last_dt.strftime('%Y-%m-%d')})"
        )

    # Determine which archives are needed
    needed_keys = []
    skipped_count = 0
    for k in all_keys:
        fname = k.split("/")[-1]
        parts = fname.replace(".zip", "").split("-")
        try:
            year = int(parts[-2])
            month = int(parts[-1])
        except Exception:
            continue

        if year < start_year:
            continue

        m_start = datetime.datetime(year, month, 1, tzinfo=datetime.timezone.utc)
        m_start_ts = int(m_start.timestamp() * 1000)
        if month == 12:
            m_end = datetime.datetime(year + 1, 1, 1, tzinfo=datetime.timezone.utc)
        else:
            m_end = datetime.datetime(year, month + 1, 1, tzinfo=datetime.timezone.utc)
        m_end_ts = int(m_end.timestamp() * 1000)

        # Check if entire month is already covered
        if existing_min_ts is not None and not force:
            # If this is the earliest available archive for the asset and existing_min_ts falls within it:
            if k == all_keys[0] and m_start_ts <= existing_min_ts < m_end_ts:
                skipped_count += 1
                continue
            if (
                m_start_ts >= existing_min_ts
                and m_end_ts <= existing_max_ts + 86_400_000
            ):
                skipped_count += 1
                continue

        needed_keys.append(k)

    if not needed_keys:
        print(
            f"✅ [{symbol}] All monthly archives (>= {start_year}) are already fully covered! ({existing_count:,} bars). Skipping download."
        )
        return existing_count

    print(
        f"📦 [{symbol}] Need {len(needed_keys)} monthly archives ({needed_keys[0].split('/')[-1]} to {needed_keys[-1].split('/')[-1]}; skipped {skipped_count} existing)"
    )

    # Concurrently download and parse only missing monthly zips
    downloaded_records = []
    t0 = time.time()
    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_to_key = {
            executor.submit(download_and_parse_zip, k, symbol, sz_dec): k
            for k in needed_keys
        }
        for future in as_completed(future_to_key):
            k = future_to_key[future]
            fname = k.split("/")[-1]
            try:
                recs = future.result()
                downloaded_records.extend(recs)
                print(
                    f"   ↳ [{symbol}] Processed {fname} ({len(recs):,} bars)",
                    flush=True,
                )
            except Exception as e:
                print(f"❌ [{symbol}] Error downloading {fname}: {e}", file=sys.stderr)

    elapsed = time.time() - t0
    print(
        f"⏱️ [{symbol}] Downloaded {len(downloaded_records):,} new bars in {elapsed:.1f}s"
    )

    # Deduplicate downloaded records by timestamp
    new_by_ts = {}
    for r in downloaded_records:
        new_by_ts[int(r["timestamp_ms"])] = r
    del downloaded_records

    tmp_file = f"{output_file}.tmp.{os.getpid()}"
    last_written_ts = -1
    total_written = 0

    if existing_min_ts is not None and not force:
        prepend_ts = sorted([ts for ts in new_by_ts if ts < existing_min_ts])
        append_ts = sorted([ts for ts in new_by_ts if ts > existing_max_ts])
        print(
            f"🔀 [{symbol}] Merging {len(prepend_ts):,} prepended bars + {existing_count:,} existing native bars + {len(append_ts):,} appended bars..."
        )

        with open(tmp_file, "w", encoding="utf-8") as out_f:
            # 1. Write prepended bars
            for ts in prepend_ts:
                if ts > last_written_ts:
                    out_f.write(json.dumps(new_by_ts[ts]) + "\n")
                    last_written_ts = ts
                    total_written += 1

            # 2. Stream existing bars (preserves native Hyperliquid L2 book / funding / OI)
            with open(output_file, "r", encoding="utf-8") as in_f:
                for line in in_f:
                    if not line.strip():
                        continue
                    try:
                        rec = json.loads(line)
                        ts = int(rec["timestamp_ms"])
                        if ts > last_written_ts:
                            out_f.write(line if line.endswith("\n") else line + "\n")
                            last_written_ts = ts
                            total_written += 1
                    except Exception:
                        pass

            # 3. Write appended bars
            for ts in append_ts:
                if ts > last_written_ts:
                    out_f.write(json.dumps(new_by_ts[ts]) + "\n")
                    last_written_ts = ts
                    total_written += 1
    else:
        sorted_ts = sorted(new_by_ts.keys())
        print(
            f"💾 [{symbol}] Writing {len(sorted_ts):,} bars chronologically to {output_file}..."
        )
        with open(tmp_file, "w", encoding="utf-8") as out_f:
            for ts in sorted_ts:
                out_f.write(json.dumps(new_by_ts[ts]) + "\n")
                total_written += 1

    del new_by_ts
    import gc

    gc.collect()

    os.replace(tmp_file, output_file)

    first_ts, last_ts, final_count = get_existing_file_bounds(output_file)
    first_dt = datetime.datetime.fromtimestamp(first_ts / 1000, datetime.timezone.utc)
    last_dt = datetime.datetime.fromtimestamp(last_ts / 1000, datetime.timezone.utc)
    span_days = (last_ts - first_ts) / (86400 * 1000)
    print(
        f"✅ [{symbol}] Ready! Total {final_count:,} bars spanning {span_days:.1f} days ({first_dt.strftime('%Y-%m-%d')} to {last_dt.strftime('%Y-%m-%d')})"
    )
    return final_count


def main():
    parser = argparse.ArgumentParser(
        description="Pull historical 1m Binance futures klines"
    )
    parser.add_argument(
        "--start-year",
        type=int,
        default=2020,
        help="Earliest year to pull from (default: 2020 for maximum historical coverage)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Force re-download of all archives even if present",
    )
    parser.add_argument(
        "--assets",
        type=str,
        default="BTC,ETH,SOL,AVAX,NEAR,XRP,SUI,LINK",
        help="Comma-separated symbols to process (default: BTC,ETH,SOL,AVAX,NEAR,XRP,SUI,LINK)",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=12,
        help="Number of concurrent worker threads (default: 12)",
    )
    args = parser.parse_args()

    symbols = [s.strip().upper() for s in args.assets.split(",") if s.strip()]
    print(
        "================================================================================"
    )
    print(
        f"📥 [BINANCE DATA INGESTION] Pulling 1m historical futures klines (Start Year: {args.start_year})"
    )
    print(f"   Assets: {', '.join(symbols)}")
    print(
        "   Note: HYPE is a Hyperliquid-native token; its native history will be preserved."
    )
    print(
        "================================================================================"
    )

    grand_total_bars = 0
    t_start = time.time()
    for sym in symbols:
        total = process_asset(sym, args.start_year, args.workers, args.force)
        grand_total_bars += total

    total_time = time.time() - t_start
    print(
        "\n================================================================================"
    )
    print(
        f"🎉 [INGESTION COMPLETE] Successfully ingested {grand_total_bars:,} total 1m bars across {len(symbols)} assets in {total_time:.1f}s!"
    )
    print(
        "================================================================================"
    )


if __name__ == "__main__":
    main()
