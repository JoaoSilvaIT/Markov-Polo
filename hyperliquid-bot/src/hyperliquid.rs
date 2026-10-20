use ethers::types::H160;
use hyperliquid_rust_sdk::{
    ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ClientTrigger,
    ExchangeClient, ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};
use std::collections::HashMap;
use std::time::Duration;

const HL_REST_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetches the best bid and best ask from the L2 order book
pub async fn fetch_book_ticker(info: &InfoClient, coin: &str) -> Result<(f64, f64), String> {
    let l2 = tokio::time::timeout(HL_REST_TIMEOUT, info.l2_snapshot(coin.to_string()))
        .await
        .map_err(|_| format!("Timeout fetching L2 snapshot for {}", coin))?
        .map_err(|e| format!("Failed to fetch L2 snapshot for {}: {}", coin, e))?;

    if l2.levels.len() < 2 || l2.levels[0].is_empty() || l2.levels[1].is_empty() {
        return Err(format!("Insufficient depth in L2 book for {}", coin));
    }

    let best_bid = l2.levels[0][0]
        .px
        .parse::<f64>()
        .map_err(|e| format!("Invalid bid price {}: {}", l2.levels[0][0].px, e))?;
    let best_ask = l2.levels[1][0]
        .px
        .parse::<f64>()
        .map_err(|e| format!("Invalid ask price {}: {}", l2.levels[1][0].px, e))?;

    Ok((best_bid, best_ask))
}

/// Retrieves size decimals for perpetual assets (BTC: 5, ETH: 4, SOL: 2, etc.)
pub async fn fetch_sz_decimals_map(info: &InfoClient) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    if let Ok(meta) = info.meta().await {
        for asset in meta.universe {
            map.insert(asset.name, asset.sz_decimals);
        }
    } else {
        // Safe fallbacks for major assets
        map.insert("BTC".to_string(), 5);
        map.insert("ETH".to_string(), 4);
        map.insert("SOL".to_string(), 2);
        map.insert("AVAX".to_string(), 2);
    }
    map
}

pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

pub fn ceil_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor - 1e-12).ceil() / factor
}

/// Quantizes order size to `sz_decimals`, stepping up with `ceil` if rounding to nearest
/// would drop the order below Hyperliquid's $10.00 minimum notional requirement.
pub fn quantize_size_for_notional(target_notional_usdc: f64, price: f64, sz_decimals: u32) -> f64 {
    if price <= 0.0 || !price.is_finite() || target_notional_usdc <= 0.0 {
        return 0.0;
    }
    let raw_sz = target_notional_usdc / price;
    let rounded = round_to_decimals(raw_sz, sz_decimals);
    if rounded * price < 10.50 {
        ceil_to_decimals(target_notional_usdc.max(10.50) / price, sz_decimals)
    } else {
        rounded
    }
}

pub fn round_to_significant_and_decimal(value: f64, sig_figs: u32, max_decimals: u32) -> f64 {
    let abs_value = value.abs();
    if abs_value == 0.0 {
        return 0.0;
    }
    let magnitude = abs_value.log10().floor() as i32;
    let scale = 10f64.powi(sig_figs as i32 - magnitude - 1);
    let rounded = (abs_value * scale).round() / scale;
    round_to_decimals(rounded.copysign(value), max_decimals)
}

/// Formats and rounds price according to Hyperliquid rules (5 sig figs, max decimals 6 - sz_decimals)
pub fn quantize_price(price: f64, sz_decimals: u32) -> f64 {
    let price_decimals = 6u32.saturating_sub(sz_decimals);
    round_to_significant_and_decimal(price, 5, price_decimals)
}

/// Returns the effective tick size at a given price and `sz_decimals` on Hyperliquid.
pub fn effective_tick_size(price: f64, sz_decimals: u32) -> f64 {
    let abs_val = price.abs();
    if abs_val <= 0.0 || !abs_val.is_finite() {
        return 0.01;
    }
    let price_decimals = 6u32.saturating_sub(sz_decimals) as i32;
    let magnitude = abs_val.log10().floor() as i32;
    let sig_decimals = 5 - magnitude - 1;
    let eff_decimals = price_decimals.min(sig_decimals).max(0);
    10f64.powi(-eff_decimals)
}

/// Places a Post-Only (Alo) Limit Order
pub async fn place_post_only_order(
    exchange: &ExchangeClient,
    coin: &str,
    is_buy: bool,
    price: f64,
    size: f64,
    reduce_only: bool,
    cloid: Option<uuid::Uuid>,
) -> Result<u64, String> {
    // Hyperliquid rule: Reduce-only closing orders (like Take Profit) cannot use "Alo" (Post-Only);
    // they must use "Gtc" (Good-Til-Cancelled). Entries use "Alo" for maker rebates and anti-taker protection.
    let tif = if reduce_only {
        "Gtc".to_string()
    } else {
        "Alo".to_string()
    };

    let order_req = ClientOrderRequest {
        asset: coin.to_string(),
        is_buy,
        reduce_only,
        limit_px: price,
        sz: size,
        cloid,
        order_type: ClientOrder::Limit(ClientLimit {
            tif,
        }),
    };

    let res = tokio::time::timeout(HL_REST_TIMEOUT, exchange.order(order_req, None))
        .await
        .map_err(|_| format!("Order submission timed out after 10s for {}", coin))?
        .map_err(|e| format!("Order submission failed: {}", e))?;

    match res {
        ExchangeResponseStatus::Ok(resp) => {
            if let Some(data) = resp.data {
                if let Some(first) = data.statuses.first() {
                    match first {
                        ExchangeDataStatus::Resting(resting) => Ok(resting.oid),
                        ExchangeDataStatus::Filled(filled) => Ok(filled.oid),
                        ExchangeDataStatus::Error(msg) => {
                            Err(format!("Exchange rejected order: {}", msg))
                        }
                        other => Err(format!("Unexpected order status: {:?}", other)),
                    }
                } else {
                    Err("Empty order statuses returned".to_string())
                }
            } else {
                Err("Missing response data".to_string())
            }
        }
        ExchangeResponseStatus::Err(e) => Err(format!("Exchange error: {}", e)),
    }
}

/// Places an Exchange-Native Reduce-Only Disaster Stop-Market Trigger Order (`tpsl: "sl"`, `is_market: true`)
/// with a 5.0% protective worst-price slippage collar on `limit_px` so flash-gap triggers never fail with `iocCancelRejected`.
pub async fn place_disaster_stop_trigger_order(
    exchange: &ExchangeClient,
    coin: &str,
    is_buy_to_close: bool,
    trigger_px: f64,
    size: f64,
    sz_decimals: u32,
    cloid: Option<uuid::Uuid>,
) -> Result<u64, String> {
    let clean_trigger_px = quantize_price(trigger_px, sz_decimals);
    let raw_limit_px = if is_buy_to_close {
        clean_trigger_px * 1.05
    } else {
        clean_trigger_px * 0.95
    };
    let clean_limit_px = quantize_price(raw_limit_px, sz_decimals);

    let order_req = ClientOrderRequest {
        asset: coin.to_string(),
        is_buy: is_buy_to_close,
        reduce_only: true,
        limit_px: clean_limit_px,
        sz: size,
        cloid,
        order_type: ClientOrder::Trigger(ClientTrigger {
            is_market: true,
            trigger_px: clean_trigger_px,
            tpsl: "sl".to_string(),
        }),
    };

    let res = tokio::time::timeout(HL_REST_TIMEOUT, exchange.order(order_req, None))
        .await
        .map_err(|_| format!("Disaster stop trigger submission timed out after 10s for {}", coin))?
        .map_err(|e| format!("Disaster stop trigger submission failed: {}", e))?;

    match res {
        ExchangeResponseStatus::Ok(resp) => {
            if let Some(data) = resp.data {
                if let Some(first) = data.statuses.first() {
                    match first {
                        ExchangeDataStatus::Resting(resting) => Ok(resting.oid),
                        ExchangeDataStatus::Filled(filled) => Ok(filled.oid),
                        ExchangeDataStatus::Error(msg) => {
                            Err(format!("Exchange rejected disaster stop trigger: {}", msg))
                        }
                        other => Err(format!("Unexpected trigger order status: {:?}", other)),
                    }
                } else {
                    Err("Empty trigger order statuses returned".to_string())
                }
            } else {
                Err("Missing trigger response data".to_string())
            }
        }
        ExchangeResponseStatus::Err(e) => Err(format!("Exchange trigger error: {}", e)),
    }
}

/// Asset-specific optimal resting Maker Take-Profit ATR multiple (`k_tp_atr`) for Engine B 15m breakouts,
/// derived from the 2024–2026 Hyperliquid L1 empirical MFE hazard-rate optimization (`Hyperliquid Intraday Breakout Research.md`).
pub fn optimal_engine_b_tp_atr_multiple(symbol: &str) -> f64 {
    match symbol.to_uppercase().as_str() {
        "BTC" => 1.05,
        "ETH" => 1.15,
        "SOL" => 1.20,
        "AVAX" => 1.25,
        _ => 1.15,
    }
}

/// Cancels an active order by oid
pub async fn cancel_order(exchange: &ExchangeClient, coin: &str, oid: u64) -> Result<(), String> {
    let req = ClientCancelRequest {
        asset: coin.to_string(),
        oid,
    };
    let res = tokio::time::timeout(HL_REST_TIMEOUT, exchange.cancel(req, None))
        .await
        .map_err(|_| format!("Cancel timed out after 10s for {} oid {}", coin, oid))?
        .map_err(|e| format!("Cancel failed: {}", e))?;

    match res {
        ExchangeResponseStatus::Ok(_) => Ok(()),
        ExchangeResponseStatus::Err(e) => Err(format!("Cancel exchange error: {}", e)),
    }
}

/// Closes open position at market with slippage protection
pub async fn close_market_position(
    exchange: &ExchangeClient,
    info: &InfoClient,
    coin: &str,
    szi: f64,
    slippage: f64,
    sz_decimals: u32,
) -> Result<(), String> {
    let eps = 1e-5;
    if szi.abs() <= eps {
        return Ok(());
    }

    let is_buy = szi < 0.0; // If szi < 0 (Short), buy to close. If szi > 0 (Long), sell to close.
    let sz = round_to_decimals(szi.abs(), sz_decimals);
    if sz <= 0.0 {
        return Ok(());
    }

    let (top_bid, top_ask) = fetch_book_ticker(info, coin).await?;
    let raw_px = if is_buy {
        top_ask * (1.0 + slippage)
    } else {
        top_bid * (1.0 - slippage)
    };
    let limit_px = quantize_price(raw_px, sz_decimals);

    let order_req = ClientOrderRequest {
        asset: coin.to_string(),
        is_buy,
        reduce_only: true,
        limit_px,
        sz,
        cloid: Some(uuid::Uuid::new_v4()),
        order_type: ClientOrder::Limit(ClientLimit {
            tif: "Ioc".to_string(),
        }),
    };

    let res = tokio::time::timeout(HL_REST_TIMEOUT, exchange.order(order_req, None))
        .await
        .map_err(|_| format!("Market close submission timed out after 10s for {}", coin))?
        .map_err(|e| format!("Market close submission failed: {}", e))?;

    match res {
        ExchangeResponseStatus::Ok(resp) => {
            if let Some(data) = resp.data {
                if let Some(first) = data.statuses.first() {
                    match first {
                        ExchangeDataStatus::Filled(_) => Ok(()),
                        ExchangeDataStatus::Resting(_) => {
                            Err("Market close order is resting instead of filling (IOC violation)".to_string())
                        }
                        ExchangeDataStatus::Error(msg) => {
                            Err(format!("Exchange rejected market close order: {}", msg))
                        }
                        other => Err(format!("Unexpected order status: {:?}", other)),
                    }
                } else {
                    Ok(())
                }
            } else {
                Ok(())
            }
        }
        ExchangeResponseStatus::Err(e) => Err(format!("Market close exchange error: {}", e)),
    }
}

/// Queries user's current open position size (szi) and entry price for a coin
pub async fn get_user_position(
    info: &InfoClient,
    address: H160,
    coin: &str,
) -> Result<(f64, Option<f64>), String> {
    let state = tokio::time::timeout(HL_REST_TIMEOUT, info.user_state(address))
        .await
        .map_err(|_| format!("Timed out querying user_state for {}", coin))?
        .map_err(|e| format!("Failed to get user state: {}", e))?;

    for p in state.asset_positions {
        if p.position.coin == coin {
            let szi = p.position.szi.parse::<f64>().unwrap_or(0.0);
            let entry_px = p.position.entry_px.and_then(|px| px.parse::<f64>().ok());
            return Ok((szi, entry_px));
        }
    }
    Ok((0.0, None))
}

/// Queries user's total account equity value in USDC across clearinghouse perps and spot balances
pub async fn get_account_value(
    info: &InfoClient,
    address: H160,
) -> Result<f64, String> {
    // 1. Clearinghouse perp account value is mandatory (fail-closed if perp state RPC fails)
    let state = tokio::time::timeout(HL_REST_TIMEOUT, info.user_state(address))
        .await
        .map_err(|_| "Timed out querying user_state for account_value".to_string())?
        .map_err(|e| format!("Failed to query user_state: {}", e))?;
    let perp_val = state
        .margin_summary
        .account_value
        .parse::<f64>()
        .map_err(|e| format!("Invalid margin_summary.account_value: {}", e))?;

    // 2. Query unencumbered spot token balances (total - hold) for Unified Account setups
    let mut free_spot_usdc = 0.0;
    if let Ok(Ok(token_balances)) =
        tokio::time::timeout(HL_REST_TIMEOUT, info.user_token_balances(address)).await
    {
        for b in token_balances.balances {
            if b.coin == "USDC" {
                let total = b.total.parse::<f64>().unwrap_or(0.0);
                let hold = b.hold.parse::<f64>().unwrap_or(0.0);
                free_spot_usdc += (total - hold).max(0.0);
            }
        }
    }

    Ok((perp_val + free_spot_usdc).max(0.0))
}

/// Calculates dynamic order notional based on account equity, equity fraction, and safety boundaries.
/// Guarantees:
/// 1. Clamped to at least min_notional (Hyperliquid rule: >= 11.0 USDC)
/// 2. Clamped to at most max_notional (safety cap: default 500.0 USDC)
/// 3. Falls back to fallback_notional if equity is invalid or non-positive.
pub fn calculate_dynamic_notional(
    account_value: f64,
    equity_fraction: f64,
    min_notional: f64,
    max_notional: f64,
    fallback_notional: f64,
) -> f64 {
    if account_value.is_finite() && account_value > 0.0 && equity_fraction.is_finite() && equity_fraction > 0.0 {
        let notional = account_value * equity_fraction;
        notional.max(min_notional).min(max_notional)
    } else {
        fallback_notional.max(min_notional).min(max_notional)
    }
}

/// Computes volatility-normalized Fractional-Kelly position notional (in USDC)
/// scaled by the trade's exact stop-loss distance and bounded by per-slot leverage limits.
/// Scales seamlessly from $150 micro-account testing to $1,000+ production accounts.
pub fn calculate_risk_scaled_notional(
    account_value: f64,
    base_trade_bank: f64,
    risk_fraction_of_equity: f64,
    entry_price: f64,
    stop_price: f64,
    leverage: u32,
    max_concurrent_positions: usize,
    min_notional: f64,
    max_notional: f64,
) -> f64 {
    if !account_value.is_finite()
        || account_value <= 0.0
        || !entry_price.is_finite()
        || entry_price <= 0.0
        || !stop_price.is_finite()
        || stop_price <= 0.0
    {
        return base_trade_bank.max(min_notional).min(max_notional);
    }

    let stop_dist_pct = ((entry_price - stop_price).abs() / entry_price).max(0.0030);
    // Stress friction: 1.0 bps maker entry + 3.5 bps taker stop exit + 4.0 bps IOC stop slippage = 8.5 bps
    let stress_friction = 0.00085;
    let total_loss_per_unit_notional = stop_dist_pct + stress_friction;

    let risk_budget_usdc = account_value * risk_fraction_of_equity.clamp(0.001, 0.025);
    let risk_targeted_notional = risk_budget_usdc / total_loss_per_unit_notional;

    // Half-Kelly / per-slot margin ceiling: allow up to 60% of max exchange leverage per concurrent slot
    let slots = max_concurrent_positions.max(1) as f64;
    let slot_leverage_cap = (leverage.max(1) as f64 * 0.60) / slots;
    let max_slot_notional = (account_value * slot_leverage_cap).max(base_trade_bank);

    risk_targeted_notional
        .min(max_slot_notional)
        .max(min_notional)
        .min(max_notional)
}

/// Computes the continuous-time drawdown-constrained Kelly risk multiplier:
/// When drawdown from high-water mark exceeds 2.5%, applies an exponential taper exp(-10 * DD),
/// bounded to [0.40, 1.0] to preserve trading continuity while protecting capital.
pub fn compute_drawdown_risk_multiplier(current_equity: f64, high_water_mark: f64) -> f64 {
    if !current_equity.is_finite()
        || !high_water_mark.is_finite()
        || current_equity <= 0.0
        || high_water_mark <= 0.0
        || current_equity >= high_water_mark
    {
        return 1.0;
    }
    let dd = (high_water_mark - current_equity) / high_water_mark;
    if dd > 0.025 {
        (-10.0 * dd).exp().clamp(0.40, 1.0)
    } else {
        1.0
    }
}

/// Checks if Hyperliquid's $11.00 minimum notional floor causes excessive step-function risk inflation
/// on a micro-account when the stop-loss distance is unusually wide.
pub fn exceeds_micro_account_step_risk(
    account_value: f64,
    notional_usdc: f64,
    entry_price: f64,
    stop_price: f64,
    max_allowed_equity_risk_frac: f64,
) -> bool {
    if account_value <= 0.0 || entry_price <= 0.0 || stop_price <= 0.0 || notional_usdc <= 0.0 {
        return false;
    }
    let stop_dist_pct = (entry_price - stop_price).abs() / entry_price;
    let total_loss_usdc = notional_usdc * (stop_dist_pct + 0.00085);
    (total_loss_usdc / account_value) > max_allowed_equity_risk_frac
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_calculate_dynamic_notional_quarter_balance() {
        // 1/4 of $100 = $25.0
        let notional = calculate_dynamic_notional(100.0, 0.25, 11.0, 500.0, 25.0);
        assert!((notional - 25.0).abs() < 1e-4);
    }

    #[test]
    fn test_calculate_dynamic_notional_third_balance() {
        // 1/3 of $100 = $33.333...
        let notional = calculate_dynamic_notional(100.0, 1.0 / 3.0, 11.0, 500.0, 25.0);
        assert!((notional - 33.333333).abs() < 1e-4);
    }

    #[test]
    fn test_calculate_dynamic_notional_min_clamp() {
        // 1/4 of $30 = $7.5 -> clamped to min 11.0
        let notional = calculate_dynamic_notional(30.0, 0.25, 11.0, 500.0, 25.0);
        assert!((notional - 11.0).abs() < 1e-4);
    }

    #[test]
    fn test_calculate_dynamic_notional_max_cap() {
        // 1/4 of $4000 = $1000 -> clamped to max 500.0
        let notional = calculate_dynamic_notional(4000.0, 0.25, 11.0, 500.0, 25.0);
        assert!((notional - 500.0).abs() < 1e-4);
    }

    #[test]
    fn test_calculate_dynamic_notional_fallback_on_invalid() {
        // 0.0 or negative or NaN account value falls back safely
        let notional_zero = calculate_dynamic_notional(0.0, 0.25, 11.0, 500.0, 25.0);
        assert!((notional_zero - 25.0).abs() < 1e-4);

        let notional_neg = calculate_dynamic_notional(-50.0, 0.25, 11.0, 500.0, 25.0);
        assert!((notional_neg - 25.0).abs() < 1e-4);

        let notional_nan = calculate_dynamic_notional(f64::NAN, 0.25, 11.0, 500.0, 25.0);
        assert!((notional_nan - 25.0).abs() < 1e-4);
    }

    #[test]
    fn test_quantize_size_for_notional_prevents_sub_10_dollar_rounding() {
        // SOL at $185.00 with $11.00 target notional:
        // 11.0 / 185.0 = 0.059459 -> round_to_decimals gives 0.05 ($9.25 < $10.00 min notional!)
        // quantize_size_for_notional must step up to 0.06 ($11.10 >= $10.50).
        let sz_sol = quantize_size_for_notional(11.0, 185.0, 2);
        assert_eq!(sz_sol, 0.06);
        assert!(sz_sol * 185.0 >= 10.50);

        // AVAX at $10.47 with $25.00 target notional (sz_decimals = 2):
        let sz_avax = quantize_size_for_notional(25.0, 10.47, 2);
        assert_eq!(sz_avax, 2.39);
        assert!(sz_avax * 10.47 >= 10.50);
    }

    #[test]
    fn test_effective_tick_size_across_all_4_coins() {
        assert!((effective_tick_size(90_000.0, 5) - 1.0).abs() < 1e-9);   // BTC: $1.0
        assert!((effective_tick_size(2_500.0, 4) - 0.1).abs() < 1e-9);    // ETH: $0.10
        assert!((effective_tick_size(185.0, 2) - 0.01).abs() < 1e-9);     // SOL: $0.01
        assert!((effective_tick_size(10.47, 2) - 0.001).abs() < 1e-9);    // AVAX: $0.001
    }

    #[test]
    fn test_risk_scaled_notional_150_and_1000_accounts() {
        // 1. $150 account, 1.5% stop distance, 0.75% equity risk ($1.125 risk budget):
        // Risk notional = 1.125 / (0.015 + 0.00085) = $70.98 (within 0.90x slot cap $135.0)
        let n_150_calm = calculate_risk_scaled_notional(150.0, 37.5, 0.0075, 100.0, 98.5, 3, 2, 11.0, 500.0);
        assert!((n_150_calm - 70.9779).abs() < 0.1);

        // 2. $150 account, wide 4.0% ATR stop distance, 0.60% Engine B risk ($0.90 risk budget):
        // Risk notional = 0.90 / (0.040 + 0.00085) = $22.03 (automatically scales down below $37.50 base!)
        let n_150_volatile = calculate_risk_scaled_notional(150.0, 37.5, 0.0060, 100.0, 96.0, 3, 2, 11.0, 500.0);
        assert!((n_150_volatile - 22.0318).abs() < 0.1);

        // 3. $1,000 account, 1.5% stop distance, 0.75% risk ($7.50 risk budget):
        // Risk notional = 7.50 / 0.01585 = $473.19 (scales smoothly toward $500 cap)
        let n_1000_calm = calculate_risk_scaled_notional(1000.0, 250.0, 0.0075, 100.0, 98.5, 3, 2, 11.0, 500.0);
        assert!((n_1000_calm - 473.186).abs() < 0.1);

        // 4. Drawdown risk taper & micro-account step-function risk check:
        assert_eq!(compute_drawdown_risk_multiplier(150.0, 150.0), 1.0);
        let mult_5pct_dd = compute_drawdown_risk_multiplier(142.5, 150.0); // 5% drawdown
        assert!(mult_5pct_dd < 0.65 && mult_5pct_dd > 0.55);

        // On $150 account, $11 floor with a massive 25% stop loss = $2.75 loss (1.83% > 1.75% max cap) -> vetoed!
        assert!(exceeds_micro_account_step_risk(150.0, 11.0, 100.0, 75.0, 0.0175));
        // Normal 2.0% stop loss on $11 floor = $0.23 loss (0.15% <= 1.75% max cap) -> allowed!
        assert!(!exceeds_micro_account_step_risk(150.0, 11.0, 100.0, 98.0, 0.0175));
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_account_value_query() {
        use std::str::FromStr;
        let info = InfoClient::new(None, Some(hyperliquid_rust_sdk::BaseUrl::Mainnet)).await.unwrap();
        let addr_str = std::env::var("HL_WALLET_ADDRESS")
            .unwrap_or_else(|_| "0x0000000000000000000000000000000000000000".to_string());
        let addr = H160::from_str(&addr_str).unwrap();
        let val = get_account_value(&info, addr).await.unwrap();
        println!("LIVE VAL: {}", val);
        assert!(val >= 0.0);
    }
}

