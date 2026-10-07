# Dual-Engine Regime-Switching Audit for Hyperliquid Perpetuals

## Executive assessment

A dual mean-reversion/trend architecture is **mathematically feasible but not automatically superior** to the current mean-reversion-plus-cash-veto system. Trend following has documented evidence across futures and crypto, including walk-forward crypto studies, but that evidence does not establish profitability for a 1-minute/15-minute Hyperliquid implementation after maker non-fills, taker fallback, funding, regime-classification lag, and BTC/ETH/SOL concentration.[^1][^2][^3]

The recommended design is not a binary switch directly from Engine A to Engine B. It is a **three-mode controller**:

1. `MEAN_REVERSION`
2. `NEUTRAL_TRANSITION`
3. `TREND`

The neutral state absorbs posterior uncertainty and prevents the system from reversing its trading logic at precisely the least reliable point—the regime boundary. Engine B should initially run in shadow mode, then at 25% of Engine A's per-trade risk, and scale only after independent out-of-sample evidence.

The preferred Engine B is a simple 15-minute, volatility-normalized Donchian/time-series-momentum model with 1-minute execution, ATR-based initial risk, and a monotone Chandelier exit. The HMM should be an eligibility and risk controller, not the directional predictor; direction should come from observable breakout and momentum rules.

## Feasibility and limitations

### Why the combination may help

Mean reversion and trend following are economically complementary: one monetizes oscillation while the other attempts to monetize persistence. Long-run time-series momentum research finds that an asset's own past return can predict its future return, while crypto-specific work reports that simple moving-average trend systems remained profitable across multiple walk-forward periods, although those results are based on much slower horizons and should not be transferred directly to intraday perpetuals.[^2][^1]

If the net return streams have correlation \(\rho_{AB}\), portfolio variance is:

\[
\sigma_p^2=w_A^2\sigma_A^2+w_B^2\sigma_B^2+2w_Aw_B\rho_{AB}\sigma_A\sigma_B.
\]

Diversification improves risk-adjusted performance only when Engine B contributes positive net expectancy and sufficiently low covariance after all implementation costs. A 75% Engine A win rate and 40% Engine B win rate are insufficient inputs: payoff distributions, tail losses, regime duration, turnover, fill probability, funding and covariance determine the result.

For example, a trend system with a 40% win rate and realized average winner/loser ratio of 3 has pre-cost expectancy:

\[
E[R]=0.40(3R)-0.60(R)=0.60R.
\]

That attractive arithmetic is meaningful only if the 3:1 ratio is observed out of sample after fees and if stopped losses remain close to \(R\). Gap-like moves, stop slippage and false HMM transitions can materially reduce it.

### Why it may not help

A mutually exclusive regime switch does not create the same diversification as simultaneously holding two independent strategies. Engine A and Engine B occupy different time segments, so the relevant test is whether Engine B improves the complete controller's returns during intervals where the cash-veto system would be flat.

The correct incremental series is:

\[
\Delta R_t=R_{\text{dual},t}-R_{\text{cash-veto},t}.
\]

Engine B is valuable only if \(E[\Delta R_t]>0\) with acceptable drawdown and turnover under purged walk-forward validation. The HMM must use causal filtered posteriors rather than retrospectively smoothed states; otherwise transition timing and reported performance contain look-ahead information.

Regime-switching studies also warn that transaction costs can erase switching benefits and that smoothing/dwell filters are often necessary to reduce turnover. Optimal trend rules under regime switching depend on state persistence and costs rather than on one universal signal setting.[^4][^5]

## State-machine design

### Recommended controller

Use distinct entry and exit thresholds plus minimum dwell time:

| Current mode | Candidate transition | Required evidence | Minimum dwell |
|---|---|---|---:|
| Mean reversion | Neutral | \(P_T\geq0.65\), severe shock, or Engine A stationarity failure | Immediate on severe shock; otherwise 1 closed 15m bar |
| Neutral | Trend | \(P_T\geq0.80\) for 2 consecutive closed 15m bars, plus directional breakout | 30 minutes of confirmation |
| Neutral | Mean reversion | \(P_C\geq0.75\) for 3 consecutive bars, shock cleared, OU eligibility restored | 45 minutes of confirmation |
| Trend | Neutral | \(P_T<0.55\) for 2 bars, or trend exit/invalidity | Trend mode held at least 4 bars unless risk stop fires |
| Any mode | Risk-off neutral | Severe shock conflicts with the proposed direction, stale data, model fault or reconciliation fault | Immediate |

Here \(P_T\) and \(P_C\) are causal forward-filter probabilities for trend and consolidation. These thresholds are starting hypotheses for walk-forward calibration, not constants justified by theory.

### Fast-path override

Waiting for two 15-minute confirmations can enter 30 minutes late. Allow an accelerated transition from neutral to trend when all conditions hold:

\[
P_T\geq0.90,
\]

\[
|Z_{15}|\geq3,
\]

and price closes beyond the prior Donchian boundary in the same direction. This permits a one-bar transition without allowing the HMM alone to choose direction.

A severe shock should not automatically authorize Engine B. Liquidation cascades often reverse violently. It should move the controller immediately from mean reversion to neutral; Engine B still requires breakout persistence or a post-shock continuation setup.

### Transition rules

- Never switch directly from an open Engine A position into an opposite Engine B position.
- Existing positions remain owned by their originating engine until reduced or closed under that engine's invalidation rules.
- A new engine cannot submit until the account is flat, all prior reduce-only orders are terminal, and the cluster lease is transferred atomically.
- Persist `controller_epoch`, `engine_owner`, posterior vector, transition reason and model version with every order intent.
- On model retraining, enter neutral and replay the new filter over historical bars before enabling either engine.
- A model disagreement or posterior in the no-man's-land between thresholds means cash, not a forced classification.

## Recommended trend model

### Signal timeframe

Use completed 15-minute bars for signal generation and one-minute data only for execution and risk monitoring. A trend signal built from one-minute bars is likely to produce excessive turnover relative to the available move, while the existing 15-minute HMM already defines the controller's information clock.

### Directional rule

Start with a deliberately simple channel breakout:

For a long candidate:

\[
C_t>\max(H_{t-20},\ldots,H_{t-1}),
\]

\[
\operatorname{EMA}_{16}(C_t)>\operatorname{EMA}_{64}(C_t),
\]

\[
\frac{\operatorname{EMA}_{16}(C_t)-\operatorname{EMA}_{16}(C_{t-4})}
{\operatorname{ATR}_{20,t}}>s_{\min}.
\]

Reverse the inequalities for shorts. A 20-bar 15-minute Donchian window represents five hours and provides a transparent starting point; test neighboring values 16, 20, 24 and 32 rather than optimizing a wide search. The EMA filter prevents taking a one-bar range break against the slower directional structure.

An alternative that is equally defensible is volatility-normalized time-series momentum:

\[
M_t=
\frac{\sum_{j=1}^{L} \lambda^{j-1}r_{t-j}}
{\hat\sigma_t\sqrt{\sum_{j=1}^{L}\lambda^{2(j-1)}}}.
\]

Use sign\((M_t)\) for direction and require \(|M_t|\) to exceed a frozen walk-forward threshold. Do not deploy Donchian, EMA crossover, MACD, ADX and HMM direction simultaneously; stacking correlated filters increases overfitting without necessarily adding information.

### Entry risk

Define one risk unit by the initial volatility stop:

\[
R_{px}=k_0\operatorname{ATR}_{20},
\qquad k_0\in[1.5,2.0].
\]

The initial stop must be a real reduce-only stop or monitored price-protected IOC path, not merely a local intention. Position notional is then based on loss at the stop rather than leverage:

\[
N=\min\left(
\frac{fE}{R_{px}/P_{entry}+c_{stress}},
N_{margin},N_{liquidity}
\right),
\]

where \(E\) is equity, \(f\) is per-trade equity risk, and \(c_{stress}\) includes expected stop slippage and fees.

## Execution blueprint

### Maker-first versus taker

An ALO order that would immediately match is canceled rather than filled, so post-only execution cannot guarantee participation in a fast breakout. Hyperliquid supplies real-time order updates and user fills over WebSocket, which should drive the execution state machine and timeout handling.[^6][^7][^8]

A maker-only trend engine creates selection bias: it tends to miss the strongest moves and fill when momentum stalls or reverses. Therefore use **maker-first, not maker-only** execution.

Recommended long-entry sequence:

1. At the completed 15-minute breakout, record decision mid, spread, L2 book, signal strength and maximum acceptable price.
2. Submit ALO at best bid only if spread and book freshness pass limits.
3. Permit a short TTL of 1–3 seconds; amend at most two or three times while the maximum price remains valid.
4. Cancel if the signal falls back inside the breakout channel, book data becomes stale, or estimated implementation shortfall breaches the budget.
5. If still unfilled and signal strength remains high, submit a price-protected IOC for only the required residual quantity.
6. If the IOC cannot fill inside the slippage cap, skip the trade. Do not repeatedly chase.

Use client order IDs and process all `orderUpdates` and `userFills` idempotently. ALO, IOC and GTC are distinct time-in-force modes, and partial-fill quantities must remain the source of truth for subsequent stop size.[^9]

### Selective taker rule

Given the stated tier, fee-only round trips are:

| Entry/exit path | Fee cost |
|---|---:|
| Maker / maker | 2.88 bps |
| Maker / taker | 5.76 bps |
| Taker / maker | 5.76 bps |
| Taker / taker | 8.64 bps |

Add half-spread, market impact, adverse selection and expected funding to obtain total cost \(C_{rt}\). Permit taker entry only when conservative expected continuation exceeds a multiple of that cost:

\[
E[\text{gross move}\mid X_t]\geq\gamma C_{rt},
\]

with \(\gamma=5\) as an initial minimum and \(\gamma=10\) preferred. This limits friction to at most 20% or 10% of expected gross edge, respectively.

Expressed in ATR units:

\[
k_{cost}=\frac{\gamma C_{rt}}{\operatorname{ATR}_{20}/P_t}.
\]

There is no universal minimum ATR multiple because the percentage ATR and slippage vary by asset and regime. Separately require round-trip friction to be no more than 10% of initial risk:

\[
C_{rt}\leq0.10\frac{R_{px}}{P_{entry}}.
\]

A 3:1 payoff objective with an initial stop of 1.5 ATR implies that successful trends must deliver roughly 4.5 ATR on average, but this is a validation target rather than a fixed take-profit.

### Exit execution

Profit exits can remain maker-first, but protective exits cannot. A stop is a risk-transfer instruction, so when triggered it should use reduce-only, price-protected IOC execution. The system should not wait for an ALO fill while the market moves through the risk boundary.

Capture fill markouts at 100 ms, 1 s, 5 s, 30 s and 60 s. Compare maker-first and immediate-IOC cohorts by:

- Fill probability
- Missed-opportunity return
- Decision-to-fill shortfall
- Five-second adverse selection
- Net expectancy after fees

## Exit management

### Preferred trailing stop

Use a monotone Chandelier-style stop rather than Parabolic SAR. For a long:

\[
Stop_t=
\max\left(
Stop_{t-1},
H^{since\ entry}_t-k_t\operatorname{ATR}_{20,t}
\right).
\]

For a short:

\[
Stop_t=
\min\left(
Stop_{t-1},
L^{since\ entry}_t+k_t\operatorname{ATR}_{20,t}
\right).
\]

The Chandelier construction anchors the exit to the favorable extreme and scales its distance using realized volatility; the common conceptual default is approximately 3 ATR, although the correct parameter is market- and timeframe-dependent.[^10][^11]

Recommended initial grid:

| Trade state | Suggested trail |
|---|---:|
| Entry to \(+1R\) | Initial stop at 1.75 ATR |
| \(+1R\) to \(+2R\) | Chandelier 3.25 ATR |
| Above \(+2R\) | Chandelier 2.75–3.00 ATR |
| HMM trend posterior below 0.55 for two bars | Tighten to 2.25 ATR or exit on channel reversal |

The stop may tighten but never loosen in absolute price terms. ATR should be frozen for the initial risk calculation, while the trailing ATR may update causally; cap abrupt ATR contraction so one quiet bar cannot pull the stop unrealistically close.

### Secondary exits

- **Channel exit:** close a long on a completed 15-minute close below the prior 10-bar low; reverse for shorts.
- **Failure time-stop:** if the trade has not achieved at least \(+0.5R\) maximum favorable excursion within 8 completed 15-minute bars, exit or halve it.
- **Regime exit:** posterior deterioration moves the controller to neutral, but a single posterior flicker does not override the price stop.
- **Funding veto:** block or reduce new exposure when projected funding over the expected holding horizon consumes a material fraction of \(R\); funding is charged hourly on Hyperliquid.[^12]

The Chandelier should be primary; the channel and HMM are slower thesis invalidators. Parabolic SAR's accelerating stop is more likely to tighten prematurely during volatile crypto pullbacks, while a plain EMA exit can surrender too much profit after an extended move.

## Portfolio and drawdown control

### Do not allocate by win rate

Win rate cannot determine capital allocation. Estimate each engine's net return distribution and covariance using synchronized timestamps, including zeros while inactive. Then calculate risk contributions:

\[
RC_i=w_i(\Sigma w)_i.
\]

A sensible mature target is approximately equal risk contribution, subject to a lower cap for the less-proven engine. During rollout, use:

| Phase | Engine B risk relative to Engine A | Requirement |
|---|---:|---|
| Shadow | 0% | At least 100 eligible signals with simulated execution |
| Pilot | 25% | Positive pessimistic shadow expectancy and no execution faults |
| Limited live | 50% | At least 100 completed net live trades; positive lower confidence bound preferred |
| Mature | Risk-parity, capped at 50% portfolio risk contribution | 200–300 out-of-sample trades and stable parameter neighborhoods |

Because the cluster lease permits only one BTC/ETH/SOL position, allocation means **risk permitted when each engine is active**, not simultaneous capital buckets.

### Leverage is not risk

Fixed 3x leverage does not define maximum loss. Stop distance, notional, slippage and gap behavior do. Use initial risk fractions rather than giving both engines permission to consume 3x independently.

Initial caps for a micro-account:

- Engine A: 0.20%–0.30% of equity modeled loss per trade.
- Engine B pilot: 0.10%–0.20% per trade.
- Mature Engine B: no more than 0.30% until substantial live evidence exists.
- Aggregate open modeled risk: at most 0.50% of equity under the one-position cluster rule.
- Daily loss pause: 1.0%.
- Build-level hard halt: 3.0%, leaving operational buffer below the requested 5% ceiling.

Cross margin shares collateral across positions, while isolated margin fences the allocated collateral. Even with a single cluster position, isolated margin is preferable for a new directional engine because it limits propagation from a software or stop failure. The liquidation boundary must remain far beyond the strategy stop under a stressed slippage scenario; liquidation must never be the intended stop.[^13][^14]

### Ruin probability

No fixed leverage partition can guarantee ruin probability below 0.01%. Kelly maximizes expected log growth but can produce severe drawdowns; fractional or explicitly drawdown-constrained variants are necessary.[^15]

Estimate ruin and drawdown through a regime-aware block bootstrap or fitted semi-Markov simulation that preserves:

- HMM regime durations and transition ordering
- Serially correlated trade clusters
- BTC/ETH/SOL common shocks
- Conditional fill probability and slippage
- Funding
- Stop gaps and outages
- Parameter uncertainty

For candidate sizing \(f\), require:

\[
\widehat P(MDD>5\%)<10^{-4}
\]

with an uncertainty allowance. In practice, estimating a 0.01% tail requires far more data than 50–100 trades; therefore deterministic risk caps and a 3% hard halt are the binding protections.

Use no more than one-eighth Kelly during early scaling:

\[
f_{live}=\min\left(
\frac18 f_{Kelly,shrunk},
f_{stop},f_{MDD},f_{liquidity}
\right).
\]

Shrink expected returns aggressively toward zero and recompute after costs. If any risk estimate depends on a small sample or unstable regime, the minimum-cap term should dominate.

## Validation protocol

Compare three complete, causal systems—not isolated trade lists:

1. Engine A with cash veto
2. Engine B alone
3. Dual controller with neutral transition state

Use anchored or rolling walk-forward splits. Train the HMM and all thresholds on each training fold, purge observations around fold boundaries by at least the longest feature horizon, and include a cooldown/embargo where overlapping positions could leak information.

Required outputs include:

- Net return, Sharpe, Sortino and maximum drawdown
- Expected shortfall and worst five-trade cluster
- Turnover and all-in basis-point cost
- HMM switches per week and P&L around switches
- Maker fill rate, taker fallback rate and missed-trend cost
- Conditional P&L by posterior decile and regime duration
- Engine A/Engine B synchronized correlation
- Incremental \(\Delta R_t\) versus cash veto
- Sensitivity across neighboring channel, ATR and hysteresis parameters

Do not approve Engine B because the combined backtest has a higher in-sample Sharpe. Require positive incremental expectancy in most walk-forward folds, acceptable pessimistic execution assumptions, and no dependence on one asset or a small number of crisis trades.

## Implementation order

### P0: Shadow engine

- Implement the three-mode controller with neutral transition state.
- Generate Engine B intents but submit no orders.
- Record counterfactual maker and IOC fills, book snapshots, funding and stop paths.
- Verify that no Engine A position can be re-owned by Engine B.

### P1: Execution experiment

- Randomize eligible low-size signals between maker-first and immediate price-protected IOC where ethically and operationally acceptable.
- Measure shortfall, fill probability and missed-opportunity cost.
- Freeze the breakout and exit parameter grid before examining results.

### P2: Limited deployment

- Enable Engine B at 25% of Engine A's risk.
- Prefer isolated margin.
- Enforce 1% daily pause and 3% hard halt.
- Do not increase size and modify signal parameters during the same evaluation window.

## Decision

The dual-engine proposal is worth developing, but **cash remains a valid third allocation**, not an inefficiency that must always be eliminated. The system should enter Engine B only when both the causal HMM and a separate directional breakout agree; ambiguous transitions stay neutral.

The recommended first production candidate is a 15-minute 20-bar Donchian breakout, 16/64 EMA directional filter, 1.5–2.0 ATR initial stop, 3 ATR monotone Chandelier trail, and maker-first execution with bounded IOC fallback. It should not receive full Engine A sizing until its incremental net return over the existing cash-veto architecture is established out of sample.

---

## References

1. [Time Series Momentum](https://w4.stern.nyu.edu/facdir/lpederse/papers/TSMOM_Slides.pdf)

2. [ar5iv.labs.arxiv.org › html › 2009[2009.12155] A Decade of Evidence of Trend Following ...](https://ar5iv.labs.arxiv.org/html/2009.12155) - Cryptocurrency markets have many of the characteristics of 20th century commodities markets, making ...

3. [Cryptocurrency Trading: A Comprehensive Survey - Springer](https://link.springer.com/chapter/10.1007/978-981-96-6839-7_4?error=cookies_not_supported&code=dbc983a0-8d7e-40fe-a6c2-3fe804c8a1b6) - In recent years, the tendency of the number ofinancial institutions to include cryptocurrencies in t...

4. [Markov Regime-Switching Model - SignalTrace](https://www.signaltrace.wiki/markov-model/Models/Markov-Regime-Switching-Model) - Markov Regime-Switching Model The Markov regime-switching model is an econometric model in which the...

5. [Optimal Trend-Following Rules in Two-State Regime-Switching ...](https://link.springer.com/chapter/10.1007/978-3-031-90907-8_12?error=cookies_not_supported&code=39d6beb2-5ec7-4800-a58d-d0617454f617) - This chapter derives the optimal trend-following rule in a two-state regime-switching model, where b...

6. [Subscriptions | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)

7. [Websocket | Hyperliquid Docs](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket)

8. [exchange-endpoint](https://hyperliquid.gitbook.io/Hyperliquid-docs/for-developers/api/exchange-endpoint)

9. [https://www.quicknode.com/docs/hyperliquid/dataset...](https://www.quicknode.com/docs/hyperliquid/datasets/orders.md)

10. [Chandelier Exit: Concept, Mechanics, and Strategic Insights](https://www.tradingview.com/chart/XAUUSD/C4W1oVsL-Chandelier-Exit-Concept-Mechanics-and-Strategic-Insights/) - ● 📊 Overview and Conceptual Origin - The Chandelier Exit stands among the most enduring volatility-b...

11. [www.quantum-algo.com · blog · guidesChandelier Exit: Complete Trading Guide - quantum-algo.com](https://www.quantum-algo.com/blog/guides/chandelier-exit-complete-guide/) - The Chandelier Exit is an ATR trailing stop that hangs below the highest high. Learn how it works, h...

12. [Hyperliquid Fees Explained: Maker, Taker, Funding, and Withdrawal ...](https://eco.com/support/en/articles/15191998-hyperliquid-fees-explained-maker-taker-funding-and-withdrawal-in-2026) - Hyperliquid base perp fees are 0.015% maker and 0.045% taker, with volume tiers and HYPE staking dis...

13. [How Liquidation Works, and How to Size a Position](https://learnhyper.com/access/liquidation/) - Maintenance margin is half the initial margin at max leverage. The liquidation formula, a sizing met...

14. [Hyperliquid Margin and Leverage 2026 | Support - Eco](https://eco.com/support/en/articles/15247720-hyperliquid-margin-and-leverage-2026) - Hyperliquid offers isolated and cross margin with up to 50x leverage on BTC and ETH and lower caps o...

15. [On Kelly Betting: Some Limitations](https://ar5iv.labs.arxiv.org/html/1710.01787) - The focal point of this paper is the so-called Kelly Criterion, a prescription for optimal resource ...

