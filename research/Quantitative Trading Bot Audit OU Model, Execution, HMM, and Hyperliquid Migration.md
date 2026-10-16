# Quantitative Trading Bot Audit

## Executive verdict

The current build is **not ready for unattended capital scaling or direct migration to leveraged perpetuals**. The two most urgent risks are not the OU equations: they are order-state accounting and false confidence in stationarity. In particular, the bot records an order as a “position” before it fills, ignores partial fills, can try to sell the pre-fee order quantity, and may remove state even when a market sell fails.[^1][^2]

The AR(1)-to-OU transformation is mathematically correct under an equally spaced, stationary Gaussian OU model, but the implementation applies it to raw single-asset prices, accepts any fitted coefficient strictly between zero and one, and estimates parameters using the same closing observation that triggers entry. Those choices can manufacture apparent reversion in a near-unit-root series and make the threshold adapt to the very excursion being tested.[^3]

The HMM should be used as a **probabilistic entry veto and risk escalation input**, not as a one-bar liquidation switch. Its current implementation has a training/live timeframe mismatch, an invalid state-label interpretation, and a numerically unsafe transition update; simply replacing `let regime = 0` with `predict()` would create new failure modes rather than fix the dormant model.[^4][^2]

For the next 48–72 hours, treat Binance Spot as an **execution and reliability validation**, not a profitability test. Eight trades cannot establish edge, and the previous observations were generated under known bugs. Freeze the strategy version, fix the P0 execution defects below, and collect event-level fills, commissions, order states, model snapshots, and reasons for every action.

## Highest-risk findings

| Severity | Finding | Why it matters | Required action |
|---|---|---|---|
| Critical | State is created immediately after submitting the entry, before any fill.[^1] | “Has position” means either an unfilled order, a partial fill, or a true position; risk actions cannot distinguish them. | Implement an explicit order/position state machine driven by execution events. |
| Critical | Only `FILLED` is handled; `PARTIALLY_FILLED` is ignored.[^2] | A partial entry can remain exposed for up to indefinitely with no take-profit, while the stored amount remains the requested full amount. | Track cumulative filled quantity and protect each filled slice immediately. |
| Critical | The stored sell quantity equals requested buy quantity, although Binance may charge a spot-buy fee in the received base asset.[^1][^5] | A take-profit sell can fail with insufficient balance unless fees are paid separately in BNB. | Derive net base quantity from fills and commissions, never from requested quantity. |
| Critical | Emergency flatten fetches and sells the asset’s entire free account balance, not the strategy-attributable quantity.[^1] | It can liquidate manual holdings or inventory owned by another strategy. | Use a dedicated subaccount or a per-fill inventory ledger; sell only reconciled bot inventory. |
| Critical | Market-sell errors are discarded and state is removed anyway.[^1] | The process can report itself flat while real exposure remains. | Require acknowledged terminal execution, then reconcile balance and fills before closing state. |
| Critical | Hard-coded decimal precision plus a floating-point epsilon replaces exchange filters.[^1] | Quantity and price rules are based on `stepSize`, `tickSize`, and notional filters, not a symbol-name precision table.[^6] | Cache `exchangeInfo`; quantize with fixed-point decimal/integer ticks and always round sell quantity down. |
| High | The model includes the current close in calibration and then tests that same close against the new threshold.[^2] | This is causal live, but it is in-sample signal leakage: an outlier changes its own mean and volatility threshold. | Fit through bar `t-1`, evaluate bar `t`, and submit only after `t` closes. |
| High | The entry uses a normal limit order after price is already at or below the limit.[^1][^2] | The order can cross the book and become a taker despite being described as maker execution. | Use `LIMIT_MAKER`; Binance rejects it rather than allowing an immediate taker fill.[^7] |
| High | Fill checks occur only on one-minute bar closes.[^2] | A filled entry can remain unprotected for nearly a minute, and partial fills are not protected at all. | Consume Binance User Data Stream `executionReport` events in real time.[^8][^9] |
| High | Three independent containers do not enforce a portfolio-wide exposure cap through an in-process `state.len()` check.[^1][^2] | Separate processes have separate locks and memory; a shared JSON file would instead introduce cross-process races. | Add a central portfolio-risk service or database transaction controlling reservations and gross exposure. |
| High | There is no price stop, time stop, or maximum-loss invariant.[^1] | A structural break may remain below the 1.5% slope trigger or occur between candle closes. | Add exchange-side catastrophe protection, a model time stop, and account-level loss limits. |

## OU estimation

### What is correct

For an exact OU process sampled every `dt`, the conditional transition is

\[
X_{t+\Delta}\mid X_t \sim \mathcal{N}\left(\mu+(X_t-\mu)e^{-\theta\Delta},\; \frac{\sigma^2}{2\theta}(1-e^{-2\theta\Delta})\right).
\]

Writing this as `X[t] = a + b X[t-1] + epsilon[t]` gives

\[
b=e^{-\theta\Delta},\qquad \theta=-\frac{\ln b}{\Delta},\qquad \mu=\frac{a}{1-b},
\]

and

\[
\sigma=\sigma_\epsilon\sqrt{\frac{2\theta}{1-b^2}}.
\]

Those mappings in `ou_process.rs` are correct. For regular observations with Gaussian conditional errors and no observation noise, OLS for `a` and `b` is also the conditional Gaussian MLE; replacing it with a numerical optimizer alone will not solve the important problems.[^3]

### What is not robust

Raw BTC, ETH, and SOL prices do not have a compelling permanent equilibrium level. Mean-reversion frameworks are normally safer on a stationary residual, relative-value spread, log-price deviation, or cointegrating relationship; a fitted `0 < b < 1` is not itself proof of stationarity, especially when `b` is close to one.[^10][^11]

A 480-observation near-unit-root regression has meaningful finite-sample uncertainty. Downward bias in the fitted persistence can create a positive `theta`, an unrealistically short half-life, and a seemingly finite equilibrium mean even when the data are practically indistinguishable from a random walk. Exact likelihood methods are standard for OU estimation, but finite-span bias and model misspecification remain relevant even under MLE.[^12][^13]

The implementation also computes variance and covariance as differences between large raw moments. With an approximately 86,000 BTC price and comparatively small intrawindow movement, `E[X²] - E[X]²` is needlessly vulnerable to cancellation. Statistical calculations should use centered sums, Welford-style updates, or a stable regression package; execution arithmetic should use fixed-point decimals, while `f64` remains appropriate for model estimation.

### Recommended estimator

Use the following gate before any OU threshold is tradable:

1. Transform the modeled series. Prefer a log residual such as `log(price) - slow_reference`, or a cointegrated multi-asset residual, rather than the raw price level.
2. Estimate only on bars through `t-1`; evaluate the signal on `t`.
3. Fit the exact OU conditional likelihood or a numerically stable AR(1), then bootstrap the entire rolling calibration to obtain uncertainty in `b`, `theta`, `mu`, and half-life.
4. Require stationarity evidence rather than `b < 1` alone: use an ADF-type unit-root rejection, a complementary stationarity check, parameter stability across subwindows, and a structural-break test. Such tests are filters, not guarantees, because finite samples and breaks can still mislead.[^11][^10]
5. Require a plausible half-life range and enough effective cycles. For example, reject a model whose upper confidence bound includes `b = 1`, whose half-life is longer than the trade horizon, or whose calibration window contains too few estimated half-lives.
6. Reject non-finite values, tiny variance, implausible `mu`, and thresholds outside current exchange price bands.

A Kalman filter is useful only if the intended model includes a latent time-varying mean, observation noise, missing observations, or irregular timestamps. A local-level state-space model can track a slowly moving equilibrium, but then the process is no longer the fixed-mean stationary OU assumed by the current threshold formula. Start with a bias-aware exact OU fit and robust validation; add a Kalman model only after defining and testing that different hypothesis.

## Threshold optimization

The function named `calculate_optimal_thresholds` is not Bertram’s optimizer. It applies `max(2 sigma_eq, 2.5 cost)` and exits at the estimated mean. Bertram’s framework uses first-passage-time moments and optimizes expected return or Sharpe per unit time; under its assumptions, the optimum entry and exit levels are symmetric around the equilibrium mean, rather than necessarily entering two standard deviations below the mean and exiting exactly at the mean.[^14][^15][^3]

Bertram’s published setup also concerns an exponential OU/log-price formulation and assumes continuing stationarity, an assumption explicitly identified as problematic in later work. Therefore, there is no universal theorem that says `2.0 sigma_eq` is optimal for these three assets, this horizon, long-only spot execution, or the observed fill process.[^16][^14]

The replacement should numerically optimize a net objective using the same rules that will trade live:

\[
(a^*,m^*)=\arg\max_{a<m}\frac{\mathbb{E}[\text{net PnL}(a,m)]}{\sqrt{\operatorname{Var}[\text{net PnL}(a,m)]}},
\]

subject to a maximum holding time, a stop policy, minimum fill probability, exchange filters, and regime eligibility. Estimate the objective walk-forward with event-level bid/ask data; do not optimize on candle closes that assume a passive limit filled merely because a bar touched its price.

At minimum, replace the single `transaction_cost` argument with a cost model containing entry role, exit role, spread, expected adverse selection, slippage, and funding where applicable. The current code passes one side’s assumed 0.10% fee and multiplies it by 2.5, so the minimum deviation is 0.25% of price—not 2.5 times a two-sided 0.20% baseline round trip. Binance exposes symbol- and account-specific commission rates through `GET /api/v3/account/commission`; use that instead of a hard-coded fee.[^2][^17][^18][^3]

A practical first validation is not to search continuously for an “optimal” z-score. Freeze a small grid such as entry z-scores from 1.5 to 3.0 and exits from 0 to the opposite side of the mean, then use purged walk-forward evaluation. Select a broad stable plateau, not the single best cell, and include rejected/post-only orders and partial fills.

## HMM integration

### Current defects

The cold path trains on 1,000 completed 15-minute closes, while the hot path holds 480 one-minute closes. Feeding one-minute features to parameters trained on 15-minute features would be a distribution shift; instead, keep a separate rolling 15-minute feature stream and update the regime only when a 15-minute bar closes.[^4][^2]

`sort_by_sharpe()` sorts states from lowest to highest return-to-volatility ratio, but `main.rs` defines state zero as “Consolidation.” State zero is therefore the lowest-Sharpe state, potentially the bearish/cascade state—not the calm state. Label states from their fitted emission properties: calm should have low expected absolute return and low volatility; stress should have high volatility and/or strongly negative return; the remaining state can be directional or transitional.[^2][^4]

The Baum–Welch transition update exponentiates an unnormalized joint log probability without subtracting the sequence log-likelihood. Over a long sequence this can underflow to zero, after which row normalization cannot recover valid transition estimates. Implement a scaled forward-backward algorithm or log-space `xi` normalization, add multiple initializations, use a convergence criterion, place defensible variance floors in standardized feature space, and validate all loaded weights.[^4]

### Safe live policy

Use a causal forward-filter posterior, not a hard Viterbi label over the entire recent history. With three states and two features, one online update is only `O(K²)` and adds negligible latency. Persist the training timeframe, feature definitions, scaler, model version, state semantics, and training timestamp together with the weights.

A non-conflicting state machine could be:

| Condition | Pending entry | Open position |
|---|---|---|
| `P(calm) >= 0.70` for two completed HMM bars and OU gates pass | May enter | Normal management |
| `P(calm) < 0.50` | Cancel unfilled remainder | Do not automatically market-sell; tighten time/risk checks |
| `P(stress) >= 0.85` for two bars | Block entry | Flatten only if confirmed by price-risk, drift, or maximum-loss rule |
| Data/model stale or invalid | Cancel entry | Reduce risk under a predefined fail-safe policy |

The exact probabilities must be calibrated out of sample; the values above describe hysteresis, not claimed optimums. The key principle is that the HMM vetoes new exposure sooner than it liquidates existing exposure. This prevents a noisy one-bar state flip from repeatedly paying taker fees at the worst point of a mean-reversion cycle.

The fixed 1.5% drift rule should also become volatility-aware. A 1.5% eight-hour move has very different statistical meaning for BTC and SOL. Use a regression slope t-statistic with autocorrelation-robust errors, or normalize total fitted drift by expected random displacement over the window, then calibrate separate “block entry” and “force exit” thresholds with hysteresis.

## Binance Spot phase

### Immediate execution design

Binance’s User Data Stream pushes order updates in real time through `executionReport`, including cumulative fill quantity, last fill, commission, maker status, and partial/full state. One-minute REST polling is therefore unnecessary as the primary mechanism. It is not chiefly a capital-utilization issue at 23% gross exposure; it is a naked-position and stale-state issue.[^8][^9]

Implement this event flow:

1. Reserve portfolio risk centrally before order submission.
2. Submit a deterministic `clientOrderId` and persist an intent record before or atomically with submission.
3. Use `LIMIT_MAKER` for a maker-only entry. If rejected because it would cross, either skip or explicitly route a taker order only when expected net edge exceeds the taker hurdle.[^7]
4. On every partial fill, update average price, cumulative gross base, commissions by asset, and net sellable base.
5. Place or amend protection for the net filled quantity immediately.
6. On cancel, wait for a terminal execution report and reconcile fills; a cancel and a fill can race.
7. Mark the trade closed only after the exit is terminal and balances/fills reconcile.
8. On WebSocket disconnect, reconnect and query open orders/recent fills before resuming. Use periodic REST reconciliation as a backstop, not as the event engine.

Cache symbol filters and quantize dynamically. Binance defines quantities through `LOT_SIZE.stepSize` and validates notional through `MIN_NOTIONAL`/`NOTIONAL`; price must likewise respect tick size. Use `rust_decimal` or integer tick/lot units and avoid epsilon-based rounding.[^6][^19]

### Reducing taker drag

A risk-tiered exit is preferable to “always market” or “always wait”:

- **Soft invalidation:** cancel entry remainder, post a maker-only exit at the best ask, allow a short grace period, then re-evaluate.
- **Medium invalidation:** cancel existing exits, send a marketable IOC limit with an explicit worst acceptable price.
- **Hard stop, stale data, or connectivity uncertainty:** prioritize certainty and use the fastest controlled exit; a taker fee is cheaper than unbounded exposure.

At a 12 USDC position, baseline 0.10% is about 0.012 USDC per side. Fee optimization cannot rescue a strategy with false regime exits, but avoiding accidental taker entries and one-minute protection gaps can. If acceptable operationally, paying Binance fees with a small BNB balance may reduce fees by 25%, though it introduces BNB inventory and reconciliation requirements.[^20][^5]

Do not assume every USDC pair has the same promotional rate. Binance’s published table distinguishes standard and USDC rates, and the account API returns the actual maker/taker schedule. Log the commission from each fill as the source of truth.[^17][^20]

### Required safeguards

The state model should include `PendingEntry`, `PartiallyFilled`, `Open`, `PendingExit`, `Flattening`, `Closed`, and `Error/Reconcile`. Each transition must be idempotent and keyed by exchange order/client IDs.

Add the following invariants before the validation run:

- Never sell more than strategy-attributable free inventory.
- Never remove local exposure state because an API call was merely sent.
- Never place an exit using requested rather than net-filled quantity.
- Never trade when market data, account stream, clock synchronization, model, or symbol metadata is stale.
- Cancel stale entries after a fraction or multiple of estimated half-life; an old limit must not survive a moving equilibrium indefinitely.
- Add per-trade, per-symbol, correlated-portfolio, daily-loss, and API-error circuit breakers.
- Reconcile exchange open orders and balances at startup before enabling new entries.
- Write state atomically and design recovery for a crash between exchange acceptance and local persistence.

## Hyperliquid migration

### Strategic recommendation

Do not migrate the **exact** engine. Build a venue-independent strategy core and a new Hyperliquid adapter, then recalibrate on Hyperliquid perpetual prices, mark/oracle behavior, funding, and fills. Perpetuals introduce basis and hourly funding, so a signal estimated from Binance spot cannot be assumed to have the same OU parameters or executable equilibrium.[^21][^22]

Hyperliquid’s current entry-tier perpetual fees are reported as 0.015% maker and 0.045% taker; maker rebates exist only after supplying more than 0.5% of the venue’s 14-day weighted maker volume. A micro-account should therefore model a **positive maker fee**, not a 0% fee or rebate. The resulting nominal round trips are approximately 0.03% maker/maker, 0.06% maker/taker, and 0.09% taker/taker before funding and slippage.[^23][^24]

For a 12 USDC notional trade, that is about 0.0036 USDC maker/maker or 0.0072 USDC maker/taker, versus roughly 0.024 USDC at a 0.10%/side Binance assumption. The absolute saving is only cents, so migration is justified by bidirectional exposure and market structure—not by fee savings alone.

Hyperliquid supports post-only ALO orders and real-time `orderUpdates`, `userFills`, and funding subscriptions. Its API also enforces a 10 USDC minimum order value, which a 12 USDC notional clears but leaves little room for downward rounding.[^25][^26][^27][^28]

### Legal constraint

A no-KYC interface is not a lawful mechanism to “bypass” Portuguese or EU derivatives restrictions. Hyperliquid’s terms do not list Portugal among the expressly restricted locations, but they place responsibility on the user to determine compliance with local leveraged-trading law. Crypto perpetuals are economically derivatives and may fall under MiFID II rather than MiCA; Portugal’s post-transition framework also restricts unauthorized providers serving EU clients, with reverse solicitation interpreted narrowly. Obtain Portuguese legal advice and verify current CMVM/ESMA status before live use; do not use technical circumvention as the compliance thesis.[^29][^30][^31]

### Sizing and leverage

Half-life determines **time horizon**, not safe leverage. Define a maximum holding horizon such as

\[
H=k\,\frac{\ln 2}{\theta},
\]

then estimate the conditional maximum adverse excursion over that horizon from walk-forward historical episodes. Let `D_q(H)` be a conservative tail quantile of adverse excursion and let `g` cover gaps, mark/spot basis, slippage, and operational delay. A risk-based notional is

\[
N \leq \frac{rE}{D_q(H)+g},
\]

where `E` is equity and `r` is the allowed equity loss per trade. Correlated BTC, ETH, and SOL positions must share one cluster-level risk budget rather than each receiving an independent full allocation.

Leverage should then be chosen so liquidation is far beyond the validated stop/stress distance. Hyperliquid liquidates when equity falls below maintenance margin, and isolated margin confines that event to the isolated position; maintenance margin depends on the asset’s maximum leverage. For isolated margin fraction `m = margin/notional` and maintenance fraction `l`, the approximate adverse distance is[^32][^33]

\[
d_{liq,long}=\frac{m-l}{1-l},\qquad d_{liq,short}=\frac{m-l}{1+l}.
\]

Require `d_liq` to exceed the stress adverse excursion, gap/basis allowance, and an operational buffer by a wide margin. Query current asset-specific maintenance parameters rather than hard-coding them. No leverage formula can eliminate liquidation risk under discontinuous prices, oracle movement, failed exits, or protocol failure; only conservative notional, excess collateral, isolated margin, and a stop well before liquidation can reduce it.

For this account, begin testnet and then live validation at 1x–2x isolated margin while keeping **notional**, not margin, near the present 12 USDC scale. Moving from a 12 USDC spot position to 12 USDC perp notional at 2x reduces posted margin but does not double market risk; moving to 24 USDC notional does. Do not use 5x merely because liquidation appears distant, and never use cross margin for the first live adapter validation.

## Break-even economics

If `W` is average net win and `L` is average net loss magnitude, the break-even win probability is

\[
p_{BE}=\frac{L}{W+L}.
\]

At a genuinely **net** payoff ratio `W/L = 1.35`, break-even is about 42.6%. A 25% observed win rate is below that point, but two wins and six losses are far too few to estimate the true win probability, and those trades came from a superseded implementation.

When quoted reward/risk is gross, use

\[
p_{BE}=\frac{L_g+c_L}{(W_g-c_W)+(L_g+c_L)},
\]

where `c_W` and `c_L` include realized commissions, spread, slippage, funding, and failed/canceled execution effects. Maker rebates should enter only when actually earned; a micro-account does not meet Hyperliquid’s published maker-share rebate threshold.[^23]

The more useful objective is net expectancy per unit time and per unit risk, not win rate alone. A high win-rate mean-reversion strategy can still have negative expectancy if rare structural-break losses dominate, while a lower win rate can be profitable with sufficiently controlled losses.

## Biases and edge cases

### Statistical pitfalls

- **Single-price pseudo-stationarity:** A rolling regression can always produce a local mean even when the economic process has no stable equilibrium.
- **Near-unit-root bias:** Accepting every `b < 1` turns estimation noise into a tradable reversion speed.
- **Same-bar calibration leakage:** The trigger observation helps set its own threshold.[^2]
- **Data-snooping:** Repeatedly changing z-scores, drift limits, windows, and regime rules after a small live sample overfits noise.
- **HMM label leakage and instability:** State numbers are arbitrary unless mapped from fitted parameters; refits can permute or redefine states.[^4]
- **Backtest fill bias:** A candle touching a passive limit does not establish queue priority or executable volume.
- **Regime pretest bias:** Conditioning strategy results on a stationarity test can make naive performance statistics too optimistic.
- **Correlation concentration:** Three crypto longs are not three independent bets during market stress.
- **Parameter uncertainty:** `mu`, `theta`, `sigma`, thresholds, and half-life are estimates, not constants; sizing should reflect their uncertainty.

### Operational failure modes

- The REST prefill may include a still-open final candle and then append the same candle again from the WebSocket unless timestamps are deduplicated.[^2]
- Parsing failures default the live close to zero, which can poison regression or trigger nonsensical behavior.[^2]
- A normal limit order can execute as taker even though logs call it maker.[^1]
- Old entry limits can remain live while `mu` and thresholds move because there is no time-to-live or reprice policy.[^1][^2]
- Network work occurs while the shared state mutex is held, and per-candle spawned tasks can queue with stale model snapshots.[^2]
- The hot path unnecessarily enters `spawn_blocking` and then calls `Handle::block_on`; this complicates cancellation and failure handling without meaningful CPU benefit.[^2]
- `unwrap_or(0)` on missing order IDs can persist an invalid identity.[^1][^2]
- Alert text hard-codes BTC/USDC even when another symbol trades.[^2]
- Balance-to-base-symbol conversion by string replacement is brittle.[^1]
- Model files lack schema version, feature/timeframe metadata, scaler parameters, integrity validation, and atomic hot-swap persistence.[^4][^2]
- Exchange and blockchain outcomes are asynchronous and queue-dependent, so the end-to-end bot is not deterministic even if its signal code is.

## Validation plan

### Before resuming

1. Manually reconcile the two current positions against exchange balances, entry fills, commissions, open take-profit quantities, and order statuses.
2. Disable new entries until every open position has verified protection for its **net** quantity.
3. Implement the P0 state, partial-fill, decimal/filter, error-handling, and startup-reconciliation fixes.
4. Switch entries to post-only and add a defined taker exception policy.
5. Add hard account and portfolio kill switches.

### 48–72 hour run

Run one frozen build and label the exercise “operational validation.” Record every market-data timestamp, parameter snapshot, regime posterior, signal, order request/response, execution report, fill role, commission asset, cancellation reason, and reconciliation discrepancy. Success means no orphan orders, no uncovered partial fills, no state divergence, no invalid quantities, and correct restart recovery—not positive P&L.

### Strategy validation

Use walk-forward replay with parameters fitted only on prior data. Model passive fills using trades and order-book data, include queue uncertainty, and run sensitivity bands where ambiguous touches are counted as both filled and unfilled. Hyperliquid’s public historical archive warns that data may be missing, so data completeness checks are mandatory before relying on it.[^34]

Report results by symbol and regime with trade count, net expectancy, confidence interval, profit factor, maximum drawdown, maximum adverse excursion, holding time, maker ratio, slippage, fee/funding share, and parameter stability. Do not scale until performance remains positive across neighboring parameter values, multiple market regimes, and pessimistic execution assumptions.

## Prioritized roadmap

| Phase | Scope | Exit criterion |
|---|---|---|
| P0: Capital safety | Event-driven fills, partial fills, net inventory, exchange filters, fixed-point arithmetic, idempotent state, startup reconcile, hard stops, central risk reservations | No exchange/local divergence under disconnect, restart, partial-fill, cancel/fill-race, and rejected-order tests |
| P1: Statistical validity | Lagged calibration, stable OU estimation, unit-root/parameter gates, half-life limits, exact cost model, frozen threshold grid | Walk-forward results survive confidence intervals and pessimistic fill assumptions |
| P2: HMM | Fix Baum–Welch scaling, train/live feature parity, semantic state mapping, online posterior, hysteresis | HMM improves out-of-sample net expectancy or drawdown versus the simpler drift-only baseline |
| P3: Hyperliquid shadow | New adapter, testnet, ALO/IOC behavior, nonce/signing recovery, user streams, mark/oracle/funding model | Reconciliation and risk tests pass; legal access is confirmed |
| P4: Limited live perps | 1x–2x isolated, unchanged small notional, no cross margin, portfolio correlation cap | Stable operational record and statistically defensible net performance before any notional increase |

The best near-term architecture is a single portfolio-risk coordinator plus venue adapters, an event-sourced order ledger, and a pure strategy module that consumes completed bars and emits intents. That separation makes Binance Spot and Hyperliquid behavior testable without embedding venue-specific balances, fees, or order semantics inside the OU model.

---

## References

1. `src/order.rs` (Local Repository Reference)

2. `src/main.rs` (Local Repository Reference)

3. `src/ou_process.rs` (Local Repository Reference)

4. `src/hmm.rs` (Local Repository Reference)

5. [How to Calculate Binance Spot Trading Fees?](https://www.binance.com/en/support/faq/detail/e85d6e703b874674840122196b89780a) - This Page is for general information and educational purposes only. It does not constitute legal ter...

6. [Filters | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/filters) - Filters define trading rules on a symbol or an exchange. Filters come in three forms: symbol filters...

7. [Trading endpoints | Binance Open Platform](https://developers.binance.com/legacy-docs/binance-spot-api-docs/rest-api/trading-endpoints) - New order (TRADE)

8. [User Data Stream | Binance Open Platform](https://developers.binance.com/legacy-docs/binance-spot-api-docs/user-data-stream) - General information

9. [User Data Streams for Binance](https://developers.binance.com/en/docs/products/spot/user-data-stream) - outboundAccountPosition is sent any time an account balance has changed and contains the assets that...

10. [What Are Mean Reversion Trading Strategies?](https://www.cube.exchange/what-is/mean-reversion-trading-strategies) - Learn what mean reversion trading strategies are, how pairs and residual trades work, why stationari...

11. [[PDF] Evaluation of Dynamic Cointegration-Based Pairs Trading Strategy ...](https://arxiv.org/pdf/2109.10662/1000.pdf)

12. [1 Introduction](https://arxiv.org/html/1811.09312v4)

13. [On Finite Time Span Estimators of Parameters for Ornstein-Uhlenbeck Processes](https://ar5iv.labs.arxiv.org/html/2503.23677) - We study the bias and the mean-squared error of the maximum likelihood estimators (MLE) of parameter...

14. [OU Model Optimal Trading Thresholds Bertram — arbitragelab 1.0.0](https://hudson-and-thames-arbitragelab.readthedocs-hosted.com/en/latest/time_series_approach/ou_optimal_threshold_bertram.html)

15. [Optimal statistical arbitrage trading of Berkshire Hathaway stock and ...](https://pmc.ncbi.nlm.nih.gov/articles/PMC7810329/) - In this paper, we make use of the replicating asset for statistical arbitrage trading, where the rep...

16. [A closed-form solution for optimal mean-reverting trading strategies](https://arxiv.org/html/2003.10502v1)

17. [Commission Rates | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/commission_faq) - How do I know what the commission rates are? You can find them using the following requests: REST AP...

18. [Account - Spot REST API](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/rest-api/account) - Access market data, manage accounts, and trade on Binance Spot. Account Query Commission Rates. Get ...

19. [Schemas | Binance Developer Docs](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/ws-api/~schemas) - List of schemas used by the API.

20. [Spot Trading Fee Rate](https://www.binance.com/en/fee/trading) - Discover competitive Binance fees for trading, Taker BNB 25% off 0.01200% Taker BNB 25% off Standard...

21. [Contract specifications - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/contract-specifications)

22. [Funding - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding) - The funding rate formula applies to 8 hour funding rate. However, funding is paid every hour at one ...

23. [Fees - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees)

24. [Hyperliquid Fees Explained: Maker, Taker, HYPE Discount](https://tradingprimer.com/dex/hyperliquid-fees) - Hyperliquid base fees: 0.045% taker, 0.015% maker, so a $10,000 round trip is $9 or $3. What staking...

25. [Subscriptions | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)

26. [Order types | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/order-types) - A Post Only (ALO) limit order that automatically re-prices to track the best bid or ask until it is ...

27. [Exchange endpoint - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint) - ALO (add liquidity only, i.e. "post only") will be canceled instead of immediately matching. IOC (im...

28. [Error responses | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/error-responses)

29. [Terms of Use - Hyperliquid](https://app.hyperliquid.xyz/terms) - By accessing or using the Interface, you represent and warrant that you are not a Restricted Person ...

30. [End of the MiCA's transitional regime: what changes from July 1st?](https://cms.law/en/prt/news-information/end-of-the-mica-s-transitional-regime-what-changes-from-july-1st)

31. [[PDF] 2026-09-30 HPC MiCA Review Consultation Response](https://hyperliquidpolicy.org/micaresponse/2026-09-30_hpc_mica_review_consultation_response.pdf)

32. [Liquidations - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/liquidations)

33. [Margining | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/margining)

34. [Historical data | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data) - Historical data is uploaded to the bucket hyperliquid-archive approximately once a month. non-trade ...

