# Hyperliquid Dual-Engine Perpetuals Trading System

An autonomous quantitative trading system written in asynchronous Rust (`tokio`) for perpetual futures on the [Hyperliquid L1 DEX](https://hyperliquid.xyz) (`BTC`, `ETH`, `SOL`, `AVAX`).

The system pairs a continuous-time **Ornstein-Uhlenbeck (O-U) mean-reversion engine** with a **Donchian / Chandelier ATR trend-following engine**, arbitrated by a causal **4-feature Gaussian Hidden Markov Model (HMM)** and an exogenous **Funding / Open Interest crowding hazard overlay**.

---

## Why This Architecture

Single-regime retail and stat-arb bots typically fail in one of two ways:
1. **Mean-reversion engines** harvest steady range-bound alpha with high win rates, but suffer catastrophic left-tail drawdowns when a stationary range breaks into a directional liquidation cascade.
2. **Momentum engines** capture fat-tailed directional breakouts, but bleed capital during sideways chop through whipsaws and taker fee drag (`3.5–4.5 bps` per leg).

This engine solves both problems at the state-machine and microstructure levels:
- **3-Mode Hysteresis Controller (`MeanReversion` / `Neutral` / `Trend`):** Uses forward-filtered HMM posteriors (never smoothed lookahead probabilities) with asymmetric multi-tier dwell requirements and a leaky integrator. Ambiguous transitions and post-shock states route to a 100% cash buffer (`Neutral`) with a hybrid bar + wall-clock cooldown.
- **Maker-First Execution (`tif: "Alo"`):** All entries are routed as Post-Only limit orders (`1.0–1.5 bps` maker tier), stepping 1 tick inside multi-tick spreads to claim FIFO Queue Rank #1 while continuously monitoring L2 Order Book Imbalance (`OBI`) to cancel resting orders before toxic sweeps.
- **Crash-Safe Distributed Coordination:** Four isolated per-asset processes coordinate over a shared volume using kernel `flock(2)` mutual exclusion, monotonic fencing tokens, and `fsync`-hardened Write-Ahead Order Intent Logs (WAL).

---

## System Architecture

```text
Hyperliquid WebSocket Streams (L2Book, 1m Candles, ActiveAssetCtx, OrderUpdates, UserFills)
       │
       ├──► L2ObiTracker (Distance-Weighted 20-Level OBI, 250ms/1s/5s Physical-Time EWMAs,
       │                  Stoikov Micro-Price, Transport Lag & Feed-Quality Watchdog)
       │
       ├──► MarketCrowdingTracker (Hourly Funding Rate, Δln(OI), Mark-Oracle Basis Z-Scores
       │                           ──► Exogenous Squeeze Hazard Multiplier H_t ∈ [0.25, 1.0])
       │
       ├──► 15m Causal Feature Engine & 4D Gaussian HMM (Baum-Welch EM + Ledoit-Wolf Shrinkage)
       │      ├─ Features: [r_t/σ_t, ln(σ_t), Kaufman ER(16), |Drift(16)|]
       │      └─► 3-Mode RegimeController (Asymmetric Dwell + Leaky Decay + Shock Cooldown)
       │             │
       │             ├─► [Mode: MeanReversion] ──► Engine A (Log-Space O-U + Phillips-Yu Jackknife)
       │             │     • Asymmetric Carry-Shifted Bertram Bands (1.50σ Tier 1 / 1.25σ Tier 2)
       │             │     • Two-Stage Reversal Confirmation (Arm on band touch ──► Enter on bounce)
       │             │     • One-Way Adaptive TP Ratchet + 20% Profit Giveback Exit
       │             │
       │             ├─► [Mode: Neutral]       ──► Cash Buffer / Resting Order Cancellation
       │             │
       │             └─► [Mode: Trend]         ──► Engine B (15m Donchian Breakout + EMA 16/64)
       │                   • Multiscale L2 OBI Entry Gate + Crowding Hazard Size Scaling
       │                   • Monotone 4-Tier Profit-Lock Chandelier ATR Trailing Stop
       │
       └──► Event-Driven Fast-Sentinel (Woken <20ms on WS Fills/L2 Breaches + 1.5s REST Audit)
              • Write-Ahead Order Intent (WAL) + Monotonic Lease Fencing
              • Partial-Fill Remainder Cancellation & Immediate Reduce-Only TP Placement
              • 3-Attempt Price-Protected IOC Emergency Flattener
```

---

## Core Quantitative & Systems Engineering Details

### 1. Regime Inference (`src/hmm.rs`, `src/controller.rs`)
- **4D Gaussian HMM with Ledoit-Wolf Shrinkage:** Trained offline every 24 hours on a rolling 1,000-bar 15m window (`spawn_blocking` off the async hot path). Covariance matrices are regularized toward a pooled variance target scaled by effective sample size $N_{\text{eff}, k} = (\sum_t \gamma_{t,k})^2 / \sum_t \gamma_{t,k}^2$. Candidate models are rejected if any state has $N_{\text{eff}, k} < 40$ or non-finite parameters.
- **Why Funding & Open Interest are Exogenous (MS-X Overlay):** Appending funding and $\Delta \ln \text{OI}$ directly into a 6D Gaussian HMM balloons free parameters from 46 to 85, starving the rare volatility-shock state on a 1,000-bar window. Instead, the HMM remains strictly 4D price-based while `MarketCrowdingTracker` computes an exogenous hazard multiplier $H_t$ from robust MAD Z-scores of hourly funding, $\Delta \ln \text{OI}$, and mark-oracle basis to scale down or veto crowded late-stage breakouts.

### 2. Engine A: Log-Price Ornstein-Uhlenbeck & Carry Perturbation (`src/ou_process.rs`)
- **Jackknife Bias Correction:** Standard OLS/MLE on finite rolling windows suffers from Hurwicz upward bias on mean-reversion speed $\theta$, producing artificially tight entry bands and underestimated half-lives. We estimate AR(1) coefficients in log-price space using centered two-pass variance and apply the **Phillips & Yu (2005) 2-split jackknife** ($\hat{\theta}_{\text{jack}} = 2\hat{\theta}_{\text{full}} - \frac{1}{2}(\hat{\theta}_1 + \hat{\theta}_2)$), propagating $b_{\text{jack}} = e^{-\hat{\theta}_{\text{jack}} \Delta t}$ directly into the stationary variance $\sigma_{\text{eq}}$.
- **Continuous-Time Hourly Funding Carry Shift:** Hyperliquid settles funding hourly at $1/8\text{th}$ of the 8-hour rate. Holding a position introduces continuous carry $c_{1\text{h}} = -\text{side} \cdot F_{1\text{h}}$, shifting the effective equilibrium mean by $\Delta Z_c = c_{1\text{h}} / (\kappa_{1\text{h}} \sigma_{\text{stat}})$. Positive-carry trades relax required entry excursion toward $1.30\sigma$, while negative-carry trades widen required excursion toward $1.75\sigma$.
- **Cross-Asset BTC Lead-Lag Impulse Veto:** Altcoin (`ETH`, `SOL`, `AVAX`) mean-reversion entries and resting `Alo` orders are gated by a beta-scaled BTC impulse score $I_{\text{BTC}\to k}(t) = \hat{\beta}_k (0.65 Z_{\text{BTC},15} + 0.35 Z_{\text{OBI}})$ to prevent fading systematic BTC liquidation cascades.

### 3. Engine B: Donchian Breakout & Monotone Chandelier Ratchet (`src/trend.rs`)
- **Microstructure Confirmation:** Requires a completed 15m Trend regime, 1m close beyond the prior $N$-bar 15m Donchian channel, `EMA 16 > EMA 64` alignment, and multiscale L2 OBI confirmation (`instant`, `1s`, and `5s` EWMAs) with a fresh, uncrossed book.
- **4-Zone Progressive Profit-Lock Chandelier Stop:** Trails price by $k(P_T) \times \text{ATR}_{20}$ (compressing linearly from $2.0\times$ to $1.0\times$ as trend probability $P_T$ decays), reinforced by a monotone profit ratchet:
  - **Zone 1 ($\ge 1.0\times \text{ATR}$ or $+1.0\%$):** Locks breakeven $+15\text{ bps}$ (bounded by $60\%$ of favorable excursion when ATR is tight).
  - **Zone 2 ($\ge +2.0\%$):** Locks at least $+1.0\%$ net gain.
  - **Zone 3 ($\ge +3.0\%$):** Locks at least $+2.0\%$ net gain.
  - **Zone 4 ($\ge +4.0\%$):** Locks $\max(+2.5\%, 0.70 \times \Delta_{\text{fav}})$.

### 4. Risk Sizing & Crash-Recovery Engineering (`src/state.rs`, `src/order.rs`, `src/hyperliquid.rs`)
- **Drawdown-Constrained Fractional-Kelly Sizing:** Position notional is sized by stop-loss distance ($N = (f_{\text{risk}} \cdot E) / (d_{\text{stop}} + c_{\text{stress}})$), with an exponential drawdown taper $m_{\text{DD}} = \exp(-10 \cdot \text{DD})$ when drawdown from high-water mark exceeds $2.5\%$, plus a step-function risk guard for micro-accounts near Hyperliquid's `$10.00` minimum notional floor.
- **Write-Ahead Order Intent Log (WAL) & Monotonic Fencing:** Before any order is signed and transmitted, `OrderIntent` is serialized to a PID-tagged temp file, `fsync`ed (`file.sync_all()`), atomically renamed, and parent-directory `fsync`ed (`sync_parent_dir`). On startup after a power loss or container crash, the engine reconciles live exchange positions and open orders against the WAL and local state before enabling trading.

---

## Repository Layout

```text
hyperliquid-bot/
├── src/
│   ├── main.rs            # Orchestration, WS consumers, 1m/15m loops, Fast-Sentinel
│   ├── controller.rs      # 3-Mode Regime Controller, asymmetric dwell & cooldown logic
│   ├── hmm.rs             # 4D Gaussian HMM (Baum-Welch EM, Ledoit-Wolf shrinkage, Viterbi/forward filter)
│   ├── ou_process.rs      # Log-price O-U estimation, Phillips-Yu jackknife, carry-shifted thresholds
│   ├── trend.rs           # Donchian channel, Wilder ATR, Chandelier ratchet, L2 OBI & Crowding trackers
│   ├── order.rs           # Post-Only (Alo) execution, spread-inside quoting, WAL & IOC flattener
│   ├── state.rs           # POSIX flock(2) cluster leases, monotonic fencing tokens, atomic persistence
│   ├── hyperliquid.rs     # Hyperliquid SDK bindings, tick/lot quantization, risk-scaled Kelly sizing
│   ├── counterfactual.rs  # Shadow trade simulator & veto attribution journal (MFE/MAE/trade-through)
│   ├── telemetry.rs       # Funnel audit counters, cross-asset BTC lead-lag filter & dashboard export
│   ├── config.rs          # Environment configuration parser
│   └── discord.rs         # Asynchronous webhook alerts
├── dashboard/
│   ├── server.py          # Zero-dependency multi-threaded Python HTTP telemetry server
│   └── index.html         # Real-time 4-asset readiness & gate-checklist web terminal
├── Dockerfile             # Multi-stage Debian Bookworm build (cached deps + minimal runtime)
└── docker-compose.yml     # 4-asset cluster + read-only dashboard service (OMV / Linux NAS ready)
```

---

## Configuration & Deployment

### 1. Environment Setup (`.env`)
Create a `.env` file in `hyperliquid-bot/` using an **API Agent Wallet** (recommended: allows order execution on behalf of your main wallet without withdrawal permissions):

```ini
HL_PRIVATE_KEY=0x...your_agent_wallet_private_key...
HL_WALLET_ADDRESS=0x...your_main_account_address...
HL_IS_MAINNET=true

# Sizing & Risk Parameters
HL_SIZING_MODE=dynamic
HL_EQUITY_FRACTION=0.25
HL_LEVERAGE=3
HL_IS_CROSS=true
HL_MAX_CONCURRENT_POSITIONS=2
HL_MAX_POSITION_SIZE_USDC=500.0
POSITION_SIZE_USDC=25.0

# Strategy Parameters
HL_TREND_ENGINE_MODE=live
HL_TREND_RISK_FRACTION=1.0
HL_DONCHIAN_PERIOD=12
HL_CHANDELIER_ATR_MULT=2.0

# Optional Alerts & Dashboard
DISCORD_WEBHOOK_URL=https://discord.com/api/webhooks/...
DASHBOARD_PORT=8088
```

### 2. Run Test Suite & Build Locally
```bash
cargo test
cargo build --release
```

### 3. Production Deployment (Docker Compose / OpenMediaVault)
```bash
docker compose up -d --build
```

- **Web Terminal:** `http://<server-ip>:8088`
- **Inspect Logs:**
  ```bash
  docker compose logs -f hl_bot_btc hl_bot_eth hl_bot_sol hl_bot_avax
  ```
