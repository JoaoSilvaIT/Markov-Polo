#!/usr/bin/env python3
import os
import sys
import time
import json
import argparse
from datetime import datetime, timezone

import pandas as pd
import ta
import ccxt
from dotenv import load_dotenv
from google import genai
from google.genai import types
from alpaca.data.historical.news import NewsClient
from alpaca.data.requests import NewsRequest

# ---------------------------------------------------------------------
# 1. INITIALIZE CONFIGURATION & CLIENTS
# ---------------------------------------------------------------------
load_dotenv()

BINANCE_KEY = os.getenv("BINANCE_API_KEY")
BINANCE_SECRET = os.getenv("BINANCE_SECRET_KEY")
ALPACA_KEY = os.getenv("ALPACA_API_KEY") # Still used for news if needed
ALPACA_SECRET = os.getenv("ALPACA_SECRET_KEY")

if not BINANCE_KEY or not BINANCE_SECRET:
    print("[ERROR] Missing Binance API credentials in .env file.")
    sys.exit(1)

exchange = ccxt.binance({
    'apiKey': BINANCE_KEY,
    'secret': BINANCE_SECRET,
    'enableRateLimit': True,
    'options': {'defaultType': 'spot'}
})

# Use Alpaca strictly for the News API since Binance doesn't have a news endpoint
news_client = NewsClient(ALPACA_KEY, ALPACA_SECRET) if ALPACA_KEY else None

ai_client = genai.Client(
    vertexai=True,
    project=os.getenv("GOOGLE_CLOUD_PROJECT", "your-gcp-project-id"),
    location=os.getenv("GOOGLE_CLOUD_LOCATION", "us-central1")
)

ACTIVE_UNIVERSE = ["BTC/USDC", "ETH/USDC", "SOL/USDC"]
STATE_FILE = "binance_state.json"
JOURNAL_MD = "binance_journal.md"
JOURNAL_JSONL = "binance_journal.jsonl"
KNOWLEDGE_BASE_FILE = "trade_knowledge_base.json"
CURRENT_CYCLE_DATA = {"technicals": {}, "news": {}, "orders": []}

RISK_PER_TRADE_PCT = 0.01  # 1.0% Fixed Fractional Risk
MAX_CONCURRENT_TRADES = 3  # Max 3.0% total portfolio risk

def adaptive_round(val: float) -> float:
    if abs(val) >= 100.0: return round(val, 2)
    elif abs(val) >= 1.0: return round(val, 3)
    elif abs(val) >= 0.01: return round(val, 5)
    elif abs(val) > 0: return round(val, 8)
    return 0.0

def log_to_journal(memo: str, event_type: str = "CYCLE_EVALUATION", details: dict = None):
    now_utc = datetime.now(timezone.utc)
    entry = {
        "timestamp": now_utc.isoformat(),
        "mode": "binance_crypto",
        "event_type": event_type,
        "details": details or {},
        "memo": memo.strip() if memo else ""
    }
    try:
        with open(JOURNAL_JSONL, "a") as f:
            f.write(json.dumps(entry) + "\n")
    except Exception:
        pass

    try:
        file_exists = os.path.exists(JOURNAL_MD)
        with open(JOURNAL_MD, "a") as f:
            if not file_exists:
                f.write("# Binance Quantitative Trading Journal\n\n")
            f.write(f"## [{now_utc.strftime('%Y-%m-%d %H:%M:%S UTC')}] {event_type}\n\n")
            f.write(f"{memo.strip() if memo else 'No text memo generated.'}\n\n---\n")
    except Exception:
        pass

# ---------------------------------------------------------------------
# 2. SYSTEMIC RISK & PORTFOLIO MANAGEMENT
# ---------------------------------------------------------------------
def audit_and_manage_open_positions():
    """Programmatic Stop-Loss and Take-Profit Manager (Safer for MVP than CCXT OCO limits)"""
    print("\n" + "=" * 65)
    print("PROACTIVE POSITION AUDIT & RISK MANAGEMENT")
    print("=" * 65)

    if not os.path.exists(STATE_FILE):
        print("[AUDIT] No open positions currently tracked.")
        return

    try:
        with open(STATE_FILE, "r") as f:
            all_states = json.load(f)
    except Exception:
        all_states = {}

    if not all_states:
        print("[AUDIT] No open positions currently tracked.")
        return

    tickers = exchange.fetch_tickers(list(all_states.keys()))
    
    for sym, state in list(all_states.items()):
        current_price = tickers[sym]['last']
        entry_price = state['entry_price']
        sl_price = state['stop_loss_price']
        tp_price = state['take_profit_price']
        amount = state['amount']

        pl_usd = (current_price - entry_price) * amount
        pl_pct = ((current_price - entry_price) / entry_price) * 100

        # Stop Loss
        if current_price <= sl_price:
            print(f"[RISK TRIGGER] Stop-Loss hit for {sym}! Liquidating...")
            try:
                exchange.create_order(sym, 'market', 'sell', amount)
                del all_states[sym]
                log_to_journal(f"Stop-Loss executed for {sym} at {current_price}. P&L: {pl_usd:.2f} USD ({pl_pct:.2f}%)", "STOP_LOSS")
            except Exception as e:
                print(f"[ERROR] Failed to close {sym}: {e}")

        # Take Profit
        elif current_price >= tp_price:
            print(f"[TARGET HIT] Take-Profit hit for {sym}! Liquidating...")
            try:
                exchange.create_order(sym, 'market', 'sell', amount)
                del all_states[sym]
                log_to_journal(f"Take-Profit executed for {sym} at {current_price}. P&L: {pl_usd:.2f} USD ({pl_pct:.2f}%)", "TAKE_PROFIT")
            except Exception as e:
                print(f"[ERROR] Failed to close {sym}: {e}")

        # Trailing Breakeven (35% to target)
        else:
            total_range = tp_price - sl_price
            progress = (current_price - sl_price) / total_range if total_range > 0 else 0
            if progress >= 0.35 and sl_price < entry_price:
                print(f"[TRAILING BREAKEVEN] {sym} reached 35%+ to target. Stop-Loss moved to entry ({entry_price}).")
                all_states[sym]['stop_loss_price'] = entry_price

            print(f"  * {sym:<8} | Cur: ${current_price:.2f} | Entry: ${entry_price:.2f} | P&L: {pl_pct:+.2f}%")
            print(f"    Target Track: [SL ${sl_price:.2f}] -------- [TP ${tp_price:.2f}]")

    with open(STATE_FILE, "w") as f:
        json.dump(all_states, f, indent=2)

# ---------------------------------------------------------------------
# 3. AGENT TOOLS
# ---------------------------------------------------------------------
def fetch_market_technicals(symbol: str) -> str:
    print(f"\n[AGENT ACTION] -> fetch_market_technicals('{symbol}')...")
    ohlcv = exchange.fetch_ohlcv(symbol, '15m', limit=100)
    df = pd.DataFrame(ohlcv, columns=['timestamp', 'open', 'high', 'low', 'close', 'volume'])
    
    df["rsi_14"] = ta.momentum.RSIIndicator(close=df["close"], window=14).rsi()
    df["sma_20"] = ta.trend.SMAIndicator(close=df["close"], window=20).sma_indicator()
    df["sma_50"] = ta.trend.SMAIndicator(close=df["close"], window=50).sma_indicator()
    df["vol_sma_20"] = ta.trend.SMAIndicator(close=df["volume"], window=20).sma_indicator()
    
    atr_ind = ta.volatility.AverageTrueRange(high=df["high"], low=df["low"], close=df["close"], window=14)
    df["atr_14"] = atr_ind.average_true_range()
    
    bb = ta.volatility.BollingerBands(close=df["close"], window=20, window_dev=2)
    df["bb_width"] = (bb.bollinger_hband() - bb.bollinger_lband()) / bb.bollinger_mavg()

    latest = df.iloc[-1]
    close_px = float(latest["close"])
    atr_val = float(latest["atr_14"])
    
    trend = "strong_bullish" if (close_px > latest["sma_20"] > latest["sma_50"]) else "neutral/bearish"
    
    metrics = {
        "symbol": symbol,
        "price": close_px,
        "rsi_14": round(float(latest["rsi_14"]), 1),
        "trend": trend,
        "bb_width": round(float(latest["bb_width"]), 4),
        "volume_confirmed": float(latest["volume"]) > float(latest["vol_sma_20"]),
        "recommended_sl": adaptive_round(close_px - (1.5 * atr_val)),
        "recommended_tp": adaptive_round(close_px + (2.0 * atr_val))
    }
    CURRENT_CYCLE_DATA["technicals"][symbol] = metrics
    return json.dumps(metrics)

def scan_market_opportunities() -> str:
    print(f"\n[AGENT ACTION] -> scan_market_opportunities()...")
    candidates = []
    for sym in ACTIVE_UNIVERSE:
        techs = json.loads(fetch_market_technicals(sym))
        candidates.append(techs)
        
    high_conviction = [
        c for c in candidates 
        if c["trend"] == "strong_bullish"
        and c["volume_confirmed"]
        and 0.015 <= c["bb_width"] <= 0.10
        and 40.0 <= c["rsi_14"] <= 58.0
    ]
    return json.dumps({"high_conviction_setups": high_conviction})

def fetch_market_news(symbol: str) -> str:
    print(f"\n[AGENT ACTION] -> fetch_market_news('{symbol}')...")
    if not news_client:
        return json.dumps({"error": "Alpaca API keys not provided for news."})
    try:
        query_sym = symbol.split('/')[0]
        news_set = news_client.get_news(NewsRequest(symbols=query_sym, limit=3))
        articles = [getattr(n, "headline", "") for n in news_set.data.get("news", [])]
        return json.dumps({"symbol": symbol, "headlines": articles})
    except Exception as e:
        return json.dumps({"error": str(e)})

def place_order(symbol: str, take_profit_price: float, stop_loss_price: float) -> str:
    print(f"\n[AGENT ACTION] -> place_order for {symbol}...")
    try:
        # 1. Fetch balance & calculate Fixed Fractional Risk
        balance = exchange.fetch_balance()
        usdc_total = balance.get('USDC', {}).get('total', 0)
        
        if usdc_total < 10:
            return json.dumps({"status": "REJECTED", "reason": f"Insufficient account balance: {usdc_total} USDC"})
            
        risk_amount_usdc = usdc_total * RISK_PER_TRADE_PCT
        
        # 2. Check concurrency limits
        all_states = {}
        if os.path.exists(STATE_FILE):
            with open(STATE_FILE, "r") as f:
                all_states = json.load(f)
                
        if len(all_states) >= MAX_CONCURRENT_TRADES:
             return json.dumps({"status": "REJECTED", "reason": f"Max concurrent trades ({MAX_CONCURRENT_TRADES}) reached."})
        if symbol in all_states:
             return json.dumps({"status": "REJECTED", "reason": f"Already holding {symbol}."})
             
        # 3. Position Sizing Calculus
        ticker = exchange.fetch_ticker(symbol)
        current_price = ticker['last']
        
        distance_to_sl = current_price - stop_loss_price
        if distance_to_sl <= 0:
            return json.dumps({"status": "REJECTED", "reason": "Stop loss must be below current price."})
            
        amount_to_buy = risk_amount_usdc / distance_to_sl
        notional_value = amount_to_buy * current_price
        
        # Binance requires minimum notional of typically 5 USDC
        if notional_value < 6.0:
            return json.dumps({"status": "REJECTED", "reason": f"Order size {notional_value:.2f} USDC is too small for Binance minimums."})
            
        if notional_value > balance.get('USDC', {}).get('free', 0):
            return json.dumps({"status": "REJECTED", "reason": "Insufficient FREE USDC for this position size."})

        # 4. Execution
        exchange.load_markets()
        market = exchange.market(symbol)
        amount_to_buy = exchange.amount_to_precision(symbol, amount_to_buy)
        
        order = exchange.create_order(symbol, 'market', 'buy', float(amount_to_buy))
        fill_price = order.get('average') or order.get('price') or current_price
        
        all_states[symbol] = {
            "symbol": symbol,
            "order_id": order['id'],
            "entry_price": fill_price,
            "amount": float(amount_to_buy),
            "stop_loss_price": stop_loss_price,
            "take_profit_price": take_profit_price,
            "opened_at": datetime.now(timezone.utc).isoformat(),
            "risk_amount_usdc": risk_amount_usdc
        }
        with open(STATE_FILE, "w") as f:
            json.dump(all_states, f, indent=2)
            
        res = {
            "status": "DISPATCHED",
            "symbol": symbol,
            "notional_usdc": notional_value,
            "amount": amount_to_buy,
            "fill_price": fill_price,
            "risk_taken_usdc": risk_amount_usdc
        }
        CURRENT_CYCLE_DATA["orders"].append(res)
        return json.dumps(res)
        
    except Exception as e:
        return json.dumps({"status": "ERROR", "message": str(e)})

# ---------------------------------------------------------------------
# 4. AGENT REASONING ENGINE
# ---------------------------------------------------------------------
tools = [scan_market_opportunities, fetch_market_news, place_order]

def run_agent_cycle():
    global CURRENT_CYCLE_DATA
    CURRENT_CYCLE_DATA = {"technicals": {}, "news": {}, "orders": []}
    
    audit_and_manage_open_positions()
    
    chat = ai_client.chats.create(
        model="gemini-2.5-flash",
        config=types.GenerateContentConfig(tools=tools, temperature=0.1)
    )

    goal = f"""
You are a Binance Quantitative Portfolio Manager.
You have a strict mandate: Beat 2.5% APY while prioritizing extreme capital preservation.
Max risk per trade is exactly 1.0% Fixed Fractional.

Directives:
1. Call 'scan_market_opportunities'. 
2. If the array is empty, you MUST conclude "Decision: PASS".
3. If a candidate exists, call 'fetch_market_news' for it. Assign a strict News Sentiment Score.
4. If news is not distinctly Bullish, conclude "Decision: PASS".
5. If confluence is perfect, call 'place_order' using the recommended_tp and recommended_sl from the scan.
6. Write a brief memo summarizing the decision.
"""
    print("\nSTARTING BINANCE QUANTITATIVE CYCLE...")
    try:
        response = chat.send_message(goal)
        memo = response.text or "Cycle completed."
        print(f"\nMEMO:\n{memo}")
        log_to_journal(memo, "TRADE_DISPATCHED" if CURRENT_CYCLE_DATA["orders"] else "PASS", CURRENT_CYCLE_DATA)
    except Exception as e:
        print(f"[ERROR] Vertex AI error: {e}")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--loop", type=int, default=15)
    args = parser.parse_args()

    if args.once:
        run_agent_cycle()
    else:
        while True:
            run_agent_cycle()
            print(f"\nSleeping for {args.loop} minutes...")
            time.sleep(args.loop * 60)
