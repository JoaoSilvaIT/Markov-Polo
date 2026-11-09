#!/usr/bin/env python3
"""
Multi-Asset NautilusTrader Audit Runner for Hyperliquid L1 Perpetuals.
Iterates through all 9 assets: BTC, ETH, SOL, AVAX, HYPE, NEAR, XRP, SUI, LINK.
Executes independent event-driven simulation with exact FIFO queue matching and VIP0 fees.
Outputs consolidated cross-asset institutional metrics.
"""

import os
import sys
import contextlib
import io
from decimal import Decimal

from nautilus_trader.backtest.engine import BacktestEngine, BacktestEngineConfig
from nautilus_trader.config import LoggingConfig
from nautilus_trader.model.enums import AccountType, OmsType, BookType, OrderStatus
from nautilus_trader.model.identifiers import Venue, InstrumentId
from nautilus_trader.model.objects import Currency, Money
from nautilus_trader.persistence.catalog import ParquetDataCatalog

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BASE_DIR)

from markov_polo_nautilus import MarkovPoloNautilusStrategy, MarkovPoloNautilusConfig

CATALOG_DIR = os.path.join(BASE_DIR, "catalog")
ALL_ASSETS = ["BTC", "ETH", "SOL", "AVAX", "HYPE", "NEAR", "XRP", "SUI", "LINK"]

# Asset-specific calibration matching Markov-Polo parameters
ASSET_PARAMS = {
    "BTC": {
        "horizon": 1500,
        "z_entry": 1.70,
        "stall": 180,
        "notional": 175.0,
        "beta": 0.55,
    },
    "ETH": {
        "horizon": 1200,
        "z_entry": 1.68,
        "stall": 150,
        "notional": 175.0,
        "beta": 0.56,
    },
    "SOL": {
        "horizon": 800,
        "z_entry": 1.65,
        "stall": 120,
        "notional": 133.85,
        "beta": 0.58,
    },
    "AVAX": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
    "HYPE": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
    "NEAR": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
    "XRP": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
    "SUI": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
    "LINK": {
        "horizon": 600,
        "z_entry": 1.65,
        "stall": 100,
        "notional": 115.0,
        "beta": 0.60,
    },
}


def audit_single_asset(symbol: str, catalog: ParquetDataCatalog) -> dict:
    inst_str = f"{symbol}-USDC-PERP.HYPERLIQUID"
    inst_list = catalog.instruments(instrument_ids=[inst_str])
    if not inst_list:
        return {"symbol": symbol, "status": "NO_DATA"}

    instrument = inst_list[0]
    quotes = catalog.quote_ticks(instrument_ids=[inst_str])
    trades = catalog.trade_ticks(instrument_ids=[inst_str])

    if len(quotes) == 0 or len(trades) == 0:
        return {"symbol": symbol, "status": "INSUFFICIENT_DATA"}

    engine_config = BacktestEngineConfig(
        trader_id=f"AUDIT-{symbol}",
        logging=LoggingConfig(
            log_level="ERROR"
        ),  # Suppress internal engine logs for clean table
    )
    engine = BacktestEngine(config=engine_config)

    engine.add_venue(
        venue=Venue("HYPERLIQUID"),
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=Currency.from_str("USDC"),
        starting_balances=[Money(536.0, Currency.from_str("USDC"))],
        default_leverage=Decimal("3.0"),
        book_type=BookType.L1_MBP,
        queue_position=True,
        trade_execution=True,
        use_reduce_only=True,
    )
    engine.add_instrument(instrument)
    engine.add_data(quotes)
    engine.add_data(trades)

    params = ASSET_PARAMS.get(symbol, ASSET_PARAMS["SOL"])
    strat_config = MarkovPoloNautilusConfig(
        instrument_id=inst_str,
        target_notional_usdc=params["notional"],
        min_notional_usdc=22.0,
        ou_horizon_ticks=params["horizon"],
        base_entry_sigma=params["z_entry"],
        hard_disaster_stop_bps=40.0,
        beta_overshoot=params["beta"],
        stall_trigger_ticks=params["stall"],
    )
    strategy = MarkovPoloNautilusStrategy(config=strat_config)
    engine.add_strategy(strategy)

    # Run simulation suppressing engine stdout
    with contextlib.redirect_stdout(io.StringIO()):
        engine.run()

    account = engine.portfolio.account(Venue("HYPERLIQUID"))
    final_balance = (
        float(account.balance_total(Currency.from_str("USDC"))) if account else 536.0
    )
    net_pnl = final_balance - 536.0

    orders = engine.cache.orders()
    filled_orders = [o for o in orders if o.status == OrderStatus.FILLED]
    positions = engine.cache.positions()
    closed_pos = [p for p in positions if p.is_closed]

    win_count = sum(1 for p in closed_pos if float(p.realized_pnl) > 0.0)
    win_rate = (win_count / len(closed_pos) * 100.0) if closed_pos else 0.0

    return {
        "symbol": symbol,
        "status": "OK",
        "quotes": len(quotes),
        "trades": len(trades),
        "orders_submitted": len(orders),
        "orders_filled": len(filled_orders),
        "closed_positions": len(closed_pos),
        "win_rate": win_rate,
        "net_pnl_usdc": net_pnl,
    }


def main():
    print(
        "=========================================================================================="
    )
    print(
        "🔬 [NAUTILUS TRADER] INSTITUTIONAL AUDIT SUITE FOR MARKOV-POLO (HYPERLIQUID L1)"
    )
    print("   Data source : Tardis.dev L2 orderbook quotes & microsecond trades")
    print("   Engine      : NautilusTrader 1.231.0 with strict FIFO queue emulation")
    print(
        "   Account     : $536.00 USDC, 3x Cross Margin, VIP0 Maker: 1.44 bps, Taker: 4.32 bps"
    )
    print(
        "=========================================================================================="
    )

    catalog = ParquetDataCatalog(CATALOG_DIR)
    results = []

    for sym in ALL_ASSETS:
        print(f"▶️  Auditing {sym:<4}...", end="", flush=True)
        res = audit_single_asset(sym, catalog)
        results.append(res)
        pnl_str = f"${res['net_pnl_usdc']:+.2f}" if res["status"] == "OK" else "N/A"
        print(f" Done. PnL: {pnl_str} | Closed Pos: {res.get('closed_positions', 0)}")

    print("\n" + "=" * 90)
    print(
        f"{'Asset':<6} | {'Ticks Ingested':<16} | {'Orders':<14} | {'Positions':<10} | {'Win Rate':<9} | {'Net PnL (USDC)':<15}"
    )
    print("-" * 90)

    total_net_pnl = 0.0
    total_positions = 0
    total_wins = 0

    for r in results:
        if r["status"] == "OK":
            ticks_str = f"{r['quotes'] + r['trades']:,}"
            orders_str = f"{r['orders_filled']}/{r['orders_submitted']}"
            pos_str = f"{r['closed_positions']}"
            wr_str = f"{r['win_rate']:.1f}%" if r["closed_positions"] > 0 else "N/A"
            pnl_val = r["net_pnl_usdc"]
            pnl_color = f"+${pnl_val:.2f}" if pnl_val >= 0 else f"-${abs(pnl_val):.2f}"

            total_net_pnl += pnl_val
            total_positions += r["closed_positions"]
            if r["closed_positions"] > 0:
                total_wins += int(round(r["win_rate"] * r["closed_positions"] / 100.0))

            print(
                f"{r['symbol']:<6} | {ticks_str:<16} | {orders_str:<14} | {pos_str:<10} | {wr_str:<9} | {pnl_color:<15}"
            )
        else:
            print(f"{r['symbol']:<6} | {r['status']}")

    print("=" * 90)
    overall_wr = (total_wins / total_positions * 100.0) if total_positions > 0 else 0.0
    tot_color = (
        f"+${total_net_pnl:.2f}"
        if total_net_pnl >= 0
        else f"-${abs(total_net_pnl):.2f}"
    )
    print(f"📊 [TOTAL AUDIT PORTFOLIO SUMMARY]")
    print(f"   Total Completed Positions : {total_positions}")
    print(f"   Aggregated Win Rate       : {overall_wr:.1f}%")
    print(f"   Net Realized Portfolio PnL: {tot_color} USDC (on $536.00 USDC account)")
    print("=" * 90)


if __name__ == "__main__":
    main()
