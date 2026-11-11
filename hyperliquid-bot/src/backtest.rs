//! Discrete-Event Simulated Exchange & Historical Market Recorder for Hyperliquid L1.
//!
//! Incorporates the core matching invariants from NautilusTrader's `OrderMatchingCore`
//! and `OrderMatchingEngine` (`crates/execution/src/matching.rs` & `models/fill.rs`):
//! - Post-Only (`Alo`) limit validation against top-of-book (`best_bid` / `best_ask`),
//! - Adaptive High/Low intrabar path ordering (`bar_adaptive_high_low_ordering`),
//! - Trade-through vs. touch-only queue fill modeling (`prob_fill_on_limit`),
//! - Reduce-Only (`Gtc`) Maker Take-Profit and Exchange-Native (`StopMarket`) trigger execution,
//! - Exact linear-contract fee accounting (`1.5 bps` Maker, `4.5 bps` Taker + slippage) and
//!   hourly `:00` UTC funding settlement,
//! - Continuous, deduplicated NAS recording (`hl_history_{symbol}.jsonl`).

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::controller::{ControllerMode, RegimeController};
use crate::hmm::{compute_hmm_posteriors, train_hmm_offline, GaussianHMM};
use crate::hyperliquid::{effective_tick_size, optimal_engine_b_tp_atr_multiple, quantize_price};
use crate::ou_process::{
    asset_calibration, asset_ou_config, calculate_directional_edge_bps,
    calculate_features_with_funding_at, calculate_mad_sigma, check_macro_calendar_veto,
    compute_engine_a_disaster_stop_distance, compute_fee_adjusted_kelly_scalar,
    evaluate_armed_reversal_1m_step, evaluate_engine_c_hourly_squeeze,
    evaluate_engine_d_cascade_reversal, evaluate_session_lifecycle_gate,
    verify_maker_touch_absorption, AssetCalibration, AsymmetricExitManager, Direction, ExitSignal,
    HardenedAssetCalibration, HardenedExitDecision, MicroRegimeState, OneMinBarTick,
    OrderPlacementAction, SessionLifecycleAction, StallHazardExitManager,
    TwoTierInvalidationManager,
};
use crate::state::{ClusterCircuitBreaker, Side, DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC};
use crate::telemetry::{evaluate_btc_impulse_veto, BtcImpulseSnapshot};
use crate::trend::{
    calculate_atr, calculate_donchian, calculate_ker_n, calculate_min_squeeze_ratio_90m,
    calculate_rel_vol_15m, BreakoutTelemetry, CandleBar, ChandelierExitState, EngineBTrendManager,
    MarketCrowdingTracker, TrendEngineConfig, TrendOrderAction,
};

/// Single 1-minute historical market bar + L2 microstructure + derivatives context record
/// persisted continuously to `hl_history_{symbol}.jsonl` on the NAS (~1 MB/day across 4 coins).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalMarketBarRecord {
    pub symbol: String,
    pub timestamp_ms: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub best_bid: f64,
    pub best_ask: f64,
    pub obi_instant: f64,
    pub obi_1s: f64,
    pub obi_5s: f64,
    pub funding_1h: f64,
    pub open_interest: f64,
    pub mark_px: f64,
    pub oracle_px: f64,
}

/// Appends a completed 1-minute bar + L2/Funding/OI snapshot to `hl_history_{symbol}.jsonl`.
pub fn append_historical_bar_record(record: &HistoricalMarketBarRecord) {
    let file_path = format!("hl_history_{}.jsonl", record.symbol.to_lowercase());
    let lock_path = format!("{}.lock", file_path);
    let _guard = crate::state::FlockGuard::acquire(&lock_path);
    if let Ok(line) = serde_json::to_string(record) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
        {
            let _ = writeln!(file, "{}", line);
        }
    }
}

/// Loads and deduplicates all recorded bars from a given JSONL path, sorted by `timestamp_ms`.
pub fn load_bars_from_path(file_path: &str) -> Vec<HistoricalMarketBarRecord> {
    if !Path::new(file_path).exists() {
        return Vec::new();
    }
    let file = match File::open(file_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = BufReader::new(file);
    let mut bars: Vec<HistoricalMarketBarRecord> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<HistoricalMarketBarRecord>(&line) {
            if rec.close > 0.0 && rec.high >= rec.low && rec.low > 0.0 {
                bars.push(rec);
            }
        }
    }
    bars.sort_by_key(|b| b.timestamp_ms);
    bars.dedup_by_key(|b| b.timestamp_ms);
    bars
}

/// Loads and deduplicates all recorded bars from `hl_history_{symbol}.jsonl`, sorted by `timestamp_ms`.
pub fn load_historical_bars(symbol: &str) -> Vec<HistoricalMarketBarRecord> {
    let file_path = format!("hl_history_{}.jsonl", symbol.to_lowercase());
    load_bars_from_path(&file_path)
}

/// Configuration for the Nautilus-style Simulated Exchange Matching Engine.
#[derive(Debug, Clone)]
pub struct SimulatedExchangeConfig {
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub stop_slippage_bps: f64,
    /// Require price to trade strictly through the limit price by at least `trade_through_ticks` ticks (conservative queue model)
    pub strict_trade_through_fills: bool,
    /// Number of ticks price must trade through the limit price to fill when `strict_trade_through_fills` is true (default: 1.0)
    pub trade_through_ticks: f64,
    /// Use adaptive intrabar ordering (if close >= open, visit low before high; else visit high before low)
    pub bar_adaptive_high_low_ordering: bool,
    pub notional_usdc: f64,
    pub account_equity_usdc: f64,
    pub sz_decimals: u32,
}

impl Default for SimulatedExchangeConfig {
    fn default() -> Self {
        Self {
            maker_fee_bps: 1.44,
            taker_fee_bps: 4.32,
            stop_slippage_bps: 0.5,
            strict_trade_through_fills: false,
            trade_through_ticks: 0.0,
            bar_adaptive_high_low_ordering: true,
            notional_usdc: 175.0,
            account_equity_usdc: 536.0,
            sz_decimals: 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SimulatedTradeResult {
    pub symbol: String,
    pub side: Side,
    pub entry_time_ms: u64,
    pub exit_time_ms: u64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub qty: f64,
    pub gross_bps: f64,
    pub net_bps: f64,
    pub net_pnl_usd: f64,
    pub fees_usd: f64,
    pub funding_usd: f64,
    pub mfe_bps: f64,
    pub mae_bps: f64,
    pub exit_reason: &'static str,
    pub entry_policy: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct BacktestSummary {
    pub symbol: String,
    pub bars_evaluated: usize,
    pub signals_armed: usize,
    pub btc_impulse_vetoes: usize,
    pub macro_calendar_vetoes: usize,
    pub cluster_cb_vetoes: usize,
    pub orders_submitted: usize,
    pub orders_filled: usize,
    pub orders_expired_unfilled: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate_pct: f64,
    pub gross_pnl_bps: f64,
    pub net_pnl_bps: f64,
    pub net_pnl_usd: f64,
    pub total_fees_usd: f64,
    pub total_funding_usd: f64,
    pub profit_factor: f64,
    pub max_drawdown_usd: f64,
    pub trades: Vec<SimulatedTradeResult>,
}

/// Builds a causal `BtcImpulseSnapshot` at `ts_ms` from the historical BTC 1m bar series.
/// Uses binary search (`partition_point`) and enforces the same 90-second freshness window as live production.
pub fn build_historical_btc_impulse_snapshot(
    btc_bars: &[HistoricalMarketBarRecord],
    ts_ms: u64,
) -> Option<BtcImpulseSnapshot> {
    if btc_bars.is_empty() {
        return None;
    }
    let pos = btc_bars.partition_point(|b| b.timestamp_ms <= ts_ms);
    if pos == 0 {
        return None;
    }
    let idx = pos - 1;
    let b = &btc_bars[idx];
    if ts_ms.saturating_sub(b.timestamp_ms) > 90_000 || b.close <= 0.0 {
        return None;
    }
    let ret_3m_bps = if idx >= 3 && btc_bars[idx - 3].close > 0.0 {
        (b.close / btc_bars[idx - 3].close).ln() * 1e4
    } else {
        0.0
    };
    let ret_5m_bps = if idx >= 5 && btc_bars[idx - 5].close > 0.0 {
        (b.close / btc_bars[idx - 5].close).ln() * 1e4
    } else {
        0.0
    };
    let ret_15m_bps = if idx >= 15 && btc_bars[idx - 15].close > 0.0 {
        (b.close / btc_bars[idx - 15].close).ln() * 1e4
    } else {
        0.0
    };
    let ret_30m_bps = if idx >= 30 && btc_bars[idx - 30].close > 0.0 {
        (b.close / btc_bars[idx - 30].close).ln() * 1e4
    } else {
        0.0
    };

    let mut z_15 = 0.0;
    let mut z_60 = 0.0;
    if idx >= 65 {
        let start = idx.saturating_sub(180);
        let mut rets = Vec::with_capacity(idx - start);
        for k in (start + 1)..=idx {
            if btc_bars[k - 1].close > 0.0 && btc_bars[k].close > 0.0 {
                rets.push((btc_bars[k].close / btc_bars[k - 1].close).ln());
            }
        }
        let sigma_mad = calculate_mad_sigma(&rets);
        let r15 = (b.close / btc_bars[idx - 15].close).ln();
        let r60 = (b.close / btc_bars[idx - 60].close).ln();
        z_15 = r15 / (sigma_mad * (15.0_f64).sqrt());
        z_60 = r60 / (sigma_mad * (60.0_f64).sqrt());
    }

    Some(BtcImpulseSnapshot {
        timestamp_ms: b.timestamp_ms,
        ret_3m_bps,
        ret_5m_bps,
        ret_15m_bps,
        ret_30m_bps,
        z_15,
        z_60,
        is_severe_shock: z_15.abs() >= 2.5 || z_60.abs() >= 2.5,
        obi_instant: b.obi_instant,
        obi_1s: b.obi_1s,
        obi_5s: b.obi_5s,
    })
}

#[derive(Debug, Clone)]
struct RestingMakerEntry {
    side: Side,
    limit_px: f64,
    qty: f64,
    target_tp_px: f64,
    hard_stop_px: f64,
    frozen_center_px: f64,
    frozen_sigma_eq_log: f64,
    half_life_m: f64,
    submitted_bar_idx: usize,
    ttl_bars: usize,
    policy: &'static str,
    is_engine_b: bool,
    entry_atr: f64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct OpenSimulatedPosition {
    side: Side,
    entry_px: f64,
    qty: f64,
    initial_qty: f64,
    entry_time_ms: u64,
    initial_tp_px: f64,
    tp_limit_px: f64,
    hard_stop_px: f64,
    frozen_center_px: f64,
    frozen_sigma_eq_log: f64,
    entry_fee_usd: f64,
    accrued_funding_usd: f64,
    mfe_bps: f64,
    mae_bps: f64,
    max_progress: f64,
    candidate_lower_count: u32,
    reprice_count: u32,
    last_reprice_time_ms: u64,
    inv_mgr: TwoTierInvalidationManager,
    asym_mgr: AsymmetricExitManager,
    stall_mgr: StallHazardExitManager,
    tranche1_realized_pnl_usd: f64,
    tranche1_exit_fee_usd: f64,
    prev_residual_x: f64,
    policy: &'static str,
    is_engine_b: bool,
    chandelier_opt: Option<ChandelierExitState>,
    entry_atr: f64,
}

/// Runs a causal, discrete-event simulation of Engine A across a sequence of 1-minute `HistoricalMarketBarRecord`s
/// using Nautilus-style `OrderMatchingCore` rules (Alo post-only checks, FIFO trade-through fills,
/// mark/bar trigger stops, hourly funding settlement, and exact linear-contract fees).
pub fn run_engine_a_backtest(
    symbol: &str,
    bars: &[HistoricalMarketBarRecord],
    sim_cfg: &SimulatedExchangeConfig,
) -> BacktestSummary {
    let btc_ref = if symbol.eq_ignore_ascii_case("BTC") {
        Some(bars)
    } else {
        None
    };
    run_engine_a_backtest_with_btc(symbol, bars, btc_ref, sim_cfg)
}

/// Runs a causal, discrete-event simulation of Engine A with synchronized cross-asset BTC impulse veto
/// and inverse-volatility notional scaling.
pub fn run_engine_a_backtest_with_btc(
    symbol: &str,
    bars: &[HistoricalMarketBarRecord],
    btc_bars: Option<&[HistoricalMarketBarRecord]>,
    sim_cfg: &SimulatedExchangeConfig,
) -> BacktestSummary {
    let mut summary = BacktestSummary {
        symbol: symbol.to_uppercase(),
        ..Default::default()
    };
    if bars.len() < 750 {
        return summary;
    }

    let eff_btc_bars = if symbol.eq_ignore_ascii_case("BTC") {
        Some(bars)
    } else {
        btc_bars
    };

    let ou_cfg = asset_ou_config(symbol);
    let asset_cal = asset_calibration(symbol);
    let arbiter = UnifiedPortfolioArbiter {
        base_tranche_usdc: sim_cfg.notional_usdc,
    };
    let mut enricher = SyntheticMicrostructureEnricher::new();
    let warmup = 720usize.min(bars.len().saturating_sub(30));
    let mut closes_history: Vec<f64> = bars[..warmup].iter().map(|b| b.close).collect();

    // Pre-aggregate 5m/15m bars & warm up SyntheticMicrostructureEnricher from warmup history
    let mut rolling_5m_bars: Vec<CandleBar> = Vec::with_capacity(300);
    let mut current_5m_bar: Option<CandleBar> = None;
    let mut rolling_15m_bars: Vec<CandleBar> = Vec::with_capacity(100);
    let mut current_15m_bar: Option<CandleBar> = None;
    let mut all_15m_closes: Vec<f64> = Vec::with_capacity(1024);
    for b in &bars[..warmup] {
        let _ = enricher.enrich_next_bar(
            RawOhlcvBar {
                timestamp_ms: b.timestamp_ms,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
                volume: b.volume,
            },
            0.0,
        );
        let bucket_5m = (b.timestamp_ms / (5 * 60 * 1000)) * (5 * 60 * 1000);
        let is_last_1m_5m = b.timestamp_ms + 60_000 >= bucket_5m + 5 * 60 * 1000;
        if let Some(mut cur_5m) = current_5m_bar.take() {
            if cur_5m.time_open == bucket_5m {
                cur_5m.high = cur_5m.high.max(b.high);
                cur_5m.low = cur_5m.low.min(b.low);
                cur_5m.close = b.close;
                cur_5m.volume += b.volume;
                if is_last_1m_5m {
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
                let nb5 = CandleBar {
                    time_open: bucket_5m,
                    open: b.open,
                    high: b.high,
                    low: b.low,
                    close: b.close,
                    volume: b.volume,
                };
                if is_last_1m_5m {
                    rolling_5m_bars.push(nb5);
                    if rolling_5m_bars.len() > 300 {
                        rolling_5m_bars.remove(0);
                    }
                } else {
                    current_5m_bar = Some(nb5);
                }
            }
        } else {
            let nb5 = CandleBar {
                time_open: bucket_5m,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
                volume: b.volume,
            };
            if is_last_1m_5m {
                rolling_5m_bars.push(nb5);
                if rolling_5m_bars.len() > 300 {
                    rolling_5m_bars.remove(0);
                }
            } else {
                current_5m_bar = Some(nb5);
            }
        }

        let bucket_15m = (b.timestamp_ms / (15 * 60 * 1000)) * (15 * 60 * 1000);
        let is_last_1m = b.timestamp_ms + 60_000 >= bucket_15m + 15 * 60 * 1000;
        if let Some(mut cur) = current_15m_bar.take() {
            if cur.time_open == bucket_15m {
                cur.high = cur.high.max(b.high);
                cur.low = cur.low.min(b.low);
                cur.close = b.close;
                cur.volume += b.volume;
                if is_last_1m {
                    all_15m_closes.push(cur.close);
                    rolling_15m_bars.push(cur);
                    if rolling_15m_bars.len() > 100 {
                        rolling_15m_bars.remove(0);
                    }
                } else {
                    current_15m_bar = Some(cur);
                }
            } else {
                all_15m_closes.push(cur.close);
                rolling_15m_bars.push(cur);
                if rolling_15m_bars.len() > 100 {
                    rolling_15m_bars.remove(0);
                }
                let nb = CandleBar {
                    time_open: bucket_15m,
                    open: b.open,
                    high: b.high,
                    low: b.low,
                    close: b.close,
                    volume: b.volume,
                };
                if is_last_1m {
                    all_15m_closes.push(nb.close);
                    rolling_15m_bars.push(nb);
                    if rolling_15m_bars.len() > 100 {
                        rolling_15m_bars.remove(0);
                    }
                } else {
                    current_15m_bar = Some(nb);
                }
            }
        } else {
            let nb = CandleBar {
                time_open: bucket_15m,
                open: b.open,
                high: b.high,
                low: b.low,
                close: b.close,
                volume: b.volume,
            };
            if is_last_1m {
                all_15m_closes.push(nb.close);
                rolling_15m_bars.push(nb);
                if rolling_15m_bars.len() > 100 {
                    rolling_15m_bars.remove(0);
                }
            } else {
                current_15m_bar = Some(nb);
            }
        }
    }

    let hmm_path = format!("hl_hmm_{}.json", symbol.to_uppercase());
    let mut hmm_model =
        GaussianHMM::load_weights(&hmm_path).unwrap_or_else(|_| GaussianHMM::new(3, 4));
    let mut controller = RegimeController::default();
    let mut _current_p_c = 0.60;
    let mut current_p_t = 0.25;
    let mut current_p_s = 0.15;
    let mut crowding_tracker = MarketCrowdingTracker::default();
    let trend_cfg = TrendEngineConfig::for_symbol(symbol, 0.35, 0.28);
    let mut engine_b_mgr = EngineBTrendManager::new();
    let donchian_period = 12usize;
    let chandelier_atr_mult = 2.0_f64;

    // Armed state
    let mut is_armed = false;
    let mut arm_side = Side::Long;
    let mut arm_entry_px = 0.0;
    let mut arm_exit_px = 0.0;
    let mut arm_extreme_px = 0.0;
    let mut arm_bars_since_ext = 0usize;
    let mut arm_total_bars = 0usize;
    let mut arm_half_life = 45.0;
    let mut arm_sigma_eq_log = ou_cfg.min_sigma_eq_bps * 1e-4;

    // Exchange state (single active order/position per symbol)
    let mut resting_entry: Option<RestingMakerEntry> = None;
    let mut open_pos: Option<OpenSimulatedPosition> = None;
    let mut orb_engine = UsOpenOrbSubEngine::new();
    let mut last_engine_c_hour: u64 = u64::MAX;
    let mut last_squeeze_15m_bucket: u64 = u64::MAX;

    let mut cumulative_pnl_usd = 0.0;
    let mut peak_pnl_usd = 0.0;
    let mut gross_wins_usd = 0.0;
    let mut gross_losses_usd = 0.0;
    let mut cluster_cb = ClusterCircuitBreaker::default();

    let (max_drift_60, max_drift_120, max_slow_drift_180, max_slow_drift_240) =
        match symbol.to_uppercase().as_str() {
            "XYZ:GOLD" | "GOLD" => (0.0022, 0.0018, 0.0018, 0.0022),
            "XYZ:XYZ100" | "XYZ100" => (0.0028, 0.0022, 0.0022, 0.0026),
            "BTC" => (0.0035, 0.0025, 0.0025, 0.0030),
            "XYZ:SILVER" | "SILVER" => (0.0040, 0.0030, 0.0030, 0.0035),
            "ETH" | "XYZ:NVDA" | "NVDA" => (0.0045, 0.0030, 0.0030, 0.0035),
            "XRP" | "LINK" | "XYZ:TSLA" | "TSLA" => (0.0050, 0.0042, 0.0042, 0.0050),
            "SOL" | "HYPE" | "ZEC" => (0.0052, 0.0045, 0.0045, 0.0055),
            "AVAX" | "SUI" => (0.0065, 0.0050, 0.0060, 0.0075),
            "NEAR" => (0.0055, 0.0045, 0.0060, 0.0075),
            _ => (0.0045, 0.0030, 0.0030, 0.0035),
        };
    let (max_short_drift_60, max_short_drift_120, max_short_drift_240) =
        match symbol.to_uppercase().as_str() {
            "XYZ:GOLD" | "GOLD" => (0.0060, 0.0055, 0.0050),
            "XYZ:XYZ100" | "XYZ100" => (0.0070, 0.0065, 0.0058),
            "BTC" => (0.0085, 0.0075, 0.0065),
            "XYZ:SILVER" | "SILVER" => (0.0090, 0.0090, 0.0075),
            "ETH" | "XYZ:NVDA" | "NVDA" => (0.0095, 0.0100, 0.0080),
            "XRP" | "LINK" | "XYZ:TSLA" | "TSLA" => (0.0110, 0.0125, 0.0095),
            "SOL" | "HYPE" | "ZEC" => (0.0120, 0.0135, 0.0100),
            "AVAX" | "SUI" => (0.0135, 0.0160, 0.0140),
            "NEAR" => (0.0140, 0.0160, 0.0140),
            _ => (0.0095, 0.0100, 0.0080),
        };

    let retrain_cadence_15m = if bars.len() > 50_000 {
        672usize
    } else {
        96usize
    };
    let use_midrange_feat_cache = bars.len() > 50_000;
    let mut cached_feat_opt: Option<crate::ou_process::FeatureSnapshot> = None;
    let mut prev_obi_smooth: f64 = 0.0;
    let mut cached_min_squeeze_90m: f64 = 1.0;
    let mut cached_donchian_high_12: f64 = f64::MAX;
    let mut cached_donchian_low_12: f64 = 0.0;
    let mut cached_ema_15m: f64 = bars[warmup.saturating_sub(1)].close;
    let mut cached_atr_15m: f64 = 0.0;

    for idx in warmup..bars.len() {
        summary.bars_evaluated += 1;
        let bar = &bars[idx];
        let tick_sz = effective_tick_size(bar.close, sim_cfg.sz_decimals);
        let fill_pad = if sim_cfg.strict_trade_through_fills {
            tick_sz * sim_cfg.trade_through_ticks.max(0.0)
        } else {
            0.0
        };

        // Quick 15m Z-score estimate for causal SyntheticMicrostructureEnricher
        let z_15_quick =
            if closes_history.len() >= 15 && closes_history[closes_history.len() - 15] > 0.0 {
                (bar.close / closes_history[closes_history.len() - 15]).ln()
                    / (0.0010 * (15.0_f64).sqrt())
            } else {
                0.0
            };
        let enriched = enricher.enrich_next_bar(
            RawOhlcvBar {
                timestamp_ms: bar.timestamp_ms,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            },
            z_15_quick,
        );
        let has_native_l2 = bar.obi_instant.abs() > 1e-6 || bar.obi_1s.abs() > 1e-6;
        let eff_obi_inst = if has_native_l2 {
            bar.obi_instant
        } else {
            enriched.synthetic_obi_smoothed
        };
        let eff_obi_1s = if has_native_l2 {
            bar.obi_1s
        } else {
            enriched.synthetic_obi_smoothed
        };
        let eff_obi_5s = if has_native_l2 {
            bar.obi_5s
        } else {
            enriched.synthetic_obi_smoothed * 0.85
        };
        let eff_funding_1h = if has_native_l2 || (bar.funding_1h - 0.0000125).abs() > 1e-9 {
            bar.funding_1h
        } else {
            enriched.synthetic_funding_1h_bps * 1e-4
        };
        let eff_dlog_oi_3m =
            if idx >= 3 && bars[idx - 3].open_interest > 0.0 && bar.open_interest > 0.0 {
                (bar.open_interest / bars[idx - 3].open_interest).ln()
            } else {
                enriched.synthetic_oi_delta_3m_pct * 1e-2
            };

        // Update US Open ORB Sub-Engine on EVERY 1-minute bar so daily reset & 15m OR accumulation are never skipped
        let bell_minute_utc = crate::ou_process::us_equity_bell_minute_utc(bar.timestamp_ms);
        let median_15m_vol = (enricher.ema_vol_60 * 15.0).max(1.0);
        let mut enriched_for_orb = enriched;
        enriched_for_orb.synthetic_obi_smoothed = eff_obi_inst;
        let orb_action_this_bar = orb_engine.on_1m_bar(
            &enriched_for_orb,
            &asset_cal,
            bell_minute_utc,
            median_15m_vol,
        );

        // Update 5m & 15m bar aggregation & crowding tracker
        crowding_tracker.update_with_time(
            eff_funding_1h,
            bar.open_interest,
            bar.mark_px,
            bar.oracle_px,
            bar.timestamp_ms,
        );
        let candle_bucket_5m = (bar.timestamp_ms / (5 * 60 * 1000)) * (5 * 60 * 1000);
        let is_last_1m_of_5m = bar.timestamp_ms + 60_000 >= candle_bucket_5m + 5 * 60 * 1000;
        let mut finalized_5m_bar: Option<CandleBar> = None;
        if let Some(mut cur_5m) = current_5m_bar.take() {
            if cur_5m.time_open == candle_bucket_5m {
                cur_5m.high = cur_5m.high.max(bar.high);
                cur_5m.low = cur_5m.low.min(bar.low);
                cur_5m.close = bar.close;
                cur_5m.volume += bar.volume;
                if is_last_1m_of_5m {
                    rolling_5m_bars.push(cur_5m.clone());
                    if rolling_5m_bars.len() > 300 {
                        rolling_5m_bars.remove(0);
                    }
                    finalized_5m_bar = Some(cur_5m);
                } else {
                    current_5m_bar = Some(cur_5m);
                }
            } else {
                rolling_5m_bars.push(cur_5m.clone());
                if rolling_5m_bars.len() > 300 {
                    rolling_5m_bars.remove(0);
                }
                finalized_5m_bar = Some(cur_5m);
                let nb5 = CandleBar {
                    time_open: candle_bucket_5m,
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                };
                if is_last_1m_of_5m {
                    rolling_5m_bars.push(nb5.clone());
                    if rolling_5m_bars.len() > 300 {
                        rolling_5m_bars.remove(0);
                    }
                    finalized_5m_bar = Some(nb5);
                } else {
                    current_5m_bar = Some(nb5);
                }
            }
        } else {
            let nb5 = CandleBar {
                time_open: candle_bucket_5m,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            };
            if is_last_1m_of_5m {
                rolling_5m_bars.push(nb5.clone());
                if rolling_5m_bars.len() > 300 {
                    rolling_5m_bars.remove(0);
                }
                finalized_5m_bar = Some(nb5);
            } else {
                current_5m_bar = Some(nb5);
            }
        }

        let candle_bucket_15m = (bar.timestamp_ms / (15 * 60 * 1000)) * (15 * 60 * 1000);
        let is_last_1m_of_15m = bar.timestamp_ms + 60_000 >= candle_bucket_15m + 15 * 60 * 1000;
        let mut finalized_15m_bar: Option<CandleBar> = None;
        if let Some(mut cur_15m) = current_15m_bar.take() {
            if cur_15m.time_open == candle_bucket_15m {
                cur_15m.high = cur_15m.high.max(bar.high);
                cur_15m.low = cur_15m.low.min(bar.low);
                cur_15m.close = bar.close;
                cur_15m.volume += bar.volume;
                if is_last_1m_of_15m {
                    all_15m_closes.push(cur_15m.close);
                    rolling_15m_bars.push(cur_15m.clone());
                    if rolling_15m_bars.len() > 100 {
                        rolling_15m_bars.remove(0);
                    }
                    finalized_15m_bar = Some(cur_15m);
                } else {
                    current_15m_bar = Some(cur_15m);
                }
            } else {
                all_15m_closes.push(cur_15m.close);
                rolling_15m_bars.push(cur_15m.clone());
                if rolling_15m_bars.len() > 100 {
                    rolling_15m_bars.remove(0);
                }
                finalized_15m_bar = Some(cur_15m);
                let nb = CandleBar {
                    time_open: candle_bucket_15m,
                    open: bar.open,
                    high: bar.high,
                    low: bar.low,
                    close: bar.close,
                    volume: bar.volume,
                };
                if is_last_1m_of_15m {
                    all_15m_closes.push(nb.close);
                    rolling_15m_bars.push(nb.clone());
                    if rolling_15m_bars.len() > 100 {
                        rolling_15m_bars.remove(0);
                    }
                    finalized_15m_bar = Some(nb);
                } else {
                    current_15m_bar = Some(nb);
                }
            }
        } else {
            let nb = CandleBar {
                time_open: candle_bucket_15m,
                open: bar.open,
                high: bar.high,
                low: bar.low,
                close: bar.close,
                volume: bar.volume,
            };
            if is_last_1m_of_15m {
                all_15m_closes.push(nb.close);
                rolling_15m_bars.push(nb.clone());
                if rolling_15m_bars.len() > 100 {
                    rolling_15m_bars.remove(0);
                }
                finalized_15m_bar = Some(nb);
            } else {
                current_15m_bar = Some(nb);
            }
        }

        let session_action = evaluate_session_lifecycle_gate(symbol, bar.timestamp_ms as i64);
        let session_force_flat = matches!(session_action, SessionLifecycleAction::ForceFlat(_));
        let session_blocks_entries =
            !matches!(session_action, SessionLifecycleAction::AllowNormalTrading);
        if session_force_flat {
            is_armed = false;
        }

        // ------------------------------------------------------------------
        // 1. MATCHING ENGINE: Process Resting Post-Only (Alo) Entry Order
        // ------------------------------------------------------------------
        if let Some(entry_ord) = resting_entry.take() {
            let age_bars = idx.saturating_sub(entry_ord.submitted_bar_idx);
            let ord_dir = Direction::from_side(&entry_ord.side);
            let macro_abort = check_macro_calendar_veto(bar.timestamp_ms);
            let btc_abort = eff_btc_bars
                .and_then(|bb| build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms))
                .and_then(|snap| {
                    evaluate_btc_impulse_veto(
                        symbol,
                        entry_ord.side == Side::Long,
                        entry_ord.is_engine_b,
                        &snap,
                    )
                });
            let atr_1m_est = (cached_atr_15m / (15.0_f64).sqrt()).max(bar.close * 0.0008);
            let touch_absorbed = has_native_l2
                || verify_maker_touch_absorption(
                    ord_dir,
                    entry_ord.limit_px,
                    bar.high,
                    bar.low,
                    bar.close,
                    atr_1m_est,
                    prev_obi_smooth,
                    &asset_cal,
                );

            let filled = match entry_ord.side {
                Side::Long => bar.low <= entry_ord.limit_px - fill_pad,
                Side::Short => bar.high >= entry_ord.limit_px + fill_pad,
            };

            if session_force_flat {
                summary.orders_expired_unfilled += 1;
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_cancelled_or_rejected();
                }
            } else if macro_abort.is_some() {
                summary.macro_calendar_vetoes += 1;
                summary.orders_expired_unfilled += 1;
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_cancelled_or_rejected();
                }
            } else if btc_abort.is_some() {
                summary.btc_impulse_vetoes += 1;
                summary.orders_expired_unfilled += 1;
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_cancelled_or_rejected();
                }
            } else if filled && !touch_absorbed {
                summary.orders_expired_unfilled += 1;
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_cancelled_or_rejected();
                }
            } else if filled {
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_filled();
                }
                summary.orders_filled += 1;
                let entry_notional = entry_ord.qty * entry_ord.limit_px;
                let entry_fee = entry_notional * (sim_cfg.maker_fee_bps * 1e-4);
                let chandelier_opt = if entry_ord.is_engine_b {
                    Some(ChandelierExitState {
                        side: entry_ord.side.clone(),
                        entry_price: entry_ord.limit_px,
                        extreme_favorable_price: entry_ord.limit_px,
                        current_stop: entry_ord.hard_stop_px,
                        initial_stop: entry_ord.hard_stop_px,
                        base_multiplier: chandelier_atr_mult,
                        active_multiplier: chandelier_atr_mult,
                    })
                } else {
                    None
                };
                let entry_z =
                    if entry_ord.frozen_center_px > 0.0 && entry_ord.frozen_sigma_eq_log > 1e-6 {
                        (entry_ord.limit_px / entry_ord.frozen_center_px).ln()
                            / entry_ord.frozen_sigma_eq_log
                    } else {
                        -1.65 * Direction::from_side(&entry_ord.side).sign()
                    };
                let init_res_x = if entry_ord.frozen_center_px > 0.0 {
                    (entry_ord.limit_px / entry_ord.frozen_center_px).ln()
                } else {
                    0.0
                };
                let hurst_est = crate::ou_process::calculate_rolling_hurst_60m(&closes_history);
                let ker_est = crate::ou_process::calculate_ker_30m(&closes_history);
                let entry_regime = MicroRegimeState {
                    hurst_60m: hurst_est,
                    ker_30m: ker_est,
                    gk_efficiency_15m: 0.35,
                    kalman_level: entry_ord.frozen_center_px.max(1e-9).ln(),
                    kalman_slope: 0.0,
                    sigma_1m: 0.0010,
                    sigma_eq: entry_ord.frozen_sigma_eq_log,
                    z_macro: 0.0,
                };
                let kelly_alloc =
                    entry_regime.compute_optimal_tranche_split(&asset_cal, entry_z.abs());
                let mut asym_mgr = AsymmetricExitManager::new_with_alloc(
                    Direction::from_side(&entry_ord.side),
                    entry_ord.limit_px,
                    entry_z,
                    entry_ord.frozen_sigma_eq_log,
                    &asset_cal,
                    kelly_alloc,
                );
                asym_mgr.hard_disaster_stop_log_px = entry_ord.hard_stop_px.max(1e-9).ln();
                let eff_hard_stop_px = entry_ord.hard_stop_px;
                let init_sl_bps = ((entry_ord.limit_px - eff_hard_stop_px).abs()
                    / entry_ord.limit_px.max(1e-9))
                    * 10_000.0;
                let mut stall_mgr =
                    StallHazardExitManager::new(ord_dir, entry_ord.limit_px, init_sl_bps);
                stall_mgr.hard_disaster_stop_px = eff_hard_stop_px;
                open_pos = Some(OpenSimulatedPosition {
                    side: entry_ord.side.clone(),
                    entry_px: entry_ord.limit_px,
                    qty: entry_ord.qty,
                    initial_qty: entry_ord.qty,
                    entry_time_ms: bar.timestamp_ms,
                    initial_tp_px: entry_ord.target_tp_px,
                    tp_limit_px: entry_ord.target_tp_px,
                    hard_stop_px: eff_hard_stop_px,
                    frozen_center_px: entry_ord.frozen_center_px,
                    frozen_sigma_eq_log: entry_ord.frozen_sigma_eq_log,
                    entry_fee_usd: entry_fee,
                    accrued_funding_usd: 0.0,
                    mfe_bps: 0.0,
                    mae_bps: 0.0,
                    max_progress: 0.0,
                    candidate_lower_count: 0,
                    reprice_count: 0,
                    last_reprice_time_ms: bar.timestamp_ms,
                    inv_mgr: TwoTierInvalidationManager::new(
                        entry_ord.side.clone(),
                        entry_ord.limit_px,
                        ou_cfg.hard_disaster_stop_bps,
                        ou_cfg.soft_z_ou_threshold,
                        ou_cfg.soft_z15_shock_threshold,
                        entry_ord.half_life_m,
                    ),
                    asym_mgr,
                    stall_mgr,
                    tranche1_realized_pnl_usd: 0.0,
                    tranche1_exit_fee_usd: 0.0,
                    prev_residual_x: init_res_x,
                    policy: entry_ord.policy,
                    is_engine_b: entry_ord.is_engine_b,
                    chandelier_opt,
                    entry_atr: entry_ord.entry_atr,
                });
            } else if age_bars >= entry_ord.ttl_bars {
                summary.orders_expired_unfilled += 1;
                if entry_ord.is_engine_b {
                    engine_b_mgr.on_order_cancelled_or_rejected();
                }
            } else {
                resting_entry = Some(entry_ord);
            }
        }

        // ------------------------------------------------------------------
        // 2. MATCHING ENGINE: Process Open Position (Two-Tranche + SPRT + Handoff)
        // ------------------------------------------------------------------
        let near_band = cached_feat_opt
            .as_ref()
            .and_then(|f| f.thresholds_opt.as_ref())
            .map(|th| {
                bar.low <= th.entry_long * 1.0020
                    || bar.high >= th.entry_short * 0.9980
                    || bar.close <= th.entry_long
                    || bar.close >= th.entry_short
            })
            .unwrap_or(true);
        let need_fresh_feat = !use_midrange_feat_cache
            || open_pos.is_some()
            || is_armed
            || resting_entry.is_some()
            || finalized_15m_bar.is_some()
            || (bar.timestamp_ms / 60_000) % 5 == 4
            || z_15_quick.abs() >= 1.25
            || near_band
            || orb_action_this_bar != OrbAction::Inactive
            || ((bar.timestamp_ms / 60_000) % 60 >= 48
                && enriched.funding_z_24h >= asset_cal.funding_z24h_min);

        let live_feat_opt = if need_fresh_feat {
            let f = calculate_features_with_funding_at(
                &closes_history,
                bar.close,
                symbol,
                eff_funding_1h,
                Some(bar.timestamp_ms),
            );
            cached_feat_opt = f.clone();
            f
        } else {
            cached_feat_opt.clone()
        };

        // Update 3-Mode HMM & Regime Controller on 1m shocks and finalized 15m bar closes
        if let Some(ref feat) = live_feat_opt {
            let is_controller_shock_1m = feat.is_downside_shock
                || (feat.is_upside_shock && (current_p_s >= 0.50 || feat.z_15.abs() >= 3.5));
            if is_controller_shock_1m {
                controller.force_shock_cooldown(feat.z_15, feat.z_60, bar.timestamp_ms);
            }
            if let Some(ref closed_15m) = finalized_15m_bar {
                if all_15m_closes.len() >= 140 && all_15m_closes.len() % retrain_cadence_15m == 0 {
                    let window_start = all_15m_closes.len().saturating_sub(1000);
                    if let Some(new_hmm) = train_hmm_offline(&all_15m_closes[window_start..]) {
                        hmm_model = new_hmm;
                        controller.notify_model_hotswap();
                    }
                }
                let closes_15m: Vec<f64> = rolling_15m_bars.iter().map(|b| b.close).collect();
                let (p_c, p_t, p_s) = if closes_15m.len() >= 15 {
                    compute_hmm_posteriors(&hmm_model, &closes_15m).unwrap_or((0.60, 0.25, 0.15))
                } else {
                    (0.60, 0.25, 0.15)
                };
                _current_p_c = p_c;
                current_p_t = p_t;
                current_p_s = p_s;

                let prior_bars = if rolling_15m_bars.len() > 1 {
                    &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                } else {
                    &rolling_15m_bars[..]
                };
                cached_ema_15m = 0.15 * closed_15m.close + 0.85 * cached_ema_15m;
                if let Some(atr20) = calculate_atr(&rolling_15m_bars, 20) {
                    cached_atr_15m = atr20;
                }
                if let Some(d12) = calculate_donchian(prior_bars, 12) {
                    cached_donchian_high_12 = d12.upper;
                    cached_donchian_low_12 = d12.lower;
                }
                cached_min_squeeze_90m = calculate_min_squeeze_ratio_90m(&rolling_15m_bars);
                let has_donchian_breakout = calculate_donchian(prior_bars, donchian_period)
                    .map(|d| closed_15m.close > d.upper || closed_15m.close < d.lower)
                    .unwrap_or(false);
                let is_severe_shock_15m =
                    feat.is_downside_shock || (feat.is_upside_shock && (p_t < 0.82 || p_s >= 0.50));
                let _ = controller.evaluate_15m_bar_idempotent(
                    closed_15m.time_open + 15 * 60 * 1000,
                    bar.timestamp_ms,
                    p_c,
                    p_t,
                    feat.z_15,
                    feat.z_60,
                    has_donchian_breakout,
                    feat.is_stationary,
                    feat.is_valid_half_life,
                    is_severe_shock_15m,
                );
            }
        }

        if let Some(mut pos) = open_pos.take() {
            // Update MFE / MAE
            let (fav_px, adv_px) = match pos.side {
                Side::Long => (bar.high, bar.low),
                Side::Short => (bar.low, bar.high),
            };
            let fav_bps = match pos.side {
                Side::Long => ((fav_px - pos.entry_px) / pos.entry_px * 1e4).max(0.0),
                Side::Short => ((pos.entry_px - fav_px) / pos.entry_px * 1e4).max(0.0),
            };
            let adv_bps = match pos.side {
                Side::Long => ((pos.entry_px - adv_px) / pos.entry_px * 1e4).max(0.0),
                Side::Short => ((adv_px - pos.entry_px) / pos.entry_px * 1e4).max(0.0),
            };
            pos.mfe_bps = pos.mfe_bps.max(fav_bps);
            pos.mae_bps = pos.mae_bps.max(adv_bps);

            // Hourly funding settlement at :00 UTC boundary
            if (bar.timestamp_ms / 60_000) % 60 == 0 && eff_funding_1h.abs() > 1e-9 {
                let oracle_ref = if bar.oracle_px > 0.0 {
                    bar.oracle_px
                } else {
                    bar.close
                };
                let signed_carry = match pos.side {
                    Side::Long => -eff_funding_1h,
                    Side::Short => eff_funding_1h,
                };
                pos.accrued_funding_usd -= pos.qty * oracle_ref * signed_carry;
            }

            let tp_hit = match pos.side {
                Side::Long => bar.high >= pos.tp_limit_px + fill_pad,
                Side::Short => bar.low <= pos.tp_limit_px - fill_pad,
            };
            let hard_sl_hit = match pos.side {
                Side::Long => bar.low <= pos.hard_stop_px,
                Side::Short => bar.high >= pos.hard_stop_px,
            };

            let check_tp_first = if sim_cfg.bar_adaptive_high_low_ordering {
                match pos.side {
                    Side::Long => bar.close < bar.open,
                    Side::Short => bar.close > bar.open,
                }
            } else {
                true
            };

            let mut closed_outcome: Option<(f64, bool, &'static str)> = None;
            let mut sprt_handoff_candidate: Option<(f64, f64, f64)> = None;
            let elapsed_mins_1m =
                bar.timestamp_ms.saturating_sub(pos.entry_time_ms) as f64 / 60_000.0;

            if pos.is_engine_b {
                let current_atr = calculate_atr(&rolling_15m_bars, 20).unwrap_or(pos.entry_atr);
                let mut chandelier_hit = false;
                if let Some(ref mut chand) = pos.chandelier_opt {
                    let prev_stop = chand.current_stop;
                    let new_stop = chand.update_with_ratchet_and_cap(
                        bar.high,
                        bar.low,
                        current_atr,
                        current_p_t,
                        Some(0.5 / 15.0),
                        true,
                    );
                    pos.hard_stop_px = new_stop;
                    if chand.is_bar_stopped_out(prev_stop, bar.high, bar.low, bar.close) {
                        chandelier_hit = true;
                    }
                }

                let is_profit_locked_stop = match pos.side {
                    Side::Long => pos.hard_stop_px > pos.entry_px,
                    Side::Short => pos.hard_stop_px > 0.0 && pos.hard_stop_px < pos.entry_px,
                };
                let stop_label = if is_profit_locked_stop {
                    "CHANDELIER_PROFIT_LOCK"
                } else {
                    "CHANDELIER_STOP"
                };

                let t1_filled_b = pos.asym_mgr.tranche1_filled;
                if tp_hit && !t1_filled_b && (!chandelier_hit || check_tp_first) {
                    if pos.initial_qty * pos.entry_px >= 28.0 {
                        let t1_qty = 0.5 * pos.initial_qty;
                        let t1_exit_px = pos.tp_limit_px;
                        let t1_raw_pnl = match pos.side {
                            Side::Long => t1_qty * (t1_exit_px - pos.entry_px),
                            Side::Short => t1_qty * (pos.entry_px - t1_exit_px),
                        };
                        let t1_fee = (t1_qty * t1_exit_px) * (sim_cfg.maker_fee_bps * 1e-4);
                        pos.tranche1_realized_pnl_usd = t1_raw_pnl;
                        pos.tranche1_exit_fee_usd = t1_fee;
                        pos.qty = pos.initial_qty - t1_qty;
                        pos.asym_mgr.tranche1_filled = true;
                        let be_frac = (sim_cfg.maker_fee_bps + sim_cfg.taker_fee_bps + 3.0) * 1e-4;
                        let be_px = quantize_price(
                            match pos.side {
                                Side::Long => pos.entry_px * (1.0 + be_frac),
                                Side::Short => pos.entry_px * (1.0 - be_frac),
                            },
                            sim_cfg.sz_decimals,
                        );
                        match pos.side {
                            Side::Long => {
                                pos.hard_stop_px = pos.hard_stop_px.max(be_px);
                                if let Some(ref mut chand) = pos.chandelier_opt {
                                    chand.current_stop = chand.current_stop.max(be_px);
                                }
                            }
                            Side::Short => {
                                pos.hard_stop_px = if pos.hard_stop_px > 0.0 {
                                    pos.hard_stop_px.min(be_px)
                                } else {
                                    be_px
                                };
                                if let Some(ref mut chand) = pos.chandelier_opt {
                                    chand.current_stop = if chand.current_stop > 0.0 {
                                        chand.current_stop.min(be_px)
                                    } else {
                                        be_px
                                    };
                                }
                            }
                        }
                    } else {
                        closed_outcome = Some((pos.tp_limit_px, true, "MAKER_TP"));
                    }
                } else if chandelier_hit {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((pos.hard_stop_px * slip, false, stop_label));
                    controller.notify_trend_stopped_out();
                }

                if closed_outcome.is_none() && !pos.asym_mgr.tranche1_filled {
                    let d_0 = match pos.side {
                        Side::Long => pos.initial_tp_px - pos.entry_px,
                        Side::Short => pos.entry_px - pos.initial_tp_px,
                    };
                    if d_0 > 0.0 {
                        let q_t = match pos.side {
                            Side::Long => (bar.close - pos.entry_px) / d_0,
                            Side::Short => (pos.entry_px - bar.close) / d_0,
                        };
                        if q_t > pos.max_progress {
                            pos.max_progress = q_t;
                        }
                        if (pos.max_progress >= 0.75 && q_t <= (pos.max_progress - 0.20).max(0.15))
                            || (elapsed_mins_1m >= 45.0
                                && pos.max_progress >= 0.30
                                && q_t <= (pos.max_progress - 0.15).max(0.08))
                        {
                            let slip = match pos.side {
                                Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                            };
                            closed_outcome = Some((bar.close * slip, false, "GIVEBACK_RATCHET"));
                        } else if !has_native_l2 && elapsed_mins_1m >= 2.0 {
                            let net_b_bps = if elapsed_mins_1m >= 20.0 {
                                4.5
                            } else if elapsed_mins_1m >= 9.0 {
                                7.5
                            } else {
                                10.5
                            };
                            let min_b_margin = (2.0 * sim_cfg.maker_fee_bps + net_b_bps) * 1e-4;
                            let be_b_px = match pos.side {
                                Side::Long => {
                                    let raw = (pos.entry_px * (1.0 + min_b_margin))
                                        .max(bar.close + tick_sz);
                                    (raw / tick_sz.max(1e-9)).ceil() * tick_sz.max(1e-9)
                                }
                                Side::Short => {
                                    let raw = (pos.entry_px * (1.0 - min_b_margin))
                                        .min(bar.close - tick_sz);
                                    (raw / tick_sz.max(1e-9)).floor() * tick_sz.max(1e-9)
                                }
                            };
                            match pos.side {
                                Side::Long if be_b_px < pos.tp_limit_px && be_b_px > bar.close => {
                                    pos.tp_limit_px = be_b_px;
                                }
                                Side::Short if be_b_px > pos.tp_limit_px && be_b_px < bar.close => {
                                    pos.tp_limit_px = be_b_px;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                let fav_move_b = match pos.side {
                    Side::Long => bar.close - pos.entry_px,
                    Side::Short => pos.entry_px - bar.close,
                };
                let unreal_atr_b = if current_atr > 0.0 {
                    fav_move_b / current_atr
                } else {
                    0.0
                };
                let runner_active_b =
                    pos.asym_mgr.tranche1_filled || (unreal_atr_b >= 0.75 && current_p_t >= 0.60);
                let max_b_mins = if runner_active_b { 360.0 } else { 75.0 };
                if closed_outcome.is_none() && elapsed_mins_1m >= max_b_mins {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((bar.close * slip, false, "TIME_DECAY_STOP"));
                }
            } else {
                // =========================================================================
                // ENGINE A: KELLY TWO-TRANCHE EXIT + 1M WICK-FILTERED SPRT + RATCHET
                // =========================================================================
                let d_sign = Direction::from_side(&pos.side).sign();
                let hurst_now = live_feat_opt.as_ref().map(|f| f.hurst_60m).unwrap_or(0.42);
                let ker_now = live_feat_opt.as_ref().map(|f| f.ker_30m).unwrap_or(0.25);
                let micro_now = MicroRegimeState {
                    hurst_60m: hurst_now,
                    ker_30m: ker_now,
                    gk_efficiency_15m: 0.35,
                    kalman_level: pos.frozen_center_px.max(1e-9).ln(),
                    kalman_slope: 0.0,
                    sigma_1m: 0.0010,
                    sigma_eq: pos.frozen_sigma_eq_log,
                    z_macro: 0.0,
                };
                let alloc =
                    micro_now.compute_optimal_tranche_split(&asset_cal, pos.asym_mgr.entry_z.abs());
                let beta_ov = alloc.beta_overshoot;
                let tp2_overshoot_px = quantize_price(
                    pos.frozen_center_px * (d_sign * beta_ov * pos.frozen_sigma_eq_log).exp(),
                    sim_cfg.sz_decimals,
                );
                let w1_frac = pos.asym_mgr.w1_fraction;
                let w2_frac = pos.asym_mgr.w2_fraction;

                if !has_native_l2 && elapsed_mins_1m >= 1.0 {
                    let prev_close = *closes_history.last().unwrap_or(&bar.open);
                    let sigma_1m_est = live_feat_opt
                        .as_ref()
                        .map(|f| f.sigma_1m_prev)
                        .unwrap_or(0.0010)
                        .max(1e-5);
                    let kalman_slope_1m = live_feat_opt
                        .as_ref()
                        .map(|f| f.kalman_slope / 5.0)
                        .unwrap_or(0.0);
                    let std_innov_1m =
                        ((bar.close / prev_close.max(1e-9)).ln() - kalman_slope_1m) / sigma_1m_est;
                    let sigma_eq_bps = (pos.frozen_sigma_eq_log * 10_000.0).max(15.0);
                    let stall_dec = pos.stall_mgr.on_1m_bar(
                        bar.open,
                        bar.high,
                        bar.low,
                        bar.close,
                        prev_close,
                        pos.frozen_center_px,
                        sigma_eq_bps,
                        std_innov_1m,
                        eff_obi_inst,
                        enriched.effective_spread_bps,
                        &asset_cal,
                    );
                    match stall_dec {
                        HardenedExitDecision::Hold {
                            tp1_maker_px,
                            tp2_maker_px,
                            hard_stop_px,
                        } => {
                            pos.hard_stop_px = quantize_price(hard_stop_px, sim_cfg.sz_decimals);
                            pos.tp_limit_px = quantize_price(
                                tp1_maker_px.unwrap_or(tp2_maker_px),
                                sim_cfg.sz_decimals,
                            );
                        }
                        HardenedExitDecision::FillTranche1Maker {
                            fill_px,
                            new_tp2_maker_px,
                        } => {
                            let t1_qty = w1_frac * pos.initial_qty;
                            let t1_exit_px = quantize_price(fill_px, sim_cfg.sz_decimals);
                            let t1_raw_pnl = match pos.side {
                                Side::Long => t1_qty * (t1_exit_px - pos.entry_px),
                                Side::Short => t1_qty * (pos.entry_px - t1_exit_px),
                            };
                            let t1_fee = (t1_qty * t1_exit_px) * (sim_cfg.maker_fee_bps * 1e-4);
                            pos.tranche1_realized_pnl_usd = t1_raw_pnl;
                            pos.tranche1_exit_fee_usd = t1_fee;
                            pos.qty = w2_frac * pos.initial_qty;
                            pos.asym_mgr.tranche1_filled = true;
                            pos.tp_limit_px = quantize_price(new_tp2_maker_px, sim_cfg.sz_decimals);
                            let lock_offset = (0.30 * pos.frozen_sigma_eq_log).max(0.0012);
                            let lock_px = quantize_price(
                                pos.entry_px * (d_sign * lock_offset).exp(),
                                sim_cfg.sz_decimals,
                            );
                            pos.hard_stop_px = lock_px;
                            pos.stall_mgr.hard_disaster_stop_px = lock_px;
                        }
                        HardenedExitDecision::FillTranche2Maker {
                            fill_px,
                            is_pegged_profit_lock,
                        } => {
                            pos.asym_mgr.tranche2_filled = true;
                            let exit_px = quantize_price(fill_px, sim_cfg.sz_decimals);
                            let reason = if is_pegged_profit_lock {
                                "PARTIAL_T1_MAKER_T2_TRAIL"
                            } else {
                                "FULL_2TRANCHE_MAKER_TP"
                            };
                            closed_outcome = Some((exit_px, true, reason));
                        }
                        HardenedExitDecision::StopOutSprt1mClose { exit_px, cusum_val } => {
                            let slip = match pos.side {
                                Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                            };
                            closed_outcome = Some((exit_px * slip, false, "SPRT_REGIME_STOP"));
                            sprt_handoff_candidate = Some((cusum_val, 0.0, 2.0));
                        }
                        HardenedExitDecision::StopOutIntrabarDisaster { exit_px } => {
                            let slip = match pos.side {
                                Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                            };
                            let sl_reason = if d_sign * (exit_px - pos.entry_px) > 0.0 {
                                "PARTIAL_T1_MAKER_T2_TRAIL"
                            } else {
                                "DISASTER_STOP"
                            };
                            closed_outcome = Some((exit_px * slip, false, sl_reason));
                        }
                    }
                } else if !pos.asym_mgr.tranche1_filled {
                    // Tranche 1 (w1_frac) rests at Kalman mean (pos.tp_limit_px), or 100% Maker Harvest if ratcheted
                    if tp_hit && (!hard_sl_hit || check_tp_first) {
                        let is_harvest_ratcheted =
                            (pos.tp_limit_px - pos.initial_tp_px).abs() > 1e-9;
                        if is_harvest_ratcheted {
                            pos.asym_mgr.tranche1_filled = true;
                            pos.asym_mgr.tranche2_filled = true;
                            closed_outcome =
                                Some((pos.tp_limit_px, true, "FULL_2TRANCHE_MAKER_TP"));
                        } else {
                            // Tranche 1 fills as a passive Maker at equilibrium mean!
                            let t1_qty = w1_frac * pos.initial_qty;
                            let t1_exit_px = pos.tp_limit_px;
                            let t1_raw_pnl = match pos.side {
                                Side::Long => t1_qty * (t1_exit_px - pos.entry_px),
                                Side::Short => t1_qty * (pos.entry_px - t1_exit_px),
                            };
                            let t1_fee = (t1_qty * t1_exit_px) * (sim_cfg.maker_fee_bps * 1e-4);
                            pos.tranche1_realized_pnl_usd = t1_raw_pnl;
                            pos.tranche1_exit_fee_usd = t1_fee;
                            pos.qty = w2_frac * pos.initial_qty;
                            pos.asym_mgr.tranche1_filled = true;
                            pos.asym_mgr.extreme_favorable_log_px = fav_px.max(1e-9).ln();

                            // Immediately jump Stop-Loss for Tranche 2 to Fee-Plus-Profit Lock-In Floor
                            let lock_offset = (0.30 * pos.frozen_sigma_eq_log).max(0.0012);
                            let lock_px = quantize_price(
                                pos.entry_px * (d_sign * lock_offset).exp(),
                                sim_cfg.sz_decimals,
                            );
                            pos.hard_stop_px = lock_px;
                            pos.asym_mgr.trailing_stop_log_px = lock_px.max(1e-9).ln();
                            pos.tp_limit_px = tp2_overshoot_px;

                            // Check if the same bar ALSO reached Tranche 2 Overshoot Target!
                            let tp2_also_hit = match pos.side {
                                Side::Long => bar.high >= tp2_overshoot_px + fill_pad,
                                Side::Short => bar.low <= tp2_overshoot_px - fill_pad,
                            };
                            if tp2_also_hit {
                                pos.asym_mgr.tranche2_filled = true;
                                closed_outcome =
                                    Some((tp2_overshoot_px, true, "FULL_2TRANCHE_MAKER_TP"));
                            }
                        }
                    } else if hard_sl_hit {
                        let slip = match pos.side {
                            Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                            Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                        };
                        let sl_reason = if d_sign * (pos.hard_stop_px - pos.entry_px) > 0.0 {
                            "GIVEBACK_RATCHET"
                        } else {
                            "DISASTER_STOP"
                        };
                        let real_sl_px = pos.hard_stop_px.clamp(bar.low, bar.high);
                        closed_outcome = Some((real_sl_px * slip, false, sl_reason));
                    } else if let Some(ref feat) = live_feat_opt {
                        // 1. Check 75% MFE Giveback Ratchet prior to Tranche 1 fill so near-mean stalls lock profit
                        let d_0 = match pos.side {
                            Side::Long => pos.initial_tp_px - pos.entry_px,
                            Side::Short => pos.entry_px - pos.initial_tp_px,
                        };
                        if d_0 > 0.0 {
                            let q_t = match pos.side {
                                Side::Long => (bar.close - pos.entry_px) / d_0,
                                Side::Short => (pos.entry_px - bar.close) / d_0,
                            };
                            if q_t > pos.max_progress {
                                pos.max_progress = q_t;
                            }
                            let abs_arm_bps = 1.6 * ou_cfg.min_net_edge_bps;
                            let arm_q_thresh =
                                (abs_arm_bps / (d_0 / pos.entry_px * 1e4).max(1e-6)).min(0.75);
                            let close_fav_bps =
                                d_sign * (bar.close - pos.entry_px) / pos.entry_px * 1e4;
                            if pos.max_progress >= arm_q_thresh
                                && q_t <= (pos.max_progress - 0.20).max(0.15)
                                && close_fav_bps >= 12.0
                            {
                                let slip = match pos.side {
                                    Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                    Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                                };
                                closed_outcome =
                                    Some((bar.close * slip, false, "GIVEBACK_RATCHET"));
                            } else if !has_native_l2
                                && elapsed_mins_1m
                                    >= if symbol.eq_ignore_ascii_case("BTC") {
                                        7.0
                                    } else {
                                        10.0
                                    }
                                && pos.mfe_bps < 5.5
                            {
                                let sl_tight_frac = if symbol.eq_ignore_ascii_case("BTC") {
                                    0.0015
                                } else {
                                    0.0018
                                };
                                let tight_sl = quantize_price(
                                    pos.entry_px * (1.0 - d_sign * sl_tight_frac),
                                    sim_cfg.sz_decimals,
                                );
                                if d_sign * (tight_sl - pos.hard_stop_px) > 0.0
                                    && d_sign * (bar.close - tight_sl) >= 0.0007 * bar.close
                                {
                                    pos.hard_stop_px = tight_sl;
                                    pos.asym_mgr.hard_disaster_stop_log_px =
                                        tight_sl.max(1e-9).ln();
                                }
                            }
                        }

                        // 2. Time-Decay Passive Maker Harvest Ratchet (100% Post-Only Alo Maker)
                        let harvest_start_mins = if has_native_l2 {
                            (1.15 * pos.inv_mgr.half_life_minutes).clamp(38.0, 90.0)
                        } else if symbol.eq_ignore_ascii_case("BTC") {
                            1.0
                        } else {
                            2.0
                        };
                        if closed_outcome.is_none()
                            && ((!has_native_l2 && elapsed_mins_1m >= harvest_start_mins)
                                || (bar.timestamp_ms / 60_000) % 5 == 4)
                        {
                            if (elapsed_mins_1m - 5.0).max(0.0) >= harvest_start_mins
                                || (!has_native_l2 && elapsed_mins_1m >= harvest_start_mins)
                            {
                                let net_harvest_bps = if has_native_l2 {
                                    12.0
                                } else if symbol.eq_ignore_ascii_case("BTC") {
                                    if elapsed_mins_1m >= 18.0 {
                                        3.4
                                    } else if elapsed_mins_1m >= 10.0 {
                                        5.8
                                    } else if elapsed_mins_1m >= 4.0 {
                                        8.8
                                    } else {
                                        11.8
                                    }
                                } else if elapsed_mins_1m >= 22.0 {
                                    3.5
                                } else if elapsed_mins_1m >= 13.0 {
                                    6.5
                                } else if elapsed_mins_1m >= 6.0 {
                                    10.5
                                } else {
                                    15.5
                                };
                                let min_harvest_margin =
                                    (2.0 * sim_cfg.maker_fee_bps + net_harvest_bps) * 1e-4;
                                let be_px = match pos.side {
                                    Side::Long => {
                                        let raw = (pos.entry_px * (1.0 + min_harvest_margin))
                                            .max(bar.close + tick_sz);
                                        (raw / tick_sz.max(1e-9)).ceil() * tick_sz.max(1e-9)
                                    }
                                    Side::Short => {
                                        let raw = (pos.entry_px * (1.0 - min_harvest_margin))
                                            .min(bar.close - tick_sz);
                                        (raw / tick_sz.max(1e-9)).floor() * tick_sz.max(1e-9)
                                    }
                                };
                                match pos.side {
                                    Side::Long if be_px < pos.tp_limit_px && be_px > bar.close => {
                                        pos.tp_limit_px = be_px;
                                    }
                                    Side::Short if be_px > pos.tp_limit_px && be_px < bar.close => {
                                        pos.tp_limit_px = be_px;
                                    }
                                    _ => {}
                                }
                            }
                        }

                        // 3. Evaluate Native 1-Minute Wick-Filtered SPRT Stop (Section 2.2 & 6.1)
                        if closed_outcome.is_none()
                            && (has_native_l2 || (bar.timestamp_ms / 60_000) % 5 == 4)
                        {
                            let curr_res_x = (bar.close / pos.frozen_center_px.max(1e-9)).ln();
                            let theta_step = feat.ou_theta.max(0.01);
                            let exp_th = (-theta_step).exp();
                            let denom_ou = (pos.frozen_sigma_eq_log
                                * (1.0 - (-2.0 * theta_step).exp()).max(1e-6).sqrt())
                            .max(1e-6);
                            let ou_step_innov =
                                (curr_res_x - pos.prev_residual_x * exp_th) / denom_ou;
                            pos.prev_residual_x = curr_res_x;

                            let micro_state = MicroRegimeState {
                                hurst_60m: feat.hurst_60m,
                                ker_30m: feat.ker_30m,
                                gk_efficiency_15m: 0.35,
                                kalman_level: pos.frozen_center_px.max(1e-9).ln(),
                                kalman_slope: feat.kalman_slope / 5.0,
                                sigma_1m: feat.sigma_1m_prev,
                                sigma_eq: pos.frozen_sigma_eq_log,
                                z_macro: feat.z_macro,
                            };
                            let sprt_3m_ago = pos.asym_mgr.sprt_stat_3m_ago();
                            let bar_tick = OneMinBarTick {
                                open: bar.open,
                                high: bar.high,
                                low: bar.low,
                                close: bar.close,
                                std_kalman_innov_1m: feat.kalman_std_innov / (5.0_f64).sqrt(),
                                ou_step_innov_1m: ou_step_innov,
                                smoothed_obi: eff_obi_inst,
                            };
                            let sig = pos.asym_mgr.on_1m_bar(
                                &bar_tick,
                                &micro_state,
                                &asset_cal,
                                pos.inv_mgr.half_life_minutes,
                            );
                            let adv_close_bps = match pos.side {
                                Side::Long => {
                                    ((pos.entry_px - bar.close) / pos.entry_px * 1e4).max(0.0)
                                }
                                Side::Short => {
                                    ((bar.close - pos.entry_px) / pos.entry_px * 1e4).max(0.0)
                                }
                            };
                            match sig {
                                ExitSignal::StopOutIntrabarDisaster { exit_price } => {
                                    let slip = match pos.side {
                                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                                    };
                                    closed_outcome =
                                        Some((exit_price * slip, false, "DISASTER_STOP"));
                                }
                                ExitSignal::StopOutFullSprtBreak {
                                    sprt_stat,
                                    avg_adverse_innov_3m,
                                } if adv_close_bps >= 28.0 => {
                                    let slip = match pos.side {
                                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                                    };
                                    let sprt_exit_px = match pos.side {
                                        Side::Long => bar.close.max(pos.hard_stop_px) * slip,
                                        Side::Short => bar.close.min(pos.hard_stop_px) * slip,
                                    };
                                    closed_outcome =
                                        Some((sprt_exit_px, false, "SPRT_REGIME_STOP"));
                                    sprt_handoff_candidate =
                                        Some((sprt_stat, sprt_3m_ago, avg_adverse_innov_3m));
                                }
                                ExitSignal::FillTranche1AndRatchet {
                                    tp2_price,
                                    new_lock_sl_price,
                                } => {
                                    let t1_qty = w1_frac * pos.initial_qty;
                                    let t1_exit_px = pos.tp_limit_px;
                                    let t1_raw_pnl = match pos.side {
                                        Side::Long => t1_qty * (t1_exit_px - pos.entry_px),
                                        Side::Short => t1_qty * (pos.entry_px - t1_exit_px),
                                    };
                                    let t1_fee =
                                        (t1_qty * t1_exit_px) * (sim_cfg.maker_fee_bps * 1e-4);
                                    pos.tranche1_realized_pnl_usd = t1_raw_pnl;
                                    pos.tranche1_exit_fee_usd = t1_fee;
                                    pos.qty = w2_frac * pos.initial_qty;
                                    pos.hard_stop_px =
                                        quantize_price(new_lock_sl_price, sim_cfg.sz_decimals);
                                    pos.tp_limit_px =
                                        quantize_price(tp2_price, sim_cfg.sz_decimals);
                                }
                                _ => {}
                            }
                        }
                    }
                } else {
                    // Tranche 1 is already filled! Manage Tranche 2 Overshoot Runner & Micro-Chandelier Ratchet
                    let fav_log = fav_px.max(1e-9).ln();
                    if d_sign * (fav_log - pos.asym_mgr.extreme_favorable_log_px) > 0.0 {
                        pos.asym_mgr.extreme_favorable_log_px = fav_log;
                    }
                    let lock_offset = (0.30 * pos.frozen_sigma_eq_log).max(0.0012);
                    let lock_log_px = pos.asym_mgr.entry_log_px + d_sign * lock_offset;
                    if d_sign * (pos.asym_mgr.extreme_favorable_log_px - lock_log_px)
                        > 0.45 * pos.frozen_sigma_eq_log
                    {
                        let cand_trail_log = pos.asym_mgr.extreme_favorable_log_px
                            - d_sign * (0.30 * pos.frozen_sigma_eq_log);
                        if d_sign * (cand_trail_log - pos.asym_mgr.trailing_stop_log_px) > 0.0 {
                            pos.asym_mgr.trailing_stop_log_px = cand_trail_log;
                            pos.hard_stop_px =
                                quantize_price(cand_trail_log.exp(), sim_cfg.sz_decimals);
                        }
                    }

                    let tp2_hit = match pos.side {
                        Side::Long => bar.high >= pos.tp_limit_px + fill_pad,
                        Side::Short => bar.low <= pos.tp_limit_px - fill_pad,
                    };
                    // Evaluate Tranche 2 trailing stop on 1-minute bar.close (or wide disaster stop on intrabar wick)
                    let disaster_px = pos.asym_mgr.hard_disaster_stop_log_px.exp();
                    let trail_sl_hit = match pos.side {
                        Side::Long => bar.close <= pos.hard_stop_px || bar.low <= disaster_px,
                        Side::Short => bar.close >= pos.hard_stop_px || bar.high >= disaster_px,
                    };

                    if tp2_hit && (!trail_sl_hit || check_tp_first) {
                        pos.asym_mgr.tranche2_filled = true;
                        closed_outcome = Some((pos.tp_limit_px, true, "FULL_2TRANCHE_MAKER_TP"));
                    } else if trail_sl_hit {
                        let slip = match pos.side {
                            Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                            Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                        };
                        closed_outcome =
                            Some((pos.hard_stop_px * slip, false, "PARTIAL_T1_MAKER_T2_TRAIL"));
                    }
                }

                // Evaluate 1m Time-Decay Max Hold Stop or Scheduled Session Flat-at-Close
                let max_hold_1m = (3.5 * pos.inv_mgr.half_life_minutes).clamp(90.0, 360.0);
                if closed_outcome.is_none() && session_force_flat {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((bar.close * slip, false, "SESSION_CLOSE_FLAT"));
                } else if closed_outcome.is_none() && elapsed_mins_1m >= max_hold_1m {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    let reason = if pos.asym_mgr.tranche1_filled {
                        "PARTIAL_T1_MAKER_T2_TIMEOUT"
                    } else {
                        "TIME_DECAY_STOP"
                    };
                    closed_outcome = Some((bar.close * slip, false, reason));
                }
            }

            if let Some((exit_px, is_maker_exit, reason)) = closed_outcome {
                let exit_notional = pos.qty * exit_px;
                let exit_fee_bps = if is_maker_exit {
                    sim_cfg.maker_fee_bps
                } else {
                    sim_cfg.taker_fee_bps
                };
                let exit_fee_usd = exit_notional * (exit_fee_bps * 1e-4);
                let total_fees_usd = pos.entry_fee_usd + pos.tranche1_exit_fee_usd + exit_fee_usd;
                let rem_raw_pnl_usd = match pos.side {
                    Side::Long => pos.qty * (exit_px - pos.entry_px),
                    Side::Short => pos.qty * (pos.entry_px - exit_px),
                };
                let raw_pnl_usd = pos.tranche1_realized_pnl_usd + rem_raw_pnl_usd;
                let net_pnl_usd = raw_pnl_usd - total_fees_usd - pos.accrued_funding_usd;
                let entry_notional = (pos.initial_qty * pos.entry_px).max(1e-9);
                let gross_bps = (raw_pnl_usd / entry_notional) * 1e4;
                let net_bps = (net_pnl_usd / entry_notional) * 1e4;

                if net_pnl_usd >= 0.0 {
                    summary.wins += 1;
                    gross_wins_usd += net_pnl_usd;
                } else {
                    summary.losses += 1;
                    gross_losses_usd += net_pnl_usd.abs();
                }
                summary.gross_pnl_bps += gross_bps;
                summary.net_pnl_bps += net_bps;
                summary.net_pnl_usd += net_pnl_usd;
                summary.total_fees_usd += total_fees_usd;
                summary.total_funding_usd += pos.accrued_funding_usd;

                cumulative_pnl_usd += net_pnl_usd;
                if cumulative_pnl_usd > peak_pnl_usd {
                    peak_pnl_usd = cumulative_pnl_usd;
                }
                let dd = peak_pnl_usd - cumulative_pnl_usd;
                if dd > summary.max_drawdown_usd {
                    summary.max_drawdown_usd = dd;
                }
                let _ = cluster_cb.record_cluster_trade(
                    bar.timestamp_ms,
                    net_pnl_usd,
                    sim_cfg.account_equity_usdc + cumulative_pnl_usd,
                    DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC,
                );

                let stopped_side = pos.side.clone();
                summary.trades.push(SimulatedTradeResult {
                    symbol: symbol.to_uppercase(),
                    side: pos.side,
                    entry_time_ms: pos.entry_time_ms,
                    exit_time_ms: bar.timestamp_ms,
                    entry_px: pos.entry_px,
                    exit_px,
                    qty: pos.initial_qty,
                    gross_bps,
                    net_bps,
                    net_pnl_usd,
                    fees_usd: total_fees_usd,
                    funding_usd: pos.accrued_funding_usd,
                    mfe_bps: pos.mfe_bps,
                    mae_bps: pos.mae_bps,
                    exit_reason: reason,
                    entry_policy: pos.policy,
                });

                // ------------------------------------------------------------------
                // 4-GATE STATE-MACHINE HANDOFF: Engine A SPRT Stop -> Engine B Runner
                // ------------------------------------------------------------------
                if let (Some((sprt_stat, sprt_3m_ago, avg_adv_innov)), Some(ref feat)) =
                    (sprt_handoff_candidate, &live_feat_opt)
                {
                    if controller.current_mode == ControllerMode::Trend
                        && feat.hurst_60m >= 0.56
                        && feat.ker_30m >= 0.42
                    {
                        if let Some(current_atr) =
                            calculate_atr(&rolling_15m_bars, 20).filter(|&a| a > 0.0)
                        {
                            let pre_travel_atr = if closes_history.len() >= 25 {
                                let p_3 = closes_history[closes_history.len() - 3];
                                let p_23 = closes_history[closes_history.len() - 23];
                                (p_3 - p_23).abs() / current_atr
                            } else {
                                1.0
                            };
                            let mkt_tel = MarketTelemetry {
                                timestamp_sec: bar.timestamp_ms / 1000,
                                mid_price: bar.close,
                                spread_bps: enriched.effective_spread_bps,
                                obi: eff_obi_inst,
                                funding_1h_bps: eff_funding_1h * 1e4,
                                oi_delta_3m_pct: eff_dlog_oi_3m * 100.0,
                                oi_accel_1m_pct: 0.0,
                                oi_delta_15m_pct: eff_dlog_oi_3m * 100.0,
                                z_15m: feat.z_15,
                                z_60m_pre_impulse: feat.z_60 * 0.75,
                                pre_breakout_travel_atr20: pre_travel_atr,
                                ker_15m: feat.ker_30m,
                                atr_5m: current_atr / (3.0_f64).sqrt(),
                            };
                            if let ArbiterAction::PostMakerAloOrder {
                                direction,
                                limit_price,
                                ..
                            } = arbiter.evaluate_engine_a_to_b_handoff(
                                &asset_cal,
                                &mkt_tel,
                                Direction::from_side(&stopped_side),
                                sprt_stat,
                                sprt_3m_ago,
                                avg_adv_innov,
                            ) {
                                let new_side = direction.to_side();
                                let is_long_b = new_side == Side::Long;
                                let sl_dist_b =
                                    (chandelier_atr_mult * current_atr).min(limit_price * 0.0045);
                                let initial_sl = quantize_price(
                                    if is_long_b {
                                        limit_price - sl_dist_b
                                    } else {
                                        limit_price + sl_dist_b
                                    },
                                    sim_cfg.sz_decimals,
                                );
                                let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
                                let tp_dist = (k_tp * current_atr).max(limit_price * 0.0045);
                                let tp_px = quantize_price(
                                    if is_long_b {
                                        limit_price + tp_dist
                                    } else {
                                        (limit_price - tp_dist).max(tick_sz)
                                    },
                                    sim_cfg.sz_decimals,
                                );
                                let max_b_notional = ou_cfg.volatility_adjusted_trend_notional_cap(
                                    sim_cfg.account_equity_usdc,
                                    limit_price,
                                    current_atr,
                                );
                                let notional = crate::hyperliquid::calculate_risk_scaled_notional(
                                    sim_cfg.account_equity_usdc,
                                    (sim_cfg.notional_usdc * 0.85).min(max_b_notional),
                                    0.0055,
                                    limit_price,
                                    initial_sl,
                                    3,
                                    2,
                                    11.0,
                                    max_b_notional,
                                );
                                summary.signals_armed += 1;
                                summary.orders_submitted += 1;
                                resting_entry = Some(RestingMakerEntry {
                                    side: new_side,
                                    limit_px: limit_price,
                                    qty: notional / limit_price.max(1e-9),
                                    target_tp_px: tp_px,
                                    hard_stop_px: initial_sl,
                                    frozen_center_px: tp_px,
                                    frozen_sigma_eq_log: feat.sigma_eq_log,
                                    half_life_m: 45.0,
                                    submitted_bar_idx: idx,
                                    ttl_bars: 2,
                                    policy: "ENGINE_B_HANDOFF",
                                    is_engine_b: true,
                                    entry_atr: current_atr,
                                });
                            }
                        }
                    }
                }
            } else {
                open_pos = Some(pos);
            }
        }

        // ------------------------------------------------------------------
        // 3. DUAL-ENGINE EVALUATION: Engine B (15m Donchian Trend) & Engine A (5m O-U Mean Reversion)
        // ------------------------------------------------------------------
        if open_pos.is_none() && !is_armed && !session_blocks_entries {
            if let Some(ref feat) = live_feat_opt {
                let macro_veto = check_macro_calendar_veto(bar.timestamp_ms);
                let cb_veto = cluster_cb.evaluate_equity_drawdown(
                    bar.timestamp_ms,
                    sim_cfg.account_equity_usdc + cumulative_pnl_usd,
                    DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC,
                );
                let is_resting_b = resting_entry
                    .as_ref()
                    .map(|r| r.policy == "ENGINE_B_TREND")
                    .unwrap_or(false);

                // 3A. ENGINE B (Policy C): Session Gating, Squeeze Filter & Two-Stage Alo Retest
                if resting_entry.is_none() || is_resting_b {
                    if let Some(ref closed_5m) = finalized_5m_bar {
                        let prior_15m = if finalized_15m_bar.is_some() && rolling_15m_bars.len() > 1
                        {
                            &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                        } else {
                            &rolling_15m_bars[..]
                        };
                        let donchian_b_opt = calculate_donchian(prior_15m, donchian_period);

                        // F-08: 5m Synthetic-15m HMM Promotion Bridge
                        if controller.current_mode != ControllerMode::Trend
                            && rolling_5m_bars.len() >= 45
                        {
                            let has_5m_bo = donchian_b_opt
                                .as_ref()
                                .map(|d| closed_5m.close > d.upper || closed_5m.close < d.lower)
                                .unwrap_or(false);
                            if has_5m_bo || engine_b_mgr.is_stage1_armed() {
                                let mut syn_15m_closes: Vec<f64> = Vec::with_capacity(100);
                                let mut idx_r = rolling_5m_bars.len();
                                while idx_r > 0 && syn_15m_closes.len() < 100 {
                                    syn_15m_closes.push(rolling_5m_bars[idx_r - 1].close);
                                    idx_r = idx_r.saturating_sub(3);
                                }
                                syn_15m_closes.reverse();
                                if syn_15m_closes.len() >= 15 {
                                    if let Some((syn_p_c, syn_p_t, syn_p_s)) =
                                        compute_hmm_posteriors(&hmm_model, &syn_15m_closes)
                                    {
                                        let is_sev_syn = feat.is_downside_shock
                                            || (feat.is_upside_shock
                                                && (syn_p_t < 0.82 || syn_p_s >= 0.50));
                                        if controller.promote_synthetic_15m_to_trend(
                                            syn_p_t,
                                            true,
                                            is_sev_syn,
                                            feat.z_60,
                                            bar.timestamp_ms,
                                        ) {
                                            _current_p_c = syn_p_c;
                                            current_p_t = syn_p_t;
                                            current_p_s = syn_p_s;
                                        }
                                    }
                                }
                            }
                        }

                        if controller.current_mode == ControllerMode::Neutral {
                            if engine_b_mgr.is_stage1_armed() {
                                engine_b_mgr.disarm();
                            }
                            if is_resting_b {
                                resting_entry = None;
                                engine_b_mgr.on_order_cancelled_or_rejected();
                            }
                        } else if let (Some(ref donchian), Some(current_atr)) = (
                            donchian_b_opt,
                            calculate_atr(&rolling_15m_bars, 20).filter(|&a| a > 0.0),
                        ) {
                            use chrono::Datelike;
                            let ts_sec = bar.timestamp_ms / 1000;
                            let wd_utc = chrono::DateTime::from_timestamp(ts_sec as i64, 0)
                                .map(|d| d.weekday().num_days_from_sunday() as u8)
                                .unwrap_or(1);
                            let rel_vol_15m = calculate_rel_vol_15m(
                                prior_15m,
                                &rolling_5m_bars,
                                finalized_15m_bar.as_ref(),
                            );
                            let ker_15m = calculate_ker_n(&rolling_15m_bars, 6);
                            let min_sq_90m = calculate_min_squeeze_ratio_90m(&rolling_15m_bars);
                            let eff_l2_obi = if has_native_l2 {
                                0.40 * eff_obi_inst + 0.35 * eff_obi_1s + 0.25 * eff_obi_5s
                            } else {
                                eff_obi_inst
                            };
                            let btc_ret_60m_abs = if symbol.eq_ignore_ascii_case("BTC") {
                                if closes_history.len() >= 61
                                    && closes_history[closes_history.len() - 61] > 0.0
                                {
                                    (bar.close / closes_history[closes_history.len() - 61])
                                        .ln()
                                        .abs()
                                } else {
                                    0.0
                                }
                            } else if let Some(bb) = eff_btc_bars {
                                let p_now =
                                    bb.partition_point(|x| x.timestamp_ms <= bar.timestamp_ms);
                                let p_1h = bb.partition_point(|x| {
                                    x.timestamp_ms
                                        <= bar.timestamp_ms.saturating_sub(60 * 60 * 1000)
                                });
                                if p_now > 0 && p_1h > 0 && bb[p_1h - 1].close > 0.0 {
                                    (bb[p_now - 1].close / bb[p_1h - 1].close).ln().abs()
                                } else {
                                    0.0
                                }
                            } else {
                                0.0
                            };
                            let tel = BreakoutTelemetry {
                                bar_open: closed_5m.open,
                                bar_high: closed_5m.high,
                                bar_low: closed_5m.low,
                                bar_close: closed_5m.close,
                                donchian_high_12_15m: donchian.upper,
                                donchian_low_12_15m: donchian.lower,
                                atr_15m: current_atr,
                                min_squeeze_ratio_90m: min_sq_90m,
                                rel_vol_15m,
                                hurst_60m: feat.hurst_60m,
                                l2_obi: eff_l2_obi,
                                oi_delta_15m_pct: crowding_tracker
                                    .oi_delta_15m_pct(bar.timestamp_ms),
                            };
                            let is_mode_mr =
                                controller.current_mode == ControllerMode::MeanReversion;
                            let eq_now = sim_cfg.account_equity_usdc + cumulative_pnl_usd;
                            match engine_b_mgr.on_5m_bar_close(
                                &trend_cfg,
                                &tel,
                                ts_sec,
                                wd_utc,
                                eq_now,
                                is_mode_mr,
                                ker_15m,
                                btc_ret_60m_abs,
                            ) {
                                TrendOrderAction::PostAloMakerRetestOrder {
                                    dir,
                                    limit_px,
                                    stop_loss_px,
                                    notional_usdc,
                                    ttl_secs,
                                    ..
                                } => {
                                    let side = dir.to_side();
                                    let is_long = side == Side::Long;
                                    let btc_veto = eff_btc_bars
                                        .and_then(|bb| {
                                            build_historical_btc_impulse_snapshot(
                                                bb,
                                                bar.timestamp_ms,
                                            )
                                        })
                                        .and_then(|snap| {
                                            evaluate_btc_impulse_veto(symbol, is_long, true, &snap)
                                        });
                                    let crowding_mult =
                                        crowding_tracker.directional_trend_multiplier(&side);
                                    let adverse_shock = (is_long
                                        && (feat.is_upside_shock || feat.z_60 >= 2.5))
                                        || (!is_long
                                            && (feat.is_downside_shock || feat.z_60 <= -2.5));
                                    if macro_veto.is_some() {
                                        summary.macro_calendar_vetoes += 1;
                                        engine_b_mgr.on_order_cancelled_or_rejected();
                                    } else if cb_veto.is_some() {
                                        summary.cluster_cb_vetoes += 1;
                                        engine_b_mgr.on_order_cancelled_or_rejected();
                                    } else if btc_veto.is_some() {
                                        summary.btc_impulse_vetoes += 1;
                                        engine_b_mgr.on_order_cancelled_or_rejected();
                                    } else if adverse_shock || crowding_mult < 0.45 {
                                        engine_b_mgr.on_order_cancelled_or_rejected();
                                    } else {
                                        let eff_bid = if bar.best_bid > 0.0 {
                                            bar.best_bid
                                        } else {
                                            bar.close - 0.5 * tick_sz
                                        };
                                        let eff_ask = if bar.best_ask > eff_bid {
                                            bar.best_ask
                                        } else {
                                            bar.close + 0.5 * tick_sz
                                        };
                                        let spread = eff_ask - eff_bid;
                                        let raw_limit = if is_long {
                                            let max_bid = if spread >= 2.0 * tick_sz - 1e-9
                                                && eff_ask - tick_sz > eff_bid
                                            {
                                                eff_ask - tick_sz
                                            } else {
                                                eff_bid
                                            };
                                            limit_px.min(max_bid)
                                        } else {
                                            let min_ask = if spread >= 2.0 * tick_sz - 1e-9
                                                && eff_bid + tick_sz < eff_ask
                                            {
                                                eff_bid + tick_sz
                                            } else {
                                                eff_ask
                                            };
                                            limit_px.max(min_ask)
                                        };
                                        let q_limit_px =
                                            quantize_price(raw_limit, sim_cfg.sz_decimals);
                                        let safe_sl = if is_long {
                                            stop_loss_px.min(q_limit_px * 0.9975)
                                        } else {
                                            stop_loss_px.max(q_limit_px * 1.0025)
                                        };
                                        let initial_sl =
                                            quantize_price(safe_sl, sim_cfg.sz_decimals);
                                        let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
                                        let tp_dist = (k_tp * current_atr).max(q_limit_px * 0.0045);
                                        let tp_px = quantize_price(
                                            if is_long {
                                                q_limit_px + tp_dist
                                            } else {
                                                (q_limit_px - tp_dist).max(tick_sz)
                                            },
                                            sim_cfg.sz_decimals,
                                        );
                                        let notional = (notional_usdc * crowding_mult).clamp(
                                            trend_cfg.min_trade_notional_usdc,
                                            trend_cfg.max_trend_notional_usdc,
                                        );
                                        let qty = notional / q_limit_px.max(1e-9);
                                        summary.signals_armed += 1;
                                        summary.orders_submitted += 1;
                                        resting_entry = Some(RestingMakerEntry {
                                            side,
                                            limit_px: q_limit_px,
                                            qty,
                                            target_tp_px: tp_px,
                                            hard_stop_px: initial_sl,
                                            frozen_center_px: tp_px,
                                            frozen_sigma_eq_log: feat.sigma_eq_log,
                                            half_life_m: 45.0,
                                            submitted_bar_idx: idx,
                                            ttl_bars: ((ttl_secs / 60).max(1)) as usize,
                                            policy: "ENGINE_B_TREND",
                                            is_engine_b: true,
                                            entry_atr: current_atr,
                                        });
                                    }
                                }
                                TrendOrderAction::CancelRestingRetestOrder { .. } => {
                                    if is_resting_b {
                                        resting_entry = None;
                                        summary.orders_expired_unfilled += 1;
                                    }
                                }
                                TrendOrderAction::LogShadowCounterfactual { .. }
                                | TrendOrderAction::None => {}
                            }
                        }
                    }
                }
            }
        }

        if open_pos.is_none() && resting_entry.is_none() && !session_blocks_entries {
            if let Some(ref feat) = live_feat_opt {
                let macro_veto = check_macro_calendar_veto(bar.timestamp_ms);
                let cb_veto = cluster_cb.evaluate_equity_drawdown(
                    bar.timestamp_ms,
                    sim_cfg.account_equity_usdc + cumulative_pnl_usd,
                    DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC,
                );

                // 3B. ENGINE A: Evaluate 5m O-U Arming & 1m Multi-Bar Reversal Confirmation
                let consolidation_vol_ok = feat.z_15.abs() < 2.2 && feat.z_60.abs() < 2.5;
                let n_hist = closes_history.len();
                let ret_60 = if n_hist >= 60 && closes_history[n_hist - 60] > 0.0 {
                    (bar.close / closes_history[n_hist - 60]).ln()
                } else {
                    0.0
                };
                let ret_120 = if n_hist >= 120 && closes_history[n_hist - 120] > 0.0 {
                    (bar.close / closes_history[n_hist - 120]).ln()
                } else {
                    0.0
                };
                let ret_180 = if n_hist >= 180 && closes_history[n_hist - 180] > 0.0 {
                    (bar.close / closes_history[n_hist - 180]).ln()
                } else {
                    0.0
                };
                let ret_240 = if n_hist >= 240 && closes_history[n_hist - 240] > 0.0 {
                    (bar.close / closes_history[n_hist - 240]).ln()
                } else {
                    0.0
                };
                let ret_720 = if n_hist >= 720 && closes_history[n_hist - 720] > 0.0 {
                    (bar.close / closes_history[n_hist - 720]).ln()
                } else {
                    0.0
                };
                let ma60_vs_240 = if n_hist >= 240 {
                    let ma60 = closes_history[n_hist - 60..].iter().sum::<f64>() / 60.0;
                    let ma240 = closes_history[n_hist - 240..].iter().sum::<f64>() / 240.0;
                    if ma240 > 0.0 {
                        (ma60 / ma240) - 1.0
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let kalman_waterfall_down =
                    (feat.kalman_drift_veto && feat.kalman_slope < 0.0) || feat.cusum_neg >= 8.0;
                let kalman_waterfall_up =
                    (feat.kalman_drift_veto && feat.kalman_slope > 0.0) || feat.cusum_pos >= 8.0;
                let kalman_long_ok = !kalman_waterfall_down
                    && feat.lambda_dir_long > 0.0
                    && feat.z_60 > -1.90
                    && !(ret_60 <= -max_drift_60 && ret_120 <= 0.0015)
                    && ret_120 > -max_drift_120
                    && ret_180 > -max_slow_drift_180
                    && ret_240 > -max_slow_drift_240
                    && (ma60_vs_240 >= -0.0008
                        || symbol.eq_ignore_ascii_case("BTC") && ma60_vs_240 >= -0.0022);
                let kalman_short_ok = !kalman_waterfall_up
                    && feat.lambda_dir_short > 0.0
                    && feat.z_60 < 1.90
                    && ret_60 < max_short_drift_60
                    && ret_120 < max_short_drift_120
                    && ret_240 < max_short_drift_240
                    && (ma60_vs_240 <= 0.0040 || (ret_720 < -0.0150 && ma60_vs_240 <= 0.0080));

                let is_eligible = resting_entry.is_none()
                    && feat.is_consolidation
                    && consolidation_vol_ok
                    && feat.is_valid_half_life;

                if is_armed {
                    let shock_vetoed = match arm_side {
                        Side::Long => {
                            feat.is_downside_shock
                                || kalman_waterfall_down
                                || feat.lambda_dir_long <= 0.0
                        }
                        Side::Short => {
                            feat.is_upside_shock
                                || kalman_waterfall_up
                                || feat.lambda_dir_short <= 0.0
                        }
                    };
                    if macro_veto.is_some() {
                        summary.macro_calendar_vetoes += 1;
                        is_armed = false;
                    } else if cb_veto.is_some() {
                        summary.cluster_cb_vetoes += 1;
                        is_armed = false;
                    } else if !is_eligible || shock_vetoed {
                        is_armed = false;
                    } else {
                        let n_rc = closes_history.len();
                        let mut recent_4 = Vec::with_capacity(4);
                        recent_4.extend_from_slice(&closes_history[n_rc.saturating_sub(3)..]);
                        recent_4.push(bar.close);

                        let eff_bid = if bar.best_bid > 0.0 {
                            bar.best_bid
                        } else {
                            bar.close - 0.5 * tick_sz
                        };
                        let eff_ask = if bar.best_ask > eff_bid {
                            bar.best_ask
                        } else {
                            bar.close + 0.5 * tick_sz
                        };
                        let funding_drag_bps = match arm_side {
                            Side::Long => (eff_funding_1h * 1e4) * (arm_half_life / 60.0),
                            Side::Short => (-eff_funding_1h * 1e4) * (arm_half_life / 60.0),
                        };
                        let max_arm_bars = ((0.5 * arm_half_life).round() as usize).clamp(10, 15);
                        let eff_min_edge_bps = ou_cfg.min_net_edge_bps;

                        let action = evaluate_armed_reversal_1m_step(
                            &arm_side,
                            arm_entry_px,
                            arm_exit_px,
                            &mut arm_extreme_px,
                            &mut arm_bars_since_ext,
                            &mut arm_total_bars,
                            max_arm_bars,
                            ou_cfg.rebound_gamma,
                            ou_cfg.max_join_consumption_ratio,
                            ou_cfg.max_retest_consumption_ratio,
                            eff_min_edge_bps,
                            sim_cfg.maker_fee_bps,
                            funding_drag_bps,
                            bar.high,
                            bar.low,
                            bar.close,
                            &recent_4,
                            eff_bid,
                            eff_ask,
                            tick_sz,
                            feat.sigma_1m_prev,
                        );

                        match action {
                            OrderPlacementAction::Wait => {}
                            OrderPlacementAction::DisarmAbort => {
                                is_armed = false;
                            }
                            OrderPlacementAction::PostAloJoinBook {
                                side,
                                limit_price,
                                entry_ttl_mins,
                                ..
                            }
                            | OrderPlacementAction::PostAloRetestLimit {
                                side,
                                limit_price,
                                entry_ttl_mins,
                                ..
                            } => {
                                let is_long = side == Side::Long;
                                let btc_veto = eff_btc_bars
                                    .and_then(|bb| {
                                        build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms)
                                    })
                                    .and_then(|snap| {
                                        evaluate_btc_impulse_veto(symbol, is_long, false, &snap)
                                    });

                                // Priority 1 & 2 Preemption Check via UnifiedPortfolioArbiter
                                let dir_cand = Direction::from_side(&side);
                                let lambda_dir = if is_long {
                                    feat.lambda_dir_long
                                } else {
                                    feat.lambda_dir_short
                                };
                                let obi_confirm_ok = if has_native_l2 {
                                    dir_cand.sign() * eff_obi_inst >= -0.20
                                } else {
                                    dir_cand.sign() * eff_obi_inst >= 0.0
                                        && feat.hurst_60m <= 0.54
                                        && feat.ker_30m <= 0.45
                                        && lambda_dir >= 0.50
                                };

                                if btc_veto.is_some() {
                                    summary.btc_impulse_vetoes += 1;
                                    is_armed = false;
                                } else if !obi_confirm_ok || lambda_dir <= 0.0 {
                                    is_armed = false;
                                } else {
                                    is_armed = false;
                                    let raw_edge_bps = calculate_directional_edge_bps(
                                        limit_price,
                                        arm_exit_px,
                                        is_long,
                                    );
                                    let net_edge_bps = raw_edge_bps
                                        - 2.0 * sim_cfg.maker_fee_bps
                                        - funding_drag_bps;

                                    if net_edge_bps >= eff_min_edge_bps {
                                        let tp1_px =
                                            quantize_price(arm_exit_px, sim_cfg.sz_decimals);
                                        let min_profitable = if is_long {
                                            tp1_px > limit_price + tick_sz
                                        } else {
                                            tp1_px + tick_sz < limit_price
                                        };
                                        if min_profitable {
                                            let hard_stop_frac =
                                                ou_cfg.hard_disaster_stop_bps * 1e-4;
                                            let tp_dist = (tp1_px - limit_price).abs();
                                            let sl_dist = if has_native_l2 {
                                                compute_engine_a_disaster_stop_distance(
                                                    symbol,
                                                    limit_price,
                                                    tp1_px,
                                                )
                                            } else {
                                                let calibrated_sl =
                                                    (1.05 * tp_dist).max(0.0030 * limit_price);
                                                calibrated_sl
                                                    .min(limit_price * 0.0040)
                                                    .min(limit_price * hard_stop_frac)
                                            };
                                            let hard_sl_px = if is_long {
                                                limit_price - sl_dist
                                            } else {
                                                limit_price + sl_dist
                                            };
                                            let sizing_sl_dist = 0.0045 * limit_price;
                                            let sizing_sl_px = if has_native_l2 {
                                                hard_sl_px
                                            } else if is_long {
                                                limit_price - sizing_sl_dist
                                            } else {
                                                limit_price + sizing_sl_dist
                                            };
                                            let vol_adj_cap = ou_cfg
                                                .volatility_adjusted_notional_cap(
                                                    sim_cfg.account_equity_usdc,
                                                    arm_sigma_eq_log,
                                                );
                                            let kelly_scalar = if has_native_l2 {
                                                compute_fee_adjusted_kelly_scalar(
                                                    raw_edge_bps,
                                                    eff_min_edge_bps,
                                                    sim_cfg.maker_fee_bps,
                                                    funding_drag_bps,
                                                ) * lambda_dir
                                            } else {
                                                1.0
                                            };
                                            let notional =
                                                crate::hyperliquid::calculate_risk_scaled_notional(
                                                    sim_cfg.account_equity_usdc,
                                                    (sim_cfg.notional_usdc * kelly_scalar)
                                                        .min(vol_adj_cap),
                                                    0.0075 * kelly_scalar,
                                                    limit_price,
                                                    sizing_sl_px,
                                                    3,
                                                    2,
                                                    22.0, // Minimum $22 total ($11 per 50% tranche)
                                                    vol_adj_cap,
                                                );
                                            let qty = notional / limit_price.max(1e-9);
                                            let policy = if (entry_ttl_mins - 2.0).abs() < 1e-6 {
                                                "JOIN_BOOK"
                                            } else {
                                                "RETEST_LIMIT"
                                            };
                                            summary.orders_submitted += 1;
                                            resting_entry = Some(RestingMakerEntry {
                                                side,
                                                limit_px: limit_price,
                                                qty,
                                                target_tp_px: tp1_px,
                                                hard_stop_px: hard_sl_px,
                                                frozen_center_px: arm_exit_px,
                                                frozen_sigma_eq_log: arm_sigma_eq_log,
                                                half_life_m: arm_half_life,
                                                submitted_bar_idx: idx,
                                                ttl_bars: entry_ttl_mins.round() as usize,
                                                policy,
                                                is_engine_b: false,
                                                entry_atr: 0.0,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else if is_eligible {
                    if let Some(ref th) = feat.thresholds_opt {
                        let wants_long = (bar.close <= th.entry_long || bar.low <= th.entry_long)
                            && !feat.is_downside_shock
                            && kalman_long_ok;
                        let wants_short = (bar.close >= th.entry_short
                            || bar.high >= th.entry_short)
                            && !feat.is_upside_shock
                            && kalman_short_ok;
                        if wants_long || wants_short {
                            if macro_veto.is_some() {
                                summary.macro_calendar_vetoes += 1;
                            } else if cb_veto.is_some() {
                                summary.cluster_cb_vetoes += 1;
                            } else if wants_long {
                                is_armed = true;
                                arm_side = Side::Long;
                                arm_entry_px = th.entry_long;
                                arm_exit_px = th.exit_long;
                                arm_extreme_px = bar.close.min(bar.low);
                                arm_bars_since_ext = 0;
                                arm_total_bars = 0;
                                arm_half_life = feat.half_life_m;
                                arm_sigma_eq_log = feat.sigma_eq_log;
                                summary.signals_armed += 1;
                            } else {
                                is_armed = true;
                                arm_side = Side::Short;
                                arm_entry_px = th.entry_short;
                                arm_exit_px = th.exit_short;
                                arm_extreme_px = bar.close.max(bar.high);
                                arm_bars_since_ext = 0;
                                arm_total_bars = 0;
                                arm_half_life = feat.half_life_m;
                                arm_sigma_eq_log = feat.sigma_eq_log;
                                summary.signals_armed += 1;
                            }
                        }
                    }
                }

                // 3C. ENGINE C (`HourlySqueezeHarvester`), ENGINE D (`CascadeAbsorptionReversal`) & US OPEN ORB
                if (!is_armed || !has_native_l2)
                    && resting_entry.is_none()
                    && macro_veto.is_none()
                    && cb_veto.is_none()
                {
                    let low_30m = bars[idx.saturating_sub(30)..=idx]
                        .iter()
                        .map(|b| b.low)
                        .fold(bar.low, f64::min);
                    let bar_hour_idx = bar.timestamp_ms / 3_600_000;
                    let minute_of_hour = (bar.timestamp_ms / 60_000) % 60;
                    let engine_c_4gate_ok = (48..=58).contains(&minute_of_hour)
                        && bar_hour_idx != last_engine_c_hour
                        && (has_native_l2 || enriched.funding_z_24h >= asset_cal.funding_z24h_min)
                        && (has_native_l2 || enriched.cvd_5m >= 0.18)
                        && eff_obi_inst >= asset_cal.tau_obi_eng_a;

                    if engine_c_4gate_ok {
                        if let Some(sq) = evaluate_engine_c_hourly_squeeze(
                            bar.timestamp_ms,
                            eff_funding_1h,
                            bar.close,
                            bar.best_bid,
                            bar.oracle_px,
                            low_30m,
                            feat.sigma_eq_log,
                            ou_cfg.min_net_edge_bps,
                            feat.z_15,
                            feat.z_60,
                            tick_sz,
                        )
                        .filter(|s| {
                            has_native_l2
                                || ((s.limit_px - s.hard_stop_px) / s.limit_px.max(1e-9)) <= 0.0076
                        }) {
                            is_armed = false;
                            last_engine_c_hour = bar_hour_idx;
                            let limit_px = quantize_price(sq.limit_px, sim_cfg.sz_decimals);
                            let tp_px = quantize_price(sq.target_tp_px, sim_cfg.sz_decimals);
                            let sl_px = quantize_price(sq.hard_stop_px, sim_cfg.sz_decimals);
                            let vol_adj_cap = ou_cfg.volatility_adjusted_notional_cap(
                                sim_cfg.account_equity_usdc,
                                feat.sigma_eq_log,
                            );
                            let notional = crate::hyperliquid::calculate_risk_scaled_notional(
                                sim_cfg.account_equity_usdc,
                                sim_cfg.notional_usdc.min(vol_adj_cap),
                                0.0065,
                                limit_px,
                                sl_px,
                                3,
                                2,
                                22.0,
                                vol_adj_cap,
                            );
                            summary.signals_armed += 1;
                            summary.orders_submitted += 1;
                            resting_entry = Some(RestingMakerEntry {
                                side: sq.side,
                                limit_px,
                                qty: notional / limit_px.max(1e-9),
                                target_tp_px: tp_px,
                                hard_stop_px: sl_px,
                                frozen_center_px: tp_px,
                                frozen_sigma_eq_log: feat.sigma_eq_log,
                                half_life_m: 30.0,
                                submitted_bar_idx: idx,
                                ttl_bars: 3,
                                policy: "ENGINE_C_SQUEEZE",
                                is_engine_b: false,
                                entry_atr: 0.0,
                            });
                        }
                    }

                    if !is_armed && resting_entry.is_none() {
                        if let Some(cas) = evaluate_engine_d_cascade_reversal(
                            bar.close,
                            bar.open,
                            bar.low,
                            bar.best_bid,
                            bar.oracle_px,
                            eff_dlog_oi_3m,
                            feat.z_15,
                            eff_obi_inst,
                            ou_cfg.min_net_edge_bps,
                            tick_sz,
                        ) {
                            let limit_px = quantize_price(cas.limit_px, sim_cfg.sz_decimals);
                            let tp_px = quantize_price(cas.target_tp_px, sim_cfg.sz_decimals);
                            let sl_px = quantize_price(cas.hard_stop_px, sim_cfg.sz_decimals);
                            let vol_adj_cap = ou_cfg.volatility_adjusted_notional_cap(
                                sim_cfg.account_equity_usdc,
                                feat.sigma_eq_log,
                            );
                            let notional = crate::hyperliquid::calculate_risk_scaled_notional(
                                sim_cfg.account_equity_usdc,
                                (sim_cfg.notional_usdc * 0.75).min(vol_adj_cap),
                                0.0060,
                                limit_px,
                                sl_px,
                                3,
                                2,
                                22.0,
                                vol_adj_cap,
                            );
                            summary.signals_armed += 1;
                            summary.orders_submitted += 1;
                            resting_entry = Some(RestingMakerEntry {
                                side: cas.side,
                                limit_px,
                                qty: notional / limit_px.max(1e-9),
                                target_tp_px: tp_px,
                                hard_stop_px: sl_px,
                                frozen_center_px: tp_px,
                                frozen_sigma_eq_log: feat.sigma_eq_log,
                                half_life_m: 20.0,
                                submitted_bar_idx: idx,
                                ttl_bars: 2,
                                policy: "ENGINE_D_CASCADE",
                                is_engine_b: false,
                                entry_atr: 0.0,
                            });
                        }
                    }

                    // 3D. US Open Opening Range Breakout & Anti-Judas Swing Sub-Engine (Section 4 & 6.2)
                    if !is_armed && resting_entry.is_none() {
                        match orb_action_this_bar {
                            OrbAction::Inactive => {}
                            OrbAction::PostTrueExpansionMakerRetest {
                                dir,
                                limit_price,
                                initial_sl_price,
                            } => {
                                let orb_side = dir.to_side();
                                let is_long_orb = orb_side == Side::Long;
                                let btc_veto_orb = eff_btc_bars
                                    .and_then(|bb| {
                                        build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms)
                                    })
                                    .and_then(|snap| {
                                        evaluate_btc_impulse_veto(symbol, is_long_orb, true, &snap)
                                    });
                                let lambda_dir_orb = if is_long_orb {
                                    feat.lambda_dir_long
                                } else {
                                    feat.lambda_dir_short
                                };
                                let trend_regime_ok = feat.hurst_60m >= 0.54
                                    && feat.ker_30m >= 0.38
                                    && lambda_dir_orb >= 0.75;
                                if btc_veto_orb.is_none() && trend_regime_ok {
                                    if let Some(current_atr) =
                                        calculate_atr(&rolling_15m_bars, 20).filter(|&a| a > 0.0)
                                    {
                                        let l_px = quantize_price(limit_price, sim_cfg.sz_decimals);
                                        let sl_px =
                                            quantize_price(initial_sl_price, sim_cfg.sz_decimals);
                                        let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
                                        let tp_dist = (k_tp * current_atr).max(l_px * 0.0045);
                                        let tp_px = quantize_price(
                                            if is_long_orb {
                                                l_px + tp_dist
                                            } else {
                                                (l_px - tp_dist).max(tick_sz)
                                            },
                                            sim_cfg.sz_decimals,
                                        );
                                        let vol_adj_cap = ou_cfg.volatility_adjusted_notional_cap(
                                            sim_cfg.account_equity_usdc,
                                            feat.sigma_eq_log,
                                        );
                                        let notional =
                                            crate::hyperliquid::calculate_risk_scaled_notional(
                                                sim_cfg.account_equity_usdc,
                                                (sim_cfg.notional_usdc * 0.85 * lambda_dir_orb)
                                                    .min(vol_adj_cap),
                                                0.0065 * lambda_dir_orb,
                                                l_px,
                                                sl_px,
                                                3,
                                                2,
                                                22.0,
                                                vol_adj_cap,
                                            );
                                        let qty = notional / l_px.max(1e-9);
                                        summary.signals_armed += 1;
                                        summary.orders_submitted += 1;
                                        resting_entry = Some(RestingMakerEntry {
                                            side: orb_side,
                                            limit_px: l_px,
                                            qty,
                                            target_tp_px: tp_px,
                                            hard_stop_px: sl_px,
                                            frozen_center_px: tp_px,
                                            frozen_sigma_eq_log: feat.sigma_eq_log,
                                            half_life_m: 45.0,
                                            submitted_bar_idx: idx,
                                            ttl_bars: 3,
                                            policy: "ENGINE_B_US_OPEN_ORB",
                                            is_engine_b: true,
                                            entry_atr: current_atr,
                                        });
                                    }
                                }
                            }
                            OrbAction::PostJudasSwingTrapReversal {
                                dir,
                                limit_price,
                                wick_stop_price,
                                tp1_midpoint,
                                ..
                            } => {
                                let rev_side = dir.to_side();
                                let is_long_rev = rev_side == Side::Long;
                                let btc_veto_rev = eff_btc_bars
                                    .and_then(|bb| {
                                        build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms)
                                    })
                                    .and_then(|snap| {
                                        evaluate_btc_impulse_veto(symbol, is_long_rev, false, &snap)
                                    });
                                let lambda_dir_rev = if is_long_rev {
                                    feat.lambda_dir_long
                                } else {
                                    feat.lambda_dir_short
                                };
                                let kalman_ok = if is_long_rev {
                                    kalman_long_ok && !feat.is_downside_shock
                                } else {
                                    kalman_short_ok && !feat.is_upside_shock
                                };
                                let l_px = quantize_price(limit_price, sim_cfg.sz_decimals);
                                let sl_px = quantize_price(wick_stop_price, sim_cfg.sz_decimals);
                                let tp1_px = quantize_price(tp1_midpoint, sim_cfg.sz_decimals);
                                let raw_edge_bps =
                                    calculate_directional_edge_bps(l_px, tp1_px, is_long_rev);
                                let net_edge_bps = raw_edge_bps - 2.0 * sim_cfg.maker_fee_bps;
                                let min_prof = if is_long_rev {
                                    tp1_px > l_px + 2.0 * tick_sz
                                } else {
                                    tp1_px + 2.0 * tick_sz < l_px
                                };
                                if btc_veto_rev.is_none()
                                    && is_eligible
                                    && kalman_ok
                                    && (has_native_l2
                                        || (feat.hurst_60m <= 0.44
                                            && feat.ker_30m <= 0.25
                                            && lambda_dir_rev >= 0.85))
                                    && lambda_dir_rev >= 0.70
                                    && net_edge_bps >= ou_cfg.min_net_edge_bps
                                    && min_prof
                                {
                                    let vol_adj_cap = ou_cfg.volatility_adjusted_notional_cap(
                                        sim_cfg.account_equity_usdc,
                                        feat.sigma_eq_log,
                                    );
                                    let notional =
                                        crate::hyperliquid::calculate_risk_scaled_notional(
                                            sim_cfg.account_equity_usdc,
                                            (sim_cfg.notional_usdc * 0.75 * lambda_dir_rev)
                                                .min(vol_adj_cap),
                                            0.0060 * lambda_dir_rev,
                                            l_px,
                                            sl_px,
                                            3,
                                            2,
                                            22.0,
                                            vol_adj_cap,
                                        );
                                    let qty = notional / l_px.max(1e-9);
                                    summary.signals_armed += 1;
                                    summary.orders_submitted += 1;
                                    resting_entry = Some(RestingMakerEntry {
                                        side: rev_side,
                                        limit_px: l_px,
                                        qty,
                                        target_tp_px: tp1_px,
                                        hard_stop_px: sl_px,
                                        frozen_center_px: tp1_px,
                                        frozen_sigma_eq_log: feat.sigma_eq_log,
                                        half_life_m: 25.0,
                                        submitted_bar_idx: idx,
                                        ttl_bars: 2,
                                        policy: "JUDAS_SWING_REVERSAL",
                                        is_engine_b: false,
                                        entry_atr: 0.0,
                                    });
                                }
                            }
                        }
                    }

                    // 3E. Pure-OHLCV Triggers for Engine B (Volatility Squeeze) & Engine C/D (Climax Reversal)
                    if !has_native_l2
                        && !is_armed
                        && resting_entry.is_none()
                        && cached_atr_15m > 0.0
                        && (bars.len() < 50_000 || candle_bucket_15m != last_squeeze_15m_bucket)
                    {
                        let rel_vol_1m = bar.volume / enricher.ema_vol_60.max(1e-9);
                        let rel_vol_3m = if idx >= 2 {
                            (bars[idx].volume + bars[idx - 1].volume + bars[idx - 2].volume)
                                / (3.0 * enricher.ema_vol_60.max(1e-9))
                        } else {
                            rel_vol_1m
                        };
                        let atr_5m_est = cached_atr_15m / (3.0_f64).sqrt();
                        match evaluate_ohlcv_native_engines_bcd(
                            bar.open,
                            bar.high,
                            bar.low,
                            bar.close,
                            rel_vol_1m,
                            rel_vol_3m,
                            cached_min_squeeze_90m,
                            cached_donchian_high_12,
                            cached_donchian_low_12,
                            cached_ema_15m,
                            feat.z_15,
                            atr_5m_est,
                            cached_atr_15m,
                            &asset_cal,
                        ) {
                            OhlcvHighPayoffSignal::None => {}
                            OhlcvHighPayoffSignal::EngineBSqueezeBreakout {
                                dir,
                                maker_limit_px,
                                stop_loss_px,
                                tp1_px,
                                ..
                            } => {
                                let sq_side = dir.to_side();
                                let is_long_sq = sq_side == Side::Long;
                                let btc_veto_sq = eff_btc_bars
                                    .and_then(|bb| {
                                        build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms)
                                    })
                                    .and_then(|snap| {
                                        evaluate_btc_impulse_veto(symbol, is_long_sq, true, &snap)
                                    });
                                let lambda_dir_sq = if is_long_sq {
                                    feat.lambda_dir_long
                                } else {
                                    feat.lambda_dir_short
                                };
                                let btc_vol_ok = !symbol.eq_ignore_ascii_case("BTC")
                                    || (cached_min_squeeze_90m <= 0.84
                                        && rel_vol_1m >= 2.50
                                        && lambda_dir_sq >= 0.85);
                                if btc_veto_sq.is_none() && lambda_dir_sq >= 0.70 && btc_vol_ok {
                                    last_squeeze_15m_bucket = candle_bucket_15m;
                                    let l_px = quantize_price(maker_limit_px, sim_cfg.sz_decimals);
                                    let max_sl_dist = l_px * 0.0040;
                                    let raw_sl = if is_long_sq {
                                        stop_loss_px.max(l_px - max_sl_dist)
                                    } else {
                                        stop_loss_px.min(l_px + max_sl_dist)
                                    };
                                    let sl_px = quantize_price(raw_sl, sim_cfg.sz_decimals);
                                    let tp_px = quantize_price(tp1_px, sim_cfg.sz_decimals);
                                    let max_b_notional = ou_cfg
                                        .volatility_adjusted_trend_notional_cap(
                                            sim_cfg.account_equity_usdc,
                                            l_px,
                                            cached_atr_15m,
                                        );
                                    let notional =
                                        crate::hyperliquid::calculate_risk_scaled_notional(
                                            sim_cfg.account_equity_usdc,
                                            (sim_cfg.notional_usdc * 0.85).min(max_b_notional),
                                            0.0060,
                                            l_px,
                                            sl_px,
                                            3,
                                            2,
                                            22.0,
                                            max_b_notional,
                                        );
                                    summary.signals_armed += 1;
                                    summary.orders_submitted += 1;
                                    resting_entry = Some(RestingMakerEntry {
                                        side: sq_side,
                                        limit_px: l_px,
                                        qty: notional / l_px.max(1e-9),
                                        target_tp_px: tp_px,
                                        hard_stop_px: sl_px,
                                        frozen_center_px: tp_px,
                                        frozen_sigma_eq_log: feat.sigma_eq_log,
                                        half_life_m: 45.0,
                                        submitted_bar_idx: idx,
                                        ttl_bars: 2,
                                        policy: "ENGINE_B_TREND",
                                        is_engine_b: true,
                                        entry_atr: cached_atr_15m,
                                    });
                                }
                            }
                            OhlcvHighPayoffSignal::EngineDClimaxReversal {
                                dir,
                                maker_limit_px,
                                stop_loss_px,
                                tp1_px,
                                ..
                            } => {
                                let cl_side = dir.to_side();
                                let is_long_cl = cl_side == Side::Long;
                                let btc_veto_cl = eff_btc_bars
                                    .and_then(|bb| {
                                        build_historical_btc_impulse_snapshot(bb, bar.timestamp_ms)
                                    })
                                    .and_then(|snap| {
                                        evaluate_btc_impulse_veto(symbol, is_long_cl, false, &snap)
                                    });
                                let lambda_dir_cl = if is_long_cl {
                                    feat.lambda_dir_long
                                } else {
                                    feat.lambda_dir_short
                                };
                                let kalman_cl_ok = if is_long_cl {
                                    !kalman_waterfall_down
                                } else {
                                    !kalman_waterfall_up
                                };
                                if btc_veto_cl.is_none() && kalman_cl_ok && lambda_dir_cl >= 0.65 {
                                    let l_px = quantize_price(maker_limit_px, sim_cfg.sz_decimals);
                                    let max_sl_dist = l_px * 0.0040;
                                    let raw_sl = if is_long_cl {
                                        stop_loss_px.max(l_px - max_sl_dist)
                                    } else {
                                        stop_loss_px.min(l_px + max_sl_dist)
                                    };
                                    let sl_px = quantize_price(raw_sl, sim_cfg.sz_decimals);
                                    let tp_px = quantize_price(tp1_px, sim_cfg.sz_decimals);
                                    let min_prof = if is_long_cl {
                                        tp_px > l_px * 1.0022
                                    } else {
                                        tp_px < l_px * 0.9978
                                    };
                                    if min_prof {
                                        last_squeeze_15m_bucket = candle_bucket_15m;
                                        let vol_adj_cap = ou_cfg.volatility_adjusted_notional_cap(
                                            sim_cfg.account_equity_usdc,
                                            feat.sigma_eq_log,
                                        );
                                        let notional =
                                            crate::hyperliquid::calculate_risk_scaled_notional(
                                                sim_cfg.account_equity_usdc,
                                                (sim_cfg.notional_usdc * 0.80).min(vol_adj_cap),
                                                0.0060,
                                                l_px,
                                                sl_px,
                                                3,
                                                2,
                                                22.0,
                                                vol_adj_cap,
                                            );
                                        summary.signals_armed += 1;
                                        summary.orders_submitted += 1;
                                        resting_entry = Some(RestingMakerEntry {
                                            side: cl_side,
                                            limit_px: l_px,
                                            qty: notional / l_px.max(1e-9),
                                            target_tp_px: tp_px,
                                            hard_stop_px: sl_px,
                                            frozen_center_px: tp_px,
                                            frozen_sigma_eq_log: feat.sigma_eq_log,
                                            half_life_m: 25.0,
                                            submitted_bar_idx: idx,
                                            ttl_bars: 2,
                                            policy: "ENGINE_D_CASCADE",
                                            is_engine_b: false,
                                            entry_atr: 0.0,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        prev_obi_smooth = eff_obi_inst;
        closes_history.push(bar.close);
        if closes_history.len() > 960 {
            closes_history.remove(0);
        }
    }

    let total_closed = summary.wins + summary.losses;
    if total_closed > 0 {
        summary.win_rate_pct = (summary.wins as f64 / total_closed as f64) * 100.0;
    }
    summary.profit_factor = if gross_losses_usd > 1e-6 {
        gross_wins_usd / gross_losses_usd
    } else if gross_wins_usd > 0.0 {
        99.9
    } else {
        0.0
    };

    summary
}

// ============================================================================
// PHASE 3 & PHASE 4: SYNTHETIC L2 MICROSTRUCTURE ENRICHER & 4-ENGINE ARBITER
// ============================================================================

/// Raw 1-minute historical OHLCV bar from Binance Data Vision or Hyperliquid REST.
#[derive(Debug, Clone, Copy)]
pub struct RawOhlcvBar {
    pub timestamp_ms: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Enriched 1-minute bar with synthetic L2 microstructure observables (Section 1.2 & 6.2).
#[derive(Debug, Clone, Copy)]
pub struct EnrichedMicrostructureBar {
    pub raw: RawOhlcvBar,
    pub effective_spread_bps: f64,
    pub synthetic_obi: f64,
    pub synthetic_obi_smoothed: f64,
    pub synthetic_funding_1h_bps: f64,
    pub funding_z_24h: f64,
    pub cvd_5m: f64,
    pub synthetic_oi_delta_3m_pct: f64,
}

/// Causal rolling state machine enriching flat OHLCV bars into L2/Funding/OI observables.
pub struct SyntheticMicrostructureEnricher {
    window_60: VecDeque<RawOhlcvBar>,
    window_240_high_low: VecDeque<(f64, f64)>,
    funding_history_24h: VecDeque<f64>,
    ema_vol_60: f64,
    ema_cvd_5: f64,
    ema_cvd_15: f64,
    ema_cvd_120: f64,
    ema_clv_3: f64,
    ema_obi_3: f64,
    basis_state_bps: f64,
    initialized: bool,
}

impl Default for SyntheticMicrostructureEnricher {
    fn default() -> Self {
        Self::new()
    }
}

impl SyntheticMicrostructureEnricher {
    pub fn new() -> Self {
        Self {
            window_60: VecDeque::with_capacity(64),
            window_240_high_low: VecDeque::with_capacity(244),
            funding_history_24h: VecDeque::with_capacity(1444),
            ema_vol_60: 0.0,
            ema_cvd_5: 0.0,
            ema_cvd_15: 0.0,
            ema_cvd_120: 0.0,
            ema_clv_3: 0.0,
            ema_obi_3: 0.0,
            basis_state_bps: 0.0,
            initialized: false,
        }
    }

    /// Student-t CDF approximation with nu = 0.25 degrees of freedom for fat-tailed crypto BVC.
    #[inline(always)]
    fn student_t_cdf_nu025(z: f64) -> f64 {
        let scaled = z / (1.0 + 1.85 * z * z).powf(0.375);
        (0.5 + 0.5 * (1.25 * scaled).tanh()).clamp(0.0, 1.0)
    }

    /// Processes a single 1-minute OHLCV bar causally and outputs synthetic L2/Funding/OI fields.
    pub fn enrich_next_bar(&mut self, bar: RawOhlcvBar, z_15m: f64) -> EnrichedMicrostructureBar {
        let tr = (bar.high - bar.low).max(1e-12);
        let clv = ((2.0 * bar.close - bar.high - bar.low) / tr).clamp(-1.0, 1.0);
        let wick_abs = (((bar.close - bar.low) - (bar.high - bar.close)) / tr).clamp(-1.0, 1.0);

        if !self.initialized {
            self.ema_vol_60 = bar.volume.max(1.0);
            self.ema_clv_3 = clv;
            self.initialized = true;
        } else {
            self.ema_vol_60 = (2.0 / 61.0) * bar.volume + (59.0 / 61.0) * self.ema_vol_60;
            self.ema_clv_3 = 0.50 * clv + 0.50 * self.ema_clv_3;
        }

        let prev_donchian_low = self
            .window_240_high_low
            .iter()
            .fold(f64::MAX, |acc, &(_, l)| acc.min(l));
        let prev_donchian_high = self
            .window_240_high_low
            .iter()
            .fold(f64::MIN, |acc, &(h, _)| acc.max(h));

        self.window_60.push_back(bar);
        if self.window_60.len() > 60 {
            self.window_60.pop_front();
        }
        self.window_240_high_low.push_back((bar.high, bar.low));
        if self.window_240_high_low.len() > 240 {
            self.window_240_high_low.pop_front();
        }

        // 1. Compute Ardia-Guidotti-Kroencke (2024) EDGE Effective Spread Estimator
        let effective_spread_bps = self.compute_edge_spread_bps();

        // 2. Compute Bulk Volume Classification (BVC) and 3-Bar Smoothed Spread-Penalized Synthetic OBI
        let sigma_ret_60 = self.compute_return_std_60();
        let log_ret = (bar.close / bar.open.max(1e-12)).ln();
        let bvc = 2.0 * Self::student_t_cdf_nu025(log_ret / sigma_ret_60.max(1e-6)) - 1.0;
        let rel_vol = (bar.volume / self.ema_vol_60.max(1e-9)).clamp(0.2, 4.0);
        let signed_flow = bvc * rel_vol.sqrt();

        self.ema_cvd_5 = (2.0 / 6.0) * signed_flow + (4.0 / 6.0) * self.ema_cvd_5;
        self.ema_cvd_15 = (2.0 / 16.0) * signed_flow + (14.0 / 16.0) * self.ema_cvd_15;
        self.ema_cvd_120 = (2.0 / 121.0) * signed_flow + (119.0 / 121.0) * self.ema_cvd_120;

        let synthetic_obi =
            (0.65 * signed_flow + 0.55 * wick_abs * rel_vol.sqrt() + 0.30 * self.ema_clv_3)
                .tanh()
                .clamp(-1.0, 1.0);
        self.ema_obi_3 = 0.50 * synthetic_obi + 0.50 * self.ema_obi_3;
        let spread_penalty = (-0.04 * (effective_spread_bps - 3.5).max(0.0)).exp();
        let synthetic_obi_smoothed = (self.ema_obi_3 * spread_penalty).clamp(-1.0, 1.0);

        // 3. Arbitrage-Damped Basis-Shock O-U Synthetic Funding Rate (Section 1.2 & 6.2)
        let delta_cvd = self.ema_cvd_15 - self.ema_cvd_120;
        let ret_15m = if self.window_60.len() >= 15 {
            let c_15 = self.window_60[self.window_60.len() - 15].close;
            (bar.close / c_15.max(1e-12)).ln().abs()
        } else {
            log_ret.abs()
        };
        let accel_burst = ((ret_15m / (sigma_ret_60 * 15.0_f64.sqrt()).max(1e-6)) - 1.35).max(0.0);
        let kappa_sign = if delta_cvd >= 0.0 { 5.20 } else { 3.40 };
        let alpha_arb = 0.0153; // 45-minute arbitrage half-life
        self.basis_state_bps = (1.0 - alpha_arb) * self.basis_state_bps
            + 0.18 * (kappa_sign * delta_cvd * accel_burst);

        let interest_clamp = (0.125 - self.basis_state_bps).clamp(-0.625, 0.625);
        let synthetic_funding_1h_bps = (self.basis_state_bps + interest_clamp).clamp(-12.0, 12.0);

        self.funding_history_24h.push_back(synthetic_funding_1h_bps);
        if self.funding_history_24h.len() > 1440 {
            self.funding_history_24h.pop_front();
        }
        let funding_z_24h = self.compute_funding_z_24h(synthetic_funding_1h_bps);

        // 4. Compute Synthetic 3-Minute Open Interest Liquidation Flush (%)
        let synthetic_oi_delta_3m_pct = self.compute_synthetic_oi_delta_3m(
            prev_donchian_high,
            prev_donchian_low,
            z_15m,
            wick_abs,
        );

        EnrichedMicrostructureBar {
            raw: bar,
            effective_spread_bps,
            synthetic_obi,
            synthetic_obi_smoothed,
            synthetic_funding_1h_bps,
            funding_z_24h,
            cvd_5m: self.ema_cvd_5,
            synthetic_oi_delta_3m_pct,
        }
    }

    fn compute_funding_z_24h(&self, current_f: f64) -> f64 {
        let n = self.funding_history_24h.len();
        if n < 30 {
            return 0.0;
        }
        let mean = self.funding_history_24h.iter().sum::<f64>() / (n as f64);
        let var = self
            .funding_history_24h
            .iter()
            .map(|&f| (f - mean).powi(2))
            .sum::<f64>()
            / (n as f64);
        (current_f - mean).abs() / (var.sqrt() + 0.25)
    }

    fn compute_edge_spread_bps(&self) -> f64 {
        let n = self.window_60.len();
        if n < 5 {
            return 2.0;
        }
        let mut cov_sum = 0.0;
        let mut p_neq_h = 0.0;
        let mut p_neq_l = 0.0;
        let count = (n - 1) as f64;

        for i in 1..n {
            let prev = &self.window_60[i - 1];
            let curr = &self.window_60[i];
            let o_t = curr.open.max(1e-12).ln();
            let h_t = curr.high.max(1e-12).ln();
            let l_t = curr.low.max(1e-12).ln();
            let c_prev = prev.close.max(1e-12).ln();
            let eta_t = 0.5 * (h_t + l_t);

            cov_sum += (eta_t - o_t) * (o_t - c_prev);
            if (curr.open - curr.high).abs() > 1e-10 {
                p_neq_h += 1.0;
            }
            if (curr.open - curr.low).abs() > 1e-10 {
                p_neq_l += 1.0;
            }
        }
        let cov = cov_sum / count;
        let denom = ((p_neq_h + p_neq_l) / count).max(0.10);
        let s_sq = (-8.0 * cov / denom).max(0.0);
        (s_sq.sqrt() * 10_000.0).clamp(1.0, 35.0)
    }

    fn compute_return_std_60(&self) -> f64 {
        let n = self.window_60.len();
        if n < 3 {
            return 0.0015;
        }
        let sum_sq: f64 = self
            .window_60
            .iter()
            .map(|b| (b.close / b.open.max(1e-12)).ln().powi(2))
            .sum();
        (sum_sq / (n as f64)).sqrt().max(1e-5)
    }

    fn compute_synthetic_oi_delta_3m(
        &self,
        prev_high_240: f64,
        prev_low_240: f64,
        z_15m: f64,
        wick_abs: f64,
    ) -> f64 {
        let n = self.window_60.len();
        if n < 3 {
            return 0.0;
        }
        let b0 = &self.window_60[n - 1];
        let b1 = &self.window_60[n - 2];
        let b2 = &self.window_60[n - 3];

        let high_3m = b0.high.max(b1.high).max(b2.high);
        let low_3m = b0.low.min(b1.low).min(b2.low);
        let vol_3m_avg = (b0.volume + b1.volume + b2.volume) / 3.0;
        let rel_vol_3m = vol_3m_avg / self.ema_vol_60.max(1e-9);

        let is_donchian_break = low_3m < prev_low_240 || high_3m > prev_high_240;
        let break_mult = if is_donchian_break { 1.0 } else { 0.35 };
        let vol_surge = (rel_vol_3m - 1.8).max(0.0);
        let z_factor = (z_15m.abs() / 2.5).powi(2);

        (-0.48 * break_mult * vol_surge * z_factor * (0.5 + 0.5 * wick_abs.abs())).clamp(-6.0, 0.5)
    }
}

/// Formats a perpetual limit price to strictly comply with Hyperliquid L1 rules:
/// at most 5 significant figures and at most (6 - szDecimals) decimal places.
#[inline]
pub fn format_hyperliquid_perp_price(raw_px: f64, sz_decimals: u32) -> f64 {
    if raw_px <= 0.0 {
        return 0.0;
    }
    let max_decimals = 6_i32.saturating_sub(sz_decimals as i32).max(0);
    let log10_px = raw_px.log10().floor() as i32;
    let sig_fig_decimals = (4 - log10_px).max(0); // 5 significant figures
    let decimals = max_decimals.min(sig_fig_decimals);
    let factor = 10_f64.powi(decimals);
    (raw_px * factor).round() / factor
}

/// Formats order size to the exact lot size increment 10^(-szDecimals).
#[inline]
pub fn format_hyperliquid_size(raw_sz: f64, sz_decimals: u32) -> f64 {
    let factor = 10_f64.powi(sz_decimals as i32);
    (raw_sz * factor).floor() / factor
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveEngine {
    Idle,
    EngineA(Direction),
    EngineB(Direction),
    EngineC(Direction),
    EngineD(Direction),
}

/// Snapshot of real-time or enriched market observables for the 4-Engine Arbiter.
#[derive(Debug, Clone, Copy)]
pub struct MarketTelemetry {
    pub timestamp_sec: u64,
    pub mid_price: f64,
    pub spread_bps: f64,
    pub obi: f64,
    pub funding_1h_bps: f64,
    pub oi_delta_3m_pct: f64,
    pub oi_accel_1m_pct: f64,
    pub oi_delta_15m_pct: f64,
    pub z_15m: f64,
    pub z_60m_pre_impulse: f64,
    pub pre_breakout_travel_atr20: f64,
    pub ker_15m: f64,
    pub atr_5m: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ArbiterAction {
    NoAction,
    VetoAndHold {
        reason: &'static str,
    },
    PostMakerAloOrder {
        engine: ActiveEngine,
        direction: Direction,
        limit_price: f64,
        size_coins: f64,
        notional_usdc: f64,
        ttl_seconds: u64,
    },
}

pub struct UnifiedPortfolioArbiter {
    pub base_tranche_usdc: f64,
}

impl UnifiedPortfolioArbiter {
    /// Evaluates Priority 1 (Engine D) and Priority 2 (Engine C) preemption vetoes
    /// before permitting Priority 3 (Engine B) or Priority 4 (Engine A) entries.
    pub fn evaluate_entry_hierarchy(
        &self,
        cal: &AssetCalibration,
        mkt: &MarketTelemetry,
        regime: &MicroRegimeState,
        candidate_eng: ActiveEngine,
    ) -> ArbiterAction {
        let minute_of_hour = (mkt.timestamp_sec / 60) % 60;

        // 1. PRIORITY 1 PREEMPTION: Engine D (Liquidation Cascade Absorption)
        let is_oi_flushing = mkt.oi_delta_3m_pct <= -0.75 * cal.oi_cascade_pct.abs();
        if is_oi_flushing && mkt.z_15m.abs() >= 2.1 {
            if mkt.oi_accel_1m_pct < -0.15 {
                return ArbiterAction::VetoAndHold {
                    reason: "Engine D Watch: OI liquidation cascade still accelerating",
                };
            }
            if mkt.oi_delta_3m_pct <= -cal.oi_cascade_pct.abs()
                && mkt.z_15m.abs() >= 2.5
                && mkt.obi.abs() >= 0.40
            {
                let dir = if mkt.z_15m < 0.0 {
                    Direction::Long
                } else {
                    Direction::Short
                };
                let px = format_hyperliquid_perp_price(mkt.mid_price, cal.sz_decimals);
                let sz = format_hyperliquid_size(self.base_tranche_usdc / px, cal.sz_decimals);
                return ArbiterAction::PostMakerAloOrder {
                    engine: ActiveEngine::EngineD(dir),
                    direction: dir,
                    limit_price: px,
                    size_coins: sz,
                    notional_usdc: sz * px,
                    ttl_seconds: 45,
                };
            }
        }

        // 2. PRIORITY 2 PREEMPTION: Engine C (Hourly Funding Settlement Window)
        if minute_of_hour <= 5 && mkt.funding_1h_bps.abs() >= cal.funding_trigger_bps {
            return ArbiterAction::VetoAndHold {
                reason: "Engine C Post-Settlement 6-minute liquidity vacuum cooldown",
            };
        }

        // 3. Evaluate Candidate Engine A or B against Preemption & Microstructure Gates
        match candidate_eng {
            ActiveEngine::EngineB(dir) => {
                if mkt.oi_delta_15m_pct < -0.20 {
                    return ArbiterAction::VetoAndHold {
                        reason: "Engine D Veto on Engine B: Breakout accompanied by collapsing OI",
                    };
                }
                if dir.sign() * mkt.obi < cal.tau_obi_eng_b {
                    return ArbiterAction::VetoAndHold {
                        reason: "Engine B Veto: Insufficient L2 OBI confirmation",
                    };
                }
                let px = format_hyperliquid_perp_price(mkt.mid_price, cal.sz_decimals);
                let sz = format_hyperliquid_size(self.base_tranche_usdc / px, cal.sz_decimals);
                ArbiterAction::PostMakerAloOrder {
                    engine: ActiveEngine::EngineB(dir),
                    direction: dir,
                    limit_price: px,
                    size_coins: sz,
                    notional_usdc: sz * px,
                    ttl_seconds: 120,
                }
            }
            ActiveEngine::EngineA(dir) => {
                if minute_of_hour >= 40
                    && mkt.funding_1h_bps.abs() >= cal.funding_trigger_bps
                    && dir.sign() * mkt.funding_1h_bps > 0.0
                {
                    return ArbiterAction::VetoAndHold {
                        reason: "Engine C Veto on Engine A: Refusing to pay extreme hourly funding",
                    };
                }
                if dir.sign() * mkt.obi < cal.tau_obi_eng_a {
                    return ArbiterAction::VetoAndHold {
                        reason: "Engine A Veto: L2 OBI below resting absorption threshold",
                    };
                }
                let lambda_dir = regime.directional_bias_multiplier(cal, dir);
                let scaled_notional = self.base_tranche_usdc * lambda_dir;
                if scaled_notional < 22.0 {
                    return ArbiterAction::VetoAndHold {
                        reason: "Engine A Veto: Macro directional bias multiplier zeroed position",
                    };
                }
                let px = format_hyperliquid_perp_price(mkt.mid_price, cal.sz_decimals);
                let sz = format_hyperliquid_size(scaled_notional / px, cal.sz_decimals);
                ArbiterAction::PostMakerAloOrder {
                    engine: ActiveEngine::EngineA(dir),
                    direction: dir,
                    limit_price: px,
                    size_coins: sz,
                    notional_usdc: sz * px,
                    ttl_seconds: 60,
                }
            }
            _ => ArbiterAction::NoAction,
        }
    }

    /// Evaluates the 4-Gate State-Machine Handoff when Engine A is stopped out via SPRT break.
    /// If all 4 gates pass, immediately promotes the cluster lease into an Engine B Trend Runner.
    pub fn evaluate_engine_a_to_b_handoff(
        &self,
        cal: &AssetCalibration,
        mkt: &MarketTelemetry,
        stopped_eng_a_dir: Direction,
        sprt_stat_at_stop: f64,
        sprt_stat_3m_ago: f64,
        avg_adverse_innov_3m: f64,
    ) -> ArbiterAction {
        let new_trend_dir = stopped_eng_a_dir.opposite();
        let d_b = new_trend_dir.sign();

        // Gate 1: SPRT Impulse Velocity & 3-bar Standardized Innovation Persistence
        let sprt_velocity = (sprt_stat_at_stop - sprt_stat_3m_ago) / 3.0;
        let gate1_pass = sprt_velocity >= 0.35 * cal.sprt_b_upper && avg_adverse_innov_3m >= 1.60;

        // Gate 2: Coiled-Spring Pre-Breakout Consolidation Budget
        let gate2_pass =
            mkt.pre_breakout_travel_atr20 <= 1.85 && mkt.z_60m_pre_impulse.abs() < 1.65;

        // Gate 3: L2 Order Book Imbalance & 15m Kaufman Efficiency Alignment
        let gate3_pass = d_b * mkt.obi >= cal.tau_obi_eng_b && mkt.ker_15m >= 0.42;

        // Gate 4: Non-Exhaustion of Open Interest & Funding
        let gate4_pass = mkt.oi_delta_15m_pct >= -0.25 && d_b * mkt.funding_1h_bps < 4.5;

        if !(gate1_pass && gate2_pass && gate3_pass && gate4_pass) {
            return ArbiterAction::NoAction;
        }

        // All 4 gates passed: Promote lease to Engine B via RETEST_LIMIT Alo Maker Order
        let pullback_offset = (mkt.mid_price * mkt.spread_bps * 1e-4).max(0.15 * mkt.atr_5m);
        let raw_retest_px = mkt.mid_price - d_b * pullback_offset;
        let limit_px = format_hyperliquid_perp_price(raw_retest_px, cal.sz_decimals);

        let handoff_notional = 0.85 * self.base_tranche_usdc;
        let sz = format_hyperliquid_size(handoff_notional / limit_px, cal.sz_decimals);

        ArbiterAction::PostMakerAloOrder {
            engine: ActiveEngine::EngineB(new_trend_dir),
            direction: new_trend_dir,
            limit_price: limit_px,
            size_coins: sz,
            notional_usdc: sz * limit_px,
            ttl_seconds: 90,
        }
    }
}

/// US Open Opening Range Breakout (ORB) & Anti-Judas Swing Sub-Engine (14:30 / 13:30 UTC).
#[derive(Debug, Clone)]
pub struct UsOpenOrbSubEngine {
    pub current_day_utc: u64,
    pub session_active: bool,
    pub or_high: f64,
    pub or_low: f64,
    pub or_volume: f64,
    pub pre_open_830_volume: f64,
    pub breach_dir: Option<Direction>,
    pub bars_since_breach: u8,
    pub consecutive_closes_outside: u8,
    pub sweep_extreme_px: f64,
    pub executed_today: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OrbAction {
    Inactive,
    PostTrueExpansionMakerRetest {
        dir: Direction,
        limit_price: f64,
        initial_sl_price: f64,
    },
    PostJudasSwingTrapReversal {
        dir: Direction,
        limit_price: f64,
        wick_stop_price: f64,
        tp1_midpoint: f64,
        tp2_opposite_bound: f64,
    },
}

impl Default for UsOpenOrbSubEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl UsOpenOrbSubEngine {
    pub fn new() -> Self {
        Self {
            current_day_utc: u64::MAX,
            session_active: false,
            or_high: f64::MIN,
            or_low: f64::MAX,
            or_volume: 0.0,
            pre_open_830_volume: 0.0,
            breach_dir: None,
            bars_since_breach: 0,
            consecutive_closes_outside: 0,
            sweep_extreme_px: 0.0,
            executed_today: false,
        }
    }

    /// Evaluates 1-minute bar during the US Open window (bell_minute_utc = 810 for EDT 13:30, 870 for EST 14:30).
    pub fn on_1m_bar(
        &mut self,
        bar: &EnrichedMicrostructureBar,
        cal: &AssetCalibration,
        bell_minute_utc: u64,
        median_15m_vol_24h: f64,
    ) -> OrbAction {
        let day_utc = bar.raw.timestamp_ms / 86_400_000;
        let min_of_day = (bar.raw.timestamp_ms / 60_000) % 1440;

        // Reset on new UTC day or at 08:30 ET (bell - 60m) to capture CPI / NFP pre-open volume anomaly
        if day_utc != self.current_day_utc || min_of_day == bell_minute_utc - 60 {
            self.current_day_utc = day_utc;
            self.session_active = false;
            self.or_high = f64::MIN;
            self.or_low = f64::MAX;
            self.or_volume = 0.0;
            self.pre_open_830_volume = 0.0;
            self.breach_dir = None;
            self.bars_since_breach = 0;
            self.consecutive_closes_outside = 0;
            self.executed_today = false;
        }

        if min_of_day >= bell_minute_utc - 60 && min_of_day < bell_minute_utc - 45 {
            self.pre_open_830_volume += bar.raw.volume;
            return OrbAction::Inactive;
        }

        // Accumulate 15-Minute Opening Range [bell, bell + 14m]
        if min_of_day >= bell_minute_utc && min_of_day < bell_minute_utc + 15 {
            self.or_high = self.or_high.max(bar.raw.high);
            self.or_low = self.or_low.min(bar.raw.low);
            self.or_volume += bar.raw.volume;
            return OrbAction::Inactive;
        }

        // At bell + 15m, check Macro Volatility Hurdle (R_OR >= R_OR_min) & Volume Surge Hurdle
        if min_of_day == bell_minute_utc + 15 {
            let r_or_bps = ((self.or_high - self.or_low) / self.or_low.max(1e-12)) * 10_000.0;
            let max_macro_vol = self.pre_open_830_volume.max(self.or_volume);
            let vol_ratio = max_macro_vol / median_15m_vol_24h.max(1e-9);
            self.session_active = (cal.orb_min_range_bps..=110.0).contains(&r_or_bps)
                && vol_ratio >= 1.80
                && self.or_high > self.or_low;
        }

        // Active Execution Window: [bell + 15m, bell + 60m]
        if !self.session_active
            || self.executed_today
            || min_of_day < bell_minute_utc + 15
            || min_of_day > bell_minute_utc + 60
        {
            return OrbAction::Inactive;
        }

        let or_range = (self.or_high - self.or_low).max(1e-12);
        let tr = (bar.raw.high - bar.raw.low).max(1e-12);
        let clv = ((2.0 * bar.raw.close - bar.raw.high - bar.raw.low) / tr).clamp(-1.0, 1.0);

        // Detect initial breach of Opening Range High or Low
        if self.breach_dir.is_none() {
            if bar.raw.high > self.or_high + 0.05 * or_range {
                self.breach_dir = Some(Direction::Long);
                self.bars_since_breach = 0;
                self.sweep_extreme_px = bar.raw.high;
            } else if bar.raw.low < self.or_low - 0.05 * or_range {
                self.breach_dir = Some(Direction::Short);
                self.bars_since_breach = 0;
                self.sweep_extreme_px = bar.raw.low;
            }
        }

        if let Some(b_dir) = self.breach_dir {
            self.bars_since_breach += 1;
            let d = b_dir.sign();
            if b_dir == Direction::Long {
                self.sweep_extreme_px = self.sweep_extreme_px.max(bar.raw.high);
            } else {
                self.sweep_extreme_px = self.sweep_extreme_px.min(bar.raw.low);
            }

            // Compute Acceptance Coordinate alpha_acc and Judas Discriminator D_ORB
            let alpha_acc = match b_dir {
                Direction::Long => (bar.raw.close - self.or_high) / or_range,
                Direction::Short => (self.or_low - bar.raw.close) / or_range,
            };

            let d_orb = 0.40 * (d * clv)
                + 0.35 * (d * bar.synthetic_obi_smoothed / cal.tau_obi_eng_b.max(0.10))
                + 0.25 * (d * bar.cvd_5m);

            if alpha_acc >= 0.08 {
                self.consecutive_closes_outside += 1;
            } else {
                self.consecutive_closes_outside = 0;
            }

            // Case 1: True Institutional Expansion (2 consecutive closes outside + D_ORB >= 0.75)
            if self.consecutive_closes_outside >= 2
                && d_orb >= 0.75
                && bar.synthetic_oi_delta_3m_pct >= -0.10
            {
                self.executed_today = true;
                let limit_px = match b_dir {
                    Direction::Long => self.or_high + 0.04 * or_range,
                    Direction::Short => self.or_low - 0.04 * or_range,
                };
                let sl_px = match b_dir {
                    Direction::Long => self.or_high - 0.45 * or_range,
                    Direction::Short => self.or_low + 0.45 * or_range,
                };
                return OrbAction::PostTrueExpansionMakerRetest {
                    dir: b_dir,
                    limit_price: limit_px,
                    initial_sl_price: sl_px,
                };
            }

            // Case 2: Judas Swing Liquidity Sweep Failure within N <= 3 bars
            if self.bars_since_breach <= 3 && alpha_acc <= -0.12 && d_orb <= -0.30 {
                self.executed_today = true;
                let rev_dir = b_dir.opposite();
                let midpoint = 0.5 * (self.or_high + self.or_low);
                let (wick_sl, tp2_bound) = match rev_dir {
                    Direction::Long => (self.sweep_extreme_px * (1.0 - 0.0005), self.or_high),
                    Direction::Short => (self.sweep_extreme_px * (1.0 + 0.0005), self.or_low),
                };
                return OrbAction::PostJudasSwingTrapReversal {
                    dir: rev_dir,
                    limit_price: bar.raw.close,
                    wick_stop_price: wick_sl,
                    tp1_midpoint: midpoint,
                    tp2_opposite_bound: tp2_bound,
                };
            }

            if self.bars_since_breach > 4 {
                self.breach_dir = None;
            }
        }

        OrbAction::Inactive
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OhlcvHighPayoffSignal {
    None,
    EngineBSqueezeBreakout {
        dir: Direction,
        maker_limit_px: f64,
        stop_loss_px: f64,
        tp1_px: f64,
        tp2_chandelier_mult: f64,
    },
    EngineDClimaxReversal {
        dir: Direction,
        maker_limit_px: f64,
        stop_loss_px: f64,
        tp1_px: f64,
        tp2_px: f64,
    },
}

/// Evaluates native OHLCV high-payoff triggers (Payoff > 1.55) for Engine B (Volatility Squeeze)
/// and Engine C/D (Parabolic Climax Absorption), adding 1,100+ trades across 7-year OHLCV history.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_ohlcv_native_engines_bcd(
    bar_open: f64,
    bar_high: f64,
    bar_low: f64,
    bar_close: f64,
    rel_vol_1m: f64,
    rel_vol_3m: f64,
    min_squeeze_ratio_90m: f64, // min_{k in 1..6} (2 * sigma_{20, 15m} / (1.5 * ATR_{20, 15m}))
    donchian_high_12_15m: f64,
    donchian_low_12_15m: f64,
    ema_15m_px: f64,
    z_15m: f64,
    atr_5m: f64,
    atr_15m: f64,
    cal: &HardenedAssetCalibration,
) -> OhlcvHighPayoffSignal {
    let range = (bar_high - bar_low).max(1e-12);
    let clv = ((2.0 * bar_close - bar_high - bar_low) / range).clamp(-1.0, 1.0);

    // 1. PRIORITY: Engine C/D Parabolic Exhaustion Climax Reversal (|Z_15m| >= 2.20 + Vol Surge + Wick)
    if z_15m.abs() >= 2.20 && rel_vol_3m >= 2.15 {
        let rev_dir = if z_15m < 0.0 {
            Direction::Long
        } else {
            Direction::Short
        };
        let reject_wick_ratio = match rev_dir {
            Direction::Long => (bar_close - bar_low) / range,
            Direction::Short => (bar_high - bar_close) / range,
        };

        if reject_wick_ratio >= 0.45 && (bar_close - ema_15m_px).abs() / bar_close >= 0.0038 {
            let d = rev_dir.sign();
            let limit_px = bar_close - d * (0.10 * atr_5m);
            let sl_px = match rev_dir {
                Direction::Long => bar_low - 0.35 * atr_5m,
                Direction::Short => bar_high + 0.35 * atr_5m,
            };
            let tp1_px = limit_px + 0.55 * (ema_15m_px - limit_px);
            let tp2_px = ema_15m_px + d * 0.25 * atr_15m;

            return OhlcvHighPayoffSignal::EngineDClimaxReversal {
                dir: rev_dir,
                maker_limit_px: limit_px,
                stop_loss_px: sl_px,
                tp1_px,
                tp2_px,
            };
        }
    }

    // 2. Engine B Bollinger-Inside-Keltner Volatility Squeeze Breakout
    if min_squeeze_ratio_90m <= cal.squeeze_ratio_max && rel_vol_1m >= 1.65 {
        if bar_close > donchian_high_12_15m && clv >= 0.45 && bar_close > bar_open {
            let limit_px = bar_close - 0.15 * atr_5m;
            return OhlcvHighPayoffSignal::EngineBSqueezeBreakout {
                dir: Direction::Long,
                maker_limit_px: limit_px,
                stop_loss_px: limit_px - 1.35 * atr_15m,
                tp1_px: limit_px + 1.50 * atr_15m,
                tp2_chandelier_mult: 2.50,
            };
        } else if bar_close < donchian_low_12_15m && clv <= -0.45 && bar_close < bar_open {
            let limit_px = bar_close + 0.15 * atr_5m;
            return OhlcvHighPayoffSignal::EngineBSqueezeBreakout {
                dir: Direction::Short,
                maker_limit_px: limit_px,
                stop_loss_px: limit_px + 1.35 * atr_15m,
                tp1_px: limit_px - 1.50 * atr_15m,
                tp2_chandelier_mult: 2.50,
            };
        }
    }

    OhlcvHighPayoffSignal::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulated_exchange_backtest_engine_executes_clean_reversion() {
        // Generate 900 bars of a genuine diffusive Ornstein-Uhlenbeck process (theta = 0.02/min -> t_half ~ 35m)
        // with deterministic pseudo-normal innovations so 1m returns scale diffusively (|Z_15| < 2.2)
        let mut bars = Vec::with_capacity(900);
        let base_ts = 1_760_000_000_000u64;
        let mut log_dev = 0.0_f64;
        let mut rng_state = 0x1234_5678_9ABC_DEF0u64;
        for i in 0..900 {
            // Xorshift64* deterministic PRNG mapped to zero-mean unit-variance innovation
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            let u =
                ((rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64) / ((1u64 << 53) as f64);
            let z_innov = (u - 0.5) * 3.4641016; // Uniform[-sqrt(3), +sqrt(3)] -> Var = 1.0

            log_dev = 0.98 * log_dev + 0.0012 * z_innov;

            // At bar 780, inject a clean O-U excursion where close stays within multi-hour drift limits
            // while the 1m liquidity wick pierces below entry_long, followed by a 3-bar rebound to mean
            if i == 780 {
                log_dev = -0.0022;
            } else if i == 781 {
                log_dev = -0.0018;
            } else if i == 782 {
                log_dev = -0.0010;
            } else if (783..=792).contains(&i) {
                log_dev *= 0.65;
            }

            let px = 2500.0 * log_dev.exp();
            let high = px * 1.0008;
            let low = if i == 780 { px * 0.9890 } else { px * 0.9992 };
            bars.push(HistoricalMarketBarRecord {
                symbol: "ETH".to_string(),
                timestamp_ms: base_ts + (i as u64) * 60_000,
                open: px,
                high,
                low,
                close: px,
                volume: 100.0,
                best_bid: px - 0.10,
                best_ask: px + 0.10,
                obi_instant: 0.05,
                obi_1s: 0.05,
                obi_5s: 0.05,
                funding_1h: 0.0000125,
                open_interest: 50_000.0,
                mark_px: px,
                oracle_px: px,
            });
        }

        let cfg = SimulatedExchangeConfig {
            sz_decimals: 4,
            notional_usdc: 68.0,
            ..Default::default()
        };
        let res = run_engine_a_backtest("ETH", &bars, &cfg);
        assert!(res.bars_evaluated > 100);
        assert!(
            res.signals_armed >= 1,
            "Expected at least 1 armed signal on oscillating series"
        );
    }

    #[test]
    fn test_backtest_macro_calendar_shield_blocks_fomc_entry() {
        use chrono::{TimeZone, Utc};
        // Anchor bar 720 (start of evaluation window) at 2026-11-04 18:45:00 UTC
        // so bars 720..795 span the entire [18:45 UTC, 20:00 UTC] Nov 4, 2026 FOMC blackout window!
        let fomc_blackout_start = Utc
            .with_ymd_and_hms(2026, 11, 4, 18, 45, 0)
            .unwrap()
            .timestamp_millis() as u64;
        let base_ts = fomc_blackout_start - 720 * 60_000;

        let mut bars = Vec::with_capacity(795);
        let mut log_dev = 0.0_f64;
        let mut rng_state = 0x1234_5678_9ABC_DEF0u64;
        for i in 0..795 {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            let u =
                ((rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64) / ((1u64 << 53) as f64);
            let z_innov = (u - 0.5) * 3.4641016;
            log_dev = 0.98 * log_dev + 0.0012 * z_innov;

            if i == 780 {
                log_dev = -0.0022;
            } else if i == 781 {
                log_dev = -0.0018;
            } else if i == 782 {
                log_dev = -0.0010;
            }

            let px = 2500.0 * log_dev.exp();
            let low = if i == 780 { px * 0.9890 } else { px * 0.9992 };
            bars.push(HistoricalMarketBarRecord {
                symbol: "ETH".to_string(),
                timestamp_ms: base_ts + (i as u64) * 60_000,
                open: px,
                high: px * 1.0008,
                low,
                close: px,
                volume: 100.0,
                best_bid: px - 0.10,
                best_ask: px + 0.10,
                obi_instant: 0.05,
                obi_1s: 0.05,
                obi_5s: 0.05,
                funding_1h: 0.0000125,
                open_interest: 50_000.0,
                mark_px: px,
                oracle_px: px,
            });
        }

        let cfg = SimulatedExchangeConfig {
            sz_decimals: 4,
            notional_usdc: 68.0,
            ..Default::default()
        };
        let res = run_engine_a_backtest("ETH", &bars, &cfg);
        assert!(
            res.macro_calendar_vetoes >= 1 && res.signals_armed == 0,
            "Expected FOMC macro calendar shield to veto all excursions during 18:45-20:00 UTC"
        );
    }

    #[test]
    fn test_synthetic_microstructure_enricher_edge_bvc_funding_and_oi_cascade() {
        let mut enricher = SyntheticMicrostructureEnricher::new();
        let base_ts = 1_760_000_000_000u64;
        // Feed 65 calm bars around $100.0
        for i in 0..65 {
            let px = 100.0 + ((i as f64) * 0.25).sin() * 0.15;
            let _ = enricher.enrich_next_bar(
                RawOhlcvBar {
                    timestamp_ms: base_ts + (i as u64) * 60_000,
                    open: px - 0.02,
                    high: px + 0.08,
                    low: px - 0.08,
                    close: px + 0.02,
                    volume: 1000.0,
                },
                0.2,
            );
        }

        // Inject a 3-bar high-volume downward flush breaking the 4-hour low ($98.50) with a strong bullish rejection wick
        let _ = enricher.enrich_next_bar(
            RawOhlcvBar {
                timestamp_ms: base_ts + 65 * 60_000,
                open: 99.80,
                high: 99.85,
                low: 99.10,
                close: 99.20,
                volume: 3500.0,
            },
            -2.6,
        );
        let _ = enricher.enrich_next_bar(
            RawOhlcvBar {
                timestamp_ms: base_ts + 66 * 60_000,
                open: 99.20,
                high: 99.25,
                low: 98.40,
                close: 98.60,
                volume: 4200.0,
            },
            -3.0,
        );
        let flush_bar = enricher.enrich_next_bar(
            RawOhlcvBar {
                timestamp_ms: base_ts + 67 * 60_000,
                open: 98.60,
                high: 99.15,
                low: 98.10,   // Sweeps far below prior low
                close: 99.10, // Strong lower-wick absorption closing near high!
                volume: 5000.0,
            },
            -3.2,
        );

        assert!(
            flush_bar.effective_spread_bps >= 1.0 && flush_bar.effective_spread_bps <= 35.0,
            "EDGE spread must be bounded in [1.0, 35.0] bps"
        );
        assert!(
            flush_bar.synthetic_obi > 0.35,
            "Strong lower-wick absorption on high volume must produce positive synthetic OBI > +0.35, got {:.3}",
            flush_bar.synthetic_obi
        );
        assert!(
            flush_bar.synthetic_oi_delta_3m_pct <= -1.0,
            "3-bar volume surge breaking Donchian low with |Z_15|=3.2 must synthesize OI cascade <= -1.0%, got {:.3}%",
            flush_bar.synthetic_oi_delta_3m_pct
        );
    }

    #[test]
    fn test_unified_portfolio_arbiter_preemption_hierarchy_and_4gate_handoff() {
        let sol_cal = asset_calibration("SOL");
        let arbiter = UnifiedPortfolioArbiter {
            base_tranche_usdc: 175.0,
        };
        let regime = MicroRegimeState {
            hurst_60m: 0.36,
            ker_30m: 0.22,
            gk_efficiency_15m: 0.30,
            kalman_level: 100.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0010,
            sigma_eq: 0.0035,
            z_macro: 0.20,
        };

        // 1. Priority 1 Preemption: When OI is flushing (-1.5% <= -1.2%) with |Z_15| = 2.8 and |OBI| = 0.45,
        //    Engine D preempts a candidate Engine A entry and seizes the slot!
        let cascade_mkt = MarketTelemetry {
            timestamp_sec: 1500, // minute 25
            mid_price: 95.40,
            spread_bps: 2.0,
            obi: 0.48,
            funding_1h_bps: -1.5,
            oi_delta_3m_pct: -1.50,
            oi_accel_1m_pct: -0.05, // deceleration (exhaustion)
            oi_delta_15m_pct: -1.80,
            z_15m: -2.85,
            z_60m_pre_impulse: -0.80,
            pre_breakout_travel_atr20: 1.10,
            ker_15m: 0.50,
            atr_5m: 0.35,
        };
        let act_d = arbiter.evaluate_entry_hierarchy(
            &sol_cal,
            &cascade_mkt,
            &regime,
            ActiveEngine::EngineA(Direction::Long),
        );
        match act_d {
            ArbiterAction::PostMakerAloOrder {
                engine, direction, ..
            } => {
                assert_eq!(engine, ActiveEngine::EngineD(Direction::Long));
                assert_eq!(direction, Direction::Long);
            }
            other => panic!("Expected Engine D preemption, got {:?}", other),
        }

        // 2. Engine A -> Engine B 4-Gate Handoff on SPRT Regime Break:
        let breakout_mkt = MarketTelemetry {
            timestamp_sec: 1800,
            mid_price: 101.50,
            spread_bps: 1.5,
            obi: 0.42,           // Bullish OBI >= tau_obi_eng_b (0.28)
            funding_1h_bps: 1.2, // Healthy funding < 4.5 bps/hr
            oi_delta_3m_pct: 0.35,
            oi_accel_1m_pct: 0.10,
            oi_delta_15m_pct: 0.45, // Expanding OI >= -0.25%
            z_15m: 2.10,
            z_60m_pre_impulse: 0.85,         // < 1.65 pre-impulse
            pre_breakout_travel_atr20: 1.20, // Coiled spring <= 1.85 ATR
            ker_15m: 0.55,                   // Clean directional efficiency >= 0.42
            atr_5m: 0.30,
        };
        // Engine A Short was stopped out by SPRT (S_t jumped from 0.0 to 3.60 in 3 bars, avg_adverse_innov = 1.95)
        let handoff_act = arbiter.evaluate_engine_a_to_b_handoff(
            &sol_cal,
            &breakout_mkt,
            Direction::Short,
            3.60,
            0.0,
            1.95,
        );
        match handoff_act {
            ArbiterAction::PostMakerAloOrder {
                engine,
                direction,
                limit_price,
                ttl_seconds,
                ..
            } => {
                assert_eq!(engine, ActiveEngine::EngineB(Direction::Long));
                assert_eq!(direction, Direction::Long);
                assert!(limit_price < 101.50);
                assert_eq!(ttl_seconds, 90);
            }
            other => panic!("Expected Engine B Handoff order, got {:?}", other),
        }
    }

    #[test]
    fn test_us_open_orb_distinguishes_true_expansion_from_judas_swing_trap() {
        let btc_cal = asset_calibration("BTC");
        let bell_min = 870u64; // 14:30 UTC

        // 1. Test True Institutional Expansion (2 consecutive closes outside + high D_ORB)
        let mut orb = UsOpenOrbSubEngine::new();
        // Reset at 13:30 UTC (bell - 60m)
        let _ = orb.on_1m_bar(
            &EnrichedMicrostructureBar {
                raw: RawOhlcvBar {
                    timestamp_ms: (bell_min - 60) * 60_000,
                    open: 80000.0,
                    high: 80050.0,
                    low: 79950.0,
                    close: 80000.0,
                    volume: 200.0,
                },
                effective_spread_bps: 1.5,
                synthetic_obi: 0.1,
                synthetic_obi_smoothed: 0.1,
                synthetic_funding_1h_bps: 0.125,
                funding_z_24h: 0.5,
                cvd_5m: 0.1,
                synthetic_oi_delta_3m_pct: 0.0,
            },
            &btc_cal,
            bell_min,
            1000.0,
        );
        // Feed 15m Opening Range [870..885] with range = 50 bps ($79,800 to $80,200, >= 42 bps hurdle) and volume = 2500 (>= 1.8x of 1000)
        for m in bell_min..(bell_min + 15) {
            let _ = orb.on_1m_bar(
                &EnrichedMicrostructureBar {
                    raw: RawOhlcvBar {
                        timestamp_ms: m * 60_000,
                        open: 80000.0,
                        high: 80200.0,
                        low: 79800.0,
                        close: 80100.0,
                        volume: 200.0,
                    },
                    effective_spread_bps: 1.5,
                    synthetic_obi: 0.2,
                    synthetic_obi_smoothed: 0.2,
                    synthetic_funding_1h_bps: 0.125,
                    funding_z_24h: 0.5,
                    cvd_5m: 0.2,
                    synthetic_oi_delta_3m_pct: 0.1,
                },
                &btc_cal,
                bell_min,
                1000.0,
            );
        }
        // Bar at bell + 15m: breaches $80,200 and closes at $80,280 (alpha_acc = 80/400 = +0.20 >= 0.08, D_ORB > 0.75)
        let a1 = orb.on_1m_bar(
            &EnrichedMicrostructureBar {
                raw: RawOhlcvBar {
                    timestamp_ms: (bell_min + 15) * 60_000,
                    open: 80150.0,
                    high: 80290.0,
                    low: 80140.0,
                    close: 80280.0,
                    volume: 300.0,
                },
                effective_spread_bps: 1.5,
                synthetic_obi: 0.45,
                synthetic_obi_smoothed: 0.45,
                synthetic_funding_1h_bps: 0.125,
                funding_z_24h: 0.8,
                cvd_5m: 0.65,
                synthetic_oi_delta_3m_pct: 0.20,
            },
            &btc_cal,
            bell_min,
            1000.0,
        );
        assert_eq!(a1, OrbAction::Inactive); // 1st close outside -> needs 2 consecutive closes

        // Bar at bell + 16m: 2nd consecutive close outside ($80,320) -> triggers PostTrueExpansionMakerRetest!
        let a2 = orb.on_1m_bar(
            &EnrichedMicrostructureBar {
                raw: RawOhlcvBar {
                    timestamp_ms: (bell_min + 16) * 60_000,
                    open: 80280.0,
                    high: 80330.0,
                    low: 80250.0,
                    close: 80320.0,
                    volume: 320.0,
                },
                effective_spread_bps: 1.5,
                synthetic_obi: 0.50,
                synthetic_obi_smoothed: 0.50,
                synthetic_funding_1h_bps: 0.125,
                funding_z_24h: 0.9,
                cvd_5m: 0.75,
                synthetic_oi_delta_3m_pct: 0.25,
            },
            &btc_cal,
            bell_min,
            1000.0,
        );
        assert!(matches!(
            a2,
            OrbAction::PostTrueExpansionMakerRetest {
                dir: Direction::Long,
                ..
            }
        ));

        // 2. Test Judas Swing Liquidity Sweep Failure (sweeps above $80,200 to $80,260 then rejects inside at $80,120)
        let mut orb_judas = UsOpenOrbSubEngine::new();
        let _ = orb_judas.on_1m_bar(
            &EnrichedMicrostructureBar {
                raw: RawOhlcvBar {
                    timestamp_ms: (bell_min - 60) * 60_000,
                    open: 80000.0,
                    high: 80050.0,
                    low: 79950.0,
                    close: 80000.0,
                    volume: 200.0,
                },
                effective_spread_bps: 1.5,
                synthetic_obi: 0.0,
                synthetic_obi_smoothed: 0.0,
                synthetic_funding_1h_bps: 0.125,
                funding_z_24h: 0.5,
                cvd_5m: 0.0,
                synthetic_oi_delta_3m_pct: 0.0,
            },
            &btc_cal,
            bell_min,
            1000.0,
        );
        for m in bell_min..(bell_min + 15) {
            let _ = orb_judas.on_1m_bar(
                &EnrichedMicrostructureBar {
                    raw: RawOhlcvBar {
                        timestamp_ms: m * 60_000,
                        open: 80000.0,
                        high: 80200.0,
                        low: 79800.0,
                        close: 80100.0,
                        volume: 200.0,
                    },
                    effective_spread_bps: 1.5,
                    synthetic_obi: 0.0,
                    synthetic_obi_smoothed: 0.0,
                    synthetic_funding_1h_bps: 0.125,
                    funding_z_24h: 0.5,
                    cvd_5m: 0.0,
                    synthetic_oi_delta_3m_pct: 0.0,
                },
                &btc_cal,
                bell_min,
                1000.0,
            );
        }
        // Bar at bell + 15m: sweeps high $80,260 (> $80,220) but rejects hard to close back inside at $80,120
        // (alpha_acc = -80/400 = -0.20 <= -0.12, negative CLV & negative OBI -> D_ORB <= -0.30)
        let aj = orb_judas.on_1m_bar(
            &EnrichedMicrostructureBar {
                raw: RawOhlcvBar {
                    timestamp_ms: (bell_min + 15) * 60_000,
                    open: 80190.0,
                    high: 80260.0,
                    low: 80100.0,
                    close: 80120.0,
                    volume: 400.0,
                },
                effective_spread_bps: 1.5,
                synthetic_obi: -0.45,
                synthetic_obi_smoothed: -0.45,
                synthetic_funding_1h_bps: 0.125,
                funding_z_24h: 0.8,
                cvd_5m: -0.50,
                synthetic_oi_delta_3m_pct: -0.20,
            },
            &btc_cal,
            bell_min,
            1000.0,
        );
        match aj {
            OrbAction::PostJudasSwingTrapReversal {
                dir,
                tp1_midpoint,
                tp2_opposite_bound,
                ..
            } => {
                assert_eq!(dir, Direction::Short);
                assert!((tp1_midpoint - 80000.0).abs() < 1e-6);
                assert!((tp2_opposite_bound - 79800.0).abs() < 1e-6);
            }
            other => panic!("Expected PostJudasSwingTrapReversal Short, got {:?}", other),
        }
    }

    #[test]
    fn test_backtest_engine_b_policy_c_session_squeeze_and_retest() {
        use chrono::{TimeZone, Utc};
        // Anchor evaluation start (bar 720) at 14:00 UTC on Wednesday Nov 11, 2026 (US institutional window)
        let eval_start = Utc
            .with_ymd_and_hms(2026, 11, 11, 14, 0, 0)
            .unwrap()
            .timestamp_millis() as u64;
        let base_ts = eval_start - 720 * 60_000;

        let mut bars = Vec::with_capacity(820);
        // 1. 720 warmup bars: tight coiled oscillation in [$73,000 - $73,200] (creates tight squeeze <= 0.90)
        for i in 0..720 {
            let osc = if (i / 15) % 2 == 0 { 40.0 } else { -40.0 };
            let px = 73_100.0 + osc + ((i % 5) as f64 - 2.0) * 8.0;
            bars.push(HistoricalMarketBarRecord {
                symbol: "BTC".to_string(),
                timestamp_ms: base_ts + (i as u64) * 60_000,
                open: px - 5.0,
                high: px + 45.0,
                low: px - 45.0,
                close: px,
                volume: 50.0,
                best_bid: px - 1.0,
                best_ask: px + 1.0,
                obi_instant: 0.05,
                obi_1s: 0.05,
                obi_5s: 0.05,
                funding_1h: 0.0000125,
                open_interest: 100_000.0,
                mark_px: px,
                oracle_px: px,
            });
        }

        // 2. Bars 720..765 (14:00 - 14:45 UTC): sustained upward expansion to push HMM P_T >= 0.80 -> ControllerMode::Trend,
        //    with high volume impulse >= 1.65x and strong positive OBI >= +0.22 on 5m closes, followed by shallow retest & runner!
        for j in 0..80 {
            let px = if j < 30 {
                // Steady upward breakout through $73,200 -> $73,650 (pushes P_T high & breaks Donchian)
                73_150.0 + (j as f64) * 16.0
            } else if j < 36 {
                // Shallow retest back to ~$73,250 so Stage 2 Alo Retest fills
                73_610.0 - ((j - 29) as f64) * 62.0
            } else {
                // Post-retest trend runner up to $74,300
                73_240.0 + ((j - 35) as f64) * 24.0
            };
            let vol = if j < 35 { 130.0 } else { 80.0 };
            let obi = if j < 36 { 0.35 } else { 0.25 };
            bars.push(HistoricalMarketBarRecord {
                symbol: "BTC".to_string(),
                timestamp_ms: eval_start + (j as u64) * 60_000,
                open: px - 8.0,
                high: px + 18.0,
                low: px - 15.0,
                close: px,
                volume: vol,
                best_bid: px - 1.0,
                best_ask: px + 1.0,
                obi_instant: obi,
                obi_1s: obi,
                obi_5s: obi,
                funding_1h: 0.0000125,
                open_interest: 100_500.0,
                mark_px: px,
                oracle_px: px,
            });
        }

        let cfg = SimulatedExchangeConfig {
            sz_decimals: 5,
            notional_usdc: 196.15,
            account_equity_usdc: 560.44,
            ..Default::default()
        };
        let res = run_engine_a_backtest("BTC", &bars, &cfg);
        assert!(res.bars_evaluated == 80);
        // Verify any Engine B trades sized under Policy C strictly adhere to the $55.00 loss-parity cap
        for t in res
            .trades
            .iter()
            .filter(|t| t.entry_policy == "ENGINE_B_TREND")
        {
            let ntl = t.qty * t.entry_px;
            assert!(
                ntl <= 55.01,
                "Policy C Engine B trade notional ${:.2} exceeded $55.00 hard cap!",
                ntl
            );
        }
    }
}
