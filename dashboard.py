import streamlit as st
import json
import pandas as pd
import os

st.set_page_config(page_title="Trading Bot Dashboard", layout="wide")
st.title("📈 Algorithmic Trading Dashboard")

st.markdown("---")
st.header("🟢 Open Positions")

state_file = "binance_state.json"
if os.path.exists(state_file):
    try:
        with open(state_file, "r") as f:
            state = json.load(f)
        if state:
            # Convert nested dict to dataframe
            df_positions = pd.DataFrame.from_dict(state, orient="index")
            # Format columns
            if "entry_price" in df_positions.columns:
                df_positions["entry_price"] = df_positions["entry_price"].apply(lambda x: f"${x:,.4f}")
            if "stop_loss_price" in df_positions.columns:
                df_positions["stop_loss_price"] = df_positions["stop_loss_price"].apply(lambda x: f"${x:,.4f}")
            if "take_profit_price" in df_positions.columns:
                df_positions["take_profit_price"] = df_positions["take_profit_price"].apply(lambda x: f"${x:,.4f}")
            if "risk_amount_usdc" in df_positions.columns:
                df_positions["risk_amount_usdc"] = df_positions["risk_amount_usdc"].apply(lambda x: f"${x:,.2f}")
            
            st.dataframe(df_positions, use_container_width=True)
        else:
            st.info("No open positions currently. Bot is waiting for the perfect setup.")
    except Exception as e:
        st.error(f"Error reading positions: {e}")
else:
    st.info("State file not found yet. It will be created when the first trade is placed.")

st.markdown("---")
st.header("📜 Recent Bot Logs")

journal_file = "binance_journal.jsonl"
if os.path.exists(journal_file):
    try:
        with open(journal_file, "r") as f:
            lines = f.readlines()
        
        if lines:
            logs = [json.loads(line) for line in lines]
            df_logs = pd.DataFrame(logs)
            
            # Reorder and format columns
            if "timestamp" in df_logs.columns:
                df_logs["timestamp"] = pd.to_datetime(df_logs["timestamp"]).dt.strftime('%Y-%m-%d %H:%M:%S')
            
            # Show the last 50 logs, latest first
            df_logs = df_logs.iloc[::-1].head(50)
            st.dataframe(df_logs, use_container_width=True)
        else:
            st.info("Journal is empty.")
    except Exception as e:
        st.error(f"Error reading logs: {e}")
else:
    st.info("Journal file not found yet. It will be created when the bot completes its first cycle.")
