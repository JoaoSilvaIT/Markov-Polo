# Intraday Crypto Perpetual Breakout Research Brief for Hyperliquid L1

## Overview
This report evaluates three practical questions for a small-account Hyperliquid L1 perpetual system: how to pre-filter 15-minute Donchian breakouts with a volatility-compression gate, how to design exits under strong maker-versus-taker fee asymmetry and minimum-order constraints, and how to deploy native exchange-side trigger stops without overloading the API budget.[cite:3][cite:7][cite:10] The analysis combines official Hyperliquid documentation, the official Rust SDK structures, and a 2024-01 through 2026-09 event study on BTC, ETH, SOL, and AVAX 15-minute perpetual data using a Donchian-20 breakout definition and ATR-normalized path metrics.[cite:3][cite:10][cite:62][cite:63][cite:64]

The core conclusion is operational rather than theoretical: the best production design is a **two-layer stop architecture** with one native disaster stop-market order resting on Hyperliquid and a faster client-side ratcheting daemon for normal stop updates, while profit-taking should usually be handled by a single maker take-profit order only when the entire position can be represented above the minimum notional floor.[cite:3][cite:7][cite:10] The event study also shows that a naive “fresh squeeze” label does not automatically dominate a broad breakout sample, so the compression filter should be used mainly to reject visibly extended impulses rather than as a stand-alone alpha signal.[cite:62][cite:63]

## Data and framing
The official Hyperliquid documentation states that TP/SL orders are triggered by the **mark price**, not the last trade, which matters directly for liquidation cascades and wick-heavy microstructure because the stop trigger source is more stable than trade prints.[cite:7] Hyperliquid also documents a shared 1,200-weight-per-minute REST budget and user-level open-order limits, including the rule that reduce-only and trigger orders can be rejected when the account already has at least 1,000 open orders.[cite:3]

The official Rust SDK defines trigger orders through a `Trigger` struct with `is_market`, `trigger_px`, and `tpsl` fields, nested inside `Order::Trigger`, and the signed order payload is carried in `OrderRequest` with `asset`, `is_buy`, `limit_px`, `sz`, `reduce_only`, `order_type`, and optional `cloid`.[cite:10] That structure is sufficient to maintain a reduce-only stop-market disaster stop alongside a resting maker take-profit order, and it aligns with the exchange documentation for trigger semantics.[cite:7][cite:10]

## Compression filter
A practical compression-and-residual-budget gate should combine three terms rather than relying on a single squeeze indicator. The recommended state vector on 15-minute bars is:

\[
\text{DCW}_{20,t} = \frac{\max(H_{t-20:t-1}) - \min(L_{t-20:t-1})}{\text{ATR}_{20,t}}
\]

\[
\text{BBWATR}_{20,t} = \frac{4\,\sigma_{20}(C_{t-20:t-1})}{\text{ATR}_{20,t}}
\]

\[
Z_{60,t} = \frac{\ln(C_t/C_{t-4})}{2\,\sigma_{r,96,t-1}}
\]

where ATR is Wilder ATR-20 on 15-minute bars, the Donchian channel is shifted one bar to avoid look-ahead, and \(\sigma_{r,96,t-1}\) is the rolling standard deviation of 15-minute log returns over the prior 96 bars.[cite:62][cite:63] The causal decision rule is then: accept only breakouts with low channel-width expansion and low prior 60-minute extension, and reject breakouts where the channel is already wide or where the last hour’s standardized move is already stretched.[cite:62][cite:63]

Asset-specific calibration from the event study suggests these “coil” cutoffs for a permissive fresh-breakout gate and these “late chase” cutoffs for a hard reject gate:[cite:63]

| Asset | Fresh if DCW20 <= | Fresh if BBWATR20 <= | Fresh if \(|Z_{60}|\) <= | Reject if \(|Z_{60}|\) >= |
|---|---:|---:|---:|---:|
| BTC | 3.207[cite:63] | 2.518[cite:63] | 1.744[cite:63] | 2.347[cite:63] |
| ETH | 3.149[cite:63] | 2.370[cite:63] | 1.768[cite:63] | 2.394[cite:63] |
| SOL | 3.163[cite:63] | 2.516[cite:63] | 1.759[cite:63] | 2.261[cite:63] |
| AVAX | 3.203[cite:63] | 2.567[cite:63] | 1.740[cite:63] | 2.222[cite:63] |

These thresholds should not be interpreted as proof that the “fresh” bucket has the highest unconditional forward MFE. In the broad sample, median 60-minute MFE clustered near 0.87x to 0.98x ATR across all three buckets, which means the filter is better framed as a **risk-control veto** than as an independent entry engine.[cite:62] In production terms, the most useful rule is therefore: do not fire the breakout engine when \(\text{DCW}_{20}\ge 4.5\) or when \(|Z_{60}|\) exceeds the asset’s 90th-percentile threshold, because those are the conditions most consistent with negative residual move budget after an already-expanded impulse.[cite:62][cite:63]

The supporting event-study summary is attached below.[cite:62]

[cite:62]

A visual comparison of median 60-minute MFE by bucket is also attached below.[cite:60]

[cite:60]

## Exit design
The event study is highly consistent with the live observation that these 15-minute crypto breakouts often realize their useful move quickly: the median 60-minute MFE is roughly 0.93x to 0.96x ATR across the four assets, and the median 120-minute MFE rises only modestly from that level.[cite:62] That pattern argues against a pure open-ended chandelier as the only monetization path, because the system often gets paid briefly and then gives the move back.[cite:62]

The EV comparison done here is intentionally conservative. It uses a maker-win assumption of -1.0 bps, a loss-side friction proxy of +8.0 bps to approximate taker fee plus slippage on stop execution, and checks whether a fixed take-profit at \(k_{TP}\times ATR\) is hit within 60 minutes; when it is not hit, the trade is marked against subsequent adverse excursion as a stop-side proxy.[cite:64] Under that simple proxy, the best \(k_{TP}\) in-sample for all four assets collapsed to 0.50x ATR, but the absolute edge remained negative because the broad breakout definition itself is not selective enough after costs.[cite:61][cite:64]

That result does **not** mean the strategy is unusable. It means the raw Donchian breakout needs stricter gating, faster profit harvesting, or both. For Hyperliquid small accounts, the best practical exit stack is:

- Use one full-size resting reduce-only maker take-profit order when notional size is too small to split above the exchange minimum.[cite:7]
- Set the first TP target near 0.50x to 0.75x ATR20 on 15-minute bars, because the sample’s median 60-minute realized opportunity is near 1.0x ATR and fees make waiting for large tails expensive.[cite:61][cite:62][cite:64]
- Run a client-side giveback or chandelier ratchet only after price has moved at least 0.75x ATR in favor, because an earlier ratchet likely converts noise into churn in these instruments.[cite:62]
- Avoid multi-leg partial scaling when any child leg would fall below the exchange minimum notional floor, because execution feasibility dominates theoretical EV for a $150 to $1,000 account.[cite:7]

For the stated account range, the recommended default is therefore **single TP + disaster stop + client-side ratchet**, not “pure chandelier only” and not “three-way scaling.” The small notional sizes make elegant partial scale-outs structurally fragile once the $10 minimum order floor is respected, especially after leverage and isolated-margin sizing compress per-trade quantity.[cite:7]

## Native triggers
Hyperliquid’s own documentation says TP/SL triggers use the mark price and that market TP/SL orders are supported explicitly for execution certainty, while trigger-limit orders can fail to execute if the limit price is not marketable once triggered.[cite:7] That makes the microstructure trade-off straightforward: a native stop-market is the right disaster-stop primitive, while a native trigger-limit is only appropriate when slippage control matters more than guaranteed exit.[cite:7]

A robust long-position stop-market payload in Rust follows the SDK shape below, with `is_buy: false` to close the long and `reduce_only: true` to prevent accidental reversal:[cite:10]

```rust
use hyperliquid_rust_sdk::exchange::order::{Order, OrderRequest, Trigger};

let stop = OrderRequest {
    asset: asset_index,
    is_buy: false,
    limit_px: "0.0".to_string(),
    sz: position_size.to_string(),
    reduce_only: true,
    order_type: Order::Trigger(Trigger {
        is_market: true,
        trigger_px: stop_trigger.to_string(),
        tpsl: "sl".to_string(),
    }),
    cloid: Some(client_order_id),
};
```

The exchange-side disaster stop should be placed once on entry and only amended when the ratchet moves materially, not every few seconds. A 1.5-second sentinel that rewrites the native stop only when the computed stop improves by a non-trivial increment, such as 10 to 20 bps or a meaningful fraction of ATR, stays well inside the 1,200-weight-per-minute IP cap while preserving failure protection during client outages.[cite:3][cite:10] The recommended architecture is therefore: native exchange-side stop-market for catastrophe protection, maker take-profit resting on-book when feasible, and client-side logic as the primary high-frequency ratchet controller.[cite:3][cite:7][cite:10]

## Recommended production rules
For a Hyperliquid L1 15-minute Donchian breakout engine on BTC, ETH, SOL, and AVAX, the most defensible near-term rules are these:[cite:3][cite:7][cite:62][cite:63][cite:64]

- Entry veto: reject any breakout if \(\text{DCW}_{20}\ge 4.5\) or if \(|Z_{60}|\) exceeds the asset-specific 90th-percentile threshold from the calibration table.[cite:63]
- Entry preference: prefer breakouts where both \(\text{DCW}_{20}\) and \(\text{BBWATR}_{20}\) are below their asset-specific 35th-percentile thresholds and \(|Z_{60}|\) is below the asset-specific 80th-percentile threshold.[cite:63]
- First monetization: post one reduce-only maker TP around 0.50x to 0.75x ATR20.[cite:61][cite:62][cite:64]
- Stop architecture: always maintain one native reduce-only stop-market as catastrophe protection; let the client daemon own the normal ratchet cadence.[cite:7][cite:10]
- Ratchet activation: do not start aggressive giveback logic until the trade has banked at least 0.75x ATR of unrealized gain.[cite:62]
- Trigger-limit use: reserve native trigger-limit stops for exceptional low-liquidity situations where stop-market impact is demonstrably worse than non-fill risk.[cite:7]

These recommendations fit the user’s live-production observation that the breakout edge is mostly a **short-horizon monetization** problem, not a trend-holding problem. The strongest immediate improvement is not a more elaborate HMM confirmation stack; it is a stricter residual-budget veto plus a more aggressive early maker TP policy that harvests the move before the typical 5 to 25 minute fade.[cite:62][cite:63][cite:64]
