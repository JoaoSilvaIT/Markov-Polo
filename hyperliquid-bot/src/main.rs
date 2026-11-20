use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use hyperliquid_rust_sdk::{BaseUrl, ExchangeClient, InfoClient, Message, Subscription};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::unbounded_channel, Mutex, Notify, RwLock};

pub mod backtest;
mod config;
mod controller;
pub mod counterfactual;
mod discord;
mod hmm;
mod hyperliquid;
pub mod monte_carlo;
pub mod mux;
mod order;
mod ou_process;
mod state;
mod telemetry;
mod trend;

use crate::config::Config;
use crate::controller::{load_controller_state, save_controller_state};
use crate::telemetry::{
    check_btc_lead_lag_veto, load_funnel_counters, save_funnel_counters, save_telemetry,
    LiveTelemetry,
};
use crate::hmm::{compute_hmm_posteriors, train_hmm_offline, GaussianHMM};
use crate::hyperliquid::{
    calculate_dynamic_notional, cancel_order, compute_macro_drawdown_risk_multiplier,
    effective_tick_size, fetch_book_ticker, fetch_open_orders_for_coin, fetch_sz_decimals_map,
    get_account_value, get_user_position, place_disaster_stop_trigger_order,
    place_reduce_only_tp_order, quantize_price, round_to_decimals,
};
use crate::order::{emergency_flatten_fast, place_macro_trend_ioc_entry};
use crate::ou_process::{
    asset_calibration, calculate_features_with_funding_at, check_macro_calendar_veto,
    evaluate_session_lifecycle_gate, AssetFeeSchedule, SessionLifecycleAction,
};
use crate::state::{
    check_cluster_circuit_breaker, has_active_cluster_position, load_armed_signal,
    load_circuit_breaker, load_state, save_armed_signal, save_circuit_breaker, save_state, Side,
    StateMap,
};
use crate::trend::{
    calculate_donchian, check_trend_alignment, CandleBar, L2ObiTracker, MacroTrend4hEngine,
    MarketCrowdingTracker, TrendAlignment, TrendPullback1hEngine,
};

const L2_ORDERBOOK_FRESH_MS: u64 = 16_500;

async fn backfill_missing_1m_candles(
    info_client: &InfoClient,
    coin: &str,
    start_ms: u64,
    end_ms: u64,
    rolling_closes: &mut Vec<f64>,
    rolling_5m_bars: &mut Vec<CandleBar>,
    current_5m_bar: &mut Option<CandleBar>,
    rolling_15m_bars: &mut Vec<CandleBar>,
    current_15m_bar: &mut Option<CandleBar>,
) -> Result<usize, String> {
    if start_ms >= end_ms {
        return Ok(0);
    }
    let mut candles = tokio::time::timeout(
        Duration::from_secs(10),
        info_client.candles_snapshot(coin.to_string(), "1m".to_string(), start_ms, end_ms),
    )
    .await
    .map_err(|_| format!("Timeout fetching candles_snapshot for {}", coin))?
    .map_err(|e| format!("REST candles_snapshot error: {}", e))?;

    // 1. Sort strictly by time_open
    candles.sort_by_key(|c| c.time_open);

    // 2. Filter to range [start_ms, end_ms) and deduplicate
    candles.retain(|c| c.time_open >= start_ms && c.time_open < end_ms);
    candles.dedup_by_key(|c| c.time_open);

    // 3. Construct expected sequence of consecutive 60s bars
    let expected: Vec<u64> = (start_ms..end_ms).step_by(60_000).collect();
    let received: Vec<u64> = candles.iter().map(|c| c.time_open).collect();

    // 4. Require exact equality for data continuity certification
    if received != expected {
        return Err(format!(
            "incomplete backfill for {}: expected {} bars ({:?}), received {} bars ({:?})",
            coin,
            expected.len(),
            expected,
            received.len(),
            received
        ));
    }

    let mut backfilled_count = 0;
    for c in candles {
        let open_f = c.open.parse::<f64>().map_err(|e| format!("Invalid open: {}", e))?;
        let high_f = c.high.parse::<f64>().map_err(|e| format!("Invalid high: {}", e))?;
        let low_f = c.low.parse::<f64>().map_err(|e| format!("Invalid low: {}", e))?;
        let close_f = c.close.parse::<f64>().map_err(|e| format!("Invalid close: {}", e))?;
        let vol_f = c.vlm.parse::<f64>().unwrap_or(0.0);

        if !open_f.is_finite() || open_f <= 0.0
            || !high_f.is_finite() || high_f <= 0.0
            || !low_f.is_finite() || low_f <= 0.0
            || !close_f.is_finite() || close_f <= 0.0
            || !vol_f.is_finite() || vol_f < 0.0
        {
            return Err(format!(
                "Non-finite or non-positive OHLCV detected in candle bar at {}",
                c.time_open
            ));
        }

        if high_f < open_f.max(close_f) || low_f > open_f.min(close_f) || high_f < low_f {
            return Err(format!(
                "Geometric OHLC violation at {}: high={:.2}, low={:.2}, open={:.2}, close={:.2}",
                c.time_open, high_f, low_f, open_f, close_f
            ));
        }

        rolling_closes.push(close_f);
        if rolling_closes.len() > 960 {
            rolling_closes.remove(0);
        }

        let candle_bucket_5m = (c.time_open / (5 * 60 * 1000)) * (5 * 60 * 1000);
        if let Some(ref mut cur_5m) = current_5m_bar {
            if cur_5m.time_open == candle_bucket_5m {
                cur_5m.high = cur_5m.high.max(high_f);
                cur_5m.low = cur_5m.low.min(low_f);
                cur_5m.close = close_f;
                cur_5m.volume += vol_f;
            } else {
                let closed = cur_5m.clone();
                rolling_5m_bars.push(closed);
                if rolling_5m_bars.len() > 300 {
                    rolling_5m_bars.remove(0);
                }
                *cur_5m = CandleBar {
                    time_open: candle_bucket_5m,
                    open: open_f,
                    high: high_f,
                    low: low_f,
                    close: close_f,
                    volume: vol_f,
                };
            }
        } else {
            *current_5m_bar = Some(CandleBar {
                time_open: candle_bucket_5m,
                open: open_f,
                high: high_f,
                low: low_f,
                close: close_f,
                volume: vol_f,
            });
        }

        let candle_bucket_15m = (c.time_open / (15 * 60 * 1000)) * (15 * 60 * 1000);
        if let Some(ref mut cur_15m) = current_15m_bar {
            if cur_15m.time_open == candle_bucket_15m {
                cur_15m.high = cur_15m.high.max(high_f);
                cur_15m.low = cur_15m.low.min(low_f);
                cur_15m.close = close_f;
                cur_15m.volume += vol_f;
            } else {
                let closed = cur_15m.clone();
                rolling_15m_bars.push(closed);
                if rolling_15m_bars.len() > 100 {
                    rolling_15m_bars.remove(0);
                }
                *cur_15m = CandleBar {
                    time_open: candle_bucket_15m,
                    open: open_f,
                    high: high_f,
                    low: low_f,
                    close: close_f,
                    volume: vol_f,
                };
            }
        } else {
            *current_15m_bar = Some(CandleBar {
                time_open: candle_bucket_15m,
                open: open_f,
                high: high_f,
                low: low_f,
                close: close_f,
                volume: vol_f,
            });
        }
        backfilled_count += 1;
    }
    Ok(backfilled_count)
}

#[inline]
fn effective_equity_fraction_for_config(config: &Config) -> f64 {
    if config.trading_symbol.starts_with("xyz:") {
        config.equity_fraction.min(0.32)
    } else {
        config.equity_fraction
    }
}

/// Dynamically resolves the trade bank notional (in USDC).
/// If sizing_mode is "dynamic", queries Hyperliquid for live account equity, caches it,
/// and allocates `equity_fraction` of the total balance (0.35 Crypto, 0.32 HIP-3),
/// clamped to Hyperliquid's minimum ($11.0) and the maximum safety cap.
/// If live fetch fails or mode is "fixed", gracefully falls back to cached equity or `config.position_size_usdc`.
async fn resolve_trade_bank(
    info: &InfoClient,
    address: H160,
    config: &Config,
    cached_equity: &RwLock<f64>,
) -> f64 {
    if config.sizing_mode == "fixed" {
        return config.position_size_usdc;
    }
    let eff_eq_frac = effective_equity_fraction_for_config(config);

    if let Ok(account_val) = get_account_value(info, address).await {
        if account_val > 0.0 {
            let mut w = cached_equity.write().await;
            *w = account_val;
            return calculate_dynamic_notional(
                account_val,
                eff_eq_frac,
                11.0,
                config.max_position_size_usdc,
                config.position_size_usdc,
            );
        }
    }

    let r = cached_equity.read().await;
    if *r > 0.0 {
        return calculate_dynamic_notional(
            *r,
            eff_eq_frac,
            11.0,
            config.max_position_size_usdc,
            config.position_size_usdc,
        );
    }

    config.position_size_usdc
}

fn spawn_discord_alert_bg(discord_url: Option<String>, msg: String) {
    if let Some(url) = discord_url {
        if !url.trim().is_empty() {
            tokio::spawn(async move {
                crate::discord::send_alert(Some(&url), &msg).await;
            });
        }
    }
}

pub async fn execute_emergency_flatten_with_retry_bbo(
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
    let mut last_err = String::new();
    for attempt in 1..=3 {
        let bbo_for_attempt = if attempt == 1 { fast_bbo } else { None };
        match emergency_flatten_fast(
            state_file_path,
            exchange,
            info,
            wallet_address,
            state,
            symbol,
            sz_decimals,
            discord_url.clone(),
            bbo_for_attempt,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                eprintln!(
                    "⚠️ [EMERGENCY FLATTEN RETRY] Attempt {}/3 failed for {}: {}. Retrying...",
                    attempt, symbol, last_err
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(attempt as u64 * 350)).await;
            }
        }
    }
    let critical_msg = format!(
        "🚨🚨 **CRITICAL: EMERGENCY FLATTEN EXHAUSTED ALL 3 RETRIES FOR {}!** Error: {}. MANUAL INTERVENTION REQUIRED IMMEDIATELY!",
        symbol, last_err
    );
    eprintln!("{}", critical_msg);
    spawn_discord_alert_bg(discord_url, critical_msg);
    Err(last_err)
}

pub async fn execute_emergency_flatten_with_retry(
    state_file_path: &str,
    exchange: &ExchangeClient,
    info: &InfoClient,
    wallet_address: H160,
    state: &mut StateMap,
    symbol: &str,
    sz_decimals: u32,
    discord_url: Option<String>,
) -> Result<(), String> {
    execute_emergency_flatten_with_retry_bbo(
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

pub fn is_symbol_live_capital_enabled(symbol: &str) -> bool {
    let raw = std::env::var("HL_LIVE_SYMBOLS")
        .unwrap_or_else(|_| "BTC,ETH,SOL,AVAX".to_string());
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("ALL") || trimmed == "*" {
        return true;
    }
    let sym_norm = crate::config::normalize_trading_symbol(symbol);
    trimmed.split(',').any(|tok| {
        let t = tok.trim();
        !t.is_empty() && crate::config::normalize_trading_symbol(t) == sym_norm
    })
}

/// Package 8B Fast-Sentinel intrabar escape guard for live Engine A positions:
/// Prevents waiting up to 59s for a 1m close during sudden flash dumps.
pub fn check_fast_sentinel_intrabar_escape(
    unreal_bps: f64,
    ms_below_hard_cap: u64,
    live_intrabar_cusum: f64,
    eff_b_star: f64,
    cur_1m_bar_adverse_bps: f64,
    btc_adverse_ret_5m_bps: f64,
) -> Option<&'static str> {
    if unreal_bps <= -48.5 || (unreal_bps <= -45.0 && ms_below_hard_cap >= 2_500) {
        return Some("FAST_SENTINEL_INTRABAR_HARD_CAP_45BPS");
    }
    if unreal_bps <= -34.0
        && live_intrabar_cusum >= eff_b_star
        && (cur_1m_bar_adverse_bps <= -24.0 || btc_adverse_ret_5m_bps <= -40.0)
    {
        return Some("FAST_SENTINEL_INTRABAR_CUSUM_CRASH_ESCAPE");
    }
    None
}

/// Computes the 2-tranche asymmetric TP plan `(tp_qty, tp_price, tranche_tag)` shared identically
/// by `Fast-Sentinel` and adaptive/stall TP reprices so reprices never collapse an active 2-tranche split.
pub fn compute_tp_order_plan(
    engine_type: &str,
    entry_price: f64,
    raw_tp: f64,
    total_amt: f64,
    tranche1_tp_filled: bool,
    sz_decimals: u32,
) -> (f64, f64, &'static str) {
    compute_tp_order_plan_with_mode(
        engine_type,
        entry_price,
        raw_tp,
        total_amt,
        tranche1_tp_filled,
        false,
        sz_decimals,
    )
}

/// Same as `compute_tp_order_plan`, with `exact_target = true` skipping the 0.65x T1 compression
/// for explicit Stall-Hazard / Breakeven / State-3 Maker-Peg harvest targets, while enforcing a
/// 22 bps minimum T1 profit floor whenever `raw_tp` is at least 22 bps away from `entry_price`.
/// Package 8B: Engine A uses a 35% Tranche 1 / 65% Tranche 2 split (`T1-35%`) to expand Avg Win.
pub fn compute_tp_order_plan_with_mode(
    engine_type: &str,
    entry_price: f64,
    raw_tp: f64,
    total_amt: f64,
    tranche1_tp_filled: bool,
    exact_target: bool,
    sz_decimals: u32,
) -> (f64, f64, &'static str) {
    let total_notional = total_amt * entry_price;
    let t1_frac = if engine_type == "EngineB" { 0.50 } else { 0.35 };
    let t1_qty = round_to_decimals(total_amt * t1_frac, sz_decimals);
    let rem_qty = round_to_decimals(total_amt - t1_qty, sz_decimals);
    let use_two_tranche = !tranche1_tp_filled
        && total_notional >= 28.0
        && t1_qty > 0.0
        && rem_qty > 0.0
        && t1_qty * raw_tp >= 10.50
        && rem_qty * raw_tp >= 10.50;

    if use_two_tranche {
        if engine_type == "EngineB" {
            (t1_qty, quantize_price(raw_tp, sz_decimals), "T1-50%")
        } else if exact_target {
            (t1_qty, quantize_price(raw_tp, sz_decimals), "T1-35%")
        } else {
            let dist = raw_tp - entry_price;
            let raw_dist_frac = if entry_price > 0.0 {
                dist.abs() / entry_price
            } else {
                0.0
            };
            let min_t1_frac = 0.0022; // 22 bps floor (Package 8B)
            let t1_eng_a_px = if raw_dist_frac <= min_t1_frac {
                raw_tp
            } else {
                let shaved = entry_price + 0.65 * dist;
                if dist >= 0.0 {
                    shaved.max(entry_price * (1.0 + min_t1_frac))
                } else {
                    shaved.min(entry_price * (1.0 - min_t1_frac))
                }
            };
            (t1_qty, quantize_price(t1_eng_a_px, sz_decimals), "T1-35%")
        }
    } else {
        (total_amt, quantize_price(raw_tp, sz_decimals), "FULL")
    }
}

#[tokio::main]
async fn main() {
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--mux") {
        mux::run_mux_server().await;
        return;
    }
    let run_bt = cli_args.iter().any(|a| a == "--backtest");
    let run_mc = cli_args.iter().any(|a| a == "--monte-carlo");
    if run_bt || run_mc {
        println!("🔬 [NAUTILUS-STYLE SIMULATED EXCHANGE BACKTESTER] Starting discrete-event replay...");
        let info = InfoClient::new(None, Some(BaseUrl::Mainnet))
            .await
            .expect("Failed to create InfoClient for backtest");
        let mut sz_map = fetch_sz_decimals_map(&info).await;
        let mut dummy_asset_map = std::collections::HashMap::new();
        crate::hyperliquid::register_hip3_dex_assets(&mut dummy_asset_map, &mut sz_map, "xyz", 1)
            .await;
        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let start_ms = now_ms.saturating_sub(2400 * 60 * 1000); // 40 hours (2,400 x 1m bars)

        let native_only = cli_args.iter().any(|a| a == "--native");
        if native_only {
            println!("🔬 [NAUTILUS-STYLE SIMULATED EXCHANGE BACKTESTER] Replaying captured native L1 Hyperliquid bars (with real L2 OBI & funding)...");
        } else {
            println!("🔬 [NAUTILUS-STYLE SIMULATED EXCHANGE BACKTESTER] Starting discrete-event replay...");
        }

        use rayon::prelude::*;

        let mut grand_wins = 0usize;
        let mut grand_losses = 0usize;
        let mut grand_net_usd = 0.0;
        let mut grand_net_bps = 0.0;
        let mut grand_fees_usd = 0.0;
        let mut grand_gross_usd = 0.0;

        let all_symbols = [
            "BTC",
            "ETH",
            "SOL",
            "AVAX",
            "HYPE",
            "NEAR",
            "XRP",
            "SUI",
            "LINK",
            "ZEC",
            "xyz:NVDA",
            "xyz:GOLD",
            "xyz:XYZ100",
            "xyz:TSLA",
            "xyz:SILVER",
        ];
        let symbols_vec: Vec<&str> = if native_only {
            all_symbols
                .iter()
                .filter(|&&sym| {
                    let native_path = format!("hl_history_native_{}.jsonl", sym.to_lowercase());
                    let hist_path = format!("hl_history_{}.jsonl", sym.to_lowercase());
                    std::path::Path::new(&native_path).exists()
                        || std::path::Path::new(&hist_path).exists()
                })
                .copied()
                .collect()
        } else {
            all_symbols.to_vec()
        };
        let symbols: &[&str] = &symbols_vec;

        if !native_only {
            for &sym in symbols {
                let file_path = format!("hl_history_{}.jsonl", sym.to_lowercase());
                let is_sufficient = std::fs::metadata(&file_path)
                    .map(|m| m.len() > 50_000)
                    .unwrap_or(false);
                if !is_sufficient {
                    println!(
                        "⏳ [BACKTEST INGESTION] Local NAS history for {} is missing or small; fetching 2,400 1m bars from Hyperliquid REST...",
                        sym
                    );
                    if let Ok(mut snaps) = info
                        .candles_snapshot(sym.to_string(), "1m".to_string(), start_ms, now_ms)
                        .await
                    {
                        snaps.sort_by_key(|c| c.time_open);
                        for c in snaps {
                            let o = c.open.parse::<f64>().unwrap_or(0.0);
                            let h = c.high.parse::<f64>().unwrap_or(0.0);
                            let l = c.low.parse::<f64>().unwrap_or(0.0);
                            let cl = c.close.parse::<f64>().unwrap_or(0.0);
                            let v = c.vlm.parse::<f64>().unwrap_or(0.0);
                            if cl > 0.0 && h >= l && l > 0.0 {
                                let sz_dec = sz_map
                    .get(sym)
                    .copied()
                    .unwrap_or_else(|| asset_calibration(sym).sz_decimals);
                                let tick = effective_tick_size(cl, sz_dec);
                                let rec = crate::backtest::HistoricalMarketBarRecord {
                                    symbol: sym.to_string(),
                                    timestamp_ms: c.time_open,
                                    open: o,
                                    high: h,
                                    low: l,
                                    close: cl,
                                    volume: v,
                                    best_bid: cl - 0.5 * tick,
                                    best_ask: cl + 0.5 * tick,
                                    obi_instant: 0.0,
                                    obi_1s: 0.0,
                                    obi_5s: 0.0,
                                    funding_1h: 0.0000125,
                                    open_interest: 0.0,
                                    mark_px: cl,
                                    oracle_px: cl,
                                    obi_ewma_tau1: None,
                                    obi_ewma_tau2: None,
                                    obi_1m_twap: None,
                                };
                                crate::backtest::append_historical_bar_record(&rec);
                            }
                        }
                    }
                }
            }
        }

        println!(
            "📂 [PARALLEL DATA INGESTION] Ingesting {} asset histories concurrently across 16 CPU cores...",
            symbols.len()
        );
        let t_ingest = std::time::Instant::now();
        let loaded_portfolio: Vec<(
            &'static str,
            Vec<crate::backtest::HistoricalMarketBarRecord>,
            u32,
        )> = symbols
            .par_iter()
            .map(|&sym| {
                let t0 = std::time::Instant::now();
                let native_file = format!("hl_history_native_{}.jsonl", sym.to_lowercase());
                let bars = if native_only && std::path::Path::new(&native_file).exists() {
                    crate::backtest::load_bars_from_path(&native_file)
                } else if native_only {
                    let all_bars = crate::backtest::load_historical_bars(sym);
                    if all_bars.len() > 100_000 {
                        // Separate real live-captured L1 bars from multi-year synthetic Binance archives
                        all_bars
                            .into_iter()
                            .filter(|b| b.open_interest > 0.0 || b.obi_ewma_tau1.is_some())
                            .collect()
                    } else {
                        all_bars
                    }
                } else {
                    crate::backtest::load_historical_bars(sym)
                };
                let sz_dec = sz_map
                    .get(sym)
                    .copied()
                    .unwrap_or_else(|| asset_calibration(sym).sz_decimals);
                println!(
                    "   ↳ [{}] Loaded {} bars from disk in {:.1}s",
                    sym,
                    bars.len(),
                    t0.elapsed().as_secs_f64()
                );
                (sym, bars, sz_dec)
            })
            .collect();
        println!(
            "✅ [INGESTION COMPLETE] All assets loaded in {:.1}s",
            t_ingest.elapsed().as_secs_f64()
        );

        let loaded_portfolio: Vec<(
            &'static str,
            Vec<crate::backtest::HistoricalMarketBarRecord>,
            u32,
        )> = loaded_portfolio
            .into_iter()
            .filter(|(_, bars, _)| !bars.is_empty())
            .collect();
        if loaded_portfolio.is_empty() {
            eprintln!("⚠️ [BACKTEST] No historical bar records found in current working directory.");
            return;
        }
        let btc_idx = loaded_portfolio
            .iter()
            .position(|(s, _, _)| *s == "BTC")
            .unwrap_or(0);
        let btc_bars = &loaded_portfolio[btc_idx].1;

        println!("🚀 [PARALLEL BACKTEST] Executing discrete-event engine across all {} assets simultaneously...", loaded_portfolio.len());
        let t_bt = std::time::Instant::now();
        let parallel_results: Vec<_> = loaded_portfolio
            .par_iter()
            .map(|&(sym, ref bars, sz_dec)| {
                let t0 = std::time::Instant::now();
                let fee_sched = AssetFeeSchedule::for_symbol(sym);
                let eq_frac = if sym.to_lowercase().starts_with("xyz:") {
                    0.32
                } else {
                    0.35
                };
                let sim_cfg = crate::backtest::SimulatedExchangeConfig {
                    maker_fee_bps: fee_sched.maker_bps,
                    taker_fee_bps: fee_sched.taker_bps,
                    sz_decimals: sz_dec,
                    notional_usdc: 560.0 * eq_frac,
                    account_equity_usdc: 560.0,
                    enable_legacy_engine_a: false, // 100% Live-Backtest Parity (Markov-Polo V2 Macro Trend)
                    ..Default::default()
                };
                let res = crate::backtest::run_engine_a_backtest_with_btc(
                    sym,
                    bars,
                    Some(btc_bars),
                    &sim_cfg,
                );
                println!(
                    "   ↳ [{}] Finished replay of {} bars in {:.1}s",
                    sym,
                    bars.len(),
                    t0.elapsed().as_secs_f64()
                );
                (sym, sz_dec, sim_cfg, res)
            })
            .collect();
        println!(
            "✅ [BACKTEST COMPLETE] All {} replays completed in {:.1}s",
            parallel_results.len(),
            t_bt.elapsed().as_secs_f64()
        );

        let precomputed_summaries: Vec<(
            &'static str,
            crate::backtest::BacktestSummary,
            crate::backtest::SimulatedExchangeConfig,
        )> = parallel_results
            .into_iter()
            .map(|(sym, _sz_dec, sim_cfg, res)| (sym, res, sim_cfg))
            .collect();

        for (sym, res, _sim_cfg) in &precomputed_summaries {
            grand_wins += res.wins;
            grand_losses += res.losses;
            grand_net_usd += res.net_pnl_usd;
            grand_net_bps += res.net_pnl_bps;

            println!(
                "📊 [BACKTEST SUMMARY - {}] Bars: {} | Armed: {} | BTC Vetoes: {} | Macro Vetoes: {} | Submitted: {} | Filled: {} | Win/Loss: {}W/{}L ({:.1}%) | Net PnL: {:+.1} bps (${:+.2}) | PF: {:.2} | Max DD: ${:.2}",
                sym,
                res.bars_evaluated,
                res.signals_armed,
                res.btc_impulse_vetoes,
                res.macro_calendar_vetoes,
                res.orders_submitted,
                res.orders_filled,
                res.wins,
                res.losses,
                res.win_rate_pct,
                res.net_pnl_bps,
                res.net_pnl_usd,
                res.profit_factor,
                res.max_drawdown_usd
            );
            let win_trades: Vec<_> = res.trades.iter().filter(|t| t.net_bps > 0.0).collect();
            let loss_trades: Vec<_> = res.trades.iter().filter(|t| t.net_bps <= 0.0).collect();
            let avg_win_bps = if !win_trades.is_empty() {
                win_trades.iter().map(|t| t.net_bps).sum::<f64>() / win_trades.len() as f64
            } else {
                0.0
            };
            let avg_loss_bps = if !loss_trades.is_empty() {
                loss_trades.iter().map(|t| t.net_bps.abs()).sum::<f64>() / loss_trades.len() as f64
            } else {
                0.0
            };
            let payoff_ratio = if avg_loss_bps > 0.0 {
                avg_win_bps / avg_loss_bps
            } else {
                0.0
            };
            let total_fees = res.total_fees_usd;
            let gross_usd = res.net_pnl_usd + total_fees;
            grand_fees_usd += total_fees;
            grand_gross_usd += gross_usd;

            println!(
                "   ↳ Payoff: {:.2} (Avg Win: {:+.1} bps | Avg Loss: -{:.1} bps) | Gross PnL: ${:+.2} | Fees Paid: ${:.2}",
                payoff_ratio, avg_win_bps, avg_loss_bps, gross_usd, total_fees
            );
            if !res.engine_b_vetoes.is_empty() {
                let veto_str: Vec<String> = res
                    .engine_b_vetoes
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v))
                    .collect();
                println!("   ↳ [ENGINE B GATE BREAKDOWN]: {}", veto_str.join(" | "));
            }

            let show_all_trades = std::env::var("HL_BACKTEST_VERBOSE").as_deref() == Ok("1");
            let trade_display = if !show_all_trades && res.trades.len() > 3 {
                println!(
                    "   ... [{} earlier trades omitted; showing latest 3]",
                    res.trades.len() - 3
                );
                &res.trades[res.trades.len() - 3..]
            } else {
                &res.trades[..]
            };
            for t in trade_display {
                let dt_str = chrono::DateTime::from_timestamp_millis(t.entry_time_ms as i64)
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                println!(
                    "   ↳ [{}] {} {:?} ({}) | Entry: ${:.4} -> Exit: ${:.4} | Net: {:+.1} bps (${:+.3}) | MFE: {:.1} / MAE: {:.1} bps | {}",
                    dt_str, t.symbol, t.side, t.entry_policy, t.entry_px, t.exit_px, t.net_bps, t.net_pnl_usd, t.mfe_bps, t.mae_bps, t.exit_reason
                );
            }
        }
        let total_tr = grand_wins + grand_losses;
        let wr = if total_tr > 0 {
            (grand_wins as f64 / total_tr as f64) * 100.0
        } else {
            0.0
        };
        let expect_bps = if total_tr > 0 {
            grand_net_bps / (total_tr as f64)
        } else {
            0.0
        };
        println!(
            "🏆 [PORTFOLIO BACKTEST TOTAL] Trades: {} ({}W / {}L, {:.1}% WR) | Net PnL: {:+.1} bps (${:+.2} USDC) | Expectancy: {:+.2} bps/trade | Gross PnL: ${:+.2} | Fees: ${:.2}",
            total_tr, grand_wins, grand_losses, wr, grand_net_bps, grand_net_usd, expect_bps, grand_gross_usd, grand_fees_usd
        );

        let all_trades: Vec<&crate::backtest::SimulatedTradeResult> = precomputed_summaries
            .iter()
            .flat_map(|(_, res, _)| res.trades.iter())
            .collect();
        let engine_a_trades: Vec<_> = all_trades
            .iter()
            .filter(|t| {
                t.entry_policy == "JOIN_BOOK"
                    || t.entry_policy == "RETEST_LIMIT"
                    || t.entry_policy == "JUDAS_SWING_REVERSAL"
            })
            .copied()
            .collect();
        let engine_b_trades: Vec<_> = all_trades
            .iter()
            .filter(|t| {
                t.entry_policy == "ENGINE_B_TREND"
                    || t.entry_policy == "ENGINE_B_1H_PULLBACK"
                    || t.entry_policy == "ENGINE_B_4H_ST3"
                    || t.entry_policy == "ENGINE_B_4H_D40"
                    || t.entry_policy == "ENGINE_B_HANDOFF"
                    || t.entry_policy == "ENGINE_B_US_OPEN_ORB"
            })
            .copied()
            .collect();
        let engine_cd_trades: Vec<_> = all_trades
            .iter()
            .filter(|t| t.entry_policy == "ENGINE_C_SQUEEZE" || t.entry_policy == "ENGINE_D_CASCADE")
            .copied()
            .collect();

        for (name, trs) in [
            ("Engine A (2-Tranche O-U Mean Reversion)", &engine_a_trades),
            ("Engine B (Donchian Breakout + Handoffs)", &engine_b_trades),
            ("Engine C/D (Funding Squeeze & Cascade)", &engine_cd_trades),
        ] {
            let n = trs.len();
            let w = trs.iter().filter(|t| t.net_bps > 0.0).count();
            let l = trs.iter().filter(|t| t.net_bps <= 0.0).count();
            let wr = if n > 0 { (w as f64 / n as f64) * 100.0 } else { 0.0 };
            let net_usd: f64 = trs.iter().map(|t| t.net_pnl_usd).sum();
            let fees_usd: f64 = trs.iter().map(|t| t.fees_usd).sum();
            let avg_win: f64 = if w > 0 {
                trs.iter().filter(|t| t.net_bps > 0.0).map(|t| t.net_bps).sum::<f64>() / w as f64
            } else {
                0.0
            };
            let avg_loss: f64 = if l > 0 {
                trs.iter().filter(|t| t.net_bps <= 0.0).map(|t| t.net_bps.abs()).sum::<f64>() / l as f64
            } else {
                0.0
            };
            let payoff = if avg_loss > 0.0 { avg_win / avg_loss } else { 0.0 };
            println!(
                "   ↳ {}: {} trades ({}W/{}L, {:.1}% WR) | Payoff: {:.2} (Avg W: +{:.1} / Avg L: -{:.1} bps) | Net: ${:+.2} | Fees: ${:.2}",
                name, n, w, l, wr, payoff, avg_win, avg_loss, net_usd, fees_usd
            );
        }
        {
            const SPLIT_MS: u64 = 1_794_096_000_000; // 2026-11-08 00:00 UTC
            let mut splits: Vec<(&str, Vec<&crate::backtest::SimulatedTradeResult>)> = vec![
                ("Pillar 4H_ST3      ", all_trades.iter().filter(|t| t.entry_policy == "ENGINE_B_4H_ST3").copied().collect()),
                ("Pillar 4H_D40      ", all_trades.iter().filter(|t| t.entry_policy == "ENGINE_B_4H_D40").copied().collect()),
                ("Pillar 1H_PB       ", all_trades.iter().filter(|t| t.entry_policy == "ENGINE_B_1H_PULLBACK").copied().collect()),
                ("Policy JOIN_BOOK   ", all_trades.iter().filter(|t| t.entry_policy == "JOIN_BOOK").copied().collect()),
            ];
            if native_only {
                splits.push(("H1 (< Nov 08 UTC)  ", all_trades.iter().filter(|t| t.entry_time_ms < SPLIT_MS).copied().collect()));
                splits.push(("H2 (>= Nov 08 UTC) ", all_trades.iter().filter(|t| t.entry_time_ms >= SPLIT_MS).copied().collect()));
            }
            for (label, filt) in splits {
                if filt.is_empty() { continue; }
                let n = filt.len();
                let w = filt.iter().filter(|t| t.net_bps > 0.0).count();
                let net_u: f64 = filt.iter().map(|t| t.net_pnl_usd).sum();
                let net_b: f64 = filt.iter().map(|t| t.net_bps).sum();
                let gp: f64 = filt.iter().filter(|t| t.net_pnl_usd > 0.0).map(|t| t.net_pnl_usd).sum();
                let gl: f64 = filt.iter().filter(|t| t.net_pnl_usd <= 0.0).map(|t| t.net_pnl_usd.abs()).sum();
                let pf = if gl > 1e-9 { gp / gl } else if gp > 0.0 { 99.9 } else { 0.0 };
                println!(
                    "   ↳ [SPLIT] {}: {:4} trades ({:4}W/{:4}L, {:5.1}% WR) | PF: {:5.2} | Net: ${:+8.2} ({:+8.1} bps, {:+.1}/tr)",
                    label, n, w, n - w, (w as f64 / n as f64) * 100.0, pf, net_u, net_b, net_b / n as f64
                );
            }
        }

        use std::collections::HashMap;
        let mut exit_counts: HashMap<&'static str, (usize, f64, f64)> = HashMap::new();
        for t in &all_trades {
            let entry = exit_counts.entry(t.exit_reason).or_insert((0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += t.net_pnl_usd;
            entry.2 += t.net_bps;
        }
        let mut exit_vec: Vec<_> = exit_counts.into_iter().collect();
        exit_vec.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        println!("   ↳ [EXIT REASONS BREAKDOWN]:");
        for (reason, (cnt, net_usd, net_bps)) in exit_vec {
            println!(
                "      • {:<45} : {:4} trades | Net: ${:+.2} ({:+.1} bps)",
                reason, cnt, net_usd, net_bps
            );
        }

        // Synchronized Chronological Compounding Portfolio Replay (Exact V2.2 Zero-Liquidation Rules)
        {
            use chrono::Datelike;
            use std::collections::BTreeMap;

            let run_compounded = |label: &str, start_eq: f64, pos_cap_usd: f64, extra_fee_mult: f64, show_yearly: bool| {
                let mut sorted_trades: Vec<&crate::backtest::SimulatedTradeResult> = all_trades.clone();
                sorted_trades.sort_by_key(|t| (t.entry_time_ms, t.exit_time_ms));

                let max_slots = crate::state::max_concurrent_cluster_positions();
                let max_same_dir = crate::state::max_same_direction_macro_positions(max_slots);

                struct ActiveSlot {
                    symbol: String,
                    side: Side,
                    exit_time_ms: u64,
                    notional: f64,
                    pnl_usd: f64,
                    is_win: bool,
                }

                let mut active: Vec<ActiveSlot> = Vec::new();
                let mut eq = start_eq;
                let mut hwm = start_eq;
                let mut max_dd_pct = 0.0_f64;
                let mut max_lev = 0.0_f64;
                let mut taken = 0usize;
                let mut wins = 0usize;
                let mut yearly: BTreeMap<i32, (f64, usize, usize, f64)> = BTreeMap::new();
                let mut year_hwm: BTreeMap<i32, f64> = BTreeMap::new();

                let close_expired = |now_ms: u64,
                                     active: &mut Vec<ActiveSlot>,
                                     eq: &mut f64,
                                     hwm: &mut f64,
                                     max_dd_pct: &mut f64,
                                     wins: &mut usize,
                                     yearly: &mut BTreeMap<i32, (f64, usize, usize, f64)>,
                                     year_hwm: &mut BTreeMap<i32, f64>| {
                    active.sort_by_key(|a| a.exit_time_ms);
                    let mut remaining = Vec::new();
                    for s in active.drain(..) {
                        if s.exit_time_ms <= now_ms {
                            let eq_before = (*eq).max(1e-9);
                            *eq += s.pnl_usd;
                            if s.is_win {
                                *wins += 1;
                            }
                            if *eq > *hwm {
                                *hwm = *eq;
                            }
                            let dd = ((*hwm - *eq) / (*hwm).max(1e-9)) * 100.0;
                            if dd > *max_dd_pct {
                                *max_dd_pct = dd;
                            }
                            let yr = chrono::DateTime::from_timestamp_millis(s.exit_time_ms as i64)
                                .map(|d| d.year())
                                .unwrap_or(2026);
                            let entry = yearly.entry(yr).or_insert((*eq, 0, 0, 0.0));
                            entry.0 = *eq;
                            entry.1 += 1;
                            if s.is_win {
                                entry.2 += 1;
                            }
                            let y_hwm = year_hwm.entry(yr).or_insert(eq_before);
                            if *eq > *y_hwm {
                                *y_hwm = *eq;
                            }
                            let y_dd = ((*y_hwm - *eq) / (*y_hwm).max(1e-9)) * 100.0;
                            if y_dd > entry.3 {
                                entry.3 = y_dd;
                            }
                        } else {
                            remaining.push(s);
                        }
                    }
                    *active = remaining;
                };

                for t in sorted_trades {
                    close_expired(
                        t.entry_time_ms,
                        &mut active,
                        &mut eq,
                        &mut hwm,
                        &mut max_dd_pct,
                        &mut wins,
                        &mut yearly,
                        &mut year_hwm,
                    );
                    if active.len() >= max_slots || active.iter().any(|a| a.symbol == t.symbol) {
                        continue;
                    }
                    if active.iter().filter(|a| a.side == t.side).count() >= max_same_dir {
                        continue;
                    }
                    let open_ntl: f64 = active.iter().map(|a| a.notional).sum();
                    let dd_mult = compute_macro_drawdown_risk_multiplier(eq, hwm);
                    let base_ntl_560 = (t.qty * t.entry_px).max(12.0);
                    let risk_frac_of_eq = (base_ntl_560 / 560.0) * dd_mult;
                    let slot_cap = (eq / max_slots as f64).min(pos_cap_usd);
                    let rem_budget = (eq - open_ntl).max(0.0);
                    let hard_cap = slot_cap.min(rem_budget);
                    if hard_cap < 12.0 {
                        continue;
                    }
                    let ntl = (eq * risk_frac_of_eq).min(hard_cap).max(12.0).min(hard_cap);
                    let scale = ntl / base_ntl_560;
                    let adj_pnl = (t.net_pnl_usd - t.fees_usd * extra_fee_mult) * scale;

                    let new_lev = (open_ntl + ntl) / eq.max(1e-9);
                    if new_lev > max_lev {
                        max_lev = new_lev;
                    }
                    taken += 1;
                    active.push(ActiveSlot {
                        symbol: t.symbol.clone(),
                        side: t.side.clone(),
                        exit_time_ms: t.exit_time_ms,
                        notional: ntl,
                        pnl_usd: adj_pnl,
                        is_win: adj_pnl > 0.0,
                    });
                }

                close_expired(
                    u64::MAX,
                    &mut active,
                    &mut eq,
                    &mut hwm,
                    &mut max_dd_pct,
                    &mut wins,
                    &mut yearly,
                    &mut year_hwm,
                );

                let tot_ret_pct = ((eq / start_eq) - 1.0) * 100.0;
                let years = yearly.len().max(1) as f64;
                let cagr_pct = if eq > 0.0 {
                    ((eq / start_eq).powf(1.0 / years) - 1.0) * 100.0
                } else {
                    0.0
                };
                let wr = if taken > 0 {
                    (wins as f64 / taken as f64) * 100.0
                } else {
                    0.0
                };
                println!(
                    "💰 [COMPOUNDED - {}] Start: ${:.2} -> Final: ${:.2} ({:+.1}%, CAGR: {:+.1}%/yr) | Trades: {} ({:.1}% WR) | MaxDD: {:.1}% | MaxLev: {:.2}x",
                    label, start_eq, eq, tot_ret_pct, cagr_pct, taken, wr, max_dd_pct, max_lev
                );
                if show_yearly && yearly.len() > 1 {
                    let mut prev_end = start_eq;
                    for (yr, (end_eq, n_tr, n_w, y_dd)) in &yearly {
                        let y_ret = ((end_eq / prev_end) - 1.0) * 100.0;
                        let y_wr = if *n_tr > 0 { (*n_w as f64 / *n_tr as f64) * 100.0 } else { 0.0 };
                        println!(
                            "      • {}: ${:8.2} -> ${:8.2} ({:+6.1}%) | {:4} trades ({:4.1}% WR) | Year MaxDD: {:4.1}%",
                            yr, prev_end, end_eq, y_ret, n_tr, y_wr, y_dd
                        );
                        prev_end = *end_eq;
                    }
                }
            };

            run_compounded("V2.2 Zero-Liq Uncapped (<=20%/slot, <=1.00x)", 560.69, 1_000_000.0, 0.0, true);
            run_compounded("V2.2 Zero-Liq 2x Fee Stress Test (Uncapped)", 560.69, 1_000_000.0, 1.0, false);
        }

        if run_mc {
            crate::monte_carlo::run_portfolio_monte_carlo_and_sensitivity(
                &loaded_portfolio,
                btc_bars,
                1000,
            );
        }
        return;
    }

    println!("🚀 [HYPERLIQUID PERP START] Fully Autonomous Deterministic O-U Architecture Initialized.");

    let config = Config::from_env();
    let preloaded_state: StateMap = load_state(&config.state_file_path).unwrap_or_default();
    let has_open_pos_on_disk = !preloaded_state.is_empty();
    let startup_stagger_ms: u64 = match config.trading_symbol.as_str() {
        "BTC" => 0,
        "ETH" => 2000,
        "SOL" => 4000,
        "AVAX" => 6000,
        "HYPE" => 8000,
        "NEAR" => 10000,
        "XRP" => 12000,
        "SUI" => 14000,
        "LINK" => 16000,
        "xyz:NVDA" => 18000,
        "xyz:GOLD" => 20000,
        "xyz:XYZ100" => 22000,
        "xyz:TSLA" => 24000,
        "xyz:SILVER" => 26000,
        "ZEC" => 28000,
        _ => 1500,
    };
    if !has_open_pos_on_disk && startup_stagger_ms > 0 {
        tokio::time::sleep(Duration::from_millis(startup_stagger_ms)).await;
    }
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

    let mut exchange_opt = None;
    let mut last_exch_err = String::new();
    for attempt in 0..5 {
        match ExchangeClient::new(None, wallet.clone(), Some(base_url.clone()), None, None).await {
            Ok(client) => {
                exchange_opt = Some(client);
                break;
            }
            Err(e) => {
                last_exch_err = format!("{:?}", e);
                if attempt < 4 {
                    let wait_ms = [2000u64, 4000, 6000, 10000][attempt];
                    eprintln!(
                        "⚠️ [STARTUP EXCHANGECLIENT RETRY {}/5] {} hit transient error ({}). Backing off {}ms...",
                        attempt + 1, config.trading_symbol, last_exch_err, wait_ms
                    );
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
            }
        }
    }
    let mut exchange_raw = exchange_opt
        .unwrap_or_else(|| panic!("Failed to create ExchangeClient after 5 attempts: {}", last_exch_err));

    let info_client = Arc::new(
        InfoClient::new(None, Some(base_url.clone()))
            .await
            .expect("Failed to create InfoClient"),
    );

    // Fetch size decimals lookup only for the symbol's clearinghouse to cut startup REST weight by >50%
    let is_hip3_symbol = config.trading_symbol.to_lowercase().starts_with("xyz:");
    let mut sz_map = if is_hip3_symbol {
        std::collections::HashMap::new()
    } else {
        fetch_sz_decimals_map(&info_client).await
    };
    if is_hip3_symbol {
        crate::hyperliquid::register_hip3_dex_assets(&mut exchange_raw.coin_to_asset, &mut sz_map, "xyz", 1).await;
    }
    let resolved_asset_id = exchange_raw.coin_to_asset.get(&config.trading_symbol).copied();
    let exchange = Arc::new(exchange_raw);
    let sz_decimals_map = Arc::new(RwLock::new(sz_map));
    let trading_symbol = config.trading_symbol.clone();
    let trading_sz_decimals = {
        let lock = sz_decimals_map.read().await;
        lock.get(&trading_symbol)
            .copied()
            .unwrap_or_else(|| asset_calibration(&trading_symbol).sz_decimals)
    };
    let startup_fee_sched = AssetFeeSchedule::for_symbol(&trading_symbol);

    println!(
        "📊 [MARKET INFO] Symbol: {} | Asset ID: {:?} | Size Decimals: {} | Fees (Maker/Taker/MM-RT/MT-RT bps): {:.3}/{:.3}/{:.3}/{:.3}",
        trading_symbol,
        resolved_asset_id,
        trading_sz_decimals,
        startup_fee_sched.maker_bps,
        startup_fee_sched.taker_bps,
        startup_fee_sched.maker_maker_rt_bps(),
        startup_fee_sched.maker_taker_rt_bps(),
    );

    // Explicitly configure exchange leverage & margin mode (Cross vs Isolated, with 429 retry)
    let mut lev_res = Err("uninitialized".to_string());
    for attempt in 0..3 {
        match exchange
            .update_leverage(config.hl_leverage, &trading_symbol, config.hl_is_cross, None)
            .await
        {
            Ok(_) => {
                lev_res = Ok(());
                break;
            }
            Err(e) => {
                lev_res = Err(e.to_string());
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(1500 * (attempt as u64 + 1))).await;
                }
            }
        }
    }
    if let Err(e) = lev_res {
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
    let state_map: StateMap = preloaded_state;
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
    let hmm_generation = Arc::new(std::sync::atomic::AtomicU64::new(1));

    let discord_url = config.discord_webhook_url.clone();

    // ------------------------------------------------------------------------
    // COLD-PATH: 24-Hour HMM Recalibration Loop
    // ------------------------------------------------------------------------
    let hmm_trainer_clone = hmm_model.clone();
    let hmm_gen_trainer = hmm_generation.clone();
    let info_trainer_clone = info_client.clone();
    let config_trainer_path = config.hmm_config_path.clone();
    let config_trainer_symbol = trading_symbol.clone();

    tokio::spawn(async move {
        if std::path::Path::new(&config_trainer_path).exists() {
            // Weights already loaded from disk; defer heavy 1000-bar 15m recalibration past the 15-bot startup window
            tokio::time::sleep(Duration::from_millis(120_000 + startup_stagger_ms)).await;
        }
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
                candles.sort_by_key(|c| c.time_open);
                candles.dedup_by_key(|c| c.time_open);
                let cur_utc_ms = chrono::Utc::now().timestamp_millis() as u64;
                if let Some(last) = candles.last() {
                    if last.time_open + 15 * 60 * 1000 > cur_utc_ms {
                        candles.pop();
                    }
                }

                let mut closes = Vec::new();
                for c in candles {
                    if let Ok(close_val) = c.close.parse::<f64>() {
                        if close_val.is_finite() && close_val > 0.0 {
                            closes.push(close_val);
                        }
                    }
                }

                if closes.len() >= 140 {
                    if let Ok(Some(new_hmm)) =
                        tokio::task::spawn_blocking(move || train_hmm_offline(&closes)).await
                    {
                        let _ = new_hmm.save_weights(&config_trainer_path);
                        let mut lock = hmm_trainer_clone.write().await;
                        *lock = new_hmm;
                        let new_gen = hmm_gen_trainer.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        println!(
                            "✅ [COLD-PATH] HMM recalibrated and hot-swapped into live memory successfully (Generation {}).",
                            new_gen
                        );
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(86400)).await;
        }
    });

    // ------------------------------------------------------------------------
    // INGESTION: Pre-fill 960 1m historical candles via REST
    // ------------------------------------------------------------------------
    let mut rolling_closes: Vec<f64> = Vec::with_capacity(960);
    let mut prev_kalman_vel_bps_min: f64 = 0.0;
    println!(
        "⏳ [INGESTION] Pre-filling 960-minute rolling window for {} via Hyperliquid REST...",
        trading_symbol
    );
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let window_start_ms = now_ms.saturating_sub(960 * 60 * 1000);
    let cur_bucket_5m = (now_ms / (5 * 60 * 1000)) * (5 * 60 * 1000);
    let mut rolling_5m_bars: Vec<CandleBar> = Vec::with_capacity(300);
    let mut initial_5m_bar: Option<CandleBar> = None;
    let cur_bucket_15m = (now_ms / (15 * 60 * 1000)) * (15 * 60 * 1000);
    let mut initial_15m_bar: Option<CandleBar> = None;
    let mut initial_last_1m_open: u64 = 0;

    let mut res_1m_candles = Err("uninitialized".to_string());
    for attempt in 0..3 {
        match info_client
            .candles_snapshot(
                trading_symbol.clone(),
                "1m".to_string(),
                window_start_ms,
                now_ms,
            )
            .await
        {
            Ok(c) => {
                res_1m_candles = Ok(c);
                break;
            }
            Err(e) => {
                let err_str = e.to_string();
                res_1m_candles = Err(err_str);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
    }

    match res_1m_candles {
        Ok(mut candles) => {
            candles.sort_by_key(|c| c.time_open);
            candles.dedup_by_key(|c| c.time_open);
            // Exclude potentially incomplete final candle returned by REST
            if let Some(last) = candles.last() {
                if last.time_open + 60_000 > now_ms {
                    candles.pop();
                }
            }
            if let Some(last) = candles.last() {
                initial_last_1m_open = last.time_open;
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

                    let b_5m = (c.time_open / (5 * 60 * 1000)) * (5 * 60 * 1000);
                    if b_5m < cur_bucket_5m {
                        if let Some(ref mut prev_5m) = initial_5m_bar {
                            if prev_5m.time_open == b_5m {
                                prev_5m.high = prev_5m.high.max(high_f);
                                prev_5m.low = prev_5m.low.min(low_f);
                                prev_5m.close = close_f;
                                prev_5m.volume += vol_f;
                            } else {
                                rolling_5m_bars.push(prev_5m.clone());
                                if rolling_5m_bars.len() > 300 {
                                    rolling_5m_bars.remove(0);
                                }
                                *prev_5m = CandleBar {
                                    time_open: b_5m,
                                    open: open_f,
                                    high: high_f,
                                    low: low_f,
                                    close: close_f,
                                    volume: vol_f,
                                };
                            }
                        } else {
                            initial_5m_bar = Some(CandleBar {
                                time_open: b_5m,
                                open: open_f,
                                high: high_f,
                                low: low_f,
                                close: close_f,
                                volume: vol_f,
                            });
                        }
                    } else if let Some(prev_5m) = initial_5m_bar.take() {
                        if prev_5m.time_open < cur_bucket_5m {
                            rolling_5m_bars.push(prev_5m);
                            if rolling_5m_bars.len() > 300 {
                                rolling_5m_bars.remove(0);
                            }
                            initial_5m_bar = Some(CandleBar {
                                time_open: cur_bucket_5m,
                                open: open_f,
                                high: high_f,
                                low: low_f,
                                close: close_f,
                                volume: vol_f,
                            });
                        } else {
                            let mut cur = prev_5m;
                            cur.high = cur.high.max(high_f);
                            cur.low = cur.low.min(low_f);
                            cur.close = close_f;
                            cur.volume += vol_f;
                            initial_5m_bar = Some(cur);
                        }
                    } else {
                        initial_5m_bar = Some(CandleBar {
                            time_open: cur_bucket_5m,
                            open: open_f,
                            high: high_f,
                            low: low_f,
                            close: close_f,
                            volume: vol_f,
                        });
                    }

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
            if let Some(ref rem_5m) = initial_5m_bar {
                if rem_5m.time_open < cur_bucket_5m {
                    rolling_5m_bars.push(rem_5m.clone());
                    if rolling_5m_bars.len() > 300 {
                        rolling_5m_bars.remove(0);
                    }
                    initial_5m_bar = None;
                }
            }
            println!(
                "✅ [INGESTION] Successfully loaded {} historical 1m bars ({} closed 5m bars) for {}.",
                rolling_closes.len(),
                rolling_5m_bars.len(),
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
    let mut res_15m_candles = Err("uninitialized".to_string());
    for attempt in 0..3 {
        match info_client
            .candles_snapshot(
                trading_symbol.clone(),
                "15m".to_string(),
                window_15m_start_ms,
                now_ms,
            )
            .await
        {
            Ok(c) => {
                res_15m_candles = Ok(c);
                break;
            }
            Err(e) => {
                let err_str = e.to_string();
                res_15m_candles = Err(err_str);
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
    }

    match res_15m_candles {
        Ok(mut candles) => {
            candles.sort_by_key(|c| c.time_open);
            candles.dedup_by_key(|c| c.time_open);
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

    // Package 9: Pre-fill 260 historical 1h bars for Engine B v2 (1H EMA12/EMA50/EMA200 Pullback-in-Trend)
    let mut engine_b_1h = TrendPullback1hEngine::new();
    let mut current_1h_bar: Option<CandleBar> = None;
    let start_1h_ms = now_ms.saturating_sub(260 * 60 * 60 * 1000);
    println!(
        "⏳ [INGESTION] Pre-filling 260h historical 1h candles for {} via Hyperliquid REST...",
        trading_symbol
    );
    let mut res_1h_candles = Err("Unattempted".to_string());
    for attempt in 0..3 {
        match info_client
            .candles_snapshot(trading_symbol.clone(), "1h".to_string(), start_1h_ms, now_ms)
            .await
        {
            Ok(c) => {
                res_1h_candles = Ok(c);
                break;
            }
            Err(e) => {
                res_1h_candles = Err(format!("{:?}", e));
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
    }
    match res_1h_candles {
        Ok(mut candles) => {
            candles.sort_by_key(|c| c.time_open);
            candles.dedup_by_key(|c| c.time_open);
            let mut bars_1h_init = Vec::with_capacity(candles.len());
            for c in candles {
                if let (Ok(open_f), Ok(high_f), Ok(low_f), Ok(close_f)) = (
                    c.open.parse::<f64>(),
                    c.high.parse::<f64>(),
                    c.low.parse::<f64>(),
                    c.close.parse::<f64>(),
                ) {
                    let vol_f = c.vlm.parse::<f64>().unwrap_or(0.0);
                    let bar_1h = CandleBar {
                        time_open: c.time_open,
                        open: open_f,
                        high: high_f,
                        low: low_f,
                        close: close_f,
                        volume: vol_f,
                    };
                    if c.time_open + 3_600_000 > now_ms {
                        current_1h_bar = Some(bar_1h);
                    } else {
                        bars_1h_init.push(bar_1h);
                    }
                }
            }
            engine_b_1h.warmup_from_1h_bars(&bars_1h_init);
            println!(
                "✅ [INGESTION] Warmed up Engine B v2 (1H EMA50 Pullback) with {} closed 1h bars for {} (EMA12=${:.2}, EMA50=${:.2}, EMA200=${:.2}, ATR_1h=${:.4}).",
                engine_b_1h.bars_seen,
                trading_symbol,
                engine_b_1h.ema12,
                engine_b_1h.ema50,
                engine_b_1h.ema200,
                engine_b_1h.atr_1h
            );
        }
        Err(e) => {
            eprintln!(
                "❌ [INGESTION ERROR] Failed to fetch 1h candles for {}: {}",
                trading_symbol, e
            );
        }
    }

    // Markov-Polo V2 (Package 10): Pre-fill 260 historical 4h bars (~43 days) for MacroTrend4hEngine (4H_ST3 + 4H_D40)
    let mut engine_b_4h = MacroTrend4hEngine::new();
    let mut current_4h_bar: Option<CandleBar> = None;
    let start_4h_ms = now_ms.saturating_sub(260 * 4 * 60 * 60 * 1000);
    println!(
        "⏳ [INGESTION] Pre-filling 260x4h (~43d) historical 4h candles for {} via Hyperliquid REST...",
        trading_symbol
    );
    let mut res_4h_candles = Err("Unattempted".to_string());
    for attempt in 0..3 {
        match info_client
            .candles_snapshot(trading_symbol.clone(), "4h".to_string(), start_4h_ms, now_ms)
            .await
        {
            Ok(c) => {
                res_4h_candles = Ok(c);
                break;
            }
            Err(e) => {
                res_4h_candles = Err(format!("{:?}", e));
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
    }
    match res_4h_candles {
        Ok(mut candles) => {
            candles.sort_by_key(|c| c.time_open);
            candles.dedup_by_key(|c| c.time_open);
            let mut bars_4h_init = Vec::with_capacity(candles.len());
            for c in candles {
                if let (Ok(open_f), Ok(high_f), Ok(low_f), Ok(close_f)) = (
                    c.open.parse::<f64>(),
                    c.high.parse::<f64>(),
                    c.low.parse::<f64>(),
                    c.close.parse::<f64>(),
                ) {
                    let vol_f = c.vlm.parse::<f64>().unwrap_or(0.0);
                    let bar_4h = CandleBar {
                        time_open: c.time_open,
                        open: open_f,
                        high: high_f,
                        low: low_f,
                        close: close_f,
                        volume: vol_f,
                    };
                    if c.time_open + 14_400_000 > now_ms {
                        current_4h_bar = Some(bar_4h);
                    } else {
                        bars_4h_init.push(bar_4h);
                    }
                }
            }
            engine_b_4h.warmup_from_4h_bars(&bars_4h_init);
            println!(
                "✅ [INGESTION] Warmed up Markov-Polo V2 4H Macro Engine (ST3 + Donchian40) with {} closed 4h bars for {} (EMA20=${:.2}, EMA100=${:.2}, ST_dir={}, ATR_4h=${:.4}).",
                engine_b_4h.bars_seen,
                trading_symbol,
                engine_b_4h.ema20,
                engine_b_4h.ema100,
                engine_b_4h.st_dir,
                engine_b_4h.atr_4h
            );
        }
        Err(e) => {
            eprintln!(
                "❌ [INGESTION ERROR] Failed to fetch 4h candles for {}: {}",
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
    // STARTUP RECONCILIATION: Deep Sync with Live Exchange Positions & Orders
    // ------------------------------------------------------------------------
    println!("🔍 [STARTUP RECONCILIATION] Initiating deep exchange synchronization for {}...", trading_symbol);

    // 1. Query live positions with fail-closed retries (429-resilient backoff)
    let mut retry_count = 0;
    let (startup_szi, startup_entry_px) = loop {
        match get_user_position(&info_client, user_address, &trading_symbol).await {
            Ok(pos) => break pos,
            Err(e) => {
                retry_count += 1;
                if retry_count >= 5 {
                    panic!("❌ [FATAL STARTUP] Could not query user position for {}: {}. Halting startup fail-closed.", trading_symbol, e);
                }
                let wait_ms = [1500u64, 3000, 5000, 8000][retry_count - 1];
                tokio::time::sleep(tokio::time::Duration::from_millis(wait_ms)).await;
            }
        }
    };

    // 2. Query live open orders with fail-closed retries (429-resilient backoff)
    retry_count = 0;
    let open_orders_res = loop {
        match fetch_open_orders_for_coin(&info_client, user_address, &trading_symbol).await {
            Ok(orders) => break orders,
            Err(e) => {
                retry_count += 1;
                if retry_count >= 5 {
                    panic!("❌ [FATAL STARTUP] Could not query open orders for {}: {}. Halting startup fail-closed.", trading_symbol, e);
                }
                let wait_ms = [1500u64, 3000, 5000, 8000][retry_count - 1];
                tokio::time::sleep(tokio::time::Duration::from_millis(wait_ms)).await;
            }
        }
    };
    let symbol_open_orders: Vec<_> = open_orders_res.into_iter().filter(|o| o.coin == trading_symbol).collect();

    // Query live account equity for dynamic sizing (5-attempt 429-resilient backoff)
    let eff_eq_frac_startup = effective_equity_fraction_for_config(&config);
    let mut startup_eq_res = Err("uninitialized".to_string());
    for attempt in 0..5 {
        match get_account_value(&info_client, user_address).await {
            Ok(eq) if eq > 1.0 => {
                startup_eq_res = Ok(eq);
                break;
            }
            Ok(eq) => {
                startup_eq_res = Ok(eq);
                if attempt < 4 {
                    let wait_ms = [1500u64, 3000, 5000, 8000][attempt];
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
            }
            Err(e) => {
                startup_eq_res = Err(e);
                if attempt < 4 {
                    let wait_ms = [1500u64, 3000, 5000, 8000][attempt];
                    tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                }
            }
        }
    }
    let initial_equity = match startup_eq_res {
        Ok(eq) => {
            println!(
                "💰 [STARTUP ACCOUNT EQUITY] Equity: ${:.2} USDC | Mode: {} | Fraction: {:.1}% | Initial Trade Bank: ${:.2} USDC",
                eq,
                config.sizing_mode.to_uppercase(),
                eff_eq_frac_startup * 100.0,
                calculate_dynamic_notional(
                    eq,
                    eff_eq_frac_startup,
                    11.0,
                    config.max_position_size_usdc,
                    config.position_size_usdc,
                )
            );
            eq
        }
        Err(e) => {
            println!(
                "⚠️ [STARTUP ACCOUNT EQUITY] Could not query initial account equity: {}. Using fallback base size: ${:.2} USDC",
                e, config.position_size_usdc
            );
            0.0
        }
    };
    let cached_account_equity = Arc::new(RwLock::new(initial_equity));

    // 3. Load local state and order intent WAL
    let pending_intent = crate::state::load_order_intent(&trading_symbol);

    {
        let mut state_lock = active_state.lock().await;
        let eps = 1e-5;

        if startup_szi.abs() > eps {
            // Case A: Real open position exists on Hyperliquid
            let ep = startup_entry_px.unwrap_or(0.0);
            let is_long = startup_szi > 0.0;
            let side_str = if is_long { "LONG" } else { "SHORT" };
            let sl_dist = ep * 0.035;
            let default_sl = quantize_price(
                if is_long { ep - sl_dist } else { ep + sl_dist },
                trading_sz_decimals,
            );
            let reconcile_engine_tag = state_lock
                .get(&trading_symbol)
                .map(|p| p.engine_type.clone())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| "EngineB4h".to_string());

            // Ensure cluster lease is held or acquired under the true Macro V2 engine tag
            let current_lease = crate::state::get_cluster_lease_for_symbol(&trading_symbol);
            let (lease_uuid, fencing_tok) = if let Some(lease) = current_lease {
                (lease.owner_uuid, lease.fencing_token)
            } else {
                let uuid = format!("{}_{}", trading_symbol, chrono::Utc::now().timestamp_micros());
                let (_, tok) = crate::state::try_acquire_cluster_lease_with_side(
                    &trading_symbol,
                    &reconcile_engine_tag,
                    side_str,
                    &uuid,
                    3600,
                );
                (uuid, tok)
            };

            if let Some(pos) = state_lock.get_mut(&trading_symbol) {
                println!(
                    "🔍 [STARTUP RECONCILIATION] Active {} position confirmed on Hyperliquid for {} (size: {:.5}, entry: ${:.4}, SL: ${:.4}). Matching local state.",
                    pos.engine_type, trading_symbol, startup_szi, pos.entry_price, pos.stop_loss_price
                );
                pos.is_filled = true;
                pos.amount = startup_szi.abs();
                pos.lifecycle = crate::state::PositionLifecycle::Active;
                if pos.stop_loss_price <= 0.0 {
                    pos.stop_loss_price = default_sl;
                }
                if pos.chandelier_stop <= 0.0 {
                    pos.chandelier_stop = pos.stop_loss_price;
                }
                if pos.active_multiplier <= 0.0 {
                    pos.active_multiplier = 4.0;
                }
                if !lease_uuid.is_empty() && fencing_tok > 0 {
                    pos.lease_owner_uuid = lease_uuid;
                    pos.lease_fencing_token = fencing_tok;
                }
                if !pos.tp_order_id.is_empty()
                    && !symbol_open_orders
                        .iter()
                        .any(|o| o.oid.to_string() == pos.tp_order_id)
                {
                    pos.tp_order_id.clear();
                }
            } else {
                println!(
                    "⚠️ [STARTUP RECONCILIATION] Found unmanaged live position on Hyperliquid for {} (size: {:.5}, entry: ${:.4}). Adopting as Markov-Polo V2 EngineB4h runner with 3.5% SL (${:.4}) and 4.0x ATR trail.",
                    trading_symbol, startup_szi, ep, default_sl
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
                        take_profit_price: 0.0,
                        opened_at: chrono::Utc::now().to_rfc3339(),
                        risk_amount_usdc: startup_szi.abs() * ep,
                        is_filled: true,
                        half_life_minutes: 288.0 * 60.0,
                        client_order_id: "".to_string(),
                        max_progress: 0.0,
                        max_favorable_price: ep,
                        effective_tp: 0.0,
                        last_reprice_time: chrono::Utc::now().timestamp_millis(),
                        reprice_count: 0,
                        candidate_lower_count: 0,
                        engine_type: "EngineB4h".to_string(),
                        chandelier_stop: default_sl,
                        active_multiplier: 4.0,
                        current_atr: sl_dist / 2.5,
                        lease_fencing_token: fencing_tok,
                        lease_owner_uuid: lease_uuid,
                        lifecycle: crate::state::PositionLifecycle::Active,
                        frozen_center_price: ep,
                        frozen_sigma_eq_log: 0.0045,
                        consecutive_ou_breach_5m_bars: 0,
                        consecutive_shock_bars: 0,
                        breakeven_ratchet_armed: false,
                        entry_ttl_mins: 3.0,
                        disaster_stop_oid: None,
                        disaster_stop_ratcheted_to_be: false,
                        last_fill_check: 0,
                        tranche1_tp_filled: false,
                        last_peak_mins: 0.0,
                        cusum_stat_1m: 0.0,
                        entry_sigma_1m: 0.0,
                    },
                );
            }

            // Cancel only lingering entry orders on exchange since position is already filled;
            // preserve both the resting Maker TP order and the Exchange-Native Disaster Stop-Market trigger order.
            for o in &symbol_open_orders {
                let is_obsolete_entry = state_lock
                    .get(&trading_symbol)
                    .map(|p| !p.order_id.is_empty() && p.order_id == o.oid.to_string())
                    .unwrap_or(false);
                if is_obsolete_entry {
                    println!("🧹 [STARTUP RECONCILIATION] Cancelling obsolete resting entry order {} for filled position on {}.", o.oid, trading_symbol);
                    let _ = cancel_order(&exchange, &trading_symbol, o.oid).await;
                }
            }

            // Clear any pending intent WAL
            crate::state::clear_order_intent(&trading_symbol);
            let _ = save_state(&state_file_path, &state_lock);
        } else {
            // Case B: Position is flat on exchange (szi == 0.0)
            if !symbol_open_orders.is_empty() {
                let mut matched_entry_oid: Option<u64> = None;

                // Correlate with intent WAL
                if let Some(ref intent) = pending_intent {
                    for o in &symbol_open_orders {
                        let matches_oid = intent.exchange_order_id == Some(o.oid);
                        let matches_price_sz = (o.limit_px.parse::<f64>().unwrap_or(0.0) - intent.limit_px).abs() < 1e-4
                            && (o.sz.parse::<f64>().unwrap_or(0.0) - intent.sz).abs() < 1e-4;
                        if matches_oid || matches_price_sz {
                            matched_entry_oid = Some(o.oid);
                            println!("✅ [STARTUP RECONCILIATION] Adopted resting order {} from OrderIntent WAL for {}.", o.oid, trading_symbol);

                            let reconstructed = crate::state::PositionState {
                                symbol: trading_symbol.clone(),
                                side: intent.side.to_string(),
                                order_id: o.oid.to_string(),
                                tp_order_id: "".to_string(),
                                entry_price: intent.limit_px,
                                amount: intent.sz,
                                stop_loss_price: intent.stop_loss,
                                take_profit_price: intent.target_tp,
                                opened_at: chrono::Utc::now().to_rfc3339(),
                                risk_amount_usdc: intent.position_size_usdc,
                                is_filled: false,
                                half_life_minutes: intent.half_life_minutes,
                                client_order_id: intent.client_order_id.clone(),
                                max_progress: 0.0,
                                max_favorable_price: intent.limit_px,
                                effective_tp: intent.target_tp,
                                last_reprice_time: chrono::Utc::now().timestamp_millis(),
                                reprice_count: 0,
                                candidate_lower_count: 0,
                                engine_type: intent.engine_type.clone(),
                                chandelier_stop: if intent.engine_type == "EngineB"
                                    || intent.engine_type == "EngineB1h"
                                    || intent.engine_type == "EngineB4h"
                                {
                                    intent.stop_loss
                                } else {
                                    0.0
                                },
                                active_multiplier: if intent.engine_type == "EngineB1h"
                                    || intent.engine_type == "EngineB4h"
                                {
                                    4.0
                                } else if intent.engine_type == "EngineB" {
                                    2.0
                                } else {
                                    0.0
                                },
                                current_atr: 0.0,
                                lease_fencing_token: intent.fencing_token,
                                lease_owner_uuid: intent.owner_uuid.clone(),
                                lifecycle: crate::state::PositionLifecycle::Active,
                                frozen_center_price: intent.target_tp,
                                frozen_sigma_eq_log: 0.0045,
                                consecutive_ou_breach_5m_bars: 0,
                                consecutive_shock_bars: 0,
                                breakeven_ratchet_armed: false,
                                entry_ttl_mins: 3.0,
                                disaster_stop_oid: None,
                                disaster_stop_ratcheted_to_be: false,
                                last_fill_check: 0,
                                tranche1_tp_filled: false,
                                last_peak_mins: 0.0,
                                cusum_stat_1m: 0.0,
                                entry_sigma_1m: 0.0,
                            };
                            state_lock.insert(trading_symbol.clone(), reconstructed);
                            let _ = save_state(&state_file_path, &state_lock);
                            break;
                        }
                    }
                }

                // If not matched with intent, check existing local state
                if matched_entry_oid.is_none() {
                    if let Some(pos) = state_lock.get_mut(&trading_symbol) {
                        for o in &symbol_open_orders {
                            if pos.order_id == o.oid.to_string() {
                                matched_entry_oid = Some(o.oid);
                                pos.is_filled = false;
                                pos.lifecycle = crate::state::PositionLifecycle::Active;
                                println!("✅ [STARTUP RECONCILIATION] Reconciled existing resting entry order {} for {}.", o.oid, trading_symbol);
                                break;
                            }
                        }
                    }
                }

                // Any order NOT matched with intent or state is an orphaned unrecognized order: cancel it!
                for o in &symbol_open_orders {
                    if Some(o.oid) != matched_entry_oid {
                        println!("⚠️ [STARTUP RECONCILIATION] Cancelling unrecognized orphan order {} on Hyperliquid for {}.", o.oid, trading_symbol);
                        let _ = cancel_order(&exchange, &trading_symbol, o.oid).await;
                    }
                }

                crate::state::clear_order_intent(&trading_symbol);
            } else {
                // Both position and open orders are zero on exchange!
                if pending_intent.is_some() {
                    println!("🧹 [STARTUP RECONCILIATION] Cleared unfulfilled OrderIntent WAL for {}.", trading_symbol);
                    crate::state::clear_order_intent(&trading_symbol);
                }

                // If local state exists, execute defensive release sequence
                if let Some(pos_clone) = state_lock.get(&trading_symbol).cloned() {
                    println!("🧹 [STARTUP RECONCILIATION] Local state present but exchange flat with zero orders. Executing defensive release sequence for {}...", trading_symbol);
                    if let Some(pos) = state_lock.get_mut(&trading_symbol) {
                        pos.lifecycle = crate::state::PositionLifecycle::CloseConfirmed;
                    }
                    let _ = save_state(&state_file_path, &state_lock);

                    let _ = crate::state::release_cluster_lease_authenticated(
                        &trading_symbol,
                        Some(&pos_clone.lease_owner_uuid),
                        Some(pos_clone.lease_fencing_token),
                    );

                    if let Some(pos) = state_lock.get_mut(&trading_symbol) {
                        pos.lifecycle = crate::state::PositionLifecycle::LeaseReleased;
                    }
                    let _ = save_state(&state_file_path, &state_lock);

                    state_lock.remove(&trading_symbol);
                    let _ = save_state(&state_file_path, &state_lock);
                }
            }
        }
    }

    println!("🚀 [STARTUP RECONCILIATION COMPLETE] Position and orders verified on Hyperliquid. Bot status: READY for {}.", trading_symbol);

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
    let obi_tracker = Arc::new(RwLock::new(L2ObiTracker::default()));
    let watcher_obi = obi_tracker.clone();
    let live_funding_rate = Arc::new(RwLock::new(0.0_f64));
    let crowding_tracker = Arc::new(RwLock::new(MarketCrowdingTracker::default()));
    let watcher_crowding = crowding_tracker.clone();
    let high_water_mark_equity = Arc::new(RwLock::new(initial_equity));
    let sentinel_notify = Arc::new(Notify::new());
    let watcher_notify = sentinel_notify.clone();
    let last_1m_close_shared = Arc::new(RwLock::new(0.0_f64));

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1500));
        let mut stop_breach_streak: u8 = 0;
        loop {
            tokio::select! {
                _ = interval.tick() => {},
                _ = watcher_notify.notified() => {},
            }

            // 1. Snapshot state quickly without holding lock during network calls
            let pos_opt = {
                let state_lock = watcher_state.lock().await;
                state_lock.get(&watcher_symbol).cloned()
            };

            let pos = match pos_opt {
                Some(p) => p,
                None => {
                    stop_breach_streak = 0;
                    continue;
                }
            };

            // Cross-reference user position on live exchange (Fail-Closed)
            let (mut current_szi, _) = match get_user_position(&watcher_info, user_address, &watcher_symbol).await {
                Ok(res) => res,
                Err(e) => {
                    eprintln!(
                        "⚠️ [FAST-SENTINEL] API failure fetching position for {}: {}. Retaining state (fail-closed).",
                        watcher_symbol, e
                    );
                    continue;
                }
            };
            let pos_exists = if pos.side == "LONG" {
                current_szi > 0.0
            } else {
                current_szi < 0.0
            };

            // -------------------------------------------------------------
            // Defense Layer 0: DST-Aware Session Flat-at-Close Guard (HIP-3 RWA)
            // -------------------------------------------------------------
            if let SessionLifecycleAction::ForceFlat(flat_reason) =
                evaluate_session_lifecycle_gate(&watcher_symbol, chrono::Utc::now().timestamp_millis())
            {
                if pos.is_filled || pos_exists || !pos.order_id.is_empty() {
                    println!(
                        "🔔 [FAST-SENTINEL FLAT-AT-CLOSE] {} triggering scheduled session close flatten ({}).",
                        watcher_symbol, flat_reason
                    );
                    let flat_msg = format!(
                        "🔔 **SCHEDULED SESSION FLAT-AT-CLOSE (HL):** {} ({}) closed ahead of market close/maintenance ({}).",
                        watcher_symbol, pos.side, flat_reason
                    );
                    crate::discord::send_alert(watcher_discord.as_deref(), &flat_msg).await;
                    let mut state_lock = watcher_state.lock().await;
                    let _ = execute_emergency_flatten_with_retry(
                        &watcher_state_path,
                        &watcher_exchange,
                        &watcher_info,
                        user_address,
                        &mut state_lock,
                        &watcher_symbol,
                        watcher_sz_decimals,
                        watcher_discord.clone(),
                    )
                    .await;
                    continue;
                }
            }

            // -------------------------------------------------------------
            // Defense Layer 1 (F-07 Hardened): Mark-Price OR 3-Tick Microprice Persistence Stop Loss
            // Prevents 1-second L2 BBO flash vacuums from triggering Taker IOC dumps while markPx is calm.
            // -------------------------------------------------------------
            if pos_exists && pos.stop_loss_price > 0.0 {
                let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let (ws_quotes, ws_micro_px) = {
                    let guard = watcher_obi.read().await;
                    if guard.is_fresh_and_uncrossed(now_ms, L2_ORDERBOOK_FRESH_MS) {
                        let micro = guard.compute_micro_price();
                        (Some((guard.best_bid, guard.best_ask)), micro)
                    } else {
                        (None, 0.0)
                    }
                };
                let quotes_res = match ws_quotes {
                    Some(q) => Ok(q),
                    None => fetch_book_ticker(&watcher_info, &watcher_symbol).await,
                };
                let (mark_px_now, mark_ts_now) = {
                    let cg = watcher_crowding.read().await;
                    (cg.mark_px, cg.last_mark_update_ms)
                };
                let mark_fresh = mark_px_now > 0.0 && now_ms.saturating_sub(mark_ts_now) <= 12_000;

                if let Ok((best_bid, best_ask)) = quotes_res {
                    let mid_now = 0.5 * (best_bid + best_ask);
                    let spread_bps = if mid_now > 0.0 && best_ask > best_bid {
                        ((best_ask - best_bid) / mid_now) * 10_000.0
                    } else {
                        999.0
                    };
                    let micro_px = if ws_micro_px > 0.0 { ws_micro_px } else { mid_now };

                    let mark_breached = mark_fresh
                        && if pos.side == "LONG" {
                            mark_px_now <= pos.stop_loss_price
                        } else {
                            mark_px_now >= pos.stop_loss_price
                        };
                    let micro_breached = spread_bps < 25.0
                        && micro_px > 0.0
                        && if pos.side == "LONG" {
                            micro_px <= pos.stop_loss_price
                        } else {
                            micro_px >= pos.stop_loss_price
                        };

                    if micro_breached {
                        stop_breach_streak = stop_breach_streak.saturating_add(1);
                    } else {
                        stop_breach_streak = 0;
                    }

                    let req_ticks = if pos.engine_type == "EngineB" { 2 } else { 3 };
                    let is_confirmed_breach =
                        mark_breached || (micro_breached && stop_breach_streak >= req_ticks);

                    if is_confirmed_breach {
                        stop_breach_streak = 0;
                        let breach_px = if mark_breached {
                            mark_px_now
                        } else if pos.side == "LONG" {
                            best_bid
                        } else {
                            best_ask
                        };
                        let trigger_src = if mark_breached { "MarkPx" } else { "Microprice-3Tick" };
                        let is_profit_lock = if pos.side == "LONG" {
                            pos.entry_price > 0.0 && breach_px > pos.entry_price * 1.0005
                        } else {
                            pos.entry_price > 0.0 && breach_px < pos.entry_price * 0.9995
                        };

                        if is_profit_lock {
                            println!(
                                "🟢 [FAST-SENTINEL PROFIT-LOCK TRAILING STOP ({})] {} hit profit-locked stop at ${:.4} (Entry: ${:.4}, Current: ${:.4})! Harvesting profit...",
                                trigger_src, watcher_symbol, pos.stop_loss_price, pos.entry_price, breach_px
                            );
                            let profit_msg = format!(
                                "🟢 **PROFIT-LOCK TRAILING STOP HIT (FAST-SENTINEL {}):** {}\n**Side:** {}\n**Entry:** ${:.4}\n**Harvest Price:** ${:.4}\nClosing position in profit.",
                                trigger_src, watcher_symbol, pos.side, pos.entry_price, breach_px
                            );
                            crate::discord::send_alert(watcher_discord.as_deref(), &profit_msg).await;
                        } else {
                            println!(
                                "🛑 [FAST-SENTINEL 1.5s STOP LOSS ({})] {} breached catastrophe stop at ${:.4} (Current: ${:.4}, Spread: {:.1} bps)! Emergency flattening...",
                                trigger_src, watcher_symbol, pos.stop_loss_price, breach_px, spread_bps
                            );
                            let stop_msg = format!(
                                "🛑 **CATASTROPHE STOP LOSS HIT (FAST-SENTINEL {}):** {}\n**Side:** {}\n**Breach Price:** ${:.4}\n**Hard Stop:** ${:.4}\nEmergency flattening immediately.",
                                trigger_src, watcher_symbol, pos.side, breach_px, pos.stop_loss_price
                            );
                            crate::discord::send_alert(watcher_discord.as_deref(), &stop_msg).await;
                        }

                        {
                            let mut state_lock = watcher_state.lock().await;
                            let _ = execute_emergency_flatten_with_retry(
                                &watcher_state_path,
                                &watcher_exchange,
                                &watcher_info,
                                user_address,
                                &mut state_lock,
                                &watcher_symbol,
                                watcher_sz_decimals,
                                watcher_discord.clone(),
                            )
                            .await;
                        }

                        let mut cb_lock = watcher_cb.lock().await;
                        if is_profit_lock {
                            cb_lock.record_win();
                        } else {
                            cb_lock.record_loss();
                        }
                        save_circuit_breaker(&watcher_cb_path, &cb_lock);
                        continue;
                    }
                }
            }

            // Update last fill check timestamp
            {
                let mut state_lock = watcher_state.lock().await;
                if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                    p.last_fill_check = chrono::Utc::now().timestamp_millis() as u64;
                }
            }

            // -------------------------------------------------------------
            // Scenario 1: Entry order pending or position open without resting TP
            // -------------------------------------------------------------
            if pos.tp_order_id.is_empty() {
                let mut is_filled = pos_exists || pos.is_filled;
                let mut is_truly_dead = false;
                let mut reported_status = "UNKNOWN".to_string();

                let order_age_secs = if let Ok(opened_dt) = chrono::DateTime::parse_from_rfc3339(&pos.opened_at) {
                    (chrono::Utc::now() - opened_dt.with_timezone(&chrono::Utc)).num_seconds()
                } else {
                    10
                };

                // Compute needs_poll for this position (recompute outside lock)
                let needs_poll = pos.is_filled
                    || !pos.tp_order_id.is_empty()
                    || pos.disaster_stop_oid.is_some()
                    || pos.last_fill_check < chrono::Utc::now().timestamp_millis() as u64 - 30_000;

                if !pos.order_id.is_empty() && !pos.is_filled && needs_poll {
                    if let Ok(oid) = pos.order_id.parse::<u64>() {
                        let open_orders = match fetch_open_orders_for_coin(&watcher_info, user_address, &watcher_symbol).await {
                            Ok(o) => o,
                            Err(e) => {
                                eprintln!(
                                    "⚠️ [FAST-SENTINEL] API failure querying open_orders for {}: {}. Retaining state (fail-closed).",
                                    watcher_symbol, e
                                );
                                continue;
                            }
                        };
                        let is_resting = open_orders.iter().any(|o| o.oid == oid);

                        if is_resting {
                            if pos_exists {
                                // Partially filled while still resting: allow up to 90s for the full Maker order to fill
                                // unless >=95% filled or price has already bounced >=12 bps away from our entry limit.
                                let fill_ratio = if pos.amount > 0.0 {
                                    current_szi.abs() / pos.amount
                                } else {
                                    1.0
                                };
                                let (best_bid_now, best_ask_now) = {
                                    let guard = watcher_obi.read().await;
                                    (guard.best_bid, guard.best_ask)
                                };
                                let moved_away_in_favor = if pos.side == "LONG" {
                                    best_bid_now > 0.0 && best_bid_now >= pos.entry_price * 1.0012
                                } else {
                                    best_ask_now > 0.0 && best_ask_now <= pos.entry_price * 0.9988
                                };

                                if fill_ratio >= 0.95 || order_age_secs >= 90 || moved_away_in_favor {
                                    println!(
                                        "ℹ️ [FAST-SENTINEL PARTIAL FILL FINALIZED] {} filled {:.1}% ({:.4}/{:.4}) after {}s. Cancelling remaining entry order to place TP.",
                                        watcher_symbol,
                                        fill_ratio * 100.0,
                                        current_szi.abs(),
                                        pos.amount,
                                        order_age_secs
                                    );
                                    let _ = cancel_order(&watcher_exchange, &watcher_symbol, oid).await;
                                    if let Ok((fresh_szi, _)) = get_user_position(&watcher_info, user_address, &watcher_symbol).await {
                                        if fresh_szi.abs() > current_szi.abs() {
                                            current_szi = fresh_szi;
                                        }
                                    }
                                } else {
                                    // Keep resting so the remaining Maker size can fill; L2 stop-loss above already guards current_szi
                                    is_filled = false;
                                }
                            } else {
                                // Toxic-OBI Queue Protection: cancel resting maker entry if book depth collapses against order
                                let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                                let toxic_obi = {
                                    let guard = watcher_obi.read().await;
                                    if guard.is_fresh_and_uncrossed(now_ms, 2500) {
                                        if pos.side == "LONG" {
                                            guard.instant_obi <= -0.45 && guard.ewma_1s_obi <= -0.35
                                        } else {
                                            guard.instant_obi >= 0.45 && guard.ewma_1s_obi >= 0.35
                                        }
                                    } else {
                                        false
                                    }
                                };
                                if toxic_obi {
                                    println!(
                                        "🛡️ [FAST-SENTINEL TOXIC-OBI ABORT] Order book collapsing against resting {} entry {} on {}. Cancelling before adverse sweep!",
                                        pos.side, oid, watcher_symbol
                                    );
                                    is_truly_dead = true;
                                    reported_status = "TOXIC_OBI_ABORT".to_string();
                                } else if pos.engine_type != "EngineB"
                                    && check_macro_calendar_veto(now_ms).is_some()
                                {
                                    let macro_msg = check_macro_calendar_veto(now_ms).unwrap_or_default();
                                    println!(
                                        "🛡️ [FAST-SENTINEL MACRO CALENDAR ABORT] Cancelling resting {} entry {} on {} prior to scheduled macro release: {}",
                                        pos.side, oid, watcher_symbol, macro_msg
                                    );
                                    is_truly_dead = true;
                                    reported_status = "MACRO_CALENDAR_ABORT".to_string();
                                } else if let Some(btc_veto) = check_btc_lead_lag_veto(
                                    &watcher_symbol,
                                    pos.side == "LONG",
                                    pos.engine_type == "EngineB",
                                    now_ms,
                                ) {
                                    println!(
                                        "🛡️ [FAST-SENTINEL BTC IMPULSE ABORT] Cancelling resting {} entry {} on {} due to adverse BTC lead impulse: {}",
                                        pos.side, oid, watcher_symbol, btc_veto
                                    );
                                    is_truly_dead = true;
                                    reported_status = "BTC_IMPULSE_ABORT".to_string();
                                } else if pos.engine_type != "EngineB" && order_age_secs >= 45 && pos.entry_price > 0.0 {
                                    // Package 6 Price-Runaway Early Cancel (45s Guard): free slot immediately if bounce already ran +12 bps without filling
                                    let mid_run = {
                                        let guard = watcher_obi.read().await;
                                        if guard.is_fresh_and_uncrossed(now_ms, L2_ORDERBOOK_FRESH_MS) {
                                            0.5 * (guard.best_bid + guard.best_ask)
                                        } else {
                                            0.0
                                        }
                                    };
                                    let ran_away = if mid_run > 0.0 {
                                        if pos.side == "LONG" {
                                            (mid_run - pos.entry_price) / pos.entry_price >= 0.0012
                                        } else {
                                            (pos.entry_price - mid_run) / pos.entry_price >= 0.0012
                                        }
                                    } else {
                                        false
                                    };
                                    if ran_away {
                                        println!(
                                            "⏱️ [FAST-SENTINEL PRICE-RUNAWAY CANCEL] {} {} entry {} unfilled after {}s while price ran >= +12 bps (mid: ${:.4} vs limit: ${:.4}). Releasing slot!",
                                            watcher_symbol, pos.side, oid, order_age_secs, mid_run, pos.entry_price
                                        );
                                        is_truly_dead = true;
                                        reported_status = "PRICE_RUNAWAY_ABORT".to_string();
                                    }
                                }
                            }
                        } else if pos_exists {
                            // Entry order is no longer resting and position exists: refresh current_szi in case a final partial fill
                            // completed in the milliseconds between the initial user_state query and open_orders query.
                            if let Ok((fresh_szi, _)) = get_user_position(&watcher_info, user_address, &watcher_symbol).await {
                                if fresh_szi.abs() > current_szi.abs() {
                                    current_szi = fresh_szi;
                                }
                            }
                        } else if !pos_exists {
                            if let Ok(Ok(order_status)) = tokio::time::timeout(
                                Duration::from_secs(10),
                                watcher_info.query_order_by_oid(user_address, oid),
                            )
                            .await
                            {
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

                if pos.is_filled && !pos_exists {
                    is_truly_dead = true;
                    let is_macro_v2 = pos.engine_type == "EngineB1h" || pos.engine_type == "EngineB4h";
                    reported_status = if is_macro_v2 {
                        "MACRO_V2_STOP_CLOSED_ON_EXCHANGE".to_string()
                    } else if pos.tranche1_tp_filled {
                        "TRANCHE2_RUNNER_CLOSED_ON_EXCHANGE".to_string()
                    } else {
                        "CLOSED_ON_EXCHANGE".to_string()
                    };
                    if (is_macro_v2 || pos.tranche1_tp_filled) && pos.entry_price > 0.0 && pos.amount > 0.0 {
                        let exit_px = if pos.stop_loss_price > 0.0 {
                            pos.stop_loss_price
                        } else if pos.chandelier_stop > 0.0 {
                            pos.chandelier_stop
                        } else {
                            pos.entry_price
                        };
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
                        let exit_reason = if is_macro_v2 {
                            if pnl_usd >= 0.0 {
                                "MACRO_V2_TRAIL_WIN".to_string()
                            } else {
                                "MACRO_V2_STOP_EXIT".to_string()
                            }
                        } else {
                            "CHANDELIER_RUNNER_EXIT".to_string()
                        };
                        crate::telemetry::append_real_trade_record(&crate::telemetry::RealTradeRecord {
                            symbol: watcher_symbol.clone(),
                            side: pos.side.clone(),
                            engine: pos.engine_type.clone(),
                            amount: pos.amount,
                            entry_price: pos.entry_price,
                            exit_price: exit_px,
                            opened_at: pos.opened_at.clone(),
                            closed_at: chrono::Utc::now().to_rfc3339(),
                            pnl_usd,
                            pnl_bps,
                            exit_reason: exit_reason.clone(),
                            is_win: pnl_usd >= 0.0,
                        });
                        {
                            let mut cb_lock = watcher_cb.lock().await;
                            if pnl_usd >= 0.0 {
                                cb_lock.record_win();
                            } else {
                                cb_lock.record_loss();
                            }
                            save_circuit_breaker(&watcher_cb_path, &cb_lock);
                        }
                        if is_macro_v2 {
                            let icon = if pnl_usd >= 0.0 { "🏆" } else { "🛑" };
                            let close_msg = format!(
                                "{} **MARKOV-POLO V2 CLOSED ON EXCHANGE ({}):** {} {}\n**Entry:** ${:.4} | **Est. Stop Fill:** ${:.4}\n**Est. PnL:** ${:+.2} ({:+.1} bps)",
                                icon, pos.engine_type, watcher_symbol, pos.side, pos.entry_price, exit_px, pnl_usd, pnl_bps
                            );
                            crate::discord::send_alert(watcher_discord.as_deref(), &close_msg).await;
                        }
                    }
                }

                if is_filled && pos.engine_type == "EngineB" && !pos.is_filled {
                    println!(
                        "🟢 [FAST-SENTINEL] Engine B Position Active for {} (Side: {})!",
                        watcher_symbol, pos.side
                    );
                    let (side_str, entry_px, stop_px, amt) = {
                        let mut state_lock = watcher_state.lock().await;
                        let res = if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.is_filled = true;
                            if current_szi.abs() > 0.0 {
                                p.amount = current_szi.abs();
                            }
                            p.opened_at = chrono::Utc::now().to_rfc3339();
                            (p.side.clone(), p.entry_price, p.chandelier_stop, p.amount)
                        } else {
                            (pos.side.clone(), pos.entry_price, pos.chandelier_stop, pos.amount)
                        };
                        let _ = save_state(&watcher_state_path, &state_lock);
                        res
                    };
                    let msg = format!(
                        "🟢 **ENGINE B BREAKOUT ENTRY FILLED (HYPERLIQUID):** {}\n**Side:** {}\n**Entry:** ${:.2}\n**Initial Stop:** ${:.2}\n**Size:** {:.5}",
                        watcher_symbol, side_str, entry_px, stop_px, amt
                    );
                    crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                }

                if is_filled
                    && pos_exists
                    && pos.take_profit_price > 0.0
                    && !(pos.engine_type == "EngineB" && pos.tranche1_tp_filled)
                {
                    if !pos.is_filled {
                        let mut state_lock = watcher_state.lock().await;
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.is_filled = true;
                            p.amount = current_szi.abs();
                            p.opened_at = chrono::Utc::now().to_rfc3339();
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }
                    }
                    let raw_tp = if pos.effective_tp > 0.0 {
                        pos.effective_tp
                    } else {
                        pos.take_profit_price
                    };
                    let tp_is_buy = pos.side == "SHORT";
                    let total_amt = if current_szi.abs() > 0.0 {
                        current_szi.abs()
                    } else {
                        pos.amount
                    };
                    let total_notional = total_amt * pos.entry_price;
                    let (tp_amount, tp_price, tranche_tag) = compute_tp_order_plan(
                        &pos.engine_type,
                        pos.entry_price,
                        raw_tp,
                        total_amt,
                        pos.tranche1_tp_filled,
                        watcher_sz_decimals,
                    );

                    println!(
                        "🟢 [FAST-SENTINEL] Position Active for {} (Side: {}, Total: {:.4} / ${:.2})! Placing {} Alo-Priority TP at ${:.2} (Qty: {:.4})",
                        watcher_symbol, pos.side, total_amt, total_notional, tranche_tag, tp_price, tp_amount
                    );

                    if let Err(e) = crate::state::assert_current_lease(&watcher_symbol, &pos.lease_owner_uuid, pos.lease_fencing_token) {
                        eprintln!("❌ [FAST-SENTINEL] TP order boundary fencing assertion failed for {}: {}", watcher_symbol, e);
                        continue;
                    }

                    // Guard against race with 1m loop reprice: check both live state and existing resting reduce-only orders
                    let already_set_in_state = {
                        let state_lock = watcher_state.lock().await;
                        state_lock
                            .get(&watcher_symbol)
                            .map(|p| !p.tp_order_id.is_empty())
                            .unwrap_or(true)
                    };
                    if already_set_in_state {
                        continue;
                    }

                    let entry_oid_opt = pos.order_id.parse::<u64>().ok();
                    let close_side_str = if tp_is_buy { "B" } else { "A" };
                    let mut existing_tp_oid: Option<u64> = None;
                    if let Ok(live_orders) = fetch_open_orders_for_coin(&watcher_info, user_address, &watcher_symbol).await {
                        if let Some(found) = live_orders.iter().find(|o| {
                            o.coin == watcher_symbol
                                && o.side == close_side_str
                                && Some(o.oid) != entry_oid_opt
                                && Some(o.oid) != pos.disaster_stop_oid
                        }) {
                            existing_tp_oid = Some(found.oid);
                        }
                    }
                    if let Some(adopted_oid) = existing_tp_oid {
                        let mut state_lock = watcher_state.lock().await;
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            if p.tp_order_id.is_empty() {
                                p.tp_order_id = adopted_oid.to_string();
                                p.is_filled = true;
                                p.amount = total_amt;
                                let _ = save_state(&watcher_state_path, &state_lock);
                                println!(
                                    "🔗 [FAST-SENTINEL] Adopted existing resting TP order {} for {} (prevented duplicate).",
                                    adopted_oid, watcher_symbol
                                );
                            }
                        }
                        continue;
                    }

                    let tp_cloid = uuid::Uuid::new_v4();
                    match place_reduce_only_tp_order(
                        &watcher_exchange,
                        &watcher_info,
                        &watcher_symbol,
                        tp_is_buy,
                        tp_price,
                        tp_amount,
                        watcher_sz_decimals,
                        Some(tp_cloid),
                    )
                    .await
                    {
                        Ok(tp_oid) => {
                            {
                                let mut state_lock = watcher_state.lock().await;
                                if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                    p.tp_order_id = tp_oid.to_string();
                                    p.is_filled = true;
                                    p.amount = total_amt;
                                    let _ = save_state(&watcher_state_path, &state_lock);
                                }
                            }
                            let msg = format!(
                                "🟢 **TAKE PROFIT PLACED ({} ALO-PRIORITY):** {}\n**Side:** {} TP\n**Target:** ${:.2}\n**TP Size:** {:.5} (Total Pos: {:.5})",
                                tranche_tag, watcher_symbol, pos.side, tp_price, tp_amount, total_amt
                            );
                            crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;

                            // Layer 2: Place Exchange-Native Reduce-Only Disaster Stop-Market Trigger Order on Hyperliquid
                            // for FULL position size (only if none resting yet)
                            if pos.stop_loss_price > 0.0 && pos.disaster_stop_oid.is_none() {
                                let raw_disaster_sl = if pos.side == "LONG" {
                                    pos.stop_loss_price * 0.9985
                                } else {
                                    pos.stop_loss_price * 1.0015
                                };
                                let disaster_sl_px = quantize_price(raw_disaster_sl, watcher_sz_decimals);
                                let sl_cloid = uuid::Uuid::new_v4();
                                match place_disaster_stop_trigger_order(
                                    &watcher_exchange,
                                    &watcher_symbol,
                                    tp_is_buy,
                                    disaster_sl_px,
                                    total_amt,
                                    watcher_sz_decimals,
                                    Some(sl_cloid),
                                )
                                .await
                                {
                                    Ok(sl_oid) => {
                                        {
                                            let mut state_lock = watcher_state.lock().await;
                                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                                p.disaster_stop_oid = Some(sl_oid);
                                                let _ = save_state(&watcher_state_path, &state_lock);
                                            }
                                        }
                                        println!(
                                            "🛡️ [FAST-SENTINEL] Exchange-Native Disaster Stop-Market Trigger placed on Hyperliquid for {} at ${:.2} (oid: {}, sz: {:.4})",
                                            watcher_symbol, disaster_sl_px, sl_oid, total_amt
                                        );
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "⚠️ [FAST-SENTINEL] Could not place exchange-native disaster stop trigger for {}: {} (Fast-Sentinel stop remains active)",
                                            watcher_symbol, e
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "❌ [FAST-SENTINEL] Failed to place TP for {}: {}",
                                watcher_symbol, e
                            );
                        }
                    }
                } else if pos_exists
                    && (pos.engine_type == "EngineB1h" || pos.engine_type == "EngineB4h")
                    && pos.stop_loss_price > 0.0
                    && pos.disaster_stop_oid.is_none()
                {
                    let sl_is_buy = pos.side == "SHORT";
                    let total_amt = if current_szi.abs() > 0.0 {
                        current_szi.abs()
                    } else {
                        pos.amount
                    };
                    let sl_px = quantize_price(pos.stop_loss_price, watcher_sz_decimals);
                    let sl_cloid = uuid::Uuid::new_v4();
                    if let Ok(sl_oid) = place_disaster_stop_trigger_order(
                        &watcher_exchange,
                        &watcher_symbol,
                        sl_is_buy,
                        sl_px,
                        total_amt,
                        watcher_sz_decimals,
                        Some(sl_cloid),
                    )
                    .await
                    {
                        let mut state_lock = watcher_state.lock().await;
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.disaster_stop_oid = Some(sl_oid);
                            p.amount = total_amt;
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }
                        println!(
                            "🛡️ [FAST-SENTINEL] Placed missing Exchange-Native StopMarket for {} {} at ${:.4} (oid: {}, sz: {:.4})",
                            pos.engine_type, watcher_symbol, sl_px, sl_oid, total_amt
                        );
                    }
                } else if is_truly_dead {
                    if let Ok(oid) = pos.order_id.parse::<u64>() {
                        let _ = cancel_order(&watcher_exchange, &watcher_symbol, oid).await;
                    }

                    // Reconcile open orders and position before cleaning up state
                    let eps = 1e-5;
                    let verify_orders = match fetch_open_orders_for_coin(&watcher_info, user_address, &watcher_symbol).await {
                        Ok(orders) => orders,
                        Err(e) => {
                            eprintln!(
                                "⚠️ [FAST-SENTINEL] Failed to verify open_orders before cleanup for {}: {}. Retaining state.",
                                watcher_symbol, e
                            );
                            continue;
                        }
                    };
                    let entry_oid_num = pos.order_id.parse::<u64>().unwrap_or(0);
                    let mut symbol_orders_remain = false;
                    if pos.is_filled && !pos_exists {
                        for o in verify_orders.iter().filter(|o| o.coin == watcher_symbol) {
                            if cancel_order(&watcher_exchange, &watcher_symbol, o.oid).await.is_err() {
                                symbol_orders_remain = true;
                            }
                        }
                    } else {
                        symbol_orders_remain = verify_orders.iter().any(|o| o.oid == entry_oid_num);
                    }

                    if current_szi.abs() <= eps && !symbol_orders_remain {
                        let mut state_lock = watcher_state.lock().await;
                        // Defensive sequence step 1: CloseConfirmed
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.lifecycle = crate::state::PositionLifecycle::CloseConfirmed;
                        }
                        let _ = save_state(&watcher_state_path, &state_lock);

                        let released = crate::state::release_cluster_lease_authenticated(
                            &watcher_symbol,
                            Some(&pos.lease_owner_uuid),
                            Some(pos.lease_fencing_token),
                        );
                        if !released {
                            eprintln!(
                                "⚠️ [FAST-SENTINEL] Lease for {} owned by newer epoch on disk, but exchange is verified flat. Proceeding to clear stale local state.",
                                watcher_symbol
                            );
                        }

                        // Defensive sequence step 2: LeaseReleased
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.lifecycle = crate::state::PositionLifecycle::LeaseReleased;
                        }
                        let _ = save_state(&watcher_state_path, &state_lock);

                        println!(
                            "⚪ [FAST-SENTINEL] Entry for {} dead (status: {}). Position flat, lease released, clearing state.",
                            watcher_symbol, reported_status
                        );
                        state_lock.remove(&watcher_symbol);
                        let _ = save_state(&watcher_state_path, &state_lock);
                    }
                }
            }
            // -------------------------------------------------------------
            // Scenario 2: TP order resting on book. Check if filled or position closed
            // -------------------------------------------------------------
            else if !pos.tp_order_id.is_empty() {
                let eps = 1e-5;
                let is_flat = current_szi.abs() <= eps;
                let mut is_tp_filled = false;
                let mut is_tp_dead = false;

                if let Ok(tp_oid) = pos.tp_order_id.parse::<u64>() {
                    if let Ok(Ok(order_status)) = tokio::time::timeout(
                        Duration::from_secs(10),
                        watcher_info.query_order_by_oid(user_address, tp_oid),
                    )
                    .await
                    {
                        let inner_status = if let Some(ref o) = order_status.order {
                            o.status.to_uppercase()
                        } else {
                            order_status.status.to_uppercase()
                        };

                        if inner_status == "FILLED" {
                            is_tp_filled = true;
                        } else if inner_status == "CANCELED"
                            || inner_status == "REJECTED"
                            || inner_status == "MARGINCANCELED"
                        {
                            is_tp_dead = true;
                        }
                    }
                }

                if is_tp_dead && !is_flat {
                    let mut state_lock = watcher_state.lock().await;
                    if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                        // Only clear if the dead oid is still the active TP oid (avoids racing a newer 1m reprice)
                        if p.tp_order_id == pos.tp_order_id {
                            p.tp_order_id.clear();
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }
                    }
                    continue;
                }

                // Sync late partial entry fill (e.g. initial 12% filled, then remaining 88% filled after initial TP/SL placement)
                let sz_step_eps = 0.5 * 10f64.powi(-(watcher_sz_decimals as i32));
                if !is_tp_filled && !is_flat && current_szi.abs() > pos.amount + sz_step_eps {
                    let new_total_amt = current_szi.abs();
                    println!(
                        "🔄 [FAST-SENTINEL PARTIAL FILL SYNC] {} size grew from {:.4} -> {:.4}. Resizing TP & Disaster Stop!",
                        watcher_symbol, pos.amount, new_total_amt
                    );
                    {
                        let mut state_lock = watcher_state.lock().await;
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.amount = new_total_amt;
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }
                    }

                    // 1. Replace resting TP order for the full position size
                    if !pos.tp_order_id.is_empty() && !(pos.engine_type == "EngineB" && pos.tranche1_tp_filled) {
                        if let Ok(old_tp_oid) = pos.tp_order_id.parse::<u64>() {
                            let _ = cancel_order(&watcher_exchange, &watcher_symbol, old_tp_oid).await;
                        }
                        let raw_tp_sync = if pos.effective_tp > 0.0 {
                            pos.effective_tp
                        } else {
                            pos.take_profit_price
                        };
                        let (new_tp_qty, new_tp_px, new_tranche_tag) = compute_tp_order_plan(
                            &pos.engine_type,
                            pos.entry_price,
                            raw_tp_sync,
                            new_total_amt,
                            pos.tranche1_tp_filled,
                            watcher_sz_decimals,
                        );
                        let tp_is_buy_sync = pos.side == "SHORT";
                        match place_reduce_only_tp_order(
                            &watcher_exchange,
                            &watcher_info,
                            &watcher_symbol,
                            tp_is_buy_sync,
                            new_tp_px,
                            new_tp_qty,
                            watcher_sz_decimals,
                            Some(uuid::Uuid::new_v4()),
                        )
                        .await
                        {
                            Ok(new_tp_oid) => {
                                let mut state_lock = watcher_state.lock().await;
                                if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                    p.tp_order_id = new_tp_oid.to_string();
                                    let _ = save_state(&watcher_state_path, &state_lock);
                                }
                                println!(
                                    "🎯 [FAST-SENTINEL PARTIAL FILL SYNC] {} {} TP resized to {:.4} @ ${:.4} (oid: {})",
                                    watcher_symbol, new_tranche_tag, new_tp_qty, new_tp_px, new_tp_oid
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "⚠️ [FAST-SENTINEL PARTIAL FILL SYNC] Failed to replace TP for {}: {}. Clearing tp_order_id for immediate Scenario 1 recovery.",
                                    watcher_symbol, e
                                );
                                let mut state_lock = watcher_state.lock().await;
                                if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                    p.tp_order_id.clear();
                                    let _ = save_state(&watcher_state_path, &state_lock);
                                }
                            }
                        }
                    }

                    // 2. Replace exchange-native Disaster Stop trigger order for the full position size
                    if pos.stop_loss_price > 0.0 {
                        let is_long_sync = pos.side == "LONG";
                        let sl_px_sync = if pos.disaster_stop_ratcheted_to_be
                            || pos.engine_type == "EngineB"
                            || pos.engine_type == "EngineB1h"
                            || pos.engine_type == "EngineB4h"
                        {
                            quantize_price(pos.stop_loss_price, watcher_sz_decimals)
                        } else {
                            let raw_sl = if is_long_sync {
                                pos.stop_loss_price * 0.9985
                            } else {
                                pos.stop_loss_price * 1.0015
                            };
                            quantize_price(raw_sl, watcher_sz_decimals)
                        };
                        if let Ok(new_sl_oid) = crate::hyperliquid::ratchet_disaster_stop_to_breakeven(
                            &watcher_exchange,
                            &watcher_symbol,
                            !is_long_sync,
                            new_total_amt,
                            sl_px_sync,
                            watcher_sz_decimals,
                            pos.disaster_stop_oid,
                            Some(uuid::Uuid::new_v4()),
                        )
                        .await
                        {
                            let mut state_lock = watcher_state.lock().await;
                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                p.disaster_stop_oid = Some(new_sl_oid);
                                let _ = save_state(&watcher_state_path, &state_lock);
                            }
                            println!(
                                "🛡️ [FAST-SENTINEL PARTIAL FILL SYNC] {} Disaster Stop resized to {:.4} @ ${:.4} (oid: {})",
                                watcher_symbol, new_total_amt, sl_px_sync, new_sl_oid
                            );
                        }
                    }
                    continue;
                }

                if is_tp_filled && !is_flat && !pos.tranche1_tp_filled {
                    // Verify this TP oid is still the active TP oid and T1 hasn't been processed yet
                    {
                        let state_lock = watcher_state.lock().await;
                        let valid_t1 = state_lock
                            .get(&watcher_symbol)
                            .map(|p| p.tp_order_id == pos.tp_order_id && !p.tranche1_tp_filled)
                            .unwrap_or(false);
                        if !valid_t1 {
                            continue;
                        }
                    }
                    // Tranche 1 (50%) filled while Tranche 2 (50% runner) remains open on exchange!
                    let rem_amt = current_szi.abs();
                    let t1_amt = (pos.amount - rem_amt).max(0.0);
                    let raw_tp_t1 = if pos.effective_tp > 0.0 {
                        pos.effective_tp
                    } else {
                        pos.take_profit_price
                    };
                    let (_, t1_exit_px, _) = compute_tp_order_plan(
                        &pos.engine_type,
                        pos.entry_price,
                        raw_tp_t1,
                        pos.amount,
                        false,
                        watcher_sz_decimals,
                    );
                    if t1_amt > 0.0 && pos.entry_price > 0.0 && t1_exit_px > 0.0 {
                        let t1_pnl_usd = if pos.side == "LONG" {
                            (t1_exit_px - pos.entry_price) * t1_amt
                        } else {
                            (pos.entry_price - t1_exit_px) * t1_amt
                        };
                        let t1_pnl_bps = if pos.side == "LONG" {
                            ((t1_exit_px - pos.entry_price) / pos.entry_price) * 10_000.0
                        } else {
                            ((pos.entry_price - t1_exit_px) / pos.entry_price) * 10_000.0
                        };
                        crate::telemetry::append_real_trade_record(&crate::telemetry::RealTradeRecord {
                            symbol: watcher_symbol.clone(),
                            side: pos.side.clone(),
                            engine: pos.engine_type.clone(),
                            amount: t1_amt,
                            entry_price: pos.entry_price,
                            exit_price: t1_exit_px,
                            opened_at: pos.opened_at.clone(),
                            closed_at: chrono::Utc::now().to_rfc3339(),
                            pnl_usd: t1_pnl_usd,
                            pnl_bps: t1_pnl_bps,
                            exit_reason: "TRANCHE1_TAKE_PROFIT".to_string(),
                            is_win: t1_pnl_usd >= 0.0,
                        });
                        if t1_pnl_usd >= 0.0 {
                            let mut cb_lock = watcher_cb.lock().await;
                            cb_lock.record_win();
                            save_circuit_breaker(&watcher_cb_path, &cb_lock);
                        }
                    }
                    let is_long_pos = pos.side == "LONG";
                    let watcher_fee_sched = AssetFeeSchedule::for_symbol(&watcher_symbol);
                    let (live_bid_ratchet, live_ask_ratchet) = {
                        let guard = watcher_obi.read().await;
                        (guard.best_bid, guard.best_ask)
                    };
                    let be_buf_frac = (watcher_fee_sched.maker_taker_rt_bps() + 3.0) * 1e-4;
                    let be_px_raw = if is_long_pos {
                        pos.entry_price * (1.0 + be_buf_frac)
                    } else {
                        pos.entry_price * (1.0 - be_buf_frac)
                    };
                    let be_px = quantize_price(be_px_raw, watcher_sz_decimals);
                    let safe_be_vs_bbo = if is_long_pos {
                        live_bid_ratchet <= 0.0 || be_px < live_bid_ratchet * 0.9995
                    } else {
                        live_ask_ratchet <= 0.0 || be_px > live_ask_ratchet * 1.0005
                    };

                    // 1. Update state: Engine B ratchets protective stop monotonically to fee-covered breakeven;
                    //    Engine A retains its wide catastrophe stop and relies on State 3 Maker-Pegged Profit Lock.
                    let is_engine_b_pos = pos.engine_type == "EngineB";
                    {
                        let mut state_lock = watcher_state.lock().await;
                        if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                            p.tranche1_tp_filled = true;
                            p.tp_order_id.clear();
                            p.amount = rem_amt;
                            if is_engine_b_pos && safe_be_vs_bbo {
                                if is_long_pos {
                                    p.stop_loss_price = p.stop_loss_price.max(be_px);
                                    p.chandelier_stop = p.chandelier_stop.max(be_px);
                                } else {
                                    if p.stop_loss_price > 0.0 {
                                        p.stop_loss_price = p.stop_loss_price.min(be_px);
                                    } else {
                                        p.stop_loss_price = be_px;
                                    }
                                    if p.chandelier_stop > 0.0 {
                                        p.chandelier_stop = p.chandelier_stop.min(be_px);
                                    } else {
                                        p.chandelier_stop = be_px;
                                    }
                                }
                            }
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }
                    }

                    // 2. Replace Exchange-Native Disaster Stop with exact remaining size:
                    //    Engine B ratchets trigger to BE; Engine A keeps original wide catastrophe stop.
                    let (target_sl_px, mark_be_ratcheted) = if is_engine_b_pos {
                        (if safe_be_vs_bbo { Some(be_px) } else { None }, true)
                    } else if pos.stop_loss_price > 0.0 {
                        let raw_wide_sl = if is_long_pos {
                            pos.stop_loss_price * 0.9985
                        } else {
                            pos.stop_loss_price * 1.0015
                        };
                        (Some(quantize_price(raw_wide_sl, watcher_sz_decimals)), false)
                    } else {
                        (None, false)
                    };
                    if let Some(new_trigger_px) = target_sl_px {
                        if let Ok(new_sl_oid) = crate::hyperliquid::ratchet_disaster_stop_to_breakeven(
                            &watcher_exchange,
                            &watcher_symbol,
                            !is_long_pos,
                            rem_amt,
                            new_trigger_px,
                            watcher_sz_decimals,
                            pos.disaster_stop_oid,
                            Some(uuid::Uuid::new_v4()),
                        )
                        .await
                        {
                            let mut state_lock = watcher_state.lock().await;
                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                p.disaster_stop_oid = Some(new_sl_oid);
                                if mark_be_ratcheted {
                                    p.disaster_stop_ratcheted_to_be = true;
                                }
                                let _ = save_state(&watcher_state_path, &state_lock);
                            }
                        }
                    }

                    // 3. Engine A places Tranche 2 TP at equilibrium target_mu; Engine B lets Chandelier trail uncapped
                    if !is_engine_b_pos {
                        let t2_raw = if pos.effective_tp > 0.0 {
                            pos.effective_tp
                        } else {
                            pos.take_profit_price
                        };
                        let t2_px = quantize_price(t2_raw, watcher_sz_decimals);
                        if let Ok(t2_oid) = place_reduce_only_tp_order(
                            &watcher_exchange,
                            &watcher_info,
                            &watcher_symbol,
                            !is_long_pos,
                            t2_px,
                            rem_amt,
                            watcher_sz_decimals,
                            Some(uuid::Uuid::new_v4()),
                        )
                        .await
                        {
                            let mut state_lock = watcher_state.lock().await;
                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                p.tp_order_id = t2_oid.to_string();
                                let _ = save_state(&watcher_state_path, &state_lock);
                            }
                        }
                        println!(
                            "🎯 [ENGINE A TRANCHE 1 FILLED (50%)] {} locked 50% early profit! Tranche 2 ({:.4}) resting at ${:.2} (Catastrophe stop resized to ${:.2}; State 3 Maker-Peg active).",
                            watcher_symbol, rem_amt, t2_px, target_sl_px.unwrap_or(pos.stop_loss_price)
                        );
                    } else {
                        println!(
                            "🚀 [ENGINE B TRANCHE 1 FILLED (50% @ +1.5 ATR)] {} locked 50%! Remaining 50% runner ({:.4}) trailing via 2.0x ATR Chandelier with BE stop @ ${:.2}.",
                            watcher_symbol, rem_amt, be_px
                        );
                        let msg_t1 = format!(
                            "🚀 **ENGINE B TRANCHE 1 FILLED (50%):** {}\n**Remaining Runner:** {:.5} trailing via Chandelier Stop\n**Breakeven Stop:** ${:.2}",
                            watcher_symbol, rem_amt, be_px
                        );
                        crate::discord::send_alert(watcher_discord.as_deref(), &msg_t1).await;
                    }
                    continue;
                }

                if is_tp_filled || is_flat {
                    let final_orders = match fetch_open_orders_for_coin(&watcher_info, user_address, &watcher_symbol).await {
                        Ok(orders) => orders,
                        Err(e) => {
                            eprintln!(
                                "⚠️ [FAST-SENTINEL] Failed to query open_orders on TP close for {}: {}. Retaining state.",
                                watcher_symbol, e
                            );
                            continue;
                        }
                    };
                    let mut symbol_orders_remain = false;
                    if is_flat {
                        // Cancel any sibling orders (such as the Exchange-Native Disaster Stop-Market trigger order)
                        for o in final_orders.iter().filter(|o| o.coin == watcher_symbol) {
                            if cancel_order(&watcher_exchange, &watcher_symbol, o.oid).await.is_err() {
                                symbol_orders_remain = true;
                            }
                        }
                    } else {
                        symbol_orders_remain = final_orders.iter().any(|o| o.coin == watcher_symbol);
                    }

                    if is_flat && !symbol_orders_remain {
                        {
                            let mut state_lock = watcher_state.lock().await;
                            // Defensive sequence step 1: CloseConfirmed
                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                p.lifecycle = crate::state::PositionLifecycle::CloseConfirmed;
                            }
                            let _ = save_state(&watcher_state_path, &state_lock);

                            let released = crate::state::release_cluster_lease_authenticated(
                                &watcher_symbol,
                                Some(&pos.lease_owner_uuid),
                                Some(pos.lease_fencing_token),
                            );
                            if !released {
                                eprintln!(
                                    "⚠️ [FAST-SENTINEL] Lease for {} owned by newer epoch on TP close, but exchange is verified flat. Proceeding to clear stale local state.",
                                    watcher_symbol
                                );
                            }

                            // Defensive sequence step 2: LeaseReleased
                            if let Some(p) = state_lock.get_mut(&watcher_symbol) {
                                p.lifecycle = crate::state::PositionLifecycle::LeaseReleased;
                            }
                            let _ = save_state(&watcher_state_path, &state_lock);

                            println!(
                                "🎉 [FAST-SENTINEL] Take Profit for {} (Side: {}) verified flat & zero orders! State cleared, lease released.",
                                watcher_symbol, pos.side
                            );
                            if pos.entry_price > 0.0 && pos.amount > 0.0 {
                                let exit_px = if pos.effective_tp > 0.0 {
                                    pos.effective_tp
                                } else if pos.take_profit_price > 0.0 {
                                    pos.take_profit_price
                                } else {
                                    pos.entry_price
                                };
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
                                    symbol: watcher_symbol.clone(),
                                    side: pos.side.clone(),
                                    engine: pos.engine_type.clone(),
                                    amount: pos.amount,
                                    entry_price: pos.entry_price,
                                    exit_price: exit_px,
                                    opened_at: pos.opened_at.clone(),
                                    closed_at: chrono::Utc::now().to_rfc3339(),
                                    pnl_usd,
                                    pnl_bps,
                                    exit_reason: "TAKE_PROFIT".to_string(),
                                    is_win: pnl_usd >= 0.0,
                                });
                            }
                            state_lock.remove(&watcher_symbol);
                            let _ = save_state(&watcher_state_path, &state_lock);
                        }

                        // Reset circuit breaker (WITHOUT holding state_lock!)
                        let mut cb_lock = watcher_cb.lock().await;
                        cb_lock.record_win();
                        save_circuit_breaker(&watcher_cb_path, &cb_lock);
                        drop(cb_lock);

                        let msg = format!(
                            "🎉 **TRADE CLOSED IN PROFIT (HYPERLIQUID):** {} {} Take Profit has filled successfully.",
                            watcher_symbol, pos.side
                        );
                        crate::discord::send_alert(watcher_discord.as_deref(), &msg).await;
                    }
                }
            }
        }
    });

    // ------------------------------------------------------------------------
    // HOT-PATH: WebSocket 1m Candle Ingestion & Markov-Polo V2.1 Macro Execution
    // ------------------------------------------------------------------------
    let trend_engine_mode = config.trend_engine_mode.clone();
    let donchian_period = config.donchian_period;
    let max_slots_cfg = crate::state::max_concurrent_cluster_positions();
    let max_same_dir_cfg = crate::state::max_same_direction_macro_positions(max_slots_cfg);
    println!(
        "⚙️ [STARTUP CONFIG] Symbol: {} | MARKOV-POLO V2.2 ZERO-LIQ DUAL-PILLAR: {} (4H SuperTrend-3.0 + 1H EMA50 Pullback | 0.75% risk/tr, {} slots / max {}/dir, Hard Cap <= 1.00x Equity [<= {:.0}%/slot], Macro DD-Taper >15%, 4.0x ATR Trail) | Exchange Leverage: {}x",
        trading_symbol,
        trend_engine_mode.to_uppercase(),
        max_slots_cfg,
        max_same_dir_cfg,
        100.0 / (max_slots_cfg.max(1) as f64),
        config.hl_leverage,
    );
    let cached_equity_loop = cached_account_equity.clone();
    let mut controller = load_controller_state(&trading_symbol);
    let mut current_5m_bar: Option<CandleBar> = initial_5m_bar;
    let mut current_15m_bar: Option<CandleBar> = initial_15m_bar;

    // Initialize regime posteriors immediately from pre-filled 15m bars to eliminate startup blind-lag
    let (init_p_c, init_p_t, init_p_s) = if rolling_15m_bars.len() >= 15 {
        let hmm_lock = hmm_model.read().await;
        let closes_15m: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
        let res = compute_hmm_posteriors(&*hmm_lock, &closes_15m);
        drop(hmm_lock);
        res.unwrap_or((0.60, 0.25, 0.15))
    } else {
        (0.60, 0.25, 0.15)
    };
    let mut current_p_c = init_p_c;
    let mut current_p_t = init_p_t;
    let mut current_p_s = init_p_s;
    println!(
        "🧭 [STARTUP REGIME INITIALIZED] Symbol: {} | P_C: {:.1}% | P_T: {:.1}% | P_S: {:.1}% (from {} historical 15m bars)",
        trading_symbol, current_p_c * 100.0, current_p_t * 100.0, current_p_s * 100.0, rolling_15m_bars.len()
    );
    let mut last_shock_bar_ms: u64 = 0;

    let obi_tracker_bg = obi_tracker.clone();
    let funding_bg = live_funding_rate.clone();
    let crowding_bg = crowding_tracker.clone();
    let notify_bg = sentinel_notify.clone();
    let state_bg = active_state.clone();
    let symbol_bg = trading_symbol.clone();
    let base_url_bg = base_url.clone();
    let ws_endpoint_opt = std::env::var("HL_WS_ENDPOINT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ws_endpoint_bg = ws_endpoint_opt.clone();

    tokio::spawn(async move {
        let mut mux_failures: u32 = 0;
        loop {
            let (l2_sender, mut l2_receiver) = unbounded_channel();
            let mut _direct_client_guard: Option<InfoClient> = None;
            let mut mux_task_handle: Option<tokio::task::JoinHandle<()>> = None;
            let used_mux = ws_endpoint_bg.is_some() && mux_failures < 3;

            if used_mux {
                let ep = ws_endpoint_bg.as_ref().unwrap().clone();
                let subs = vec![
                    Subscription::L2Book {
                        coin: symbol_bg.clone(),
                    },
                    Subscription::ActiveAssetCtx {
                        coin: symbol_bg.clone(),
                    },
                    Subscription::OrderUpdates { user: user_address },
                    Subscription::UserFills { user: user_address },
                ];
                let sym_log = symbol_bg.clone();
                mux_task_handle = Some(tokio::spawn(async move {
                    if let Err(e) =
                        crate::mux::stream_via_ws_endpoint(&ep, &subs, l2_sender, 60, "L2+USER")
                            .await
                    {
                        eprintln!("⚠️ [L2+USER MUX WS] {} stream ended: {}", sym_log, e);
                    }
                }));
                println!(
                    "📡 [L2 + USER WEBSOCKET (MUX)] Subscribed to L2Book, ActiveAssetCtx, OrderUpdates & UserFills for {} via {}.",
                    symbol_bg,
                    ws_endpoint_bg.as_deref().unwrap_or("")
                );
            } else {
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
                        l2_sender.clone(),
                    )
                    .await
                {
                    eprintln!("❌ [L2 WS ERROR] Subscription failed: {}. Retrying in 5s...", e);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // Subscribe to real-time Asset Context (hourly funding rate & mark price)
                let _ = l2_client
                    .subscribe(
                        Subscription::ActiveAssetCtx {
                            coin: symbol_bg.clone(),
                        },
                        l2_sender.clone(),
                    )
                    .await;

                // Subscribe to private OrderUpdates & UserFills for sub-50ms Fast-Sentinel wakeup
                let _ = l2_client
                    .subscribe(
                        Subscription::OrderUpdates {
                            user: user_address,
                        },
                        l2_sender.clone(),
                    )
                    .await;
                let _ = l2_client
                    .subscribe(
                        Subscription::UserFills {
                            user: user_address,
                        },
                        l2_sender,
                    )
                    .await;

                _direct_client_guard = Some(l2_client);
                println!("📡 [L2 + USER WEBSOCKET] Subscribed to continuous L2Book, ActiveAssetCtx, OrderUpdates & UserFills for {}.", symbol_bg);
            }

            let mut received_any_l2_msg = false;
            loop {
                let msg_res = tokio::time::timeout(Duration::from_secs(60), l2_receiver.recv()).await;
                let msg = match msg_res {
                    Ok(Some(m)) => {
                        received_any_l2_msg = true;
                        m
                    }
                    Ok(None) => break,
                    Err(_) => {
                        eprintln!("⚠️ [L2 WEBSOCKET WATCHDOG] No L2 book update received for 60s on {}. Stalled connection detected! Reconnecting...", symbol_bg);
                        break;
                    }
                };

                match msg {
                    Message::L2Book(l2_msg) => {
                        let local_recv_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                        let l2_data = l2_msg.data;
                        let bids: Vec<(f64, f64)> = l2_data.levels.first().cloned().unwrap_or_default()
                            .into_iter()
                            .filter_map(|l| Some((l.px.parse::<f64>().ok()?, l.sz.parse::<f64>().ok()?)))
                            .collect();
                        let asks: Vec<(f64, f64)> = l2_data.levels.get(1).cloned().unwrap_or_default()
                            .into_iter()
                            .filter_map(|l| Some((l.px.parse::<f64>().ok()?, l.sz.parse::<f64>().ok()?)))
                            .collect();
                        let best_bid = bids.first().map(|b| b.0).unwrap_or(0.0);
                        let best_ask = asks.first().map(|a| a.0).unwrap_or(0.0);
                        obi_tracker_bg.write().await.update(&bids, &asks, l2_data.time, local_recv_ms);

                        // Instant event-driven stop-loss wakeup if L2 quote crosses active stop
                        if best_bid > 0.0 && best_ask > 0.0 {
                            if let Ok(guard) = state_bg.try_lock() {
                                if let Some(pos) = guard.get(&symbol_bg) {
                                    if pos.stop_loss_price > 0.0 {
                                        let breached = if pos.side == "LONG" {
                                            best_bid <= pos.stop_loss_price
                                        } else {
                                            best_ask >= pos.stop_loss_price
                                        };
                                        if breached {
                                            notify_bg.notify_one();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Message::ActiveAssetCtx(ctx_msg) => {
                        if ctx_msg.data.coin == symbol_bg {
                            if let hyperliquid_rust_sdk::AssetCtx::Perps(perp_ctx) = ctx_msg.data.ctx {
                                let f_rate = perp_ctx.funding.parse::<f64>().unwrap_or(0.0);
                                let oi_val = perp_ctx.open_interest.parse::<f64>().unwrap_or(0.0);
                                let mark_val = perp_ctx.shared.mark_px.parse::<f64>().unwrap_or(0.0);
                                let oracle_val = perp_ctx.oracle_px.parse::<f64>().unwrap_or(0.0);
                                let local_recv_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                                if f_rate.is_finite() {
                                    *funding_bg.write().await = f_rate;
                                }
                                crowding_bg.write().await.update_with_time(
                                    f_rate,
                                    oi_val,
                                    mark_val,
                                    oracle_val,
                                    local_recv_ms,
                                );
                            }
                        }
                    }
                    Message::OrderUpdates(upd_msg) => {
                        if upd_msg.data.iter().any(|u| u.order.coin == symbol_bg) {
                            notify_bg.notify_one();
                        }
                    }
                    Message::UserFills(fills_msg) => {
                        if fills_msg.data.fills.iter().any(|f| f.coin == symbol_bg) {
                            notify_bg.notify_one();
                        }
                    }
                    _ => {}
                }
            }
            if let Some(h) = mux_task_handle {
                h.abort();
            }
            if used_mux {
                if received_any_l2_msg {
                    mux_failures = 0;
                } else {
                    mux_failures = mux_failures.saturating_add(1);
                }
            } else {
                mux_failures = 0;
            }
            eprintln!("⚠️ [L2 WEBSOCKET] Disconnected. Reconnecting in 2s...");
            let reconn_now = chrono::Utc::now().timestamp_millis().max(0) as u64;
            obi_tracker_bg.write().await.on_l2_reconnect(reconn_now);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    let mut pending_1m: Option<CandleBar> = None;
    let mut last_processed_1m_open: u64 = initial_last_1m_open;
    let mut candle_feed_degraded: bool = false;
    let mut funnel_counters = load_funnel_counters(&trading_symbol);
    let mut cf_tracker = crate::counterfactual::CounterfactualTracker::new(crate::counterfactual::COUNTERFACTUAL_JOURNAL);
    let mut candle_mux_failures: u32 = 0;

    loop {
        let (sender, mut receiver) = unbounded_channel();
        let mut _ws_info_client_guard: Option<InfoClient> = None;
        let mut candle_mux_handle: Option<tokio::task::JoinHandle<()>> = None;
        let used_candle_mux = ws_endpoint_opt.is_some() && candle_mux_failures < 3;

        if used_candle_mux {
            let ep = ws_endpoint_opt.as_ref().unwrap().clone();
            let subs = vec![Subscription::Candle {
                coin: trading_symbol.clone(),
                interval: "1m".to_string(),
            }];
            let sym_log = trading_symbol.clone();
            candle_mux_handle = Some(tokio::spawn(async move {
                if let Err(e) =
                    crate::mux::stream_via_ws_endpoint(&ep, &subs, sender, 150, "CANDLE").await
                {
                    eprintln!("⚠️ [CANDLE MUX WS] {} stream ended: {}", sym_log, e);
                }
            }));
            println!(
                "📡 [WEBSOCKET (MUX)] Subscribed to 1m candles for {} via {}.",
                trading_symbol,
                ws_endpoint_opt.as_deref().unwrap_or("")
            );
        } else {
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

            _ws_info_client_guard = Some(ws_info_client);
            println!("📡 [WEBSOCKET] Successfully subscribed to 1m candles for {}.", trading_symbol);
        }

        let mut received_any_candle = false;
        loop {
            let recv_res = tokio::time::timeout(Duration::from_secs(150), receiver.recv()).await;
            let msg = match recv_res {
                Ok(Some(m)) => {
                    received_any_candle = true;
                    m
                }
                Ok(None) => {
                    eprintln!("⚠️ [WEBSOCKET] Candle stream channel closed for {}. Reconnecting in 3s...", trading_symbol);
                    break;
                }
                Err(_) => {
                    eprintln!("⚠️ [WEBSOCKET WATCHDOG] No 1m candle received for 150s on {}. Stalled connection detected! Reconnecting WebSocket...", trading_symbol);
                    break;
                }
            };

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
                    Some(ref pending) if update_bar.time_open < pending.time_open => {
                        // Drop backwards/out-of-order candle update without replacing pending
                        println!(
                            "⚠️ [OUT-OF-ORDER CANDLE DROPPED] Arrived open_ms: {} < pending: {}",
                            update_bar.time_open, pending.time_open
                        );
                        None
                    }
                    Some(_) => pending_1m.replace(update_bar),
                };

                let completed_bar = match completed_1m {
                    Some(b) => b,
                    None => continue,
                };

                // Strict Monotonicity Guard: drop duplicate or backwards completed candles
                if completed_bar.time_open <= last_processed_1m_open {
                    println!(
                        "⚠️ [DROPPED CANDLE] Completed bar open_ms {} <= last processed open_ms {} (out-of-order or duplicate).",
                        completed_bar.time_open, last_processed_1m_open
                    );
                    continue;
                }

                let mut is_candle_gap = false;
                if last_processed_1m_open > 0 && completed_bar.time_open > last_processed_1m_open + 60_000 {
                    let gap_minutes = (completed_bar.time_open - last_processed_1m_open) / 60_000;
                    println!(
                        "⚠️ [CANDLE GAP DETECTED] Missing {} candle(s) between {} and {}. Initiating REST backfill...",
                        gap_minutes, last_processed_1m_open, completed_bar.time_open
                    );

                    // Severe macro gap (>= 30m): emergency disarm, flatten resting entry, and force neutral
                    if gap_minutes >= 30 {
                        println!("🛑 [MACRO GAP >= 30m] Severe data gap detected ({} min). Cancelling resting maker orders and forcing Neutral...", gap_minutes);
                        controller.force_neutral_data_quality();
                        let mut armed_state = load_armed_signal(&arm_file_path);
                        if armed_state.is_armed {
                            armed_state.is_armed = false;
                            save_armed_signal(&arm_file_path, &armed_state);
                        }
                        let mut state_lock = active_state.lock().await;
                        if let Some(pos) = state_lock.get(&trading_symbol) {
                            if !pos.is_filled {
                                let _ = execute_emergency_flatten_with_retry(
                                    &state_file_path,
                                    &exchange,
                                    &info_client,
                                    user_address,
                                    &mut state_lock,
                                    &trading_symbol,
                                    trading_sz_decimals,
                                    discord_url.clone(),
                                ).await;
                            }
                        }
                    }

                    // REST backfill of missing candles
                    let backfill_res = backfill_missing_1m_candles(
                        &info_client,
                        &trading_symbol,
                        last_processed_1m_open + 60_000,
                        completed_bar.time_open,
                        &mut rolling_closes,
                        &mut rolling_5m_bars,
                        &mut current_5m_bar,
                        &mut rolling_15m_bars,
                        &mut current_15m_bar,
                    ).await;

                    match backfill_res {
                        Ok(0) => {
                            eprintln!("❌ [BACKFILL EMPTY] Expected missing candles but received 0. Retaining candle_feed_degraded=true (fail-closed).");
                            is_candle_gap = true;
                            candle_feed_degraded = true;
                            controller.force_neutral_data_quality();
                            let mut armed_state = load_armed_signal(&arm_file_path);
                            if armed_state.is_armed {
                                armed_state.is_armed = false;
                                save_armed_signal(&arm_file_path, &armed_state);
                            }
                        }
                        Ok(count) => {
                            println!("✅ [BACKFILL COMPLETE] Successfully recovered and validated {} missing 1m bars from REST. Continuity certified.", count);
                            is_candle_gap = false;
                            candle_feed_degraded = false;
                        }
                        Err(e) => {
                            eprintln!("❌ [BACKFILL FAILED] Could not validate missing candles via REST: {}. Retaining candle_feed_degraded=true (fail-closed).", e);
                            is_candle_gap = true;
                            candle_feed_degraded = true;
                            controller.force_neutral_data_quality();
                            let mut armed_state = load_armed_signal(&arm_file_path);
                            if armed_state.is_armed {
                                armed_state.is_armed = false;
                                save_armed_signal(&arm_file_path, &armed_state);
                            }
                        }
                    }
                }
                last_processed_1m_open = completed_bar.time_open;

                let close_price = completed_bar.close;
                let high_price = completed_bar.high;
                let low_price = completed_bar.low;
                let open_price = completed_bar.open;
                let vol_val = completed_bar.volume;

                let now_candle_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                cf_tracker.on_candle_bar(&trading_symbol, high_price, low_price, close_price, now_candle_ms);

                let candle_bucket_5m = (completed_bar.time_open / (5 * 60 * 1000)) * (5 * 60 * 1000);
                let is_last_1m_of_5m = completed_bar.time_open + 60_000 >= candle_bucket_5m + 5 * 60 * 1000;

                if let Some(mut cur_5m) = current_5m_bar.take() {
                    if cur_5m.time_open == candle_bucket_5m {
                        cur_5m.high = cur_5m.high.max(high_price);
                        cur_5m.low = cur_5m.low.min(low_price);
                        cur_5m.close = close_price;
                        cur_5m.volume += vol_val;
                        if is_last_1m_of_5m {
                            rolling_5m_bars.push(cur_5m);
                            if rolling_5m_bars.len() > 300 {
                                rolling_5m_bars.remove(0);
                            }
                        } else {
                            current_5m_bar = Some(cur_5m);
                        }
                    } else {
                        rolling_5m_bars.push(cur_5m);
                        if rolling_5m_bars.len() > 300 {
                            rolling_5m_bars.remove(0);
                        }
                        let new_5m = CandleBar {
                            time_open: candle_bucket_5m,
                            open: open_price,
                            high: high_price,
                            low: low_price,
                            close: close_price,
                            volume: vol_val,
                        };
                        if is_last_1m_of_5m {
                            rolling_5m_bars.push(new_5m);
                            if rolling_5m_bars.len() > 300 {
                                rolling_5m_bars.remove(0);
                            }
                        } else {
                            current_5m_bar = Some(new_5m);
                        }
                    }
                } else {
                    let new_5m = CandleBar {
                        time_open: candle_bucket_5m,
                        open: open_price,
                        high: high_price,
                        low: low_price,
                        close: close_price,
                        volume: vol_val,
                    };
                    if is_last_1m_of_5m {
                        rolling_5m_bars.push(new_5m);
                        if rolling_5m_bars.len() > 300 {
                            rolling_5m_bars.remove(0);
                        }
                    } else {
                        current_5m_bar = Some(new_5m);
                    }
                }

                let candle_bucket_15m = (completed_bar.time_open / (15 * 60 * 1000)) * (15 * 60 * 1000);
                let is_last_1m_of_15m = completed_bar.time_open + 60_000 >= candle_bucket_15m + 15 * 60 * 1000;
                let mut finalized_15m_bar: Option<CandleBar> = None;

                if let Some(mut cur_15m) = current_15m_bar.take() {
                    if cur_15m.time_open == candle_bucket_15m {
                        cur_15m.high = cur_15m.high.max(high_price);
                        cur_15m.low = cur_15m.low.min(low_price);
                        cur_15m.close = close_price;
                        cur_15m.volume += vol_val;
                        if is_last_1m_of_15m {
                            rolling_15m_bars.push(cur_15m.clone());
                            if rolling_15m_bars.len() > 100 {
                                rolling_15m_bars.remove(0);
                            }
                            finalized_15m_bar = Some(cur_15m);
                        } else {
                            current_15m_bar = Some(cur_15m);
                        }
                    } else {
                        rolling_15m_bars.push(cur_15m.clone());
                        if rolling_15m_bars.len() > 100 {
                            rolling_15m_bars.remove(0);
                        }
                        finalized_15m_bar = Some(cur_15m);
                        let new_bar = CandleBar {
                            time_open: candle_bucket_15m,
                            open: open_price,
                            high: high_price,
                            low: low_price,
                            close: close_price,
                            volume: vol_val,
                        };
                        if is_last_1m_of_15m {
                            rolling_15m_bars.push(new_bar.clone());
                            if rolling_15m_bars.len() > 100 {
                                rolling_15m_bars.remove(0);
                            }
                            finalized_15m_bar = Some(new_bar);
                        } else {
                            current_15m_bar = Some(new_bar);
                        }
                    }
                } else {
                    let new_bar = CandleBar {
                        time_open: candle_bucket_15m,
                        open: open_price,
                        high: high_price,
                        low: low_price,
                        close: close_price,
                        volume: vol_val,
                    };
                    if is_last_1m_of_15m {
                        rolling_15m_bars.push(new_bar.clone());
                        if rolling_15m_bars.len() > 100 {
                            rolling_15m_bars.remove(0);
                        }
                        finalized_15m_bar = Some(new_bar);
                    } else {
                        current_15m_bar = Some(new_bar);
                    }
                }

                // Package 9: Aggregate finalized 1-hour candles for Engine B v2 (1H EMA50 Pullback-in-Trend)
                let candle_bucket_1h = (completed_bar.time_open / 3_600_000) * 3_600_000;
                let is_last_1m_of_1h = completed_bar.time_open + 60_000 >= candle_bucket_1h + 3_600_000;
                let mut finalized_1h_bar: Option<CandleBar> = None;
                if let Some(mut cur_1h) = current_1h_bar.take() {
                    if cur_1h.time_open == candle_bucket_1h {
                        cur_1h.high = cur_1h.high.max(high_price);
                        cur_1h.low = cur_1h.low.min(low_price);
                        cur_1h.close = close_price;
                        cur_1h.volume += vol_val;
                        if is_last_1m_of_1h {
                            finalized_1h_bar = Some(cur_1h);
                        } else {
                            current_1h_bar = Some(cur_1h);
                        }
                    } else {
                        finalized_1h_bar = Some(cur_1h);
                        let nb1h = CandleBar {
                            time_open: candle_bucket_1h,
                            open: open_price,
                            high: high_price,
                            low: low_price,
                            close: close_price,
                            volume: vol_val,
                        };
                        if is_last_1m_of_1h {
                            finalized_1h_bar = Some(nb1h);
                        } else {
                            current_1h_bar = Some(nb1h);
                        }
                    }
                } else {
                    let nb1h = CandleBar {
                        time_open: candle_bucket_1h,
                        open: open_price,
                        high: high_price,
                        low: low_price,
                        close: close_price,
                        volume: vol_val,
                    };
                    if is_last_1m_of_1h {
                        finalized_1h_bar = Some(nb1h);
                    } else {
                        current_1h_bar = Some(nb1h);
                    }
                }
                let engine_b_1h_sig_opt = finalized_1h_bar
                    .as_ref()
                    .and_then(|b1h| engine_b_1h.on_1h_bar_close(b1h, true));

                // Markov-Polo V2 (Package 10): Aggregate finalized 4-hour candles for MacroTrend4hEngine (4H_ST3 + 4H_D40)
                let candle_bucket_4h = (completed_bar.time_open / 14_400_000) * 14_400_000;
                let is_last_1m_of_4h = completed_bar.time_open + 60_000 >= candle_bucket_4h + 14_400_000;
                let mut finalized_4h_bar: Option<CandleBar> = None;
                if let Some(mut cur_4h) = current_4h_bar.take() {
                    if cur_4h.time_open == candle_bucket_4h {
                        cur_4h.high = cur_4h.high.max(high_price);
                        cur_4h.low = cur_4h.low.min(low_price);
                        cur_4h.close = close_price;
                        cur_4h.volume += vol_val;
                        if is_last_1m_of_4h {
                            finalized_4h_bar = Some(cur_4h);
                        } else {
                            current_4h_bar = Some(cur_4h);
                        }
                    } else {
                        finalized_4h_bar = Some(cur_4h);
                        let nb4h = CandleBar {
                            time_open: candle_bucket_4h,
                            open: open_price,
                            high: high_price,
                            low: low_price,
                            close: close_price,
                            volume: vol_val,
                        };
                        if is_last_1m_of_4h {
                            finalized_4h_bar = Some(nb4h);
                        } else {
                            current_4h_bar = Some(nb4h);
                        }
                    }
                } else {
                    let nb4h = CandleBar {
                        time_open: candle_bucket_4h,
                        open: open_price,
                        high: high_price,
                        low: low_price,
                        close: close_price,
                        volume: vol_val,
                    };
                    if is_last_1m_of_4h {
                        finalized_4h_bar = Some(nb4h);
                    } else {
                        current_4h_bar = Some(nb4h);
                    }
                }
                let engine_b_4h_sig_opt = finalized_4h_bar
                    .as_ref()
                    .and_then(|b4h| engine_b_4h.on_4h_bar_close(b4h, true));

                let exchange_clone = exchange.clone();
                let info_clone = info_client.clone();
                let state_clone = active_state.clone();
                let discord_clone = discord_url.clone();
                let symbol_clone = trading_symbol.clone();
                let state_path_clone = state_file_path.clone();
                let cb_clone = active_cb.clone();
                let cb_path_clone = cb_file_path.clone();
                let sz_decimals = trading_sz_decimals;

                // Causal Feature Isolation: evaluate live 1m features and 15m cutoff decision features
                // Package 9: Pass completed_bar.time_open for exact live-backtest parity (horizon_scale + min_gross_entry_bps + macro hazard)
                let current_close = close_price;
                let closes_clone = rolling_closes.clone();
                let finalized_15m_clone = finalized_15m_bar.clone();
                let eval_bar_ts_ms = completed_bar.time_open;

                let current_funding_1h = *live_funding_rate.read().await;
                let (live_features, decision_features) = if closes_clone.len() >= 100 {
                    let symbol_for_feat = symbol_clone.clone();
                    tokio::task::spawn_blocking(move || {
                        let live_feat = calculate_features_with_funding_at(
                            &closes_clone,
                            current_close,
                            &symbol_for_feat,
                            current_funding_1h,
                            Some(eval_bar_ts_ms),
                        );
                        let decision_feat = if let Some(ref closed_15m) = finalized_15m_clone {
                            if (closed_15m.close - current_close).abs() < 1e-9 {
                                live_feat.clone()
                            } else if closes_clone.len() > 1 {
                                calculate_features_with_funding_at(
                                    &closes_clone[..closes_clone.len() - 1],
                                    closed_15m.close,
                                    &symbol_for_feat,
                                    current_funding_1h,
                                    Some(eval_bar_ts_ms),
                                )
                            } else {
                                calculate_features_with_funding_at(
                                    &closes_clone,
                                    closed_15m.close,
                                    &symbol_for_feat,
                                    current_funding_1h,
                                    Some(eval_bar_ts_ms),
                                )
                            }
                        } else {
                            None
                        };
                        (live_feat, decision_feat)
                    })
                    .await
                    .unwrap_or((None, None))
                } else {
                    (None, None)
                };

                if let Some(ref live_feat) = live_features {
                    let _hist_last_price = live_feat.hist_last_price;
                    let is_stationary = live_feat.is_stationary;
                    let is_valid_half_life = live_feat.is_valid_half_life;
                    let total_drift_pct = live_feat.total_drift_pct;
                    let half_life_m = live_feat.half_life_m;
                    let z_15 = live_feat.z_15;
                    let z_60 = live_feat.z_60;
                    let thresholds_opt = live_feat.thresholds_opt.clone();

                    let cb_lock = cb_clone.lock().await;
                    let symbol_cb_paused = cb_lock.is_paused();
                    drop(cb_lock);
                    let now_ms_cb = chrono::Utc::now().timestamp_millis().max(0) as u64;
                    let live_eq_cb = *cached_equity_loop.read().await;
                    let cluster_cb_reason = check_cluster_circuit_breaker(now_ms_cb, live_eq_cb);
                    if let Some(ref reason) = cluster_cb_reason {
                        println!(
                            "🚨 [CLUSTER CROSS-MARGIN CIRCUIT BREAKER ACTIVE] {} locked out: {}",
                            symbol_clone, reason
                        );
                    }
                    let is_cb_paused = symbol_cb_paused || cluster_cb_reason.is_some();

                    let cluster_active = has_active_cluster_position(&symbol_clone);
                    let is_cluster_blocked = cluster_active.is_some();

                    funnel_counters.total_1m_ticks += 1;
                    if candle_feed_degraded {
                        funnel_counters.feed_quality_degraded += 1;
                    }
                    if is_candle_gap {
                        funnel_counters.candle_gap_detected += 1;
                    }
                    if controller.shock_cooldown_bars > 0
                        || (controller.cooldown_until_ms > 0
                            && (chrono::Utc::now().timestamp_millis().max(0) as u64)
                                < controller.cooldown_until_ms)
                    {
                        funnel_counters.shock_cooldown_active += 1;
                    }
                    if z_15.abs() >= 2.5 {
                        funnel_counters.short_term_shock_veto += 1;
                    }
                    if z_60.abs() >= 2.5 {
                        funnel_counters.lingering_macro_shock_veto += 1;
                    }
                    if is_cluster_blocked {
                        funnel_counters.portfolio_lock_congested += 1;
                    }
                    if is_cb_paused {
                        funnel_counters.circuit_breaker_paused += 1;
                    }

                    // ------------------------------------------------------------------
                    // 3-MODE CONTROLLER REGIME INFERENCE (HMM Causal Filter + Hysteresis)
                    // ------------------------------------------------------------------
                    // Fast-path safety: Downside flash crashes unconditionally initiate/preserve Neutral cooldown to protect against liquidation cascades.
                    // Upside impulses only force Neutral if HMM detects a severe macro shock regime (current_p_s >= 0.50) or extreme macro volatility (|Z15| >= 3.5).
                    // Normal intraday upside bounces do not force a cash lockout, and individual Short entries already independently check !is_upside_shock.
                    let is_controller_shock_1m = live_feat.is_downside_shock
                        || (live_feat.is_upside_shock && (current_p_s >= 0.50 || z_15.abs() >= 3.5));

                    let session_closed_for_shock = matches!(
                        evaluate_session_lifecycle_gate(&symbol_clone, completed_bar.time_open as i64),
                        SessionLifecycleAction::ForceFlat(_)
                    );
                    if is_controller_shock_1m && !session_closed_for_shock {
                        if last_shock_bar_ms != completed_bar.time_open {
                            last_shock_bar_ms = completed_bar.time_open;
                            let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                            controller.force_shock_cooldown(z_15, z_60, now_ms);
                            save_controller_state(&symbol_clone, &controller);
                            println!(
                                "🚨 [CONTROLLER SHOCK VETO] Severe shock on 1m bar {} (Z15: {:.2}, Z60: {:.2}) -> {}",
                                completed_bar.time_open, z_15, z_60, controller.last_reason
                            );
                        }
                    }

                    let active_gen = hmm_generation.load(std::sync::atomic::Ordering::Relaxed);
                    if active_gen != controller.model_generation {
                        controller.notify_model_hotswap();
                        controller.model_generation = active_gen;
                        save_controller_state(&symbol_clone, &controller);
                        if rolling_15m_bars.len() >= 15 {
                            let hmm_lock = hmm_model.read().await;
                            let closes_15m: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
                            if let Some((p_c, p_t, p_s)) = compute_hmm_posteriors(&*hmm_lock, &closes_15m) {
                                current_p_c = p_c;
                                current_p_t = p_t;
                                current_p_s = p_s;
                            }
                        }
                    }

                    // Strictly evaluate 15m dwell streaks & state transitions on finalized 15m candle closes!
                    if let Some(ref closed_15m) = finalized_15m_bar {
                        let bar_close_ms = closed_15m.time_open + 15 * 60 * 1000;
                        let decision_close = closed_15m.close;
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
                        // Strict Close-confirmed breakout (wicks eliminated to prevent false breakouts)
                        let has_donchian_breakout = if let Some(ref d) = donchian_opt {
                            decision_close > d.upper || decision_close < d.lower
                        } else {
                            false
                        };

                        // Features evaluated strictly at the cutoff of the finalized 15m candle.
                        // Upside impulses only veto into Neutral if NOT in confirmed trend regime.
                        let d_feat = decision_features.as_ref().unwrap_or(&live_feat);
                        let is_severe_shock_15m = d_feat.is_downside_shock
                            || (d_feat.is_upside_shock && (p_t < 0.82 || p_s >= 0.50));
                        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                        let prev_mode = controller.current_mode;
                        if let Some(new_mode) = controller.evaluate_15m_bar_idempotent(
                            bar_close_ms,
                            now_ms,
                            p_c,
                            p_t,
                            d_feat.z_15,
                            d_feat.z_60,
                            has_donchian_breakout,
                            d_feat.is_stationary,
                            d_feat.is_valid_half_life,
                            is_severe_shock_15m,
                        ) {
                            save_controller_state(&symbol_clone, &controller);

                            println!(
                                "🧭 [3-MODE CONTROLLER 15m EVAL] Mode: {} (was {}) | Streaks: Trend={}, Consolidation={} | P_C: {:.1}% | P_T: {:.1}% | P_Shock: {:.1}% | Reason: {}",
                                new_mode, prev_mode, controller.trend_streak, controller.consolidation_streak, p_c * 100.0, p_t * 100.0, p_s * 100.0, controller.last_reason
                            );

                            let (gate_a, gate_b) = controller.inspect_gates(
                                p_c,
                                p_t,
                                d_feat.z_15,
                                d_feat.z_60,
                                has_donchian_breakout,
                                d_feat.is_stationary,
                                d_feat.is_valid_half_life,
                                now_ms,
                            );
                            println!(
                                "🔍 [GATE FUNNEL] {} | Vol: (Z15:{:.2}, Z60:{:.2}) | Cooldown: {}b | Donchian: {} | Engine A Gate: {} | Engine B Gate: {}",
                                symbol_clone, d_feat.z_15, d_feat.z_60, controller.shock_cooldown_bars,
                                if has_donchian_breakout { "BREAKOUT" } else { "RANGE" },
                                gate_a,
                                gate_b
                            );
                            println!("{}", funnel_counters.format_summary());
                            save_funnel_counters(&symbol_clone, &funnel_counters);
                        }

                        // Non-blocking background equity refresh on 15m bar close
                        if config.sizing_mode == "dynamic" {
                            let info_bg = info_clone.clone();
                            let cached_eq_bg = cached_equity_loop.clone();
                            let hwm_bg = high_water_mark_equity.clone();
                            tokio::spawn(async move {
                                if let Ok(account_val) = get_account_value(&info_bg, user_address).await {
                                    if account_val > 0.0 {
                                        {
                                            let mut w = cached_eq_bg.write().await;
                                            *w = account_val;
                                        }
                                        let mut hwm = hwm_bg.write().await;
                                        if account_val > *hwm {
                                            *hwm = account_val;
                                        }
                                    }
                                }
                            });
                        }
                    }

                    let donchian_ref_bars = if finalized_15m_bar.is_some() && rolling_15m_bars.len() > 1 {
                        &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                    } else {
                        &rolling_15m_bars[..]
                    };
                    let donchian_channel_opt = calculate_donchian(donchian_ref_bars, donchian_period);

                    let closes_15m_curr: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
                    let alignment_curr = check_trend_alignment(&closes_15m_curr);
                    let align_str = match alignment_curr {
                        TrendAlignment::Bullish => "BULLISH",
                        TrendAlignment::Bearish => "BEARISH",
                        TrendAlignment::Neutral => "NEUTRAL",
                    };

                    let (donchian_upper_val, donchian_lower_val, donchian_mid_val, dist_up_pct, dist_low_pct, donchian_status) = match donchian_channel_opt {
                        Some(ref d) => {
                            let up_dist = ((d.upper - close_price) / close_price) * 100.0;
                            let low_dist = ((close_price - d.lower) / close_price) * 100.0;
                            let status = if close_price > d.upper {
                                "BREAKOUT_HIGH"
                            } else if close_price < d.lower {
                                "BREAKOUT_LOW"
                            } else {
                                "COILING"
                            };
                            (Some(d.upper), Some(d.lower), Some(d.middle), up_dist, low_dist, status.to_string())
                        }
                        None => (None, None, None, 0.0, 0.0, "AWAITING_DATA".to_string()),
                    };

                    let (pos_has, pos_side, pos_amount, pos_entry, pos_tp, pos_sl, pos_chand, pos_eng_lbl) = {
                        let sl = state_clone.lock().await;
                        if let Some(p) = sl.get(&symbol_clone) {
                            (true, p.side.clone(), p.amount, p.entry_price, p.take_profit_price, p.stop_loss_price, p.chandelier_stop, p.engine_type.clone())
                        } else {
                            (false, "".to_string(), 0.0, 0.0, 0.0, 0.0, 0.0, "".to_string())
                        }
                    };
                    let active_engine = if pos_has && !pos_eng_lbl.is_empty() {
                        pos_eng_lbl.to_uppercase()
                    } else {
                        "MARKOV_V2_MACRO".to_string()
                    };

                    let st_dir_str = if engine_b_4h.st_dir > 0 {
                        "BULL"
                    } else if engine_b_4h.st_dir < 0 {
                        "BEAR"
                    } else {
                        "WARMING"
                    };
                    println!(
                        "💓 [MARKOV-POLO V2.1] {} Px: ${:.4} | 4H ST: {} (ATR4h: ${:.4}) | 1H EMA50: ${:.4} (ATR1h: ${:.4}) | Pos: {}",
                        symbol_clone,
                        close_price,
                        st_dir_str,
                        engine_b_4h.atr_4h,
                        engine_b_1h.ema50,
                        engine_b_1h.atr_1h,
                        if pos_has { format!("{} @ ${:.4} (SL ${:.4})", pos_side, pos_entry, pos_sl) } else { "FLAT".to_string() },
                    );

                    if let Some(qual) = obi_tracker.read().await.compute_feed_quality() {
                        if qual.sample_count >= 10 && finalized_15m_bar.is_some() {
                            let (inst_obi, ewma_250, ewma_1s, ewma_5s, gaps) = {
                                let tr = obi_tracker.read().await;
                                (tr.instant_obi, tr.ewma_250ms_obi, tr.ewma_1s_obi, tr.ewma_5s_obi, tr.gaps_over_3s)
                            };
                            let window_end = chrono::Utc::now().timestamp_millis().max(0) as u64;
                            let window_start = window_end.saturating_sub(15 * 60 * 1000);
                            crate::trend::append_l2_telemetry_record(
                                &symbol_clone,
                                window_start,
                                window_end,
                                &qual,
                                gaps,
                                inst_obi,
                                ewma_250,
                                ewma_1s,
                                ewma_5s,
                            );
                            println!(
                                "📶 [L2 FEED QUALITY] {} | Status: {} ({}) | Inter-Arrival: (p50: {:.0}ms, p90: {:.0}ms, p95: {:.0}ms, p99: {:.0}ms, MAD: {:.0}ms) | Observed Age: (p95: {:.0}ms, p99: {:.0}ms, Mean: {:.0}ms) | EWMAs: (tau1: {:.0}ms, tau2: {:.0}ms) ({} samples)",
                                symbol_clone, qual.status, qual.regime, qual.p50_ms, qual.p90_ms, qual.p95_ms, qual.p99_ms, qual.mad_ms, qual.observed_age_p95_ms, qual.observed_age_p99_ms, qual.mean_lag_ms, qual.tau_1_ms, qual.tau_2_ms, qual.sample_count
                            );
                        }
                    }

                    // ------------------------------------------------------------------
                    // LIVE TELEMETRY: Real-time Metrics & Macro V2.1 Status for Dashboard
                    // ------------------------------------------------------------------
                    let (long_buy_val, short_sell_val, exit_long_val, exit_short_val) = match thresholds_opt {
                        Some(ref th) => (Some(th.entry_long), Some(th.entry_short), Some(th.exit_long), Some(th.exit_short)),
                        None => (None, None, None, None),
                    };

                    let (prox_long, prox_short, overall_prox, prox_summary) = if pos_has {
                        let summary = format!(
                            "V2.1 {} {} Active | Entry ${:.4} | Trail SL ${:.4}",
                            pos_eng_lbl, pos_side, pos_entry, pos_sl
                        );
                        (100.0, 100.0, 100.0, summary)
                    } else if engine_b_1h.ema50 > 0.0 && engine_b_4h.atr_4h > 0.0 {
                        let dist_ema50_pct = ((close_price - engine_b_1h.ema50).abs() / close_price) * 100.0;
                        let pb_prox = (1.0 - (dist_ema50_pct / 3.0).min(1.0)) * 100.0;
                        let summary = format!(
                            "V2.1 Macro | 4H ST: {} | 1H EMA50: ${:.4} ({:.2}% away) | ATR4h: ${:.4}",
                            st_dir_str, engine_b_1h.ema50, dist_ema50_pct, engine_b_4h.atr_4h
                        );
                        (pb_prox, pb_prox, pb_prox, summary)
                    } else {
                        (0.0, 0.0, 0.0, "V2.1 Macro: Warming 1H/4H Bars".to_string())
                    };

                    let (l2_instant, l2_250ms, l2_1s, l2_5s, l2_fresh, l2_bid_snap, l2_ask_snap) = {
                        let guard = obi_tracker.read().await;
                        (
                            guard.instant_obi,
                            guard.ewma_250ms_obi,
                            guard.ewma_1s_obi,
                            guard.ewma_5s_obi,
                            guard.is_fresh_and_uncrossed(chrono::Utc::now().timestamp_millis() as u64, L2_ORDERBOOK_FRESH_MS),
                            guard.best_bid,
                            guard.best_ask,
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
                        is_severe_shock: is_controller_shock_1m || controller.shock_cooldown_bars > 0,
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
                        donchian_upper: donchian_upper_val,
                        donchian_lower: donchian_lower_val,
                        donchian_middle: donchian_mid_val,
                        donchian_status,
                        donchian_dist_upper_pct: dist_up_pct,
                        donchian_dist_lower_pct: dist_low_pct,
                        trend_alignment: align_str.to_string(),
                        active_engine,
                        account_equity: *cached_equity_loop.read().await,
                        high_water_mark: *high_water_mark_equity.read().await,
                        ret_1m_bps: if let Some(&prev1) = rolling_closes.last() {
                            if prev1 > 0.0 {
                                (close_price / prev1).ln() * 1e4
                            } else {
                                0.0
                            }
                        } else {
                            0.0
                        },
                        ret_3m_bps: if rolling_closes.len() >= 4 && rolling_closes[rolling_closes.len() - 4] > 0.0 {
                            (close_price / rolling_closes[rolling_closes.len() - 4]).ln() * 1e4
                        } else {
                            0.0
                        },
                        ret_5m_bps: if rolling_closes.len() >= 6 && rolling_closes[rolling_closes.len() - 6] > 0.0 {
                            (close_price / rolling_closes[rolling_closes.len() - 6]).ln() * 1e4
                        } else {
                            0.0
                        },
                        ret_15m_bps: if rolling_closes.len() >= 16 && rolling_closes[rolling_closes.len() - 16] > 0.0 {
                            (close_price / rolling_closes[rolling_closes.len() - 16]).ln() * 1e4
                        } else {
                            0.0
                        },
                        ret_30m_bps: if rolling_closes.len() >= 31 && rolling_closes[rolling_closes.len() - 31] > 0.0 {
                            (close_price / rolling_closes[rolling_closes.len() - 31]).ln() * 1e4
                        } else {
                            0.0
                        },
                        ret_60m_bps: if rolling_closes.len() >= 61 && rolling_closes[rolling_closes.len() - 61] > 0.0 {
                            (close_price / rolling_closes[rolling_closes.len() - 61]).ln() * 1e4
                        } else {
                            0.0
                        },
                    };
                    save_telemetry(&symbol_clone, &telemetry_data);

                    // Continuous NAS Market History Recorder (`hl_history_{symbol}.jsonl`):
                    // Persists 1m OHLCV + L2 Bid/Ask/OBI + Hourly Funding + Open Interest + Mark/Oracle prices
                    {
                        let (oi_snap, mark_snap, oracle_snap) = {
                            let cg = crowding_tracker.read().await;
                            (cg.open_interest, cg.mark_px, cg.oracle_px)
                        };
                        let tick_rec = effective_tick_size(close_price, sz_decimals);
                        crate::backtest::append_historical_bar_record(
                            &crate::backtest::HistoricalMarketBarRecord {
                                symbol: symbol_clone.clone(),
                                timestamp_ms: completed_bar.time_open,
                                open: open_price,
                                high: high_price,
                                low: low_price,
                                close: close_price,
                                volume: completed_bar.volume,
                                best_bid: if l2_bid_snap > 0.0 { l2_bid_snap } else { close_price - 0.5 * tick_rec },
                                best_ask: if l2_ask_snap > 0.0 { l2_ask_snap } else { close_price + 0.5 * tick_rec },
                                obi_instant: l2_instant,
                                obi_1s: l2_1s,
                                obi_5s: l2_5s,
                                funding_1h: current_funding_1h,
                                open_interest: oi_snap,
                                mark_px: if mark_snap > 0.0 { mark_snap } else { close_price },
                                oracle_px: if oracle_snap > 0.0 { oracle_snap } else { close_price },
                                obi_ewma_tau1: Some(l2_250ms),
                                obi_ewma_tau2: Some(l2_1s),
                                obi_1m_twap: Some(l2_1s),
                            },
                        );
                    }

                    // ------------------------------------------------------------------
                    // MARKOV-POLO V2.1: TRIPLE-PILLAR MACRO TREND ENGINE
                    //   Priority 1: 4H SuperTrend-3.0 Flip (7Y PF 1.49, 7/7 yrs)
                    //   Priority 2: 4H Donchian-40 Breakout (7Y PF 1.24, 7/7 yrs)
                    //   Priority 3: 1H EMA50 Pullback-in-Trend (7Y PF 1.24, 7/7 yrs)
                    // ------------------------------------------------------------------
                    let macro_dispatch_opt: Option<(&'static str, &'static str, Side, f64, f64, f64, f64, f64, usize)> =
                        if let Some(ref s4) = engine_b_4h_sig_opt {
                            let lbl = if s4.policy_tag == "ENGINE_B_4H_ST3" {
                                "4H SUPERTREND 3.0 FLIP"
                            } else {
                                "4H DONCHIAN-40 BREAKOUT"
                            };
                            Some((
                                "EngineB4h",
                                lbl,
                                s4.side.clone(),
                                s4.ref_price,
                                s4.initial_stop_px,
                                s4.atr_4h,
                                s4.trail_atr_mult,
                                s4.max_sl_frac,
                                s4.max_hold_hours,
                            ))
                        } else {
                            engine_b_1h_sig_opt.as_ref().map(|s1| {
                                (
                                    "EngineB1h",
                                    "1H EMA50 PULLBACK",
                                    s1.side.clone(),
                                    s1.ref_price,
                                    s1.initial_stop_px,
                                    s1.atr_1h,
                                    s1.trail_atr_mult,
                                    0.032,
                                    s1.max_hold_hours,
                                )
                            })
                        };

                    if let Some((eng_tag, reason_lbl, sig_side, ref_px, init_sl_px, sig_atr, trail_m, max_sl_f, max_hold_h)) =
                        macro_dispatch_opt
                    {
                        let session_ok_m = matches!(
                            evaluate_session_lifecycle_gate(&symbol_clone, now_ms_cb as i64),
                            SessionLifecycleAction::AllowNormalTrading
                        );
                        let macro_veto_m = check_macro_calendar_veto(now_ms_cb);
                        let side_label_m = if sig_side == Side::Long { "LONG" } else { "SHORT" };
                        let macro_cluster_veto_m =
                            crate::state::has_active_macro_cluster_veto(&symbol_clone, side_label_m);
                        let mut state_lock_m = state_clone.lock().await;
                        let already_in_pos_m = state_lock_m.contains_key(&symbol_clone);
                        if !already_in_pos_m
                            && session_ok_m
                            && !candle_feed_degraded
                            && !is_candle_gap
                            && !is_cb_paused
                            && macro_cluster_veto_m.is_none()
                            && macro_veto_m.is_none()
                            && trend_engine_mode == "live"
                            && is_symbol_live_capital_enabled(&symbol_clone)
                        {
                            let _ = resolve_trade_bank(&info_clone, user_address, &config, &cached_equity_loop).await;
                            let live_eq_m = *cached_equity_loop.read().await;
                            let hwm_eq_m = *high_water_mark_equity.read().await;
                            let dd_mult_m = compute_macro_drawdown_risk_multiplier(live_eq_m, hwm_eq_m);
                            let sl_dist_m = (ref_px - init_sl_px).abs();
                            let existing_cluster_ntl =
                                crate::state::total_active_cluster_notional_excluding(&symbol_clone);
                            let max_slots_m = crate::state::max_concurrent_cluster_positions();

                            if let Some(notional_m) =
                                crate::hyperliquid::calculate_zero_liquidation_macro_notional(
                                    live_eq_m,
                                    existing_cluster_ntl,
                                    max_slots_m,
                                    config.max_position_size_usdc,
                                    0.0075 * dd_mult_m, // 0.75% equity risk/trade (~$4.20 on $560), hard-capped <= 1.00x total portfolio equity
                                    ref_px,
                                    init_sl_px,
                                    config.hl_leverage,
                                )
                            {
                                println!(
                                    "🚀 [MARKOV-POLO V2.1 {}] {} {:?} | Ref: ${:.4} | SL: ${:.4} | ATR=${:.4} | Notional=${:.2} (ClusterExp: ${:.2}/${:.2} <= 1.00x) | MaxHold={}h",
                                    reason_lbl,
                                    symbol_clone,
                                    sig_side,
                                    ref_px,
                                    init_sl_px,
                                    sig_atr,
                                    notional_m,
                                    existing_cluster_ntl + notional_m,
                                    live_eq_m,
                                    max_hold_h
                                );
                                match place_macro_trend_ioc_entry(
                                    &state_path_clone,
                                    &exchange_clone,
                                    &info_clone,
                                    &mut state_lock_m,
                                    &symbol_clone,
                                    eng_tag,
                                    reason_lbl,
                                    sig_side,
                                    sl_dist_m,
                                    sig_atr,
                                    trail_m,
                                    max_sl_f,
                                    max_hold_h,
                                    notional_m,
                                    sz_decimals,
                                    discord_clone.clone(),
                                )
                                .await
                                {
                                    Ok(res) => {
                                        funnel_counters.engine_b_orders_placed += 1;
                                        println!(
                                            "✅ [MARKOV-POLO V2.1 ENTRY EXECUTED] {} ({}) -> {:?}",
                                            symbol_clone, reason_lbl, res
                                        );
                                    }
                                    Err(e) => {
                                        funnel_counters.engine_b_orders_rejected += 1;
                                        eprintln!(
                                            "❌ [MARKOV-POLO V2.1 ENTRY ERROR] {} ({}): {}",
                                            symbol_clone, reason_lbl, e
                                        );
                                    }
                                }
                            } else {
                                println!(
                                    "🛡️ [ZERO-LIQUIDATION CAP VETO] {} {} {:?} skipped: cluster exposure ${:.2} already at 1.00x equity (${:.2}) ceiling.",
                                    symbol_clone, reason_lbl, sig_side, existing_cluster_ntl, live_eq_m
                                );
                            }
                        } else {
                            println!(
                                "⏸️ [MARKOV-POLO V2.1 SIGNAL GATED] {} {} {:?} (pos={}, sess={}, cb={}, cluster_veto={:?}, macro={})",
                                symbol_clone,
                                reason_lbl,
                                sig_side,
                                already_in_pos_m,
                                session_ok_m,
                                is_cb_paused,
                                macro_cluster_veto_m,
                                macro_veto_m.is_some()
                            );
                        }
                    }

                    // ------------------------------------------------------------------
                    // SESSION LIFECYCLE GATE (Friday NY Pre-Close Flat for US Equities)
                    // ------------------------------------------------------------------
                    let session_gate = evaluate_session_lifecycle_gate(&symbol_clone, now_ms_cb as i64);
                    if let SessionLifecycleAction::ForceFlat(flat_reason) = session_gate {
                        let mut sl_flat = state_clone.lock().await;
                        if sl_flat.contains_key(&symbol_clone) {
                            println!(
                                "🔔 [1M SESSION FLAT-AT-CLOSE] {} triggering scheduled close/maintenance flatten ({}).",
                                symbol_clone, flat_reason
                            );
                            let _ = execute_emergency_flatten_with_retry(
                                &state_path_clone,
                                &exchange_clone,
                                &info_clone,
                                user_address,
                                &mut sl_flat,
                                &symbol_clone,
                                sz_decimals,
                                discord_clone.clone(),
                            )
                            .await;
                        }
                    }

                    // ------------------------------------------------------------------
                    // ACTIVE POSITION LIFECYCLE (Markov-Polo V2.1 Chandelier Trail + Stop Ratchet)
                    // ------------------------------------------------------------------
                    let mut state_lock = state_clone.lock().await;
                    if state_lock.contains_key(&symbol_clone) {
                        // Failsafe Exchange Reconciliation if local state still shows `!pos.is_filled`
                        let needs_fill_check = state_lock
                            .get(&symbol_clone)
                            .map(|p| !p.is_filled)
                            .unwrap_or(false);
                        if needs_fill_check {
                            if let Ok((live_szi, _)) =
                                get_user_position(&info_clone, user_address, &symbol_clone).await
                            {
                                if live_szi.abs() > 1e-5 {
                                    if let Some(p) = state_lock.get_mut(&symbol_clone) {
                                        p.is_filled = true;
                                        p.amount = live_szi.abs();
                                        p.opened_at = chrono::Utc::now().to_rfc3339();
                                    }
                                    let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                    sentinel_notify.notify_one();
                                }
                            }
                        }

                        // Refresh cross-container cluster lease heartbeat (3600s TTL)
                        if let Some(pos) = state_lock.get(&symbol_clone) {
                            let refresh_ok = crate::state::refresh_cluster_lease_authenticated(
                                &symbol_clone,
                                Some(&pos.lease_owner_uuid),
                                Some(pos.lease_fencing_token),
                                3600,
                            );
                            if !refresh_ok {
                                eprintln!(
                                    "⚠️ [PORTFOLIO LEASE LOST] Cluster lease refresh failed for {}.",
                                    symbol_clone
                                );
                            }
                        }

                        let mut chandelier_stopped = false;
                        let mut time_stop_triggered = false;
                        let mut elapsed_mins = 0.0;
                        let mut max_allowed_mins = 288.0 * 60.0;
                        let mut sl_ratchet_sync_opt: Option<(f64, f64, bool, Option<u64>)> = None;

                        if let Some(pos) = state_lock.get_mut(&symbol_clone) {
                            if pos.is_filled {
                                if let Ok(opened_dt) =
                                    chrono::DateTime::parse_from_rfc3339(&pos.opened_at)
                                {
                                    elapsed_mins = (chrono::Utc::now()
                                        - opened_dt.with_timezone(&chrono::Utc))
                                    .num_seconds()
                                        as f64
                                        / 60.0;
                                }
                                let is_4h_pos = pos.engine_type != "EngineB1h";
                                let side_enum = if pos.side == "LONG" {
                                    Side::Long
                                } else {
                                    Side::Short
                                };
                                let is_long_m = pos.side == "LONG";
                                let prev_stop = if pos.chandelier_stop > 0.0 {
                                    pos.chandelier_stop
                                } else {
                                    pos.stop_loss_price
                                };
                                if prev_stop > 0.0
                                    && ((is_long_m && (low_price <= prev_stop || close_price <= prev_stop))
                                        || (!is_long_m
                                            && (high_price >= prev_stop || close_price >= prev_stop)))
                                {
                                    chandelier_stopped = true;
                                } else {
                                    if is_long_m && high_price > pos.max_favorable_price {
                                        pos.max_favorable_price = high_price;
                                    } else if !is_long_m
                                        && (pos.max_favorable_price <= 0.0
                                            || low_price < pos.max_favorable_price)
                                    {
                                        pos.max_favorable_price = low_price;
                                    }
                                    let trail_m = if pos.active_multiplier > 0.0 {
                                        pos.active_multiplier
                                    } else {
                                        4.0
                                    };
                                    let cur_atr_m = if is_4h_pos {
                                        if engine_b_4h.atr_4h > 0.0 {
                                            engine_b_4h.atr_4h
                                        } else {
                                            pos.current_atr
                                        }
                                    } else if engine_b_1h.atr_1h > 0.0 {
                                        engine_b_1h.atr_1h
                                    } else {
                                        pos.current_atr
                                    };
                                    let new_stop = quantize_price(
                                        TrendPullback1hEngine::update_trailing_stop(
                                            &side_enum,
                                            prev_stop,
                                            pos.max_favorable_price,
                                            cur_atr_m,
                                            pos.current_atr,
                                            trail_m,
                                        ),
                                        sz_decimals,
                                    );
                                    let stop_improved_bps = if prev_stop > 0.0 {
                                        let d_s = if is_long_m { 1.0 } else { -1.0 };
                                        d_s * ((new_stop - prev_stop) / prev_stop) * 1e4
                                    } else {
                                        0.0
                                    };
                                    if stop_improved_bps > 0.0 {
                                        pos.chandelier_stop = new_stop;
                                        pos.stop_loss_price = new_stop;
                                    }
                                    if stop_improved_bps >= 15.0 {
                                        sl_ratchet_sync_opt = Some((
                                            new_stop,
                                            pos.amount,
                                            !is_long_m,
                                            pos.disaster_stop_oid,
                                        ));
                                    }
                                    max_allowed_mins = if pos.half_life_minutes >= 60.0 {
                                        pos.half_life_minutes
                                    } else if is_4h_pos {
                                        288.0 * 60.0
                                    } else {
                                        96.0 * 60.0
                                    };
                                    if elapsed_mins >= max_allowed_mins {
                                        time_stop_triggered = true;
                                    }
                                    println!(
                                        "🚀 [MARKOV-POLO V2.1 {} RUNNER] {} {} | Px: ${:.4} | Trail SL: ${:.4} | Extreme: ${:.4} | Hold: {:.1}h/{:.0}h",
                                        pos.engine_type,
                                        symbol_clone,
                                        pos.side,
                                        close_price,
                                        pos.stop_loss_price,
                                        pos.max_favorable_price,
                                        elapsed_mins / 60.0,
                                        max_allowed_mins / 60.0
                                    );
                                }
                            }
                        }
                        let _ = crate::state::save_state(&state_path_clone, &state_lock);

                        // Sync exchange-native StopMarket trigger on Hyperliquid when trailing stop improves >= 15 bps
                        if let Some((new_sl_px, sl_amt, sl_is_buy, old_sl_oid)) = sl_ratchet_sync_opt {
                            if let Ok(new_sl_oid) = crate::hyperliquid::ratchet_disaster_stop_to_breakeven(
                                &exchange_clone,
                                &symbol_clone,
                                sl_is_buy,
                                sl_amt,
                                new_sl_px,
                                sz_decimals,
                                old_sl_oid,
                                Some(uuid::Uuid::new_v4()),
                            )
                            .await
                            {
                                if let Some(p) = state_lock.get_mut(&symbol_clone) {
                                    p.disaster_stop_oid = Some(new_sl_oid);
                                    let _ = crate::state::save_state(&state_path_clone, &state_lock);
                                }
                                println!(
                                    "🛡️ [MARKOV-POLO V2.1 STOP RATCHET] {} updated Exchange-Native StopMarket to ${:.4} (oid: {})",
                                    symbol_clone, new_sl_px, new_sl_oid
                                );
                            }
                        }

                        let fast_flatten_bbo = if l2_fresh && l2_bid_snap > 0.0 && l2_ask_snap >= l2_bid_snap {
                            Some((l2_bid_snap, l2_ask_snap))
                        } else {
                            None
                        };

                        if chandelier_stopped {
                            let (sl_px, ent_px, is_long_c) = state_lock
                                .get(&symbol_clone)
                                .map(|p| (p.chandelier_stop, p.entry_price, p.side == "LONG"))
                                .unwrap_or((0.0, 0.0, true));
                            let is_profit_exit = ent_px > 0.0
                                && ((is_long_c && close_price > ent_px)
                                    || (!is_long_c && close_price < ent_px));
                            println!(
                                "🛑 [CHANDELIER TRAILING STOP EXECUTED HL] {} stopped out at ${:.4} (Stop: ${:.4}). Flattening!",
                                symbol_clone, close_price, sl_px
                            );
                            let icon = if is_profit_exit { "🏆" } else { "🛑" };
                            let ch_msg = format!(
                                "{} **MARKOV-POLO V2.1 TRAILING STOP EXECUTED:** {} @ ${:.4} (Entry: ${:.4} | Stop: ${:.4}).",
                                icon, symbol_clone, close_price, ent_px, sl_px
                            );
                            spawn_discord_alert_bg(discord_clone.clone(), ch_msg);
                            let flatten_res = execute_emergency_flatten_with_retry_bbo(
                                &state_path_clone,
                                &exchange_clone,
                                &info_clone,
                                user_address,
                                &mut state_lock,
                                &symbol_clone,
                                sz_decimals,
                                discord_clone.clone(),
                                fast_flatten_bbo,
                            )
                            .await;
                            drop(state_lock);
                            if flatten_res.is_ok() {
                                let mut cb_lock = cb_clone.lock().await;
                                if is_profit_exit {
                                    cb_lock.record_win();
                                } else {
                                    cb_lock.record_loss();
                                }
                                save_circuit_breaker(&cb_path_clone, &cb_lock);
                            }
                        } else if time_stop_triggered {
                            let (ent_px, is_long_t) = state_lock
                                .get(&symbol_clone)
                                .map(|p| (p.entry_price, p.side == "LONG"))
                                .unwrap_or((0.0, true));
                            let is_profit_exit = ent_px > 0.0
                                && ((is_long_t && close_price > ent_px)
                                    || (!is_long_t && close_price < ent_px));
                            println!(
                                "⏱️ [MACRO MAX-HOLD EXIT] {} reached max hold {:.1}h >= {:.0}h. Flattening at ${:.4}.",
                                symbol_clone, elapsed_mins / 60.0, max_allowed_mins / 60.0, close_price
                            );
                            let _ = execute_emergency_flatten_with_retry_bbo(
                                &state_path_clone,
                                &exchange_clone,
                                &info_clone,
                                user_address,
                                &mut state_lock,
                                &symbol_clone,
                                sz_decimals,
                                discord_clone.clone(),
                                fast_flatten_bbo,
                            )
                            .await;
                            drop(state_lock);
                            let mut cb_lock = cb_clone.lock().await;
                            if is_profit_exit {
                                cb_lock.record_win();
                            } else {
                                cb_lock.record_loss();
                            }
                            save_circuit_breaker(&cb_path_clone, &cb_lock);
                            let time_msg = format!(
                                "⏱️ **MARKOV-POLO V2.1 MAX-HOLD EXIT:** {} held {:.1}h (limit {:.0}h). Closed @ ${:.4} (Entry: ${:.4}).",
                                symbol_clone, elapsed_mins / 60.0, max_allowed_mins / 60.0, close_price, ent_px
                            );
                            crate::discord::send_alert(discord_clone.as_deref(), &time_msg).await;
                        }
                    }

                    // Advance rolling window with finalized 1m close & shared Fast-Sentinel price
                    *last_1m_close_shared.write().await = close_price;
                    prev_kalman_vel_bps_min = live_features
                        .as_ref()
                        .map(|f| (f.kalman_slope / 5.0) * 1e4)
                        .unwrap_or(prev_kalman_vel_bps_min);
                    if rolling_closes.len() >= 960 {
                        rolling_closes.remove(0);
                    }
                    rolling_closes.push(close_price);
                }
            }
        }

        if let Some(h) = candle_mux_handle {
            h.abort();
        }
        if used_candle_mux {
            if received_any_candle {
                candle_mux_failures = 0;
            } else {
                candle_mux_failures = candle_mux_failures.saturating_add(1);
            }
        } else {
            candle_mux_failures = 0;
        }
        eprintln!("⚠️ [WEBSOCKET] Stream ended or dropped. Reconnecting in 2s...");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_fast_sentinel_intrabar_escape, compute_tp_order_plan,
        compute_tp_order_plan_with_mode, is_symbol_live_capital_enabled,
    };

    #[test]
    fn test_compute_tp_order_plan_preserves_two_tranche_splits() {
        // Engine A Short AVAX: 14.6 AVAX @ $8.6032 ($125.61 >= $28) -> T1-35% = 5.11 @ 65% target
        let (qty1, px1, tag1) = compute_tp_order_plan("EngineA", 8.6032, 8.5362, 14.6, false, 2);
        assert_eq!(tag1, "T1-35%");
        assert!((qty1 - 5.11).abs() < 1e-6);
        assert!(px1 < 8.6032 && px1 > 8.5362, "got {}", px1);

        // Engine A Tranche 2 after T1 filled -> FULL remaining 9.49 @ target_mu
        let (qty2, px2, tag2) = compute_tp_order_plan("EngineA", 8.6032, 8.5469, 9.49, true, 2);
        assert_eq!(tag2, "FULL");
        assert!((qty2 - 9.49).abs() < 1e-6);
        assert!((px2 - 8.5469).abs() < 1e-6);

        // Engine B Long -> T1-50% directly at 1.5 ATR target
        let (qty_b, px_b, tag_b) = compute_tp_order_plan("EngineB", 100.0, 102.5, 0.50, false, 2);
        assert_eq!(tag_b, "T1-50%");
        assert!((qty_b - 0.25).abs() < 1e-6);
        assert!((px_b - 102.5).abs() < 1e-6);

        // Sub-$28 order stays single-tranche FULL so both legs never violate $10 MinTradeNtl
        let (qty_sm, _, tag_sm) = compute_tp_order_plan("EngineA", 10.0, 10.2, 2.0, false, 2);
        assert_eq!(tag_sm, "FULL");
        assert!((qty_sm - 2.0).abs() < 1e-6);

        // Package 5/8B: ZEC Short ratchet to $907.05 (25.3 bps from $909.35) must honor 22 bps T1 floor
        let (qty_z, px_z, tag_z) = compute_tp_order_plan("EngineA", 909.35, 907.05, 0.11, false, 3);
        assert_eq!(tag_z, "T1-35%");
        assert!((qty_z - 0.039).abs() < 1e-6);
        assert!(px_z <= 909.35 * 0.9978 + 0.05, "T1 violated 22 bps floor: {}", px_z);

        // Explicit harvest target (Stall-Hazard / State-3 Maker-Peg) bypasses 0.65x shaving completely
        let (_, px_exact, _) =
            compute_tp_order_plan_with_mode("EngineA", 909.35, 907.05, 0.11, false, true, 3);
        assert!((px_exact - 907.05).abs() < 1e-4, "expected 907.05, got {}", px_exact);
    }

    #[test]
    fn test_package8b_fast_sentinel_intrabar_escape_avax_crash_and_normal_wick_preservation() {
        // 1. AVAX 17/11 15:02-15:06 flash dump: -34.8 bps 1m bar + BTC -50.5 bps/5m + CUSUM >= eff_b_star
        //    -> Fast-Sentinel cuts intrabar at -34.5 bps instead of waiting 59s for -60.7 bps!
        let esc_crash = check_fast_sentinel_intrabar_escape(-34.5, 0, 3.15, 3.08, -34.8, -50.5);
        assert_eq!(esc_crash, Some("FAST_SENTINEL_INTRABAR_CUSUM_CRASH_ESCAPE"));

        // 2. Hard cap intrabar persistence: -45.5 bps sustained >= 2.5s OR -49.0 bps instant
        assert!(check_fast_sentinel_intrabar_escape(-45.5, 1000, 1.0, 3.08, -5.0, 0.0).is_none());
        assert_eq!(
            check_fast_sentinel_intrabar_escape(-45.5, 2600, 1.0, 3.08, -5.0, 0.0),
            Some("FAST_SENTINEL_INTRABAR_HARD_CAP_45BPS")
        );
        assert_eq!(
            check_fast_sentinel_intrabar_escape(-49.0, 0, 1.0, 3.08, -5.0, 0.0),
            Some("FAST_SENTINEL_INTRABAR_HARD_CAP_45BPS")
        );

        // 3. Normal reversible MAE (-39.9 bps, low CUSUM 1.8, no active 1m/BTC crash) is PRESERVED!
        assert!(check_fast_sentinel_intrabar_escape(-39.9, 0, 1.80, 3.08, -12.0, -8.0).is_none());
    }

    #[test]
    fn test_package6_live_capital_quarantine_gate() {
        std::env::remove_var("HL_LIVE_SYMBOLS");
        assert!(is_symbol_live_capital_enabled("BTC"));
        assert!(is_symbol_live_capital_enabled("ETH"));
        assert!(is_symbol_live_capital_enabled("SOL"));
        assert!(is_symbol_live_capital_enabled("AVAX"));
        assert!(!is_symbol_live_capital_enabled("HYPE"));
        assert!(!is_symbol_live_capital_enabled("XRP"));
        assert!(!is_symbol_live_capital_enabled("xyz:GOLD"));

        std::env::set_var("HL_LIVE_SYMBOLS", "BTC,ETH,SOL,AVAX,SUI");
        assert!(is_symbol_live_capital_enabled("SUI"));
        assert!(!is_symbol_live_capital_enabled("LINK"));

        std::env::set_var("HL_LIVE_SYMBOLS", "ALL");
        assert!(is_symbol_live_capital_enabled("xyz:NVDA"));
        std::env::remove_var("HL_LIVE_SYMBOLS");
    }
}
