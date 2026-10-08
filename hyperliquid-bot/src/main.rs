use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use hyperliquid_rust_sdk::{BaseUrl, ExchangeClient, InfoClient, Message, Subscription};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::unbounded_channel, Mutex, RwLock};

mod config;
mod controller;
mod discord;
mod hmm;
mod hyperliquid;
mod order;
mod ou_process;
mod state;
mod telemetry;
mod trend;

use crate::config::Config;
use crate::controller::{
    load_controller_state, save_controller_state, ControllerMode,
};
use crate::telemetry::{save_telemetry, LiveTelemetry};
use crate::hmm::{compute_hmm_posteriors, train_hmm_offline, GaussianHMM};
use crate::hyperliquid::{
    cancel_order, fetch_book_ticker, fetch_sz_decimals_map, get_user_position, place_post_only_order, quantize_price,
};
use crate::order::{emergency_flatten, place_ou_maker_entry};
use crate::ou_process::{
    calculate_mad_sigma, calculate_net_edge_bps, calculate_optimal_thresholds,
    estimate_ou_parameters,
};
use crate::state::{
    has_active_cluster_position, load_armed_signal, load_circuit_breaker, load_state,
    release_cluster_lease, save_armed_signal, save_circuit_breaker, save_state, Side, StateMap,
};
use crate::trend::{
    calculate_atr, calculate_donchian, check_trend_alignment,
    load_shadow_trade, record_shadow_entry, record_shadow_exit, save_shadow_trade, CandleBar,
    ChandelierExitState, L2ObiTracker, TrendAlignment,
};

#[tokio::main]
async fn main() {
    println!("🚀 [HYPERLIQUID PERP START] Fully Autonomous Deterministic O-U Architecture Initialized.");

    let config = Config::from_env();
    let base_url = if config.hl_is_mainnet {
        BaseUrl::Mainnet
    } else {
        BaseUrl::Testnet
    };

    let wallet: LocalWallet = config
        .hl_private_key
        .parse()
        .expect("Failed to parse HL_PRIVATE_KEY into LocalWallet");

    let user_address: H160 = if let Some(ref addr_str) = config.hl_wallet_address {
        H160::from_str(addr_str).unwrap_or_else(|_| wallet.address())
    } else {
        wallet.address()
    };

    println!(
        "🔑 [WALLET] Connected. Agent Address: {:?}, User Account: {:?} (Mainnet: {})",
        wallet.address(),
        user_address,
        config.hl_is_mainnet
    );

    let exchange = Arc::new(
        ExchangeClient::new(None, wallet, Some(base_url.clone()), None, None)
            .await
            .expect("Failed to create ExchangeClient"),
    );

    let info_client = Arc::new(
        InfoClient::new(None, Some(base_url.clone()))
            .await
            .expect("Failed to create InfoClient"),
    );

    // Fetch size decimals lookup
    let sz_decimals_map = Arc::new(RwLock::new(fetch_sz_decimals_map(&info_client).await));
    let trading_symbol = config.trading_symbol.clone();
    let trading_sz_decimals = {
        let lock = sz_decimals_map.read().await;
        *lock.get(&trading_symbol).unwrap_or(&4)
    };

    println!(
        "📊 [MARKET INFO] Symbol: {} | Size Decimals: {}",
        trading_symbol, trading_sz_decimals
    );

    // Explicitly configure exchange leverage & margin mode (Cross vs Isolated)
    if let Err(e) = exchange
        .update_leverage(config.hl_leverage, &trading_symbol, config.hl_is_cross, None)
        .await
    {
        eprintln!(
            "⚠️ [LEVERAGE] Could not configure leverage to {}x for {}: {}",
            config.hl_leverage, trading_symbol, e
        );
    } else {
        println!(
            "⚙️ [LEVERAGE] Successfully configured {}x (Cross: {}) for {}",
            config.hl_leverage, config.hl_is_cross, trading_symbol
        );
    }

    // Position State
    let state_map: StateMap = load_state(&config.state_file_path).unwrap_or_default();
    let active_state = Arc::new(Mutex::new(state_map));
    let state_file_path = config.state_file_path.clone();

    // Circuit Breaker
    let cb_path = format!("hl_hmm_cb_{}.json", trading_symbol);
    let cb_state = load_circuit_breaker(&cb_path);
    let active_cb = Arc::new(Mutex::new(cb_state));
    let cb_file_path = cb_path.clone();

    // Armed Signal State
    let arm_file_path = format!("hl_hmm_arm_{}.json", trading_symbol);

    // Initialize HMM (4D: normalized return, log vol, Kaufman ER, directional drift)
    let initial_hmm = GaussianHMM::load_weights(&config.hmm_config_path)
        .unwrap_or_else(|_| GaussianHMM::new(3, 4));
    let hmm_model = Arc::new(RwLock::new(initial_hmm));

    let discord_url = config.discord_webhook_url.clone();

    // ------------------------------------------------------------------------
    // COLD-PATH: 24-Hour HMM Recalibration Loop
    // ------------------------------------------------------------------------
    let hmm_trainer_clone = hmm_model.clone();
    let info_trainer_clone = info_client.clone();
    let config_trainer_path = config.hmm_config_path.clone();
    let config_trainer_symbol = trading_symbol.clone();

    tokio::spawn(async move {
        loop {
            println!("⚙️ [COLD-PATH] Initiating 24-hour HMM recalibration for {}...", config_trainer_symbol);
            let end_time = chrono::Utc::now().timestamp_millis() as u64;
            let start_time = end_time.saturating_sub(1000 * 15 * 60 * 1000); // 1000 15m bars

            if let Ok(mut candles) = info_trainer_clone
                .candles_snapshot(
                    config_trainer_symbol.clone(),
                    "15m".to_string(),
                    start_time,
                    end_time,
                )
                .await
            {
                let cur_utc_ms = chrono::Utc::now().timestamp_millis() as u64;
                if let Some(last) = candles.last() {
                    if last.time_open + 15 * 60 * 1000 > cur_utc_ms {
                        candles.pop();
                    }
                }

                let mut closes = Vec::new();
                for c in candles {
                    if let Ok(close_val) = c.close.parse::<f64>() {
                        closes.push(close_val);
                    }
                }

                if closes.len() >= 140 {
                    if let Ok(Some(new_hmm)) =
                        tokio::task::spawn_blocking(move || train_hmm_offline(&closes)).await
                    {
                        let _ = new_hmm.save_weights(&config_trainer_path);
                        let mut lock = hmm_trainer_clone.write().await;
                        *lock = new_hmm;
                        println!("✅ [COLD-PATH] HMM recalibrated and hot-swapped into live memory successfully.");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(86400)).await;
        }
    });

    // ------------------------------------------------------------------------
    // INGESTION: Pre-fill 480 1m historical candles via REST
    // ------------------------------------------------------------------------
    let mut rolling_closes: Vec<f64> = Vec::with_capacity(480);
    println!(
        "⏳ [INGESTION] Pre-filling 480-minute rolling window for {} via Hyperliquid REST...",
        trading_symbol
    );
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let window_start_ms = now_ms.saturating_sub(480 * 60 * 1000);
    let cur_bucket_15m = (now_ms / (15 * 60 * 1000)) * (15 * 60 * 1000);
    let mut initial_15m_bar: Option<CandleBar> = None;

    match info_client
        .candles_snapshot(
            trading_symbol.clone(),
            "1m".to_string(),
            window_start_ms,
            now_ms,
        )
        .await
    {
        Ok(mut candles) => {
            // Exclude potentially incomplete final candle returned by REST
            if let Some(last) = candles.last() {
                if last.time_open + 60_000 > now_ms {
                    candles.pop();
                }
            }
            for c in candles {
                if let (Ok(open_f), Ok(high_f), Ok(low_f), Ok(close_f)) = (
                    c.open.parse::<f64>(),
                    c.high.parse::<f64>(),
                    c.low.parse::<f64>(),
                    c.close.parse::<f64>(),
                ) {
                    let vol_f = c.vlm.parse::<f64>().unwrap_or(0.0);
                    rolling_closes.push(close_f);

                    if c.time_open >= cur_bucket_15m {
                        if let Some(ref mut b15) = initial_15m_bar {
                            b15.high = b15.high.max(high_f);
                            b15.low = b15.low.min(low_f);
                            b15.close = close_f;
                            b15.volume += vol_f;
                        } else {
                            initial_15m_bar = Some(CandleBar {
                                time_open: cur_bucket_15m,
                                open: open_f,
                                high: high_f,
                                low: low_f,
                                close: close_f,
                                volume: vol_f,
                            });
                        }
                    }
                }
            }
            println!(
                "✅ [INGESTION] Successfully loaded {} historical 1m bars for {}.",
                rolling_closes.len(),
                trading_symbol
            );
        }
        Err(e) => {
            eprintln!(
                "❌ [INGESTION ERROR] Failed to fetch 1m candles for {}: {}",
                trading_symbol, e
            );
        }
    }

    // Pre-fill 15m historical candles for Trend Following & Donchian Breakouts
    let mut rolling_15m_bars: Vec<CandleBar> = Vec::with_capacity(100);
    println!(
        "⏳ [INGESTION] Pre-filling 15m historical candles for {} via Hyperliquid REST...",
        trading_symbol
    );
    let window_15m_start_ms = now_ms.saturating_sub(100 * 15 * 60 * 1000);
    match info_client
        .candles_snapshot(
            trading_symbol.clone(),
            "15m".to_string(),
            window_15m_start_ms,
            now_ms,
        )
        .await
    {
        Ok(mut candles) => {
            // Exclude potentially incomplete final candle returned by REST
            if let Some(last) = candles.last() {
                if last.time_open + 15 * 60 * 1000 > now_ms {
                    candles.pop();
                }
            }
            for c in candles {
                if let (Ok(open_f), Ok(high_f), Ok(low_f), Ok(close_f)) = (
                    c.open.parse::<f64>(),
                    c.high.parse::<f64>(),
                    c.low.parse::<f64>(),
                    c.close.parse::<f64>(),
                ) {
                    let vol_f = c.vlm.parse::<f64>().unwrap_or(0.0);
                    rolling_15m_bars.push(CandleBar {
                        time_open: c.time_open,
                        open: open_f,
                        high: high_f,
                        low: low_f,
                        close: close_f,
                        volume: vol_f,
                    });
                }
            }
            println!(
                "✅ [INGESTION] Successfully loaded {} historical 15m bars for {}.",
                rolling_15m_bars.len(),
                trading_symbol
            );
        }
        Err(e) => {
            eprintln!(
                "❌ [INGESTION ERROR] Failed to fetch 15m candles for {}: {}",
                trading_symbol, e
            );
        }
    }

    crate::discord::send_alert(
        discord_url.as_deref(),
        &format!(
            "🚀 **Hyperliquid Perpetual Bot Online**\nSymbol: **{}** | Bi-Directional O-U + Post-Only Maker Engine Active.",
            trading_symbol
        ),
    )
    .await;

    // ------------------------------------------------------------------------
    // STARTUP RECONCILIATION: Sync Local State with Live Hyperliquid Position
    // ------------------------------------------------------------------------
    let (startup_szi, startup_entry_px) = get_user_position(&info_client, user_address, &trading_symbol)
        .await
        .unwrap_or((0.0, None));
    {
        let mut state_lock = active_state.lock().await;
        if startup_szi.abs() > 0.0 {
            let ep = startup_entry_px.unwrap_or(0.0);
            let is_long = startup_szi > 0.0;
            let side_str = if is_long { "LONG" } else { "SHORT" };
            let sl_dist = ep * 0.015;
            let default_sl = quantize_price(
                if is_long { ep - sl_dist } else { ep + sl_dist },
                trading_sz_decimals,
            );

            if let Some(pos) = state_lock.get_mut(&trading_symbol) {
                println!(
                    "🔍 [STARTUP RECONCILIATION] Active position detected on Hyperliquid for {} (size: {:.5}, entry: ${:.2}). Matching local state.",
                    trading_symbol, startup_szi, pos.entry_price
                );
                pos.is_filled = true;
                pos.amount = startup_szi.abs();
                if pos.stop_loss_price <= 0.0 {
                    pos.stop_loss_price = default_sl;
                }
            } else {
                let default_tp = quantize_price(
                    if is_long { ep * 1.008 } else { ep * 0.992 },
                    trading_sz_decimals,
                );
                println!(
                    "⚠️ [STARTUP RECONCILIATION] Found unmanaged live position on Hyperliquid for {} (size: {:.5}, entry: ${:.2}). Adopting into active state with 1.5% catastrophe stop (${:.2}) and target TP (${:.2}).",
                    trading_symbol, startup_szi, ep, default_sl, default_tp
                );
                state_lock.insert(
                    trading_symbol.clone(),
                    crate::state::PositionState {
                        symbol: trading_symbol.clone(),
                        side: side_str.to_string(),
                        order_id: "".to_string(),
                        tp_order_id: "".to_string(),
                        entry_price: ep,
                        amount: startup_szi.abs(),
                        stop_loss_price: default_sl,
                        take_profit_price: default_tp,
                        opened_at: chrono::Utc::now().to_rfc3339(),
                        risk_amount_usdc: config.position_size_usdc,
                        is_filled: true,
                        half_life_minutes: 30.0,
                        client_order_id: "".to_string(),
                        max_progress: 0.0,
                        max_favorable_price: ep,
                        effective_tp: default_tp,
                        last_reprice_time: chrono::Utc::now().timestamp_millis(),
                        reprice_count: 0,
                        candidate_lower_count: 0,
                        engine_type: "EngineA".to_string(),
                        chandelier_stop: 0.0,
                        current_atr: 0.0,
                    },
                );
            }
            let _ = save_state(&state_file_path, &state_lock);
        }
    }

    // ------------------------------------------------------------------------
    // FAST ORDER SUPERVISOR (1.5-Second State Reconciliation & TP Execution)
    // ------------------------------------------------------------------------
    let watcher_state = active_state.clone();
    let watcher_exchange = exchange.clone();
    let watcher_info = info_client.clone();
    let watcher_symbol = trading_symbol.clone();
    let watcher_state_path = state_file_path.clone();
    let watcher_discord = discord_url.clone();
    let watcher_cb = active_cb.clone();
    let watcher_cb_path = cb_file_path.clone();
    let watcher_sz_decimals = trading_sz_decimals;

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        loop {
            interval.tick().await;

            let mut state_lock = watcher_state.lock().await;
            if let Some(pos) = state_lock.get(&watcher_symbol).cloned() {
                // Cross-reference user position on live exchange
                let (current_szi, _) = get_user_position(&watcher_info, user_address, &watcher_symbol)
                    .await
                    .unwrap_or((0.0, None));
                let pos_exists = if pos.side == "LONG" {
                    current_szi > 0.0
                } else {
                    current_szi < 0.0
                };

                // -------------------------------------------------------------
                // Defense Layer 1: Live Order Book Catastrophe Stop Loss (1.5s Sentinel)
                // -------------------------------------------------------------
                if (pos.is_filled || pos_exists) && pos.stop_loss_price > 0.0 {
                    if let Ok((best_bid, best_ask)) = fetch_book_ticker(&watcher_info, &watcher_symbol).await {
                        let is_breached = if pos.side == "LONG" {
                            best_bid > 0.0 && best_bid <= pos.stop_loss_price
                        } else {
                            best_ask > 0.0 && best_ask >= pos.stop_loss_price
                        };

                        if is_breached {
                            let breach_px = if pos.side == "LONG" { best_bid } else { best_ask };
                            println!(
                                "🛑 [FAST-SENTINEL 1.5s STOP LOSS] {} breached catastrophe stop at ${:.2} (Current: ${:.2})! Emergency flattening...",
                                watcher_symbol, pos.stop_loss_price, breach_px
                            );
                            let stop_msg = format!(
                                "🛑 **CATASTROPHE STOP LOSS HIT (1.5s FAST-SENTINEL):** {}\n**Side:** {}\n**Breach Price:** ${:.2}\n**Hard Stop:** ${:.2}\nEmergency flattening immediately.",
                                watcher_symbol, pos.side, breach_px, pos.stop_loss_price
                            );
                            crate::discord::send_alert(watcher_discord.as_deref(), &stop_msg).await;

                            let _ = emergency_flatten(
                                &watcher_state_path,
                                &watcher_exchange,
                                &watcher_info,
                                user_address,
                                &mut state_lock,
                                &watcher_symbol,
                                watcher_discord.clone(),
                            )
                            .await;

                            let mut cb_lock = watcher_cb.lock().await;
                            cb_lock.record_loss();
                            save_circuit_breaker(&watcher_cb_path, &cb_lock);
                            continue;
                        }
                    }
                }

                // -------------------------------------------------------------
                // Scenario 1: Entry order pending or position open without resting TP
                // -------------------------------------------------------------
                if pos.tp_order_id.is_empty() {
                    let mut is_filled = pos_exists || pos.is_filled;
                    let mut is_truly_dead = false;
                    let mut reported_status = "UNKNOWN".to_string();

                    if !pos.order_id.is_empty() {
                        if let Ok(oid) = pos.order_id.parse::<u64>() {
                            let open_orders = watcher_info.open_orders(user_address).await.unwrap_or_default();
                            let is_resting = open_orders.iter().any(|o| o.oid == oid);

                            if is_resting {
                                // Order is physically resting on matching engine
                            } else if !pos_exists {
                                let order_age_secs = if let Ok(opened_dt) = chrono::DateTime::parse_from_rfc3339(&pos.opened_at) {
                                    (chrono::Utc::now() - opened_dt.with_timezone(&chrono::Utc)).num_seconds()
                                } else {
                                    10
                                };

                                if let Ok(order_status) = watcher_info.query_order_by_oid(user_address, oid).await {
                                    reported_status = if let Some(ref o) = order_status.order {
                                        o.status.to_uppercase()
                                    } else {
                                        order_status.status.to_uppercase()
                                    };

                                    if reported_status == "FILLED" {
                                        is_filled = true;
                                    } else if reported_status == "CANCELED"
                                        || reported_status == "REJECTED"
                                        || reported_status == "MARGINCANCELED"
                                    {
                                        is_truly_dead = true;
                                    } else if reported_status == "UNKNOWNOID" && order_age_secs >= 5 {
                                        is_truly_dead = true;
                                    }
                                } else if order_age_secs >= 15 {
                                    is_truly_dead = true;
                                }
                            }
                        }
                    }

                    if is_filled && pos.take_profit_price > 0.0 {
                        println!(
                            "🟢 [FAST-SENTINEL] Position Active for {} (Side: {})! Placing Take Profit Limit order at ${:.2}",
                            watcher_symbol, pos.side, pos.take_profit_price
                        );

                        // Long position -> TP Sell (is_buy = false, reduce_only = true)
                        // Short position -> TP Buy (is_buy = true, reduce_only = true)
                        let tp_is_buy = pos.side == "SHORT";
                        let tp_price = quantize_price(pos.take_profit_price, watcher_sz_decimals);
                        let tp_amount = if current_szi.abs() > 0.0 { current_szi.abs() } else { pos.amount };

                        match place_post_only_order(
                            &watcher_exchange,
                            &watcher_symbol,
                            tp_is_buy,
                            tp_price,
                            tp_amount,
                            true, // reduce_only = true (routes as Gtc in hyperliquid.rs)
                        )
                        .await
                        {
                            Ok(tp_oid) => {
                                if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                    p.tp_order_id = tp_oid.to_string();
                                    p.is_filled = true;
                                    p.opened_at = chrono::Utc::now().to_rfc3339();
                                    let _ = save_state(&watcher_state_path, &state_lock);
                                    let msg = format!(
                                        "🟢 **TAKE PROFIT PLACED (HYPERLIQUID):** {}\n**Side:** {} TP\n**Target:** ${:.2}\n**Size:** {:.5}",
                                        watcher_symbol, pos.side, tp_price, tp_amount
                                    );
                                    crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "❌ [FAST-SENTINEL] Failed to place TP for {}: {}",
                                    watcher_symbol, e
                                );
                            }
                        }
                    } else if is_truly_dead && !pos.order_id.is_empty() {
                        if let Ok(oid) = pos.order_id.parse::<u64>() {
                            let _ = cancel_order(&watcher_exchange, &watcher_symbol, oid).await;
                        }

                        println!(
                            "⚪ [FAST-SENTINEL] Entry for {} dead (status: {}). Cleaning up state.",
                            watcher_symbol, reported_status
                        );
                        state_lock.remove(&watcher_symbol);
                        let _ = save_state(&watcher_state_path, &state_lock);
                        release_cluster_lease(&watcher_symbol);
                    }
                }
                // -------------------------------------------------------------
                // Scenario 2: TP order resting on book. Check if filled or position closed
                // -------------------------------------------------------------
                else if !pos.tp_order_id.is_empty() {
                    let is_flat = current_szi.abs() == 0.0;
                    let mut is_tp_filled = is_flat;

                    if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
                        if let Ok(order_status) = watcher_info.query_order_by_oid(user_address, tp_oid).await {
                            let inner_status = if let Some(ref o) = order_status.order {
                                o.status.to_uppercase()
                            } else {
                                order_status.status.to_uppercase()
                            };

                            if inner_status == "FILLED" {
                                is_tp_filled = true;
                            }
                        }
                    }

                    if is_tp_filled {
                        println!(
                            "🎉 [FAST-SENTINEL] Take Profit for {} (Side: {}) has FILLED / Position Flat!",
                            watcher_symbol, pos.side
                        );
                        state_lock.remove(&watcher_symbol);
                        let _ = save_state(&watcher_state_path, &state_lock);
                        release_cluster_lease(&watcher_symbol);

                        // Reset circuit breaker
                        let mut cb_lock = watcher_cb.lock().await;
                        cb_lock.record_win();
                        save_circuit_breaker(&watcher_cb_path, &cb_lock);

                        let msg = format!(
                            "🎉 **TRADE CLOSED IN PROFIT (HYPERLIQUID)!** {} {} Take Profit has filled successfully.",
                            watcher_symbol, pos.side
                        );
                        crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                    }
                }
            }
        }
    });

    // ------------------------------------------------------------------------
    // HOT-PATH: WebSocket 1m Candle Ingestion & Bi-Directional O-U Execution
    // ------------------------------------------------------------------------
    let position_size_usdc = config.position_size_usdc;
    let trend_engine_mode = config.trend_engine_mode.clone();
    let trend_risk_fraction = config.trend_risk_fraction;
    let donchian_period = config.donchian_period;
    let chandelier_atr_mult = config.chandelier_atr_mult;
    let mut controller = load_controller_state(&trading_symbol);
    let mut current_15m_bar: Option<CandleBar> = initial_15m_bar;
    let mut current_p_c = 0.60;
    let mut current_p_t = 0.25;
    let mut current_p_s = 0.15;

    let obi_tracker = Arc::new(RwLock::new(L2ObiTracker::default()));
    let obi_tracker_bg = obi_tracker.clone();
    let symbol_bg = trading_symbol.clone();
    let base_url_bg = base_url.clone();

    tokio::spawn(async move {
        loop {
            let (l2_sender, mut l2_receiver) = unbounded_channel();
            let mut l2_client = match InfoClient::new(None, Some(base_url_bg.clone())).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("❌ [L2 WS ERROR] Failed to connect InfoClient: {}. Retrying in 5s...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };

            if let Err(e) = l2_client
                .subscribe(
                    Subscription::L2Book {
                        coin: symbol_bg.clone(),
                    },
                    l2_sender,
                )
                .await
            {
                eprintln!("❌ [L2 WS ERROR] Subscription failed: {}. Retrying in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            println!("📡 [L2 WEBSOCKET] Subscribed to continuous L2Book stream for {}.", symbol_bg);

            while let Some(msg) = l2_receiver.recv().await {
                if let Message::L2Book(l2_msg) = msg {
                    let l2_data = l2_msg.data;
                    let bids: Vec<(f64, f64)> = l2_data.levels.first().cloned().unwrap_or_default()
                        .into_iter()
                        .filter_map(|l| Some((l.px.parse::<f64>().ok()?, l.sz.parse::<f64>().ok()?)))
                        .collect();
                    let asks: Vec<(f64, f64)> = l2_data.levels.get(1).cloned().unwrap_or_default()
                        .into_iter()
                        .filter_map(|l| Some((l.px.parse::<f64>().ok()?, l.sz.parse::<f64>().ok()?)))
                        .collect();
                    obi_tracker_bg.write().await.update(&bids, &asks, l2_data.time);
                }
            }
            eprintln!("⚠️ [L2 WEBSOCKET] Disconnected. Reconnecting in 3s...");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });

    loop {
        let (sender, mut receiver) = unbounded_channel();
        let mut ws_info_client = match InfoClient::new(None, Some(base_url.clone())).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("❌ [WS ERROR] Failed to connect InfoClient: {}. Retrying in 5s...", e);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        if let Err(e) = ws_info_client
            .subscribe(
                Subscription::Candle {
                    coin: trading_symbol.clone(),
                    interval: "1m".to_string(),
                },
                sender,
            )
            .await
        {
            eprintln!("❌ [WS ERROR] Subscription failed: {}. Retrying in 5s...", e);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        println!("📡 [WEBSOCKET] Successfully subscribed to 1m candles for {}.", trading_symbol);

        let mut pending_1m: Option<CandleBar> = None;

        while let Some(msg) = receiver.recv().await {
            if let Message::Candle(candle_msg) = msg {
                let candle = candle_msg.data;
                let open_ms = candle.time_open;
                let close_price_raw = candle.close.parse::<f64>().unwrap_or(0.0);
                let high_price_raw = candle.high.parse::<f64>().unwrap_or(close_price_raw);
                let low_price_raw = candle.low.parse::<f64>().unwrap_or(close_price_raw);
                let open_price_raw = candle.open.parse::<f64>().unwrap_or(close_price_raw);
                let vol_val_raw = candle.volume.parse::<f64>().unwrap_or(0.0);

                if close_price_raw <= 0.0 {
                    continue;
                }

                let update_bar = CandleBar {
                    time_open: open_ms,
                    open: open_price_raw,
                    high: high_price_raw,
                    low: low_price_raw,
                    close: close_price_raw,
                    volume: vol_val_raw,
                };

                // Pending candle pattern: only process completed/closed 1-minute bars
                let completed_1m = match pending_1m {
                    None => {
                        pending_1m = Some(update_bar);
                        None
                    }
                    Some(ref mut pending) if pending.time_open == update_bar.time_open => {
                        pending.high = pending.high.max(update_bar.high);
                        pending.low = pending.low.min(update_bar.low);
                        pending.close = update_bar.close;
                        pending.volume = update_bar.volume;
                        None
                    }
                    Some(_) => pending_1m.replace(update_bar),
                };

                let completed_bar = match completed_1m {
                    Some(b) => b,
                    None => continue,
                };

                let close_price = completed_bar.close;
                let high_price = completed_bar.high;
                let low_price = completed_bar.low;
                let open_price = completed_bar.open;
                let vol_val = completed_bar.volume;

                let candle_bucket_15m = (completed_bar.time_open / (15 * 60 * 1000)) * (15 * 60 * 1000);
                let mut finalized_15m_bar = false;

                if let Some(ref mut cur_15m) = current_15m_bar {
                    if cur_15m.time_open == candle_bucket_15m {
                        cur_15m.high = cur_15m.high.max(high_price);
                        cur_15m.low = cur_15m.low.min(low_price);
                        cur_15m.close = close_price;
                        cur_15m.volume += vol_val;
                    } else {
                        rolling_15m_bars.push(cur_15m.clone());
                        if rolling_15m_bars.len() > 100 {
                            rolling_15m_bars.remove(0);
                        }
                        finalized_15m_bar = true;
                        *cur_15m = CandleBar {
                            time_open: candle_bucket_15m,
                            open: open_price,
                            high: high_price,
                            low: low_price,
                            close: close_price,
                            volume: vol_val,
                        };
                    }
                } else {
                    current_15m_bar = Some(CandleBar {
                        time_open: candle_bucket_15m,
                        open: open_price,
                        high: high_price,
                        low: low_price,
                        close: close_price,
                        volume: vol_val,
                    });
                }

                let closes_clone = rolling_closes.clone();
                let exchange_clone = exchange.clone();
                let info_clone = info_client.clone();
                let state_clone = active_state.clone();
                let discord_clone = discord_url.clone();
                let symbol_clone = trading_symbol.clone();
                let state_path_clone = state_file_path.clone();
                let cb_clone = active_cb.clone();
                let cb_path_clone = cb_file_path.clone();
                let arm_path_clone = arm_file_path.clone();
                let sz_decimals = trading_sz_decimals;

                // In-Sample Signal Leakage Prevention: evaluate on historical closes through t-1
                let current_close = close_price;
                let calc_res = if closes_clone.len() >= 100 {
                    tokio::task::spawn_blocking(move || {
                        let hist_last_price = *closes_clone.last().unwrap_or(&0.0);

                        // O-U Parameter Estimation with Phillips & Yu Jackknife bias correction
                        let (thresholds_opt, half_life_m) =
                            if let Some(ou_params) = estimate_ou_parameters(&closes_clone, 1.0) {
                                let hl = ou_params.half_life_minutes();
                                let tx_cost = hist_last_price * 0.0005; // ~5 bps roundtrip on Hyperliquid Maker
                                let th = calculate_optimal_thresholds(&ou_params, tx_cost);
                                (Some(th), hl)
                            } else {
                                (None, 999.0)
                            };

                        // OLS Slope drift calculation
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
                        let slope = (n_drift * sum_ip - sum_i * sum_p)
                            / (n_drift * sum_ii - sum_i * sum_i);
                        let total_drift_pct = (slope * n_drift) / hist_last_price;
                        let is_drifting = total_drift_pct.abs() > 0.015;

                        // Standardized Downside/Upside Shock Calculation with robust MAD
                        let n = closes_clone.len();
                        let mut z_15 = 0.0;
                        let mut z_60 = 0.0;
                        if n >= 65 && current_close > 0.0 {
                            let mut returns_1m = Vec::with_capacity(n - 1);
                            for i in 1..n {
                                returns_1m.push((closes_clone[i] / closes_clone[i - 1]).ln());
                            }
                            let sigma_mad = calculate_mad_sigma(&returns_1m);

                            let ret_15 = (current_close / closes_clone[n - 15]).ln();
                            z_15 = ret_15 / (sigma_mad * (15.0_f64).sqrt());

                            let ret_60 = (current_close / closes_clone[n - 60]).ln();
                            z_60 = ret_60 / (sigma_mad * (60.0_f64).sqrt());
                        }
                        let is_downside_shock = z_15 < -3.0 || z_60 < -3.0;
                        let is_upside_shock = z_15 > 3.0 || z_60 > 3.0;

                        Some((
                            thresholds_opt,
                            total_drift_pct,
                            is_drifting,
                            half_life_m,
                            z_15,
                            z_60,
                            is_downside_shock,
                            is_upside_shock,
                            hist_last_price,
                        ))
                    })
                    .await
                    .unwrap_or(None)
                } else {
                    None
                };

                if let Some((
                    thresholds_opt,
                    total_drift_pct,
                    is_drifting,
                    half_life_m,
                    z_15,
                    z_60,
                    is_downside_shock,
                    is_upside_shock,
                    hist_last_price,
                )) = calc_res
                {
                    let is_stationary = thresholds_opt.is_some();
                    let is_consolidation = !is_drifting && is_stationary;
                    let is_valid_half_life = half_life_m >= 15.0 && half_life_m <= 120.0;

                    let cb_lock = cb_clone.lock().await;
                    let is_cb_paused = cb_lock.is_paused();
                    drop(cb_lock);

                    let cluster_active = has_active_cluster_position(&symbol_clone);
                    let is_cluster_blocked = cluster_active.is_some();

                    // ------------------------------------------------------------------
                    // 3-MODE CONTROLLER REGIME INFERENCE (HMM Causal Filter + Hysteresis)
                    // ------------------------------------------------------------------
                    // Fast-path safety: Severe shock on ANY 1m candle immediately forces Neutral
                    if z_15.abs() >= 3.0 || z_60.abs() >= 3.0 {
                        if controller.current_mode != ControllerMode::Neutral {
                            println!(
                                "🚨 [CONTROLLER SHOCK VETO] Immediate downgrade to Neutral on severe shock (Z15: {:.2}, Z60: {:.2})",
                                z_15, z_60
                            );
                        }
                        controller.force_shock_cooldown(z_15, z_60);
                        save_controller_state(&symbol_clone, &controller);
                    }

                    // Strictly evaluate 15m dwell streaks & state transitions on finalized 15m candle closes!
                    if finalized_15m_bar {
                        let bar_close_ms = rolling_15m_bars.last().map(|b| b.time_open + 15 * 60 * 1000).unwrap_or(0);
                        let closes_15m: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
                        let (p_c, p_t, p_s) = if closes_15m.len() >= 15 {
                            let hmm_lock = hmm_model.read().await;
                            let res = compute_hmm_posteriors(&*hmm_lock, &closes_15m);
                            drop(hmm_lock);
                            res.unwrap_or((0.60, 0.25, 0.15))
                        } else {
                            (0.60, 0.25, 0.15)
                        };
                        current_p_c = p_c;
                        current_p_t = p_t;
                        current_p_s = p_s;

                        let prior_bars = if rolling_15m_bars.len() > 1 {
                            &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                        } else {
                            &rolling_15m_bars[..]
                        };
                        let donchian_opt = calculate_donchian(prior_bars, donchian_period);
                        let has_donchian_breakout = if let Some(ref d) = donchian_opt {
                            close_price > d.upper || close_price < d.lower || high_price > d.upper || low_price < d.lower
                        } else {
                            false
                        };

                        let prev_mode = controller.current_mode;
                        if let Some(new_mode) = controller.evaluate_15m_bar_idempotent(
                            bar_close_ms,
                            p_c,
                            p_t,
                            z_15,
                            z_60,
                            has_donchian_breakout,
                            is_stationary,
                            is_valid_half_life,
                        ) {
                            save_controller_state(&symbol_clone, &controller);

                            println!(
                                "🧭 [3-MODE CONTROLLER 15m EVAL] Mode: {} (was {}) | Streaks: Trend={}, Consolidation={} | P_C: {:.1}% | P_T: {:.1}% | P_Shock: {:.1}% | Reason: {}",
                                new_mode, prev_mode, controller.trend_streak, controller.consolidation_streak, p_c * 100.0, p_t * 100.0, p_s * 100.0, controller.last_reason
                            );
                        }
                    }

                    let (target_buy_str, target_short_str) = match thresholds_opt {
                        Some(ref th) => (
                            format!("${:.2}", th.entry_long),
                            format!("${:.2}", th.entry_short),
                        ),
                        None => ("N/A".to_string(), "N/A".to_string()),
                    };

                    println!(
                        "💓 [HOT-PATH] {} Price: ${:.2} | Long Buy: {} | Short Sell: {} | Mode: {} | P_C: {:.0}% | P_T: {:.0}% | P_S: {:.0}% | Drift: {:.2}% | t_half: {:.1}m | Z15: {:.2} | Z60: {:.2}",
                        symbol_clone, close_price, target_buy_str, target_short_str, controller.current_mode, current_p_c * 100.0, current_p_t * 100.0, current_p_s * 100.0, total_drift_pct * 100.0, half_life_m, z_15, z_60
                    );

                    // ------------------------------------------------------------------
                    // LIVE TELEMETRY: Real-time Metrics & Proximity for Web Dashboard
                    // ------------------------------------------------------------------
                    let (long_buy_val, short_sell_val, exit_long_val, exit_short_val) = match thresholds_opt {
                        Some(ref th) => (Some(th.entry_long), Some(th.entry_short), Some(th.exit_long), Some(th.exit_short)),
                        None => (None, None, None, None),
                    };

                    let (prox_long, prox_short, overall_prox, prox_summary) = match (long_buy_val, short_sell_val) {
                        (Some(buy_px), Some(sell_px)) => {
                            let dist_long_pct = ((close_price - buy_px) / close_price).max(0.0) * 100.0;
                            let p_long = if close_price <= buy_px {
                                100.0
                            } else {
                                (1.0 - (dist_long_pct / 1.5).min(1.0)) * 100.0
                            };

                            let dist_short_pct = ((sell_px - close_price) / close_price).max(0.0) * 100.0;
                            let p_short = if close_price >= sell_px {
                                100.0
                            } else {
                                (1.0 - (dist_short_pct / 1.5).min(1.0)) * 100.0
                            };

                            let best_prox = p_long.max(p_short);
                            let summary = if !is_valid_half_life {
                                format!("Vetoed: t_half={:.1}m outside 15m-120m band", half_life_m)
                            } else if controller.current_mode == ControllerMode::Neutral {
                                format!("Dwell Pacing: Streak {}/3 bars", controller.consolidation_streak)
                            } else if p_long >= p_short {
                                format!("{:.1}% to Long Entry (${:.2} vs ${:.2})", p_long, close_price, buy_px)
                            } else {
                                format!("{:.1}% to Short Entry (${:.2} vs ${:.2})", p_short, close_price, sell_px)
                            };

                            (p_long, p_short, best_prox, summary)
                        }
                        _ => (0.0, 0.0, 0.0, "Awaiting O-U Calibration".to_string()),
                    };

                    let (pos_has, pos_side, pos_amount, pos_entry, pos_tp, pos_sl, pos_chand) = {
                        let sl = state_clone.lock().await;
                        if let Some(p) = sl.get(&symbol_clone) {
                            (true, p.side.clone(), p.amount, p.entry_price, p.take_profit_price, p.stop_loss_price, p.chandelier_stop)
                        } else {
                            (false, "".to_string(), 0.0, 0.0, 0.0, 0.0, 0.0)
                        }
                    };

                    let (l2_instant, l2_1s, l2_5s, l2_fresh) = {
                        let guard = obi_tracker.read().await;
                        (
                            guard.instant_obi,
                            guard.ewma_1s_obi,
                            guard.ewma_5s_obi,
                            guard.is_fresh_and_uncrossed(chrono::Utc::now().timestamp_millis() as u64, 2500),
                        )
                    };

                    let telemetry_data = LiveTelemetry {
                        symbol: symbol_clone.clone(),
                        timestamp_ms: chrono::Utc::now().timestamp_millis() as u64,
                        price: close_price,
                        long_buy: long_buy_val,
                        short_sell: short_sell_val,
                        exit_long: exit_long_val,
                        exit_short: exit_short_val,
                        mode: format!("{:?}", controller.current_mode).to_uppercase(),
                        streak_consolidation: controller.consolidation_streak,
                        streak_trend: controller.trend_streak,
                        mode_reason: controller.last_reason.clone(),
                        shock_cooldown_bars: controller.shock_cooldown_bars,
                        p_consolidation: current_p_c,
                        p_trend: current_p_t,
                        p_shock: current_p_s,
                        drift_pct: total_drift_pct * 100.0,
                        half_life_m,
                        is_half_life_valid: is_valid_half_life,
                        is_stationary,
                        z_15,
                        z_60,
                        is_severe_shock: z_15.abs() >= 3.0 || z_60.abs() >= 3.0,
                        obi_instant: l2_instant,
                        obi_1s: l2_1s,
                        obi_5s: l2_5s,
                        l2_fresh,
                        has_position: pos_has,
                        position_side: pos_side,
                        position_amount: pos_amount,
                        position_entry_price: pos_entry,
                        tp_price: pos_tp,
                        sl_price: pos_sl,
                        chandelier_stop: pos_chand,
                        trade_proximity_long_pct: prox_long,
                        trade_proximity_short_pct: prox_short,
                        overall_proximity_pct: overall_prox,
                        proximity_summary: prox_summary,
                    };
                    save_telemetry(&symbol_clone, &telemetry_data);

                    // ------------------------------------------------------------------
                    // ENGINE B SUPERVISOR: Active Shadow Trend Trailing Stop & Exits
                    // ------------------------------------------------------------------
                    if let Some(mut shadow_trade) = load_shadow_trade(&symbol_clone) {
                        if shadow_trade.is_active {
                            let current_atr = calculate_atr(&rolling_15m_bars, 20).unwrap_or(shadow_trade.atr_at_entry);
                            let mut chandelier = ChandelierExitState {
                                side: shadow_trade.side.clone(),
                                entry_price: shadow_trade.entry_price,
                                extreme_favorable_price: shadow_trade.extreme_favorable_price,
                                current_stop: shadow_trade.current_stop,
                                initial_stop: shadow_trade.initial_stop,
                                base_multiplier: chandelier_atr_mult,
                                active_multiplier: shadow_trade.active_multiplier,
                            };

                            let new_stop = chandelier.update_with_cap(high_price, low_price, current_atr, current_p_t, Some(0.5));
                            shadow_trade.current_stop = new_stop;
                            shadow_trade.extreme_favorable_price = chandelier.extreme_favorable_price;
                            shadow_trade.active_multiplier = chandelier.active_multiplier;
                            save_shadow_trade(&symbol_clone, Some(&shadow_trade));

                            println!(
                                "🟣 [SHADOW-TREND MONITOR] {} {} | Price: ${:.2} (H: ${:.2}, L: ${:.2}) | Ratchet Stop: ${:.2} (mult: {:.2}) | Extreme: ${:.2}",
                                symbol_clone, shadow_trade.side, close_price, high_price, low_price, new_stop, chandelier.active_multiplier, shadow_trade.extreme_favorable_price
                            );

                            if chandelier.is_bar_stopped_out(high_price, low_price) {
                                let exit_price = if shadow_trade.side == Side::Long {
                                    open_price.min(chandelier.current_stop)
                                } else {
                                    open_price.max(chandelier.current_stop)
                                };
                                let net_pnl = record_shadow_exit(&mut shadow_trade, exit_price, "Monotone Dynamic Chandelier Trailing Stop");
                                controller.notify_trend_stopped_out();
                                save_controller_state(&symbol_clone, &controller);
                                let ret_bps = shadow_trade.return_bps.unwrap_or(0.0);
                                println!(
                                    "🛑 [SHADOW-TREND EXIT] {} stopped out at ${:.2}! Net PnL: ${:.2} ({:.1} bps).",
                                    symbol_clone, exit_price, net_pnl, ret_bps
                                );
                                crate::discord::send_alert(
                                    discord_clone.as_deref(),
                                    &format!(
                                        "🛑 **[PHASE 0 SHADOW-TREND EXIT]** {} {}\n**Exit Price:** ${:.2} (Stop: ${:.2})\n**Net PnL:** ${:.2} ({:.1} bps)\n**Reason:** Monotone Dynamic Chandelier Trailing Stop",
                                        symbol_clone, shadow_trade.side, exit_price, new_stop, net_pnl, ret_bps
                                    ),
                                ).await;
                            }
                        }
                    }

                    // ------------------------------------------------------------------
                    // ENGINE B ENTRY: Donchian Breakout + Continuous L2 EWMA OBI Hurdle
                    // ------------------------------------------------------------------
                    if controller.current_mode == ControllerMode::Trend {
                        let active_shadow = load_shadow_trade(&symbol_clone);
                        if active_shadow.is_none() {
                            if is_cluster_blocked {
                                println!("⏸️ [SHADOW-TREND VETO] Active cluster lease in place. Vetoing Engine B entry to prevent cross-engine conflict.");
                            } else {
                                let prior_bars = if finalized_15m_bar && rolling_15m_bars.len() > 1 {
                                    &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                                } else {
                                    &rolling_15m_bars[..]
                                };
                                let donchian_opt = calculate_donchian(prior_bars, donchian_period);
                                if let Some(ref donchian) = donchian_opt {
                                    let closes_15m: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
                                    let alignment = check_trend_alignment(&closes_15m);
                                    let is_long_breakout = (close_price > donchian.upper || high_price > donchian.upper) && alignment == TrendAlignment::Bullish;
                                    let is_short_breakout = (close_price < donchian.lower || low_price < donchian.lower) && alignment == TrendAlignment::Bearish;

                                    if is_long_breakout || is_short_breakout {
                                        let side = if is_long_breakout { Side::Long } else { Side::Short };
                                        let atr_opt = calculate_atr(&rolling_15m_bars, 20);

                                        if let Some(current_atr) = atr_opt.filter(|&a| a > 0.0) {
                                            let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                                            let obi_lock = obi_tracker.read().await;
                                            let (obi_favorable, obi_reason) = obi_lock.satisfies_entry_hurdle(&side, now_ms);
                                            let obi_inst = obi_lock.instant_obi;
                                            let obi_250ms = obi_lock.ewma_250ms_obi;
                                            let obi_1s = obi_lock.ewma_1s_obi;
                                            let obi_5s = obi_lock.ewma_5s_obi;
                                            drop(obi_lock);

                                            if obi_favorable {
                                                let trend_notional = position_size_usdc * trend_risk_fraction;
                                                let shadow_size = trend_notional / close_price;

                                                if trend_engine_mode == "shadow" {
                                                    let shadow_trade = record_shadow_entry(
                                                        &symbol_clone,
                                                        side.clone(),
                                                        close_price,
                                                        shadow_size,
                                                        current_atr,
                                                        chandelier_atr_mult,
                                                    );
                                                    let side_str = if side == Side::Long { "LONG" } else { "SHORT" };
                                                    println!(
                                                        "🟣 [SHADOW-TREND ENTRY] {} {} breakout entered at ${:.2} | Size: {:.4} (${:.2}) | Initial Stop: ${:.2} | Multiscale OBI (inst: {:.2}, 250ms: {:.2}, 1s: {:.2}, 5s: {:.2})",
                                                        symbol_clone, side_str, close_price, shadow_size, trend_notional, shadow_trade.initial_stop, obi_inst, obi_250ms, obi_1s, obi_5s
                                                    );
                                                    crate::discord::send_alert(
                                                        discord_clone.as_deref(),
                                                        &format!(
                                                            "🟣 **[PHASE 0 SHADOW-TREND ENTRY]** {} {}\n**Simulated Entry:** ${:.2}\n**Initial Stop:** ${:.2}\n**Size:** {:.4} (${:.2})\n**Multiscale EWMA OBI:** inst={:.2}, 250ms={:.2}, 1s={:.2}, 5s={:.2} | **ATR:** ${:.2}",
                                                            symbol_clone, side_str, close_price, shadow_trade.initial_stop, shadow_size, trend_notional, obi_inst, obi_250ms, obi_1s, obi_5s, current_atr
                                                        ),
                                                    ).await;
                                                }
                                            } else {
                                                println!(
                                                    "⚠️ [SHADOW-TREND VETO] Multiscale OBI hurdle failed: {} (inst: {:.3}, 250ms: {:.3}, 1s: {:.3}, 5s: {:.3}).",
                                                    obi_reason, obi_inst, obi_250ms, obi_1s, obi_5s
                                                );
                                            }
                                        } else {
                                            println!("⚠️ [SHADOW-TREND VETO] Missing or non-positive 20-period ATR. Entry aborted.");
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let mut state_lock = state_clone.lock().await;
                        let has_position = state_lock.contains_key(&symbol_clone);

                        if has_position {
                            let mut armed_state = load_armed_signal(&arm_path_clone);
                            if armed_state.is_armed {
                                armed_state.is_armed = false;
                                save_armed_signal(&arm_path_clone, &armed_state);
                            }

                            let _ = crate::state::refresh_cluster_lease(&symbol_clone, 3600);

                            let mut entry_ttl_expired = false;
                            let mut time_stop_triggered = false;
                            let mut hard_sl_triggered = false;
                            let mut giveback_triggered = false;
                            let mut elapsed_mins = 0.0;
                            let mut max_allowed_mins = 120.0;

                            let mut should_reprice = false;
                            let mut new_reprice_target = 0.0;
                            let mut reprice_qty = 0.0;
                            let mut reprice_is_buy = false;
                            let mut old_tp_order_id = String::new();

                            if let Some(pos) = state_lock.get_mut(&symbol_clone) {
                                if let Ok(opened_dt) =
                                    chrono::DateTime::parse_from_rfc3339(&pos.opened_at)
                                {
                                    elapsed_mins = (chrono::Utc::now()
                                        - opened_dt.with_timezone(&chrono::Utc))
                                    .num_seconds()
                                        as f64
                                        / 60.0;
                                    if !pos.is_filled {
                                        if elapsed_mins >= 5.0 {
                                            entry_ttl_expired = true;
                                        }
                                    } else {
                                        // Hard Catastrophe Stop Loss check
                                        if pos.stop_loss_price > 0.0 {
                                            if pos.side == "LONG" && close_price <= pos.stop_loss_price {
                                                hard_sl_triggered = true;
                                            } else if pos.side == "SHORT" && close_price >= pos.stop_loss_price {
                                                hard_sl_triggered = true;
                                            }
                                        }

                                        max_allowed_mins = if pos.half_life_minutes > 0.0 {
                                            (2.0 * pos.half_life_minutes).clamp(15.0, 360.0)
                                        } else {
                                            120.0
                                        };
                                        if elapsed_mins >= max_allowed_mins {
                                            time_stop_triggered = true;
                                        }

                                        // -------------------------------------------------------------
                                        // Normalized Progress & 20% Giveback Ratchet (Bi-Directional)
                                        // -------------------------------------------------------------
                                        let is_long = pos.side == "LONG";
                                        let d_0 = if is_long {
                                            pos.take_profit_price - pos.entry_price
                                        } else {
                                            pos.entry_price - pos.take_profit_price
                                        };

                                        if d_0 > 0.0 {
                                            let q_t = if is_long {
                                                (close_price - pos.entry_price) / d_0
                                            } else {
                                                (pos.entry_price - close_price) / d_0
                                            };

                                            if q_t > pos.max_progress {
                                                pos.max_progress = q_t;
                                                pos.max_favorable_price = close_price;
                                            }

                                            // Arm Profit Protection once trade covers >= 75% of target
                                            // Trigger profit-locked exit if price retraces by 20% giveback
                                            let q_be = 0.15; // covers fees + slippage
                                            if pos.max_progress >= 0.75
                                                && q_t <= f64::max(q_be, pos.max_progress - 0.20)
                                            {
                                                giveback_triggered = true;
                                            }

                                            // -------------------------------------------------------------
                                            // One-Way Adaptive Ratchet (Causal Live Mean Tracking)
                                            // -------------------------------------------------------------
                                            if let Some(ref th) = thresholds_opt {
                                                let delta_reprice = (pos.entry_price * 0.0005)
                                                    .max(0.10 * d_0)
                                                    .max(0.03);
                                                let now_ms = chrono::Utc::now().timestamp_millis();
                                                let cooldown_ok =
                                                    (now_ms - pos.last_reprice_time) >= 180_000;

                                                if is_long {
                                                    let live_mu = th.exit_long;
                                                    let eps_shave = (live_mu * 0.0005).max(0.02);
                                                    let b_cand = live_mu - eps_shave;
                                                    let p_min = pos.entry_price * 1.0020;

                                                    if b_cand <= pos.effective_tp - delta_reprice
                                                        && b_cand >= p_min
                                                    {
                                                        pos.candidate_lower_count += 1;
                                                        if pos.candidate_lower_count >= 3
                                                            && cooldown_ok
                                                            && pos.reprice_count < 3
                                                        {
                                                            should_reprice = true;
                                                            new_reprice_target = b_cand;
                                                            reprice_qty = pos.amount;
                                                            reprice_is_buy = false; // TP for Long is Sell
                                                            old_tp_order_id = pos.tp_order_id.clone();
                                                            pos.effective_tp = b_cand;
                                                            pos.last_reprice_time = now_ms;
                                                            pos.reprice_count += 1;
                                                            pos.candidate_lower_count = 0;
                                                        }
                                                    } else if b_cand > pos.effective_tp {
                                                        pos.candidate_lower_count = 0;
                                                    }
                                                } else {
                                                    // SHORT position
                                                    let live_mu = th.exit_short;
                                                    let eps_shave = (live_mu * 0.0005).max(0.02);
                                                    let b_cand = live_mu + eps_shave;
                                                    let p_max = pos.entry_price * 0.9980;

                                                    if b_cand >= pos.effective_tp + delta_reprice
                                                        && b_cand <= p_max
                                                    {
                                                        pos.candidate_lower_count += 1;
                                                        if pos.candidate_lower_count >= 3
                                                            && cooldown_ok
                                                            && pos.reprice_count < 3
                                                        {
                                                            should_reprice = true;
                                                            new_reprice_target = b_cand;
                                                            reprice_qty = pos.amount;
                                                            reprice_is_buy = true; // TP for Short is Buy
                                                            old_tp_order_id = pos.tp_order_id.clone();
                                                            pos.effective_tp = b_cand;
                                                            pos.last_reprice_time = now_ms;
                                                            pos.reprice_count += 1;
                                                            pos.candidate_lower_count = 0;
                                                        }
                                                    } else if b_cand < pos.effective_tp {
                                                        pos.candidate_lower_count = 0;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            let _ = crate::state::save_state(&state_path_clone, &state_lock);

                            // Handle Adaptive Reprice Execution on Hyperliquid
                            if should_reprice && new_reprice_target > 0.0 && reprice_qty > 0.0 {
                                let rounded_target =
                                    quantize_price(new_reprice_target, sz_decimals);
                                println!(
                                    "🔄 [ONE-WAY RATCHET REPRICE HL] {} live mean shifted. Repricing TP order to ${:.2}...",
                                    symbol_clone, rounded_target
                                );
                                if !old_tp_order_id.is_empty() {
                                    if let Ok(old_oid) = old_tp_order_id.parse::<u64>() {
                                        let _ = cancel_order(&exchange_clone, &symbol_clone, old_oid).await;
                                    }
                                }
                                match place_post_only_order(
                                    &exchange_clone,
                                    &symbol_clone,
                                    reprice_is_buy,
                                    rounded_target,
                                    reprice_qty,
                                    true, // reduce_only = true
                                )
                                .await
                                {
                                    Ok(new_oid) => {
                                        if let Some(p) = state_lock.get_mut(&symbol_clone) {
                                            p.tp_order_id = new_oid.to_string();
                                            p.take_profit_price = rounded_target;
                                            let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                        }
                                        let reprice_msg = format!(
                                            "🔄 **TP ADAPTIVELY REPRICED (HL):** {}\n**New Target:** ${:.2}\n**Live Mean Ratchet Active**",
                                            symbol_clone, rounded_target
                                        );
                                        crate::discord::send_alert(discord_clone.as_deref(), &reprice_msg).await;
                                    }
                                    Err(e) => {
                                        eprintln!("❌ [REPRICE FAILED HL] Could not replace TP order for {}: {}", symbol_clone, e);
                                    }
                                }
                            }

                            if hard_sl_triggered {
                                let sl_px = state_lock
                                    .get(&symbol_clone)
                                    .map(|p| p.stop_loss_price)
                                    .unwrap_or(0.0);
                                println!(
                                    "🛑 [CATASTROPHE STOP-LOSS EXECUTED HL] {} close price ${:.2} breached hard stop ${:.2}. Emergency flattening!",
                                    symbol_clone, close_price, sl_px
                                );
                                let sl_msg = format!(
                                    "🛑 **CATASTROPHE STOP-LOSS EXECUTED (HL):** {} breached hard stop at ${:.2} (Close: ${:.2}). Emergency flattening position.",
                                    symbol_clone, sl_px, close_price
                                );
                                crate::discord::send_alert(discord_clone.as_deref(), &sl_msg).await;
                                let mut cb_lock = cb_clone.lock().await;
                                cb_lock.record_loss();
                                save_circuit_breaker(&cb_path_clone, &cb_lock);
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            } else if entry_ttl_expired {
                                println!(
                                    "⏱️ [ENTRY TTL EXPIRED] Resting order for {} unfilled after {:.1}m. Flattening.",
                                    symbol_clone, elapsed_mins
                                );
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            } else if giveback_triggered {
                                let peak_prog = state_lock
                                    .get(&symbol_clone)
                                    .map(|p| p.max_progress * 100.0)
                                    .unwrap_or(0.0);
                                let ent_px = state_lock
                                    .get(&symbol_clone)
                                    .map(|p| p.entry_price)
                                    .unwrap_or(0.0);
                                println!(
                                    "🏆 [PROFIT RATCHET EXECUTED HL] {} peak progress was {:.1}%. Retracement triggered 20% giveback exit at ${:.2}. Banking profit!",
                                    symbol_clone, peak_prog, close_price
                                );
                                let ratchet_msg = format!(
                                    "🏆 **PROFIT RATCHET EXECUTED (HL):** {} reached {:.1}% peak progress. Retracement triggered profit lock at ${:.2} (Entry: ${:.2}). Banking profit.",
                                    symbol_clone, peak_prog, close_price, ent_px
                                );
                                crate::discord::send_alert(discord_clone.as_deref(), &ratchet_msg).await;
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            } else if is_downside_shock || is_upside_shock {
                                println!(
                                    "🚨 [SHOCK VETO ON POSITION] Severe shock on {} (Z15: {:.2}, Z60: {:.2})! Flattening.",
                                    symbol_clone, z_15, z_60
                                );
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            } else if !is_consolidation {
                                println!(
                                    "🚨 [INVALIDATION] Regime shift detected for {}. Flattening.",
                                    symbol_clone
                                );
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            } else if time_stop_triggered {
                                println!(
                                    "⏱️ [TIME-STOP] {} reached max hold {:.1}m >= limit {:.1}m. Flattening.",
                                    symbol_clone, elapsed_mins, max_allowed_mins
                                );
                                let mut cb_lock = cb_clone.lock().await;
                                let tripped = cb_lock.record_time_stop();
                                save_circuit_breaker(&cb_path_clone, &cb_lock);

                                let mut time_msg = format!(
                                    "⏱️ **HALF-LIFE TIME-STOP EXPIRED (HL):** {} held for {:.1}m (limit: {:.1}m). Exiting.",
                                    symbol_clone, elapsed_mins, max_allowed_mins
                                );
                                if tripped {
                                    time_msg.push_str("\n🚨 **CIRCUIT BREAKER TRIGGERED:** 3 consecutive time stops! Trading paused 24h.");
                                }
                                crate::discord::send_alert(discord_clone.as_deref(), &time_msg).await;
                                let _ = emergency_flatten(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    user_address,
                                    &mut state_lock,
                                    &symbol_clone,
                                    discord_clone.clone(),
                                )
                                .await;
                            }
                        } else if !has_position {
                            // ------------------------------------------------------------------
                            // BI-DIRECTIONAL REVERSAL-CONFIRMED ENTRY PROTOCOL (Long & Short)
                            // ------------------------------------------------------------------
                            let mut armed_state = load_armed_signal(&arm_path_clone);
                            let is_eligible = is_consolidation
                                && is_valid_half_life
                                && !is_cb_paused
                                && !is_cluster_blocked
                                && controller.current_mode == ControllerMode::MeanReversion;

                            if armed_state.is_armed && controller.current_mode != ControllerMode::MeanReversion {
                                println!(
                                    "⏸️ [MEAN REVERSION VETO] Controller mode is {} (not MeanReversion). Clearing armed state for {}.",
                                    controller.current_mode, symbol_clone
                                );
                                armed_state.is_armed = false;
                                save_armed_signal(&arm_path_clone, &armed_state);
                            }

                            if armed_state.is_armed {
                                let arm_side = armed_state.side.clone();
                                let arm_timeout_mins = (0.5 * armed_state.half_life_m).clamp(10.0, 30.0);
                                let mut expired = false;

                                if let Some(ref at_ts) = armed_state.armed_at {
                                    if let Ok(at_dt) = chrono::DateTime::parse_from_rfc3339(at_ts) {
                                        let elapsed = (chrono::Utc::now()
                                            - at_dt.with_timezone(&chrono::Utc))
                                        .num_seconds() as f64
                                            / 60.0;
                                        if elapsed >= arm_timeout_mins {
                                            expired = true;
                                        }
                                    }
                                }

                                let shock_vetoed = if arm_side == "LONG" {
                                    is_downside_shock
                                } else {
                                    is_upside_shock
                                };

                                if !is_eligible || expired || shock_vetoed {
                                    println!(
                                        "⚪ [DISARMED] {} {} signal disarmed (Shock: {}, Expired: {})",
                                        symbol_clone, arm_side, shock_vetoed, expired
                                    );
                                    armed_state.is_armed = false;
                                    save_armed_signal(&arm_path_clone, &armed_state);
                                } else if arm_side == "LONG" {
                                    // Update lowest dip price
                                    if close_price < armed_state.extreme_price {
                                        armed_state.extreme_price = close_price;
                                        save_armed_signal(&arm_path_clone, &armed_state);
                                    }

                                    let live_sma = rolling_closes.iter().sum::<f64>()
                                        / rolling_closes.len() as f64;
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
                                    let bounced_off_low = close_price
                                        >= armed_state.extreme_price * 1.0005
                                        || close_price >= dynamic_entry_cap;
                                    let still_below_exit = close_price < dynamic_exit_target;

                                    if is_green_bounce && bounced_off_low && still_below_exit {
                                        let entry_limit_price = f64::min(close_price, dynamic_entry_cap);
                                        let net_edge_bps = calculate_net_edge_bps(
                                            entry_limit_price,
                                            dynamic_exit_target,
                                        );

                                        if net_edge_bps < 25.0 {
                                            println!(
                                                "⚠️ [LONG HURDLE VETO] Edge too narrow for {}: {:.1} bps (< 25 bps).",
                                                symbol_clone, net_edge_bps
                                            );
                                            armed_state.is_armed = false;
                                            save_armed_signal(&arm_path_clone, &armed_state);
                                        } else {
                                            println!(
                                                "🟢 [LONG REVERSAL CONFIRMED] {} reversed from dip ${:.2} to ${:.2}! Edge: {:.1} bps. Submitting Maker Buy.",
                                                symbol_clone, armed_state.extreme_price, close_price, net_edge_bps
                                            );
                                            let frozen_hl = armed_state.half_life_m;
                                            armed_state.is_armed = false;
                                            save_armed_signal(&arm_path_clone, &armed_state);

                                            if let Err(e) = place_ou_maker_entry(
                                                &state_path_clone,
                                                &exchange_clone,
                                                &info_clone,
                                                &mut state_lock,
                                                &symbol_clone,
                                                Side::Long,
                                                entry_limit_price,
                                                dynamic_exit_target,
                                                frozen_hl,
                                                position_size_usdc,
                                                sz_decimals,
                                                discord_clone.clone(),
                                            )
                                            .await
                                            {
                                                eprintln!("❌ [ORDER ERROR] Long maker entry failed: {}", e);
                                            }
                                        }
                                    }
                                } else if arm_side == "SHORT" {
                                    // Update highest peak price
                                    if close_price > armed_state.extreme_price {
                                        armed_state.extreme_price = close_price;
                                        save_armed_signal(&arm_path_clone, &armed_state);
                                    }

                                    let live_sma = rolling_closes.iter().sum::<f64>()
                                        / rolling_closes.len() as f64;
                                    let dynamic_exit_target = if armed_state.unitless_exit_dev != 0.0 {
                                        live_sma * armed_state.unitless_exit_dev.exp()
                                    } else {
                                        armed_state.exit_threshold
                                    };
                                    let dynamic_entry_floor = if armed_state.unitless_entry_dev != 0.0 {
                                        live_sma * armed_state.unitless_entry_dev.exp()
                                    } else {
                                        armed_state.entry_threshold
                                    };

                                    let is_red_rejection = close_price < hist_last_price;
                                    let rejected_off_high = close_price
                                        <= armed_state.extreme_price * 0.9995
                                        || close_price <= dynamic_entry_floor;
                                    let still_above_exit = close_price > dynamic_exit_target;

                                    if is_red_rejection && rejected_off_high && still_above_exit {
                                        let entry_limit_price = f64::max(close_price, dynamic_entry_floor);
                                        // For Short: edge is entry_limit_price down to dynamic_exit_target
                                        let net_edge_bps = (entry_limit_price / dynamic_exit_target).ln() * 10000.0;

                                        if net_edge_bps < 25.0 {
                                            println!(
                                                "⚠️ [SHORT HURDLE VETO] Edge too narrow for {}: {:.1} bps (< 25 bps).",
                                                symbol_clone, net_edge_bps
                                            );
                                            armed_state.is_armed = false;
                                            save_armed_signal(&arm_path_clone, &armed_state);
                                        } else {
                                            println!(
                                                "🔴 [SHORT REVERSAL CONFIRMED] {} rejected from peak ${:.2} down to ${:.2}! Edge: {:.1} bps. Submitting Maker Short.",
                                                symbol_clone, armed_state.extreme_price, close_price, net_edge_bps
                                            );
                                            let frozen_hl = armed_state.half_life_m;
                                            armed_state.is_armed = false;
                                            save_armed_signal(&arm_path_clone, &armed_state);

                                            if let Err(e) = place_ou_maker_entry(
                                                &state_path_clone,
                                                &exchange_clone,
                                                &info_clone,
                                                &mut state_lock,
                                                &symbol_clone,
                                                Side::Short,
                                                entry_limit_price,
                                                dynamic_exit_target,
                                                frozen_hl,
                                                position_size_usdc,
                                                sz_decimals,
                                                discord_clone.clone(),
                                            )
                                            .await
                                            {
                                                eprintln!("❌ [ORDER ERROR] Short maker entry failed: {}", e);
                                            }
                                        }
                                    }
                                }
                            } else if is_eligible {
                                if let Some(ref thresholds) = thresholds_opt {
                                    // Check Long Arm: price dipped below lower boundary
                                    if close_price <= thresholds.entry_long && !is_downside_shock {
                                        let baseline_arm = rolling_closes.iter().sum::<f64>()
                                            / rolling_closes.len() as f64;
                                        let unitless_entry_dev =
                                            (thresholds.entry_long / baseline_arm).ln();
                                        let unitless_exit_dev =
                                            (thresholds.exit_long / baseline_arm).ln();

                                        armed_state.is_armed = true;
                                        armed_state.side = "LONG".to_string();
                                        armed_state.armed_at = Some(chrono::Utc::now().to_rfc3339());
                                        armed_state.entry_threshold = thresholds.entry_long;
                                        armed_state.exit_threshold = thresholds.exit_long;
                                        armed_state.unitless_entry_dev = unitless_entry_dev;
                                        armed_state.unitless_exit_dev = unitless_exit_dev;
                                        armed_state.half_life_m = half_life_m;
                                        armed_state.extreme_price = close_price;
                                        save_armed_signal(&arm_path_clone, &armed_state);

                                        println!(
                                            "🟡 [LONG ARMED] {} touched lower threshold (${:.2} <= ${:.2}). Waiting for green bounce.",
                                            symbol_clone, close_price, thresholds.entry_long
                                        );
                                    }
                                    // Check Short Arm: price spiked above upper boundary
                                    else if close_price >= thresholds.entry_short && !is_upside_shock {
                                        let baseline_arm = rolling_closes.iter().sum::<f64>()
                                            / rolling_closes.len() as f64;
                                        let unitless_entry_dev =
                                            (thresholds.entry_short / baseline_arm).ln();
                                        let unitless_exit_dev =
                                            (thresholds.exit_short / baseline_arm).ln();

                                        armed_state.is_armed = true;
                                        armed_state.side = "SHORT".to_string();
                                        armed_state.armed_at = Some(chrono::Utc::now().to_rfc3339());
                                        armed_state.entry_threshold = thresholds.entry_short;
                                        armed_state.exit_threshold = thresholds.exit_short;
                                        armed_state.unitless_entry_dev = unitless_entry_dev;
                                        armed_state.unitless_exit_dev = unitless_exit_dev;
                                        armed_state.half_life_m = half_life_m;
                                        armed_state.extreme_price = close_price;
                                        save_armed_signal(&arm_path_clone, &armed_state);

                                        println!(
                                            "🟣 [SHORT ARMED] {} touched upper threshold (${:.2} >= ${:.2}). Waiting for red rejection.",
                                            symbol_clone, close_price, thresholds.entry_short
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Advance rolling window with finalized 1m close
                    if rolling_closes.len() >= 480 {
                        rolling_closes.remove(0);
                    }
                    rolling_closes.push(close_price);
                }
            }

        eprintln!("⚠️ [WEBSOCKET] Stream ended or dropped. Reconnecting in 3s...");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
