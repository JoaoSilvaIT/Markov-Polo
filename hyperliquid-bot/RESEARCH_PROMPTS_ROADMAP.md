# Deep Research Agent Prompts: Hyperliquid Dual-Engine Quantitative Roadmap

Below are four self-contained, institutional-grade research briefs ready to copy-paste directly into your Deep Research Agents. Each prompt includes full architectural context from `hyperliquid-bot`, mathematical constraints, Hyperliquid L1 venue mechanics, and required deliverables.

---

## Prompt 1: Cross-Asset BTC $\to$ Altcoin Lead-Lag Transfer Entropy & Beta-Neutral Pair Viability

```markdown
# Quantitative Research Brief: BTC-to-Altcoin Lead-Lag Transfer Entropy & Beta-Neutral Spread Modeling on Hyperliquid Perpetuals

## 1. System Context & Production Architecture
We operate an autonomous, multi-asset algorithmic trading system in Rust (`tokio`) on the Hyperliquid L1 decentralized perpetual futures exchange across four assets: **BTC, ETH, SOL, and AVAX**.
- **Account Scale**: Currently scaling from $150 USDC (live testing) to $1,000–$2,500 USDC equity, using 3x cross margin and a portfolio concurrency cap of `HL_MAX_CONCURRENT_POSITIONS = 2`.
- **Dual-Engine Regime Controller**:
  - **Engine A (Mean Reversion)**: Continuous-time Ornstein-Uhlenbeck (O-U) model in log-price space with Phillips-Yu (2005) Jackknife bias correction, 1.50σ (Tier 1) and 1.25σ (Tier 2) Bertram entry bands, reversal-bounce confirmation, and 100% Post-Only (`Alo`) maker execution.
  - **Engine B (Trend Momentum)**: 12-to-20 period 15-minute Donchian channel breakout with EMA 16/64 alignment, multiscale distance-weighted L2 Order Book Imbalance (OBI) EWMAs, and a monotone 4-tier Chandelier ATR trailing stop ratchet.
- **Current Cross-Asset Filter**: We recently implemented a heuristic BTC Lead-Lag Impulse Filter that vetoes altcoin (`ETH`, `SOL`, `AVAX`) Engine A Longs when `BTC Z_15 <= -1.8` or `(BTC Z_15 <= -1.2 && BTC OBI_1s <= -0.25)`.

## 2. Core Research Questions to Investigate
1. **Sub-Minute Lead-Lag Transfer Entropy & Impulse Response**:
   - In cryptocurrency perpetuals, informed aggressive order flow (Taker sweeps and L2 book depletion) typically strikes BTC first before propagating to ETH, SOL, and AVAX over $\Delta t \in [1\text{s}, 60\text{s}]$.
   - Formulate a causal, low-latency lead-lag estimator (e.g., Hayashi-Yoshida cross-correlation on asynchronous L2 micro-price changes, Lead-Lag Ratio [LLR], or linear Granger/Transfer Entropy on standardized OBI + log-returns) suitable for real-time Rust execution using 5-second L2 snapshots and 1-minute bars.
   - What are the empirically supported lead times and beta-amplification multipliers ($\beta_{\text{ETH/BTC}}, \beta_{\text{SOL/BTC}}, \beta_{\text{AVAX/BTC}}$) during (a) normal consolidation regimes vs. (b) tail liquidation cascades?

2. **Single-Leg Beta-Gated O-U vs. 2-Leg Beta-Neutral Residual O-U ($Y_t = \ln P_{\text{Alt}, t} - \alpha_t - \beta_t \ln P_{\text{BTC}, t}$)**:
   - Because our cluster allows `2` concurrent positions and $11.00 minimum notional per leg (only $22.00 total notional required for a 2-leg pair, well within a $150–$1,000 account), we can either:
     - **Option 1 (Current)**: Trade single-leg directional O-U on the altcoin, gated by a BTC impulse veto; OR
     - **Option 2 (Relative-Value Spread)**: Fit the O-U process directly to the Kalman-filtered or EWMA-regressed log-price residual $Y_t = \ln P_{\text{Alt}, t} - \hat{\beta}_t \ln P_{\text{BTC}, t}$, and execute a beta-weighted pair (or single-leg trade strictly driven by the idiosyncratic residual $Y_t$ rather than raw log-price $\ln P_{\text{Alt}, t}$).
   - Compare Option 1 and Option 2 mathematically and empirically after accounting for Hyperliquid's Tier 1 fee structure (1.0 bps Maker / 3.5 bps Taker), hourly funding differential, and leg-execution synchronization risk (when Leg 1 fills as `Alo` Maker and Leg 2 remains unfilled).

## 3. Required Deliverables
1. **Exact Mathematical Formulation** of a causal Cross-Asset Impulse Score $I_{\text{BTC}\to k}(t)$ combining BTC short-horizon return, BTC multiscale L2 OBI, and dynamic rolling beta $\hat{\beta}_{k, t}$.
2. **Kalman Filter / Online Recursive OLS Specification** for decomposing altcoin log-price movements into systematic BTC factor shock ($\beta_{k,t} r_{\text{BTC}, t}$) + idiosyncratic O-U residual ($\epsilon_{k,t}$), so Engine A only mean-reverts idiosyncratic dislocations and never fades a systematic BTC factor move.
3. **Concrete Rust Implementation Blueprint** (structs, state updates, and parameter defaults for `ETH`, `SOL`, and `AVAX`).
```

---

## Prompt 2: Hyperliquid Hourly Funding Rate, Open Interest Crowding & Squeeze Regime Prediction

```markdown
# Quantitative Research Brief: Hourly Funding Rate & Open Interest Crowding Integration for HMM Regime Switching & O-U Drift on Hyperliquid

## 1. System Context & Production Architecture
We run an autonomous Rust trading bot on Hyperliquid Perpetuals (`BTC`, `ETH`, `SOL`, `AVAX`) driven by a 3-state, 4-feature Gaussian Hidden Markov Model (`GaussianHMM` with Ledoit-Wolf variance shrinkage and semantic state sorting: `State 0 = Consolidation`, `State 1 = Trend`, `State 2 = Volatility Shock`).
- Currently, the 4 HMM features (computed on 15m bars) are strictly price-based:
  1. Volatility-normalized return $r_t / \sigma_t$
  2. Log realized volatility $\ln(\sigma_t)$
  3. Kaufman Efficiency Ratio $\text{KER}_{16}(t)$
  4. Absolute directional drift $|D_{16}(t)|$
- We also stream Hyperliquid's `ActiveAssetCtx` WebSocket channel (`PerpsAssetCtx`), which provides real-time **hourly `funding`**, **`open_interest`**, **`oracle_px`**, and **`mark_px`** (`mark - oracle` basis premium).
- On Hyperliquid, funding is settled **every hour** at $1/8\text{th}$ of the 8-hour rate based on the mark-oracle premium index plus a clamped interest rate component.

## 2. Core Research Questions to Investigate
1. **Distinguishing Spot-Driven Healthy Trends from Leveraged Blow-Off Squeezes**:
   - Price-only HMMs cannot distinguish between:
     - **Regime T1 (Healthy Spot-Led Trend)**: Price rises with moderate/neutral funding and steady Open Interest (OI) absorption $\to$ high probability of trend continuation (ideal for Engine B Donchian breakout).
     - **Regime T2 (Crowded Perp-Led Exhaustion / Squeeze Setup)**: Price stalls (`KER` dropping) while hourly funding spikes to extremes ($|F_{1\text{h}}| > 3\text{–}8\text{ bps/hr}$) and OI surges $\to$ high probability of a violent liquidation cascade in the opposite direction (fatal for late Engine B breakouts, highly lucrative for Engine A contrarian entries once the flush begins).
   - Should `funding` and `open_interest` delta ($\Delta \ln \text{OI}_t$) be added as a 5th/6th emission dimension in `GaussianHMM`, or modeled as an explicit exogenous **Crowding / Squeeze Hazard Multiplier** in `RegimeController`? (Evaluate the curse of dimensionality and $N_{\text{eff}} \ge 40$ rare-state rejection constraints on a 1,000-bar 15m rolling training window).

2. **Continuous-Time O-U SDE under Stochastic Carry ($F_t$)**:
   - Derive the exact adjustment to the Bertram optimal entry/exit thresholds $(a^*, b^*)$ when holding the perpetual contract incurs a continuous carry rate $c_t = \text{side} \cdot F_{1\text{h}}(t)$ per hour.
   - How should the asymmetric shift of $(a_{\text{long}}^*, a_{\text{short}}^*)$ be parameterized so that positive carry trades require a smaller price excursion (e.g., $1.30\sigma$) while negative carry trades require a deeper price excursion (e.g., $1.75\sigma$)?

## 3. Required Deliverables
1. **Theoretical & Empirical Trade-off Analysis**: 5D/6D HMM vs. 4D Price HMM + Exogenous Funding/OI Hazard Overlay (including impact on transition matrix stability and Baum-Welch convergence).
2. **Closed-Form Carry-Adjusted O-U Threshold Equations** for Long and Short entries/exits incorporating Hyperliquid's hourly funding schedule and mark-oracle premium.
3. **Production-Ready Rust Equations & Thresholds** for `BTC`, `ETH`, `SOL`, and `AVAX` (including Z-score normalization of funding and $\Delta \text{OI}$ across assets with different baseline betas).
```

---

## Prompt 3: Micro-Price Queue-Dancing, Spread-Stepping & Adverse Selection Mitigation on Hyperliquid L1

```markdown
# Quantitative Research Brief: Optimal Post-Only (Alo) Queue Placement, Micro-Price Re-Pegging, and Toxic Flow Cancellation on Hyperliquid CLOB

## 1. System Context & Production Architecture
Our Rust execution engine on Hyperliquid L1 (`BTC`, `ETH`, `SOL`, `AVAX`) executes 100% of entries via `tif: "Alo"` (Post-Only Maker, 1.0 bps fee) to avoid the 3.5 bps Taker fee.
- **L2 Telemetry Available**: Real-time WebSocket `L2Book` (top 20 levels, ~500ms–5.4s cadence), tracking exponential distance-weighted Order Book Imbalance (`tau_bps = 5.0`), physical-time EWMAs (`tau_1 = 10s`, `tau_2 = 30s`, `tau_3 = 60s`), Stoikov micro-price $P_{\text{micro}} = P_{\text{mid}} + I_{\text{inst}} \cdot (\text{spread}/2)$, and inter-arrival latency percentiles (`hl_l2_telemetry_*.jsonl`).
- **Counterfactual & Candidate Logs**: We log every vetoed and executed trade with MFE, MAE, touch-only vs. 1-tick trade-through fills in `hl_counterfactual_trades.jsonl` and `hl_candidates_*.jsonl`.
- **Current Execution Mechanics**:
  - When spread $\ge 2$ ticks, we step 1 tick inside the spread (`top_bid + tick` for Buy, `top_ask - tick` for Sell) to achieve FIFO Queue Rank #1.
  - While resting on the book, `Fast-Sentinel` aborts the order if OBI turns severely toxic (`instant_obi <= -0.45 && ewma_1s_obi <= -0.35` for Longs).
  - Unfilled entry orders expire after a 5-minute TTL.

## 2. Core Research Questions to Investigate
1. **Optimal BBO Re-Pegging ("Queue Dancing") vs. Static Limit Waiting**:
   - When Engine A confirms a reversal bounce (or Engine B confirms a 1m Donchian breakout) and places an `Alo` bid at `top_bid`, a genuine V-recovery often causes `top_bid` to tick upward within 2–15 seconds, leaving our resting bid stranded behind the market ("missed-winner selection bias").
   - Conversely, chasing `top_bid` upward too aggressively surrenders the O-U net edge (`>= 20 bps` hurdle) and increases the probability of buying the top of a micro-bounce.
   - Formulate a quantitative **Expected Value of Repricing ($\Delta \text{EV}_{\text{repeg}}$)** rule as a function of:
     - Remaining net edge to target $\mu$ (in bps),
     - Stoikov micro-price divergence $(P_{\text{micro}} - P_{\text{order}}) / \text{spread}$,
     - Multiscale OBI slope $(\text{OBI}_{\text{inst}} - \text{OBI}_{1\text{s}})$, and
     - Elapsed order age $\tau_{\text{age}} \in [0\text{s}, 300\text{s}]$.

2. **Hyperliquid L1 400ms ALO Write-Priority & Cancellation Latency Dynamics**:
   - Incorporate Hyperliquid's specific matching engine mechanics (price-time FIFO priority, 400ms ALO write-priority window, and gasless EIP-712 cancel/replace rate limits) to determine the optimal minimum cooldown between order amendments and the maximum number of re-pegs per trade.

3. **Dynamic Toxic-Flow Cancellation Thresholds**:
   - Calibrate the exact joint condition on $(\text{OBI}_{\text{inst}}, \text{OBI}_{\tau_1}, \Delta P_{\text{micro}})$ that maximizes avoided adverse-selection losses (cancelling a resting bid milliseconds before a large sell sweep clears the level) while minimizing false cancellations during normal maker fills.

## 3. Required Deliverables
1. **Explicit Decision Rule & State Machine for BBO Re-Pegging** (when to hold queue position, when to step up to the new `best_bid`, and when to abort/cancel).
2. **Optimal Entry TTL Curve**: Should Entry TTL be a function of O-U half-life $t_{1/2}$ (e.g., $\min(180\text{s}, 0.10 \cdot t_{1/2})$) rather than a static 5 minutes?
3. **Empirical Calibration Protocol** using our existing `hl_counterfactual_trades.jsonl` and `hl_l2_telemetry_*.jsonl` schemas.
```

---

## Prompt 4: Intraday Seasonality, Session-Conditioned Priors & Micro-Account Scaling ($150 $\to$ $1,000+ USDC)

```markdown
# Quantitative Research Brief: Intraday Microstructure Seasonality & Volatility-Scaled Kelly Compounding from $150 to $1,000+ on Hyperliquid

## 1. System Context & Production Architecture
We run a 24/7 autonomous Dual-Engine (O-U Mean Reversion + Donchian/Chandelier Trend) trading system on Hyperliquid Perpetuals across `BTC`, `ETH`, `SOL`, and `AVAX`.
- **Account Trajectory**: Currently running live validation on a **$150 USDC** micro-account, with planned capital scaling to **$1,000+ USDC** in the immediate next phase.
- **Current Sizing Engine**: Volatility-normalized Fractional-Kelly sizing (`calculate_risk_scaled_notional`), risking `0.75%` of account equity per Tier-1 Engine A trade, `0.375%` per Tier-2 Engine A trade, and `0.60%` per Engine B trend trade, subject to Hyperliquid's `$10.00` (`$11.00` safety floor) minimum notional and `max_concurrent_positions = 2`.

## 2. Core Research Questions to Investigate
1. **Intraday UTC Session Seasonality in Crypto Perpetuals**:
   - Cryptocurrency perpetual volatility, order-book depth, and autocorrelation exhibit strong diurnal and weekly seasonality across three distinct regimes:
     - **Session 1 (US Macro & Equity Open: `13:00–17:00 UTC`)**: High jump-diffusion intensity, macro news shocks, directional trend expansion, and elevated O-U breakdown risk.
     - **Session 2 (US Afternoon / EU Close: `17:00–22:00 UTC`)**: Post-impulse trend continuation or high-amplitude mean reversion.
     - **Session 3 (Asian / Off-Peak / Weekend Liquidity: `00:00–08:00 UTC` & Saturday/Sunday)**: Lower realized volatility, tighter range-bound stationarity, and higher O-U mean-reversion win rates, punctuated by occasional thin-book liquidity sweeps.
   - How should `RegimeController` transition thresholds ($P_C, P_T$), O-U entry band multipliers ($k_\sigma \in [1.25, 2.00]$), and Chandelier ATR multipliers ($k_{\text{ATR}}$) be conditioned on the UTC session and day-of-week without overfitting?

2. **Optimal Micro-Account Scaling Trajectory ($150 $\to$ $500 $\to$ $1,000 $\to$ $2,500)**:
   - At `$150` equity, the `$11.00` minimum notional floor (`7.3%` of account equity) interacts with tick/lot quantization (`0.01 SOL`, `0.01 AVAX`) to create discrete step-function jumps in effective risk whenever stop distances exceed `2.5%`.
   - At `$1,000` equity, the `$11.00` floor is only `1.1%` of equity, unlocking continuous fractional sizing and up to 3–4 concurrent portfolio slots (`HL_MAX_CONCURRENT_POSITIONS`).
   - Derive the mathematically optimal **Equity Milestone Scaling Schedule** specifying exact transitions for:
     - `HL_MAX_CONCURRENT_POSITIONS` ($1 \to 2 \to 3$),
     - Per-trade equity risk fraction $f_{\text{risk}}$ under drawdown-constrained Kelly ($\Pr(\text{MDD} > 10\%) < 0.01$), and
     - Dynamic drawdown de-leveraging (how to automatically scale $f_{\text{risk}}$ down during a $-5\%$ equity drawdown and restore it upon high-water-mark recovery).

## 3. Required Deliverables
1. **UTC Session-Conditioned Parameter Matrix** (explicit multipliers or Bayesian prior adjustments for `RegimeController`, Engine A $\sigma$-bands, and Engine B Donchian/Chandelier parameters across UTC hours and weekends).
2. **Equity-Tier Compounding & Drawdown-Control Schedule** for `$150`, `$300`, `$500`, `$1,000`, and `$2,500` account milestones.
3. **Rust Implementation Specification** for a `DrawdownGovernor` and `SessionCalendar` module that integrates directly into `main.rs` and `controller.rs`.
```
