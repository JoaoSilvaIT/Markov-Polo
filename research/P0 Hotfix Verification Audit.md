# P0 Hotfix Verification Audit

## Verdict

**No: the submitted implementation does not yet satisfy all production-safety requirements for unattended live deployment.** The shock indexing, MAD scale, frozen thesis, fill-based clock, entry TTL and post-only book lookup are substantially improved. However, several P0 defects from the previous audit remain in the code: partial fills are still not protected, exchange-accepted orders can still become orphaned, the cluster lease expires while filled positions remain open, and cancellation/flattening can clear state without proving terminal exchange state.[^1][^2]

A tightly supervised micro-account can be used only after the blockers below are repaired and failure-injection tests pass. The mathematical improvements do not compensate for uncertain inventory or order state.

## Requirement status

| Requirement | Status | Finding |
|---|---|---|
| Current-bar shock numerator | Pass | `current_close / closes[n-h]` correctly represents \(S_t/S_{t-h}\), while MAD uses returns through \(t-1\).[^1] |
| Robust lagged volatility | Conditional pass | MAD is causal, but the fixed `1e-5` floor is economically large for quiet one-minute returns and must be calibrated/logged.[^3] |
| Frozen ARM thesis | Mostly pass | Entry, exit and half-life are frozen, but model version, cost hurdle and volatility snapshot are not stored.[^4][^1] |
| Dynamic ARM timeout | Pass | The implementation evaluates `clamp(0.5 * frozen_half_life, 10, 30)` correctly.[^1] |
| Atomic cluster acquisition | Partial | `create_new` serializes acquisition only if every container sees the same filesystem inode namespace; lease lifecycle is unsafe after five minutes.[^4][^2] |
| Post-only pricing | Partial | Best bid is queried and price is capped, but prices are hard-coded to two decimals and not validated against live symbol filters.[^5][^2] |
| Five-minute entry TTL | Partial | TTL exists, but partial fills and failed/ambiguous cancellation are mishandled.[^1][^2] |
| Fill-based holding clock | Partial | Clock is reset when REST first observes `FILLED`, not at exchange first-fill time; partial fills remain outside the position clock.[^1] |
| Severe-shock flatten | Partial | It is implemented, but a single threshold crossing can trigger an immediate taker exit and the flatten procedure lacks terminal-state guarantees.[^1][^2] |
| Partial-fill safety | Fail | Sentinel still branches only on exact `FILLED` status.[^1] |
| Durable/idempotent submission | Fail | No persisted intent or `newClientOrderId`; state is written only after exchange acceptance.[^5][^2] |
| Reconciliation-safe flatten | Fail | State and lease can be cleared after ignored cancel failures or a zero free balance.[^2] |

## Verified mathematics

### Shock indexing

The historical close vector ends at \(S_{t-1}\), while `current_close` is the newly completed \(S_t\). Consequently:

- `closes[n - 15]` equals \(S_{t-15}\).
- `closes[n - 60]` equals \(S_{t-60}\).
- MAD is calculated solely from returns ending at \(t-1\).

The implementation therefore matches:

\[
Z_h(t)=\frac{\ln(S_t/S_{t-h})}{\hat\sigma_{MAD,t-1}\sqrt h}.
\]

There is no look-ahead bias in this computation.[^1]

Reject malformed candles before signal evaluation. `close_price` currently falls back to zero on parse failure; although the shock block checks `current_close > 0`, a zero close is still appended later and can corrupt the next 480-minute window.[^1]

### MAD behavior

The median and median absolute deviation calculations are structurally correct, including averaging the middle pair for even samples and applying 1.4826 normal-consistency scaling. A floor is necessary, but `1e-5` means a minimum one-minute volatility of one basis point. That may suppress shocks during quiet periods rather than merely prevent division by zero.[^3]

Separate numerical and economic floors:

```rust
const NUMERIC_SIGMA_FLOOR: f64 = 1e-12;
let raw_sigma = 1.4826 * mad;
let sigma = raw_sigma.max(symbol_calibrated_floor);
```

Log `raw_sigma`, the applied floor and whether the floor was active. Calibrate the economic floor from an out-of-sample lower percentile of lagged one-minute volatility for each market, or use a shared conservative floor chosen before testing.

A \(-3.0\) threshold is suitable as an initial conservative deployment parameter, but it is not proven optimal by this implementation. Keep it frozen during the next run and record counterfactual alarms at \(-2.5\), \(-3.0\), and \(-3.5\) without changing live behavior.

## Remaining P0 blockers

### Partial fills remain unprotected

The supervisor handles an entry only when `order_info["status"] == "FILLED"`. It does nothing when status is `PARTIALLY_FILLED`, even though executed inventory already exists. Binance explicitly defines `PARTIALLY_FILLED` as a separate live order state, and its real-time user stream reports each order update through `executionReport`, including cumulative executed quantity.[^6][^7][^1]

This creates the exact unresolved scenario:

1. A 25-USDC maker order fills 40%.
2. The remaining 60% rests for up to five minutes.
3. No take-profit exists, `is_filled` remains false and the holding clock does not start.
4. At TTL or shock, `emergency_flatten` attempts cancellation and balance inference.

Required fix:

- Treat `executedQty > protectedQty` as inventory immediately, independent of status.
- Add `executed_qty`, `protected_qty`, `first_fill_at`, and `entry_terminal` to `PositionState`.
- Prefer the Spot User Data Stream; account and order events are pushed in real time.[^7]
- On the first partial fill, either cancel the unfilled remainder and protect/exit the cumulative inventory, or maintain a single exit whose quantity is amended as inventory grows.
- Start the position clock from the exchange trade timestamp of the first fill, not the time REST happens to observe `FILLED`.

### Order submission is not durable

`place_ou_limit_buy` acquires the lease, calls Binance, extracts the exchange order ID, inserts local state, then invokes `save_state` without checking its result. A process crash, response loss, permission error or full filesystem after exchange acceptance can leave a live order that does not exist locally.[^2]

The client does not send `newClientOrderId`. Binance supports a user-supplied client order ID to uniquely identify an open order, enabling deterministic reconciliation after an ambiguous request.[^5][^8][^6]

Required sequence:

1. Atomically persist and fsync an `INTENT_CREATED` record containing a UUID/client ID.
2. Submit the order with that `newClientOrderId`.
3. If the HTTP result is ambiguous, query by `origClientOrderId`; never blindly retry.
4. Persist `ACKNOWLEDGED` with exchange order ID.
5. On restart, reconcile every nonterminal intent and all exchange open orders before enabling entries.

All state-save failures must propagate and trigger a global halt. Atomic rename protects against torn JSON content, but not against ignored I/O failure or an unrecorded exchange side effect.

### Lease expires during open positions

The cluster lease is acquired with a 300-second expiry. It is refreshed only if the same symbol calls `try_acquire_cluster_lease` again, but there is no renewal loop after submission or fill. Five minutes after acquisition, another container can remove the “stale” lock and acquire it even while the first symbol has an open position.[^4][^2]

The fallback scan of per-symbol state files reduces this risk only if all containers share those files under the exact hard-coded names. It is not a substitute for a valid lease, and the code cannot prove that configured state paths match those names.[^4]

Required fix:

- Renew the lease periodically, for example every 30 seconds with a 120-second TTL, for every nonterminal lifecycle state: `ORDER_PENDING`, `PARTIAL`, `OPEN`, and `EXITING`.
- Store a random `lease_id`, not only `owner`, and require owner plus lease ID for renewal and release. This prevents an old process instance from deleting a newer lease belonging to the same symbol.
- On inability to renew, block all new orders globally and reconcile; do not release or flatten solely because of transient lock storage failure.
- Ensure the lock resides on a genuinely shared POSIX filesystem. If each Docker container has a private writable layer, `O_EXCL` provides no cross-container exclusion. On NFS or unusual overlay/network filesystems, verify `O_EXCL` semantics under the actual deployment.

There is also a stale-lock race: two processes can both observe expiry and both call `remove_file`; one can delete while the other creates a replacement. A robust file implementation should rename an expired lock to a unique tombstone before attempting `create_new`, then verify lease ID after acquisition. A transactional database or Redis compare-and-set remains safer.

### Flatten is not terminal-state safe

`emergency_flatten` ignores cancellation errors, immediately queries only free balance, and can remove local state plus release the cluster lease when the sellable balance quantizes to zero. Inventory may still be locked by an entry or TP order, a cancel may have raced a fill, or the REST balance query may have failed and returned zero.[^2]

Required fix:

- Do not use `unwrap_or(0.0)` for risk-critical balance retrieval.
- After cancel, wait for `CANCELED`, `FILLED`, `EXPIRED`, or another terminal state through execution reports or bounded polling.
- Re-fetch the entry and TP cumulative fills.
- Reconcile `free + locked` inventory, cumulative buys, cumulative sells and commissions.
- Remove state and release the lease only after exchange open orders are absent and attributable inventory is below a recorded dust threshold.
- If reconciliation cannot be proven, enter `RECONCILE_REQUIRED`, retain the lease and page the operator.

Market orders themselves can partially fill under insufficient liquidity and finish `EXPIRED`; success of the REST call is not proof that the requested quantity was fully liquidated.[^9]

## Post-only defects

### Tick-size handling

`create_limit_buy` and `create_limit_sell` serialize every price with `format!("{:.2}", price)`. That is not a general Binance filter implementation. The fallback `top_ask - 0.01` also hard-codes a one-cent tick, and the quantity helper hard-codes decimal precision by symbol rather than consuming `stepSize`.[^5][^2]

Use live `exchangeInfo` filters and integer tick/step arithmetic:

\[
P_{buy}=\left\lfloor\frac{\min(P_{bid},P_{ceiling})}{tickSize}\right\rfloor tickSize.
\]

Then validate:

- `P_buy < bestAsk` immediately before submission.
- `price % tickSize == 0`.
- `quantity % stepSize == 0`.
- Quantity lies inside `LOT_SIZE`.
- Entry and emergency-exit notional satisfy the active notional filters.

`LIMIT_MAKER` will reject an order that would immediately trade as taker, so the exchange still provides a final safety boundary. However, a rejection followed by lease release must not be confused with an ambiguous transport failure.[^10]

### Market-data failure

If `fetch_book_ticker` fails, the code silently substitutes `(0, 0)` and falls back to the theoretical entry price. This is fail-open behavior for the new book-aware safety control.[^2]

Change it to fail closed: release the lease only after proving no order was submitted, return an error, and do not trade without a fresh bid/ask whose age is within a configured limit.

## Model-freezing caveat

The frozen entry threshold, target and half-life are correctly used at Stage 2. However, the submitted price remains:[^4][^1]

```rust
min(best_bid, frozen_entry_threshold)
```

because the caller passes `min(close_price, frozen_entry)` and the order function caps again at that value. After a confirmed rebound above the boundary, this posts at the old boundary rather than joining the current bid. That is a valid **retest policy**, but it is not a true join-bid policy and may sharply reduce fill probability.[^1][^2]

Document and test the intended behavior:

- **Retest:** cap at frozen \(a^*\), use a short TTL, accept lower fills.
- **Join bid after confirmation:** allow best bid up to an economic ceiling derived from the frozen target and required net edge.

Also freeze the cost assumption and model version so an audit can reconstruct the exact thesis.

## Shock exits

Any single \(Z_{15}<-3\) or \(Z_{60}<-3\) observation now immediately invokes emergency market flatten for both pending entries and filled positions. This is safer against cascades than the prior implementation, but it can create taker whipsaws from one bad candle or bad print.[^1]

For the micro-account validation phase, immediate cancellation of unfilled remainder is appropriate. For filled inventory, require either:

- Two consecutive severe-shock closes; or
- One severe shock plus an adverse residual/price breach beyond a hard risk boundary.

Keep a separate catastrophic threshold, such as a much more extreme statistic or account-risk limit, that flattens immediately. This two-level design avoids turning every statistical alarm into an unconditional taker stop.

## Release checklist

Before the next unattended live run, require all of the following:

- Partial fills create protected inventory and start a first-fill clock.
- Deterministic `newClientOrderId` and durable pre-submit intents are implemented.
- Startup reconciles intents, open orders, fills and balances before enabling trading.
- Cluster lease is renewed throughout pending, partial, open and exiting states.
- Lease uses a unique fencing token and is stored on a verified shared filesystem.
- Book-ticker failure blocks submission.
- Tick size, step size and notional filters come from live exchange metadata.
- Cancel and market-sell outcomes are reconciled to terminal states.
- No zero/free-balance fallback may clear state.
- Malformed or zero candles are rejected rather than appended.
- Integration tests inject: HTTP timeout after exchange acceptance, crash before state save, partial fill then TTL, fill/cancel race, stale lease takeover, stale book, TP placement failure, balance API failure and partially filled emergency sell.

After those pass, deploy at the minimum practical notional with manual supervision and zero size increase. “Production safe” should mean exchange state remains recoverable under failures—not merely that the normal path executes correctly.

---

## References

1. [main.rs](local-repo-reference) - use stdsyncArc use tokiosyncMutex, RwLock use stdtimeDuration use futuresutilStreamExt use tokiotung...

2. [order.rs](local-repo-reference) - use cratebinanceBinanceClient, getsymbollotprecision, quantizelotsize use cratestatePositionState, S...

3. [ou_process.rs](local-repo-reference) - use stdf64 Represents the mathematical parameters of an Ornstein-Uhlenbeck stochastic process. deriv...

4. [state.rs](local-repo-reference) - use chronoUtc use serdeDeserialize, Serialize use serdejsonValue use stdcollectionsHashMap use stdfs...

5. [binance.rs](local-repo-reference) - use reqwestClient use serdejsonValue use hmacHmac, Mac use sha2Sha256 use stdtimeSystemTime, UNIXEPO...

6. [SPOT API Glossary | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/spot_glossary) - Disclaimer: This glossary refers only to the SPOT API Implementation. The definition for these terms...

7. [User Data Streams for Binance](https://developers.binance.com/en/docs/products/spot/user-data-stream) - outboundAccountPosition is sent any time an account balance has changed and contains the assets that...

8. [Trade - Spot REST API | Binance Developer Docs](https://developers.binance.com/en/docs/catalog/core-trading-spot-trading/api/rest-api/trade) - Access market data, manage accounts, and trade on Binance Spot.

9. [MARKET orders | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/faqs/market_orders_faq) - Disclaimer:

10. [developers.binance.com › en › docsFIX API | Binance Developer Docs](https://developers.binance.com/en/docs/products/spot/fix-api) - If you are using Node.js and connecting via raw TLS sockets (tls.connect()), you must explicitly set...

