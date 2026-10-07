# OU Exit Management and Maker Execution Review

## Executive verdict

The near-miss is not evidence that the take-profit should blindly follow every new rolling mean. It shows that the current system conflates three different objects: the **frozen model thesis**, a **noisy rolling estimate of fair value**, and the **actual executable liquidation price**. A time-varying rolling estimate does not preserve Bertram’s static-parameter first-passage problem; once the equilibrium, boundary, queue state and fill probability evolve, the mathematically exact problem becomes a stochastic-control problem without the original closed-form solution.[^1]

The practical solution is a **one-way adaptive exit ratchet**: retain the entry-time model for attribution and maximum holding time, estimate a causal live exit reference separately, permit the long position’s target to move downward but never upward, and reprice only after a material persistent change. Near the target, optimize expected realized value rather than nominal limit price by moving into a short-lived “harvest” state with a modest front-running offset and a profit-protection trigger.

The empirical trade also contains a unit-label issue. The target price move was $19.43 per ETH, but at 0.0098 ETH the gross target P&L was approximately $0.1904, not $19.43. The observed high represented about $0.1670 gross unrealized P&L, and the later $2,558.74 price represented approximately $0.1205 gross unrealized P&L before fees.

## What the trade demonstrates

The original distance was:

\[
D_0=2565.87-2546.44=19.43.
\]

The maximum realized progress was:

\[
q_{max}=\frac{2563.48-2546.44}{19.43}=0.877.
\]

The later progress was:

\[
q_t=\frac{2558.74-2546.44}{19.43}=0.633.
\]

Thus, the trade moved through 87.7% of the initial target and subsequently retraced to 63.3% progress. This is a classic interaction between model uncertainty and non-execution risk: a passive limit can earn a better conditional price but provides no fill guarantee, and execution probability depends on price distance, queue position and future order flow.[^2][^3]

The statement that the order missed by “about two ticks” should be checked against Binance `PRICE_FILTER.tickSize`. The numerical gap was $2.39; it equals two ticks only if one tick is approximately $1.195. Journals should record both `gap_price` and `gap_ticks = gap_price / tickSize` rather than describing ticks manually.

## Static versus dynamic mean

### The key modeling distinction

Under a constant-parameter OU model,

\[
dX_t=\theta(\mu-X_t)dt+\sigma dW_t,
\]

\(\mu\) is fixed during the modeled episode. Bertram-style boundaries are derived for static process parameters and transaction costs. Re-estimating \(\mu_t\) each minute creates a different, time-inhomogeneous model:[^1]

\[
dX_t=\theta_t(\mu_t-X_t)dt+\sigma_t dW_t.
\]

A trailing 480-minute estimate is not automatically the latent equilibrium “toward which price is currently driven.” It is an estimator that mechanically incorporates the recent sell-off. Its fall can represent a genuine regime shift, ordinary estimation noise, or simply the rolling-window arithmetic caused by replacing old observations with lower new ones.

Consequently, neither extreme is optimal:

- **Never adapt:** risks leaving the order above a demonstrably lower fair-value estimate.
- **Track every update:** chases estimator noise, repeatedly loses queue priority and can lower the target after every adverse move.

The correct production architecture separates:

- `thesis_at_entry`: frozen parameters, initial boundary, initial distance, half-life and expiry.
- `live_fair_value`: causal estimate used for risk and adaptive liquidation.
- `working_order_price`: microstructure-aware executable quote.
- `profit_floor`: path-dependent protection after substantial favorable movement.

### Recommended adaptive boundary

For a long position, define a causal candidate exit value:

\[
b_t^{cand}=\hat\mu_{t|t-1}-\epsilon_t.
\]

Then make the accepted adaptive target one-way:

\[
b_t^{eff}=\max\left(P_{min},\min(b_{t-1}^{eff},b_t^{cand})\right),
\]

where

\[
P_{min}=P_{entry}\exp(c_{remaining}+m_{min})
\]

is the minimum economically acceptable passive exit after remaining fees, expected slippage and a configured alpha margin. The target may ratchet downward when fair value deteriorates, but it may not increase and extend the holding period when the rolling estimate rises.

Use the live estimate only after all of these conditions hold:

- The new value is based exclusively on data through the latest completed bar.
- The downward change persists for at least three completed one-minute bars.
- The model remains eligible under shock, stationarity and half-life gates.
- The new target remains above the economic floor.
- The required change exceeds an event threshold.

### Repricing threshold

A fixed 15 bps threshold is defensible as a first canary parameter, but it was about 20% of this trade’s full 76.3 bps target distance. It is therefore relatively coarse and could react too late to exactly this type of near-miss.

A better initial rule is:

\[
\delta_{reprice}=
\max\left(
3\,tickSize,
5\text{ bps}\times P_t,
0.10D_0
\right),
\]

subject to a three-minute cooldown and no more than three discretionary target replacements per position. For this trade, \(0.10D_0\) was approximately $1.94, or 7.6 bps, so the target would have changed after a persistent decline of roughly 7.6 bps rather than waiting for 15 bps.

This is a starting policy, not a universal optimum. The threshold should ultimately minimize out-of-sample **implementation shortfall plus missed-exit loss**, with replacements evaluated against queue-ahead depth, fill probability and adverse-selection markouts. Queue position has measurable value because earlier orders fill faster and tend to face lower adverse-selection costs.[^4][^5]

On Binance, changing price requires cancel/replacement and loses time priority; the keep-priority amendment feature preserves priority only when reducing quantity, not when changing price. Therefore, use keep-priority amendments for quantity reductions after partial fills, but explicitly price the queue reset into every target-price replacement.[^6]

## Front-running the mean

### No universal epsilon

Neither 3–5 bps nor \(0.1\sigma\) is universally optimal. The right offset depends on tick size, spread, top-of-book depth, queue position, short-horizon volatility, time remaining, maker/taker economics and the conditional probability that price reverses before reaching the theoretical boundary. Limit-order research frames this as a trade-off among execution probability, price improvement and adverse selection rather than a fixed percentage rule.[^3][^2]

A static percentage also behaves inconsistently across assets and regimes. A full residual standard deviation fraction such as \(0.1\sigma_{eq}\) may be excessively large when equilibrium volatility is high, while a 3 bps constant may be smaller than ordinary one-minute noise or larger than the remaining net edge.

### Practical offset

Use an empirically constrained offset:

\[
\epsilon_t=
\operatorname{clip}
\left(
\max(2\,tickSize,\epsilon_{fill,t}),
2\text{ bps}\times P_t,
\min(10\text{ bps}\times P_t,0.15D_{rem})
\right),
\]

where \(\epsilon_{fill,t}\) is estimated from the historical distribution of `theoretical_boundary - session_high` for comparable trades that approached but did not fill. The upper bound prevents the execution overlay from surrendering a disproportionate share of the residual alpha.

Until enough live data exists to estimate \(\epsilon_{fill,t}\), begin with **5 bps**, constrained to at least two ticks and at most 10%–15% of the remaining gross target distance. Test 3, 5, 7.5 and 10 bps as a frozen walk-forward grid; do not optimize a separate offset for each coin on a tiny sample.

### Order-book placement

For a maker sell, calculate:

\[
P_{theory}=b_t^{eff}-\epsilon_t,
\]

then choose a valid post-only price respecting both the economic floor and current book:

\[
P_{work}=
\max(P_{min},P_{theory},bestBid+tickSize),
\]

rounded upward to the exchange tick. If joining or improving the current best ask at a lower price still clears \(P_{min}\), the harvest state may instead quote at that best ask or one tick inside a multi-tick spread. Record queue-ahead quantity at submission and do not infer a fill merely from a candle high.

A passive order farther from the current quote has lower execution probability; queue location and prevailing market state also materially affect whether it fills. Accordingly, the offset should be calibrated on actual order-level data, not OHLC candle touches.[^7][^3]

## Profit protection

### Break-even is not automatically beneficial

A break-even stop changes the stopping problem and cannot be assumed to improve expected value. It usually truncates some eventual winners, adds taker fees and spread costs, and can be triggered by ordinary noise; this can reduce arithmetic expectancy. Conversely, it can reduce left-tail losses, drawdown and downside deviation, potentially improving Sharpe or Sortino even while lowering mean return. OU research with stop-loss or trailing-stop constraints shows that adding a stop changes the optimal liquidation boundary rather than leaving the original take-profit unchanged.[^8][^9][^10]

There is no publicly verifiable single rule used by “top stat-arb funds.” A robust institutional-style implementation treats profit protection as a separately tested path-dependent policy, accounts for all execution costs, and sizes or exits based on model confidence and portfolio risk rather than relying on a folklore percentage.

### Why entry break-even is weak

After reaching 75%–88% of the target, a stop at entry plus fees allows almost the entire favorable move to disappear. It protects against a realized loss but does little to control profit giveback. A more useful ratchet operates in normalized progress space.

Define:

\[
q_t=\frac{P_t-P_{entry}}{b_0-P_{entry}},
\qquad
q_t^{max}=\max_{u\le t}q_u.
\]

Recommended canary policy:

- Arm `PROFIT_PROTECT` when \(q_t^{max}\ge0.75\).
- Enter aggressive `HARVEST` when \(q_t^{max}\ge0.85\), or when the residual is within \(\epsilon_t\) of the adaptive boundary.
- Trigger a price-protected IOC exit when
  \[
  q_t\le\max(q_{BE},q_t^{max}-0.20),
  \]
  where \(q_{BE}\) is the progress required to cover all remaining fees and slippage.
- Tighten the giveback allowance from 0.20 to 0.15 only after sufficient evidence shows that the tighter rule survives costs.

For the observed trade, \(q_{max}=0.877\), so a 0.20 giveback trigger would have fired at approximately \(q=0.677\), corresponding to roughly $2,559.59 before tick rounding. That would have protected a substantial portion of the move rather than merely break-even.

Do not trigger from intrabar high alone unless tick/trade data are available. On a one-minute close strategy, update `q_max` and evaluate the ratchet on completed bars; an optional faster risk path can use best bid or mark price but must be tested separately to avoid microstructure whipsaw.

## State machine

### Common states

| State | Entry condition | Working behavior | Exit/transition |
|---|---|---|---|
| `OPEN_PASSIVE` | Entry fill confirmed | Rest maker TP near frozen/adaptive boundary | `APPROACH` at 70% progress |
| `APPROACH` | \(q\ge0.70\) | Evaluate persistent live-mean decline; reprice by event threshold | `HARVEST` at 85% or proximity condition |
| `HARVEST` | \(q_{max}\ge0.85\) | Quote at adaptive boundary minus epsilon; short TTL | Fill, ratchet trigger, or regime exit |
| `PROFIT_PROTECT` | \(q_{max}\ge0.75\) | Maintain path-dependent floor | IOC if giveback threshold breaks |
| `TIME_EXIT` | Frozen \(2t_{1/2}\) expires | Brief maker grace period | Price-protected IOC |
| `REGIME_EXIT` | Severe shock/model invalidation | No maker waiting | Immediate price-protected IOC |

`APPROACH`, `HARVEST` and `PROFIT_PROTECT` may be orthogonal flags rather than mutually exclusive states. Persist the following fields so restart behavior is deterministic:

```text
entry_price
entry_time
frozen_exit_boundary
frozen_half_life
live_effective_target
working_order_price
last_reprice_time
reprice_count
max_progress
max_favorable_price
profit_protect_armed
profit_floor_price
remaining_quantity
protected_quantity
```

### Repricing transaction

Every price change must be an idempotent order-state transaction:

1. Read current order status and cumulative fill.
2. Protect any new partial fill.
3. Submit cancel/replace with deterministic client IDs where supported.
4. Reconcile ambiguous responses by client ID before retrying.
5. Persist the new order identity and target.
6. Never leave the position without either a live exit order or an explicit IOC escalation path.

A price replacement should occur only if the expected benefit exceeds queue-reset cost:

\[
\Delta EV_{replace}
=
\Delta P(fill)\times expected\ retained\ profit
-
price\ concession
-
queue\ reset\ cost
-
API/failure\ risk
>0.
\]

Estimate this from live telemetry rather than asserting that every lower \(\mu_t\) warrants cancellation.

## Venue-specific rules

### Binance Spot

- Keep the initial TP as `LIMIT_MAKER`.
- Use user-data execution reports for partial fills and terminal statuses.
- Reprice no more frequently than every three minutes and only after the persistent event threshold.
- Use Binance keep-priority amendment only to reduce remaining quantity; it cannot change price while preserving priority.[^6]
- In `HARVEST`, post at the adaptive target minus epsilon, or join/improve best ask if doing so remains above the economic floor.
- Give the harvest quote a 30–60 second TTL after a profit-ratchet trigger; then use a price-protected IOC/marketable limit rather than an unbounded market order.
- Recompute actual net P&L using commissions in their charged asset before declaring the break-even floor.

### Hyperliquid Perpetuals

- Mark every liquidation order `reduceOnly` so an exit cannot reverse the position.
- Use ALO for passive take-profit and IOC for urgent ratchet, time-stop or regime exits; Hyperliquid documents ALO as post-only and IOC as cancelling any unfilled remainder.[^11]
- Include expected funding through the remaining holding horizon in \(P_{min}\).
- Base risk triggers on a consistent executable reference—typically best bid for a long liquidation—and log mark, oracle and mid separately.
- Treat every price modification as a probable queue reset and assign a new deterministic client-order lifecycle.
- Maintain an independent exchange-side emergency stop/trigger where available; local profit logic is not a substitute for crash protection.

## Calibration experiment

Do not infer a universal rule from this one trade. Log and evaluate each exit policy against identical entry events:

| Policy | Description |
|---|---|
| A | Frozen \(\mu_0\), exact target |
| B | Frozen \(\mu_0\), 5 bps front-run |
| C | One-way live target, 7.5 bps reprice threshold |
| D | Policy C plus 5 bps front-run |
| E | Policy D plus 75%/20%-giveback ratchet |

For each policy, measure net P&L, fill ratio, missed-exit rate, profit giveback from MFE, maker/taker share, queue time, cancel count, adverse-selection markouts, holding time, time-stop frequency, Sharpe, Sortino and maximum drawdown. Use event-level order-book replay where available; candle highs cannot establish queue fills.

Require the adaptive policy to improve out-of-sample net expectancy or risk-adjusted performance after fees, not merely produce a higher TP fill rate. A lower nominal exit can increase realized value if its execution probability improves enough, but aggressive fills can also introduce adverse selection; execution optimization must include both effects.[^2]

## Recommended defaults

For the next minimum-notional canary, use:

- Frozen entry model and half-life for attribution and deadline.
- Causal live fair value with a three-bar persistence requirement.
- One-way downward target ratchet for longs; never raise the target after entry.
- Reprice threshold of `max(3 ticks, 5 bps, 10% of initial target distance)`.
- Three-minute reprice cooldown and maximum three target replacements.
- Initial front-run epsilon of 5 bps, floored at two ticks and capped at 15% of remaining distance.
- Profit protection armed at 75% progress.
- Harvest mode at 85% progress.
- IOC protection after a 20-percentage-point giveback from maximum progress.
- Immediate IOC on severe shock or model invalidation, without waiting for the profit ratchet.

These are controlled initial parameters, not claims of mathematical universality. The true optimum must be selected from order-level walk-forward data under the exact venue fee, queue and latency model.

---

## References

1. [1 Introduction](https://arxiv.org/html/1811.09312v4)

2. [Optimal order placement in limit order markets - arXiv](https://arxiv.org/html/1210.1625v4)

3. [Empirical Evidence](https://academic.oup.com/restud/article/71/4/1027/1564787)

4. [The determinants of limit order cancellations - Dahlström](https://onlinelibrary.wiley.com/doi/full/10.1111/fire.12363)

5. [A Model for Queue Position Valuation](https://business.columbia.edu/sites/default/files-efs/pubfiles/25461/queue-value-2016.pdf)

6. [Order Amend Keep Priority | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/order_amend_keep_priority) - By default, amended orders get a random new client order ID, but you can pass the current client ord...

7. [Fill Probabilities in a Limit Order Book with State- ...](https://arxiv.org/pdf/2403.02572.pdf) - by F Lokin · 2024 · Cited by 6 — We model the limit order book within a general state-dependent stoc...

8. [Optimal Trading with a Trailing Stop](https://www.alphaxiv.org/abs/1701.3960v2) - Trailing stop is a popular stop-loss trading strategy. In this paper, we study the problem of timing...

9. [Optimal Multiple Stopping Approach to Mean Reversion Trading](https://academiccommons.columbia.edu/doi/10.7916/D85X2HHK/download)

10. [Optimal Mean Reversion Trading With Transaction Costs ...](https://ideas.repec.org/a/wsi/ijtafx/v18y2015i03ns021902491550020x.html) - Motivated by the industry practice of pairs trading, we study the optimal timing strategies for trad...

11. [Exchange endpoint - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/exchange-endpoint) - For limit orders, TIF (time-in-force) sets the behavior of the order upon first hitting the book. "e...

