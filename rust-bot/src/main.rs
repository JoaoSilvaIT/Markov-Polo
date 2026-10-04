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

use crate::config::Config;
use crate::binance::BinanceClient;
use crate::state::{load_state, save_state};
use chrono::{DateTime, Utc, Timelike};
use std::time::Duration;
use tokio::sync::Mutex;
use std::sync::Arc;
use serde_json::{json, Value};
#[tokio::main]
async fn main() {
    let config = Config::from_env();
    let binance = Arc::new(BinanceClient::new(config.binance_api_key.clone(), config.binance_secret_key.clone()));
    let gemini = Arc::new(gemini::GeminiClient::new().await);
    
    let state = match load_state() {
        Ok(s) => s,
        Err(_) => std::collections::HashMap::new(),
    };
    let state = Arc::new(Mutex::new(state));

    println!("\n🚀 [SYSTEM START] Rust Decoupled Quantitative Architecture Initialized.");
    println!("🛡️ [SENTINEL DAEMON] Risk Management Active (15s Loop)");
    println!("🧠 [ALPHA DAEMON] AI Trade Entry Armed (Locked to :00, :15, :30, :45)");

    // Sentinel Daemon
    let sentinel_binance = binance.clone();
    let sentinel_state = state.clone();
    let sentinel_discord = config.discord_webhook_url.clone();
    tokio::spawn(async move {
        ws_sentinel::run_sentinel(sentinel_binance, sentinel_state, sentinel_discord).await;
    });

    let mut last_ai_scan_minute = -1;
    let mut run_now = std::env::args().any(|arg| arg == "--run-now");

    // Alpha Daemon
    loop {
        let now: DateTime<Utc> = Utc::now();
        let minute = now.minute() as i32;

        if (minute % 15 == 0 && minute != last_ai_scan_minute) || run_now {
            if run_now {
                println!("\n[ALPHA DAEMON] OVERRIDE: '--run-now' flag detected. Executing immediate cycle.");
                run_now = false;
            }
            
            println!("\n[ALPHA DAEMON] STARTING BINANCE QUANTITATIVE CYCLE...");
            
            let mut conversation = vec![
                json!({
                    "role": "user",
                    "parts": [{"text": "Scan the market. Check news if a setup looks good. If the technicals and news align perfectly with extreme capital preservation, place an order. If anything is uncertain, PASS."}]
                })
            ];

            let mut turn_count = 0;
            loop {
                if turn_count > 10 {
                    println!("Max turns reached.");
                    break;
                }
                turn_count += 1;

                let response = gemini.send_conversation(&conversation).await;
                if let Ok(resp) = response {
                    if let Some(candidates) = resp["candidates"].as_array() {
                        if let Some(parts) = candidates[0]["content"]["parts"].as_array() {
                            
                            // Find function_call or text
                            let mut func_call = None;
                            let mut text_output = None;
                            
                            for p in parts {
                                if p.get("functionCall").is_some() {
                                    func_call = p.get("functionCall");
                                } else if p.get("text").is_some() {
                                    text_output = p.get("text");
                                }
                            }
                            
                            // If we have text, print it
                            if let Some(text) = text_output {
                                println!("\nMEMO:\n{}", text.as_str().unwrap_or(""));
                            }
                            
                            // If it's a function call, execute it and continue the loop
                            if let Some(func_call) = func_call {
                                let name = func_call["name"].as_str().unwrap_or("");
                                println!("\n[AGENT ACTION] -> {}...", name);
                                
                                // Add model's request to history
                                conversation.push(json!({
                                    "role": "model",
                                    "parts": [{"functionCall": func_call}]
                                }));

                                let mut result_data = json!({"status": "error", "message": "unknown tool"});

                                if name == "scan_market_opportunities" {
                                    let mut results = vec![];
                                    for sym in &["BTC/USDC", "ETH/USDC", "SOL/USDC"] {
                                        if let Ok(klines) = binance.fetch_ohlcv(sym, "15m", 100).await {
                                            if let Ok(funding) = binance.fetch_funding_rate(sym).await {
                                                let closes: Vec<f64> = klines.iter().filter_map(|k| k[4].as_str().unwrap_or("0").parse().ok()).collect();
                                                
                                                // Offload heavy math to Tokio blocking pool to prevent async starvation
                                                let closes_clone = closes.clone();
                                                let (regime, vol) = tokio::task::spawn_blocking(move || {
                                                    let r = hmm::detect_hmm_regime(&closes_clone);
                                                    let v = garch::forecast_garch(&closes_clone);
                                                    (r, v)
                                                }).await.unwrap_or((0, 0.0));
                                                
                                                let tech = indicators::compute_indicators(sym, &klines, regime, vol, funding);
                                                results.push(tech);
                                            }
                                        }
                                    }
                                    println!("\n[DATA VERIFICATION] Raw scan data sent to AI:\n{:#?}", results);
                                    result_data = json!({ "opportunities": results });
                                } 
                                else if name == "fetch_market_news" {
                                    let args = &func_call["args"];
                                    let sym = args["symbol"].as_str().unwrap_or("");
                                    let news = alpaca::fetch_market_news(
                                        &config.alpaca_api_key.clone().unwrap_or_default(),
                                        &config.alpaca_secret_key.clone().unwrap_or_default(),
                                        sym
                                    ).await;
                                    result_data = serde_json::from_str(&news).unwrap_or(json!({"error": "Failed to parse news"}));
                                }
                                else if name == "place_order" {
                                    let args = &func_call["args"];
                                    let sym = args["symbol"].as_str().unwrap_or("");
                                    let tp = args["take_profit_price"].as_f64().unwrap_or(0.0);
                                    let sl = args["stop_loss_price"].as_f64().unwrap_or(0.0);
                                    let risk = args["risk_pct"].as_f64().unwrap_or(0.02);
                                    
                                    let mut st = state.lock().await;
                                    let order_res = order::place_order(&binance, &mut st, sym, tp, sl, risk, config.discord_webhook_url.clone()).await;
                                    result_data = match order_res {
                                        Ok(v) => v,
                                        Err(e) => json!({"status": "REJECTED", "error": e})
                                    };
                                }

                                // Append function response
                                conversation.push(json!({
                                    "role": "function",
                                    "parts": [{"functionResponse": {
                                        "name": name,
                                        "response": result_data
                                    }}]
                                }));
                                continue;
                            }
                            
                            // If NO function call was made, the conversation is finished
                            break;
                        } else {
                            println!("API Error: Missing parts in candidate. Raw: {}", candidates[0]);
                            break;
                        }
                    } else {
                        println!("API Error: No candidates found. Raw response: {}", resp);
                        break;
                    }
                } else {
                    println!("Gemini HTTP Error: {:?}", response.err());
                    break;
                }
            }
            
            last_ai_scan_minute = minute;
            let next_scan = (minute - (minute % 15) + 15) % 60;
            println!("\n[ALPHA DAEMON] Cycle complete. Next AI scan locked for xx:{:02}:00.", next_scan);
        }
        
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}
