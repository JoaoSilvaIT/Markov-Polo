import streamlit as st
import json
import pandas as pd
import os

st.set_page_config(page_title="Trading Bot Dashboard", layout="wide")
st.title("📈 Algorithmic Trading Dashboard: Champion vs. Challenger")
st.markdown("Comparing the Static 1.0% Risk Technical Bot against the advanced HMM Regime-Switching Bot.")

col1, col2 = st.columns(2)

def render_bot_section(col_title, state_file, journal_file):
    st.header(f"🟢 {col_title} - Open Positions")
    if os.path.exists(state_file):
        try:
            with open(state_file, "r") as f:
                state = json.load(f)
            if state:
                df_positions = pd.DataFrame.from_dict(state, orient="index")
                for col in ["entry_price", "stop_loss_price", "take_profit_price"]:
                    if col in df_positions.columns:
                        df_positions[col] = df_positions[col].apply(lambda x: f"${x:,.4f}")
                if "risk_amount_usdc" in df_positions.columns:
                    df_positions["risk_amount_usdc"] = df_positions["risk_amount_usdc"].apply(lambda x: f"${x:,.2f}")
                st.dataframe(df_positions, use_container_width=True)
            else:
                st.info("No open positions currently. Bot is waiting for the perfect setup.")
        except Exception as e:
            st.error(f"Error reading positions: {e}")
    else:
        st.info("State file not found yet.")

    st.markdown("---")
    st.header(f"📜 {col_title} - Logs")
    if os.path.exists(journal_file):
        try:
            with open(journal_file, "r") as f:
                lines = f.readlines()
            if lines:
                logs = [json.loads(line) for line in lines]
                df_logs = pd.DataFrame(logs)
                if "timestamp" in df_logs.columns:
                    df_logs["timestamp"] = pd.to_datetime(df_logs["timestamp"]).dt.strftime('%Y-%m-%d %H:%M:%S')
                df_logs = df_logs.iloc[::-1].head(50)
                st.dataframe(df_logs, use_container_width=True)
            else:
                st.info("Journal is empty.")
        except Exception as e:
            st.error(f"Error reading logs: {e}")
    else:
        st.info("Journal file not found yet.")

with col1:
    render_bot_section("Static Bot (Champion)", "binance_state.json", "binance_journal.jsonl")

with col2:
    render_bot_section("HMM Bot (Challenger)", "binance_hmm_state.json", "binance_hmm_journal.jsonl")
