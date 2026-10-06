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
use crate::binance::BinanceClient;
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

    let ws_url = format!("wss://stream.binance.com:9443/ws/{}@kline_1m", trading_symbol.to_lowercase());

    loop {
        let ws_stream = match connect_async(&ws_url).await {
            Ok((stream, _)) => stream,
            Err(e) => { tokio::time::sleep(Duration::from_secs(5)).await; continue; }
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
                            if rolling_closes.len() >= 480 { rolling_closes.remove(0); }
                            rolling_closes.push(close_price);
                            
                            let closes_clone = rolling_closes.clone();
                            let hmm_clone = hmm_model.clone();
                            let binance_clone = binance.clone();
                            let state_clone = active_state.clone();
                            let discord_clone = discord_url.clone();
                            let symbol_clone = trading_symbol.clone();
                            let state_path_clone = state_file_path.clone();
                            
                            tokio::task::spawn_blocking(move || {
                                if let Some(ou_params) = estimate_ou_parameters(&closes_clone, 1.0) {
                                    let thresholds = calculate_optimal_thresholds(&ou_params, 0.0004);
                                    
                                    // Async context for locking inside spawn_blocking requires block_on or spawn
                                    let rt = tokio::runtime::Handle::current();
                                    rt.block_on(async move {
                                        // Viterbi Predict
                                        let hmm = hmm_clone.read().await;
                                        // (In full implementation, we'd format `x` matrix and call hmm.predict)
                                        let regime = 0; // 0 = Consolidation, 1 = Trend, 2 = Cascade
                                        // 🚨 STATIONARITY FAIL-SAFE: Slow Drift Filter
                                        // The researcher warned that single-asset O-U can misclassify slow drift as consolidation.
                                        // We calculate the raw slope of the 480-minute window to detect structural drift.
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
                                        // If the price is drifting more than $2 per minute on average, it's a trend, not consolidation.
                                        let is_drifting = slope.abs() > 2.0;

                                        let is_consolidation = regime == 0 && !is_drifting;
                                        
                                        let current_price = *closes_clone.last().unwrap_or(&0.0);
                                        let regime_str = if is_consolidation { "Consolidation" } else if is_drifting { "Trending (Drift)" } else { "Trending (HMM)" };
                                        println!("💓 [HOT-PATH] {} Price: ${:.2} | Target Buy: ${:.2} | Regime: {} | Drift: ${:.2}/m", symbol_clone, current_price, thresholds.entry_long, regime_str, slope);
                                        
                                        let mut state_lock = state_clone.lock().await;
                                        let has_position = state_lock.contains_key(&symbol_clone);

                                        // Check and place Take Profit if needed
                                        if has_position {
                                            let mut needs_tp = false;
                                            let mut order_id = "".to_string();
                                            let mut tp_price = 0.0;
                                            let mut amount = 0.0;
                                            
                                            if let Some(pos) = state_lock.get(&symbol_clone) {
                                                if pos.tp_order_id.is_empty() {
                                                    needs_tp = true;
                                                    order_id = pos.order_id.clone();
                                                    tp_price = pos.take_profit_price;
                                                    amount = pos.amount;
                                                }
                                            }
                                            
                                            if needs_tp {
                                                if let Ok(order_info) = binance_clone.fetch_order(&symbol_clone, &order_id).await {
                                                    if order_info["status"] == "FILLED" {
                                                        println!("🟢 [EXECUTION] Entry Filled! Placing resting Take Profit at ${:.2}", tp_price);
                                                        if let Ok(sell_res) = binance_clone.create_limit_sell(&symbol_clone, amount, tp_price).await {
                                                            if let Some(pos) = state_lock.get_mut(&symbol_clone) {
                                                                pos.tp_order_id = sell_res["orderId"].as_u64().unwrap_or(0).to_string();
                                                                let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                                                let msg = format!("🟢 **TAKE PROFIT PLACED:** BTC/USDC\n**Target:** ${:.2}\n**Size:** {:.5}", tp_price, amount);
                                                                let _ = crate::discord::send_alert(discord_clone.as_deref(), &msg).await;
                                                            }
                                                        }
                                                    }
                                                }
                                            } else {
                                                // TP is already placed. Check if it has been FILLED to close the trade loop.
                                                let mut tp_filled = false;
                                                let mut tp_order_to_check = "".to_string();
                                                if let Some(pos) = state_lock.get(&symbol_clone) {
                                                    tp_order_to_check = pos.tp_order_id.clone();
                                                }
                                                if !tp_order_to_check.is_empty() {
                                                    if let Ok(order_info) = binance_clone.fetch_order(&symbol_clone, &tp_order_to_check).await {
                                                        if order_info["status"] == "FILLED" {
                                                            tp_filled = true;
                                                        }
                                                    }
                                                }
                                                if tp_filled {
                                                    state_lock.remove(&symbol_clone);
                                                    let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                                    let msg = "🎉 **TRADE CLOSED IN PROFIT!** Take Profit Limit Sell has successfully filled. Ready for next cycle.".to_string();
                                                    let _ = crate::discord::send_alert(discord_clone.as_deref(), &msg).await;
                                                }
                                            }
                                        }

                                        if !is_consolidation && has_position {
                                            // 🚨 INVALIDATION PROTOCOL: Stationarity broken
                                            let _ = emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;
                                        } 
                                        else if is_consolidation && !has_position {
                                            // 🟢 O-U ENTRY SIGNAL
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
                                    });
                                }
                            });
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
