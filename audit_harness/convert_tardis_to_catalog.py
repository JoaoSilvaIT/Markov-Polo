#!/usr/bin/env python3
"""
Converts downloaded Tardis Hyperliquid L2 datasets into NautilusTrader's Parquet Data Catalog.
Outputs:
- CryptoPerpetual instruments with exact Hyperliquid lot sizes and VIP0 fee specs.
- High-resolution TradeTick stream.
- High-resolution QuoteTick stream from top-of-book (level 0) depth.
"""

import os
import glob
import pandas as pd
from decimal import Decimal
from nautilus_trader.model.instruments import CryptoPerpetual
from nautilus_trader.model.identifiers import InstrumentId, Symbol, TradeId
from nautilus_trader.model.objects import Price, Quantity, Currency
from nautilus_trader.model.data import TradeTick, QuoteTick
from nautilus_trader.model.enums import AggressorSide
from nautilus_trader.persistence.catalog import ParquetDataCatalog

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
DATA_DIR = os.path.join(BASE_DIR, "data")
CATALOG_DIR = os.path.join(BASE_DIR, "catalog")

INSTRUMENT_SPECS = {
    "BTC": {"tick": "1.0", "min_sz": "0.00001"},
    "ETH": {"tick": "0.10", "min_sz": "0.0001"},
    "SOL": {"tick": "0.01", "min_sz": "0.01"},
    "AVAX": {"tick": "0.001", "min_sz": "0.01"},
    "HYPE": {"tick": "0.001", "min_sz": "0.01"},
    "NEAR": {"tick": "0.0001", "min_sz": "0.1"},
    "XRP": {"tick": "0.0001", "min_sz": "1"},
    "SUI": {"tick": "0.0001", "min_sz": "0.1"},
    "LINK": {"tick": "0.001", "min_sz": "0.1"},
}


def build_hyperliquid_instruments():
    instruments = {}
    for sym, spec in INSTRUMENT_SPECS.items():
        inst_id = InstrumentId.from_str(f"{sym}-USDC-PERP.HYPERLIQUID")
        p = Price.from_str(spec["tick"])
        s = Quantity.from_str(spec["min_sz"])
        inst = CryptoPerpetual(
            instrument_id=inst_id,
            raw_symbol=Symbol(sym),
            base_currency=Currency.from_str(sym),
            quote_currency=Currency.from_str("USDC"),
            settlement_currency=Currency.from_str("USDC"),
            is_inverse=False,
            price_precision=p.precision,
            size_precision=s.precision,
            price_increment=p,
            size_increment=s,
            ts_event=0,
            ts_init=0,
            lot_size=s,
            min_quantity=s,
            margin_init=Decimal("0.0333"),  # 3x target leverage
            margin_maint=Decimal("0.0166"),  # 50% maint margin
            maker_fee=Decimal("0.000144"),  # 1.44 bps
            taker_fee=Decimal("0.000432"),  # 4.32 bps
        )
        instruments[sym] = inst
    return instruments


def convert_trades(catalog, instruments, target_dates=None, max_rows_per_file=50_000):
    trade_files = glob.glob(os.path.join(DATA_DIR, "hyperliquid_trades_*.csv.gz"))
    trade_files.sort()
    print(f"📊 [CONVERTER] Processing {len(trade_files)} trade datasets...")

    for fpath in trade_files:
        basename = os.path.basename(fpath)
        parts = basename.replace(".csv.gz", "").split("_")
        date_str = parts[2]
        sym = parts[3]

        if target_dates and date_str not in target_dates:
            continue
        if sym not in instruments:
            continue

        inst = instruments[sym]
        px_dec = inst.price_precision
        sz_dec = inst.size_precision

        print(f"   ↳ Ingesting trades: {sym} ({date_str})...")
        df = pd.read_csv(fpath)
        if max_rows_per_file and len(df) > max_rows_per_file:
            df = df.head(max_rows_per_file)
        df.sort_values("timestamp", inplace=True)

        ts_arr = (df["timestamp"].values * 1_000).astype("int64")
        px_arr = df["price"].values.astype("float64")
        sz_arr = df["amount"].values.astype("float64")
        sides_arr = df["side"].values
        ids_arr = df["id"].values

        ticks = []
        n = len(df)
        for i in range(n):
            side = (
                AggressorSide.BUYER if sides_arr[i] == "buy" else AggressorSide.SELLER
            )
            p = Price(px_arr[i], precision=px_dec)
            s = Quantity(sz_arr[i], precision=sz_dec)
            tid = TradeId(str(ids_arr[i]))
            ts = int(ts_arr[i])

            ticks.append(
                TradeTick(
                    instrument_id=inst.id,
                    price=p,
                    size=s,
                    aggressor_side=side,
                    trade_id=tid,
                    ts_event=ts,
                    ts_init=ts,
                )
            )

            if len(ticks) >= 50_000:
                ticks.sort(key=lambda x: x.ts_init)
                catalog.write_data(ticks)
                ticks.clear()

        if ticks:
            ticks.sort(key=lambda x: x.ts_init)
            catalog.write_data(ticks)
            ticks.clear()


def convert_quotes(catalog, instruments, target_dates=None, max_rows_per_file=50_000):
    book_files = glob.glob(
        os.path.join(DATA_DIR, "hyperliquid_book_snapshot_5_*.csv.gz")
    )
    book_files.sort()
    print(f"📖 [CONVERTER] Processing {len(book_files)} order book depth datasets...")

    for fpath in book_files:
        basename = os.path.basename(fpath)
        parts = basename.replace(".csv.gz", "").split("_")
        # hyperliquid_book_snapshot_5_{date}_{sym} -> parts[4] is date, parts[5] is sym
        date_str = parts[4]
        sym = parts[5]

        if target_dates and date_str not in target_dates:
            continue
        if sym not in instruments:
            continue

        inst = instruments[sym]
        px_dec = inst.price_precision
        sz_dec = inst.size_precision

        print(f"   ↳ Ingesting top-of-book quotes: {sym} ({date_str})...")
        df = pd.read_csv(fpath)
        if max_rows_per_file and len(df) > max_rows_per_file:
            df = df.head(max_rows_per_file)
        df.sort_values("timestamp", inplace=True)

        ts_arr = (df["timestamp"].values * 1_000).astype("int64")
        bid_px_arr = df["bids[0].price"].values.astype("float64")
        bid_sz_arr = df["bids[0].amount"].values.astype("float64")
        ask_px_arr = df["asks[0].price"].values.astype("float64")
        ask_sz_arr = df["asks[0].amount"].values.astype("float64")

        quotes = []
        n = len(df)
        for i in range(n):
            b_px = bid_px_arr[i]
            a_px = ask_px_arr[i]
            if b_px <= 0 or a_px <= 0 or b_px >= a_px:
                continue

            ts = int(ts_arr[i])
            quotes.append(
                QuoteTick(
                    instrument_id=inst.id,
                    bid_price=Price(b_px, precision=px_dec),
                    ask_price=Price(a_px, precision=px_dec),
                    bid_size=Quantity(bid_sz_arr[i], precision=sz_dec),
                    ask_size=Quantity(ask_sz_arr[i], precision=sz_dec),
                    ts_event=ts,
                    ts_init=ts,
                )
            )

            if len(quotes) >= 50_000:
                quotes.sort(key=lambda x: x.ts_init)
                catalog.write_data(quotes)
                quotes.clear()

        if quotes:
            quotes.sort(key=lambda x: x.ts_init)
            catalog.write_data(quotes)
            quotes.clear()


def main():
    os.makedirs(CATALOG_DIR, exist_ok=True)
    catalog = ParquetDataCatalog(CATALOG_DIR)
    instruments = build_hyperliquid_instruments()

    print("🏛️ [CATALOG SETUP] Registering Hyperliquid instruments in Parquet catalog...")
    for inst in instruments.values():
        catalog.write_data([inst])
    print(f"✅ Registered {len(instruments)} instruments.")

    target_dates = ["2025-01-01", "2025-02-01"]
    convert_trades(
        catalog, instruments, target_dates=target_dates, max_rows_per_file=50_000
    )
    convert_quotes(
        catalog, instruments, target_dates=target_dates, max_rows_per_file=50_000
    )
    print(f"\n🎉 [PARQUET CONVERSION COMPLETE] Data catalog populated at {CATALOG_DIR}")


if __name__ == "__main__":
    main()
