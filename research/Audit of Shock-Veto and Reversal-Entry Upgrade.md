# Audit of Shock-Veto and Reversal-Entry Upgrade

## Executive assessment

The upgrade is directionally correct, but it is **not yet safe for unattended production**. The downside-shock gate is causal and contains no look-ahead bias, yet the implementation evaluates the shock only through bar \(t-1\), so it is one completed minute stale at the decision on bar \(t\). More importantly, four P0 implementation defects remain: the cross-container cluster gate is a non-atomic file-based check, partial entry fills remain temporarily unprotected, accepted orders can become orphaned if local persistence fails, and order/position timing starts at submission instead of the first fill.[^1][^2][^3]

The five-basis-point rebound is useful as a minimal noise filter, but one green candle is insufficient evidence of regime reversal. It should be replaced by an asset-adaptive residual rebound plus persistence rule, while the armed setup should carry a frozen model snapshot. The current code records the original entry threshold but submits and protects the trade using newly recalculated thresholds, mean, and half-life, which changes the trade thesis between arming and execution.[^3][^1]

## Priority findings

| Priority | Finding | Consequence | Required action |
|---|---|---|---|
| P0 | Cluster check is a file-read followed later by order submission | Two containers can simultaneously observe no active position and both submit | Use one atomic portfolio lease/reservation in Redis, PostgreSQL, SQLite with a transaction, or a single execution coordinator |
| P0 | Sentinel reacts only to `FILLED`, not `PARTIALLY_FILLED` | Acquired inventory can remain unprotected while the rest of the entry rests | Protect incremental executed quantity immediately or cancel remainder and protect the final cumulative fill |
| P0 | Order is sent before durable state is guaranteed | Exchange can accept an order while `save_state` fails, creating an orphan | Persist an intent first, use deterministic `clientOrderId`, submit, then reconcile by client ID |
| P0 | `opened_at` is set at order submission | Entry queue time consumes the OU holding horizon; late fills inherit an almost-expired position | Store `submitted_at`, `first_fill_at`, and `last_fill_at`; start position time-stop at first fill |
| P0 | Shock gate blocks entries but does not invalidate an existing position | A newly detected cascade can leave a long position governed only by slow drift/time-stop | Add explicit shock response for positions: cancel remainder, tighten risk, and flatten after persistence/price confirmation |
| P1 | Shock numerator excludes current closed bar | The newest crash minute is not part of the veto evaluated on that minute | Use current \(S_t\) in the numerator and returns only through \(t-1\) in the volatility estimate |
| P1 | Armed model snapshot is incomplete | Boundaries and half-life drift between ARM and BUY | Freeze model version, \(a^*\), \(m^*\), half-life, volatility and expiry at arming |
| P1 | Rebound rule is fixed at 5 bps | Too permissive in high volatility and unnecessarily strict in very quiet markets | Scale rebound to spread, tick size and recent robust volatility |
| P1 | LIMIT_MAKER price comes from candle close/theoretical boundary, not live book | Order may be rejected as marketable or rest far below the confirmed bounce | Construct price from current best bid and enforce an economic ceiling relative to the frozen boundary |
| P1 | Price and quantity use `f64` and static precision helpers | Tick/step failures and notional drift remain possible | Use decimal/fixed-point values and live `exchangeInfo` filters |

## Shock metric

### Causality and indexing

The implementation clones the rolling close array before appending the newly closed bar. It estimates volatility and both horizon returns entirely from this clone, whose final observation is \(S_{t-1}\). For an entry decision made after observing the close \(S_t\), the code therefore computes:

\[
Z_h^{\text{code}}(t)=
\frac{\ln(S_{t-1}/S_{t-1-h})}
{\hat\sigma_{t-1}\sqrt h},
\]

not the documented statistic \(\ln(S_t/S_{t-h})/(\hat\sigma_{t-1}\sqrt h)\). This is causal and leak-free, but stale by one bar.[^3]

The correct live statistic is:

\[
Z_h(t)=
\frac{\ln(S_t/S_{t-h})}
{\hat\sigma_{t-1}^{\text{robust}}\sqrt h}.
\]

The denominator must remain frozen through \(t-1\), while the numerator should include the current completed close. With a 480-element historical array ending at \(t-1\), use `close_price / closes[n - h]`: for \(h=15\), `closes[n-15]` is \(S_{t-15}\). Reject a candle instead of substituting zero whenever its close is missing, non-finite, or non-positive; the current `unwrap_or(0.0)` can poison logs, arm state, and the next OU fit.[^3]

### Volatility denominator

The ordinary rolling standard deviation is numerically stable at the present sample size, and the `1e-6` floor prevents division by zero. Statistically, however, it has two weaknesses:

- Squared returns allow the cascade itself to inflate the denominator over subsequent minutes, making a persistent sell-off appear progressively less exceptional.
- Multiplication by \(\sqrt h\) assumes approximately uncorrelated, homoskedastic one-minute returns; overlapping crypto returns exhibit volatility clustering and can have short-run dependence.

A robust denominator is preferable for a **shock detector**, because the intended numerator event should not simultaneously redefine normal volatility. Jump-robust measures such as bipower variation estimate the continuous variance component while attenuating isolated jumps, and median-based realized-volatility estimators are also designed for jump and microstructure robustness.[^4][^5][^6]

A simple production implementation is the lagged MAD scale:

\[
\hat\sigma_{\mathrm{MAD},t-1}
=1.4826\,\operatorname{median}_{i\leq t-1}
\left|r_i-\operatorname{median}(r)\right|.
\]

Use 240–480 prior one-minute returns and impose both a small numerical floor and a sanity ceiling. A stronger implementation can maintain two scales:

- `sigma_fast`: EWMA/MAD over approximately 60–120 minutes, used to adapt to current volatility.
- `sigma_slow`: MAD or MedRV over 240–480 minutes, resistant to a cascade redefining the regime.

For downside vetoing, use `max(sigma_fast_prebar, 0.5 * sigma_slow)` only if backtesting supports it. Do not use `max(fast, slow)` blindly, because the larger denominator weakens precisely the alarm being built.

### Threshold calibration

A fixed \(-2.5\) threshold is a defensible conservative starting point, not a calibrated constant. Under an independent Gaussian null, a one-sided \(-2.5\) event has probability about 0.62%; testing two horizons each minute raises the nominal alarm opportunity, while overlap makes successive alarms strongly dependent. Crypto tails and volatility clustering make the Gaussian interpretation unreliable.

Calibrate per symbol with walk-forward data:

1. Compute the statistic using exactly the live causal implementation.
2. Label adverse events by forward maximum drawdown and failure of mean reversion over the intended holding horizon.
3. Search thresholds only on training folds.
4. Choose the least restrictive threshold satisfying a predefined false-negative constraint.
5. Validate on untouched periods, including sell-offs and ordinary volatile sessions.

Until that exists, retain \(-2.5\) for `ARM` suppression but use hysteresis: enter shock at \(Z_{15}<-2.5\) or \(Z_{60}<-2.5\), and clear it only after both exceed approximately \(-1.0\) for 3–5 consecutive completed bars. Without hysteresis, a volatility-inflated denominator can clear the veto too quickly after the first leg.

## Reversal confirmation

### Why five basis points is weak

The rule `green candle AND 5 bps off low` does prevent buying the first threshold touch, but it does not distinguish a true reversal from a one-minute dead-cat bounce. Five basis points can be smaller than a routine one-minute fluctuation during a cascade, while in quiet BTC trading it may be unnecessarily far relative to the spread.

Use a dimensionless rebound requirement:

\[
b_t=\max\left(
3\,\text{spread}_{bps},
2\,\text{tick}_{bps},
\gamma\hat\sigma_{1m,t-1}\times10^4
\right),
\]

with \(\gamma\) initially tested over 0.25–0.50. Then require:

\[
10^4\ln(S_t/S_{\min})\ge b_t.
\]

This is a research grid, not a universal optimum. Log the components and avoid optimizing \(\gamma\) separately for each coin unless the out-of-sample dataset is large enough.

### Better confirmation state

A safer confirmation stack is:

- No new low for two completed one-minute bars.
- Residual or price closes back above the **frozen** entry boundary.
- Three-minute cumulative log return is positive.
- \(Z_{15}\) and \(Z_{60}\) are above the shock-clear thresholds.
- Current robust volatility has not increased materially since arming.
- Expected net profit from the actual proposed limit price to the frozen target remains above the cost hurdle.

Not every item must become a hard gate. A compact production version should require boundary recross, two-bar low persistence, and a cleared shock state. This is materially stronger than one green bar without creating a large parameter surface.

### Boundary and order mismatch

The code stores `armed_state.entry_threshold`, but confirmation compares the live price with both that stored threshold and newly recalculated thresholds. It then submits at `min(close_price, thresholds.entry_long)` and sets the take-profit and half-life from the newly fitted model.[^1][^3]

This creates two failure modes:

- If price has crossed above \(a^*\), the order is posted back at \(a^*\), so the bot may never enter the confirmed reversal; it instead waits for a retest.
- If the OU estimate moves during the armed period, the entry, target and time-stop no longer belong to one optimal-stopping problem.

Choose one explicit policy:

**Retest policy:** Confirmation authorizes a passive order at the frozen \(a^*\), with a short 2–5 minute TTL. This minimizes price concession but has low fill probability.

**Join-bid policy:** Confirmation authorizes a post-only order at the current best bid, capped by an economic maximum:

\[
P_{limit}\le P_{max}
=\exp(m^*-c_{required}),
\]

where \(m^*\), cost hurdle and half-life are frozen at ARM. This is the better match for a reversal-confirmed strategy.

In either case, derive the post-only price from the live book, quantize it downward to the symbol tick, and ensure it does not cross the current ask. Binance specifies that `LIMIT_MAKER` is rejected if it would immediately match; price, quantity and notional must also satisfy `PRICE_FILTER`, `LOT_SIZE` and notional filters.[^7][^8]

## Armed timeout

A fixed 45-minute expiry is too long for fast OU fits and arbitrary for slow ones. At the permitted 15-minute half-life, it equals three half-lives; the original model signal is stale well before expiry. At a 120-minute half-life, it is only 0.375 half-lives.

Use:

\[
T_{arm}=\operatorname{clamp}(0.5t_{1/2},10,30)\text{ minutes}.
\]

Also disarm when any frozen economic assumption fails: model version changes, target-to-entry net edge drops below the hurdle, half-life exits its gate, robust volatility rises above a configured ratio, or the market prints a fresh shock. A new low should update the low-water mark, but it should not indefinitely refresh the original timer.

## Cluster gate

### Risk effect

The one-position cluster cap is appropriate for a 155-USDC validation account because BTC, ETH and SOL signals are not independent during broad liquidations. It reduces maximum gross exposure from 36 to 25 USDC, but raises single-trade concentration from 12 to 25 USDC. It therefore reduces simultaneous beta but more than doubles loss dollars if the selected trade fails.

Keep the 25-USDC amount only if the position-level loss breaker was recalculated. At a 2% adverse move, a 25-USDC position loses roughly 0.50 USDC before fees, already above the previously proposed 0.25%-of-equity risk budget of approximately 0.39 USDC. Size should be derived from loss-at-invalidation rather than available cluster capacity:

\[
N=\min\left(
N_{cap},
\frac{R_{account}}{d_{stop}+f+s}
\right).
\]

### Atomicity defect

`has_active_cluster_position()` reads three JSON files and checks for a non-empty order ID. The read and subsequent order placement are not one atomic operation; every Docker service has only its own in-process mutex. Two symbols can both pass the check before either writes state, then both submit.[^1][^3]

Atomic file rename prevents a single file from being partially written, but it does not implement a cross-process compare-and-set. The gate can also fail open if a state file is unreadable or if configured paths differ from the hard-coded `binance_hmm_{symbol}_state.json` convention.[^1]

Implement one portfolio reservation record:

```text
cluster = crypto_usdc
owner = ETHUSDC
status = RESERVED | ORDER_PENDING | PARTIAL | OPEN | EXITING
lease_id = UUID
expires_at = timestamp
client_order_id = deterministic ID
```

Acquire it atomically before exchange submission and release it only after exchange reconciliation proves that no pending order, fill, or inventory remains. PostgreSQL advisory locks, a unique database row, Redis `SET NX PX`, or a single execution-coordinator service are all suitable; plain JSON files are not.

## Execution edge cases

### Partial fills

The fast sentinel places the take-profit only when entry status equals `FILLED`. Binance exposes `PARTIALLY_FILLED` as a distinct order state, so a partial maker fill can create unprotected inventory while the remainder continues resting.[^9][^3]

Required behavior:

1. Consume user-data-stream execution reports.
2. On every increase in cumulative executed quantity, calculate newly protectable net quantity.
3. Either amend/recreate the take-profit for cumulative inventory or cancel the entry remainder after a short partial-fill TTL.
4. Serialize entry cancellation and exit placement under one per-symbol state machine.

### Durable order lifecycle

`place_ou_limit_buy()` submits to Binance and only then inserts and saves local state; the save result is ignored. A crash or disk error after exchange acceptance creates an orphan.[^2]

Use an event-sourced sequence:

1. Persist `INTENT_CREATED` and fsync.
2. Generate a deterministic unique `clientOrderId`.
3. Submit using that ID.
4. Persist `ACKNOWLEDGED` with exchange order ID.
5. On timeout or restart, query the exchange by client ID before any retry.

Unknown submission outcomes must be treated as `RECONCILE_REQUIRED`, never as failure safe to retry.

### Time-stop origin

`opened_at` is currently assigned when the maker order is submitted, while the time-stop reads that field regardless of fill time. Add:[^2][^3]

- `signal_armed_at`
- `submitted_at`
- `first_fill_at`
- `last_fill_at`
- `position_deadline`

Give pending entries a separate 2–5 minute TTL. Freeze the position deadline at first fill as `first_fill_at + 2 * frozen_half_life`; do not allow re-estimation or additional partial fills to extend it.

### Cancellation and balances

`emergency_flatten()` cancels orders and immediately reads free balance. If cancellation has not reached a terminal exchange state, inventory can remain locked; a zero free balance currently allows state removal. That can strand a real position.[^2]

Wait for terminal order updates, then reconcile `free + locked`, cumulative buys, cumulative sells and fees. Never delete state merely because quantized free balance is zero. Mark `DUST`, `LOCKED_PENDING_CANCEL`, or `MANUAL_RECONCILIATION` and retain the record until exchange inventory is explained.

### Existing-position shock response

Inside the position branch, emergency flattening is triggered by `!is_consolidation`, which currently reflects slow drift and the hard-coded HMM regime. `is_shock` participates in entry eligibility but not in position invalidation.[^3]

Avoid flattening on every one-minute \(-2.5\) print, but add an explicit escalation policy:

- First shock: cancel unfilled entry remainder and prohibit TP relaxation.
- Persistent shock for two closes or combined shock plus adverse residual breach: price-protected IOC flatten.
- Shock clears before persistence and position remains inside risk budget: retain the original frozen deadline.

### Exchange filters and arithmetic

The implementation quantizes quantity through symbol precision, but the reviewed path passes theoretical entry and exit prices directly. Binance requires prices to be exact multiples of `tickSize`, quantities to satisfy `stepSize`, and notional to meet active filters.[^10][^7]

Load and cache current `exchangeInfo`, refresh it periodically and after filter errors, and represent monetary values with a decimal or integer tick/lot type. Validate both the entry and prospective exit after downward quantity rounding, because a later price decline can push an emergency market exit below minimum notional.

## Recommended release gate

Do not increase live size until all P0 items are closed. The next micro-account build should use:

- Current-bar shock numerator with lagged robust scale.
- Shock hysteresis and cooldown.
- Atomic cluster reservation.
- Frozen ARM snapshot and dynamic arm TTL.
- Two-bar/boundary-recross confirmation.
- Live-book-derived post-only price with 2–5 minute order TTL.
- Immediate partial-fill protection.
- Durable idempotent order intents.
- Fill-based position deadline.
- Explicit persistent-shock handling for open positions.

The upgraded strategy should then be shadow-replayed first. Compare the old touch entry, current 5-bps rule, and proposed boundary-recross rule using identical signals and actual book snapshots. Report opportunity count, fill probability, five-second adverse selection, maximum adverse excursion, time-stop rate and net expectancy. The preferred variant is the one improving net expectancy and tail loss—not simply the one with the highest apparent win rate.

---

## References

1. [state.rs](local-repo-reference) - use chronoUtc use serdeDeserialize, Serialize use serdejsonValue use stdcollectionsHashMap use stdfs...

2. [order.rs](local-repo-reference) - use cratebinanceBinanceClient, getsymbollotprecision, quantizelotsize use cratestatePositionState, S...

3. [main.rs](local-repo-reference) - use stdsyncArc use tokiosyncMutex, RwLock use stdtimeDuration use futuresutilStreamExt use tokiotung...

4. [Power and Bipower Variation with Stochastic Volatility and ...](https://academic.oup.com/jfec/article/2/1/1/960705) - Abstract. This article shows that realized power variation and its extension, realized bipower varia...

5. [5.1 Realized measures of volatility | Financial econometrics using R](https://bookdown.org/jarneric/financial_econometrics/5.1-realized-measures-of-volatility.html) - 5.1 Realized measures of volatility | Financial econometrics using R

6. [Jump-Robust Volatility Estimation using Nearest Neighbor ...](https://www.newyorkfed.org/medialibrary/media/research/staff_reports/sr465.pdf)

7. [Filters | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/filters) - Filters define trading rules on a symbol or an exchange. Filters come in three forms: symbol filters...

8. [binance-spot-api-docs/rest-api.md at master - GitHub](https://github.com/binance/binance-spot-api-docs/blob/master/rest-api.md) - Official Documentation for the Binance Spot APIs and Streams - binance/binance-spot-api-docs

9. [SPOT API Glossary | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/spot_glossary) - Disclaimer: This glossary refers only to the SPOT API Implementation. The definition for these terms...

10. [Schemas | Binance Developer Docs](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/ws-api/~schemas) - List of schemas used by the API.

