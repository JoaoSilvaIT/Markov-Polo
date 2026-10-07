use crate::binance::{BinanceClient, get_symbol_lot_precision, quantize_lot_size};
use crate::state::{PositionState, StateMap, save_state};
use chrono::Utc;
use serde_json::{json, Value};

pub async fn place_ou_limit_buy(
    state_file_path: &str,
    client: &BinanceClient,
    state: &mut StateMap,
    symbol: &str,
    entry_price: f64,
    target_mu: f64,
    half_life_minutes: f64,
    discord_url: Option<String>,
) -> Result<Value, String> {
    
    // Default $25.0 position size under Portfolio Cluster rule (single active position across portfolio)
    let position_size_usdc = std::env::var("POSITION_SIZE_USDC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(25.0);

    if state.len() >= 3 {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({"status": "REJECTED", "reason": format!("Already hold or have open order for {}.", symbol)}));
    }

    let precision = get_symbol_lot_precision(symbol);
    let amount_to_buy = position_size_usdc / entry_price;
    let amount_f = quantize_lot_size(amount_to_buy, precision);
    
    // We place a LIMIT_MAKER order to strictly capture the Maker rebate and avoid taker fees.
    let buy_res = client.create_limit_buy(symbol, amount_f, entry_price).await.map_err(|e| e.to_string())?;
    
    let order_id = buy_res.get("orderId").and_then(|id| {
        if id.is_u64() {
            Some(id.as_u64().unwrap().to_string())
        } else if id.is_string() {
            Some(id.as_str().unwrap().to_string())
        } else {
            None
        }
    }).ok_or_else(|| format!("Exchange response missing orderId: {:?}", buy_res))?;

    let new_state = PositionState {
        symbol: symbol.to_string(),
        order_id,
        tp_order_id: "".to_string(), // Populated when Limit Buy fills and Limit Sell is placed
        entry_price,
        amount: amount_f,
        stop_loss_price: 0.0, // No structural stop loss in O-U
        take_profit_price: target_mu,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: position_size_usdc,
        is_filled: false,
        half_life_minutes,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    let max_hold = (2.0 * half_life_minutes).clamp(15.0, 360.0);
    let msg = format!("🟢 **O-U ENTRY SIGNAL (MAKER):** {}\n**Limit Maker Buy:** ${:.2}\n**Target Mean (μ):** ${:.2}\n**Size:** {:.5} (${:.2})\n**Half-Life:** {:.1}m (Max Hold: {:.1}m)", 
                      symbol, entry_price, target_mu, amount_f, position_size_usdc, half_life_minutes, max_hold);
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": new_state.order_id}))
}

pub async fn emergency_flatten(
    state_file_path: &str,
    client: &BinanceClient,
    state: &mut StateMap,
    symbol: &str,
    discord_url: Option<String>,
) -> Result<(), String> {
    let pos = match state.get(symbol).cloned() {
        Some(p) => p,
        None => return Ok(()),
    };

    println!("🚨 [EMERGENCY FLATTEN] Regime shift detected for {}. Evaluating open order & position state...", symbol);

    // Case 1: Check if the entry order was resting and never filled
    let is_position_filled = pos.is_filled || !pos.tp_order_id.is_empty();

    if !is_position_filled {
        // Double check exchange status of the entry limit buy order
        let order_info = client.fetch_order(symbol, &pos.order_id).await.ok();
        let status = order_info.as_ref().and_then(|info| info.get("status")).and_then(|s| s.as_str()).unwrap_or("");
        let executed_qty = order_info.as_ref().and_then(|info| info.get("executedQty")).and_then(|q| q.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);

        if status == "NEW" || (status == "CANCELED" && executed_qty == 0.0) {
            // Entry order never filled! Cancel resting maker buy order without liquidating wallet inventory.
            if status == "NEW" {
                let _ = client.cancel_order(symbol, &pos.order_id).await;
            }
            println!("🚨 [EMERGENCY FLATTEN] Resting entry buy order {} for {} cancelled cleanly before execution. Zero inventory acquired.", pos.order_id, symbol);
            state.remove(symbol);
            let _ = save_state(state_file_path, state);
            let msg = format!("🚨 **ENTRY CANCELLED:** Regime shift detected for {}. Resting entry order was cancelled before fill. Zero inventory acquired.", symbol);
            crate::discord::send_alert(discord_url.as_deref(), &msg).await;
            return Ok(());
        }
    }

    // Case 2: The position was filled (or partially executed). We must flatten risk.
    let msg = format!("🚨 **HMM REGIME SHIFT DETECTED!** Stationarity broken for {}. Cancelling resting orders and executing emergency Taker Market Sell to flatten risk.", symbol);
    println!("🚨 [EMERGENCY FLATTEN] Executing Market Sell for filled position {}.", symbol);
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    // 1. Cancel any active resting orders on book
    if !pos.order_id.is_empty() {
        let _ = client.cancel_order(symbol, &pos.order_id).await;
    }
    if !pos.tp_order_id.is_empty() {
        let _ = client.cancel_order(symbol, &pos.tp_order_id).await;
    }

    // 2. Fetch actual free base asset balance in the account
    let base_asset = symbol.replace("USDC", "").replace("/", "");
    let actual_balance = client.fetch_free_balance(&base_asset).await.unwrap_or(0.0);

    // 3. Determine sellable quantity (capped at position amount to never dump outside inventory)
    let precision = get_symbol_lot_precision(symbol);
    let sell_target = f64::min(pos.amount, actual_balance);
    let amount_f = quantize_lot_size(sell_target, precision);

    if amount_f > 0.0 {
        match client.create_market_sell(symbol, amount_f).await {
            Ok(res) => {
                println!("🚨 [EMERGENCY FLATTEN] Market sell successfully executed for {}: {:?}", symbol, res);
                state.remove(symbol);
                let _ = save_state(state_file_path, state);
                let ok_msg = format!("✅ **EMERGENCY FLATTEN EXECUTED:** Successfully liquidated {:.5} {} at market.", amount_f, base_asset);
                crate::discord::send_alert(discord_url.as_deref(), &ok_msg).await;
            }
            Err(e) => {
                eprintln!("❌ [EMERGENCY FLATTEN FAILED] Market sell failed for {}: {}", symbol, e);
                let err_msg = format!("❌ **EMERGENCY FLATTEN FAILED:** Failed to market sell {} {}: {}. State preserved for retry.", amount_f, symbol, e);
                crate::discord::send_alert(discord_url.as_deref(), &err_msg).await;
                return Err(e.to_string());
            }
        }
    } else {
        // Nothing to sell (balance was 0 or below stepSize)
        println!("⚠️ [EMERGENCY FLATTEN] No sellable balance found for {} (free: {:.5}). Clearing state.", base_asset, actual_balance);
        state.remove(symbol);
        let _ = save_state(state_file_path, state);
    }

    Ok(())
}
