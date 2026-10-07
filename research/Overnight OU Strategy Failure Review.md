# Overnight OU Strategy Failure Review

## Executive verdict

The overnight loss was not primarily an execution failure or a bad half-life estimate. It was a **regime-selection and portfolio-concentration failure**: three nominally separate trades were one synchronized long-crypto-beta position entered while the market’s short-horizon drift and volatility regime were changing.

The immediate remedy is not to loosen the time stop, widen the OU threshold, or rely on a 4-hour moving average. New entries should remain disabled until the system has: (1) a fast market-wide downside-shock veto, (2) entry-on-reversal rather than entry-on-first-touch, (3) a slow residual-stationarity gate, and (4) a portfolio rule that treats simultaneous BTC, ETH, and SOL signals as one correlated risk cluster.

Fitting a detrended residual is still preferable to fitting raw price, but **residualization alone would not have prevented this event**. A lagging baseline can make a cascade appear as an increasingly attractive negative residual. The residual model must therefore sit behind an independent change-point/momentum gate.

Pairs or factor-neutral spreads are structurally cleaner than three separate long-only fades, but they are not the only possible profitable architecture. A single-asset long-only strategy may have positive expectancy if it trades only in demonstrated range or supportive-trend regimes; it simply cannot be called statistical arbitrage because it retains uncompensated directional beta.

## Ledger diagnosis

The three completed positions lost 0.661 USDC on approximately 34.57 USDC of combined entry notional, or about 1.91% of deployed notional. More importantly, the entries occurred within roughly two minutes, so the portfolio did not have three independent observations or three independent risks—it placed one market-direction bet through three correlated instruments.

| Asset | Entry notional | Return on position | Hold | Failure mode |
|---|---:|---:|---:|---|
| BTC/USDC | About 11.95 USDC | -1.30% | 2h 24m | Market-beta continuation |
| ETH/USDC | About 11.82 USDC | -2.87% | 3h 40m | Market-beta continuation plus higher beta |
| SOL/USDC | About 10.81 USDC | -1.55% | 2h 28m | Market-beta continuation |

The post-only entries and sentinel behaved correctly, and the frozen time stops limited the damage. That is valuable operational evidence, but it does not validate the signal. The subsequent ETH entry should be considered part of the same regime episode unless a documented market-wide reset condition occurred between the first exits and the new signal.

## Why the model failed

The model confused a large innovation with a stationary displacement. In a fixed-parameter OU model, a more negative deviation normally increases expected reversion. During a structural break, however, the equilibrium mean, drift, volatility, or all three have changed; the old mean is no longer the correct anchor.

The Phillips–Yu jackknife addresses finite-sample bias in estimated mean-reversion parameters, particularly near a unit root, but bias correction cannot make a misspecified or structurally broken process stationary. The literature explicitly distinguishes correction of estimation bias from correctness of the underlying process specification.[^1]

The 480-minute slope was performing the wrong job. It is suitable as a slow context variable, but an eight-hour regression necessarily dilutes a move concentrated in the most recent 15–60 minutes. Online CUSUM and Page–Hinkley methods are designed to accumulate evidence of a new shift sequentially, and can react before a long trailing regression materially changes.[^2][^3][^4]

## Recommended gate stack

No single test should become a binary oracle. Use a layered gate in which **any fast danger signal can veto entry**, while slower evidence determines whether the OU engine is eligible at all.

### Fast downside-shock veto

For each asset, calculate log returns and standardized cumulative moves:

\[
Z_h(t)=\frac{x_t-x_{t-h}}{\hat\sigma_{1m,t-1}\sqrt{h}},
\qquad h\in\{15,60\}.
\]

The volatility estimate must use only data available through \(t-1\), preferably a robust EWMA or median-absolute-deviation estimator so the current crash bar cannot inflate its own denominator. Initial research thresholds—not final production constants—should veto long entries when either \(Z_{15}<-2.5\) or \(Z_{60}<-2.5\). Calibrate the threshold against false-alarm rate and net opportunity cost in walk-forward data.

Add a one-sided downside CUSUM on standardized one-minute returns:

\[
C_t^- = \min\left(0,C_{t-1}^-+z_t+k\right),
\]

and trigger a cooldown when \(C_t^-<-h\). A conventional research starting point is \(k=0.5\) and \(h=5\), followed by calibration to the desired average run length; CUSUM is attractive here because it updates causally in constant time and is explicitly intended to detect accumulated directional shifts.[^5][^2]

### Market-wide shock veto

Construct a common-market statistic from the three assets:

\[
M_h(t)=\operatorname{median}\left(Z_{h,\mathrm{BTC}},Z_{h,\mathrm{ETH}},Z_{h,\mathrm{SOL}}\right).
\]

Block every new long when \(M_{15}<-2.0\), \(M_{60}<-2.0\), or at least two assets have active downside-CUSUM alarms. This would address the exact observed failure mode: all three instruments breaking together is evidence for a common factor shock, not three independent idiosyncratic discounts.

After a market-wide alarm, do not reset eligibility merely because one bar turns green. Require a cooldown of at least 30 completed minutes and a recovery condition such as: no active CUSUM alarm, \(M_{15}>-0.5\), and realized short-horizon volatility no longer expanding. These starting thresholds require walk-forward calibration.

### Dual-speed drift filter

A dual-speed filter is appropriate, but percentage slope alone should be replaced by volatility-normalized evidence:

| Layer | Horizon | Purpose | Suggested decision |
|---|---:|---|---|
| Shock | 15 minutes | Detect abrupt cascade | Hard veto when standardized return or CUSUM breaches |
| Fast trend | 60 minutes | Detect sustained continuation | Veto when slope t-stat or standardized return is strongly negative |
| Slow context | 240–480 minutes | Classify supportive, neutral, or bearish background | Permit longs only if neutral/supportive |
| Residual stationarity | 480+ residual observations | Determine whether OU calibration is admissible | Require stable evidence, not a one-bar pass |

For the 60-minute regression, evaluate the HAC-robust t-statistic of slope rather than a fixed percentage. A practical initial veto is \(t_\beta<-2\). For the slow layer, use an exponentially weighted local-linear trend and reject longs when the predicted slow slope is materially negative in volatility units. The exact cutoffs must be selected before evaluation and tested across neighboring values.

### Higher-timeframe context

A higher-timeframe gate is useful as a **slow prior**, not as the primary crash detector. Four-hour indicators react too slowly to protect against a move that begins and triggers entries inside one hour. A completed 15-minute/one-hour market-factor gate should carry more immediate authority.

For a spot-only long strategy, require both the asset and the common crypto factor to be neutral or supportive. A simple initial rule is: do not open a long if the last completed one-hour close is below a falling one-hour EWMA and the common-market 60-minute standardized return is negative. The purpose is not to discover an optimal moving-average crossover; it is to avoid buying an asset-specific OU deviation while the entire market factor is trending down.

### Reversal-confirmed entry

The largest tactical improvement is to stop buying the first downward crossing of the lower boundary. Use a two-stage state machine:

1. **Armed:** residual falls below the lower OU boundary while every regime gate remains valid.
2. **Triggered:** residual subsequently closes back above that boundary, short-horizon return is nonnegative, and no shock gate has fired.
3. **Submitted:** place the post-only bid only after the reversal confirmation, with a short TTL.

This changes the signal from “price is very low” to “price was very low and has begun reverting.” It sacrifices some entry price and some fills, but directly targets falling-knife selection. Mean-reversion implementations commonly distinguish a boundary touch from re-entry or rejection because a trend can continue to ride an outer band.[^6][^7]

## Formal stationarity tests

ADF should be added, but only on the **detrended residual or candidate spread**, not raw crypto price. ADF’s null is a unit root, while KPSS uses stationarity as its null; agreement between ADF rejection and KPSS non-rejection is stronger than either test alone.[^8][^9]

A proposed eligibility classification is:

| ADF | KPSS | Interpretation | Action |
|---|---|---|---|
| Reject unit root | Do not reject stationarity | Consistent with stationarity | Continue to other gates |
| Fail to reject | Reject stationarity | Consistent with nonstationarity | Block entry |
| Both reject or neither rejects | Ambiguous/structural break/low power | Block entry or reduce size |

Use AIC or BIC to choose a bounded number of ADF lags, persist the deterministic specification, and require the classification to remain valid across multiple completed bars and at least one neighboring lookback. ADF has low power in short or highly persistent samples and is sensitive to lag choice and structural breaks; it is therefore too slow and unstable to serve as the cascade detector.[^10][^9][^11]

Repeated rolling tests also create multiple-testing and state-churn problems. Treat stationarity as a slowly updated eligibility feature—for example, update every 15 minutes—and use CUSUM/standardized returns for immediate safety. A break-aware offline test such as Zivot–Andrews or Bai–Perron is useful for diagnosing and labeling historical episodes, while online CUSUM is more suitable for production vetoes.[^12][^8]

## Residual OU redesign

### Would residual fitting have saved the trades?

Not by itself. For

\[
y_t=x_t-\hat g_{t|t-1},
\]

a fast sell-off against a slow baseline produces a large negative \(y_t\). Without the shock and reversal gates, the residual OU can regard the falling knife as an even stronger entry than the raw-price model. Worse, any rolling baseline mechanically induces some apparent reversion, so stationarity must be demonstrated out of sample rather than assumed from the transformation.

Residual fitting still improves scale comparability and separates slow trend from faster fluctuations. Use a causally predicted exponentially weighted local-linear baseline, calculated through \(t-1\), and fit the OU only to the resulting residual history. The baseline decay, OU window, stationarity tests, and order thresholds must all be frozen during each walk-forward validation fold.

### Boundary translation

Let the residual OU have entry boundary \(a<\mu_y\) and exit boundary \(m\). At decision time:

\[
P_{\mathrm{entry},t}=\exp\left(\hat g_{t|t-1}+a\right).
\]

Quantize this price downward to Binance tick size for a passive buy, then verify that the quantized price is still below the best ask and satisfies the post-only constraint. Transaction costs must be represented in log-return units and recomputed after tick/lot quantization.

For exact consistency with a fixed-boundary Bertram first-passage problem, freeze the baseline and OU parameters at entry:

\[
P_{\mathrm{exit}}=\exp\left(\hat g_{\mathrm{entry}}+m\right).
\]

If the baseline is allowed to move after entry, then

\[
P_{\mathrm{exit},t}=\exp\left(\hat g_t+m\right)
\]

becomes a moving price boundary. That may be operationally useful, but it is no longer the same time-homogeneous closed-form optimization and must be evaluated by complete policy simulation. Never let a falling baseline repeatedly lower the target or extend the frozen time stop.

### Separate model and safety state

At entry, snapshot the parameters used to justify the trade: baseline, OU mean, volatility, boundaries, half-life, cost estimate, and model version. Continue computing a separate live safety model for invalidation. A newly estimated lower mean must not retroactively redefine the original thesis and turn a losing trade into a supposedly valid one.

## Portfolio-level redesign

The per-symbol circuit breakers did not capture a systemic event. Add a portfolio event key so signals within a common time bucket and regime are treated as one risk cluster.

Recommended initial controls:

- Allow only one full-size long across BTC, ETH, and SOL when pairwise 60-minute return correlations or common-factor loading are high.
- If a second signal arrives within 15 minutes, either reject it or size the entire cluster to the risk budget previously assigned to one trade.
- Block all new longs after two correlated symbols trigger time-stop or structural-break exits in the same regime episode.
- Require the market-wide reset condition before accepting another signal; a per-symbol 24-hour pause should not be the only mechanism.
- Attribute P&L and drawdown by common episode as well as by symbol, so three synchronized losses are counted as one failed market-regime decision.

The new ETH entry shortly after the cascade should therefore be permitted only if the market-wide alarm reset, the residual crossed back through its entry boundary, and portfolio risk was again eligible. A fresh OU parameter estimate alone is insufficient because the rolling window still contains mixed pre-break and post-break observations.

## Directional versus relative value

### Single-asset viability

Single-asset directional mean reversion is not mathematically impossible. It can make money if conditional reversals exceed losses and costs after a strict regime-selection policy. However, a long-only spot implementation earns or loses partly from market direction, so it should be described as a **conditional directional strategy**, not statistical arbitrage.

The correct research benchmark is not whether every sell-off can be filtered. It is whether the complete walk-forward policy—gates, reversal confirmation, missed fills, fees, slippage, time stops, and correlated exposure—has positive net expectancy with uncertainty bounds. No filter guarantees protection because abrupt repricing can occur after entry.

### Pairs and factor-neutral spreads

A more defensible statistical-arbitrage architecture models a spread such as

\[
y_t=\ln P_{\mathrm{ETH},t}-\alpha_t-\beta_t\ln P_{\mathrm{BTC},t},
\]

where \(\beta_t\) is estimated causally. If the spread is cointegrated and stable, trading long ETH against short \(\beta\) units of BTC removes much of the common crypto factor and targets relative mispricing rather than outright market direction.

Crypto pairs research finds that market-neutral cointegrated strategies can be profitable, but outcomes remain highly sensitive to pair selection, lookback, fees, and regime stability. One recent broad study reported an average out-of-sample Sharpe around 0.69 for its best specification and a median maximum drawdown of 15.29%, which is evidence of feasibility—not safety or universal superiority. Other research shows that 0.1% transaction costs can roughly halve profitability in frequent-trading specifications.[^13][^14][^15]

Cointegration is also not permanent. Research on common cryptocurrency factors has found that a factor can switch between stationary and integrated behavior, which means a previously market-neutral spread can cease to be stationary. A pairs engine therefore still needs rolling hedge-ratio stability, residual ADF/KPSS checks, break detection, exposure limits, and time/price invalidation.[^16]

### Spot constraint

True market neutrality requires a short leg or economically equivalent hedge. Binance Spot without margin cannot implement a conventional long-short pair unless the system already owns and is willing to sell inventory; simply choosing the “cheaper” coin and buying it remains long crypto beta. Hyperliquid perpetuals could provide the short leg, but migration should follow—not substitute for—successful shadow testing with funding, basis, liquidation, and passive-fill assumptions.

## Recommended implementation order

### P0: before another unattended run

- Disable fresh entries following a market-wide shock until an explicit reset condition passes.
- Add 15-minute and 60-minute standardized-return vetoes plus one-sided downside CUSUM.
- Aggregate BTC, ETH, and SOL signals into one correlated portfolio risk cluster.
- Require residual boundary re-entry/reversal confirmation before submitting a bid.
- Prevent same-regime re-entry after correlated time-stop exits.

### P1: model correction

- Replace raw-price OU with a causal detrended log residual.
- Add residual ADF/KPSS eligibility checks updated on completed 15-minute boundaries.
- Preserve the existing 15–120-minute half-life gate, but apply it to the residual process.
- Freeze baseline and OU parameters for each trade’s thesis and target; maintain a distinct live risk model.

### P2: research track

Run four walk-forward variants with identical execution simulation:

1. Existing raw-price OU as the control.
2. Residual OU only.
3. Residual OU plus fast/slow gates.
4. Residual OU plus gates, reversal confirmation, and portfolio clustering.

Report results by market episode, not only by trade. Key metrics should include net expectancy, profit factor, maximum adverse excursion, time-stop rate, percentage of avoided losers, percentage of rejected winners, common-factor beta, cluster drawdown, and performance under neighboring thresholds. This decomposition will reveal whether the filters add genuine conditional edge or merely suppress activity.

In parallel, shadow-test BTC–ETH, ETH–SOL, and a three-asset factor-neutral basket. Use causal hedge ratios, require out-of-sample residual stationarity, and include both legs’ fees, funding, basis, queue effects, and asynchronous-fill risk. Pairs trading should be advanced only if it beats the gated single-asset strategy under the same pessimistic execution assumptions.

## Decision

The current system has passed an important operational milestone but failed the economic regime test. The correct response is to preserve the hardened execution layer, suspend ungated entries, and redesign the signal as **residual mean reversion conditional on no market-wide change point and observable reversal onset**.

Do not conclude from one overnight event that all directional mean reversion is invalid, and do not conclude that pairs trading is automatically profitable. The event does establish that three independent OU containers are not independent risk units and that a slow 480-minute drift limit cannot protect a short-horizon contrarian strategy from a common directional break.

---

## References

1. [[PDF] A Selective Overview of Nonparametric Methods in Financial ...](http://korora.econ.yale.edu/phillips/pubs/art/p1152.pdf)

2. [Concept Drift Detection Calculator | MetricGate](https://metricgate.com/docs/concept-drift-detection/) - Detect concept drift in streaming data using ADWIN, DDM, or Page-Hinkley methods. Get drift points a...

3. [Page-Hinkley Change Detector Calculator - MetricGate](https://metricgate.com/docs/page-hinkley-change-detector/) - Detect change points online with the Page-Hinkley test: accumulate deviation from the running mean w...

4. [Structural Break Detection: Definition & Tests | Inference ...](https://inferensys.com/glossary/quantitative-finance-and-algorithmic-trading/regime-switching-models/structural-break-detection) - Structural break detection identifies points where a financial time series' data-generating process ...

5. [www.mql5.com › en › articlesDetecting Structural Breakpoints in Price Series Using CUSUM ...](https://www.mql5.com/en/articles/23043) - This article builds a sequential CUSUM breakpoint detector for MetaTrader 5, starting from the stati...

6. [Mean Reversion Regime Filter: Skip the Falling Knives](https://algomax.ai/blog/mean-reversion-regime-filter-avoid-falling-knives) - Oversold works in ranges and fails in trends. Here is how to add a regime filter to a mean reversion...

7. [Mean Reversion Explained - Alchemy Markets](https://alchemymarkets.com/education/strategies/mean-reversion/) - Learn how mean reversion works in trading — from Bollinger Bands and RSI to pairs and volatility set...

8. [Testing for Mean Reversion: ADF, Hurst Exponent and Half ...](https://www.quantt.co.uk/resources/mean-reversion-testing) - The Augmented Dickey–Fuller (ADF) test is ・ tests the null hypothesis ・ against the alternative that...

9. [Stationarity & Efficiency Tests: Statistics Concept](https://www.luxalgo.com/library/concept/stationarity-and-efficiency-tests/) - The augmented Dickey-Fuller (ADF) test takes non-stationarity (a unit root) as its null hypothesis, ...

10. [What Is the Dickey-Fuller Test?](https://www.cube.exchange/what-is/dickey-fuller-test) - Learn what the Dickey-Fuller test is, how it detects unit roots, why traders use it for mean reversi...

11. [Mean Reversion vs Momentum: Detecting the Regime Switch](https://www.cryptohopper.com/blog/mean-reversion-vs-momentum-detecting-the-regime-switch-13184) - Your mean-reversion bot isn't broken. It's running the wrong strategy for the current regime. Mean r...

12. [Bai-Perron Structural Break Test Calculator](https://metricgate.com/docs/bai-perron-structural-break/) - The Bai-Perron test (1998, 2003) detects and dates multiple structural breaks in linear regression r...

13. [Optimal market-neutral multivariate pair trading on the ... - arXiv](https://arxiv.org/html/2405.15461v4)

14. [Pair trading strategies in the cryptoassets market](https://www.tandfonline.com/doi/abs/10.1080/14697688.2026.2653663) - The crypto-assets markets are notoriously volatile and risky. In this context, market-neutral type s...

15. [Pair trading strategies in the cryptoassets market: a cointegration framework with optimized thresholds using genetic algorithms](https://www.tandfonline.com/doi/full/10.1080/14697688.2026.2653663) - The crypto-assets markets are notoriously volatile and risky. In this context, market-neutral type s...

16. [[PDF] Common dynamic factors for cryptocurrencies and multiple pair ...](https://research.unipg.it/retrieve/54a507dc-fa88-4221-b8a3-4018c85bd6c3/CommonDynamic.pdf)

