# Dynamic Baseline and Lease Validation

## Verdict

**No. The new build improves baseline translation and fail-closed quote handling, but it does not complete the production-safety requirements.** Dynamic translation is mathematically coherent only as a deliberately moving-boundary heuristic; it is not the original time-homogeneous Bertram stopping problem. More importantly, the code still leaves partial fills unprotected during normal operation, does not reconcile cancellation to a terminal exchange state, can clear state on a failed balance lookup, ignores lease-renewal failure, and persists accepted orders only after submission.[^1][^2][^3]

A supervised minimum-notional test is reasonable only after the remaining P0 items below are fixed. Unattended deployment is not yet justified.

## Implementation status

| Change | Assessment | Reason |
|---|---|---|
| Residual deviation storage | Implemented | ARM state stores log ratios for entry and exit.[^2][^3] |
| Live-baseline translation | Implemented, model caveat | Stage 2 multiplies live SMA by exponentiated stored deviations.[^3] |
| Persistent lease renewal | Incomplete | Renewal occurs only on closed one-minute candle processing and its Boolean result is ignored.[^2][^3] |
| State-file cluster fallback | Fragile | It depends on hard-coded filenames and shared filesystem visibility rather than configured paths.[^2] |
| Fail-closed top-of-book | Implemented | Missing, nonpositive or crossed quotes abort before submission.[^1] |
| Partial-fill protection | Incomplete | Only `emergency_flatten` inspects `executedQty`; the 1.5-second supervisor still reacts solely to `FILLED`.[^1][^3] |
| Terminal flatten verification | Not implemented | Cancel errors are ignored and market-sell REST success is treated as complete liquidation.[^1] |
| Durable submission/recovery | Not implemented | Exchange submission still precedes durable state and has no client order ID.[^1] |
| Exchange filter compliance | Not demonstrated | Quantity remains symbol-hard-coded; the shown order path does not load tick/step/notional filters.[^1] |

## Dynamic translation

At ARM, the implementation computes:

\[
d_a=\ln\left(\frac{a^*}{B_{arm}}\right),\qquad
d_m=\ln\left(\frac{m^*}{B_{arm}}\right),
\]

and at bounce reconstructs:

\[
A_{bounce}=B_{bounce}e^{d_a},\qquad
M_{bounce}=B_{bounce}e^{d_m}.
\]

That dimensional transformation is correct: the stored quantities are scale-free log deviations, and the reconstructed boundaries preserve their relative displacement from the chosen baseline.[^2][^3]

However, the OU estimator and Bertram thresholds are still calculated from raw price closes, while the execution layer subsequently reinterprets those absolute-price thresholds as residual offsets around an SMA. That is not equivalent to fitting an OU directly to the residual process[^3]

\[
y_t=\ln S_t-\ln B_{t|t-1}.
\]

For a mathematically consistent residual strategy, estimate \(\theta\), \(\mu_y\), \(\sigma_y\), costs and stopping boundaries from the historical residual series itself. Then store the residual boundaries directly and translate them to prices. The current approach is a useful engineering approximation, but it should not be described as an exact residual-OU/Bertram implementation.

### Baseline timing

`live_sma` is calculated from `rolling_closes`, which contains bars only through \(t-1\), not the newly closed bounce bar \(S_t\). This is causal and avoids using the current observation to move its own boundary, but the variable name and stated formula are misleading. The implemented baseline is:[^3]

\[
B_{bounce}=\operatorname{SMA}_{480,t-1},
\]

which is preferable for out-of-sample signal integrity. Keep this behavior and document it explicitly.

### Moving-boundary limitation

Translating the frozen deviations using a changed baseline means the absolute target can move between ARM and order submission. That intentionally avoids stale absolute prices, but it abandons the fixed-boundary first-passage assumptions behind the original closed-form optimum. The policy must therefore be validated as a complete walk-forward rule rather than relying on Bertram optimality.

Add two economic checks immediately before submission:

\[
\ln\left(\frac{M_{bounce}}{P_{entry}}ight)
>c_{roundtrip}+m_{safety},
\]

and:

\[
M_{bounce}>P_{entry}+tickSize.
\]

Otherwise, a falling baseline can compress the remaining spread below fees and slippage even though `close_price < dynamic_exit_target` remains true.

The fallback test `unitless_*_dev != 0.0` is also ambiguous because zero is a valid residual boundary. Add an explicit schema/version or `Option<f64>` rather than using zero as “field absent.”[^2][^3]

## Lease lifecycle

The one-hour TTL is refreshed inside the closed-candle hot path whenever local state contains the symbol. This is better than the earlier five-minute lease, but the claim “every minute” holds only while the market WebSocket continues delivering valid completed candles and the hot path is not stalled.[^3]

The call result is discarded:

```rust
let _ = refresh_cluster_lease(symbol, 3600);
```

If the lock disappears, becomes corrupted, the volume goes read-only, or another process takes ownership, trading continues without escalation. Move lease renewal into an independent watchdog and treat failure as `RECONCILE_REQUIRED`: block new entries portfolio-wide, preserve state, and alert.[^2][^3]

The lease has only `owner`, `acquired_at`, and `expires_at`; it has no random fencing token. A restarted BTC container can share the same owner string as its predecessor, and stale code can refresh or release a newer BTC lease. Add a unique `lease_id` per acquisition and require `(owner, lease_id)` for refresh and release.[^2]

`refresh_cluster_lease` rewrites the lock directly with `fs::write`, so another reader can observe truncated or malformed JSON during renewal. Write a temporary file, flush it, and atomically replace the lock while preserving fencing semantics—or use a transactional coordination service.[^2]

The state-file fallback constructs filenames such as `binance_hmm_state_{SYMBOL}.json`, while the process itself loads a configurable state path. Unless Docker mounts and naming conventions exactly match, the fallback can miss a genuine position. Use a shared portfolio registry keyed by symbol rather than guessing per-container filenames.[^3][^2]

## Partial fills

The new emergency path reads `executedQty`, so a TTL or shock can detect that a supposedly pending entry acquired inventory. That is useful recovery logic, but it does not constitute continuous partial-fill protection.[^1]

The fast supervisor still waits for exact `FILLED`; `PARTIALLY_FILLED` inventory can remain unprotected until TTL, shock or another invalidation. Binance defines `PARTIALLY_FILLED` as a distinct live state and exposes cumulative `executedQty` for filled quantity.[^4][^5][^3]

Required behavior on every supervisor cycle or execution report:

1. Read cumulative executed quantity regardless of status.
2. If it exceeds the locally protected quantity, cancel the remaining entry immediately or update protection for the delta.
3. Record the first exchange fill timestamp and start the holding clock there.
4. Persist `executed_qty`, `protected_qty`, and terminal entry status.
5. Never allow both an active buy remainder and a full-quantity sell unless the strategy explicitly manages that coupled exposure.

A user-data execution stream is preferable to 1.5-second REST polling because order updates are event-driven; at minimum, the current poller must handle `PARTIALLY_FILLED`, `CANCELED`, `EXPIRED`, and `REJECTED`, not only `FILLED`.[^6][^4]

## Flattening defects

### Cancellation race

For a nominally unfilled order, the code fetches status, then sends cancellation, ignores the cancellation response, removes local state and releases the lease. The order can fill between the status read and cancel processing. This remains an orphan-inventory race.[^1]

Treat the cancellation response or a subsequent order query as authoritative. Only clear state after a terminal order status and final `executedQty` reconciliation. Binance cancellation responses expose executed quantity, and order status distinguishes `NEW`, `PARTIALLY_FILLED`, `FILLED`, `CANCELED`, and `EXPIRED`.[^5][^7]

### Balance fail-open

`fetch_free_balance(...).await.unwrap_or(0.0)` converts API failure into “no inventory,” after which the code removes state and releases the cluster lease. This is a direct P0 safety violation. A failed balance request must preserve state and lease, block trading, and retry/reconcile.[^1]

### Incorrect partial-fill cap

Although `executedQty` is read, the sell quantity later uses:

\[
\min(pos.amount, freeBalance),
\]

not:

\[
\min(finalExecutedQty-netAlreadySold, freeBalance).
\]

Therefore, if the account holds pre-existing base asset, a partially filled strategy order can sell more than that order acquired, up to the original requested amount. Track strategy-attributable inventory explicitly and cap liquidation by final cumulative fills minus cumulative exits and base-asset fees.[^1]

### Market-sell completion

A successful REST response does not prove a market order fully filled. Binance documents that a market order can partially fill and then expire if book liquidity is exhausted. Inspect the returned status and `executedQty`, then continue reconciliation until attributable inventory is below the exchange step size/dust limit.[^8]

## Submission durability

The entry is still submitted to Binance before local state is saved, and the save result is ignored. A crash or I/O failure in that interval creates an exchange order with no recoverable local identity.[^1]

Implement before unattended operation:

- Generate a deterministic unique `newClientOrderId`.
- Persist and fsync `INTENT_CREATED` before submission.
- Submit using that client ID.
- On timeout or malformed response, query by client ID instead of retrying blindly.
- Persist acknowledgment and exchange ID.
- At startup, reconcile all nonterminal intents, exchange open orders, trades and balances before enabling signals.

Binance supports client-supplied order IDs and rejects duplicate active identifiers, making them suitable for idempotent recovery.[^9]

## Exchange filters

The submitted order path still uses hard-coded lot precision and floating-point flooring. Binance symbol rules are defined by `PRICE_FILTER`, `LOT_SIZE`, `MARKET_LOT_SIZE`, and notional filters; valid prices and quantities must align with `tickSize` and `stepSize`.[^10][^1]

Load and cache `exchangeInfo`, refresh it periodically, and quantize with decimal/integer arithmetic rather than binary `f64`. Validate both the initial maker order and the emergency market sell, especially because a small partial fill may fall below minimum sellable notional.

## Minimum release gate

Before calling this build production-ready, require all of the following:

- Residual OU is actually calibrated on causal log residuals, or the current translation is explicitly labeled and tested as a heuristic.
- Net edge is recomputed after dynamic translation and before order submission.
- Partial fills are handled continuously, not only during emergency flatten.
- Cancel responses and final executed quantities are reconciled before state deletion.
- Balance/API failure can never be interpreted as zero inventory.
- Emergency sell quantity is capped by strategy-attributable inventory.
- Market-sell completion is verified after submission.
- A durable pre-submit intent and `newClientOrderId` provide idempotent recovery.
- Lease renewal has a fencing token, independent watchdog, checked failures, atomic persistence, and shared-storage verification.
- State discovery uses configured shared paths or a central portfolio registry.
- Price, quantity and notional obey live exchange filters.
- Failure-injection tests cover partial fill plus cancel race, API timeout after acceptance, balance outage, market-sell partial fill, lease corruption/loss, process crash before state save, and WebSocket outage.

After those tests pass, the appropriate release is still a supervised minimum-notional canary—not an immediate size increase.

---

## References

1. [order.rs](local-repo-reference) - use cratebinanceBinanceClient, getsymbollotprecision, quantizelotsize use cratestatePositionState, S...

2. [state.rs](local-repo-reference) - use chronoUtc use serdeDeserialize, Serialize use serdejsonValue use stdcollectionsHashMap use stdfs...

3. [main.rs](local-repo-reference) - use stdsyncArc use tokiosyncMutex, RwLock use stdtimeDuration use futuresutilStreamExt use tokiotung...

4. [SPOT API Glossary | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/spot_glossary) - newClientOrderId Parameter used in the SPOT API to assign the clientOrderId for the order being plac...

5. [Spot Glossary | Binance Open Platform](https://developers.binance.com/docs/binance-spot-api-docs/faqs) - Disclaimer: This glossary refers only to the SPOT API Implementation. The definition for these terms...

6. [FIX API | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/fix-api) - LIMIT_MAKER, 38, 44, This is a LIMIT order that will be rejected if the order immediately matches an...

7. [Public Rest API for Binance SPOT Testnet](https://developers.binance.com/en/docs/products/spot/testnet/rest-api) - The new client order ID for the order after being amended. If not sent, one will be randomly generat...

8. [MARKET orders | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/market_orders_faq) - your MARKET order will be partially filled to the extent of available liquidity, if you monitor the ...

9. [Trade - Spot REST API | Binance Developer Docs](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/rest-api/trade) - Orders with the same newClientOrderID can be accepted only when the previous one is filled, otherwis...

10. [Filters | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/filters) - Filters define trading rules on a symbol or an exchange. Filters come in three forms: symbol filters...

