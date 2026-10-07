# Hyperliquid Autonomous O-U Quantitative Trading Bot

A high-performance, fully autonomous quantitative perpetual futures trading engine for [Hyperliquid](https://hyperliquid.xyz), implementing continuous-time **Ornstein-Uhlenbeck (O-U)** stochastic process modeling, **Bertram optimal threshold boundaries**, and **Hidden Markov Model (HMM)** regime filtering.

This package is completely isolated from the Binance Spot bot (`rust-bot/`), allowing independent deployment, testing, and fallback.

---

## ⚡ Key Upgrades Over Binance Spot

1. **Bi-Directional Trading (Longs & Shorts):**
   - **Long:** Armed when price drops below $S_{\text{entry\_long}} = \mu - \Delta$, enters on green reversal bounce.
   - **Short:** Armed when price spikes above $S_{\text{entry\_short}} = \mu + \Delta$, enters on red reversal rejection.
2. **Strict Post-Only Maker Orders (`tif: "Alo"`):**
   - Uses Hyperliquid's native "Add Liquidity Only" flag to guarantee maker rebates / zero taker fee drag.
3. **1.5-Second Fast Sentinel:**
   - Dedicated asynchronous supervisor monitoring order fills every 1.5 seconds, immediately locking in resting Take Profit orders upon entry fill.
4. **Phillips & Yu (2005) Jackknife Bias Correction:**
   - Corrects Hurwicz upward bias on mean-reversion speed ($\theta$), avoiding phantom reversion signals.
5. **Robust Volatility Shock Filter:**
   - Short-horizon $Z_{15}$ and $Z_{60}$ deviation scores using 1.4826-scaled Median Absolute Deviation (MAD), vetoing entries during market cascades ($|Z| > 3.0$).
6. **25 bps Net Economic Edge Hurdle:**
   - Enforces $\ge 25$ bps expected edge after all fees and slippage.
7. **Atomic Portfolio Cluster Gate:**
   - Single-asset concurrent exposure across BTC, ETH, and SOL using atomic lock leasing (`hl_portfolio_cluster.lock`).
8. **3-Strike Circuit Breaker:**
   - Automatically pauses symbol trading for 24 hours after 3 consecutive time-stop exits.

---

## 🚀 Quick Setup & Deployment

### 1. Create an API Agent Wallet on Hyperliquid
1. Connect your wallet to [Hyperliquid](https://app.hyperliquid.xyz).
2. Go to **Settings** -> **API** -> **Create API Agent**.
3. Generate an API Agent key. Copy the **Private Key** and note your **Main Account Address**.
   *(Using an Agent Wallet is recommended as it allows trading execution without withdrawal access).*

### 2. Configure Environment (`.env`)
Create `.env` inside `hyperliquid-bot/`:
```bash
cp .env.example .env
```
Edit `.env` with your credentials:
```ini
HL_PRIVATE_KEY=0x...your_agent_private_key...
HL_WALLET_ADDRESS=0x...your_main_account_address...
HL_IS_MAINNET=true
HL_TRADING_SYMBOL=BTC
POSITION_SIZE_USDC=25.0
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/...
```

### 3. Run Locally (Cargo)
```bash
# Verify unit tests
cargo test

# Run BTC bot
HL_TRADING_SYMBOL=BTC cargo run --release
```

### 4. Run via Docker Compose
```bash
docker compose up -d --build
```
Check logs:
```bash
docker compose logs -f hl_bot_btc
docker compose logs -f hl_bot_eth
docker compose logs -f hl_bot_sol
```
