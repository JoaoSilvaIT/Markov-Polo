use crate::hyperliquid::{
    cancel_order, close_market_position, effective_tick_size, fetch_book_ticker,
    get_user_position, optimal_engine_b_tp_atr_multiple, place_post_only_order, quantize_price,
    quantize_size_for_notional,
};
use crate::state::{
    assert_current_lease, clear_order_intent, release_cluster_lease_authenticated, save_order_intent_atomic,
    save_state, try_acquire_cluster_lease_for_engine_with_uuid, OrderIntent, OrderIntentStatus,
    PositionLifecycle, PositionState, Side, StateMap,
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
    let position_size_usdc = position_size_usdc.max(11.0);
    if state.len() >= crate::state::max_concurrent_cluster_positions() {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({
            "status": "REJECTED",
            "reason": format!("Already hold or have open order for {}.", symbol)
        }));
    }

    let lease_uuid = format!("{}_{}", symbol, Utc::now().timestamp_micros());

    // Atomically acquire the portfolio cluster lease (1 hour renewed continuously while holding)
    let (acquired, fencing_token) = try_acquire_cluster_lease_for_engine_with_uuid(
        symbol,
        "EngineA",
        &lease_uuid,
        3600,
    );
    if !acquired {
        return Ok(json!({
            "status": "REJECTED",
            "reason": "Cluster lock currently acquired by another portfolio symbol."
        }));
    }

    // Fail-Closed: Query live order book ticker to join the top of book without crossing
    let (top_bid, top_ask) = match fetch_book_ticker(info, symbol).await {
        Ok(quotes) => quotes,
        Err(e) => {
            let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
            return Err(format!("Failed to query live book ticker: {}", e));
        }
    };

    if top_bid <= 0.0 || top_ask <= 0.0 || top_bid >= top_ask {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Invalid or crossed book ticker quotes: bid={}, ask={}",
            top_bid, top_ask
        ));
    }

    let is_buy = side == Side::Long;
    let book_tick = effective_tick_size(top_bid, sz_decimals);
    let spread = top_ask - top_bid;
    // Spread-Inside Queue Priority Optimization:
    // If bid-ask spread is >= 2 ticks wide, step 1 tick inside the spread to achieve FIFO Queue Rank #1
    // with zero queue-ahead volume while remaining strictly Post-Only (Alo).
    let best_passive_quote = if is_buy {
        if spread >= 2.0 * book_tick - 1e-9 && top_bid + book_tick < top_ask - 1e-9 {
            top_bid + book_tick
        } else {
            top_bid
        }
    } else if spread >= 2.0 * book_tick - 1e-9 && top_ask - book_tick > top_bid + 1e-9 {
        top_ask - book_tick
    } else {
        top_ask
    };
    let raw_post_only_price = if is_buy {
        f64::min(best_passive_quote, entry_price)
    } else {
        f64::max(best_passive_quote, entry_price)
    };

    let post_only_price = quantize_price(raw_post_only_price, sz_decimals);
    let target_mu_quantized = quantize_price(target_mu, sz_decimals);

    let amount_f = quantize_size_for_notional(position_size_usdc, post_only_price, sz_decimals);

    if amount_f <= 0.0 {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Calculated size {:.6} is zero or below minimum size decimals {}",
            position_size_usdc / post_only_price, sz_decimals
        ));
    }

    // Epsilon Shave + Avellaneda-Stoikov Inventory Skew:
    // Tick-aware front-run (5 bps + 4 bps, floored in ticks and capped at 25% of gross target distance)
    // to aggressively exit inventory, accelerate fill probability, and free cluster lock faster.
    let tick_sz = effective_tick_size(target_mu, sz_decimals);
    let gross_dist = (target_mu - post_only_price).abs();
    let base_shave = (target_mu * 0.0005).max(2.0 * tick_sz);
    let as_inventory_skew = (target_mu * 0.0004).max(1.5 * tick_sz);
    let total_shave = (base_shave + as_inventory_skew).min(0.25 * gross_dist);
    let target_tp = if is_buy {
        target_mu - total_shave
    } else {
        target_mu + total_shave
    };
    let target_tp_quantized = quantize_price(target_tp, sz_decimals);
    let min_profitable_tp = if is_buy {
        target_tp_quantized > post_only_price + tick_sz
    } else {
        target_tp_quantized + tick_sz < post_only_price
    };
    if !min_profitable_tp {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Quantized TP ${:.4} does not clear entry ${:.4} for {} ({})",
            target_tp_quantized, post_only_price, symbol, side
        ));
    }

    // Deterministic O-U Stop Loss: 2.2x target distance, clamped to [48 bps, 68 bps] from entry
    // so normal range noise (15-30 bps) never wicks out the trade while worst-case stop is tightly bounded.
    let edge_dist = (post_only_price - target_tp_quantized).abs();
    let sl_dist = (2.2 * edge_dist).clamp(post_only_price * 0.0048, post_only_price * 0.0068);
    let raw_sl = if is_buy {
        post_only_price - sl_dist
    } else {
        post_only_price + sl_dist
    };
    let stop_loss_price = quantize_price(raw_sl, sz_decimals);

    let cloid_uuid = uuid::Uuid::new_v4();
    let client_order_id = format!("{}_{}", symbol, Utc::now().timestamp_millis());

    // ------------------------------------------------------------------------
    // PROTOCOL P0: WRITE-AHEAD ORDER INTENT (WAL) BEFORE EXCHANGE TRANSMISSION
    // ------------------------------------------------------------------------
    let mut order_intent = OrderIntent {
        symbol: symbol.to_string(),
        client_order_id: client_order_id.clone(),
        cloid_hex: Some(cloid_uuid.to_string()),
        owner_uuid: lease_uuid.clone(),
        fencing_token,
        engine_type: "EngineA".to_string(),
        side: side.clone(),
        limit_px: post_only_price,
        sz: amount_f,
        sz_decimals,
        target_tp: target_tp_quantized,
        stop_loss: stop_loss_price,
        half_life_minutes,
        position_size_usdc,
        status: OrderIntentStatus::PendingSubmission,
        exchange_order_id: None,
        created_at_ms: Utc::now().timestamp_millis() as u64,
        updated_at_ms: Utc::now().timestamp_millis() as u64,
    };
    save_order_intent_atomic(&order_intent)?;

    // ------------------------------------------------------------------------
    // PROTOCOL P0: FENCING ASSERTION IMMEDIATELY AT ORDER BOUNDARY
    // ------------------------------------------------------------------------
    if let Err(e) = assert_current_lease(symbol, &lease_uuid, fencing_token) {
        order_intent.status = OrderIntentStatus::Aborted;
        let _ = save_order_intent_atomic(&order_intent);
        clear_order_intent(symbol);
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!("Order boundary fencing assertion rejected: {}", e));
    }

    // Submit Post-Only (Alo) Maker order with cloid (with up to 2 transit-jitter retries)
    let mut current_price = post_only_price;
    let mut current_amount = amount_f;
    let mut last_err = String::new();
    let mut oid_opt = None;

    for attempt in 0..3 {
        if attempt > 0 {
            if let Ok((fresh_bid, fresh_ask)) = fetch_book_ticker(info, symbol).await {
                if fresh_bid > 0.0 && fresh_ask > fresh_bid {
                    let fresh_raw = if is_buy {
                        f64::min(fresh_bid, entry_price)
                    } else {
                        f64::max(fresh_ask, entry_price)
                    };
                    current_price = quantize_price(fresh_raw, sz_decimals);
                    current_amount = quantize_size_for_notional(position_size_usdc, current_price, sz_decimals);
                    order_intent.limit_px = current_price;
                    order_intent.sz = current_amount;
                    let _ = save_order_intent_atomic(&order_intent);
                }
            }
        }

        match place_post_only_order(
            exchange,
            symbol,
            is_buy,
            current_price,
            current_amount,
            false, // reduce_only = false for entry
            Some(cloid_uuid),
        )
        .await
        {
            Ok(id) => {
                oid_opt = Some(id);
                break;
            }
            Err(e) => {
                if e.contains("Post only order would have immediately matched") && attempt < 2 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    last_err = e;
                    continue;
                } else {
                    last_err = e;
                    break;
                }
            }
        }
    }

    let oid = match oid_opt {
        Some(id) => {
            // Persist order_id into intent WAL
            order_intent.exchange_order_id = Some(id);
            order_intent.status = OrderIntentStatus::Submitted;
            order_intent.updated_at_ms = Utc::now().timestamp_millis() as u64;
            let _ = save_order_intent_atomic(&order_intent);
            id
        }
        None => {
            order_intent.status = OrderIntentStatus::Aborted;
            let _ = save_order_intent_atomic(&order_intent);
            clear_order_intent(symbol);
            let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
            return Err(last_err);
        }
    };

    let final_edge_dist = (current_price - target_tp_quantized).abs();
    let final_sl_dist = (2.0 * final_edge_dist).max(current_price * 0.015);
    let stop_loss_price = quantize_price(
        if is_buy {
            current_price - final_sl_dist
        } else {
            current_price + final_sl_dist
        },
        sz_decimals,
    );

    let new_state = PositionState {
        symbol: symbol.to_string(),
        side: side.to_string(),
        order_id: oid.to_string(),
        tp_order_id: "".to_string(), // Populated when entry fills and TP is placed
        entry_price: current_price,
        amount: current_amount,
        stop_loss_price,
        take_profit_price: target_tp_quantized,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: position_size_usdc,
        is_filled: false,
        half_life_minutes,
        client_order_id,
        max_progress: 0.0,
        max_favorable_price: current_price,
        effective_tp: target_tp_quantized,
        last_reprice_time: Utc::now().timestamp_millis(),
        reprice_count: 0,
        candidate_lower_count: 0,
        engine_type: "EngineA".to_string(),
        chandelier_stop: 0.0,
        active_multiplier: 0.0,
        current_atr: 0.0,
        lease_fencing_token: fencing_token,
        lease_owner_uuid: lease_uuid,
        lifecycle: PositionLifecycle::Active,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    // Intent successfully acknowledged and incorporated into state
    order_intent.status = OrderIntentStatus::Acknowledged;
    let _ = save_order_intent_atomic(&order_intent);
    clear_order_intent(symbol);

    let max_hold = (2.0 * half_life_minutes).clamp(15.0, 360.0);
    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };

    let msg = format!(
        "{} **O-U {} ENTRY SIGNAL (MAKER):** {}\n**Limit Maker {}:** ${:.2} (Best Bid: ${:.2}, Ask: ${:.2})\n**Target Mean (μ):** ${:.2}\n**Take Profit Target:** ${:.2}\n**Hard Stop Loss:** ${:.2}\n**Size:** {:.5} (${:.2})\n**Half-Life:** {:.1}m (Max Hold: {:.1}m)",
        side_emoji, side_label, symbol, side_label, current_price, top_bid, top_ask, target_mu_quantized, target_tp_quantized, stop_loss_price, current_amount, position_size_usdc, half_life_minutes, max_hold
    );
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": oid, "side": side.to_string()}))
}

pub async fn place_trend_breakout_entry(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    state: &mut StateMap,
    symbol: &str,
    side: Side,
    breakout_price: f64,
    initial_stop: f64,
    current_atr: f64,
    position_size_usdc: f64,
    sz_decimals: u32,
    discord_url: Option<String>,
) -> Result<Value, String> {
    let position_size_usdc = position_size_usdc.max(11.0);
    if state.len() >= crate::state::max_concurrent_cluster_positions() {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({
            "status": "REJECTED",
            "reason": format!("Already hold or have open order for {}.", symbol)
        }));
    }

    let lease_uuid = format!("{}_{}", symbol, Utc::now().timestamp_micros());

    // Atomically acquire the portfolio cluster lease for EngineB
    let (acquired, fencing_token) = try_acquire_cluster_lease_for_engine_with_uuid(
        symbol,
        "EngineB",
        &lease_uuid,
        3600,
    );
    if !acquired {
        return Ok(json!({
            "status": "REJECTED",
            "reason": "Cluster lock currently acquired by another portfolio symbol."
        }));
    }

    let (top_bid, top_ask) = match fetch_book_ticker(info, symbol).await {
        Ok(quotes) => quotes,
        Err(e) => {
            let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
            return Err(format!("Failed to query live book ticker: {}", e));
        }
    };

    if top_bid <= 0.0 || top_ask <= 0.0 || top_bid >= top_ask {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Invalid or crossed book ticker quotes: bid={}, ask={}",
            top_bid, top_ask
        ));
    }

    let is_buy = side == Side::Long;
    let book_tick = effective_tick_size(top_bid, sz_decimals);
    let spread = top_ask - top_bid;
    // Spread-Inside Queue Priority Optimization for Engine B:
    // Step 1 tick inside a multi-tick spread to secure FIFO Queue Rank #1 on momentum breakouts.
    let raw_post_only_price = if is_buy {
        if spread >= 2.0 * book_tick - 1e-9 && top_bid + book_tick < top_ask - 1e-9 {
            top_bid + book_tick
        } else {
            top_bid
        }
    } else if spread >= 2.0 * book_tick - 1e-9 && top_ask - book_tick > top_bid + 1e-9 {
        top_ask - book_tick
    } else {
        top_ask
    };
    let post_only_price = quantize_price(raw_post_only_price, sz_decimals);
    let amount_f = quantize_size_for_notional(position_size_usdc, post_only_price, sz_decimals);

    if amount_f <= 0.0 {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Calculated size {:.6} is zero or below minimum size decimals {}",
            position_size_usdc / post_only_price, sz_decimals
        ));
    }

    let stop_loss_price = quantize_price(initial_stop, sz_decimals);
    let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
    let tp_dist = (k_tp * current_atr).max(post_only_price * 0.0040);
    let raw_tp = if is_buy {
        post_only_price + tp_dist
    } else {
        (post_only_price - tp_dist).max(book_tick)
    };
    let take_profit_price = quantize_price(raw_tp, sz_decimals);
    let cloid_uuid = uuid::Uuid::new_v4();
    let client_order_id = format!("{}_{}", symbol, Utc::now().timestamp_millis());

    let mut order_intent = OrderIntent {
        symbol: symbol.to_string(),
        client_order_id: client_order_id.clone(),
        cloid_hex: Some(cloid_uuid.to_string()),
        owner_uuid: lease_uuid.clone(),
        fencing_token,
        engine_type: "EngineB".to_string(),
        side: side.clone(),
        limit_px: post_only_price,
        sz: amount_f,
        sz_decimals,
        target_tp: take_profit_price,
        stop_loss: stop_loss_price,
        half_life_minutes: 0.0,
        position_size_usdc,
        status: OrderIntentStatus::PendingSubmission,
        exchange_order_id: None,
        created_at_ms: Utc::now().timestamp_millis() as u64,
        updated_at_ms: Utc::now().timestamp_millis() as u64,
    };
    save_order_intent_atomic(&order_intent)?;

    if let Err(e) = assert_current_lease(symbol, &lease_uuid, fencing_token) {
        order_intent.status = OrderIntentStatus::Aborted;
        let _ = save_order_intent_atomic(&order_intent);
        clear_order_intent(symbol);
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!("Order boundary fencing assertion rejected: {}", e));
    }

    // Submit Post-Only (Alo) Maker order with cloid (with up to 2 transit-jitter retries)
    let mut current_price = post_only_price;
    let mut current_amount = amount_f;
    let mut last_err = String::new();
    let mut oid_opt = None;

    for attempt in 0..3 {
        if attempt > 0 {
            if let Ok((fresh_bid, fresh_ask)) = fetch_book_ticker(info, symbol).await {
                if fresh_bid > 0.0 && fresh_ask > fresh_bid {
                    let fresh_raw = if is_buy { fresh_bid } else { fresh_ask };
                    current_price = quantize_price(fresh_raw, sz_decimals);
                    current_amount = quantize_size_for_notional(position_size_usdc, current_price, sz_decimals);
                    order_intent.limit_px = current_price;
                    order_intent.sz = current_amount;
                    let _ = save_order_intent_atomic(&order_intent);
                }
            }
        }

        match place_post_only_order(
            exchange,
            symbol,
            is_buy,
            current_price,
            current_amount,
            false,
            Some(cloid_uuid),
        )
        .await
        {
            Ok(id) => {
                oid_opt = Some(id);
                break;
            }
            Err(e) => {
                if e.contains("Post only order would have immediately matched") && attempt < 2 {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    last_err = e;
                    continue;
                } else {
                    last_err = e;
                    break;
                }
            }
        }
    }

    let oid = match oid_opt {
        Some(id) => {
            order_intent.exchange_order_id = Some(id);
            order_intent.status = OrderIntentStatus::Submitted;
            order_intent.updated_at_ms = Utc::now().timestamp_millis() as u64;
            let _ = save_order_intent_atomic(&order_intent);
            id
        }
        None => {
            order_intent.status = OrderIntentStatus::Aborted;
            let _ = save_order_intent_atomic(&order_intent);
            clear_order_intent(symbol);
            let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
            return Err(last_err);
        }
    };

    let new_state = PositionState {
        symbol: symbol.to_string(),
        side: side.to_string(),
        order_id: oid.to_string(),
        tp_order_id: "".to_string(),
        entry_price: current_price,
        amount: current_amount,
        stop_loss_price,
        take_profit_price,
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: position_size_usdc,
        is_filled: false,
        half_life_minutes: 0.0,
        client_order_id,
        max_progress: 0.0,
        max_favorable_price: current_price,
        effective_tp: take_profit_price,
        last_reprice_time: Utc::now().timestamp_millis(),
        reprice_count: 0,
        candidate_lower_count: 0,
        engine_type: "EngineB".to_string(),
        chandelier_stop: stop_loss_price,
        active_multiplier: 2.0,
        current_atr,
        lease_fencing_token: fencing_token,
        lease_owner_uuid: lease_uuid,
        lifecycle: PositionLifecycle::Active,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    order_intent.status = OrderIntentStatus::Acknowledged;
    let _ = save_order_intent_atomic(&order_intent);
    clear_order_intent(symbol);

    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };

    let msg = format!(
        "{} **ENGINE B {} BREAKOUT ENTRY (MAKER):** {}\n**Limit Maker {}:** ${:.2} (Breakout: ${:.2})\n**Initial Chandelier Stop:** ${:.2}\n**Size:** {:.5} (${:.2})\n**ATR:** ${:.2}",
        side_emoji, side_label, symbol, side_label, current_price, breakout_price, stop_loss_price, current_amount, position_size_usdc, current_atr
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
    sz_decimals: u32,
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

    // 2. Cancel resting TP order if active
    if !pos.tp_order_id.is_empty() {
        if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
            let _ = cancel_order(exchange, symbol, tp_oid).await;
        }
    }

    // 3. Reconcile open orders on exchange: cancel any sibling orders and confirm entry order is no longer resting
    let open_orders = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        info.open_orders(wallet_address),
    )
    .await
    .map_err(|_| "Timed out querying open_orders in emergency_flatten".to_string())?
    .map_err(|e| format!("Failed to query open_orders in emergency_flatten: {}", e))?;
    for sym_ord in open_orders.iter().filter(|o| o.coin == symbol) {
        let _ = cancel_order(exchange, symbol, sym_ord.oid).await;
    }
    if !pos.order_id.is_empty() {
        if let Ok(entry_oid) = pos.order_id.parse::<u64>() {
            if open_orders.iter().any(|o| o.oid == entry_oid) {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                let _ = cancel_order(exchange, symbol, entry_oid).await;
                let post_cancel_orders = tokio::time::timeout(
                    tokio::time::Duration::from_secs(10),
                    info.open_orders(wallet_address),
                )
                .await
                .map_err(|_| "Timed out querying post-cancel open_orders".to_string())?
                .map_err(|e| format!("Failed to query post-cancel open_orders: {}", e))?;
                if post_cancel_orders.iter().any(|o| o.oid == entry_oid) {
                    let err = format!(
                        "Emergency flatten failed: entry order {} still active on exchange. Retaining state and lease.",
                        entry_oid
                    );
                    eprintln!("❌ [EMERGENCY FLATTEN BLOCKED] {}", err);
                    crate::discord::send_alert(discord_url.as_deref(), &format!("🚨 {}", err)).await;
                    return Err(err);
                }
            }
        }
    }

    // 4. Query actual live position on Hyperliquid with epsilon tolerance
    let (szi, _) = get_user_position(info, wallet_address, symbol).await?;
    let eps = 1e-5;

    if szi.abs() > eps {
        println!(
            "🚨 [EMERGENCY FLATTEN] Active open position detected for {} (szi: {:.5}). Executing Market Close...",
            symbol, szi
        );
        let msg = format!(
            "🚨 **REGIME INVALIDATION / TIME-STOP!** Liquidating open position for {} (Size: {:.5}) via Market Close.",
            symbol, szi
        );
        crate::discord::send_alert(discord_url.as_deref(), &msg).await;

        if let Err(e) = close_market_position(exchange, info, symbol, szi, 0.02, sz_decimals).await {
            eprintln!("❌ [EMERGENCY FLATTEN FAILED] Close failed for {}: {}", symbol, e);
            let err_msg = format!("❌ **EMERGENCY FLATTEN FAILED:** Close failed for {}: {}", symbol, e);
            crate::discord::send_alert(discord_url.as_deref(), &err_msg).await;
            return Err(e);
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(650)).await;
    }

    // 5. Final multi-step verification: position == 0 AND no orders remain for symbol
    let (mut final_szi, _) = get_user_position(info, wallet_address, symbol).await?;
    let mut final_orders = tokio::time::timeout(
        tokio::time::Duration::from_secs(10),
        info.open_orders(wallet_address),
    )
    .await
    .map_err(|_| "Timed out querying final open_orders".to_string())?
    .map_err(|e| format!("Failed to query final open_orders: {}", e))?;
    for rem in final_orders.iter().filter(|o| o.coin == symbol) {
        let _ = cancel_order(exchange, symbol, rem.oid).await;
    }
    if final_szi.abs() > eps || final_orders.iter().any(|o| o.coin == symbol) {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if let Ok((recheck_szi, _)) = get_user_position(info, wallet_address, symbol).await {
            final_szi = recheck_szi;
        }
        if let Ok(Ok(recheck_orders)) = tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            info.open_orders(wallet_address),
        )
        .await
        {
            final_orders = recheck_orders;
        }
    }
    let symbol_orders_remaining = final_orders.iter().any(|o| o.coin == symbol);

    if final_szi.abs() <= eps && !symbol_orders_remaining {
        // Defensive sequence step 1: Mark state as CloseConfirmed and fsync
        if let Some(p) = state.get_mut(symbol) {
            p.lifecycle = PositionLifecycle::CloseConfirmed;
        }
        let _ = save_state(state_file_path, state);

        // Defensive sequence step 2: Release authenticated lease
        let released = if !pos.lease_owner_uuid.is_empty() {
            release_cluster_lease_authenticated(
                symbol,
                Some(&pos.lease_owner_uuid),
                Some(pos.lease_fencing_token),
            )
        } else {
            release_cluster_lease_authenticated(symbol, None, None)
        };

        if !released {
            eprintln!(
                "⚠️ [EMERGENCY FLATTEN] Lease for {} owned by a newer epoch on disk, but exchange is verified 100% flat. Proceeding to clear stale local state.",
                symbol
            );
        }

        // Defensive sequence step 3: Mark state as LeaseReleased and fsync
        if let Some(p) = state.get_mut(symbol) {
            p.lifecycle = PositionLifecycle::LeaseReleased;
        }
        let _ = save_state(state_file_path, state);

        println!(
            "✅ [EMERGENCY FLATTEN] Successfully verified zero position, zero open orders, and released cluster lease for {}. Clearing state.",
            symbol
        );
        if (pos.is_filled || szi.abs() > eps) && pos.entry_price > 0.0 && pos.amount > 0.0 {
            let mut exit_px = pos.entry_price;
            if let Ok((bid, ask)) = fetch_book_ticker(info, symbol).await {
                if bid > 0.0 && ask > 0.0 {
                    exit_px = if pos.side == "LONG" { bid } else { ask };
                }
            }
            let pnl_usd = if pos.side == "LONG" {
                (exit_px - pos.entry_price) * pos.amount
            } else {
                (pos.entry_price - exit_px) * pos.amount
            };
            let pnl_bps = if pos.side == "LONG" {
                ((exit_px - pos.entry_price) / pos.entry_price) * 10_000.0
            } else {
                ((pos.entry_price - exit_px) / pos.entry_price) * 10_000.0
            };
            crate::telemetry::append_real_trade_record(&crate::telemetry::RealTradeRecord {
                symbol: symbol.to_string(),
                side: pos.side.clone(),
                engine: pos.engine_type.clone(),
                amount: pos.amount,
                entry_price: pos.entry_price,
                exit_price: exit_px,
                opened_at: pos.opened_at.clone(),
                closed_at: chrono::Utc::now().to_rfc3339(),
                pnl_usd,
                pnl_bps,
                exit_reason: if pnl_usd > 0.0 { "PROFIT_LOCK_EXIT".to_string() } else { "STOP_OR_FLATTEN".to_string() },
                is_win: pnl_usd > 0.0,
            });
        }
        state.remove(symbol);
        let _ = save_state(state_file_path, state);
        let ok_msg = format!(
            "✅ **EMERGENCY FLATTEN COMPLETE:** Position and all orders for {} flattened and verified zero.",
            symbol
        );
        crate::discord::send_alert(discord_url.as_deref(), &ok_msg).await;
        Ok(())
    } else {
        let reason = if final_szi.abs() > eps {
            format!("Residual size {:.5} remains", final_szi)
        } else {
            format!("Uncancelled open orders remain for {}", symbol)
        };
        eprintln!(
            "⚠️ [EMERGENCY FLATTEN WARNING] {} on {}. Retaining state and lease for supervisor retry!",
            reason, symbol
        );
        let warn_msg = format!(
            "⚠️ **EMERGENCY FLATTEN INCOMPLETE:** {} on {}. Retaining state for supervisor retry.",
            reason, symbol
        );
        crate::discord::send_alert(discord_url.as_deref(), &warn_msg).await;
        Err(format!("Emergency flatten reconciliation incomplete: {}", reason))
    }
}
