use crate::hyperliquid::{
    cancel_order, close_market_position, fetch_book_ticker, get_user_position, quantize_price,
    round_to_decimals, place_post_only_order,
};
use crate::state::{
    release_cluster_lease, save_state, try_acquire_cluster_lease, PositionState, Side, StateMap,
};
use chrono::Utc;
use ethers::types::H160;
use hyperliquid_rust_sdk::{ExchangeClient, InfoClient};
use serde_json::{json, Value};

pub async fn place_ou_maker_entry(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    state: &mut StateMap,
    symbol: &str,
    side: Side,
    entry_price: f64,
    target_mu: f64,
    half_life_minutes: f64,
    position_size_usdc: f64,
    sz_decimals: u32,
    discord_url: Option<String>,
) -> Result<Value, String> {
    if state.len() >= 3 {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({
            "status": "REJECTED",
            "reason": format!("Already hold or have open order for {}.", symbol)
        }));
    }

    // Atomically acquire the portfolio cluster lease (1 hour renewed continuously while holding)
    if !try_acquire_cluster_lease(symbol, 3600) {
        return Ok(json!({
            "status": "REJECTED",
            "reason": "Cluster lock currently acquired by another portfolio symbol."
        }));
    }

    // Fail-Closed: Query live order book ticker to join the top of book without crossing
    let (top_bid, top_ask) = match fetch_book_ticker(info, symbol).await {
        Ok(quotes) => quotes,
        Err(e) => {
            release_cluster_lease(symbol);
            return Err(format!("Failed to query live book ticker: {}", e));
        }
    };

    if top_bid <= 0.0 || top_ask <= 0.0 || top_bid >= top_ask {
        release_cluster_lease(symbol);
        return Err(format!(
            "Invalid or crossed book ticker quotes: bid={}, ask={}",
            top_bid, top_ask
        ));
    }

    let is_buy = side == Side::Long;
    let raw_post_only_price = if is_buy {
        f64::min(top_bid, entry_price)
    } else {
        f64::max(top_ask, entry_price)
    };

    let post_only_price = quantize_price(raw_post_only_price, sz_decimals);
    let target_mu_quantized = quantize_price(target_mu, sz_decimals);

    let amount_raw = position_size_usdc / post_only_price;
    let amount_f = round_to_decimals(amount_raw, sz_decimals);

    if amount_f <= 0.0 {
        release_cluster_lease(symbol);
        return Err(format!(
            "Calculated size {:.6} is zero or below minimum size decimals {}",
            amount_raw, sz_decimals
        ));
    }

    let client_order_id = format!("{}_{}", symbol, Utc::now().timestamp_millis());

    // Submit Post-Only (Alo) Maker order
    let oid = match place_post_only_order(
        exchange,
        symbol,
        is_buy,
        post_only_price,
        amount_f,
        false, // reduce_only = false for entry
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            release_cluster_lease(symbol);
            return Err(e);
        }
    };

    // Epsilon Shave: front-run the mean by 5 bps (min 2 ticks) to avoid Schelling point liquidity walls
    let epsilon_shave = (target_mu * 0.0005).max(0.02);
    let target_tp = if is_buy {
        target_mu - epsilon_shave
    } else {
        target_mu + epsilon_shave
    };
    let target_tp_quantized = quantize_price(target_tp, sz_decimals);

    let new_state = PositionState {
        symbol: symbol.to_string(),
        side: side.to_string(),
        order_id: oid.to_string(),
        tp_order_id: "".to_string(), // Populated when entry fills and TP is placed
        entry_price: post_only_price,
        amount: amount_f,
        stop_loss_price: 0.0, // No structural stop loss in O-U mean reversion
        take_profit_price: target_tp_quantized,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: position_size_usdc,
        is_filled: false,
        half_life_minutes,
        client_order_id,
        max_progress: 0.0,
        max_favorable_price: post_only_price,
        effective_tp: target_tp_quantized,
        last_reprice_time: Utc::now().timestamp_millis(),
        reprice_count: 0,
        candidate_lower_count: 0,
        engine_type: "EngineA".to_string(),
        chandelier_stop: 0.0,
        current_atr: 0.0,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    let max_hold = (2.0 * half_life_minutes).clamp(15.0, 360.0);
    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };

    let msg = format!(
        "{} **O-U {} ENTRY SIGNAL (MAKER):** {}\n**Limit Maker {}:** ${:.2} (Best Bid: ${:.2}, Ask: ${:.2})\n**Target Mean (μ):** ${:.2}\n**Size:** {:.5} (${:.2})\n**Half-Life:** {:.1}m (Max Hold: {:.1}m)",
        side_emoji, side_label, symbol, side_label, post_only_price, top_bid, top_ask, target_mu_quantized, amount_f, position_size_usdc, half_life_minutes, max_hold
    );
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": oid, "side": side.to_string()}))
}

pub async fn emergency_flatten(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    wallet_address: H160,
    state: &mut StateMap,
    symbol: &str,
    discord_url: Option<String>,
) -> Result<(), String> {
    let pos = match state.get(symbol).cloned() {
        Some(p) => p,
        None => return Ok(()),
    };

    println!(
        "🚨 [EMERGENCY FLATTEN] Invalidation triggered for {}. Checking order & position status...",
        symbol
    );

    // 1. Cancel resting entry order if it was active
    if !pos.order_id.is_empty() {
        if let Ok(oid) = pos.order_id.parse::<u64>() {
            let _ = cancel_order(exchange, symbol, oid).await;
        }
    }

    // 2. Cancel resting TP order if it was active
    if !pos.tp_order_id.is_empty() {
        if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
            let _ = cancel_order(exchange, symbol, tp_oid).await;
        }
    }

    // 3. Query actual live position on Hyperliquid
    let (szi, _) = get_user_position(info, wallet_address, symbol).await?;

    if szi.abs() > 0.0 {
        println!(
            "🚨 [EMERGENCY FLATTEN] Active open position detected for {} (szi: {:.5}). Executing Market Close...",
            symbol, szi
        );
        let msg = format!(
            "🚨 **REGIME INVALIDATION / TIME-STOP!** Liquidating open position for {} (Size: {:.5}) via Market Close.",
            symbol, szi
        );
        crate::discord::send_alert(discord_url.as_deref(), &msg).await;

        match close_market_position(exchange, symbol, 0.02).await {
            Ok(_) => {
                println!("✅ [EMERGENCY FLATTEN] Successfully closed position for {}.", symbol);
                state.remove(symbol);
                let _ = save_state(state_file_path, state);
                release_cluster_lease(symbol);
                let ok_msg = format!("✅ **EMERGENCY FLATTEN COMPLETE:** Position for {} flattened.", symbol);
                crate::discord::send_alert(discord_url.as_deref(), &ok_msg).await;
            }
            Err(e) => {
                eprintln!("❌ [EMERGENCY FLATTEN FAILED] Failed to close position for {}: {}", symbol, e);
                let err_msg = format!("❌ **EMERGENCY FLATTEN FAILED:** Close failed for {}: {}", symbol, e);
                crate::discord::send_alert(discord_url.as_deref(), &err_msg).await;
                return Err(e);
            }
        }
    } else {
        // Zero open position (entry never filled or was cleanly cancelled)
        println!(
            "⚪ [EMERGENCY FLATTEN] Zero open position for {}. State and cluster lease cleared.",
            symbol
        );
        state.remove(symbol);
        let _ = save_state(state_file_path, state);
        release_cluster_lease(symbol);
    }

    Ok(())
}
