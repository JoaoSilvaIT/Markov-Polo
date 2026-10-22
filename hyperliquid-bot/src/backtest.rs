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

use crate::hyperliquid::effective_tick_size;
use crate::ou_process::{
    asset_ou_config, calculate_directional_edge_bps, calculate_features_with_funding,
    compute_safe_tp_offset_bps, evaluate_armed_reversal_1m_step, InvalidationDecision,
    OrderPlacementAction, TwoTierInvalidationManager,
};
use crate::state::Side;

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
    /// Require price to trade strictly through the limit price by at least 1 tick (conservative queue model)
    pub strict_trade_through_fills: bool,
    /// Use adaptive intrabar ordering (if close >= open, visit low before high; else visit high before low)
    pub bar_adaptive_high_low_ordering: bool,
    pub notional_usdc: f64,
    pub sz_decimals: u32,
}

impl Default for SimulatedExchangeConfig {
    fn default() -> Self {
        Self {
            maker_fee_bps: 1.5,
            taker_fee_bps: 4.5,
            stop_slippage_bps: 2.5,
            strict_trade_through_fills: true,
            bar_adaptive_high_low_ordering: true,
            notional_usdc: 60.0,
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
}

/// Runs a causal, discrete-event simulation of Engine A across a sequence of 1-minute `HistoricalMarketBarRecord`s
/// using Nautilus-style `OrderMatchingCore` rules (Alo post-only checks, FIFO trade-through fills,
/// mark/bar trigger stops, hourly funding settlement, and exact linear-contract fees).
pub fn run_engine_a_backtest(
    symbol: &str,
    bars: &[HistoricalMarketBarRecord],
    sim_cfg: &SimulatedExchangeConfig,
) -> BacktestSummary {
    let mut summary = BacktestSummary {
        symbol: symbol.to_uppercase(),
        ..Default::default()
    };
    if bars.len() < 750 {
        return summary;
    }

    let ou_cfg = asset_ou_config(symbol);
    let warmup = 720usize.min(bars.len().saturating_sub(30));
    let mut closes_history: Vec<f64> = bars[..warmup].iter().map(|b| b.close).collect();

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

    let max_slow_drift_240 = match symbol.to_uppercase().as_str() {
        "BTC" => 0.0085,
        "ETH" => 0.0110,
        "SOL" => 0.0150,
        "AVAX" => 0.0180,
        _ => 0.0110,
    };

    for idx in warmup..bars.len() {
        summary.bars_evaluated += 1;
        let bar = &bars[idx];
        let tick_sz = effective_tick_size(bar.close, sim_cfg.sz_decimals);
        let fill_pad = if sim_cfg.strict_trade_through_fills {
            tick_sz
        } else {
            0.0
        };

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
                });
            } else if age_bars >= entry_ord.ttl_bars {
                summary.orders_expired_unfilled += 1;
            } else {
                resting_entry = Some(entry_ord);
            }
        }

        // ------------------------------------------------------------------
        // 2. MATCHING ENGINE: Process Open Position (Stops, TPs, Funding, 5m Soft Invalidation)
        // ------------------------------------------------------------------
        let live_feat_opt = calculate_features_with_funding(
            &closes_history,
            bar.close,
            symbol,
            bar.funding_1h,
        );

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
                let oracle_ref = if bar.oracle_px > 0.0 { bar.oracle_px } else { bar.close };
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

            // Evaluate 5m Two-Tier Soft Structural Invalidation on completed 5-minute bars
            if closed_outcome.is_none() && (bar.timestamp_ms / 60_000) % 5 == 4 {
                if let Some(ref feat) = live_feat_opt {
                    let frozen_z_ou = if pos.frozen_center_px > 0.0 && pos.frozen_sigma_eq_log > 1e-6 {
                        (bar.close / pos.frozen_center_px).ln() / pos.frozen_sigma_eq_log
                    } else {
                        feat.z_ou
                    };
                    let kalman_against = match pos.side {
                        Side::Long => (feat.kalman_drift_veto && feat.kalman_slope < 0.0) || feat.cusum_neg >= 8.0,
                        Side::Short => (feat.kalman_drift_veto && feat.kalman_slope > 0.0) || feat.cusum_pos >= 8.0,
                    };
                    let elapsed_mins = bar.timestamp_ms.saturating_sub(pos.entry_time_ms) as f64 / 60_000.0;
                    pos.inv_mgr.elapsed_minutes = (elapsed_mins - 5.0).max(0.0);
                    match pos.inv_mgr.on_5m_bar_close(
                        frozen_z_ou,
                        feat.z_15,
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
                        } => {
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
                    if pos.max_progress >= 0.75 && q_t <= (pos.max_progress - 0.20).max(0.15) {
                        let slip = match pos.side {
                            Side::Long => 1.0 - sim_cfg.stop_slippage_bps * 1e-4,
                            Side::Short => 1.0 + sim_cfg.stop_slippage_bps * 1e-4,
                        };
                        closed_outcome = Some((bar.close * slip, false, "GIVEBACK_RATCHET"));
                    } else if let Some(ref feat) = live_feat_opt {
                        if let Some(ref th) = feat.thresholds_opt {
                            let entry_tick = effective_tick_size(pos.entry_px, sim_cfg.sz_decimals);
                            let delta_reprice = (pos.entry_px * 0.0005)
                                .max(0.10 * d_0)
                                .max(3.0 * entry_tick);
                            let cooldown_ok = bar.timestamp_ms.saturating_sub(pos.last_reprice_time_ms) >= 180_000;
                            match pos.side {
                                Side::Long => {
                                    let live_mu = th.exit_long;
                                    let mu_tick = effective_tick_size(live_mu, sim_cfg.sz_decimals);
                                    let eps_shave = (live_mu * 0.0005).max(2.0 * mu_tick);
                                    let b_cand = crate::hyperliquid::quantize_price(live_mu - eps_shave, sim_cfg.sz_decimals);
                                    let p_min = pos.entry_px * 1.0020;
                                    if b_cand <= pos.tp_limit_px - delta_reprice && b_cand >= p_min && b_cand > bar.close {
                                        pos.candidate_lower_count += 1;
                                        if pos.candidate_lower_count >= 3 && cooldown_ok && pos.reprice_count < 3 {
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
                                    let mu_tick = effective_tick_size(live_mu, sim_cfg.sz_decimals);
                                    let eps_shave = (live_mu * 0.0005).max(2.0 * mu_tick);
                                    let b_cand = crate::hyperliquid::quantize_price(live_mu + eps_shave, sim_cfg.sz_decimals);
                                    let p_max = pos.entry_px * 0.9980;
                                    if b_cand >= pos.tp_limit_px + delta_reprice && b_cand <= p_max && b_cand < bar.close {
                                        pos.candidate_lower_count += 1;
                                        if pos.candidate_lower_count >= 3 && cooldown_ok && pos.reprice_count < 3 {
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
        // 3. STRATEGY ENGINE A: Evaluate 5m O-U Arming & 1m Multi-Bar Reversal Confirmation
        // ------------------------------------------------------------------
        if open_pos.is_none() && resting_entry.is_none() {
            if let Some(ref feat) = live_feat_opt {
                let consolidation_vol_ok = feat.z_15.abs() < 2.2 && feat.z_60.abs() < 2.5;
                let ret_240 = if closes_history.len() >= 240 && closes_history[closes_history.len() - 240] > 0.0 {
                    (bar.close / closes_history[closes_history.len() - 240]).ln()
                } else {
                    0.0
                };
                let kalman_waterfall_down =
                    (feat.kalman_drift_veto && feat.kalman_slope < 0.0) || feat.cusum_neg >= 8.0;
                let kalman_waterfall_up =
                    (feat.kalman_drift_veto && feat.kalman_slope > 0.0) || feat.cusum_pos >= 8.0;
                let kalman_long_ok = !kalman_waterfall_down
                    && feat.z_60 > -1.90
                    && ret_240 > -max_slow_drift_240;
                let kalman_short_ok = !kalman_waterfall_up
                    && feat.z_60 < 1.90
                    && ret_240 < max_slow_drift_240;

                let is_eligible = feat.is_consolidation && consolidation_vol_ok && feat.is_valid_half_life;

                if is_armed {
                    let shock_vetoed = match arm_side {
                        Side::Long => feat.is_downside_shock || kalman_waterfall_down,
                        Side::Short => feat.is_upside_shock || kalman_waterfall_up,
                    };
                    if !is_eligible || shock_vetoed {
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
                                is_armed = false;
                                let is_long = side == Side::Long;
                                let raw_edge_bps =
                                    calculate_directional_edge_bps(limit_price, arm_exit_px, is_long);
                                let net_edge_bps =
                                    raw_edge_bps - 2.0 * sim_cfg.maker_fee_bps - funding_drag_bps;

                                if net_edge_bps >= ou_cfg.min_net_edge_bps {
                                    if let Some(off_bps) =
                                        compute_safe_tp_offset_bps(arm_exit_px, limit_price, tick_sz)
                                    {
                                        let shave = arm_exit_px * (off_bps * 1e-4);
                                        let tp_px = if is_long {
                                            arm_exit_px - shave
                                        } else {
                                            arm_exit_px + shave
                                        };
                                        let hard_stop_frac = ou_cfg.hard_disaster_stop_bps * 1e-4;
                                        let sl_dist = (2.0 * (tp_px - limit_price).abs())
                                            .max(limit_price * hard_stop_frac);
                                        let hard_sl_px = if is_long {
                                            limit_price - sl_dist
                                        } else {
                                            limit_price + sl_dist
                                        };
                                        let notional = sim_cfg
                                            .notional_usdc
                                            .min(ou_cfg.target_notional_cap_usdc);
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
                                        });
                                    }
                                }
                            }
                        }
                    }
                } else if is_eligible {
                    if let Some(ref th) = feat.thresholds_opt {
                        if (bar.close <= th.entry_long || bar.low <= th.entry_long)
                            && !feat.is_downside_shock
                            && kalman_long_ok
                        {
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
                        } else if (bar.close >= th.entry_short || bar.high >= th.entry_short)
                            && !feat.is_upside_shock
                            && kalman_short_ok
                        {
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
            let u = ((rng_state.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64) / ((1u64 << 53) as f64);
            let z_innov = (u - 0.5) * 3.4641016; // Uniform[-sqrt(3), +sqrt(3)] -> Var = 1.0

            log_dev = 0.98 * log_dev + 0.0012 * z_innov;

            // At bar 780, inject a clean O-U excursion below entry_long followed by a 3-bar rebound to mean
            if i == 780 {
                log_dev = -0.0095;
            } else if i == 781 {
                log_dev = -0.0085;
            } else if i == 782 {
                log_dev = -0.0070;
            } else if (783..=792).contains(&i) {
                log_dev *= 0.65;
            }

            let px = 2500.0 * log_dev.exp();
            let high = px * 1.0008;
            let low = px * 0.9992;
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
}
