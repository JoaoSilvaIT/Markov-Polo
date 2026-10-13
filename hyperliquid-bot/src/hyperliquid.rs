use ethers::types::H160;
use hyperliquid_rust_sdk::{
    ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient,
};
use std::collections::HashMap;

/// Fetches the best bid and best ask from the L2 order book
pub async fn fetch_book_ticker(info: &InfoClient, coin: &str) -> Result<(f64, f64), String> {
    let l2 = info
        .l2_snapshot(coin.to_string())
        .await
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
    }
    map
}

pub fn round_to_decimals(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
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

    let res = exchange
        .order(order_req, None)
        .await
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

/// Cancels an active order by oid
pub async fn cancel_order(exchange: &ExchangeClient, coin: &str, oid: u64) -> Result<(), String> {
    let req = ClientCancelRequest {
        asset: coin.to_string(),
        oid,
    };
    let res = exchange
        .cancel(req, None)
        .await
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
) -> Result<(), String> {
    let eps = 1e-5;
    if szi.abs() <= eps {
        return Ok(());
    }

    let sz_decimals_map = fetch_sz_decimals_map(info).await;
    let sz_decimals = sz_decimals_map.get(coin).copied().unwrap_or(2);
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

    let res = exchange
        .order(order_req, None)
        .await
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
    let state = info
        .user_state(address)
        .await
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
    let mut total_val = 0.0;
    let mut query_success = false;

    // 1. Query clearinghouse perp account value
    if let Ok(state) = info.user_state(address).await {
        if let Ok(perp_val) = state.margin_summary.account_value.parse::<f64>() {
            total_val += perp_val;
            query_success = true;
        }
    }

    // 2. Query spot token balances (e.g. USDC deposited in spot clearinghouse)
    if let Ok(token_balances) = info.user_token_balances(address).await {
        for b in token_balances.balances {
            if b.coin == "USDC" {
                if let Ok(spot_val) = b.total.parse::<f64>() {
                    total_val += spot_val;
                    query_success = true;
                }
            }
        }
    }

    if query_success && total_val > 0.0 {
        Ok(total_val)
    } else if query_success {
        Ok(0.0)
    } else {
        Err("Failed to query user clearinghouse or token balance state".to_string())
    }
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

    #[tokio::test]
    #[ignore]
    async fn test_live_account_value_query() {
        use std::str::FromStr;
        let info = InfoClient::new(None, Some(hyperliquid_rust_sdk::BaseUrl::Mainnet)).await.unwrap();
        let addr = H160::from_str("0x0000000000000000000000000000000000000000").unwrap();
        let val = get_account_value(&info, addr).await.unwrap();
        println!("LIVE VAL: {}", val);
        assert!(val > 150.0);
    }
}

