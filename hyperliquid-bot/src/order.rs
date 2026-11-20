use crate::hyperliquid::{
    cancel_order, close_market_position_with_bbo, effective_tick_size, fetch_book_ticker,
    fetch_open_orders_for_coin, get_user_position, optimal_engine_b_tp_atr_multiple,
    place_disaster_stop_trigger_order, place_ioc_entry_order, place_post_only_order,
    quantize_price, quantize_size_for_notional,
};
use crate::state::{
    assert_current_lease, clear_order_intent, release_cluster_lease_authenticated, save_order_intent_atomic,
    save_state, try_acquire_cluster_lease_with_side, OrderIntent, OrderIntentStatus,
    PositionLifecycle, PositionState, Side, StateMap,
};
use chrono::Utc;
use ethers::types::H160;
use hyperliquid_rust_sdk::{ExchangeClient, InfoClient};
use serde_json::{json, Value};

#[allow(dead_code)]
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
    frozen_sigma_eq_log: f64,
    entry_ttl_mins: f64,
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
    let side_upper = match side {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    };
    let (acquired, fencing_token) = try_acquire_cluster_lease_with_side(
        symbol,
        "EngineA",
        side_upper,
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

    // Unshaved Equilibrium Mean Target (Section 2.1 & 2.2 of Research Report):
    // Eliminates the legacy adaptive TP shave (\eta^*) that compressed payoff geometry.
    let tick_sz = effective_tick_size(target_mu, sz_decimals);
    let target_tp_quantized = target_mu_quantized;
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

    // Tier 2 Hard Disaster Stop (F-07 decoupled): >= max(2.5 * tp_dist, min_soft_inv + 35 bps, 85 bps)
    // so Fast-Sentinel never prematurely front-runs 5m TwoTierInvalidationManager.
    let sl_dist = crate::ou_process::compute_engine_a_disaster_stop_distance(
        symbol,
        post_only_price,
        target_tp_quantized,
    );
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

    let final_sl_dist = crate::ou_process::compute_engine_a_disaster_stop_distance(
        symbol,
        current_price,
        target_tp_quantized,
    );
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
        frozen_center_price: target_mu_quantized,
        frozen_sigma_eq_log,
        consecutive_ou_breach_5m_bars: 0,
        consecutive_shock_bars: 0,
        breakeven_ratchet_armed: false,
        entry_ttl_mins,
        disaster_stop_oid: None,
        disaster_stop_ratcheted_to_be: false,
        last_fill_check: 0,
        tranche1_tp_filled: false,
        last_peak_mins: 0.0,
        cusum_stat_1m: 0.0,
        entry_sigma_1m: 0.0,
        initial_stop_loss_price: stop_loss_price,
        accrued_funding_usd: 0.0,
        timeout_disabled: false,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    // Intent successfully acknowledged and incorporated into state
    order_intent.status = OrderIntentStatus::Acknowledged;
    let _ = save_order_intent_atomic(&order_intent);
    clear_order_intent(symbol);

    let max_hold = (3.0 * half_life_minutes).clamp(60.0, 360.0);
    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };

    let msg = format!(
        "{} **O-U {} ENTRY SIGNAL (MAKER):** {}\n**Limit Maker {}:** ${:.2} (Best Bid: ${:.2}, Ask: ${:.2})\n**Target Mean (μ):** ${:.2}\n**Take Profit Target:** ${:.2}\n**Hard Disaster Stop:** ${:.2}\n**Size:** {:.5} (${:.2})\n**Half-Life:** {:.1}m (Max Hold: {:.1}m)",
        side_emoji, side_label, symbol, side_label, current_price, top_bid, top_ask, target_mu_quantized, target_tp_quantized, stop_loss_price, current_amount, position_size_usdc, half_life_minutes, max_hold
    );
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": oid, "side": side.to_string()}))
}

#[allow(dead_code)]
pub async fn place_trend_breakout_entry(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    state: &mut StateMap,
    symbol: &str,
    side: Side,
    breakout_price: f64,
    retest_limit_px: Option<f64>,
    initial_stop: f64,
    current_atr: f64,
    position_size_usdc: f64,
    entry_ttl_mins: f64,
    sz_decimals: u32,
    discord_url: Option<String>,
) -> Result<Value, String> {
    let position_size_usdc = position_size_usdc.max(11.0);
    let eff_ttl_mins = if entry_ttl_mins.is_finite() && entry_ttl_mins > 0.0 {
        entry_ttl_mins
    } else {
        12.0
    };
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
    let side_upper = match side {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    };
    let (acquired, fencing_token) = try_acquire_cluster_lease_with_side(
        symbol,
        "EngineB",
        side_upper,
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
    // Policy C Stage 2 Alo Maker Retest Placement:
    // Anchor passively at `retest_limit_px` when provided, clamped strictly inside/on the passive side
    // of the live order book so Post-Only (`Alo`) never crosses the spread.
    let raw_post_only_price = if let Some(target_px) = retest_limit_px.filter(|&p| p.is_finite() && p > 0.0) {
        if is_buy {
            let max_passive_bid = if spread >= 2.0 * book_tick - 1e-9 && top_ask - book_tick > top_bid {
                top_ask - book_tick
            } else {
                top_bid
            };
            target_px.min(max_passive_bid)
        } else {
            let min_passive_ask = if spread >= 2.0 * book_tick - 1e-9 && top_bid + book_tick < top_ask {
                top_bid + book_tick
            } else {
                top_ask
            };
            target_px.max(min_passive_ask)
        }
    } else if is_buy {
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

    // Ensure stop loss stays safely on the adverse side of post_only_price by at least 25 bps
    let safe_stop_raw = if is_buy {
        initial_stop.min(post_only_price * 0.9975)
    } else {
        initial_stop.max(post_only_price * 1.0025)
    };
    let stop_loss_price = quantize_price(safe_stop_raw, sz_decimals);
    let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
    let tp_dist = (k_tp * current_atr).max(post_only_price * 0.0045);
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
                    let fresh_raw = if let Some(target_px) = retest_limit_px.filter(|&p| p.is_finite() && p > 0.0) {
                        if is_buy {
                            target_px.min(fresh_bid)
                        } else {
                            target_px.max(fresh_ask)
                        }
                    } else if is_buy {
                        fresh_bid
                    } else {
                        fresh_ask
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

    let final_stop_loss_price = if is_buy {
        stop_loss_price.min(quantize_price(current_price * 0.9975, sz_decimals))
    } else {
        stop_loss_price.max(quantize_price(current_price * 1.0025, sz_decimals))
    };

    let new_state = PositionState {
        symbol: symbol.to_string(),
        side: side.to_string(),
        order_id: oid.to_string(),
        tp_order_id: "".to_string(),
        entry_price: current_price,
        amount: current_amount,
        stop_loss_price: final_stop_loss_price,
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
        chandelier_stop: final_stop_loss_price,
        active_multiplier: 2.0,
        current_atr,
        lease_fencing_token: fencing_token,
        lease_owner_uuid: lease_uuid,
        lifecycle: PositionLifecycle::Active,
        frozen_center_price: 0.0,
        frozen_sigma_eq_log: 0.0,
        consecutive_ou_breach_5m_bars: 0,
        consecutive_shock_bars: 0,
        breakeven_ratchet_armed: false,
        entry_ttl_mins: eff_ttl_mins,
        disaster_stop_oid: None,
        disaster_stop_ratcheted_to_be: false,
        last_fill_check: 0,
        tranche1_tp_filled: false,
        last_peak_mins: 0.0,
        cusum_stat_1m: 0.0,
        entry_sigma_1m: 0.0,
        initial_stop_loss_price: initial_stop,
        accrued_funding_usd: 0.0,
        timeout_disabled: false,
    };

    state.insert(symbol.to_string(), new_state.clone());
    let _ = save_state(state_file_path, state);

    order_intent.status = OrderIntentStatus::Acknowledged;
    let _ = save_order_intent_atomic(&order_intent);
    clear_order_intent(symbol);

    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };

    let msg = format!(
        "{} **ENGINE B {} BREAKOUT RETEST (POLICY C ALO MAKER):** {}\n**Retest Limit {}:** ${:.2} (Breakout: ${:.2})\n**Initial Stop:** ${:.2}\n**Size:** {:.5} (${:.2}) | **TTL:** {:.0}m | **ATR:** ${:.2}",
        side_emoji, side_label, symbol, side_label, current_price, breakout_price, final_stop_loss_price, current_amount, position_size_usdc, eff_ttl_mins, current_atr
    );
    crate::discord::send_alert(discord_url.as_deref(), &msg).await;

    Ok(json!({"status": "DISPATCHED", "order_id": oid, "side": side.to_string()}))
}

/// Markov-Polo V2 (Package 10) Macro Trend IOC Entry (`EngineB1h` & `EngineB4h`):
/// Submits a slippage-capped IOC entry order, immediately attaches an exchange-native StopMarket SL,
/// and leaves TP uncapped (`take_profit_price = 0.0`) so the monotonic 4.0*ATR Chandelier trails runners.
#[allow(clippy::too_many_arguments)]
pub async fn place_macro_trend_ioc_entry(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    state: &mut StateMap,
    symbol: &str,
    engine_type_tag: &str, // "EngineB1h" or "EngineB4h"
    reason_label: &str,    // e.g. "1H EMA50 PULLBACK", "4H SUPERTREND 3.0 FLIP", "4H DONCHIAN-40 BREAKOUT"
    side: Side,
    signal_stop_dist: f64,
    atr_ref: f64,
    trail_atr_mult: f64,
    max_sl_frac: f64,
    max_hold_hours: usize,
    position_size_usdc: f64,
    sz_decimals: u32,
    discord_url: Option<String>,
) -> Result<Value, String> {
    let position_size_usdc = position_size_usdc.max(12.0);
    let max_slots = crate::state::max_concurrent_cluster_positions();
    if state.len() >= max_slots {
        return Ok(json!({"status": "REJECTED", "reason": "Max concurrent trades reached."}));
    }
    if state.contains_key(symbol) {
        return Ok(json!({
            "status": "REJECTED",
            "reason": format!("Already hold or have open order for {}.", symbol)
        }));
    }

    let lease_uuid = format!("{}_{}", symbol, Utc::now().timestamp_micros());
    let side_upper = match side {
        Side::Long => "LONG",
        Side::Short => "SHORT",
    };
    let (acquired, fencing_token) = try_acquire_cluster_lease_with_side(
        symbol,
        engine_type_tag,
        side_upper,
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
    let ioc_raw_px = if is_buy {
        top_ask * 1.0015
    } else {
        top_bid * 0.9985
    };
    let ioc_limit_px = quantize_price(ioc_raw_px, sz_decimals);
    let ref_px = if is_buy { top_ask } else { top_bid };
    let amount_f = quantize_size_for_notional(position_size_usdc, ref_px, sz_decimals);
    if amount_f <= 0.0 {
        let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
        return Err(format!(
            "Calculated size {:.6} is zero or below minimum size decimals {}",
            position_size_usdc / ref_px, sz_decimals
        ));
    }

    let capped_sl_dist = signal_stop_dist
        .min(ref_px * max_sl_frac)
        .max(ref_px * 0.0050);
    let est_sl_px = quantize_price(
        if is_buy {
            (ref_px - capped_sl_dist).max(effective_tick_size(ref_px, sz_decimals))
        } else {
            ref_px + capped_sl_dist
        },
        sz_decimals,
    );

    let cloid_uuid = uuid::Uuid::new_v4();
    let client_order_id = format!("{}_{}_{}", symbol, engine_type_tag, Utc::now().timestamp_millis());

    let mut order_intent = OrderIntent {
        symbol: symbol.to_string(),
        client_order_id: client_order_id.clone(),
        cloid_hex: Some(cloid_uuid.to_string()),
        owner_uuid: lease_uuid.clone(),
        fencing_token,
        engine_type: engine_type_tag.to_string(),
        side: side.clone(),
        limit_px: ioc_limit_px,
        sz: amount_f,
        sz_decimals,
        target_tp: 0.0,
        stop_loss: est_sl_px,
        half_life_minutes: (max_hold_hours as f64) * 60.0,
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

    let (oid, fill_px, fill_sz) = match place_ioc_entry_order(
        exchange,
        symbol,
        is_buy,
        ioc_limit_px,
        amount_f,
        Some(cloid_uuid),
    )
    .await
    {
        Ok(res) => res,
        Err(e) => {
            order_intent.status = OrderIntentStatus::Aborted;
            let _ = save_order_intent_atomic(&order_intent);
            clear_order_intent(symbol);
            let _ = release_cluster_lease_authenticated(symbol, Some(&lease_uuid), Some(fencing_token));
            return Err(e);
        }
    };

    let eff_fill_px = if fill_px > 0.0 { fill_px } else { ref_px };
    let eff_fill_sz = if fill_sz > 0.0 { fill_sz } else { amount_f };
    let final_sl_dist = signal_stop_dist
        .min(eff_fill_px * max_sl_frac)
        .max(eff_fill_px * 0.0050);
    let final_sl_px = quantize_price(
        if is_buy {
            (eff_fill_px - final_sl_dist).max(effective_tick_size(eff_fill_px, sz_decimals))
        } else {
            eff_fill_px + final_sl_dist
        },
        sz_decimals,
    );

    // Immediately attach exchange-native StopMarket disaster stop for full filled size
    let sl_cloid = uuid::Uuid::new_v4();
    let disaster_stop_oid = match place_disaster_stop_trigger_order(
        exchange,
        symbol,
        !is_buy,
        final_sl_px,
        eff_fill_sz,
        sz_decimals,
        Some(sl_cloid),
    )
    .await
    {
        Ok(sl_id) => Some(sl_id),
        Err(e) => {
            eprintln!(
                "⚠️ [{} SL WARN] Initial StopMarket placement failed for {} (Fast-Sentinel will retry): {}",
                engine_type_tag, symbol, e
            );
            None
        }
    };

    let new_state = PositionState {
        symbol: symbol.to_string(),
        side: side.to_string(),
        order_id: oid.to_string(),
        tp_order_id: "".to_string(),
        entry_price: eff_fill_px,
        amount: eff_fill_sz,
        stop_loss_price: final_sl_px,
        take_profit_price: 0.0, // Uncapped 4.0*ATR Chandelier runner
        opened_at: Utc::now().to_rfc3339(),
        risk_amount_usdc: eff_fill_sz * eff_fill_px,
        is_filled: true,
        half_life_minutes: (max_hold_hours as f64) * 60.0, // Store exact max hold mins for EngineB1h/EngineB4h
        client_order_id,
        max_progress: 0.0,
        max_favorable_price: eff_fill_px,
        effective_tp: 0.0,
        last_reprice_time: Utc::now().timestamp_millis(),
        reprice_count: 0,
        candidate_lower_count: 0,
        engine_type: engine_type_tag.to_string(),
        chandelier_stop: final_sl_px,
        active_multiplier: trail_atr_mult,
        current_atr: atr_ref,
        lease_fencing_token: fencing_token,
        lease_owner_uuid: lease_uuid,
        lifecycle: PositionLifecycle::Active,
        frozen_center_price: 0.0,
        frozen_sigma_eq_log: 0.0,
        consecutive_ou_breach_5m_bars: 0,
        consecutive_shock_bars: 0,
        breakeven_ratchet_armed: false,
        entry_ttl_mins: 0.0,
        disaster_stop_oid,
        disaster_stop_ratcheted_to_be: false,
        last_fill_check: 0,
        tranche1_tp_filled: false,
        last_peak_mins: 0.0,
        cusum_stat_1m: 0.0,
        entry_sigma_1m: 0.0,
        initial_stop_loss_price: final_sl_px,
        accrued_funding_usd: 0.0,
        timeout_disabled: false,
    };

    state.insert(symbol.to_string(), new_state);
    let _ = save_state(state_file_path, state);

    order_intent.exchange_order_id = Some(oid);
    order_intent.status = OrderIntentStatus::Acknowledged;
    order_intent.updated_at_ms = Utc::now().timestamp_millis() as u64;
    let _ = save_order_intent_atomic(&order_intent);
    clear_order_intent(symbol);

    let side_label = if is_buy { "LONG" } else { "SHORT" };
    let side_emoji = if is_buy { "🟢" } else { "🔴" };
    let sl_bps = ((eff_fill_px - final_sl_px).abs() / eff_fill_px.max(1e-9)) * 1e4;
    let msg = format!(
        "{} **MARKOV-POLO V2 ({}) {} FILLED:** {}\n**Fill:** ${:.4} | **Size:** {:.5} (${:.2})\n**Initial SL:** ${:.4} (-{:.0} bps) | **Trail:** {:.1}x ATR (${:.4}) | **Max Hold:** {}h",
        side_emoji, reason_label, side_label, symbol, eff_fill_px, eff_fill_sz, eff_fill_sz * eff_fill_px, final_sl_px, sl_bps, trail_atr_mult, atr_ref, max_hold_hours
    );
    spawn_discord_alert(discord_url, msg);

    Ok(json!({
        "status": "FILLED",
        "order_id": oid,
        "engine": engine_type_tag,
        "reason": reason_label,
        "side": side.to_string(),
        "entry_px": eff_fill_px,
        "stop_loss": final_sl_px
    }))
}

fn spawn_discord_alert(discord_url: Option<String>, msg: String) {
    if let Some(url) = discord_url {
        if !url.trim().is_empty() {
            tokio::spawn(async move {
                crate::discord::send_alert(Some(&url), &msg).await;
            });
        }
    }
}

#[allow(dead_code)]
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
    emergency_flatten_fast(
        state_file_path,
        exchange,
        info,
        wallet_address,
        state,
        symbol,
        sz_decimals,
        discord_url,
        None,
    )
    .await
}

pub async fn emergency_flatten_fast(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    wallet_address: H160,
    state: &mut StateMap,
    symbol: &str,
    sz_decimals: u32,
    discord_url: Option<String>,
    fast_bbo: Option<(f64, f64)>,
) -> Result<(), String> {
    let pos = match state.get(symbol).cloned() {
        Some(p) => p,
        None => return Ok(()),
    };

    let eps = 1e-5;
    let mut fast_close_attempted = false;

    // Fast Path (< 200ms): For an already-filled live position, submit the reduce-only IOC market close
    // IMMEDIATELY using in-memory L2 BBO BEFORE any Discord webhook or sequential REST order-cancel queries!
    if pos.is_filled && pos.amount > eps {
        let est_szi = if pos.side == "LONG" {
            pos.amount
        } else {
            -pos.amount
        };
        println!(
            "🚨 [EMERGENCY FLATTEN FAST] Closing filled {} position FIRST (est_szi: {:.5}, bbo_cached: {})...",
            symbol,
            est_szi,
            fast_bbo.is_some()
        );
        fast_close_attempted = true;
        if let Err(e) = close_market_position_with_bbo(
            exchange,
            info,
            symbol,
            est_szi,
            0.02,
            sz_decimals,
            fast_bbo,
        )
        .await
        {
            // Do not abort here: if TP/SL filled milliseconds earlier, step 4 reconciliation below
            // will verify flat=0 and clean up cleanly; if residual remains, step 4 retries close.
            eprintln!(
                "⚠️ [EMERGENCY FLATTEN FAST] Initial fast market close attempt returned notice for {}: {}. Proceeding to order/position reconciliation...",
                symbol, e
            );
        } else {
            spawn_discord_alert(
                discord_url.clone(),
                format!(
                    "🚨 **FAST EMERGENCY FLATTEN:** Liquidated {} (Est. Size: {:.5}) via immediate Market Close.",
                    symbol, est_szi
                ),
            );
        }
    } else {
        println!(
            "🚨 [EMERGENCY FLATTEN] Invalidation triggered for {}. Checking order & position status...",
            symbol
        );
    }

    // 1. Cancel resting entry order if it was active
    if !pos.order_id.is_empty() {
        if let Ok(oid) = pos.order_id.parse::<u64>() {
            let _ = cancel_order(exchange, symbol, oid).await;
        }
    }

    // 2. Cancel resting TP order and disaster stop trigger if active
    if !pos.tp_order_id.is_empty() {
        if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
            let _ = cancel_order(exchange, symbol, tp_oid).await;
        }
    }
    if let Some(sl_oid) = pos.disaster_stop_oid {
        let _ = cancel_order(exchange, symbol, sl_oid).await;
    }

    // 3. Reconcile open orders on exchange: cancel any sibling orders and confirm entry order is no longer resting
    let open_orders = fetch_open_orders_for_coin(info, wallet_address, symbol).await?;
    for sym_ord in open_orders.iter().filter(|o| o.coin == symbol) {
        let _ = cancel_order(exchange, symbol, sym_ord.oid).await;
    }
    if !pos.order_id.is_empty() {
        if let Ok(entry_oid) = pos.order_id.parse::<u64>() {
            if open_orders.iter().any(|o| o.oid == entry_oid) {
                tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                let _ = cancel_order(exchange, symbol, entry_oid).await;
                let post_cancel_orders = fetch_open_orders_for_coin(info, wallet_address, symbol).await?;
                if post_cancel_orders.iter().any(|o| o.oid == entry_oid) {
                    let err = format!(
                        "Emergency flatten failed: entry order {} still active on exchange. Retaining state and lease.",
                        entry_oid
                    );
                    eprintln!("❌ [EMERGENCY FLATTEN BLOCKED] {}", err);
                    spawn_discord_alert(discord_url.clone(), format!("🚨 {}", err));
                    return Err(err);
                }
            }
        }
    }

    // 4. Query actual live position on Hyperliquid with epsilon tolerance (handles unfilled partial entries or fast-path residual)
    let (szi, _) = get_user_position(info, wallet_address, symbol).await?;

    if szi.abs() > eps {
        println!(
            "🚨 [EMERGENCY FLATTEN] Active open position detected for {} (szi: {:.5}). Executing Market Close...",
            symbol, szi
        );
        spawn_discord_alert(
            discord_url.clone(),
            format!(
                "🚨 **REGIME INVALIDATION / TIME-STOP!** Liquidating open position for {} (Size: {:.5}) via Market Close.",
                symbol, szi
            ),
        );

        let fallback_bbo = if fast_close_attempted { None } else { fast_bbo };
        if let Err(e) =
            close_market_position_with_bbo(exchange, info, symbol, szi, 0.02, sz_decimals, fallback_bbo)
                .await
        {
            eprintln!("❌ [EMERGENCY FLATTEN FAILED] Close failed for {}: {}", symbol, e);
            spawn_discord_alert(
                discord_url.clone(),
                format!("❌ **EMERGENCY FLATTEN FAILED:** Close failed for {}: {}", symbol, e),
            );
            return Err(e);
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // 5. Final multi-step verification: position == 0 AND no orders remain for symbol
    let (mut final_szi, _) = get_user_position(info, wallet_address, symbol).await?;
    let mut final_orders = fetch_open_orders_for_coin(info, wallet_address, symbol).await?;
    for rem in final_orders.iter().filter(|o| o.coin == symbol) {
        let _ = cancel_order(exchange, symbol, rem.oid).await;
    }
    if final_szi.abs() > eps || final_orders.iter().any(|o| o.coin == symbol) {
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        if let Ok((recheck_szi, _)) = get_user_position(info, wallet_address, symbol).await {
            final_szi = recheck_szi;
        }
        if let Ok(recheck_orders) = fetch_open_orders_for_coin(info, wallet_address, symbol).await {
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
            if let Some((bid, ask)) = fast_bbo {
                if bid > 0.0 && ask > 0.0 {
                    exit_px = if pos.side == "LONG" { bid } else { ask };
                }
            } else if let Ok((bid, ask)) = fetch_book_ticker(info, symbol).await {
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
        spawn_discord_alert(
            discord_url,
            format!(
                "✅ **EMERGENCY FLATTEN COMPLETE:** Position and all orders for {} flattened and verified zero.",
                symbol
            ),
        );
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
        spawn_discord_alert(
            discord_url,
            format!(
                "⚠️ **EMERGENCY FLATTEN INCOMPLETE:** {} on {}. Retaining state for supervisor retry.",
                reason, symbol
            ),
        );
        Err(format!("Emergency flatten reconciliation incomplete: {}", reason))
    }
}
