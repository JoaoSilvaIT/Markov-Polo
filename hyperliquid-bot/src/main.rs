use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use hyperliquid_rust_sdk::{BaseUrl, ExchangeClient, InfoClient, Message, Subscription};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::unbounded_channel, Mutex, RwLock};

mod config;
mod discord;
mod hmm;
mod hyperliquid;
mod order;
mod ou_process;
mod state;

use crate::config::Config;
use crate::hmm::{train_hmm_offline, GaussianHMM};
use crate::hyperliquid::{
    cancel_order, fetch_sz_decimals_map, get_user_position, place_post_only_order, quantize_price,
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

    // Initialize HMM
    let initial_hmm = GaussianHMM::load_weights(&config.hmm_config_path)
        .unwrap_or_else(|_| GaussianHMM::new(3, 2));
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

            if let Ok(candles) = info_trainer_clone
                .candles_snapshot(
                    config_trainer_symbol.clone(),
                    "15m".to_string(),
                    start_time,
                    end_time,
                )
                .await
            {
                let mut closes = Vec::new();
                for c in candles {
                    if let Ok(close_val) = c.close.parse::<f64>() {
                        closes.push(close_val);
                    }
                }

                if closes.len() >= 100 {
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

    match info_client
        .candles_snapshot(
            trading_symbol.clone(),
            "1m".to_string(),
            window_start_ms,
            now_ms,
        )
        .await
    {
        Ok(candles) => {
            for c in candles {
                if let Ok(close_f) = c.close.parse::<f64>() {
                    rolling_closes.push(close_f);
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

    crate::discord::send_alert(
        discord_url.as_deref(),
        &format!(
            "🚀 **Hyperliquid Perpetual Bot Online**\nSymbol: **{}** | Bi-Directional O-U + Post-Only Maker Engine Active.",
            trading_symbol
        ),
    )
    .await;

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
                // Scenario 1: Entry order is pending, TP not yet placed
                if pos.tp_order_id.is_empty() && !pos.order_id.is_empty() {
                    if let Ok(oid) = pos.order_id.parse::<u64>() {
                        // Cross-reference user position to see if order filled
                        let (current_szi, _) = get_user_position(&watcher_info, user_address, &watcher_symbol)
                            .await
                            .unwrap_or((0.0, None));
                        let pos_exists = if pos.side == "LONG" {
                            current_szi > 0.0
                        } else {
                            current_szi < 0.0
                        };

                        if let Ok(order_status) = watcher_info.query_order_by_oid(user_address, oid).await {
                            let inner_status = if let Some(ref o) = order_status.order {
                                o.status.to_uppercase()
                            } else {
                                order_status.status.to_uppercase()
                            };
                            let is_filled = inner_status == "FILLED" || pos_exists;
                            let _is_open = inner_status == "OPEN" || inner_status == "RESTING";

                            if is_filled {
                                println!(
                                    "🟢 [FAST-SENTINEL] Entry Filled for {} (Side: {})! Placing Take Profit Limit order at ${:.2}",
                                    watcher_symbol, pos.side, pos.take_profit_price
                                );

                                // Long position -> TP Sell (is_buy = false, reduce_only = true)
                                // Short position -> TP Buy (is_buy = true, reduce_only = true)
                                let tp_is_buy = pos.side == "SHORT";
                                let tp_price = quantize_price(pos.take_profit_price, watcher_sz_decimals);

                                match place_post_only_order(
                                    &watcher_exchange,
                                    &watcher_symbol,
                                    tp_is_buy,
                                    tp_price,
                                    pos.amount,
                                    true, // reduce_only = true
                                )
                                .await
                                {
                                    Ok(tp_oid) => {
                                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                            p.tp_order_id = tp_oid.to_string();
                                            p.is_filled = true;
                                            p.opened_at = chrono::Utc::now().to_rfc3339(); // Clock starts at fill
                                            let _ = save_state(&watcher_state_path, &state_lock);
                                            let msg = format!(
                                                "🟢 **TAKE PROFIT PLACED (HYPERLIQUID):** {}\n**Side:** {} TP\n**Target:** ${:.2}\n**Size:** {:.5}",
                                                watcher_symbol, pos.side, tp_price, pos.amount
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
                            } else if inner_status == "CANCELED"
                                || inner_status == "REJECTED"
                                || inner_status == "MARGINCANCELED"
                                || inner_status == "UNKNOWNOID"
                            {
                                // Truly cancelled or rejected with 0 fills
                                println!(
                                    "⚪ [FAST-SENTINEL] Entry for {} status: {}. Cleaning up state.",
                                    watcher_symbol, inner_status
                                );
                                state_lock.remove(&watcher_symbol);
                                let _ = save_state(&watcher_state_path, &state_lock);
                                release_cluster_lease(&watcher_symbol);
                            }
                            // If is_open, order is resting peacefully on the order book. Do not clean up!
                        }
                    }
                }
                // Scenario 2: TP order is resting on book. Check if filled
                else if !pos.tp_order_id.is_empty() {
                    if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
                        let (current_szi, _) = get_user_position(&watcher_info, user_address, &watcher_symbol)
                            .await
                            .unwrap_or((0.0, None));
                        let is_flat = current_szi.abs() == 0.0;

                        if let Ok(order_status) = watcher_info.query_order_by_oid(user_address, tp_oid).await {
                            let inner_status = if let Some(ref o) = order_status.order {
                                o.status.to_uppercase()
                            } else {
                                order_status.status.to_uppercase()
                            };

                            if inner_status == "FILLED" || is_flat {
                                println!(
                                    "🎉 [FAST-SENTINEL] Take Profit for {} (Side: {}) has FILLED!",
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
            }
        }
    });

    // ------------------------------------------------------------------------
    // HOT-PATH: WebSocket 1m Candle Ingestion & Bi-Directional O-U Execution
    // ------------------------------------------------------------------------
    let position_size_usdc = config.position_size_usdc;
    let mut last_processed_open_ms = 0u64;

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

        while let Some(msg) = receiver.recv().await {
            if let Message::Candle(candle_msg) = msg {
                let candle = candle_msg.data;
                let open_ms = candle.time_open;
                let close_price = candle.close.parse::<f64>().unwrap_or(0.0);

                // Detect completed 1-minute candle transitions
                if last_processed_open_ms > 0 && open_ms > last_processed_open_ms && close_price > 0.0 {
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

                        let regime_str = if !is_stationary {
                            "Trending (Non-Stationary)"
                        } else if !is_consolidation {
                            "Trending (Drift)"
                        } else if is_cb_paused {
                            "Consolidation (Vetoed: Circuit Breaker Pause)"
                        } else if is_downside_shock || is_upside_shock {
                            "Consolidation (Vetoed: Volatility Shock)"
                        } else if is_cluster_blocked {
                            "Consolidation (Vetoed: Cluster Active)"
                        } else if !is_valid_half_life {
                            "Consolidation (Vetoed: Half-Life Out of Bounds)"
                        } else {
                            "Consolidation (Tradable Bi-Directional)"
                        };

                        let (target_buy_str, target_short_str) = match thresholds_opt {
                            Some(ref th) => (
                                format!("${:.2}", th.entry_long),
                                format!("${:.2}", th.entry_short),
                            ),
                            None => ("N/A".to_string(), "N/A".to_string()),
                        };

                        println!(
                            "💓 [HOT-PATH] {} Price: ${:.2} | Long Buy: {} | Short Sell: {} | Regime: {} | Drift: {:.2}% | t_half: {:.1}m | Z15: {:.2} | Z60: {:.2}",
                            symbol_clone, close_price, target_buy_str, target_short_str, regime_str, total_drift_pct * 100.0, half_life_m, z_15, z_60
                        );

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

                            if entry_ttl_expired {
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
                                && !is_cluster_blocked;

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

                    // Advance rolling window
                    if rolling_closes.len() >= 480 {
                        rolling_closes.remove(0);
                    }
                    rolling_closes.push(close_price);
                }

                last_processed_open_ms = open_ms;
            }
        }

        eprintln!("⚠️ [WEBSOCKET] Stream ended or dropped. Reconnecting in 3s...");
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
