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
    discord_url: Option<String>,
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

    // FIXED FRACTIONAL SIZING: Max risk is strictly $0.20 per trade, adapting to SL distance.
    let risk_amount_usdc = 0.20;

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
    }

    if notional_value < 6.0 {
        return Ok(json!({"status": "REJECTED", "reason": format!("Order size {:.2} USDC is too small.", notional_value)}));
    }

    // Binance LOT_SIZE strict precision rules
    let precision = match symbol {
        "BTC/USDC" | "BTCUSDC" => 5,
        "ETH/USDC" | "ETHUSDC" => 4,
        "SOL/USDC" | "SOLUSDC" => 2,
        _ => 3,
    };
    
    // Truncate cleanly instead of rounding up to avoid Insufficient Balance
    let multiplier = 10_f64.powi(precision);
    let amount_f = (amount_to_buy * multiplier).floor() / multiplier;
    
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
    
    // REFACTORED: Place a RESTING LIMIT ORDER for Take Profit (Maker execution)
    let tp_res = client.create_limit_sell(symbol, qty_to_sell, take_profit_price).await.map_err(|e| e.to_string())?;
    let tp_order_id = tp_res["orderId"].as_u64().unwrap_or(0).to_string();

    let order_id = buy_res["orderId"].as_u64().unwrap_or(0).to_string();

    let new_state = PositionState {
        symbol: symbol.to_string(),
        order_id,
        tp_order_id,
        entry_price: fill_price,
        amount: amount_f,
        stop_loss_price,
        take_profit_price,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state);

    let msg = format!("🚀 **NEW POSITION:** {} (Fixed Risk: {:.2} USDC)\n**Entry:** ${:.2}\n**TP Limit:** ${:.2}\n**SL Market:** ${:.2}\n**Size:** {:.5}", 
                      symbol, risk_amount_usdc, fill_price, take_profit_price, stop_loss_price, amount_f);
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({
        "status": "DISPATCHED",
        "symbol": symbol,
        "notional_usdc": notional_value,
        "amount": amount_f,
        "fill_price": fill_price,
        "tp_order_id": new_state.tp_order_id,
        "risk_taken_usdc": risk_amount_usdc
    }))
}
