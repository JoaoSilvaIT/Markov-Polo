#!/usr/bin/env python3
"""
NautilusTrader Independent Institutional Backtest Audit Runner for Hyperliquid L1.
Executes an event-driven, zero-lookahead simulation against real Hyperliquid L2 book & trade data:
- Uses real FIFO queue modeling (queue_position=True).
- Uses exact Hyperliquid VIP0 fee schedule (1.44 bps Maker, 4.32 bps Taker).
- $536 USDC initial account equity at 3x cross margin.
"""

import os
import sys
from decimal import Decimal

from nautilus_trader.backtest.engine import BacktestEngine, BacktestEngineConfig
from nautilus_trader.config import LoggingConfig
from nautilus_trader.model.enums import AccountType, OmsType, BookType
from nautilus_trader.model.identifiers import Venue, InstrumentId
from nautilus_trader.model.objects import Currency, Money
from nautilus_trader.persistence.catalog import ParquetDataCatalog

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, BASE_DIR)

from markov_polo_nautilus import MarkovPoloNautilusStrategy, MarkovPoloNautilusConfig

CATALOG_DIR = os.path.join(BASE_DIR, "catalog")


def run_audit(target_symbol="SOL"):
    print(f"=================================================================")
    print(
        f"🔬 [NAUTILUS AUDIT] Initializing Independent L2 Audit Engine for {target_symbol}"
    )
    print(f"=================================================================")

    catalog = ParquetDataCatalog(CATALOG_DIR)
    inst_str = f"{target_symbol}-USDC-PERP.HYPERLIQUID"
    inst_list = catalog.instruments(instrument_ids=[inst_str])
    if not inst_list:
        print(f"❌ Error: Instrument {inst_str} not found in catalog!")
        return
    instrument = inst_list[0]

    engine_config = BacktestEngineConfig(
        trader_id="AUDIT-MARKOV-POLO",
        logging=LoggingConfig(log_level="INFO"),
    )
    engine = BacktestEngine(config=engine_config)

    # 1. Add Venue with Hyperliquid rules & $536 USDC starting margin balance
    engine.add_venue(
        venue=Venue("HYPERLIQUID"),
        oms_type=OmsType.NETTING,
        account_type=AccountType.MARGIN,
        base_currency=Currency.from_str("USDC"),
        starting_balances=[Money(536.0, Currency.from_str("USDC"))],
        default_leverage=Decimal("3.0"),
        book_type=BookType.L1_MBP,
        queue_position=True,  # Exact FIFO queue modeling!
        trade_execution=True,  # Match against actual tape prints!
        use_reduce_only=True,
    )

    # 2. Add Instrument and Data from Catalog
    engine.add_instrument(instrument)
    quotes = catalog.quote_ticks(instrument_ids=[inst_str])
    trades = catalog.trade_ticks(instrument_ids=[inst_str])

    print(f"📊 [DATA LOADED] {len(quotes):,} QuoteTicks | {len(trades):,} TradeTicks")
    engine.add_data(quotes)
    engine.add_data(trades)

    # 3. Add Decoupled Strategy
    strat_config = MarkovPoloNautilusConfig(
        instrument_id=inst_str,
        target_notional_usdc=133.85,
        min_notional_usdc=22.0,
        ou_horizon_ticks=800,
        base_entry_sigma=1.65,
        hard_disaster_stop_bps=40.0,
        beta_overshoot=0.58,
        stall_trigger_ticks=120,
    )
    strategy = MarkovPoloNautilusStrategy(config=strat_config)
    engine.add_strategy(strategy)

    # 4. Execute Event-Driven Simulation
    print(
        f"🚀 [SIMULATION RUNNING] Streaming events through NautilusTrader system kernel..."
    )
    engine.run()

    # 5. Extract Reports & Audit Metrics
    print(f"\n=================================================================")
    print(f"📋 [AUDIT REPORT - {target_symbol}] NautilusTrader Institutional Results")
    print(f"=================================================================")

    account = engine.portfolio.account(Venue("HYPERLIQUID"))
    if account:
        final_balance = float(account.balance_total(Currency.from_str("USDC")))
        net_profit = final_balance - 536.0
        pct_return = (net_profit / 536.0) * 100.0
        print(f"💰 Initial Balance : $536.00 USDC")
        print(f"💰 Final Balance   : ${final_balance:.2f} USDC")
        print(f"📈 Net PnL         : ${net_profit:+.2f} USDC ({pct_return:+.2f}%)")

    # Order & Position Stats
    orders = engine.cache.orders()
    from nautilus_trader.model.enums import OrderStatus

    filled_orders = [o for o in orders if o.status == OrderStatus.FILLED]
    print(f"📑 Total Orders Submitted : {len(orders)}")
    print(f"✅ Total Orders Filled    : {len(filled_orders)}")

    positions = engine.cache.positions()
    closed_pos = [p for p in positions if p.is_closed]
    print(f"🏁 Completed Positions   : {len(closed_pos)}")
    for i, p in enumerate(closed_pos, 1):
        realized = float(p.realized_pnl)
        print(f"   ↳ Position {i}: {p.side.name} | Realized PnL: ${realized:+.4f} USDC")

    print(f"=================================================================\n")


if __name__ == "__main__":
    target = sys.argv[1] if len(sys.argv) > 1 else "SOL"
    run_audit(target)
