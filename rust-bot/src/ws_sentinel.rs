use crate::binance::BinanceClient;
use crate::state::{StateMap, save_state};
use std::time::Duration;
use tokio::sync::Mutex;
use std::sync::Arc;
use serde_json::Value;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::StreamExt;

pub async fn run_sentinel(sentinel_binance: Arc<BinanceClient>, sentinel_state: Arc<Mutex<StateMap>>, discord_url: Option<String>) {
    let ws_url = "wss://stream.binance.com:9443/ws/!miniTicker@arr";

    loop {
        println!("[SENTINEL] Connecting to Binance WebSocket...");
        
        let connect_result = tokio::time::timeout(
            Duration::from_secs(10),
            connect_async(ws_url)
        ).await;

        let (ws_stream, _) = match connect_result {
            Ok(Ok(res)) => res,
            _ => {
                println!("[SENTINEL] WebSocket connection failed. Retrying in 3 seconds...");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        };

        println!("[SENTINEL] WebSocket connected successfully.");
        let (_, mut read) = ws_stream.split();

        loop {
            let msg_timeout = tokio::time::timeout(
                Duration::from_secs(15), // Watchdog timer
                read.next()
            ).await;

            let msg = match msg_timeout {
                Ok(Some(Ok(m))) => m,
                Ok(Some(Err(e))) => {
                    println!("[SENTINEL] WebSocket error: {}. Reconnecting...", e);
                    break;
                }
                Ok(None) => {
                    println!("[SENTINEL] WebSocket closed by server. Reconnecting...");
                    break;
                }
                Err(_) => {
                    println!("[SENTINEL] Watchdog timeout (no message for 15s). Reconnecting...");
                    break;
                }
            };

            match msg {
                Message::Text(text) => {
                    let mut current_state = sentinel_state.lock().await;
                    if current_state.is_empty() {
                        // Print audit occasionally, but we'll skip to avoid log spam on WS
                        continue;
                    }

                    let parsed: Value = match serde_json::from_str(&text) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(arr) = parsed.as_array() {
                        let mut symbols_to_remove = Vec::new();

                        for item in arr {
                            if let (Some(s), Some(c)) = (item["s"].as_str(), item["c"].as_str()) {
                                if let Ok(current_price) = c.parse::<f64>() {
                                    // Check against state
                                    for (state_sym, pos) in current_state.iter() {
                                        let raw_sym = state_sym.replace("/", "");
                                        if s == raw_sym {
                                            let tp = pos.take_profit_price;
                                            let sl = pos.stop_loss_price;
                                            let amount = pos.amount;

                                            if current_price >= tp {
                                                let msg = format!("✅ **TAKE PROFIT HIT!** {} exited at ${:.2} (Target: ${:.2}). Executed via pure WebSocket zero-latency market sell.", state_sym, current_price, tp);
                                                println!("[SENTINEL TRIGGER] {}", msg);
                                                crate::discord::send_alert(discord_url.as_deref(), &msg).await;
                                                let _ = sentinel_binance.create_market_sell(state_sym, amount).await;
                                                symbols_to_remove.push(state_sym.clone());
                                            } else if current_price <= sl {
                                                let msg = format!("🚨 **STOP LOSS HIT!** {} emergency exited at ${:.2} (Limit: ${:.2}). Executed via pure WebSocket zero-latency market sell.", state_sym, current_price, sl);
                                                println!("[SENTINEL TRIGGER] {}", msg);
                                                crate::discord::send_alert(discord_url.as_deref(), &msg).await;
                                                let _ = sentinel_binance.create_market_sell(state_sym, amount).await;
                                                symbols_to_remove.push(state_sym.clone());
                                            } else {
                                                // Minimal heartbeat, maybe once every 10 seconds? Websocket is too fast.
                                                // We'll omit it to prevent WS lag, or just rely on the GUI dashboard.
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if !symbols_to_remove.is_empty() {
                            for sym in symbols_to_remove {
                                current_state.remove(&sym);
                            }
                            let _ = save_state(&current_state);
                        }
                    }
                }
                Message::Ping(_) | Message::Pong(_) => {
                    // tokio-tungstenite handles Ping/Pong automatically
                }
                _ => {}
            }
        }
    }
}
