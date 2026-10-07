use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use std::time::Duration;
use futures_util::StreamExt;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use serde_json::Value;

mod alpaca;
mod binance;
mod config;
mod discord;
mod garch;
mod gemini;
mod hmm;
mod indicators;
mod order;
mod state;
mod ws_sentinel;
mod ou_process;

use crate::config::Config;
use crate::binance::{BinanceClient, get_symbol_lot_precision, quantize_lot_size};
use crate::hmm::{GaussianHMM, train_hmm_offline};
use crate::ou_process::{estimate_ou_parameters, calculate_optimal_thresholds, calculate_mad_sigma, calculate_net_edge_bps};
use crate::order::{place_ou_limit_buy, emergency_flatten};
use crate::state::{StateMap, load_state, load_circuit_breaker, save_circuit_breaker, load_armed_signal, save_armed_signal, has_active_cluster_position};

#[tokio::main]
async fn main() {
    println!("🚀 [SYSTEM START] Fully Autonomous Deterministic O-U Architecture Initialized.");
    
    let config = Config::from_env();
    let binance = Arc::new(BinanceClient::new(config.binance_api_key.clone(), config.binance_secret_key.clone()));
    
    // Load current position state
    let state_map: StateMap = load_state(&config.state_file_path).unwrap_or_default();
    let active_state = Arc::new(Mutex::new(state_map));
    let state_file_path = config.state_file_path.clone();

    // Circuit Breaker initialization
    let cb_path = format!("binance_hmm_cb_{}.json", config.trading_symbol);
    let cb_state = load_circuit_breaker(&cb_path);
    let active_cb = Arc::new(Mutex::new(cb_state));
    let cb_file_path = cb_path.clone();

    // Reversal-Confirmed Entry Armed state path
    let arm_file_path = format!("binance_hmm_arm_{}.json", config.trading_symbol);
    
    // Initialize HMM (will load from file or initialize empty)
    let initial_hmm = GaussianHMM::load_weights(&config.hmm_config_path).unwrap_or_else(|_| hmm::GaussianHMM::new(3, 2));
    let hmm_model = Arc::new(RwLock::new(initial_hmm));

    let discord_url = config.discord_webhook_url.clone();
    
    // ------------------------------------------------------------------------
    // COLD-PATH: Automated 24-Hour HMM Trainer
    // ------------------------------------------------------------------------
    let hmm_trainer_clone = hmm_model.clone();
    let binance_trainer_clone = binance.clone();
    let config_trainer_path = config.hmm_config_path.clone();
    let config_trainer_symbol = config.trading_symbol.clone();
    
    tokio::spawn(async move {
        loop {
            println!("⚙️ [COLD-PATH] Initiating 24-hour HMM recalibration...");
            let klines_res = binance_trainer_clone.fetch_ohlcv(&config_trainer_symbol, "15m", 1000).await.ok();
            if let Some(klines) = klines_res {
                let mut closes = Vec::new();
                for k in klines {
                    if let Some(close_str) = k[4].as_str() {
                        if let Ok(c) = close_str.parse::<f64>() { closes.push(c); }
                    }
                }
                
                // Offload heavy Expectation-Maximization to blocking pool
                if let Ok(Some(new_hmm)) = tokio::task::spawn_blocking(move || train_hmm_offline(&closes)).await {
                    // Save weights to disk
                    let _ = new_hmm.save_weights(&config_trainer_path);
                    
                    // Hot-swap the live model in memory
                    let mut lock = hmm_trainer_clone.write().await;
                    *lock = new_hmm;
                    println!("✅ [COLD-PATH] HMM recalibrated and hot-swapped into live memory successfully.");
                }
            }
            // Sleep for 24 hours
            tokio::time::sleep(Duration::from_secs(86400)).await;
        }
    });

    // ------------------------------------------------------------------------
    // HOT-PATH: WebSocket Ingestion & O-U Execution
    // ------------------------------------------------------------------------
    let mut rolling_closes: Vec<f64> = Vec::with_capacity(480);
    
    println!("⏳ [INGESTION] Pre-filling 480-minute rolling window via REST API...");
    let trading_symbol = config.trading_symbol.clone();
    match binance.fetch_ohlcv(&trading_symbol, "1m", 480).await {
        Ok(klines) => {
            for k in klines {
                if let Some(c) = k[4].as_str().and_then(|s| s.parse::<f64>().ok()) {
                    rolling_closes.push(c);
                }
            }
            println!("✅ [INGESTION] Successfully loaded {} historical 1m bars for {}.", rolling_closes.len(), trading_symbol);
        }
        Err(e) => {
            eprintln!("❌ [INGESTION ERROR] Failed to fetch historical 1m bars for {}: {}", trading_symbol, e);
        }
    }
    
    crate::discord::send_alert(discord_url.as_deref(), "🚀 **O-U Architecture Online**\nFully automated HMM recalibration and deterministic websocket ingestion active.").await;

    // ------------------------------------------------------------------------
    // FAST ORDER SUPERVISOR (Instant TP Placement & Trade Settlement)
    // Runs every 1.5 seconds to eliminate the 59-second latency gap.
    // ------------------------------------------------------------------------
    let watcher_state = active_state.clone();
    let watcher_binance = binance.clone();
    let watcher_symbol = config.trading_symbol.clone();
    let watcher_state_path = config.state_file_path.clone();
    let watcher_discord = config.discord_webhook_url.clone();
    let watcher_cb = active_cb.clone();
    let watcher_cb_path = cb_file_path.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        loop {
            interval.tick().await;

            let mut state_lock = watcher_state.lock().await;
            if let Some(pos) = state_lock.get(&watcher_symbol).cloned() {
                // Scenario 1: Entry order is pending fill, TP not placed yet
                if pos.tp_order_id.is_empty() && !pos.order_id.is_empty() {
                    if let Ok(order_info) = watcher_binance.fetch_order(&watcher_symbol, &pos.order_id).await {
                        let status = order_info.get("status").and_then(|s| s.as_str()).unwrap_or("");
                        let executed_qty = order_info.get("executedQty")
                            .and_then(|q| q.as_str())
                            .and_then(|s| s.parse::<f64>().ok())
                            .unwrap_or(0.0);

                        let mut should_place_tp = false;
                        let mut final_fill_qty = executed_qty;

                        if status == "FILLED" {
                            println!("🟢 [FAST-SENTINEL] Entry Filled for {}! (Qty: {:.5}) Placing resting Take Profit at ${:.2}", watcher_symbol, executed_qty, pos.take_profit_price);
                            should_place_tp = true;
                            final_fill_qty = if executed_qty > 0.0 { executed_qty } else { pos.amount };
                        } else if status == "PARTIALLY_FILLED" {
                            let elapsed_secs = chrono::DateTime::parse_from_rfc3339(&pos.opened_at)
                                .map(|dt| (chrono::Utc::now() - dt.with_timezone(&chrono::Utc)).num_seconds())
                                .unwrap_or(0);

                            if elapsed_secs >= 60 {
                                println!("⏱️ [FAST-SENTINEL] Entry for {} partially filled ({:.5} / {:.5}) after {}s (>= 60s). Cancelling unfilled remainder and placing TP.",
                                         watcher_symbol, executed_qty, pos.amount, elapsed_secs);
                                let _ = watcher_binance.cancel_order(&watcher_symbol, &pos.order_id).await;
                                if let Ok(cancel_info) = watcher_binance.fetch_order(&watcher_symbol, &pos.order_id).await {
                                    if let Some(post_cancel_qty) = cancel_info.get("executedQty").and_then(|q| q.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                                        final_fill_qty = post_cancel_qty;
                                    }
                                }
                                should_place_tp = true;
                            }
                        } else if status == "CANCELED" || status == "EXPIRED" {
                            if executed_qty > 0.0 {
                                println!("⚠️ [FAST-SENTINEL] Entry for {} was {} with partial fills ({:.5}). Placing TP.", watcher_symbol, status, executed_qty);
                                should_place_tp = true;
                            } else {
                                println!("⚪ [FAST-SENTINEL] Entry for {} was {} with 0 fills. Cleaning up state.", watcher_symbol, status);
                                state_lock.remove(&watcher_symbol);
                                let _ = crate::state::save_state(&watcher_state_path, &state_lock);
                                crate::state::release_cluster_lease(&watcher_symbol);
                            }
                        } else if status == "REJECTED" {
                            println!("❌ [FAST-SENTINEL] Entry for {} was REJECTED. Cleaning up state.", watcher_symbol);
                            state_lock.remove(&watcher_symbol);
                            let _ = crate::state::save_state(&watcher_state_path, &state_lock);
                            crate::state::release_cluster_lease(&watcher_symbol);
                        }

                        if should_place_tp && final_fill_qty > 0.0 {
                            // Reconcile actual executed quantity against base asset free balance to defeat the gross-vs-net fee trap
                            let base_asset = watcher_symbol.replace("USDC", "").replace("/", "");
                            let free_balance = watcher_binance.fetch_free_balance(&base_asset).await.unwrap_or(final_fill_qty);
                            
                            let precision = get_symbol_lot_precision(&watcher_symbol);
                            let sell_target = f64::min(final_fill_qty, free_balance);
                            let net_sell_amount = quantize_lot_size(sell_target, precision);

                            if net_sell_amount > 0.0 {
                                match watcher_binance.create_limit_sell(&watcher_symbol, net_sell_amount, pos.take_profit_price).await {
                                    Ok(sell_res) => {
                                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                            let tp_id = sell_res.get("orderId").and_then(|id| {
                                                if id.is_u64() { Some(id.as_u64().unwrap().to_string()) }
                                                else if id.is_string() { Some(id.as_str().unwrap().to_string()) }
                                                else { None }
                                            }).unwrap_or_default();

                                            if !tp_id.is_empty() {
                                                p.tp_order_id = tp_id;
                                                p.amount = net_sell_amount;
                                                p.is_filled = true;
                                                p.opened_at = chrono::Utc::now().to_rfc3339(); // Time-stop clock starts at actual fill!
                                                let _ = crate::state::save_state(&watcher_state_path, &state_lock);
                                                let msg = format!("🟢 **TAKE PROFIT PLACED:** {}\n**Target:** ${:.2}\n**Net Sellable Size:** {:.5}", watcher_symbol, pos.take_profit_price, net_sell_amount);
                                                let _ = crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        eprintln!("❌ [FAST-SENTINEL] Failed to place TP limit sell for {}: {}", watcher_symbol, e);
                                    }
                                }
                            } else {
                                println!("⚠️ [FAST-SENTINEL] Fill quantity {:.5} on {} is below min lot size. Releasing state.", final_fill_qty, watcher_symbol);
                                state_lock.remove(&watcher_symbol);
                                let _ = crate::state::save_state(&watcher_state_path, &state_lock);
                                crate::state::release_cluster_lease(&watcher_symbol);
                            }
                        }
                    }
                }
                // Scenario 2: TP Limit Sell is resting on book. Check if filled
                else if !pos.tp_order_id.is_empty() {
                    if let Ok(order_info) = watcher_binance.fetch_order(&watcher_symbol, &pos.tp_order_id).await {
                        if order_info["status"] == "FILLED" {
                            println!("🎉 [FAST-SENTINEL] Take Profit Limit Sell for {} has FILLED!", watcher_symbol);
                            state_lock.remove(&watcher_symbol);
                            let _ = crate::state::save_state(&watcher_state_path, &state_lock);
                            crate::state::release_cluster_lease(&watcher_symbol);
                            
                            // Reset circuit breaker on successful Take Profit win
                            let mut cb_lock = watcher_cb.lock().await;
                            cb_lock.record_win();
                            save_circuit_breaker(&watcher_cb_path, &cb_lock);

                            let msg = format!("🎉 **TRADE CLOSED IN PROFIT!** {} Take Profit Limit Sell has successfully filled. Ready for next cycle.", watcher_symbol);
                            let _ = crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                        }
                    }
                }
            }
        }
    });

    let ws_url = format!("wss://stream.binance.com:9443/ws/{}@kline_1m", trading_symbol.to_lowercase());

    loop {
        let ws_stream = match connect_async(&ws_url).await {
            Ok((stream, _)) => stream,
            Err(_e) => { tokio::time::sleep(Duration::from_secs(5)).await; continue; }
        };
        
        let (_, mut read) = ws_stream.split();
        
        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    let parsed: Value = serde_json::from_str(&text).unwrap_or_default();
                    if let Some(kline) = parsed.get("k") {
                        let is_closed = kline.get("x").and_then(|x| x.as_bool()).unwrap_or(false);
                        let close_price = kline.get("c").and_then(|c| c.as_str()).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                        
                        if is_closed {
                            // ------------------------------------------------------------------
                            // In-Sample Signal Leakage Prevention:
                            // Estimate parameters strictly on historical bars through t-1.
                            // ------------------------------------------------------------------
                            let closes_clone = rolling_closes.clone();
                            let binance_clone = binance.clone();
                            let state_clone = active_state.clone();
                            let discord_clone = discord_url.clone();
                            let symbol_clone = trading_symbol.clone();
                            let state_path_clone = state_file_path.clone();
                            let cb_clone = active_cb.clone();
                            let cb_path_clone = cb_file_path.clone();
                            let arm_path_clone = arm_file_path.clone();
                            
                            // Pure CPU computation in blocking thread, evaluating historical closes up to t-1
                            let current_close = close_price;
                            let calc_res = if closes_clone.len() >= 100 {
                                tokio::task::spawn_blocking(move || {
                                    let hist_last_price = *closes_clone.last().unwrap_or(&0.0);
                                    // Ornstein-Uhlenbeck Process Estimation
                                    let (thresholds_opt, half_life_m) = if let Some(ou_params) = estimate_ou_parameters(&closes_clone, 1.0) {
                                        let hl = ou_params.half_life_minutes();
                                        let tx_cost = hist_last_price * 0.001;
                                        let th = calculate_optimal_thresholds(&ou_params, tx_cost);
                                        (Some(th), hl)
                                    } else {
                                        (None, 999.0)
                                    };

                                    // OLS Slope drift calculation on historical window
                                    let n_drift = closes_clone.len() as f64;
                                    let mut sum_i = 0.0;
                                    let mut sum_p = 0.0;
                                    let mut sum_ip = 0.0;
                                    let mut sum_ii = 0.0;
                                    for (i, &p) in closes_clone.iter().enumerate() {
                                        let idx = i as f64;
                                        sum_i += idx;
                                        sum_p += p;
                                        sum_ip += idx * p;
                                        sum_ii += idx * idx;
                                    }
                                    let slope = (n_drift * sum_ip - sum_i * sum_p) / (n_drift * sum_ii - sum_i * sum_i);
                                    let total_drift_pct = (slope * n_drift) / hist_last_price;
                                    let is_drifting = total_drift_pct.abs() > 0.015;

                                    // Standardized Short-Horizon Downside Shock Calculation (Z15 & Z60 with robust MAD)
                                    let n = closes_clone.len();
                                    let mut z_15 = 0.0;
                                    let mut z_60 = 0.0;
                                    if n >= 65 && current_close > 0.0 {
                                        let mut returns_1m = Vec::with_capacity(n - 1);
                                        for i in 1..n {
                                            returns_1m.push((closes_clone[i] / closes_clone[i-1]).ln());
                                        }
                                        // Robust Median Absolute Deviation (MAD) scaled by 1.4826 for normal consistency
                                        let sigma_mad = calculate_mad_sigma(&returns_1m);

                                        // Live return evaluated with current completed bar close_price vs historical closes
                                        let ret_15 = (current_close / closes_clone[n - 15]).ln();
                                        z_15 = ret_15 / (sigma_mad * (15.0_f64).sqrt());

                                        let ret_60 = (current_close / closes_clone[n - 60]).ln();
                                        z_60 = ret_60 / (sigma_mad * (60.0_f64).sqrt());
                                    }
                                    let is_shock = z_15 < -3.0 || z_60 < -3.0;

                                    Some((thresholds_opt, total_drift_pct, is_drifting, half_life_m, z_15, z_60, is_shock, hist_last_price))
                                }).await.unwrap_or(None)
                            } else {
                                None
                            };

                            if let Some((thresholds_opt, total_drift_pct, is_drifting, half_life_m, z_15, z_60, is_shock, hist_last_price)) = calc_res {
                                let regime = 0; // HMM regime
                                let is_stationary = thresholds_opt.is_some();
                                let is_consolidation = regime == 0 && !is_drifting && is_stationary;

                                // Half-Life Eligibility Gate: 15m <= t_half <= 120m
                                let is_valid_half_life = half_life_m >= 15.0 && half_life_m <= 120.0;
                                
                                let cb_lock = cb_clone.lock().await;
                                let is_cb_paused = cb_lock.is_paused();
                                drop(cb_lock);

                                // Portfolio Cluster Check (only 1 active position allowed across BTC, ETH, SOL)
                                let cluster_active = has_active_cluster_position(&symbol_clone);
                                let is_cluster_blocked = cluster_active.is_some();

                                let regime_str = if !is_stationary {
                                    "Trending (Non-Stationary: AR(1) b >= 1.0)"
                                } else if !is_consolidation {
                                    if is_drifting { "Trending (Drift)" } else { "Trending (HMM)" }
                                } else if is_cb_paused {
                                    "Consolidation (Vetoed: Circuit Breaker 24h Pause)"
                                } else if is_shock {
                                    "Consolidation (Vetoed: Shock Cascade)"
                                } else if is_cluster_blocked {
                                    "Consolidation (Vetoed: Cluster Active)"
                                } else if !is_valid_half_life {
                                    if half_life_m < 15.0 {
                                        "Consolidation (Vetoed: t_half < 15m noise)"
                                    } else {
                                        "Consolidation (Vetoed: t_half > 120m slow)"
                                    }
                                } else {
                                    "Consolidation (Tradable)"
                                };

                                let target_buy_str = match thresholds_opt {
                                    Some(ref th) => format!("${:.2}", th.entry_long),
                                    None => "N/A (b >= 1.0)".to_string(),
                                };

                                println!("💓 [HOT-PATH] {} Price: ${:.2} | Target Buy: {} | Regime: {} | Drift: {:.2}%/window | t_half: {:.1}m | Z15: {:.2} | Z60: {:.2}", 
                                         symbol_clone, close_price, target_buy_str, regime_str, total_drift_pct * 100.0, half_life_m, z_15, z_60);
                                
                                let mut state_lock = state_clone.lock().await;
                                let has_position = state_lock.contains_key(&symbol_clone);

                                if has_position {
                                    // Make sure armed state is cleared if holding a position
                                    let mut armed_state = load_armed_signal(&arm_path_clone);
                                    if armed_state.is_armed {
                                        armed_state.is_armed = false;
                                        save_armed_signal(&arm_path_clone, &armed_state);
                                    }

                                    // Continuously refresh cluster lease while position is active
                                    if !crate::state::refresh_cluster_lease(&symbol_clone, 3600) {
                                        eprintln!("⚠️ [LEASE REFRESH] Could not refresh cluster lease for {}. Re-evaluating lease lock.", symbol_clone);
                                    }

                                    // Check resting entry TTL (5m) and filled position 2 * t_half Time-Stop
                                    let mut entry_ttl_expired = false;
                                    let mut time_stop_triggered = false;
                                    let mut giveback_triggered = false;
                                    let mut elapsed_mins = 0.0;
                                    let mut max_allowed_mins = 120.0;

                                    let mut should_reprice = false;
                                    let mut new_reprice_target = 0.0;
                                    let mut reprice_qty = 0.0;
                                    let mut old_tp_order_id = String::new();

                                    if let Some(pos) = state_lock.get_mut(&symbol_clone) {
                                        if let Ok(opened_dt) = chrono::DateTime::parse_from_rfc3339(&pos.opened_at) {
                                            elapsed_mins = (chrono::Utc::now() - opened_dt.with_timezone(&chrono::Utc)).num_seconds() as f64 / 60.0;
                                            if !pos.is_filled {
                                                // Entry maker buy order resting TTL: 5 minutes
                                                if elapsed_mins >= 5.0 {
                                                    entry_ttl_expired = true;
                                                }
                                            } else {
                                                // Filled position holding horizon: 2 * t_half
                                                max_allowed_mins = if pos.half_life_minutes > 0.0 {
                                                    (2.0 * pos.half_life_minutes).clamp(15.0, 360.0)
                                                } else {
                                                    120.0
                                                };
                                                if elapsed_mins >= max_allowed_mins {
                                                    time_stop_triggered = true;
                                                }

                                                // -------------------------------------------------------------
                                                // Normalized Progress & 20% Giveback Ratchet (Researcher 2)
                                                // -------------------------------------------------------------
                                                let d_0 = pos.take_profit_price - pos.entry_price;
                                                if d_0 > 0.0 {
                                                    let q_t = (close_price - pos.entry_price) / d_0;
                                                    if q_t > pos.max_progress {
                                                        pos.max_progress = q_t;
                                                        pos.max_favorable_price = close_price;
                                                    }

                                                    // Arm Profit Protection once trade covers >= 75% of target
                                                    // Trigger profit-locked exit if price retraces by 20% giveback
                                                    let q_be = 0.15; // covers ~15-20 bps roundtrip fees + slippage
                                                    if pos.max_progress >= 0.75 && q_t <= f64::max(q_be, pos.max_progress - 0.20) {
                                                        giveback_triggered = true;
                                                    }

                                                    // -------------------------------------------------------------
                                                    // One-Way Adaptive Downward Ratchet (Causal Live Mean Tracking)
                                                    // -------------------------------------------------------------
                                                    if let Some(ref th) = thresholds_opt {
                                                        let live_mu = th.exit_long;
                                                        let eps_shave = (live_mu * 0.0005).max(0.02);
                                                        let b_cand = live_mu - eps_shave;
                                                        let p_min = pos.entry_price * 1.0020; // Economic floor: >= 20 bps gross profit
                                                        let delta_reprice = (pos.entry_price * 0.0005).max(0.10 * d_0).max(0.03);

                                                        if b_cand <= pos.effective_tp - delta_reprice && b_cand >= p_min {
                                                            pos.candidate_lower_count += 1;
                                                            let now_ms = chrono::Utc::now().timestamp_millis();
                                                            let cooldown_ok = (now_ms - pos.last_reprice_time) >= 180_000; // 3 min
                                                            if pos.candidate_lower_count >= 3 && cooldown_ok && pos.reprice_count < 3 {
                                                                should_reprice = true;
                                                                new_reprice_target = b_cand;
                                                                reprice_qty = pos.amount;
                                                                old_tp_order_id = pos.tp_order_id.clone();
                                                                pos.effective_tp = b_cand;
                                                                pos.last_reprice_time = now_ms;
                                                                pos.reprice_count += 1;
                                                                pos.candidate_lower_count = 0;
                                                            }
                                                        } else if b_cand > pos.effective_tp {
                                                            // Never ratchet upward! Reset persistence count
                                                            pos.candidate_lower_count = 0;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    let _ = crate::state::save_state(&state_path_clone, &state_lock);

                                    // Handle Adaptive One-Way Reprice Execution
                                    if should_reprice && new_reprice_target > 0.0 && reprice_qty > 0.0 {
                                        let rounded_target = (new_reprice_target * 100.0).round() / 100.0;
                                        println!("🔄 [ONE-WAY RATCHET REPRICE] {} live mean dropped. Repricing TP order to ${:.2} (Attempt)...", 
                                                 symbol_clone, rounded_target);
                                        if !old_tp_order_id.is_empty() {
                                            let _ = binance_clone.cancel_order(&symbol_clone, &old_tp_order_id).await;
                                        }
                                        match binance_clone.create_limit_sell(&symbol_clone, reprice_qty, rounded_target).await {
                                            Ok(sell_res) => {
                                                let new_tp_id = sell_res.get("orderId").and_then(|id| {
                                                    if id.is_u64() { Some(id.as_u64().unwrap().to_string()) }
                                                    else if id.is_string() { Some(id.as_str().unwrap().to_string()) }
                                                    else { None }
                                                }).unwrap_or_default();
                                                if !new_tp_id.is_empty() {
                                                    if let Some(p) = state_lock.get_mut(&symbol_clone) {
                                                        p.tp_order_id = new_tp_id;
                                                        p.take_profit_price = rounded_target;
                                                        let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                                    }
                                                    let reprice_msg = format!("🔄 **TP ADAPTIVELY REPRICED:** {}\n**New Target:** ${:.2}\n**Live Mean Ratchet Active**", symbol_clone, rounded_target);
                                                    crate::discord::send_alert(discord_clone.as_deref(), &reprice_msg).await;
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!("❌ [REPRICE FAILED] Could not replace TP order for {}: {}", symbol_clone, e);
                                            }
                                        }
                                    }

                                    if entry_ttl_expired {
                                        // ⏱️ ENTRY TTL EXPIRED: Unfilled resting maker order cancelled after 5 minutes
                                        println!("⏱️ [ENTRY TTL EXPIRED] Resting limit buy for {} unfilled after {:.1}m. Cancelling order to release cluster.", symbol_clone, elapsed_mins);
                                        let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                    } else if giveback_triggered {
                                        // 🏆 PROFIT RATCHET EXECUTED: 20% Giveback from peak progress triggered
                                        let peak_prog = state_lock.get(&symbol_clone).map(|p| p.max_progress * 100.0).unwrap_or(0.0);
                                        let ent_px = state_lock.get(&symbol_clone).map(|p| p.entry_price).unwrap_or(0.0);
                                        println!("🏆 [PROFIT RATCHET EXECUTED] {} peak progress was {:.1}%. Retracement triggered 20% giveback exit at ${:.2}. Banking profit!", 
                                                 symbol_clone, peak_prog, close_price);
                                        let ratchet_msg = format!("🏆 **PROFIT RATCHET EXECUTED:** {} reached {:.1}% peak progress. Retracement triggered profit lock at ${:.2} (Entry: ${:.2}). Neutralizing risk.", 
                                                                 symbol_clone, peak_prog, close_price, ent_px);
                                        crate::discord::send_alert(discord_clone.as_deref(), &ratchet_msg).await;
                                        let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                    } else if is_shock {
                                        // 🚨 SHOCK CASCADE ON ACTIVE POSITION
                                        println!("🚨 [SHOCK CASCADE ON ACTIVE POSITION] Severe downside shock detected on {} (Z15: {:.2}, Z60: {:.2})! Flattening.", symbol_clone, z_15, z_60);
                                        let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                    } else if !is_consolidation {
                                        // 🚨 INVALIDATION PROTOCOL: Stationarity broken
                                        println!("🚨 [INVALIDATION] Regime shift / drift detected for {}. Executing emergency flatten.", symbol_clone);
                                        let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                    } else if time_stop_triggered {
                                        // ⏱️ TIME-STOP PROTOCOL: 2 * t_half elapsed
                                        println!("⏱️ [TIME-STOP] {} reached max holding horizon ({:.1}m >= limit {:.1}m). Statistical edge expired. Flattening.", symbol_clone, elapsed_mins, max_allowed_mins);
                                        
                                        // Record strike on Circuit Breaker
                                        let mut cb_lock = cb_clone.lock().await;
                                        let tripped = cb_lock.record_time_stop();
                                        save_circuit_breaker(&cb_path_clone, &cb_lock);

                                        let mut time_msg = format!("⏱️ **HALF-LIFE TIME-STOP EXPIRED:** {} held for {:.1}m (limit: {:.1}m). Statistical mean-reversion thesis expired; liquidating position to release capital.", symbol_clone, elapsed_mins, max_allowed_mins);
                                        if tripped {
                                            time_msg.push_str("\n🚨 **CIRCUIT BREAKER TRIGGERED:** 3 consecutive time-stop exits! Trading paused on this symbol for 24 hours.");
                                            eprintln!("🚨 [CIRCUIT BREAKER] 3 consecutive time-stops on {}! Halting symbol for 24 hours.", symbol_clone);
                                        }
                                        crate::discord::send_alert(discord_clone.as_deref(), &time_msg).await;
                                        let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                    }
                                } 
                                else if !has_position {
                                    // ------------------------------------------------------------------
                                    // REVERSAL-CONFIRMED ENTRY PROTOCOL (Arm -> Bounce -> Buy)
                                    // ------------------------------------------------------------------
                                    let mut armed_state = load_armed_signal(&arm_path_clone);
                                    let is_eligible = is_consolidation && is_valid_half_life && !is_cb_paused && !is_shock && !is_cluster_blocked;

                                    if armed_state.is_armed {
                                        // Update lowest dip price recorded during this downward stretch
                                        if close_price < armed_state.lowest_price {
                                            armed_state.lowest_price = close_price;
                                            save_armed_signal(&arm_path_clone, &armed_state);
                                        }

                                        // Dynamic arm timeout: clamp(0.5 * half_life, 10m, 30m)
                                        let arm_timeout_mins = (0.5 * armed_state.half_life_m).clamp(10.0, 30.0);
                                        let mut expired = false;
                                        if let Some(ref at_ts) = armed_state.armed_at {
                                            if let Ok(at_dt) = chrono::DateTime::parse_from_rfc3339(at_ts) {
                                                let elapsed = (chrono::Utc::now() - at_dt.with_timezone(&chrono::Utc)).num_seconds() as f64 / 60.0;
                                                if elapsed >= arm_timeout_mins {
                                                    expired = true;
                                                }
                                            }
                                        }

                                        // Disarm if shock occurred, circuit breaker paused, regime broke, cluster became active, or timed out
                                        if !is_eligible || expired {
                                            println!("⚪ [DISARMED] {} signal disarmed (Shock: {}, Regime broke: {}, Cluster: {}, Expired: {})", 
                                                     symbol_clone, is_shock, !is_consolidation, is_cluster_blocked, expired);
                                            armed_state.is_armed = false;
                                            save_armed_signal(&arm_path_clone, &armed_state);
                                        } 
                                        else {
                                            // 🟢 REVERSAL CONFIRMATION:
                                            // 1. Current 1m bar closed green (reversal onset): close_price > hist_last_price
                                            // 2. Price bounced off the dip low: close_price >= armed_state.lowest_price * 1.0005 || close_price >= armed_state.entry_threshold
                                            // 3. Price is still attractive below dynamically translated mean exit target
                                            let live_sma = rolling_closes.iter().sum::<f64>() / rolling_closes.len() as f64;
                                            let dynamic_exit_target = if armed_state.unitless_exit_dev != 0.0 {
                                                live_sma * armed_state.unitless_exit_dev.exp()
                                            } else {
                                                armed_state.exit_threshold
                                            };
                                            let dynamic_entry_cap = if armed_state.unitless_entry_dev != 0.0 {
                                                live_sma * armed_state.unitless_entry_dev.exp()
                                            } else {
                                                armed_state.entry_threshold
                                            };

                                            let is_green_bounce = close_price > hist_last_price;
                                            let bounced_off_low = close_price >= armed_state.lowest_price * 1.0005 || close_price >= dynamic_entry_cap;
                                            let still_below_exit = close_price < dynamic_exit_target;

                                            if is_green_bounce && bounced_off_low && still_below_exit {
                                                let entry_limit_price = f64::min(close_price, dynamic_entry_cap);
                                                let net_edge_bps = calculate_net_edge_bps(entry_limit_price, dynamic_exit_target);

                                                // Economic edge hurdle: ensure net expected return >= 25 bps (c_roundtrip ~ 15-20 bps + safety margin)
                                                if net_edge_bps < 25.0 {
                                                    println!("⚠️ [ECONOMIC HURDLE VETO] Net edge too narrow for {}: {:.1} bps (< 25.0 bps hurdle). Dynamic exit: ${:.2}, Entry limit: ${:.2}. Disarming signal.", 
                                                             symbol_clone, net_edge_bps, dynamic_exit_target, entry_limit_price);
                                                    armed_state.is_armed = false;
                                                    save_armed_signal(&arm_path_clone, &armed_state);
                                                } else {
                                                    println!("🟢 [REVERSAL CONFIRMED] {} reversed from dip low ${:.2} up to ${:.2} (prev: ${:.2})! Edge: {:.1} bps. Submitting maker buy (Live SMA: ${:.2}, Dynamic TP μ: ${:.2}).", 
                                                             symbol_clone, armed_state.lowest_price, close_price, hist_last_price, net_edge_bps, live_sma, dynamic_exit_target);
                                                    
                                                    let frozen_hl = armed_state.half_life_m;
                                                    armed_state.is_armed = false;
                                                    save_armed_signal(&arm_path_clone, &armed_state);

                                                    if let Err(e) = place_ou_limit_buy(
                                                        &state_path_clone,
                                                        &binance_clone, 
                                                        &mut state_lock, 
                                                        &symbol_clone, 
                                                        entry_limit_price, 
                                                        dynamic_exit_target, 
                                                        frozen_hl, 
                                                        discord_clone
                                                    ).await {
                                                        eprintln!("❌ [ORDER ERROR] Failed to dispatch limit maker buy for {}: {}", symbol_clone, e);
                                                    }
                                                }
                                            }
                                        }
                                    } 
                                    else if is_eligible {
                                        if let Some(ref thresholds) = thresholds_opt {
                                            // Stage 1: Arm signal when price touches or dips below threshold
                                            if close_price <= thresholds.entry_long {
                                                let baseline_arm = rolling_closes.iter().sum::<f64>() / rolling_closes.len() as f64;
                                                let unitless_entry_dev = (thresholds.entry_long / baseline_arm).ln();
                                                let unitless_exit_dev = (thresholds.exit_long / baseline_arm).ln();

                                                armed_state.is_armed = true;
                                                armed_state.armed_at = Some(chrono::Utc::now().to_rfc3339());
                                                armed_state.entry_threshold = thresholds.entry_long;
                                                armed_state.exit_threshold = thresholds.exit_long;
                                                armed_state.unitless_entry_dev = unitless_entry_dev;
                                                armed_state.unitless_exit_dev = unitless_exit_dev;
                                                armed_state.half_life_m = half_life_m;
                                                armed_state.lowest_price = close_price;
                                                save_armed_signal(&arm_path_clone, &armed_state);

                                                println!("🟡 [SIGNAL ARMED] {} touched entry threshold (${:.2} <= ${:.2}). Signal armed (Unitless Exit dev: {:.4}, t_half: {:.1}m). Waiting for green reversal bounce confirmation before buying.", 
                                                         symbol_clone, close_price, thresholds.entry_long, unitless_exit_dev, half_life_m);
                                            }
                                        }
                                    }
                                }
                            }

                            // Advance rolling window: append completed bar t for future bar calibrations
                            if rolling_closes.len() >= 480 { rolling_closes.remove(0); }
                            rolling_closes.push(close_price);
                        }
                    }
                },
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
