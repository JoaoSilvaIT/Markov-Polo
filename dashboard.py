import streamlit as st
from streamlit_autorefresh import st_autorefresh
import json
import pandas as pd
import os
import requests
import plotly.graph_objects as go
from datetime import datetime

# Set page config for a professional wide layout
st.set_page_config(page_title="Terminal | Algorithmic Trading", layout="wide", initial_sidebar_state="collapsed")

# Auto-refresh every 10 seconds (10000 ms)
st_autorefresh(interval=10000, key="datarefresh")

# Custom CSS for dark brokerage theme
st.markdown("""
<style>
    .metric-card {
        background-color: #1E1E1E;
        border-radius: 5px;
        padding: 15px;
        border: 1px solid #333;
    }
    .profit { color: #00FF00 !important; }
    .loss { color: #FF0000 !important; }
    .title-banner {
        background: linear-gradient(90deg, #1E3A8A 0%, #0F172A 100%);
        padding: 20px;
        border-radius: 5px;
        margin-bottom: 20px;
    }
</style>
""", unsafe_allow_html=True)

st.markdown('<div class="title-banner"><h1 style="color: white; margin: 0;">⚡ Algorithmic Trading Terminal</h1><p style="color: #94A3B8; margin: 0;">Live Risk Management & Execution Engine</p></div>', unsafe_allow_html=True)

def get_binance_price(symbol):
    try:
        sym = symbol.replace("/", "")
        res = requests.get(f"https://api.binance.com/api/v3/ticker/price?symbol={sym}", timeout=2).json()
        return float(res["price"])
    except:
        return None

def get_klines(symbol, interval="15m", limit=100):
    try:
        sym = symbol.replace("/", "")
        url = f"https://api.binance.com/api/v3/klines?symbol={sym}&interval={interval}&limit={limit}"
        res = requests.get(url, timeout=5).json()
        df = pd.DataFrame(res, columns=["time", "open", "high", "low", "close", "vol", "close_time", "qav", "num_trades", "tbbav", "tbqav", "ignore"])
        df['time'] = pd.to_datetime(df['time'], unit='ms')
        for col in ["open", "high", "low", "close", "vol"]:
            df[col] = df[col].astype(float)
        return df
    except:
        return pd.DataFrame()

def render_position_dashboard(bot_name, state_file):
    st.markdown(f"### {bot_name} Active Tracker")
    
    if not os.path.exists(state_file):
        st.info("No state file found. System is waiting for initial deployment.")
        return

    with open(state_file, "r") as f:
        state = json.load(f)
        
    if not state:
        st.success("🟢 System Online. Waiting for a high-probability setup...")
        return
        
    for symbol, pos in state.items():
        current_price = get_binance_price(symbol)
        
        entry_price = float(pos["entry_price"])
        amount = float(pos["amount"])
        tp_price = float(pos["take_profit_price"])
        sl_price = float(pos["stop_loss_price"])
        
        if current_price:
            notional = current_price * amount
            gross_pnl = (current_price - entry_price) * amount
            gross_pnl_pct = (current_price - entry_price) / entry_price * 100
            
            # Fee calculation: assuming 0.075% with BNB discount (entry + exit)
            entry_fee = (entry_price * amount) * 0.00075
            exit_fee = notional * 0.00075
            total_fees = entry_fee + exit_fee
            
            net_pnl = gross_pnl - total_fees
            pnl_class = "profit" if net_pnl > 0 else "loss"
            
            # Metrics
            cols = st.columns(6)
            cols[0].metric("Symbol", symbol)
            cols[1].metric("Current Price", f"${current_price:,.4f}", delta=f"{current_price - entry_price:,.4f}")
            cols[2].metric("Entry Price", f"${entry_price:,.4f}")
            cols[3].metric("Stop Loss", f"${sl_price:,.4f}")
            cols[4].metric("Take Profit", f"${tp_price:,.4f}")
            cols[5].metric("Net P&L (After Fees)", f"${net_pnl:,.2f}", delta=f"{gross_pnl_pct:,.2f}%")
            
            st.markdown(f"""
            <div style="font-size: 0.9em; color: #888; margin-bottom: 15px;">
                Position Size: {amount} {symbol.split('/')[0]} | Gross P&L: ${gross_pnl:.2f} | Est. Fees: ${total_fees:.2f}
            </div>
            """, unsafe_allow_html=True)
            
            # Chart
            df_klines = get_klines(symbol)
            if not df_klines.empty:
                fig = go.Figure(data=[go.Candlestick(x=df_klines['time'],
                                open=df_klines['open'],
                                high=df_klines['high'],
                                low=df_klines['low'],
                                close=df_klines['close'],
                                name="Market Data")])
                
                # Add horizontal lines
                fig.add_hline(y=entry_price, line_dash="dash", line_color="white", annotation_text="Entry")
                fig.add_hline(y=sl_price, line_dash="dash", line_color="red", annotation_text="Stop Loss")
                fig.add_hline(y=tp_price, line_dash="dash", line_color="green", annotation_text="Take Profit")
                
                # Current price line
                fig.add_hline(y=current_price, line_color="blue", annotation_text="Live Price", annotation_position="left")
                
                fig.update_layout(
                    title=f"Live Execution Tracker: {symbol}",
                    yaxis_title="Price",
                    xaxis_title="Time",
                    template="plotly_dark",
                    height=500,
                    margin=dict(l=0, r=0, t=40, b=0),
                    showlegend=False
                )
                
                st.plotly_chart(fig, use_container_width=True)
                
        else:
            st.warning(f"Could not fetch live price for {symbol}")

st.markdown("---")
# Tabs for organizing the dashboard
tab1, tab2 = st.tabs(["🚀 Vanguard Engine (Rust HMM)", "🛡️ Legacy Engine (Python Static)"])

with tab1:
    render_position_dashboard("Rust Vanguard (HMM)", "binance_hmm_state.json")
    
with tab2:
    render_position_dashboard("Python Legacy (Static)", "binance_state.json")

# System Logs at the bottom
st.markdown("---")
st.subheader("📜 System Audit Logs")

def get_logs(journal_file):
    if os.path.exists(journal_file):
        try:
            with open(journal_file, "r") as f:
                lines = f.readlines()
            if lines:
                logs = [json.loads(line) for line in lines]
                df_logs = pd.DataFrame(logs)
                if "timestamp" in df_logs.columns:
                    df_logs["timestamp"] = pd.to_datetime(df_logs["timestamp"]).dt.strftime('%Y-%m-%d %H:%M:%S')
                return df_logs.iloc[::-1].head(15)
        except:
            pass
    return pd.DataFrame()

col_log1, col_log2 = st.columns(2)
with col_log1:
    st.markdown("**Rust Engine Logs**")
    st.dataframe(get_logs("binance_hmm_journal.jsonl"), use_container_width=True)
with col_log2:
    st.markdown("**Python Engine Logs**")
    st.dataframe(get_logs("binance_journal.jsonl"), use_container_width=True)
