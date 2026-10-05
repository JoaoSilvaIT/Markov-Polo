use crate::binance::BinanceClient;
use crate::state::{PositionState, StateMap, save_state};
use chrono::Utc;
use serde_json::{json, Value};

pub async fn place_ou_limit_buy(
    client: &BinanceClient,
    state: &mut StateMap,
    symbol: &str,
    entry_price: f64,
    target_mu: f64,
    discord_url: Option<String>,
) -> Result<Value, String> {
    
    let position_size_usdc = 12.0; // Fixed $12 position size to safely clear Binance $5 MIN_NOTIONAL

    if state.len() >= 3 {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({"status": "REJECTED", "reason": format!("Already hold or have open order for {}.", symbol)}));
    }

    let precision = match symbol {
        "BTC/USDC" | "BTCUSDC" => 5,
        "ETH/USDC" | "ETHUSDC" => 4,
        "SOL/USDC" | "SOLUSDC" => 2,
        _ => 3,
    };
    
    let amount_to_buy = position_size_usdc / entry_price;
    let multiplier = 10_f64.powi(precision);
    let amount_f = (amount_to_buy * multiplier).floor() / multiplier;
    
    // We place a Limit Buy to capture the Maker fee.
    // If the price is already below entry_price, this might execute as a Taker.
    let buy_res = client.create_limit_buy(symbol, amount_f, entry_price).await.map_err(|e| e.to_string())?;
    
    let order_id = buy_res["orderId"].as_u64().unwrap_or(0).to_string();

    let new_state = PositionState {
        symbol: symbol.to_string(),
        order_id,
        tp_order_id: "".to_string(), // Will be populated when Limit Buy fills and Limit Sell is placed
        entry_price,
        amount: amount_f,
        stop_loss_price: 0.0, // No structural stop loss in O-U
        take_profit_price: target_mu,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: position_size_usdc,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state);

    let msg = format!("🟢 **O-U ENTRY SIGNAL:** {}\n**Maker Limit Buy:** ${:.2}\n**Target Mean (μ):** ${:.2}\n**Size:** {:.5} (${:.2})", 
                      symbol, entry_price, target_mu, amount_f, position_size_usdc);
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": new_state.order_id}))
}

pub async fn emergency_flatten(
    client: &BinanceClient,
    state: &mut StateMap,
    symbol: &str,
    discord_url: Option<String>,
) -> Result<(), String> {
    
    if let Some(pos) = state.get(symbol) {
        let msg = format!("🚨 **HMM REGIME SHIFT DETECTED!** Stationarity broken for {}. Cancelling resting orders and executing emergency Taker Market Sell to flatten risk.", symbol);
        crate::discord::send_alert(discord_url.as_deref(), &msg).await;
        
        // 1. Cancel the entry Limit Buy if it hasn't filled, or the exit Limit Sell if it has.
        if !pos.order_id.is_empty() {
            let _ = client.cancel_order(symbol, &pos.order_id).await;
        }
        if !pos.tp_order_id.is_empty() {
            let _ = client.cancel_order(symbol, &pos.tp_order_id).await;
        }
        
        // 2. Fetch actual balance of the asset to ensure we sell exactly what we hold
        let base_asset = symbol.replace("USDC", "").replace("/", "");
        let balance = client.fetch_balance().await.map_err(|e| e.to_string())?;
        
        let mut actual_balance = 0.0;
        if let Some(balances) = balance["balances"].as_array() {
            for b in balances {
                if b["asset"].as_str() == Some(base_asset.as_str()) {
                    actual_balance = b["free"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                    break;
                }
            }
        }
        
        // 3. Market Sell all holdings to flatten
        if actual_balance > 0.0 {
            // Respect precision
            let precision = match symbol {
                "BTC/USDC" | "BTCUSDC" => 5,
                "ETH/USDC" | "ETHUSDC" => 4,
                "SOL/USDC" | "SOLUSDC" => 2,
                _ => 3,
            };
            let multiplier = 10_f64.powi(precision);
            let amount_f = (actual_balance * multiplier).floor() / multiplier;
            
            let _ = client.create_market_sell(symbol, amount_f).await;
        }
        
        state.remove(symbol);
        let _ = save_state(state);
    }
    
    Ok(())
}
