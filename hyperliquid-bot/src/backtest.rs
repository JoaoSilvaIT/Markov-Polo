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
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use crate::controller::{ControllerMode, RegimeController};
use crate::hmm::{compute_hmm_posteriors, train_hmm_offline, GaussianHMM};
use crate::hyperliquid::{effective_tick_size, optimal_engine_b_tp_atr_multiple, quantize_price};
use crate::ou_process::{
    asset_ou_config, calculate_directional_edge_bps, calculate_features_with_funding_at,
    calculate_mad_sigma, check_macro_calendar_veto, compute_adaptive_tp_offset_bps,
    compute_fee_adjusted_kelly_scalar, evaluate_armed_reversal_1m_step,
    evaluate_engine_c_hourly_squeeze, evaluate_engine_d_cascade_reversal, InvalidationDecision,
    OrderPlacementAction, TwoTierInvalidationManager,
};
use crate::state::{ClusterCircuitBreaker, Side, DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC};
use crate::telemetry::{evaluate_btc_impulse_veto, BtcImpulseSnapshot};
use crate::trend::{
    calculate_atr, calculate_donchian, check_trend_alignment, evaluate_pre_breakout_budget,
    CandleBar, ChandelierExitState, MarketCrowdingTracker, TrendAlignment,
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

/// Loads and deduplicates all recorded bars from `hl_history_{symbol}.jsonl`, sorted by `timestamp_ms`.
pub fn load_historical_bars(symbol: &str) -> Vec<HistoricalMarketBarRecord> {
    let file_path = format!("hl_history_{}.jsonl", symbol.to_lowercase());
    if !Path::new(&file_path).exists() {
        return Vec::new();
    }
    let file = match File::open(&file_path) {
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
            maker_fee_bps: 1.5,
            taker_fee_bps: 4.5,
            stop_slippage_bps: 2.5,
            strict_trade_through_fills: true,
            trade_through_ticks: 1.0,
            bar_adaptive_high_low_ordering: true,
            notional_usdc: 60.0,
            account_equity_usdc: 142.50,
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
struct OpenSimulatedPosition {
    side: Side,
    entry_px: f64,
    qty: f64,
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
    let warmup = 720usize.min(bars.len().saturating_sub(30));
    let mut closes_history: Vec<f64> = bars[..warmup].iter().map(|b| b.close).collect();

    // Pre-aggregate 15m bars from warmup history for Dual-Engine (Engine B & HMM Controller)
    let mut rolling_15m_bars: Vec<CandleBar> = Vec::with_capacity(120);
    let mut current_15m_bar: Option<CandleBar> = None;
    let mut all_15m_closes: Vec<f64> = Vec::with_capacity(1024);
    for b in &bars[..warmup] {
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

    let mut cumulative_pnl_usd = 0.0;
    let mut peak_pnl_usd = 0.0;
    let mut gross_wins_usd = 0.0;
    let mut gross_losses_usd = 0.0;
    let mut cluster_cb = ClusterCircuitBreaker::default();

    let (max_drift_60, max_drift_120, max_slow_drift_180, max_slow_drift_240) =
        match symbol.to_uppercase().as_str() {
            "BTC" => (0.0035, 0.0025, 0.0025, 0.0030),
            "ETH" => (0.0045, 0.0030, 0.0030, 0.0035),
            "XRP" | "LINK" => (0.0050, 0.0042, 0.0042, 0.0050),
            "SOL" | "HYPE" => (0.0052, 0.0045, 0.0045, 0.0055),
            "AVAX" | "SUI" => (0.0065, 0.0050, 0.0060, 0.0075),
            "NEAR" => (0.0055, 0.0045, 0.0060, 0.0075),
            _ => (0.0045, 0.0030, 0.0030, 0.0035),
        };
    let (max_short_drift_60, max_short_drift_120, max_short_drift_240) =
        match symbol.to_uppercase().as_str() {
            "BTC" => (0.0085, 0.0075, 0.0065),
            "ETH" => (0.0095, 0.0100, 0.0080),
            "XRP" | "LINK" => (0.0110, 0.0125, 0.0095),
            "SOL" | "HYPE" => (0.0120, 0.0135, 0.0100),
            "AVAX" | "SUI" => (0.0135, 0.0160, 0.0140),
            "NEAR" => (0.0140, 0.0160, 0.0140),
            _ => (0.0095, 0.0100, 0.0080),
        };

    for idx in warmup..bars.len() {
        summary.bars_evaluated += 1;
        let bar = &bars[idx];
        let tick_sz = effective_tick_size(bar.close, sim_cfg.sz_decimals);
        let fill_pad = if sim_cfg.strict_trade_through_fills {
            tick_sz * sim_cfg.trade_through_ticks.max(0.0)
        } else {
            0.0
        };

        // Update 15m bar aggregation & crowding tracker
        crowding_tracker.update(
            bar.funding_1h,
            bar.open_interest,
            bar.mark_px,
            bar.oracle_px,
        );
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

        // ------------------------------------------------------------------
        // 1. MATCHING ENGINE: Process Resting Post-Only (Alo) Entry Order
        // ------------------------------------------------------------------
        if let Some(entry_ord) = resting_entry.take() {
            let age_bars = idx.saturating_sub(entry_ord.submitted_bar_idx);
            let filled = match entry_ord.side {
                Side::Long => bar.low <= entry_ord.limit_px - fill_pad,
                Side::Short => bar.high >= entry_ord.limit_px + fill_pad,
            };

            if filled {
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
                open_pos = Some(OpenSimulatedPosition {
                    side: entry_ord.side.clone(),
                    entry_px: entry_ord.limit_px,
                    qty: entry_ord.qty,
                    entry_time_ms: bar.timestamp_ms,
                    initial_tp_px: entry_ord.target_tp_px,
                    tp_limit_px: entry_ord.target_tp_px,
                    hard_stop_px: entry_ord.hard_stop_px,
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
                        entry_ord.side,
                        entry_ord.limit_px,
                        ou_cfg.hard_disaster_stop_bps,
                        ou_cfg.soft_z_ou_threshold,
                        ou_cfg.soft_z15_shock_threshold,
                        entry_ord.half_life_m,
                    ),
                    policy: entry_ord.policy,
                    is_engine_b: entry_ord.is_engine_b,
                    chandelier_opt,
                    entry_atr: entry_ord.entry_atr,
                });
            } else if age_bars >= entry_ord.ttl_bars {
                summary.orders_expired_unfilled += 1;
            } else {
                // Fast-Sentinel BTC Impulse & Macro Calendar Abort on resting unfilled limit order
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
                if macro_abort.is_some() {
                    summary.macro_calendar_vetoes += 1;
                    summary.orders_expired_unfilled += 1;
                } else if btc_abort.is_some() {
                    summary.btc_impulse_vetoes += 1;
                    summary.orders_expired_unfilled += 1;
                } else {
                    resting_entry = Some(entry_ord);
                }
            }
        }

        // ------------------------------------------------------------------
        // 2. MATCHING ENGINE: Process Open Position (Stops, TPs, Funding, 5m Soft Invalidation)
        // ------------------------------------------------------------------
        let live_feat_opt = calculate_features_with_funding_at(
            &closes_history,
            bar.close,
            symbol,
            bar.funding_1h,
            Some(bar.timestamp_ms),
        );

        // Update 3-Mode HMM & Regime Controller on 1m shocks and finalized 15m bar closes
        if let Some(ref feat) = live_feat_opt {
            let is_controller_shock_1m = feat.is_downside_shock
                || (feat.is_upside_shock && (current_p_s >= 0.50 || feat.z_15.abs() >= 3.5));
            if is_controller_shock_1m {
                controller.force_shock_cooldown(feat.z_15, feat.z_60, bar.timestamp_ms);
            }
            if let Some(ref closed_15m) = finalized_15m_bar {
                if all_15m_closes.len() >= 140 && all_15m_closes.len() % 96 == 0 {
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
            if (bar.timestamp_ms / 60_000) % 60 == 0 && bar.funding_1h.abs() > 1e-9 {
                let oracle_ref = if bar.oracle_px > 0.0 {
                    bar.oracle_px
                } else {
                    bar.close
                };
                let signed_carry = match pos.side {
                    Side::Long => -bar.funding_1h,
                    Side::Short => bar.funding_1h,
                };
                // Positive accrued_funding_usd means cost paid; negative means credit received
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

            // Adaptive intrabar ordering (Nautilus `bar_adaptive_high_low_ordering`)
            let check_tp_first = if sim_cfg.bar_adaptive_high_low_ordering {
                match pos.side {
                    Side::Long => bar.close < bar.open,
                    Side::Short => bar.close > bar.open,
                }
            } else {
                true
            };

            let mut closed_outcome: Option<(f64, bool, &'static str)> = None;
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

                if tp_hit && chandelier_hit {
                    if check_tp_first {
                        closed_outcome = Some((pos.tp_limit_px, true, "MAKER_TP"));
                    } else {
                        let slip = match pos.side {
                            Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                            Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                        };
                        closed_outcome = Some((pos.hard_stop_px * slip, false, stop_label));
                        controller.notify_trend_stopped_out();
                    }
                } else if chandelier_hit {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((pos.hard_stop_px * slip, false, stop_label));
                    controller.notify_trend_stopped_out();
                } else if tp_hit {
                    closed_outcome = Some((pos.tp_limit_px, true, "MAKER_TP"));
                }

                if closed_outcome.is_none() {
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
                        }
                    }
                }
                if closed_outcome.is_none() && elapsed_mins_1m >= 75.0 {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((bar.close * slip, false, "TIME_DECAY_STOP"));
                }
            } else {
                if tp_hit && hard_sl_hit {
                    if check_tp_first {
                        closed_outcome = Some((pos.tp_limit_px, true, "MAKER_TP"));
                    } else {
                        let slip = match pos.side {
                            Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                            Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                        };
                        closed_outcome = Some((pos.hard_stop_px * slip, false, "DISASTER_STOP"));
                    }
                } else if hard_sl_hit {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((pos.hard_stop_px * slip, false, "DISASTER_STOP"));
                } else if tp_hit {
                    closed_outcome = Some((pos.tp_limit_px, true, "MAKER_TP"));
                }

                // Evaluate 1m Time-Decay Max Hold Stop (matches `src/main.rs` 1m check)
                let max_hold_1m = (3.5 * pos.inv_mgr.half_life_minutes).clamp(90.0, 360.0);
                if closed_outcome.is_none() && elapsed_mins_1m >= max_hold_1m {
                    let slip = match pos.side {
                        Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                        Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                    };
                    closed_outcome = Some((bar.close * slip, false, "TIME_DECAY_STOP"));
                }

                // Evaluate 5m Two-Tier Soft Structural Invalidation on completed 5-minute bars
                if closed_outcome.is_none() && (bar.timestamp_ms / 60_000) % 5 == 4 {
                    if let Some(ref feat) = live_feat_opt {
                        let raw_frozen_z_ou =
                            if pos.frozen_center_px > 0.0 && pos.frozen_sigma_eq_log > 1e-6 {
                                (bar.close / pos.frozen_center_px).ln() / pos.frozen_sigma_eq_log
                            } else {
                                feat.z_ou
                            };
                        let adverse_bps_from_entry = match pos.side {
                            Side::Long => {
                                ((pos.entry_px - bar.close) / pos.entry_px * 1e4).max(0.0)
                            }
                            Side::Short => {
                                ((bar.close - pos.entry_px) / pos.entry_px * 1e4).max(0.0)
                            }
                        };
                        let min_soft_inv_excursion_bps = match symbol.to_uppercase().as_str() {
                            "BTC" => 45.0,
                            "ETH" => 50.0,
                            "XRP" | "LINK" => 58.0,
                            "SOL" | "HYPE" => 60.0,
                            "AVAX" | "SUI" => 62.0,
                            "NEAR" => 68.0,
                            _ => 50.0,
                        };
                        let (frozen_z_ou, eff_z_15) =
                            if adverse_bps_from_entry >= min_soft_inv_excursion_bps {
                                (raw_frozen_z_ou, feat.z_15)
                            } else {
                                (0.0, 0.0)
                            };
                        let kalman_against = match pos.side {
                            Side::Long => {
                                (feat.kalman_drift_veto && feat.kalman_slope < 0.0)
                                    || feat.cusum_neg >= 8.0
                                    || adverse_bps_from_entry >= min_soft_inv_excursion_bps
                            }
                            Side::Short => {
                                (feat.kalman_drift_veto && feat.kalman_slope > 0.0)
                                    || feat.cusum_pos >= 8.0
                                    || adverse_bps_from_entry >= min_soft_inv_excursion_bps
                            }
                        };
                        let elapsed_mins =
                            bar.timestamp_ms.saturating_sub(pos.entry_time_ms) as f64 / 60_000.0;
                        pos.inv_mgr.elapsed_minutes = (elapsed_mins - 5.0).max(0.0);
                        match pos.inv_mgr.on_5m_bar_close(
                            frozen_z_ou,
                            eff_z_15,
                            kalman_against,
                            sim_cfg.maker_fee_bps,
                            tick_sz,
                        ) {
                            InvalidationDecision::TriggerSoftInvalidationFlatten { reason } => {
                                let slip = match pos.side {
                                    Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                    Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                                };
                                closed_outcome = Some((bar.close * slip, false, reason));
                            }
                            InvalidationDecision::TriggerTimeDecayExit => {
                                let slip = match pos.side {
                                    Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                    Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                                };
                                closed_outcome = Some((bar.close * slip, false, "TIME_DECAY_STOP"));
                            }
                            InvalidationDecision::HoldPosition {
                                updated_tp_price: Some(be_px),
                            } => match pos.side {
                                Side::Long if be_px < pos.tp_limit_px && be_px > bar.close => {
                                    pos.tp_limit_px = be_px;
                                }
                                Side::Short if be_px > pos.tp_limit_px && be_px < bar.close => {
                                    pos.tp_limit_px = be_px;
                                }
                                _ => {}
                            },
                            InvalidationDecision::HoldPosition {
                                updated_tp_price: None,
                            } => {}
                        }
                    }
                }

                // Normalized Progress & 20% Giveback Ratchet + One-Way Adaptive Ratchet (`OU Exit Management and Maker Execution Review.md`)
                if closed_outcome.is_none() {
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
                        let fav_close_bps = match pos.side {
                            Side::Long => {
                                ((bar.close - pos.entry_px) / pos.entry_px * 1e4).max(0.0)
                            }
                            Side::Short => {
                                ((pos.entry_px - bar.close) / pos.entry_px * 1e4).max(0.0)
                            }
                        };
                        let abs_arm_bps = 1.8 * ou_cfg.min_net_edge_bps;
                        let arm_q_thresh =
                            (abs_arm_bps / (d_0 / pos.entry_px * 1e4).max(1e-6)).min(0.75);
                        let _ = fav_close_bps;
                        if pos.max_progress >= arm_q_thresh
                            && q_t <= (pos.max_progress - 0.20).max(0.15)
                        {
                            let slip = match pos.side {
                                Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                                Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                            };
                            closed_outcome = Some((bar.close * slip, false, "GIVEBACK_RATCHET"));
                        } else if let Some(ref feat) = live_feat_opt {
                            if let Some(ref th) = feat.thresholds_opt {
                                let entry_tick =
                                    effective_tick_size(pos.entry_px, sim_cfg.sz_decimals);
                                let delta_reprice = (pos.entry_px * 0.0005)
                                    .max(0.10 * d_0)
                                    .max(3.0 * entry_tick);
                                let cooldown_ok =
                                    bar.timestamp_ms.saturating_sub(pos.last_reprice_time_ms)
                                        >= 180_000;
                                match pos.side {
                                    Side::Long => {
                                        let live_mu = th.exit_long;
                                        let mu_tick =
                                            effective_tick_size(live_mu, sim_cfg.sz_decimals);
                                        let eps_shave = (live_mu * 0.0005).max(2.0 * mu_tick);
                                        let b_cand = crate::hyperliquid::quantize_price(
                                            live_mu - eps_shave,
                                            sim_cfg.sz_decimals,
                                        );
                                        let p_min = pos.entry_px * 1.0020;
                                        if b_cand <= pos.tp_limit_px - delta_reprice
                                            && b_cand >= p_min
                                            && b_cand > bar.close
                                        {
                                            pos.candidate_lower_count += 1;
                                            if pos.candidate_lower_count >= 3
                                                && cooldown_ok
                                                && pos.reprice_count < 3
                                            {
                                                pos.tp_limit_px = b_cand;
                                                pos.last_reprice_time_ms = bar.timestamp_ms;
                                                pos.reprice_count += 1;
                                                pos.candidate_lower_count = 0;
                                            }
                                        } else if b_cand > pos.tp_limit_px {
                                            pos.candidate_lower_count = 0;
                                        }
                                    }
                                    Side::Short => {
                                        let live_mu = th.exit_short;
                                        let mu_tick =
                                            effective_tick_size(live_mu, sim_cfg.sz_decimals);
                                        let eps_shave = (live_mu * 0.0005).max(2.0 * mu_tick);
                                        let b_cand = crate::hyperliquid::quantize_price(
                                            live_mu + eps_shave,
                                            sim_cfg.sz_decimals,
                                        );
                                        let p_max = pos.entry_px * 0.9980;
                                        if b_cand >= pos.tp_limit_px + delta_reprice
                                            && b_cand <= p_max
                                            && b_cand < bar.close
                                        {
                                            pos.candidate_lower_count += 1;
                                            if pos.candidate_lower_count >= 3
                                                && cooldown_ok
                                                && pos.reprice_count < 3
                                            {
                                                pos.tp_limit_px = b_cand;
                                                pos.last_reprice_time_ms = bar.timestamp_ms;
                                                pos.reprice_count += 1;
                                                pos.candidate_lower_count = 0;
                                            }
                                        } else if b_cand < pos.tp_limit_px {
                                            pos.candidate_lower_count = 0;
                                        }
                                    }
                                }
                            }
                        }
                    }
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
                let total_fees_usd = pos.entry_fee_usd + exit_fee_usd;
                let raw_pnl_usd = match pos.side {
                    Side::Long => pos.qty * (exit_px - pos.entry_px),
                    Side::Short => pos.qty * (pos.entry_px - exit_px),
                };
                let net_pnl_usd = raw_pnl_usd - total_fees_usd - pos.accrued_funding_usd;
                let entry_notional = (pos.qty * pos.entry_px).max(1e-9);
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

                summary.trades.push(SimulatedTradeResult {
                    symbol: symbol.to_uppercase(),
                    side: pos.side,
                    entry_time_ms: pos.entry_time_ms,
                    exit_time_ms: bar.timestamp_ms,
                    entry_px: pos.entry_px,
                    exit_px,
                    qty: pos.qty,
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
            } else {
                open_pos = Some(pos);
            }
        }

        // ------------------------------------------------------------------
        // 3. DUAL-ENGINE EVALUATION: Engine B (15m Donchian Trend) & Engine A (5m O-U Mean Reversion)
        // ------------------------------------------------------------------
        if open_pos.is_none() && resting_entry.is_none() {
            if let Some(ref feat) = live_feat_opt {
                let macro_veto = check_macro_calendar_veto(bar.timestamp_ms);
                let cb_veto = cluster_cb.evaluate_equity_drawdown(
                    bar.timestamp_ms,
                    sim_cfg.account_equity_usdc + cumulative_pnl_usd,
                    DEFAULT_MAX_DAILY_CLUSTER_DD_FRAC,
                );

                // 3A. ENGINE B: Evaluate on completed 15m candle close when Controller is in Trend mode
                if !is_armed && controller.current_mode == ControllerMode::Trend {
                    if let Some(ref closed_15m) = finalized_15m_bar {
                        let prior_15m = if rolling_15m_bars.len() > 1 {
                            &rolling_15m_bars[..rolling_15m_bars.len() - 1]
                        } else {
                            &rolling_15m_bars[..]
                        };
                        if let Some(ref donchian) = calculate_donchian(prior_15m, donchian_period) {
                            let closes_15m: Vec<f64> =
                                rolling_15m_bars.iter().map(|b| b.close).collect();
                            let alignment = check_trend_alignment(&closes_15m);
                            let rng_15m = (closed_15m.high - closed_15m.low).max(1e-9);
                            let is_15m_bull_close = closed_15m.close > closed_15m.open
                                && (closed_15m.close - closed_15m.low) / rng_15m >= 0.65;
                            let is_15m_bear_close = closed_15m.close < closed_15m.open
                                && (closed_15m.high - closed_15m.close) / rng_15m >= 0.65;
                            let is_long_breakout = is_15m_bull_close
                                && !feat.is_upside_shock
                                && feat.z_60 < 2.5
                                && feat.z_15 < 3.0
                                && bar.close > donchian.upper
                                && bar.close >= bar.open
                                && alignment == TrendAlignment::Bullish;
                            let is_short_breakout = is_15m_bear_close
                                && !feat.is_downside_shock
                                && feat.z_60 > -2.5
                                && feat.z_15 > -3.0
                                && bar.close < donchian.lower
                                && bar.close <= bar.open
                                && alignment == TrendAlignment::Bearish;

                            if is_long_breakout || is_short_breakout {
                                if macro_veto.is_some() {
                                    summary.macro_calendar_vetoes += 1;
                                } else if cb_veto.is_some() {
                                    summary.cluster_cb_vetoes += 1;
                                } else if let Some(current_atr) =
                                    calculate_atr(&rolling_15m_bars, 20).filter(|&a| a > 0.0)
                                {
                                    let side = if is_long_breakout {
                                        Side::Long
                                    } else {
                                        Side::Short
                                    };
                                    let is_long = side == Side::Long;
                                    let obi_ok = if is_long {
                                        bar.obi_instant >= -0.18
                                            && bar.obi_1s >= -0.15
                                            && bar.obi_5s >= -0.12
                                    } else {
                                        bar.obi_instant <= 0.18
                                            && bar.obi_1s <= 0.15
                                            && bar.obi_5s <= 0.12
                                    };
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
                                    let budget_verdict = evaluate_pre_breakout_budget(
                                        symbol,
                                        &rolling_15m_bars,
                                        donchian,
                                        current_atr,
                                        bar.close,
                                        feat.z_60,
                                        &side,
                                    );
                                    if btc_veto.is_some() {
                                        summary.btc_impulse_vetoes += 1;
                                    } else if obi_ok
                                        && crowding_mult >= 0.45
                                        && budget_verdict.approved
                                    {
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
                                            if spread >= 2.0 * tick_sz - 1e-9
                                                && eff_bid + tick_sz < eff_ask - 1e-9
                                            {
                                                eff_bid + tick_sz
                                            } else {
                                                eff_bid
                                            }
                                        } else if spread >= 2.0 * tick_sz - 1e-9
                                            && eff_ask - tick_sz > eff_bid + 1e-9
                                        {
                                            eff_ask - tick_sz
                                        } else {
                                            eff_ask
                                        };
                                        let limit_px =
                                            quantize_price(raw_limit, sim_cfg.sz_decimals);
                                        let initial_sl = quantize_price(
                                            if is_long {
                                                limit_px - chandelier_atr_mult * current_atr
                                            } else {
                                                limit_px + chandelier_atr_mult * current_atr
                                            },
                                            sim_cfg.sz_decimals,
                                        );
                                        let k_tp = optimal_engine_b_tp_atr_multiple(symbol);
                                        let tp_dist = (k_tp * current_atr).max(limit_px * 0.0045);
                                        let tp_px = quantize_price(
                                            if is_long {
                                                limit_px + tp_dist
                                            } else {
                                                (limit_px - tp_dist).max(tick_sz)
                                            },
                                            sim_cfg.sz_decimals,
                                        );
                                        let max_b_notional = ou_cfg
                                            .volatility_adjusted_trend_notional_cap(
                                                sim_cfg.account_equity_usdc,
                                                limit_px,
                                                current_atr,
                                            );
                                        let notional =
                                            crate::hyperliquid::calculate_risk_scaled_notional(
                                                sim_cfg.account_equity_usdc,
                                                (sim_cfg.notional_usdc * crowding_mult)
                                                    .min(max_b_notional),
                                                0.0060 * crowding_mult,
                                                limit_px,
                                                initial_sl,
                                                3,
                                                2,
                                                11.0,
                                                max_b_notional,
                                            );
                                        let qty = notional / limit_px.max(1e-9);
                                        summary.signals_armed += 1;
                                        summary.orders_submitted += 1;
                                        resting_entry = Some(RestingMakerEntry {
                                            side,
                                            limit_px,
                                            qty,
                                            target_tp_px: tp_px,
                                            hard_stop_px: initial_sl,
                                            frozen_center_px: tp_px,
                                            frozen_sigma_eq_log: feat.sigma_eq_log,
                                            half_life_m: 45.0,
                                            submitted_bar_idx: idx,
                                            ttl_bars: 5,
                                            policy: "ENGINE_B_TREND",
                                            is_engine_b: true,
                                            entry_atr: current_atr,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }

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
                    && feat.z_60 > -1.90
                    && !(ret_60 <= -max_drift_60 && ret_120 <= 0.0015)
                    && ret_120 > -max_drift_120
                    && ret_180 > -max_slow_drift_180
                    && ret_240 > -max_slow_drift_240
                    && (ma60_vs_240 >= -0.0008
                        || symbol.eq_ignore_ascii_case("BTC") && ma60_vs_240 >= -0.0022);
                let kalman_short_ok = !kalman_waterfall_up
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
                        Side::Long => feat.is_downside_shock || kalman_waterfall_down,
                        Side::Short => feat.is_upside_shock || kalman_waterfall_up,
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
                            Side::Long => (bar.funding_1h * 1e4) * (arm_half_life / 60.0),
                            Side::Short => (-bar.funding_1h * 1e4) * (arm_half_life / 60.0),
                        };
                        let max_arm_bars = ((0.5 * arm_half_life).round() as usize).clamp(10, 15);

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
                            ou_cfg.min_net_edge_bps,
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

                                if btc_veto.is_some() {
                                    summary.btc_impulse_vetoes += 1;
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

                                    if net_edge_bps >= ou_cfg.min_net_edge_bps {
                                        if let Some(off_bps) = compute_adaptive_tp_offset_bps(
                                            arm_exit_px,
                                            limit_price,
                                            tick_sz,
                                            arm_half_life,
                                        ) {
                                            let shave = arm_exit_px * (off_bps * 1e-4);
                                            let tp_px = if is_long {
                                                arm_exit_px - shave
                                            } else {
                                                arm_exit_px + shave
                                            };
                                            let hard_stop_frac =
                                                ou_cfg.hard_disaster_stop_bps * 1e-4;
                                            let sl_dist = (2.0 * (tp_px - limit_price).abs())
                                                .max(limit_price * hard_stop_frac);
                                            let hard_sl_px = if is_long {
                                                limit_price - sl_dist
                                            } else {
                                                limit_price + sl_dist
                                            };
                                            let vol_adj_cap = ou_cfg
                                                .volatility_adjusted_notional_cap(
                                                    sim_cfg.account_equity_usdc,
                                                    arm_sigma_eq_log,
                                                );
                                            let kelly_scalar = compute_fee_adjusted_kelly_scalar(
                                                raw_edge_bps,
                                                ou_cfg.min_net_edge_bps,
                                                sim_cfg.maker_fee_bps,
                                                funding_drag_bps,
                                            );
                                            let notional =
                                                crate::hyperliquid::calculate_risk_scaled_notional(
                                                    sim_cfg.account_equity_usdc,
                                                    (sim_cfg.notional_usdc * kelly_scalar)
                                                        .min(vol_adj_cap),
                                                    0.0075 * kelly_scalar,
                                                    limit_price,
                                                    hard_sl_px,
                                                    3,
                                                    2,
                                                    11.0,
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
                                                target_tp_px: tp_px,
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

                // 3C. ENGINE C (`HourlySqueezeHarvester`) & ENGINE D (`CascadeAbsorptionReversal`)
                if !is_armed && resting_entry.is_none() && macro_veto.is_none() && cb_veto.is_none()
                {
                    let low_30m = bars[idx.saturating_sub(30)..=idx]
                        .iter()
                        .map(|b| b.low)
                        .fold(bar.low, f64::min);
                    let dlog_oi_3m =
                        if idx >= 3 && bars[idx - 3].open_interest > 0.0 && bar.open_interest > 0.0
                        {
                            (bar.open_interest / bars[idx - 3].open_interest).ln()
                        } else {
                            0.0
                        };
                    if let Some(sq) = evaluate_engine_c_hourly_squeeze(
                        bar.timestamp_ms,
                        bar.funding_1h,
                        bar.close,
                        bar.best_bid,
                        bar.oracle_px,
                        low_30m,
                        feat.sigma_eq_log,
                        ou_cfg.min_net_edge_bps,
                        feat.z_15,
                        feat.z_60,
                        tick_sz,
                    ) {
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
                            11.0,
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
                    } else if let Some(cas) = evaluate_engine_d_cascade_reversal(
                        bar.close,
                        bar.open,
                        bar.low,
                        bar.best_bid,
                        bar.oracle_px,
                        dlog_oi_3m,
                        feat.z_15,
                        bar.obi_instant,
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
                            11.0,
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
            }
        }

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
}
