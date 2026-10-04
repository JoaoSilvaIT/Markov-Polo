use crate::binance::BinanceClient;
use crate::state::{PositionState, StateMap, save_state};
use chrono::Utc;
use serde_json::{json, Value};

pub async fn place_order(
    client: &BinanceClient,
    state: &mut StateMap,
    symbol: &str,
    take_profit_price: f64,
    stop_loss_price: f64,
    risk_pct: f64,
) -> Result<Value, String> {
    
    // 1. Fetch balance
    let balance = client.fetch_balance().await.map_err(|e| e.to_string())?;
    
    let mut usdc_free = 0.0;
    let mut usdc_total = 0.0;
    if let Some(balances) = balance["balances"].as_array() {
        for b in balances {
            if b["asset"].as_str() == Some("USDC") {
                usdc_free = b["free"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                let locked: f64 = b["locked"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                usdc_total = usdc_free + locked;
                break;
            }
        }
    }

    if usdc_total < 10.0 {
        return Ok(json!({"status": "REJECTED", "reason": format!("Insufficient account balance: {} USDC", usdc_total)}));
    }

    let mut risk_amount_usdc = usdc_total * risk_pct;

    if state.len() >= 3 {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades (3) reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({"status": "REJECTED", "reason": format!("Already holding {}.", symbol)}));
    }

    // Position sizing
    let current_price = client.fetch_ticker(symbol).await.map_err(|e| e.to_string())?;
    let distance_to_sl = current_price - stop_loss_price;
    if distance_to_sl <= 0.0 {
        return Ok(json!({"status": "REJECTED", "reason": "Stop loss must be below current price."}));
    }

    let mut amount_to_buy = risk_amount_usdc / distance_to_sl;
    let mut notional_value = amount_to_buy * current_price;

    let max_notional = usdc_free * 0.98;
    if notional_value > max_notional {
        notional_value = max_notional;
        amount_to_buy = notional_value / current_price;
        risk_amount_usdc = amount_to_buy * distance_to_sl;
    }

    if notional_value < 6.0 {
        return Ok(json!({"status": "REJECTED", "reason": format!("Order size {:.2} USDC is too small.", notional_value)}));
    }

    // Format amount (5 decimals generally safe for spot on Binance for small BTC/ETH)
    let amount_str = format!("{:.5}", amount_to_buy);
    let amount_f = amount_str.parse::<f64>().unwrap();
    
    // Execute Market Buy
    let buy_res = client.create_market_buy(symbol, amount_f).await.map_err(|e| e.to_string())?;
    
    let fill_price = if let Some(fills) = buy_res["fills"].as_array() {
        if fills.len() > 0 {
            fills[0]["price"].as_str().unwrap_or("0").parse().unwrap_or(current_price)
        } else {
            current_price
        }
    } else {
        current_price
    };
    
    // Wait for Binance to update balance
    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    
    let base_asset = symbol.split("/").next().unwrap_or(symbol);
    let new_balance = client.fetch_balance().await.map_err(|e| e.to_string())?;
    let mut actual_balance = 0.0;
    if let Some(balances) = new_balance["balances"].as_array() {
        for b in balances {
            if b["asset"].as_str() == Some(base_asset) {
                actual_balance = b["free"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                break;
            }
        }
    }

    let qty_to_sell = amount_f.min(actual_balance);
    let qty_to_sell_str = format!("{:.5}", qty_to_sell);
    // Refactored: We DO NOT place an OCO limit order here. 
    // The Sentinel loop will actively monitor the price and fire a Market Sell
    // to guarantee execution during flash crashes and bypass PRER restrictions.

    let order_id = buy_res["orderId"].as_u64().unwrap_or(0).to_string();

    let new_state = PositionState {
        symbol: symbol.to_string(),
        order_id,
        oco_list_id: "".to_string(), // OCO deprecated
        entry_price: fill_price,
        amount: amount_f,
        stop_loss_price,
        take_profit_price,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state);

    Ok(json!({
        "status": "DISPATCHED",
        "symbol": symbol,
        "notional_usdc": notional_value,
        "amount": amount_f,
        "fill_price": fill_price,
        "risk_taken_usdc": risk_amount_usdc
    }))
}
