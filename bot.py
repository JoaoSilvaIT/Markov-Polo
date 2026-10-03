import os
import sys
import time
import json
import argparse
from datetime import datetime, timedelta, timezone
from dotenv import load_dotenv

import pandas as pd
import ta
from google import genai
from google.genai import types

from alpaca.trading.client import TradingClient
from alpaca.trading.requests import MarketOrderRequest, TakeProfitRequest, StopLossRequest, GetAssetsRequest
from alpaca.trading.enums import OrderSide, TimeInForce, OrderClass, AssetClass, AssetStatus
from alpaca.data.historical import StockHistoricalDataClient, CryptoHistoricalDataClient
from alpaca.data.historical.news import NewsClient
from alpaca.data.requests import (
    StockBarsRequest,
    StockLatestBarRequest,
    CryptoBarsRequest,
    NewsRequest
)
from alpaca.data.timeframe import TimeFrame

# ---------------------------------------------------------------------
# 1. INITIALIZE CONFIGURATION & CLIENTS
# ---------------------------------------------------------------------
load_dotenv()

ALPACA_KEY = os.getenv("ALPACA_API_KEY")
ALPACA_SECRET = os.getenv("ALPACA_SECRET_KEY")

if not ALPACA_KEY or not ALPACA_SECRET:
    raise ValueError("Missing Alpaca API credentials in .env file.")

# Parse CLI arguments
parser = argparse.ArgumentParser(description="Autonomous Quantitative Trading Agent (Crypto & Stocks)")
parser.add_argument("--mode", choices=["crypto", "stock"], default=os.getenv("ASSET_MODE", "crypto").lower(),
                    help="Asset class mode: 'crypto' (24/7 markets) or 'stock' (equities/ETFs)")
parser.add_argument("--budget", type=float, default=float(os.getenv("BUDGET_PER_TRADE", 100.0)),
                    help="Max capital allocation per trade in USD ($100 budget)")
parser.add_argument("--loop", type=int, default=int(os.getenv("LOOP_MINUTES", 0)),
                    help="Run continuously every N minutes (0 for single evaluation run)")
parser.add_argument("--status", action="store_true",
                    help="Show live account overview, active positions, P&L, and recent journal history, then exit.")
args = parser.parse_args()

MODE = args.mode
BUDGET_PER_TRADE = args.budget
LOOP_MINUTES = args.loop
MAX_DAILY_LOSS_PCT = float(os.getenv("MAX_DAILY_LOSS_PCT", 3.0))
STATE_FILE = "crypto_state.json" if MODE == "crypto" else "stock_state.json"
JOURNAL_MD = "trading_journal.md"
JOURNAL_JSONL = "trading_journal.jsonl"
CURRENT_CYCLE_DATA = {"technicals": {}, "news": {}, "orders": []}

def adaptive_round(val: float) -> float:
    """Rounds values adaptively so sub-dollar assets retain precision while whole-dollar assets stay clean."""
    if abs(val) >= 100.0:
        return round(val, 2)
    elif abs(val) >= 1.0:
        return round(val, 3)
    elif abs(val) >= 0.01:
        return round(val, 5)
    elif abs(val) > 0:
        return round(val, 8)
    return 0.0

def log_to_journal(memo: str, event_type: str = "CYCLE_EVALUATION", details: dict = None):
    """Appends trade decisions, exact metrics, and the model's complete reasoning to disk."""
    now_utc = datetime.now(timezone.utc)
    timestamp_str = now_utc.strftime("%Y-%m-%d %H:%M:%S UTC")
    iso_timestamp = now_utc.isoformat()

    entry = {
        "timestamp": iso_timestamp,
        "mode": MODE,
        "event_type": event_type,
        "details": details or {},
        "memo": memo.strip() if memo else ""
    }

    # 1. Append machine-readable JSON Lines (.jsonl)
    try:
        with open(JOURNAL_JSONL, "a") as f:
            f.write(json.dumps(entry) + "\n")
    except Exception as e:
        print(f"[WARN] Failed to write to {JOURNAL_JSONL}: {e}")

    # 2. Append human-readable GitHub-flavored Markdown (.md)
    try:
        file_exists = os.path.exists(JOURNAL_MD)
        with open(JOURNAL_MD, "a") as f:
            if not file_exists:
                f.write("# Quantitative Autonomous Trading Journal\n\n")
                f.write("A permanent record of market analysis, quantitative indicators, news catalysts, AI reasoning, and executed orders.\n\n---\n\n")
            
            f.write(f"## [{timestamp_str}] {MODE.upper()} - {event_type}\n\n")
            
            # Format clean tables for technical metrics if present
            if details and details.get("technicals"):
                f.write("### Quantitative Indicators Evaluated\n\n")
                f.write("| Asset | Price | RSI-14 | Trend | ATR-14 | Volatility % | Dynamic SL | Dynamic TP | R:R |\n")
                f.write("| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |\n")
                for sym, q in details["technicals"].items():
                    bracket = q.get("quant_recommended_bracket", {})
                    sl = bracket.get("stop_loss_price", "N/A")
                    tp = bracket.get("take_profit_price", "N/A")
                    rr = bracket.get("reward_risk_ratio", "1.5")
                    f.write(f"| **{sym}** | ${q.get('latest_price')} | {q.get('rsi_14')} | `{q.get('trend_regime')}` | ${q.get('atr_14')} | {q.get('volatility_pct')}% | ${sl} | ${tp} | {rr} |\n")
                f.write("\n")

            # Format news headlines reviewed if present
            if details and details.get("news"):
                f.write("### News & Sentiment Catalysts Audited\n\n")
                for sym, headlines in details["news"].items():
                    f.write(f"**{sym} Headlines:**\n")
                    for h in headlines[:3]:
                        f.write(f"- {h}\n")
                    f.write("\n")

            # Format executed orders if present
            if details and details.get("orders"):
                f.write("### Executed Orders Dispatched\n\n")
                f.write("```json\n")
                f.write(json.dumps(details["orders"], indent=2) + "\n")
                f.write("```\n\n")

            f.write("### Agent Investment Memo & Reasoning\n\n")
            f.write(f"{memo.strip() if memo else 'No text memo generated.'}\n\n")
            f.write("---\n\n")
    except Exception as e:
        print(f"[WARN] Failed to write to {JOURNAL_MD}: {e}")

# Alpaca Execution & Data Clients
trading_client = TradingClient(ALPACA_KEY, ALPACA_SECRET, paper=True)
stock_data_client = StockHistoricalDataClient(ALPACA_KEY, ALPACA_SECRET)
crypto_data_client = CryptoHistoricalDataClient(ALPACA_KEY, ALPACA_SECRET)
news_client = NewsClient(ALPACA_KEY, ALPACA_SECRET)

# Google GenAI Client configured for Vertex AI
ai_client = genai.Client(
    vertexai=True,
    project=os.getenv("GOOGLE_CLOUD_PROJECT", "your-gcp-project-id"),
    location=os.getenv("GOOGLE_CLOUD_LOCATION", "us-central1")
)

def get_all_crypto_symbols():
    """Dynamically fetches all active, tradable USD crypto pairs on Alpaca, excluding stablecoins."""
    try:
        req = GetAssetsRequest(asset_class=AssetClass.CRYPTO, status=AssetStatus.ACTIVE)
        assets = trading_client.get_all_assets(req)
        stablecoins = {"USDT/USD", "USDC/USD", "USDG/USD", "PAXG/USD"}
        pairs = [
            a.symbol for a in assets 
            if a.tradable and a.symbol.endswith("USD") and a.symbol not in stablecoins
        ]
        priority = ["BTC/USD", "ETH/USD", "SOL/USD", "AVAX/USD", "LINK/USD", "DOGE/USD", "XRP/USD", "ADA/USD", "LTC/USD", "BCH/USD", "UNI/USD", "DOT/USD", "AAVE/USD", "RENDER/USD", "ARB/USD"]
        ordered = [p for p in priority if p in pairs] + [p for p in pairs if p not in priority]
        return ordered
    except Exception as e:
        print(f"[WARN] Error fetching dynamic crypto universe: {e}")
        return ["BTC/USD", "ETH/USD", "SOL/USD", "AVAX/USD", "LINK/USD", "DOGE/USD", "XRP/USD", "ADA/USD", "LTC/USD", "BCH/USD", "UNI/USD", "DOT/USD", "AAVE/USD", "RENDER/USD", "ARB/USD"]

STOCK_UNIVERSE = ["XLF", "GDX", "SLV", "BAC", "SOFI", "PFE", "KO", "F"]
ACTIVE_UNIVERSE = get_all_crypto_symbols() if MODE == "crypto" else STOCK_UNIVERSE

# ---------------------------------------------------------------------
# 2. SYSTEMIC RISK & CIRCUIT BREAKER
# ---------------------------------------------------------------------
def verify_account_health():
    """Validates account status and enforces daily drawdown limits."""
    account = trading_client.get_account()
    equity = float(account.equity)
    last_equity = float(account.last_equity)
    cash = float(account.cash)
    daily_drawdown_pct = ((equity - last_equity) / last_equity) * 100 if last_equity > 0 else 0.0

    print("=" * 65)
    print(f"ACCOUNT RISK & INTEGRITY CHECK [{MODE.upper()} MODE]")
    print("=" * 65)
    print(f"Status:        {account.status}")
    print(f"Equity:        ${equity:,.2f} USD")
    print(f"Cash:          ${cash:,.2f} USD")
    print(f"Daily P&L:     {daily_drawdown_pct:+.2f}%")
    print(f"Allocation:    ${BUDGET_PER_TRADE:,.2f} USD per trade")
    print(f"Universe Size: {len(ACTIVE_UNIVERSE)} assets monitored")
    print("=" * 65)

    if daily_drawdown_pct < -MAX_DAILY_LOSS_PCT:
        raise SystemExit(
            f"[CIRCUIT BREAKER TRIGGERED] Today's drawdown ({daily_drawdown_pct:.2f}%) exceeds safety "
            f"threshold (-{MAX_DAILY_LOSS_PCT}%). All new trades are locked to preserve capital."
        )


def audit_and_manage_open_positions():
    """Proactively audits all open positions against dynamic Stop-Loss and Take-Profit thresholds, executing exits immediately."""
    print("\n" + "=" * 65)
    print("PROACTIVE POSITION AUDIT & RISK MANAGEMENT")
    print("=" * 65)

    positions = trading_client.get_all_positions()
    if not positions:
        print("[AUDIT] No open positions currently in portfolio.")
        print("=" * 65)
        return []

    all_states = {}
    if os.path.exists(STATE_FILE):
        try:
            with open(STATE_FILE, "r") as f:
                all_states = json.load(f)
            if "symbol" in all_states and "clean_symbol" in all_states:
                all_states = {all_states["clean_symbol"]: all_states}
        except Exception as e:
            print(f"[WARN] Error reading {STATE_FILE}: {e}")
            all_states = {}

    active_clean_symbols = set()
    open_summary = []

    for pos in positions:
        sym = pos.symbol
        active_clean_symbols.add(sym)
        current_price = float(pos.current_price)
        avg_entry = float(pos.avg_entry_price)
        unrealized_pl = float(pos.unrealized_pl)
        pl_pct = ((current_price - avg_entry) / avg_entry) * 100 if avg_entry > 0 else 0.0

        target_state = all_states.get(sym)
        if not target_state:
            for k, v in all_states.items():
                if v.get("clean_symbol") == sym or k == sym:
                    target_state = v
                    break

        if MODE == "crypto" and target_state:
            sl_price = float(target_state.get("stop_loss_price", 0))
            tp_price = float(target_state.get("take_profit_price", float("inf")))

            # Check Stop-Loss
            if current_price <= sl_price:
                print(f"[RISK TRIGGER] Stop-Loss hit for {sym}! Current: ${adaptive_round(current_price)} <= SL: ${adaptive_round(sl_price)}. Liquidating...")
                try:
                    trading_client.close_position(sym)
                    all_states.pop(sym, None)
                    for k in list(all_states.keys()):
                        if all_states[k].get("clean_symbol") == sym:
                            all_states.pop(k, None)
                    with open(STATE_FILE, "w") as f:
                        json.dump(all_states, f, indent=2)

                    event_details = {
                        "action_taken": "STOP_LOSS_EXECUTED",
                        "symbol": sym,
                        "exit_price": current_price,
                        "entry_price": avg_entry,
                        "loss_usd": unrealized_pl,
                        "loss_pct": pl_pct
                    }
                    log_to_journal(
                        memo=f"Programmatic Stop-Loss triggered for {sym}. Liquidated at ${adaptive_round(current_price)} (SL: ${adaptive_round(sl_price)}) with P&L: ${unrealized_pl:.2f} ({pl_pct:+.2f}%).",
                        event_type="STOP_LOSS_EXECUTED",
                        details=event_details
                    )
                    continue
                except Exception as e:
                    print(f"[ERROR] Failed to close position {sym}: {e}")

            # Check Take-Profit
            elif current_price >= tp_price:
                print(f"[TARGET HIT] Take-Profit hit for {sym}! Current: ${adaptive_round(current_price)} >= TP: ${adaptive_round(tp_price)}. Liquidating...")
                try:
                    trading_client.close_position(sym)
                    all_states.pop(sym, None)
                    for k in list(all_states.keys()):
                        if all_states[k].get("clean_symbol") == sym:
                            all_states.pop(k, None)
                    with open(STATE_FILE, "w") as f:
                        json.dump(all_states, f, indent=2)

                    event_details = {
                        "action_taken": "TAKE_PROFIT_EXECUTED",
                        "symbol": sym,
                        "exit_price": current_price,
                        "entry_price": avg_entry,
                        "profit_usd": unrealized_pl,
                        "profit_pct": pl_pct
                    }
                    log_to_journal(
                        memo=f"Programmatic Take-Profit target achieved for {sym}. Liquidated at ${adaptive_round(current_price)} (TP: ${adaptive_round(tp_price)}) with P&L: +${unrealized_pl:.2f} ({pl_pct:+.2f}%).",
                        event_type="TAKE_PROFIT_EXECUTED",
                        details=event_details
                    )
                    continue
                except Exception as e:
                    print(f"[ERROR] Failed to close position {sym}: {e}")

            print(f"  * {sym:<10} | Qty: {float(pos.qty):.4f} | Entry: ${adaptive_round(avg_entry)} | Cur: ${adaptive_round(current_price)} | P&L: {pl_pct:+.2f}% (${unrealized_pl:+.2f}) | SL: ${adaptive_round(sl_price)} | TP: ${adaptive_round(tp_price)}")
        else:
            print(f"  * {sym:<10} | Qty: {float(pos.qty):.4f} | Entry: ${adaptive_round(avg_entry)} | Cur: ${adaptive_round(current_price)} | P&L: {pl_pct:+.2f}% (${unrealized_pl:+.2f}) [Broker-Managed Bracket]")

        open_summary.append({
            "symbol": sym,
            "qty": float(pos.qty),
            "entry_price": avg_entry,
            "current_price": current_price,
            "unrealized_pl": unrealized_pl,
            "pl_pct": pl_pct
        })

    # Cleanup stale entries in STATE_FILE that no longer exist in broker
    stale_keys = [k for k, v in all_states.items() if v.get("clean_symbol") not in active_clean_symbols and k not in active_clean_symbols]
    if stale_keys:
        for k in stale_keys:
            all_states.pop(k, None)
        with open(STATE_FILE, "w") as f:
            json.dump(all_states, f, indent=2)

    print("=" * 65)
    return open_summary


def show_status_and_exit():
    """Prints live account overview, active positions, P&L, and recent journal history, then exits."""
    verify_account_health()
    audit_and_manage_open_positions()

    if os.path.exists(JOURNAL_JSONL):
        print("\n" + "=" * 65)
        print("RECENT TRADING JOURNAL ENTRIES (LAST 5)")
        print("=" * 65)
        try:
            with open(JOURNAL_JSONL, "r") as f:
                lines = [line.strip() for line in f if line.strip()]
            for line in lines[-5:]:
                entry = json.loads(line)
                ts = str(entry.get("timestamp", "N/A"))[:19].replace("T", " ") + " UTC"
                mode = entry.get("mode", "N/A")
                ev = entry.get("event_type", entry.get("event", "N/A"))
                orders = entry.get("details", {}).get("orders", [])
                order_summary = f"({len(orders)} orders dispatched)" if orders else ""
                print(f"[{ts}] {mode.upper()} - {ev} {order_summary}")
        except Exception as e:
            print(f"[WARN] Could not parse {JOURNAL_JSONL}: {e}")
        print("=" * 65)
    sys.exit(0)

# ---------------------------------------------------------------------
# 3. AGENT TOOLS (QUANTITATIVE, NEWS & EXECUTION)
# ---------------------------------------------------------------------

def scan_market_opportunities(max_budget: float = BUDGET_PER_TRADE) -> str:
    """Scans all active crypto or stock assets, computing RSI-14, trend state, and ranking high-probability setups."""
    print(f"\n[AGENT ACTION] -> Calling Tool: scan_market_opportunities(max_budget=${max_budget})...")
    try:
        candidates = []
        if MODE == "crypto":
            # Scan EVERY tradable crypto pair on Alpaca
            target_universe = get_all_crypto_symbols()
            bars = crypto_data_client.get_crypto_bars(CryptoBarsRequest(
                symbol_or_symbols=target_universe,
                timeframe=TimeFrame.Hour,
                start=datetime.now(timezone.utc) - timedelta(days=7)
            ))
            df = bars.df
            for symbol in target_universe:
                if symbol not in df.index:
                    continue
                sub = df.xs(symbol)
                if len(sub) < 20:
                    continue
                close_px = float(sub.iloc[-1]["close"])
                rsi_series = ta.momentum.RSIIndicator(close=sub["close"], window=14).rsi()
                rsi_val = round(float(rsi_series.iloc[-1]), 1)
                sma_series = ta.trend.SMAIndicator(close=sub["close"], window=min(50, len(sub))).sma_indicator()
                sma_val = float(sma_series.iloc[-1])
                trend_state = "bullish" if close_px > sma_val else "bearish"

                candidates.append({
                    "symbol": symbol,
                    "latest_price": round(close_px, 4),
                    "rsi_14": rsi_val,
                    "trend_state": trend_state,
                    "timeframe": "1-Hour",
                    "affordable": True
                })

            bullish_setups = sorted([c for c in candidates if c["trend_state"] == "bullish" and c["rsi_14"] <= 65.0], key=lambda x: x["rsi_14"])
            oversold_dips = sorted([c for c in candidates if c["rsi_14"] < 35.0], key=lambda x: x["rsi_14"])

            result = {
                "mode": MODE,
                "total_scanned": len(candidates),
                "bullish_momentum_count": len(bullish_setups),
                "oversold_dip_count": len(oversold_dips),
                "top_bullish_recommendations": bullish_setups[:8],
                "top_oversold_recommendations": oversold_dips[:5],
                "all_candidates": candidates
            }
            print(f"[TOOL RESULT] Scanned {len(candidates)} crypto pairs: {len(bullish_setups)} bullish, {len(oversold_dips)} oversold.")
            return json.dumps(result)
        else:
            # Stocks use daily candles over the past 90 days
            bars = stock_data_client.get_stock_bars(StockBarsRequest(
                symbol_or_symbols=ACTIVE_UNIVERSE,
                timeframe=TimeFrame.Day,
                start=datetime.now(timezone.utc) - timedelta(days=90)
            ))
            df = bars.df
            for symbol in ACTIVE_UNIVERSE:
                if symbol not in df.index:
                    continue
                sub = df.xs(symbol)
                if len(sub) < 20:
                    continue
                close_px = round(float(sub.iloc[-1]["close"]), 2)
                if close_px > max_budget or close_px <= 0:
                    continue
                rsi_series = ta.momentum.RSIIndicator(close=sub["close"], window=14).rsi()
                rsi_val = round(float(rsi_series.iloc[-1]), 1)
                sma_series = ta.trend.SMAIndicator(close=sub["close"], window=min(50, len(sub))).sma_indicator()
                sma_val = round(float(sma_series.iloc[-1]), 2)
                trend_state = "bullish" if close_px > sma_val else "bearish"
                max_whole_shares = int(max_budget / close_px)

                candidates.append({
                    "symbol": symbol,
                    "latest_price": close_px,
                    "rsi_14": rsi_val,
                    "trend_state": trend_state,
                    "affordable_shares": max_whole_shares,
                    "estimated_cost": round(max_whole_shares * close_px, 2)
                })

        result = {
            "mode": MODE,
            "budget": max_budget,
            "qualified_count": len(candidates),
            "candidates": candidates
        }
        print(f"[TOOL RESULT] Scanned {len(candidates)} {MODE} assets with live indicators.")
        return json.dumps(result)
    except Exception as e:
        error_res = {"status": "ERROR", "message": str(e)}
        print(f"[TOOL RESULT] Error in scan: {error_res}")
        return json.dumps(error_res)


def check_portfolio_status(symbol: str) -> str:
    """Checks the live broker account for open positions, pending orders, and programmatic SL/TP execution."""
    print(f"\n[AGENT ACTION] -> Calling Tool: check_portfolio_status('{symbol}')...")
    clean_sym = symbol.replace("/", "")
    positions = trading_client.get_all_positions()
    pos = next((p for p in positions if p.symbol in (symbol, clean_sym)), None)

    if pos:
        market_val = float(pos.market_value)
        unrealized_pl = float(pos.unrealized_pl)
        current_price = float(pos.current_price)

        # For Crypto: evaluate programmatic Stop-Loss / Take-Profit state
        if MODE == "crypto" and os.path.exists(STATE_FILE):
            try:
                with open(STATE_FILE, "r") as f:
                    all_states = json.load(f)
                # Support single dict or dict of dicts
                if "symbol" in all_states and "clean_symbol" in all_states:
                    all_states = {all_states["clean_symbol"]: all_states}

                target_state = all_states.get(clean_sym) or all_states.get(symbol)
                if target_state:
                    sl_price = float(target_state.get("stop_loss_price", 0))
                    tp_price = float(target_state.get("take_profit_price", float("inf")))

                    if current_price <= sl_price:
                        print(f"\n[RISK TRIGGER] Stop-Loss hit (${current_price} <= ${sl_price})! Liquidating {symbol}...")
                        trading_client.close_position(clean_sym)
                        all_states.pop(clean_sym, None)
                        all_states.pop(symbol, None)
                        with open(STATE_FILE, "w") as f:
                            json.dump(all_states, f, indent=2)
                        result = {
                            "has_open_position": False,
                            "action_taken": "STOP_LOSS_EXECUTED",
                            "symbol": symbol,
                            "exit_price": current_price,
                            "loss_usd": unrealized_pl
                        }
                        log_to_journal(
                            memo=f"Programmatic Stop-Loss triggered for {symbol}. Liquidated at ${current_price} with P&L: ${unrealized_pl:.2f}",
                            event_type="STOP_LOSS_EXECUTED",
                            details=result
                        )
                        print(f"[TOOL RESULT] {result}")
                        return json.dumps(result)

                    if current_price >= tp_price:
                        print(f"\n[TARGET HIT] Take-Profit hit (${current_price} >= ${tp_price})! Liquidating {symbol}...")
                        trading_client.close_position(clean_sym)
                        all_states.pop(clean_sym, None)
                        all_states.pop(symbol, None)
                        with open(STATE_FILE, "w") as f:
                            json.dump(all_states, f, indent=2)
                        result = {
                            "has_open_position": False,
                            "action_taken": "TAKE_PROFIT_EXECUTED",
                            "symbol": symbol,
                            "exit_price": current_price,
                            "profit_usd": unrealized_pl
                        }
                        log_to_journal(
                            memo=f"Programmatic Take-Profit target achieved for {symbol}. Liquidated at ${current_price} with P&L: +${unrealized_pl:.2f}",
                            event_type="TAKE_PROFIT_EXECUTED",
                            details=result
                        )
                        print(f"[TOOL RESULT] {result}")
                        return json.dumps(result)
            except Exception as e:
                print(f"[WARN] Error reading {STATE_FILE}: {e}")

        result = {
            "has_open_position": True,
            "shares_held": float(pos.qty),
            "market_value": market_val,
            "current_price": current_price,
            "unrealized_pl": unrealized_pl
        }
        print(f"[TOOL RESULT] Active open position detected for {symbol}: {result}")
        return json.dumps(result)

    open_orders = trading_client.get_orders()
    for order in open_orders:
        if order.symbol in (symbol, clean_sym):
            result = {
                "has_open_position": True,
                "has_pending_order": True,
                "order_id": str(order.id),
                "order_status": str(order.status)
            }
            print(f"[TOOL RESULT] Pending order detected for {symbol}: {result}")
            return json.dumps(result)

    print(f"[TOOL RESULT] No existing positions or open orders for {symbol}.")
    return json.dumps({"has_open_position": False, "has_pending_order": False})


def fetch_market_technicals(symbol: str) -> str:
    """Computes deterministic quantitative metrics: RSI-14, SMA-20, SMA-50, ATR-14 volatility, Bollinger Bands, and Z-score."""
    print(f"\n[AGENT ACTION] -> Calling Tool: fetch_market_technicals('{symbol}')...")
    if MODE == "crypto":
        request_params = CryptoBarsRequest(
            symbol_or_symbols=symbol,
            timeframe=TimeFrame.Hour,
            start=datetime.now(timezone.utc) - timedelta(days=14)
        )
        bars = crypto_data_client.get_crypto_bars(request_params)
        df = bars.df.xs(symbol)
    else:
        request_params = StockBarsRequest(
            symbol_or_symbols=symbol,
            timeframe=TimeFrame.Day,
            start=datetime.now(timezone.utc) - timedelta(days=120)
        )
        bars = stock_data_client.get_stock_bars(request_params)
        df = bars.df.xs(symbol)

    # 1. Momentum & Moving Averages
    df["rsi_14"] = ta.momentum.RSIIndicator(close=df["close"], window=14).rsi()
    df["sma_20"] = ta.trend.SMAIndicator(close=df["close"], window=20).sma_indicator()
    df["sma_50"] = ta.trend.SMAIndicator(close=df["close"], window=50).sma_indicator()

    # 2. Volatility (ATR) & Range
    atr_ind = ta.volatility.AverageTrueRange(high=df["high"], low=df["low"], close=df["close"], window=14)
    df["atr_14"] = atr_ind.average_true_range()

    bb = ta.volatility.BollingerBands(close=df["close"], window=20, window_dev=2)
    df["bb_high"] = bb.bollinger_hband()
    df["bb_low"] = bb.bollinger_lband()
    df["bb_mid"] = bb.bollinger_mavg()
    df["bb_width"] = (df["bb_high"] - df["bb_low"]) / df["bb_mid"]

    latest = df.iloc[-1]
    close_px = float(latest["close"])
    rsi_val = float(latest["rsi_14"])
    sma_20 = float(latest["sma_20"])
    sma_50 = float(latest["sma_50"])
    atr_val = float(latest["atr_14"])
    bb_width = float(latest["bb_width"])

    std_20 = df["close"].tail(20).std()
    z_score = (close_px - sma_20) / std_20 if std_20 > 0 else 0.0

    if close_px > sma_20 and close_px > sma_50:
        trend_regime = "strong_bullish"
    elif close_px > sma_50:
        trend_regime = "bullish"
    elif close_px < sma_20 and close_px < sma_50:
        trend_regime = "bearish"
    else:
        trend_regime = "neutral_consolidation"

    # ATR Dynamic Sizing: 2.0x ATR Stop Loss, 3.0x ATR Take Profit (1.5:1 ratio)
    dynamic_stop_loss = adaptive_round(close_px - (2.0 * atr_val))
    dynamic_take_profit = adaptive_round(close_px + (3.0 * atr_val))

    quant_metrics = {
        "symbol": symbol,
        "mode": MODE,
        "latest_price": adaptive_round(close_px),
        "rsi_14": round(rsi_val, 2),
        "sma_20": adaptive_round(sma_20),
        "sma_50": adaptive_round(sma_50),
        "trend_regime": trend_regime,
        "atr_14": adaptive_round(atr_val),
        "volatility_pct": round((atr_val / close_px) * 100, 2),
        "bollinger_bandwidth": round(bb_width, 4),
        "price_z_score_20d": round(float(z_score), 2),
        "quant_recommended_bracket": {
            "stop_loss_price": dynamic_stop_loss,
            "take_profit_price": dynamic_take_profit,
            "reward_risk_ratio": 1.5
        }
    }
    print(f"[TOOL RESULT] Quant Metrics for {symbol}: Price=${adaptive_round(close_px)} | RSI={rsi_val:.1f} | ATR=${adaptive_round(atr_val)} | Trend={trend_regime}")
    CURRENT_CYCLE_DATA["technicals"][symbol] = quant_metrics
    return json.dumps(quant_metrics)


def fetch_market_news(symbol: str) -> str:
    """Pulls recent real-time market headlines and sentiment catalysts for the given symbol."""
    print(f"\n[AGENT ACTION] -> Calling Tool: fetch_market_news('{symbol}')...")
    try:
        query_sym = symbol.replace("/", "")
        req = NewsRequest(symbols=query_sym, limit=5)
        news_set = news_client.get_news(req)
        articles = []
        for n in news_set.data.get("news", []):
            articles.append({
                "headline": getattr(n, "headline", ""),
                "source": getattr(n, "source", ""),
                "created_at": str(getattr(n, "created_at", "")),
                "summary": getattr(n, "summary", "") or "Headline without body summary",
                "url": getattr(n, "url", "")
            })

        result = {
            "symbol": symbol,
            "total_articles": len(articles),
            "articles": articles
        }
        CURRENT_CYCLE_DATA["news"][symbol] = [a["headline"] for a in articles]
        print(f"[TOOL RESULT] Fetched {len(articles)} news articles for {symbol}.")
        return json.dumps(result)
    except Exception as e:
        error_res = {"status": "ERROR", "message": str(e)}
        print(f"[TOOL RESULT] News query error: {error_res}")
        return json.dumps(error_res)


def place_order(symbol: str, usd_allocation: float, take_profit_price: float, stop_loss_price: float) -> str:
    """Places an order with deterministic risk controls and dynamic Take-Profit / Stop-Loss targets."""
    print(f"\n[AGENT ACTION] -> Calling Tool: place_order for {symbol} (${usd_allocation})...")
    
    # 1. Fetch live market price
    if MODE == "crypto":
        bars = crypto_data_client.get_crypto_bars(CryptoBarsRequest(
            symbol_or_symbols=symbol,
            timeframe=TimeFrame.Hour,
            start=datetime.now(timezone.utc) - timedelta(hours=2)
        ))
        current_price = float(bars.df.xs(symbol).iloc[-1]["close"])
    else:
        bars = stock_data_client.get_stock_bars(StockBarsRequest(
            symbol_or_symbols=symbol,
            timeframe=TimeFrame.Day,
            start=datetime.now(timezone.utc) - timedelta(days=5)
        ))
        current_price = float(bars.df.xs(symbol).iloc[-1]["close"])

    # 2. Risk Guardrails
    account = trading_client.get_account()
    available_cash = float(account.cash)
    if available_cash < usd_allocation:
        return json.dumps({
            "status": "REJECTED",
            "error": f"Insufficient cash (${available_cash:,.2f} USD) to allocate ${usd_allocation:,.2f} USD. Capital preservation active."
        })

    clean_sym = symbol.replace("/", "")
    positions = trading_client.get_all_positions()
    if any(p.symbol in (symbol, clean_sym) for p in positions):
        return json.dumps({
            "status": "REJECTED",
            "error": f"Position already open for {symbol}. Will not add to existing position."
        })

    if take_profit_price <= current_price:
        return json.dumps({"status": "REJECTED", "error": f"Take-profit (${take_profit_price}) must be above entry (${current_price})."})
    if stop_loss_price >= current_price:
        return json.dumps({"status": "REJECTED", "error": f"Stop-loss (${stop_loss_price}) must be below entry (${current_price})."})

    reward = take_profit_price - current_price
    risk = current_price - stop_loss_price
    if risk <= 0 or (reward / risk) < 1.0:
        return json.dumps({"status": "REJECTED", "error": f"Unfavorable Reward-to-Risk ratio ({reward/risk:.2f} < 1.0)."})

    if (risk / current_price) > 0.15:
        return json.dumps({"status": "REJECTED", "error": "Stop-loss risk exceeds max safety threshold (15.0%)."})

    # 3. Execution
    try:
        if MODE == "crypto":
            # Crypto: notional market order + programmatic state management
            order_req = MarketOrderRequest(
                symbol=symbol,
                notional=round(usd_allocation, 2),
                side=OrderSide.BUY,
                time_in_force=TimeInForce.GTC
            )
            order = trading_client.submit_order(order_req)
            all_states = {}
            if os.path.exists(STATE_FILE):
                try:
                    with open(STATE_FILE, "r") as f:
                        all_states = json.load(f)
                    if "symbol" in all_states and "clean_symbol" in all_states:
                        all_states = {all_states["clean_symbol"]: all_states}
                except Exception:
                    all_states = {}

            clean_sym = symbol.replace("/", "")
            all_states[clean_sym] = {
                "symbol": symbol,
                "clean_symbol": clean_sym,
                "order_id": str(order.id),
                "entry_price": current_price,
                "allocation_usd": usd_allocation,
                "stop_loss_price": stop_loss_price,
                "take_profit_price": take_profit_price,
                "opened_at": datetime.now(timezone.utc).isoformat()
            }
            with open(STATE_FILE, "w") as f:
                json.dump(all_states, f, indent=2)

            execution_result = {
                "status": "DISPATCHED",
                "order_id": str(order.id),
                "symbol": symbol,
                "allocation_usd": usd_allocation,
                "estimated_entry": adaptive_round(current_price),
                "take_profit_target": adaptive_round(take_profit_price),
                "stop_loss_target": adaptive_round(stop_loss_price),
                "managed_mode": "Programmatic Daemon SL/TP"
            }
        else:
            # Stocks: Broker-level whole-share bracket order
            share_qty = max(1, int(round(usd_allocation / current_price)))
            bracket_request = MarketOrderRequest(
                symbol=symbol,
                qty=share_qty,
                side=OrderSide.BUY,
                time_in_force=TimeInForce.GTC,
                order_class=OrderClass.BRACKET,
                take_profit=TakeProfitRequest(limit_price=round(take_profit_price, 2)),
                stop_loss=StopLossRequest(stop_price=round(stop_loss_price, 2))
            )
            order = trading_client.submit_order(bracket_request)
            execution_result = {
                "status": "DISPATCHED",
                "order_id": str(order.id),
                "symbol": symbol,
                "shares": share_qty,
                "estimated_entry": current_price,
                "take_profit_limit": round(take_profit_price, 2),
                "stop_loss_stop": round(stop_loss_price, 2)
            }

        CURRENT_CYCLE_DATA["orders"].append(execution_result)
        print(f"[TOOL RESULT] Order successfully dispatched: {execution_result}")
        return json.dumps(execution_result)
    except Exception as e:
        error_result = {"status": "FAILED", "error": str(e)}
        print(f"[TOOL RESULT] Order failed: {error_result}")
        return json.dumps(error_result)


# ---------------------------------------------------------------------
# 4. AGENT REASONING ENGINE
# ---------------------------------------------------------------------
tools = [
    scan_market_opportunities,
    check_portfolio_status,
    fetch_market_technicals,
    fetch_market_news,
    place_order
]

def run_agent_cycle():
    """Runs a complete autonomous quantitative evaluation cycle."""
    global CURRENT_CYCLE_DATA
    CURRENT_CYCLE_DATA = {"technicals": {}, "news": {}, "orders": []}
    verify_account_health()
    audit_and_manage_open_positions()

    account = trading_client.get_account()
    cash = float(account.cash)
    if cash < BUDGET_PER_TRADE * 0.5:
        print(f"\n[CAPITAL PRESERVATION] Cash available (${cash:,.2f}) is below minimum threshold for new trades (${BUDGET_PER_TRADE * 0.5:,.2f}).")
        print("Portfolio capital is fully deployed. Open positions are actively being monitored for Stop-Loss / Take-Profit triggers.")
        return

    chat = ai_client.chats.create(
        model="gemini-2.5-flash",
        config=types.GenerateContentConfig(
            tools=tools,
            temperature=0.1
        )
    )

    goal = f"""
You are an autonomous Senior Quantitative Portfolio Manager operating in {MODE.upper()} mode with a ${BUDGET_PER_TRADE} budget.
Your mission is to evaluate the 24/7 market state and execute high-probability trades ONLY when strict quantitative and sentiment criteria are met.

Operational Directives:
1. MARKET SCAN: Call 'scan_market_opportunities' with max_budget={BUDGET_PER_TRADE} to inspect live momentum and RSI across the entire universe.
2. CANDIDATE SELECTION & PORTFOLIO AUDIT:
   - Select the top candidate from the scan recommendations.
   - You MUST call 'check_portfolio_status' for the chosen candidate. If an open position or pending order already exists, pick another candidate.
   - You MUST call 'fetch_market_technicals' to calculate the exact ATR volatility and dynamic Take-Profit / Stop-Loss targets.
   - BUY CRITERIA:
     * Trend Regime is 'strong_bullish' OR 'bullish' (OR oversold near support with RSI-14 < 35.0).
     * RSI-14 is below 65.0 (avoid chasing overextended tops).
   - If technical criteria are not met, evaluate the next candidate.
3. NEWS & CATALYST AUDIT:
   - Once a qualified setup is identified, call 'fetch_market_news' to confirm there are NO catastrophic red flags.
4. LIVE EXECUTION:
   - If both the quantitative rules and news audit align:
     * You MUST invoke the 'place_order' tool with usd_allocation={BUDGET_PER_TRADE} and the quant-recommended Take-Profit and Stop-Loss prices. Do NOT simply write that you are placing an order in text—execute it via the tool!
   - If NO candidate qualifies, do NOT call place_order.
5. REPORT: Deliver a structured Quantitative Investment Memo only after completing all necessary tool interactions.
"""

    print("\n" + "=" * 65)
    print(f"STARTING {MODE.upper()} AUTONOMOUS AGENT CYCLE...")
    print("=" * 65)
    
    max_retries = 3
    for attempt in range(1, max_retries + 1):
        try:
            response = chat.send_message(goal)
            print("\n" + "=" * 65)
            print("QUANTITATIVE INVESTMENT MEMO:")
            print("=" * 65)
            memo = response.text
            if not memo and response.candidates and response.candidates[0].content:
                text_parts = [p.text for p in response.candidates[0].content.parts if hasattr(p, "text") and p.text]
                memo = "\n".join(text_parts) if text_parts else "Trade cycle completed. Check logs for executed actions."
            print(memo or "Trade cycle completed.")
            event_type = "TRADE_DISPATCHED" if CURRENT_CYCLE_DATA.get("orders") else "EVALUATION_PASS"
            log_to_journal(memo=memo or "Evaluation completed.", event_type=event_type, details=CURRENT_CYCLE_DATA)
            print(f"\n[JOURNAL UPDATED] Decisions and quantitative data logged to {JOURNAL_MD} and {JOURNAL_JSONL}")
            break
        except Exception as e:
            if "429" in str(e) or "RESOURCE_EXHAUSTED" in str(e):
                if attempt < max_retries:
                    wait_time = attempt * 15
                    print(f"\n[RATE LIMIT] Vertex AI quota hit. Backing off for {wait_time}s (Attempt {attempt}/{max_retries})...")
                    time.sleep(wait_time)
                else:
                    print(f"\n[ERROR] Vertex AI quota exceeded after {max_retries} retries: {e}")
                    raise e
            else:
                raise e


# ---------------------------------------------------------------------
# 5. ENTRYPOINT (SINGLE RUN OR CONTINUOUS DAEMON LOOP)
# ---------------------------------------------------------------------
if __name__ == "__main__":
    if args.status:
        show_status_and_exit()

    if LOOP_MINUTES > 0:
        print(f"\n[DAEMON ACTIVATED] Running continuous 24/7 {MODE.upper()} agent every {LOOP_MINUTES} minutes.")
        print("Press Ctrl+C to stop.\n")
        cycle_count = 1
        while True:
            try:
                print(f"\n>>> CYCLE #{cycle_count} - {datetime.now(timezone.utc).strftime('%Y-%m-%d %H:%M:%S UTC')} <<<")
                run_agent_cycle()
                print(f"\n[CYCLE COMPLETE] Sleeping for {LOOP_MINUTES} minutes until next evaluation...")
                time.sleep(LOOP_MINUTES * 60)
                cycle_count += 1
            except KeyboardInterrupt:
                print("\n[STOPPED] Autonomous daemon stopped by user.")
                sys.exit(0)
            except Exception as e:
                print(f"[ERROR] Cycle encountered error: {e}. Retrying next cycle...")
                time.sleep(60)
    else:
        run_agent_cycle()
