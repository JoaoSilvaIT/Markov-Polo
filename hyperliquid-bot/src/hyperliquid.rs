use ethers::types::H160;
use hyperliquid_rust_sdk::{
    ClientCancelRequest, ClientLimit, ClientOrder, ClientOrderRequest, ExchangeClient,
    ExchangeDataStatus, ExchangeResponseStatus, InfoClient, MarketCloseParams,
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
        cloid: None,
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
    coin: &str,
    slippage: f64,
) -> Result<(), String> {
    let params = MarketCloseParams {
        asset: coin,
        sz: None,
        px: None,
        slippage: Some(slippage),
        cloid: None,
        wallet: None,
    };
    let res = exchange
        .market_close(params)
        .await
        .map_err(|e| format!("Market close failed: {}", e))?;

    match res {
        ExchangeResponseStatus::Ok(_) => Ok(()),
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
