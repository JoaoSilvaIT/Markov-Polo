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
use crate::ou_process::{estimate_ou_parameters, calculate_optimal_thresholds};
use crate::order::{place_ou_limit_buy, emergency_flatten};
use crate::state::{StateMap, load_state};

#[tokio::main]
async fn main() {
    println!("🚀 [SYSTEM START] Fully Autonomous Deterministic O-U Architecture Initialized.");
    
    let config = Config::from_env();
    let binance = Arc::new(BinanceClient::new(config.binance_api_key.clone(), config.binance_secret_key.clone()));
    
    // Load current position state
    let state_map: StateMap = load_state(&config.state_file_path).unwrap_or_default();
    let active_state = Arc::new(Mutex::new(state_map));
    let state_file_path = config.state_file_path.clone();
    
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
    if let Ok(klines) = binance.fetch_ohlcv(&trading_symbol, "1m", 480).await {
        for k in klines {
            if let Some(c) = k[4].as_str().and_then(|s| s.parse::<f64>().ok()) {
                rolling_closes.push(c);
            }
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

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        loop {
            interval.tick().await;

            let mut state_lock = watcher_state.lock().await;
            if let Some(pos) = state_lock.get(&watcher_symbol).cloned() {
                // Scenario 1: Entry order is pending fill, TP not placed yet
                if pos.tp_order_id.is_empty() && !pos.order_id.is_empty() {
                    if let Ok(order_info) = watcher_binance.fetch_order(&watcher_symbol, &pos.order_id).await {
                        if order_info["status"] == "FILLED" {
                            println!("🟢 [FAST-SENTINEL] Entry Filled for {}! Calculating net sellable quantity and placing resting Take Profit at ${:.2}", watcher_symbol, pos.take_profit_price);
                            
                            // Reconcile actual executed quantity against base asset free balance to defeat the gross-vs-net fee trap
                            let executed_qty = order_info.get("executedQty")
                                .and_then(|q| q.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(pos.amount);
                            
                            let base_asset = watcher_symbol.replace("USDC", "").replace("/", "");
                            let free_balance = watcher_binance.fetch_free_balance(&base_asset).await.unwrap_or(executed_qty);
                            
                            let precision = get_symbol_lot_precision(&watcher_symbol);
                            let sell_target = f64::min(executed_qty, free_balance);
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
                            
                            // Pure CPU computation in blocking thread, evaluating historical closes up to t-1
                            let calc_res = if closes_clone.len() >= 100 {
                                tokio::task::spawn_blocking(move || {
                                    let hist_last_price = *closes_clone.last().unwrap_or(&0.0);
                                    if let Some(ou_params) = estimate_ou_parameters(&closes_clone, 1.0) {
                                        // Binance Spot taker fee is ~0.10%. Absolute dollar transaction cost on historical price.
                                        let tx_cost = hist_last_price * 0.001;
                                        let thresholds = calculate_optimal_thresholds(&ou_params, tx_cost);
                                        
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

                                        Some((thresholds, total_drift_pct, is_drifting))
                                    } else {
                                        None
                                    }
                                }).await.unwrap_or(None)
                            } else {
                                None
                            };

                            if let Some((thresholds, total_drift_pct, is_drifting)) = calc_res {
                                let regime = 0; // HMM regime
                                let is_consolidation = regime == 0 && !is_drifting;
                                
                                let regime_str = if is_consolidation { "Consolidation" } else if is_drifting { "Trending (Drift)" } else { "Trending (HMM)" };
                                println!("💓 [HOT-PATH] {} Price: ${:.2} | Target Buy: ${:.2} | Regime: {} | Drift: {:.2}%/window", symbol_clone, close_price, thresholds.entry_long, regime_str, total_drift_pct * 100.0);
                                
                                let mut state_lock = state_clone.lock().await;
                                let has_position = state_lock.contains_key(&symbol_clone);

                                if !is_consolidation && has_position {
                                    // 🚨 INVALIDATION PROTOCOL: Stationarity broken
                                    let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                } 
                                else if is_consolidation && !has_position {
                                    // 🟢 O-U ENTRY SIGNAL evaluated strictly out-of-sample on bar t
                                    if close_price <= thresholds.entry_long {
                                        let _ = place_ou_limit_buy(
                                            &state_path_clone,
                                            &binance_clone, 
                                            &mut state_lock, 
                                            &symbol_clone, 
                                            thresholds.entry_long, 
                                            thresholds.exit_long, 
                                            discord_clone
                                        ).await;
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
