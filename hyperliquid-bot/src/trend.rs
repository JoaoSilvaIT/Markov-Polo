use crate::state::{sync_parent_dir, FlockGuard, Side};
use chrono::{Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;

pub const SHADOW_TREND_JOURNAL: &str = "hl_shadow_trend_journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandleBar {
    pub time_open: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DonchianChannel {
    pub upper: f64,
    pub lower: f64,
    pub middle: f64,
}

/// Computes the Donchian Channel over the previous N closed bars (excluding the forming bar)
pub fn calculate_donchian(bars: &[CandleBar], period: usize) -> Option<DonchianChannel> {
    if bars.len() < period || period == 0 {
        return None;
    }

    let slice = &bars[bars.len() - period..];
    let mut upper = f64::NEG_INFINITY;
    let mut lower = f64::INFINITY;

    for b in slice {
        if b.high > upper {
            upper = b.high;
        }
        if b.low < lower {
            lower = b.low;
        }
    }

    if upper.is_infinite() || lower.is_infinite() {
        return None;
    }

    Some(DonchianChannel {
        upper,
        lower,
        middle: (upper + lower) / 2.0,
    })
}

/// Wilder 20-period Average True Range
pub fn calculate_atr(bars: &[CandleBar], period: usize) -> Option<f64> {
    if bars.len() <= period || period == 0 {
        return None;
    }

    let mut trs = Vec::with_capacity(bars.len() - 1);
    for i in 1..bars.len() {
        let prev_close = bars[i - 1].close;
        let high = bars[i].high;
        let low = bars[i].low;

        let tr = (high - low)
            .max((high - prev_close).abs())
            .max((low - prev_close).abs());
        trs.push(tr);
    }

    if trs.len() < period {
        return None;
    }

    // Seed ATR with simple average of first `period` True Ranges
    let mut atr: f64 = trs[0..period].iter().sum::<f64>() / period as f64;

    // Wilder smoothing for subsequent periods
    for tr in &trs[period..] {
        atr = ((period as f64 - 1.0) * atr + tr) / period as f64;
    }

    Some(atr)
}

/// Standard Exponential Moving Average (EMA)
pub fn calculate_ema(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period || period == 0 {
        return None;
    }

    let alpha = 2.0 / (period as f64 + 1.0);
    // Seed with SMA
    let mut ema: f64 = closes[0..period].iter().sum::<f64>() / period as f64;

    for &c in &closes[period..] {
        ema = c * alpha + ema * (1.0 - alpha);
    }

    Some(ema)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendAlignment {
    Bullish,
    Bearish,
    Neutral,
}

/// Evaluates EMA 16 vs EMA 64 directional filter
pub fn check_trend_alignment(closes: &[f64]) -> TrendAlignment {
    let ema_16 = calculate_ema(closes, 16);
    let ema_64 = calculate_ema(closes, 64);

    match (ema_16, ema_64) {
        (Some(fast), Some(slow)) => {
            if fast > slow {
                TrendAlignment::Bullish
            } else if fast < slow {
                TrendAlignment::Bearish
            } else {
                TrendAlignment::Neutral
            }
        }
        _ => TrendAlignment::Neutral,
    }
}

/// Computes Order Book Imbalance (OBI) across top K levels:
/// OBI = (Sum(Bid_Vlm) - Sum(Ask_Vlm)) / (Sum(Bid_Vlm) + Sum(Ask_Vlm))
/// Bounded in [-1.0, 1.0]
#[allow(dead_code)]
pub fn calculate_obi_score(bids: &[(f64, f64)], asks: &[(f64, f64)], depth: usize) -> f64 {
    let top_bids = &bids[0..depth.min(bids.len())];
    let top_asks = &asks[0..depth.min(asks.len())];

    let sum_bid_vlm: f64 = top_bids.iter().map(|&(_, sz)| sz).sum();
    let sum_ask_vlm: f64 = top_asks.iter().map(|&(_, sz)| sz).sum();

    let total = sum_bid_vlm + sum_ask_vlm;
    if total <= 1e-9 {
        return 0.0;
    }

    (sum_bid_vlm - sum_ask_vlm) / total
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BreakoutBudgetVerdict {
    pub approved: bool,
    pub is_coiled: bool,
    pub dcw_20: f64,
    pub bbw_atr_20: f64,
    pub overshoot_atr: f64,
    pub rmb_atr: f64,
    pub min_rmb_atr: f64,
    pub max_z60: f64,
}

/// Evaluates causal Track 1 Pre-Breakout Compression ("Coil") and Residual Move Budget (`RMB_t`)
/// calibrated from the 2024–2026 Hyperliquid L1 15-minute event study (`asset_thresholds.csv`).
#[allow(dead_code)]
pub fn evaluate_pre_breakout_budget(
    symbol: &str,
    bars_15m: &[CandleBar],
    donchian: &DonchianChannel,
    current_atr: f64,
    current_px: f64,
    z_60: f64,
    side: &Side,
) -> BreakoutBudgetVerdict {
    let atr = current_atr.max(1e-9);
    let dcw_20 = (donchian.upper - donchian.lower).max(0.0) / atr;

    let slice_len = bars_15m.len().min(20);
    let bbw_atr_20 = if slice_len >= 5 {
        let slice = &bars_15m[bars_15m.len() - slice_len..];
        let mean_c = slice.iter().map(|b| b.close).sum::<f64>() / (slice_len as f64);
        let var_c = slice
            .iter()
            .map(|b| (b.close - mean_c).powi(2))
            .sum::<f64>()
            / (slice_len as f64);
        (4.0 * var_c.sqrt()) / atr
    } else {
        dcw_20 * 0.75
    };

    let (coil_dcw, coil_bbw, max_z60, max_overshoot_atr, b_max_atr, min_rmb_atr) =
        match symbol.to_uppercase().as_str() {
            "BTC" => (3.207, 2.518, 2.347, 0.65, 3.05, 1.05),
            "ETH" => (3.149, 2.370, 2.394, 0.70, 3.25, 1.15),
            "XRP" => (3.160, 2.480, 2.300, 0.72, 3.45, 1.20),
            "LINK" => (3.175, 2.500, 2.280, 0.72, 3.50, 1.22),
            "SOL" | "HYPE" => (3.163, 2.516, 2.261, 0.75, 3.55, 1.25),
            "AVAX" | "SUI" => (3.203, 2.567, 2.222, 0.75, 3.70, 1.30),
            "NEAR" => (3.220, 2.600, 2.200, 0.78, 3.80, 1.35),
            _ => (3.200, 2.500, 2.300, 0.70, 3.25, 1.15),
        };

    let overshoot_atr = match side {
        Side::Long => (current_px - donchian.upper).max(0.0) / atr,
        Side::Short => (donchian.lower - current_px).max(0.0) / atr,
    };

    // Active 60m (4-bar) and 240m (16-bar / 4h) directional displacement
    // Prevents chasing late-stage multi-hour trend exhaustion rallies/waterfalls
    let recent_n = bars_15m.len().min(4);
    let d_60_atr = if recent_n >= 1 {
        let anchor_px = bars_15m[bars_15m.len() - recent_n].open;
        match side {
            Side::Long => (current_px - anchor_px).max(0.0) / atr,
            Side::Short => (anchor_px - current_px).max(0.0) / atr,
        }
    } else {
        0.5 * dcw_20
    };
    let slow_n = bars_15m.len().min(16);
    let d_240_atr = if slow_n >= 4 {
        let anchor_px = bars_15m[bars_15m.len() - slow_n].open;
        match side {
            Side::Long => (current_px - anchor_px).max(0.0) / atr,
            Side::Short => (anchor_px - current_px).max(0.0) / atr,
        }
    } else {
        d_60_atr
    };

    let consumed_budget = (0.5 * dcw_20).max(d_60_atr).max(0.55 * d_240_atr) + 0.50 * overshoot_atr;
    let rmb_atr = b_max_atr - consumed_budget;

    let is_coiled =
        dcw_20 >= 1.20 && dcw_20 <= coil_dcw && bbw_atr_20 <= coil_bbw && z_60.abs() <= 1.76;

    // Hard reject if DCW_20 >= 4.5, 4h run-up > 3.2x ATR, |Z60| >= asset 90th-pct cutoff, overshoot > max_overshoot_atr, or RMB < min_rmb_atr
    let approved = dcw_20 >= 1.20
        && dcw_20 < 4.50
        && d_240_atr <= 3.20
        && z_60.abs() < max_z60
        && overshoot_atr <= max_overshoot_atr
        && rmb_atr >= min_rmb_atr;

    BreakoutBudgetVerdict {
        approved,
        is_coiled,
        dcw_20,
        bbw_atr_20,
        overshoot_atr,
        rmb_atr,
        min_rmb_atr,
        max_z60,
    }
}

/// Feed quality statistics for WebSocket L2 telemetry monitoring
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedQualityStats {
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub mad_ms: f64,
    pub observed_age_p95_ms: f64,
    pub observed_age_p99_ms: f64,
    pub mean_lag_ms: f64,
    pub sample_count: usize,
    pub status: String,
    pub tau_1_ms: f64,
    pub tau_2_ms: f64,
    pub regime: String,
}

fn default_tau_1() -> f64 {
    10_000.0
}
fn default_tau_2() -> f64 {
    30_000.0
}
fn default_regime_label() -> String {
    "LowLatency".to_string()
}

/// Real-time L2 continuous order book tracker with basis-point distance weighting, multiscale physical-time dynamic EWMA, and local latency instrumentation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2ObiTracker {
    pub instant_obi: f64,
    pub ewma_250ms_obi: f64,
    pub ewma_1s_obi: f64,
    pub ewma_5s_obi: f64,
    pub last_exchange_time_ms: u64,
    pub last_local_recv_ms: u64,
    pub last_update_ms: u64,
    pub best_bid: f64,
    pub best_ask: f64,
    #[serde(default)]
    pub updates_since_reconnect: u32,
    #[serde(default)]
    pub reconnect_time_ms: u64,
    #[serde(default)]
    pub inter_arrival_samples: Vec<f64>,
    #[serde(default)]
    pub transport_lag_samples: Vec<f64>,
    #[serde(default = "default_tau_1")]
    pub tau_1_ms: f64,
    #[serde(default = "default_tau_2")]
    pub tau_2_ms: f64,
    #[serde(default = "default_regime_label")]
    pub current_regime_label: String,
    #[serde(default)]
    pub low_latency_streak: u32,
    #[serde(default)]
    pub stable_start_ms: u64,
    #[serde(skip)]
    pub last_instant: Option<std::time::Instant>,
    #[serde(default)]
    pub gaps_over_3s: u32,
}

impl Default for L2ObiTracker {
    fn default() -> Self {
        Self {
            instant_obi: 0.0,
            ewma_250ms_obi: 0.0,
            ewma_1s_obi: 0.0,
            ewma_5s_obi: 0.0,
            last_exchange_time_ms: 0,
            last_local_recv_ms: 0,
            last_update_ms: 0,
            best_bid: 0.0,
            best_ask: 0.0,
            updates_since_reconnect: 0,
            reconnect_time_ms: 0,
            inter_arrival_samples: Vec::with_capacity(30),
            transport_lag_samples: Vec::with_capacity(30),
            tau_1_ms: 10_000.0,
            tau_2_ms: 30_000.0,
            current_regime_label: "LowLatency".to_string(),
            low_latency_streak: 0,
            stable_start_ms: 0,
            last_instant: None,
            gaps_over_3s: 0,
        }
    }
}

impl L2ObiTracker {
    pub fn update(
        &mut self,
        bids: &[(f64, f64)],
        asks: &[(f64, f64)],
        exchange_time_ms: u64,
        local_recv_ms: u64,
    ) {
        if bids.is_empty() || asks.is_empty() {
            return;
        }
        self.best_bid = bids[0].0;
        self.best_ask = asks[0].0;

        let mid_price = (self.best_bid + self.best_ask) / 2.0;
        if mid_price <= 0.0 {
            return;
        }

        // Multiscale Distance-Weighted OBI across top 20 levels:
        // Asset-invariant basis-point exponential decay: w_i = exp(-d_bps / tau_bps), tau_bps = 5.0 bps (near-touch priority)
        let tau_bps = 5.0;
        let depth = 20.min(bids.len()).min(asks.len());
        let mut sum_bid_w = 0.0;
        let mut sum_ask_w = 0.0;

        for i in 0..depth {
            let bid_dist_bps = ((mid_price - bids[i].0) / mid_price).max(0.0) * 10_000.0;
            let w_bid = (-bid_dist_bps / tau_bps).exp();
            sum_bid_w += w_bid * bids[i].1;

            let ask_dist_bps = ((asks[i].0 - mid_price) / mid_price).max(0.0) * 10_000.0;
            let w_ask = (-ask_dist_bps / tau_bps).exp();
            sum_ask_w += w_ask * asks[i].1;
        }

        let denom = sum_bid_w + sum_ask_w;
        if denom <= 1e-9 {
            return;
        }
        let obi_inst = ((sum_bid_w - sum_ask_w) / denom).clamp(-1.0, 1.0);
        self.instant_obi = obi_inst;

        // Telemetry: record local inter-arrival via Instant (immune to NTP clock adjustments)
        let now_instant = std::time::Instant::now();
        let instant_dt = self
            .last_instant
            .map(|prev| prev.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let clock_dt = if self.last_local_recv_ms > 0 && local_recv_ms >= self.last_local_recv_ms {
            (local_recv_ms - self.last_local_recv_ms) as f64
        } else {
            0.0
        };

        let inter_arrival_dt = if instant_dt > 0.5 {
            instant_dt
        } else {
            clock_dt
        };

        if inter_arrival_dt > 0.0 {
            self.inter_arrival_samples.push(inter_arrival_dt);
            if self.inter_arrival_samples.len() > 30 {
                self.inter_arrival_samples.remove(0);
            }
        }
        self.last_instant = Some(now_instant);

        // Observed age / clock-adjusted transport lag
        if local_recv_ms >= exchange_time_ms {
            let lag = (local_recv_ms - exchange_time_ms) as f64;
            self.transport_lag_samples.push(lag);
            if self.transport_lag_samples.len() > 30 {
                self.transport_lag_samples.remove(0);
            }
        }

        // Amortized dynamic EWMA recalibration: evaluate every 12 updates to avoid hot-path sort overhead (~65s at 5.4s cadence)
        if (self.updates_since_reconnect + 1) % 12 == 0 && self.inter_arrival_samples.len() >= 8 {
            if let Some(qual) = self.compute_feed_quality() {
                let is_degraded = qual.p95_ms > 7500.0
                    || qual.observed_age_p99_ms > 1500.0
                    || qual.mad_ms > 2500.0;
                if is_degraded {
                    // Fail-Safe: Instant degradation on single cycle
                    self.low_latency_streak = 0;
                    self.stable_start_ms = 0;
                    self.tau_1_ms = 20_000.0;
                    self.tau_2_ms = 60_000.0;
                    self.current_regime_label = "JitterSubsampled".to_string();
                } else if self.current_regime_label == "JitterSubsampled" {
                    // Asymmetric recovery hysteresis: require >= 2 consecutive cycles (24 updates)
                    // and stability duration >= 60_000ms before returning to LowLatency
                    self.low_latency_streak = self.low_latency_streak.saturating_add(1);
                    if self.stable_start_ms == 0 {
                        self.stable_start_ms = local_recv_ms;
                    }
                    let stable_duration = local_recv_ms.saturating_sub(self.stable_start_ms);
                    if self.low_latency_streak >= 2 && stable_duration >= 60_000 {
                        self.tau_1_ms = 10_000.0;
                        self.tau_2_ms = 30_000.0;
                        self.current_regime_label = "LowLatency".to_string();
                        self.low_latency_streak = 0;
                        self.stable_start_ms = 0;
                    }
                } else {
                    self.low_latency_streak = 0;
                }
            }
        }

        // Physical-time elapsed: evaluate physical time delta
        let dt_ms = if self.last_local_recv_ms > 0 && local_recv_ms > self.last_local_recv_ms {
            (local_recv_ms - self.last_local_recv_ms) as f64
        } else if self.last_exchange_time_ms > 0 && exchange_time_ms > self.last_exchange_time_ms {
            (exchange_time_ms - self.last_exchange_time_ms) as f64
        } else {
            0.0
        };

        if dt_ms > 0.0 && dt_ms <= 12000.0 {
            let ln2 = std::f64::consts::LN_2;

            // Multiscale physical-time EWMA decay with dynamic tau_1 and tau_2:
            // tau_1 (10,000ms / 20,000ms): immediate queue / snapshot pressure (~2 snapshots)
            let alpha_tau1 = 1.0 - (-ln2 * dt_ms / self.tau_1_ms).exp();
            self.ewma_250ms_obi = (1.0 - alpha_tau1) * self.ewma_250ms_obi + alpha_tau1 * obi_inst;

            // tau_2 (30,000ms / 60,000ms): primary microstructure execution feature (~5-6 snapshots)
            let alpha_tau2 = 1.0 - (-ln2 * dt_ms / self.tau_2_ms).exp();
            self.ewma_1s_obi = (1.0 - alpha_tau2) * self.ewma_1s_obi + alpha_tau2 * obi_inst;

            // tau_3 = 60,000ms (1 min): institutional persistence filter
            let alpha_60s = 1.0 - (-ln2 * dt_ms / 60_000.0).exp();
            self.ewma_5s_obi = (1.0 - alpha_60s) * self.ewma_5s_obi + alpha_60s * obi_inst;

            self.updates_since_reconnect = self.updates_since_reconnect.saturating_add(1);
        } else {
            // Disconnect or stale interval (>12s): record gap and reinitialize
            if dt_ms > 12000.0 {
                self.gaps_over_3s = self.gaps_over_3s.saturating_add(1);
            }
            self.ewma_250ms_obi = obi_inst;
            self.ewma_1s_obi = obi_inst;
            self.ewma_5s_obi = obi_inst;
            self.updates_since_reconnect = 1;
            self.reconnect_time_ms = local_recv_ms;
        }

        self.last_exchange_time_ms = exchange_time_ms;
        self.last_local_recv_ms = local_recv_ms;
        self.last_update_ms = local_recv_ms;
    }

    pub fn compute_feed_quality(&self) -> Option<FeedQualityStats> {
        if self.inter_arrival_samples.len() < 5 {
            return None;
        }
        let mut sorted_arr = self.inter_arrival_samples.clone();
        sorted_arr.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = sorted_arr.len();

        let percentile = |pct: f64| -> f64 {
            let idx = ((pct / 100.0) * (n as f64 - 1.0)).round() as usize;
            sorted_arr[idx.min(n - 1)]
        };

        let p50 = percentile(50.0);
        let p90 = percentile(90.0);
        let p95 = percentile(95.0);
        let p99 = percentile(99.0);

        let mut abs_devs: Vec<f64> = sorted_arr.iter().map(|&x| (x - p50).abs()).collect();
        abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mad = 1.4826 * abs_devs[abs_devs.len() / 2];

        let mut sorted_lags = self.transport_lag_samples.clone();
        sorted_lags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let lag_n = sorted_lags.len();
        let lag_percentile = |pct: f64| -> f64 {
            if lag_n == 0 {
                return 0.0;
            }
            let idx = ((pct / 100.0) * (lag_n as f64 - 1.0)).round() as usize;
            sorted_lags[idx.min(lag_n - 1)]
        };
        let age_p95 = lag_percentile(95.0);
        let age_p99 = lag_percentile(99.0);

        let mean_lag = if !self.transport_lag_samples.is_empty() {
            self.transport_lag_samples.iter().sum::<f64>() / self.transport_lag_samples.len() as f64
        } else {
            0.0
        };

        let status = if p95 > 12_000.0
            || age_p99 > 3_000.0
            || mean_lag > 3_000.0
            || (self.gaps_over_3s > 0
                && self.last_update_ms.saturating_sub(self.reconnect_time_ms) < 60_000)
        {
            "SIGNAL_UNUSABLE".to_string()
        } else if p95 > 7_500.0 || age_p99 > 1_500.0 || mean_lag > 1_500.0 {
            "SIGNAL_DEGRADED".to_string()
        } else {
            "SIGNAL_GOOD".to_string()
        };

        Some(FeedQualityStats {
            p50_ms: p50,
            p90_ms: p90,
            p95_ms: p95,
            p99_ms: p99,
            mad_ms: mad,
            observed_age_p95_ms: age_p95,
            observed_age_p99_ms: age_p99,
            mean_lag_ms: mean_lag,
            sample_count: n,
            status,
            tau_1_ms: self.tau_1_ms,
            tau_2_ms: self.tau_2_ms,
            regime: self.current_regime_label.clone(),
        })
    }

    /// Explicit hook invoked upon WebSocket reconnection to reset epoch statistics
    pub fn on_l2_reconnect(&mut self, now_ms: u64) {
        self.updates_since_reconnect = 0;
        self.reconnect_time_ms = now_ms;
        self.inter_arrival_samples.clear();
        self.transport_lag_samples.clear();
        self.last_instant = None;
        self.low_latency_streak = 0;
        self.stable_start_ms = 0;
        self.last_local_recv_ms = 0;
        self.last_exchange_time_ms = 0;
        self.last_update_ms = 0;
        self.best_bid = 0.0;
        self.best_ask = 0.0;
        self.gaps_over_3s = 0;
        self.instant_obi = 0.0;
        self.ewma_250ms_obi = 0.0;
        self.ewma_1s_obi = 0.0;
        self.ewma_5s_obi = 0.0;
    }
}

/// Appends an institutional L2 WebSocket quality & transport telemetry record to JSONL
pub fn append_l2_telemetry_record(
    symbol: &str,
    window_start_ms: u64,
    window_end_ms: u64,
    qual: &FeedQualityStats,
    gaps_over_3s: u32,
    instant_obi: f64,
    ewma_tau1: f64,
    ewma_tau2: f64,
    ewma_5s: f64,
) {
    let record = serde_json::json!({
        "symbol": symbol,
        "window_start_ms": window_start_ms,
        "window_end_ms": window_end_ms,
        "sample_count": qual.sample_count,
        "interarrival_p50_ms": qual.p50_ms,
        "interarrival_p90_ms": qual.p90_ms,
        "interarrival_p95_ms": qual.p95_ms,
        "interarrival_p99_ms": qual.p99_ms,
        "interarrival_mad_ms": qual.mad_ms,
        "observed_age_p95_ms": qual.observed_age_p95_ms,
        "observed_age_p99_ms": qual.observed_age_p99_ms,
        "mean_observed_age_ms": qual.mean_lag_ms,
        "gaps_over_3s": gaps_over_3s,
        "tau1_ms": qual.tau_1_ms,
        "tau2_ms": qual.tau_2_ms,
        "regime": qual.regime,
        "obi_instant": instant_obi,
        "obi_tau1": ewma_tau1,
        "obi_tau2": ewma_tau2,
        "obi_5s": ewma_5s,
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let file_path = format!("hl_l2_telemetry_{}.jsonl", symbol);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
        {
            let _ = writeln!(file, "{}", line);
        }
    }
}

/// Frozen snapshot of candidate telemetry evaluated at decision/entry time
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineBCandidateLog {
    pub candidate_id: String,
    pub symbol: String,
    pub decision_time_ms: u64,
    pub side: String,
    pub tau_regime: String,
    pub tau1_ms: f64,
    pub tau2_ms: f64,
    pub obi_instant: f64,
    pub obi_tau1: f64,
    pub obi_tau2: f64,
    pub obi_5s: f64,
    pub p_trend: f64,
    pub donchian_distance_bps: f64,
    pub feed_quality: String,
    pub hurdle_passed: bool,
    pub portfolio_selected: bool,
}

pub fn append_candidate_log(log_entry: &EngineBCandidateLog) {
    if let Ok(line) = serde_json::to_string(log_entry) {
        let file_path = format!("hl_candidates_{}.jsonl", log_entry.symbol);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
        {
            let _ = writeln!(file, "{}", line);
        }
    }
}

impl L2ObiTracker {
    pub fn is_fresh_and_uncrossed(&self, now_ms: u64, max_latency_ms: u64) -> bool {
        if self.last_update_ms == 0 || now_ms < self.last_update_ms {
            return false;
        }
        let latency = now_ms - self.last_update_ms;
        latency <= max_latency_ms && self.best_ask > self.best_bid && self.best_bid > 0.0
    }

    /// Computes the Stoikov Micro-Price conditional on current order book depth and imbalance:
    /// P_micro = S_mid + I * (spread / 2)
    #[allow(dead_code)]
    pub fn compute_micro_price(&self) -> f64 {
        if self.best_bid <= 0.0 || self.best_ask <= 0.0 || self.best_bid >= self.best_ask {
            return 0.0;
        }
        let mid = (self.best_bid + self.best_ask) / 2.0;
        let spread = self.best_ask - self.best_bid;
        mid + self.instant_obi * (spread / 2.0)
    }

    #[allow(dead_code)]
    pub fn satisfies_entry_hurdle(&self, side: &Side, now_ms: u64) -> (bool, &'static str) {
        if !self.is_fresh_and_uncrossed(now_ms, 7500) {
            return (false, "L2 Book Stale or Crossed");
        }
        let elapsed_since_reconnect = now_ms.saturating_sub(self.reconnect_time_ms);
        if self.updates_since_reconnect < 3 || elapsed_since_reconnect < 15_000 {
            return (false, "L2 Book Warming Up After Reconnect (requires >=3 updates and >=15s continuous stream)");
        }

        // Fail-Closed when Signal Resolution is incompatible with EWMA half-lives
        if let Some(qual) = self.compute_feed_quality() {
            if qual.status == "SIGNAL_UNUSABLE" {
                return (false, "Feed Quality SIGNAL_UNUSABLE (resolution incompatible with EWMA half-lives or recent transport gap >12s)");
            }
        }

        match side {
            Side::Long => {
                // Multiscale hurdle: ensure bid depth is not overwhelmed by adverse sell walls.
                // Tolerates natural taker absorption during breakout momentum (down to -0.18 instant, -0.15 1s, -0.12 5s) while rejecting toxic sell imbalance.
                if self.instant_obi >= -0.18
                    && self.ewma_1s_obi >= -0.15
                    && self.ewma_5s_obi >= -0.12
                {
                    (true, "Bullish Multiscale Depth Alignment")
                } else {
                    (false, "Insufficient Bid Depth / Fleeting Ask Asymmetry")
                }
            }
            Side::Short => {
                // Multiscale hurdle: ensure ask depth is not overwhelmed by adverse buy walls.
                if self.instant_obi <= 0.18 && self.ewma_1s_obi <= 0.15 && self.ewma_5s_obi <= 0.12
                {
                    (true, "Bearish Multiscale Depth Alignment")
                } else {
                    (false, "Insufficient Ask Depth / Fleeting Bid Asymmetry")
                }
            }
        }
    }
}

/// Exogenous Funding, Open Interest & Basis Crowding Hazard Tracker (MS-X Overlay)
/// Preserves the 4D price-based Gaussian HMM while scaling/vetoing crowded leveraged breakouts.
#[derive(Debug, Clone, Default)]
pub struct MarketCrowdingTracker {
    pub funding_1h: f64,
    pub open_interest: f64,
    pub mark_px: f64,
    pub oracle_px: f64,
    pub funding_samples: Vec<f64>,
    pub oi_samples: Vec<f64>,
    pub dlog_oi_samples: Vec<f64>,
    pub basis_samples: Vec<f64>,
    pub funding_z: f64,
    pub dlog_oi_z: f64,
    pub basis_z: f64,
    /// Timestamped Open Interest history `(timestamp_ms, open_interest)` for exact 15m delta lookup
    pub oi_history: VecDeque<(u64, f64)>,
    /// Timestamp (local ms) when `mark_px` was last refreshed from ActiveAssetCtx
    pub last_mark_update_ms: u64,
}

impl MarketCrowdingTracker {
    fn robust_z(val: f64, samples: &[f64], min_scale: f64) -> f64 {
        if samples.len() < 8 {
            return 0.0;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        let median = if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        };
        let mut abs_devs: Vec<f64> = sorted.iter().map(|&x| (x - median).abs()).collect();
        abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mad = if abs_devs.len() % 2 == 0 {
            (abs_devs[mid - 1] + abs_devs[mid]) / 2.0
        } else {
            abs_devs[mid]
        };
        let scale = (mad / 0.6745).max(min_scale);
        ((val - median) / scale).clamp(-5.0, 5.0)
    }

    #[allow(dead_code)]
    pub fn update(&mut self, funding_1h: f64, open_interest: f64, mark_px: f64, oracle_px: f64) {
        self.update_with_time(funding_1h, open_interest, mark_px, oracle_px, 0);
    }

    pub fn update_with_time(
        &mut self,
        funding_1h: f64,
        open_interest: f64,
        mark_px: f64,
        oracle_px: f64,
        timestamp_ms: u64,
    ) {
        if !funding_1h.is_finite() || !open_interest.is_finite() || open_interest <= 0.0 {
            return;
        }
        self.funding_1h = funding_1h;
        self.open_interest = open_interest;
        self.mark_px = mark_px;
        self.oracle_px = oracle_px;
        if timestamp_ms > 0 && mark_px > 0.0 {
            self.last_mark_update_ms = timestamp_ms;
        }

        if timestamp_ms > 0 {
            self.oi_history.push_back((timestamp_ms, open_interest));
            let cutoff_ms = timestamp_ms.saturating_sub(25 * 60 * 1000);
            while let Some(&(ts, _)) = self.oi_history.front() {
                if ts < cutoff_ms && self.oi_history.len() > 2 {
                    self.oi_history.pop_front();
                } else {
                    break;
                }
            }
        }

        self.funding_samples.push(funding_1h);
        if self.funding_samples.len() > 240 {
            self.funding_samples.remove(0);
        }

        if let Some(&prev_oi) = self.oi_samples.first() {
            if prev_oi > 0.0 {
                let dlog_oi = (open_interest / prev_oi).ln();
                self.dlog_oi_samples.push(dlog_oi);
                if self.dlog_oi_samples.len() > 240 {
                    self.dlog_oi_samples.remove(0);
                }
                self.dlog_oi_z = Self::robust_z(dlog_oi, &self.dlog_oi_samples, 1e-4);
            }
        }
        self.oi_samples.push(open_interest);
        if self.oi_samples.len() > 60 {
            self.oi_samples.remove(0);
        }

        if oracle_px > 0.0 && mark_px > 0.0 {
            let basis = (mark_px - oracle_px) / oracle_px;
            self.basis_samples.push(basis);
            if self.basis_samples.len() > 240 {
                self.basis_samples.remove(0);
            }
            self.basis_z = Self::robust_z(basis, &self.basis_samples, 1e-4);
        }

        self.funding_z = Self::robust_z(funding_1h, &self.funding_samples, 1e-5);
    }

    /// Returns the causal 15-minute percentage change in Open Interest (e.g. -0.20 means -0.20%).
    /// Fails open (0.0) if Open Interest history has not yet spanned at least 10 minutes.
    pub fn oi_delta_15m_pct(&self, now_ms: u64) -> f64 {
        if self.open_interest <= 0.0
            || !self.open_interest.is_finite()
            || self.oi_history.is_empty()
        {
            return 0.0;
        }
        let target_ts = now_ms.saturating_sub(15 * 60 * 1000);
        // Find sample closest to 15 minutes ago (require at least 10 minutes of history)
        let mut best_sample: Option<(u64, f64)> = None;
        for &(ts, oi) in &self.oi_history {
            if ts <= target_ts {
                best_sample = Some((ts, oi));
            } else {
                if best_sample.is_none() && now_ms.saturating_sub(ts) >= 10 * 60 * 1000 {
                    best_sample = Some((ts, oi));
                }
                break;
            }
        }
        match best_sample {
            Some((_, prev_oi)) if prev_oi > 0.0 => {
                ((self.open_interest - prev_oi) / prev_oi) * 100.0
            }
            _ => 0.0,
        }
    }

    /// Computes the exogenous Crowding Hazard Multiplier H_t in [0.25, 1.0]:
    /// H_t = exp(-0.35 * max(0, |F_z| - 2.0) - 0.25 * max(0, dOI_z - 2.0) - 0.15 * max(0, |basis_z| - 1.5))
    pub fn crowding_hazard(&self) -> f64 {
        let f_term = 0.35 * (self.funding_z.abs() - 2.0).max(0.0);
        let oi_term = 0.25 * (self.dlog_oi_z - 2.0).max(0.0);
        let basis_term = 0.15 * (self.basis_z.abs() - 1.5).max(0.0);
        (-f_term - oi_term - basis_term).exp().clamp(0.25, 1.0)
    }

    /// Returns directional exposure multiplier for Engine B breakouts:
    /// Only penalizes breakouts aligned with crowded funding (e.g. Long breakout into extreme positive funding + surging OI).
    pub fn directional_trend_multiplier(&self, side: &Side) -> f64 {
        let is_crowded_direction = match side {
            Side::Long => self.funding_1h > 0.0001 && self.funding_z > 1.5,
            Side::Short => self.funding_1h < -0.0001 && self.funding_z < -1.5,
        };
        if is_crowded_direction {
            self.crowding_hazard()
        } else {
            1.0
        }
    }
}

/// Monotone Hybrid Ratchet Chandelier Exit State
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChandelierExitState {
    pub side: Side,
    pub entry_price: f64,
    pub extreme_favorable_price: f64,
    pub current_stop: f64,
    pub initial_stop: f64,
    pub base_multiplier: f64,
    pub active_multiplier: f64,
}

impl ChandelierExitState {
    #[allow(dead_code)]
    pub fn new(side: Side, entry_price: f64, current_atr: f64, base_multiplier: f64) -> Self {
        let initial_stop = match side {
            Side::Long => entry_price - base_multiplier * current_atr,
            Side::Short => entry_price + base_multiplier * current_atr,
        };

        Self {
            side,
            entry_price,
            extreme_favorable_price: entry_price,
            current_stop: initial_stop,
            initial_stop,
            base_multiplier,
            active_multiplier: base_multiplier,
        }
    }

    /// Updates trailing stop on bar close / price tick with finite guards and optional contraction cap.
    #[allow(dead_code)]
    pub fn update(
        &mut self,
        current_high: f64,
        current_low: f64,
        current_atr: f64,
        p_trend: f64,
    ) -> f64 {
        self.update_with_cap(current_high, current_low, current_atr, p_trend, None)
    }

    pub fn update_with_cap(
        &mut self,
        current_high: f64,
        current_low: f64,
        current_atr: f64,
        p_trend: f64,
        max_contraction_step: Option<f64>,
    ) -> f64 {
        self.update_with_ratchet_and_cap(
            current_high,
            current_low,
            current_atr,
            p_trend,
            max_contraction_step,
            false,
        )
    }

    pub fn update_with_ratchet_and_cap(
        &mut self,
        current_high: f64,
        current_low: f64,
        current_atr: f64,
        p_trend: f64,
        max_contraction_step: Option<f64>,
        enable_breakeven_ratchet: bool,
    ) -> f64 {
        // Guard against non-finite or invalid inputs
        if current_atr <= 0.0
            || current_atr.is_nan()
            || current_atr.is_infinite()
            || p_trend.is_nan()
            || p_trend.is_infinite()
            || current_high.is_nan()
            || current_low.is_nan()
        {
            return self.current_stop;
        }

        // Chandelier ATR Multiplier Compression by P_Trend Decay
        // Per research: when P_Trend < 0.60, compress multiplier to half the base (e.g., 3.0→1.5, 2.0→1.0)
        // This tightens the stop during regime decay, front-running mean reversion
        let effective_multiplier = if p_trend < 0.60 {
            // Hard compression to half base when regime decay detected
            (self.base_multiplier * 0.5).max(1.0)
        } else {
            // Normal decay formula for P_Trend >= 0.60
            let decay_factor = ((p_trend - 0.55) / (0.85 - 0.55)).clamp(0.0, 1.0);
            let min_multiplier = (self.base_multiplier * 0.5).max(1.0);
            min_multiplier + (self.base_multiplier - min_multiplier) * decay_factor
        };

        if let Some(max_step) = max_contraction_step {
            if effective_multiplier < self.active_multiplier {
                self.active_multiplier =
                    (self.active_multiplier - max_step).max(effective_multiplier);
            } else {
                self.active_multiplier =
                    (self.active_multiplier + max_step).min(effective_multiplier);
            }
        } else {
            self.active_multiplier = effective_multiplier;
        }

        match self.side {
            Side::Long => {
                if current_high > self.extreme_favorable_price {
                    self.extreme_favorable_price = current_high;
                }
                let mut candidate_stop =
                    self.extreme_favorable_price - self.active_multiplier * current_atr;

                if enable_breakeven_ratchet {
                    let delta_fav = (self.extreme_favorable_price - self.entry_price).max(0.0);
                    let ret_fav_pct = if self.entry_price > 0.0 {
                        delta_fav / self.entry_price
                    } else {
                        0.0
                    };
                    let eps = 1e-6;

                    // 1. Breakeven Floor: once favorable progress >= 0.75 ATR or >= +45 bps (0.45%), AND at least +32 bps gross (to cover round-trip fees + breathing room),
                    // stop moves to at least entry + 15 bps (bounded by 50% of delta_fav)
                    if (delta_fav >= (0.75 * current_atr) - eps || ret_fav_pct >= 0.0045)
                        && ret_fav_pct >= 0.0032
                    {
                        let be_stop =
                            (self.entry_price * 1.0015).min(self.entry_price + 0.50 * delta_fav);
                        candidate_stop = f64::max(candidate_stop, be_stop);
                    }
                    // 1b. Early Impulse Profit Lock: once favorable progress >= 1.0 ATR or >= +75 bps (0.75%), AND at least +40 bps gross, protect at least 45% of peak excursion
                    if (delta_fav >= (1.0 * current_atr) - eps || ret_fav_pct >= 0.0075)
                        && ret_fav_pct >= 0.0040
                    {
                        candidate_stop =
                            f64::max(candidate_stop, self.entry_price + 0.45 * delta_fav);
                    }
                    // 2. Tier 2 (+2% Gain Rule): once gain >= +2.0%, lock in at least +1.0% net profit
                    if ret_fav_pct >= 0.020 {
                        candidate_stop = f64::max(candidate_stop, self.entry_price * 1.0100);
                    }
                    // 3. Tier 3 (+3% Gain Rule): once gain >= +3.0%, lock in at least +2.0% net profit
                    if ret_fav_pct >= 0.030 {
                        candidate_stop = f64::max(candidate_stop, self.entry_price * 1.0200);
                    }
                    // 4. Tier 4 (Super-Trend Runner): once gain >= +4.0%, protect at least 70% of peak excursion
                    if ret_fav_pct >= 0.040 {
                        let locked_70 = self.entry_price + 0.70 * delta_fav;
                        candidate_stop = f64::max(
                            candidate_stop,
                            f64::max(self.entry_price * 1.0250, locked_70),
                        );
                    }
                    // 5. ATR Giveback Floor: once progress >= 1.5 ATR AND at least +45 bps gross, protect at least 50% of peak progress
                    if delta_fav >= (1.5 * current_atr) - eps && ret_fav_pct >= 0.0045 {
                        candidate_stop =
                            f64::max(candidate_stop, self.entry_price + 0.5 * delta_fav);
                    }
                }

                // Monotone Ratchet: stop strictly moves UP
                self.current_stop = f64::max(self.current_stop, candidate_stop);
            }
            Side::Short => {
                if current_low < self.extreme_favorable_price {
                    self.extreme_favorable_price = current_low;
                }
                let mut candidate_stop =
                    self.extreme_favorable_price + self.active_multiplier * current_atr;

                if enable_breakeven_ratchet {
                    let delta_fav = (self.entry_price - self.extreme_favorable_price).max(0.0);
                    let ret_fav_pct = if self.entry_price > 0.0 {
                        delta_fav / self.entry_price
                    } else {
                        0.0
                    };
                    let eps = 1e-6;

                    // 1. Breakeven Floor: once favorable progress >= 0.75 ATR or >= +45 bps (0.45%), AND at least +32 bps gross (to cover round-trip fees + breathing room),
                    // stop moves to at least entry - 15 bps (bounded by 50% of delta_fav)
                    if (delta_fav >= (0.75 * current_atr) - eps || ret_fav_pct >= 0.0045)
                        && ret_fav_pct >= 0.0032
                    {
                        let be_stop =
                            (self.entry_price * 0.9985).max(self.entry_price - 0.50 * delta_fav);
                        candidate_stop = f64::min(candidate_stop, be_stop);
                    }
                    // 1b. Early Impulse Profit Lock: once favorable progress >= 1.0 ATR or >= +75 bps (0.75%), AND at least +40 bps gross, protect at least 45% of peak excursion
                    if (delta_fav >= (1.0 * current_atr) - eps || ret_fav_pct >= 0.0075)
                        && ret_fav_pct >= 0.0040
                    {
                        candidate_stop =
                            f64::min(candidate_stop, self.entry_price - 0.45 * delta_fav);
                    }
                    // 2. Tier 2 (+2% Gain Rule): once gain >= +2.0%, lock in at least +1.0% net profit
                    if ret_fav_pct >= 0.020 {
                        candidate_stop = f64::min(candidate_stop, self.entry_price * 0.9900);
                    }
                    // 3. Tier 3 (+3% Gain Rule): once gain >= +3.0%, lock in at least +2.0% net profit
                    if ret_fav_pct >= 0.030 {
                        candidate_stop = f64::min(candidate_stop, self.entry_price * 0.9800);
                    }
                    // 4. Tier 4 (Super-Trend Runner): once gain >= +4.0%, protect at least 70% of peak excursion
                    if ret_fav_pct >= 0.040 {
                        let locked_70 = self.entry_price - 0.70 * delta_fav;
                        candidate_stop = f64::min(
                            candidate_stop,
                            f64::min(self.entry_price * 0.9750, locked_70),
                        );
                    }
                    // 5. ATR Giveback Floor: once progress >= 1.5 ATR AND at least +45 bps gross, protect at least 50% of peak progress
                    if delta_fav >= (1.5 * current_atr) - eps && ret_fav_pct >= 0.0045 {
                        candidate_stop =
                            f64::min(candidate_stop, self.entry_price - 0.5 * delta_fav);
                    }
                }

                // Monotone Ratchet: stop strictly moves DOWN
                self.current_stop = f64::min(self.current_stop, candidate_stop);
            }
        }

        self.current_stop
    }

    #[allow(dead_code)]
    pub fn is_stopped_out(&self, current_price: f64) -> bool {
        match self.side {
            Side::Long => current_price <= self.current_stop,
            Side::Short => current_price >= self.current_stop,
        }
    }

    pub fn is_bar_stopped_out(
        &self,
        prev_stop: f64,
        bar_high: f64,
        bar_low: f64,
        bar_close: f64,
    ) -> bool {
        match self.side {
            Side::Long => bar_low <= prev_stop || bar_close <= self.current_stop,
            Side::Short => bar_high >= prev_stop || bar_close >= self.current_stop,
        }
    }
}

fn default_chandelier_mult() -> f64 {
    2.0
}

/// Shadow Trading Record for Paper-Execution Validation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowTrade {
    pub symbol: String,
    pub side: Side,
    pub entry_time: String,
    pub entry_price: f64,
    pub size: f64,
    pub initial_stop: f64,
    pub current_stop: f64,
    pub extreme_favorable_price: f64,
    pub atr_at_entry: f64,
    #[serde(default = "default_chandelier_mult")]
    pub active_multiplier: f64,
    pub is_active: bool,
    pub exit_time: Option<String>,
    pub exit_price: Option<f64>,
    pub pnl_usdc: Option<f64>,
    pub return_bps: Option<f64>,
    pub exit_reason: Option<String>,
    #[serde(default)]
    pub is_portfolio_selected: bool,
    #[serde(default)]
    pub lease_token: Option<String>,
}

pub const SHADOW_CLUSTER_LOCK: &str = "hl_shadow_cluster.lock";
pub const SHADOW_CLUSTER_LOCK_GUARD: &str = "hl_shadow_cluster.lock.guard";
pub const SHADOW_CLUSTER_COUNTER: &str = "hl_shadow_cluster.counter";

/// Atomically reads and increments the monotonic shadow counter.
/// Fails closed if the counter file exists but is corrupted.
pub fn get_and_inc_shadow_counter() -> Result<u64, String> {
    let counter_path = std::path::Path::new(SHADOW_CLUSTER_COUNTER);
    let current = if counter_path.exists() {
        let content = fs::read_to_string(counter_path)
            .map_err(|e| format!("Failed to read shadow counter file: {}", e))?;
        content
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("Corrupted shadow counter '{}': {}", content.trim(), e))?
    } else {
        0u64
    };

    let next = current
        .checked_add(1)
        .ok_or_else(|| "Shadow counter overflow".to_string())?;

    let tmp_path = format!("{}.tmp.{}", SHADOW_CLUSTER_COUNTER, std::process::id());
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp shadow counter: {}", e))?;
        file.write_all(next.to_string().as_bytes())
            .map_err(|e| format!("Failed to write temp shadow counter: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync temp shadow counter: {}", e))?;
    }

    fs::rename(&tmp_path, counter_path)
        .map_err(|e| format!("Failed to atomically rename shadow counter: {}", e))?;

    sync_parent_dir(SHADOW_CLUSTER_COUNTER);

    Ok(next)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShadowClusterLease {
    pub symbol: String,
    #[serde(default)]
    pub owner_uuid: String,
    pub lease_token: String,
    pub acquired_at: i64,
    pub expires_at: i64,
    #[serde(default)]
    pub fencing_token: u64,
}

fn write_shadow_lease_atomic(lease: &ShadowClusterLease) -> Result<(), String> {
    let content = serde_json::to_string_pretty(lease)
        .map_err(|e| format!("Failed to serialize shadow lease: {}", e))?;
    let lock_path = std::path::Path::new(SHADOW_CLUSTER_LOCK);
    let tmp_path = format!("{}.tmp.{}", SHADOW_CLUSTER_LOCK, std::process::id());
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp shadow lease: {}", e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write temp shadow lease: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync temp shadow lease: {}", e))?;
    }
    fs::rename(&tmp_path, lock_path)
        .map_err(|e| format!("Failed to rename shadow lease file: {}", e))?;
    sync_parent_dir(SHADOW_CLUSTER_LOCK);
    Ok(())
}

/// Atomically acquires the virtual shadow cluster lease using flock mutual exclusion.
/// Returns (is_acquired, lease_token)
pub fn try_acquire_shadow_cluster_lease(symbol: &str) -> (bool, Option<String>) {
    let owner_uuid = format!("{}_{}", symbol, Utc::now().timestamp_micros());
    let (acq, tok, _) = try_acquire_shadow_cluster_lease_with_uuid(symbol, &owner_uuid);
    (acq, tok)
}

pub fn try_acquire_shadow_cluster_lease_with_uuid(
    symbol: &str,
    owner_uuid: &str,
) -> (bool, Option<String>, u64) {
    let _guard = match FlockGuard::acquire(SHADOW_CLUSTER_LOCK_GUARD) {
        Ok(g) => g,
        Err(_) => return (false, None, 0),
    };
    let now = Utc::now().timestamp();

    // Check if an existing lease file exists
    if std::path::Path::new(SHADOW_CLUSTER_LOCK).exists() {
        if let Ok(c) = fs::read_to_string(SHADOW_CLUSTER_LOCK) {
            if let Ok(mut lease) = serde_json::from_str::<ShadowClusterLease>(&c) {
                // If this symbol already holds the lease
                if lease.symbol == symbol {
                    if !lease.owner_uuid.is_empty() && lease.owner_uuid != owner_uuid {
                        eprintln!(
                            "⚠️ [SHADOW LEASE REJECTED] Active lease for {} held by another UUID ('{}' != '{}'). Rejecting duplicate takeover.",
                            symbol, lease.owner_uuid, owner_uuid
                        );
                        return (false, None, 0);
                    }
                    // Same owner_uuid -> refresh without altering epoch/fencing_token!
                    lease.expires_at = now + 3600;
                    if write_shadow_lease_atomic(&lease).is_ok() {
                        return (true, Some(lease.lease_token), lease.fencing_token);
                    }
                }

                // If held by another symbol and unexpired, verify if holder's shadow trade finished early
                if lease.expires_at > now {
                    let owner_finished_early = load_shadow_trade(&lease.symbol)
                        .map(|t| !t.is_active)
                        .unwrap_or(false);
                    if !owner_finished_early {
                        return (false, None, 0);
                    }
                }
            }
        }
    }

    let new_token = match get_and_inc_shadow_counter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("❌ [SHADOW LEASE ERROR] Counter failure: {}", e);
            return (false, None, 0);
        }
    };
    let lease_token = format!(
        "{}_{}_{}",
        symbol,
        new_token,
        Utc::now().timestamp_subsec_nanos()
    );
    let new_lease = ShadowClusterLease {
        symbol: symbol.to_string(),
        owner_uuid: owner_uuid.to_string(),
        lease_token: lease_token.clone(),
        acquired_at: now,
        expires_at: now + 3600,
        fencing_token: new_token,
    };

    if write_shadow_lease_atomic(&new_lease).is_ok() {
        (true, Some(lease_token), new_token)
    } else {
        (false, None, 0)
    }
}

/// Renews the active shadow cluster lease heartbeat while a position is held under flock guard.
/// PRESERVES fencing_token (does not increment epoch on heartbeat).
pub fn refresh_shadow_cluster_lease(symbol: &str, token: &str) -> bool {
    let _guard = match FlockGuard::acquire(SHADOW_CLUSTER_LOCK_GUARD) {
        Ok(g) => g,
        Err(_) => return false,
    };
    let now = Utc::now().timestamp();
    if let Ok(c) = fs::read_to_string(SHADOW_CLUSTER_LOCK) {
        if let Ok(mut lease) = serde_json::from_str::<ShadowClusterLease>(&c) {
            if lease.symbol == symbol && lease.lease_token == token {
                // Heartbeat extends expiration only. Do NOT alter fencing_token!
                lease.expires_at = now + 3600;
                return write_shadow_lease_atomic(&lease).is_ok();
            }
        }
    }
    false
}

/// Releases the shadow cluster lease safely verifying token ownership under flock guard.
pub fn release_shadow_cluster_lease(symbol: &str, token: Option<&str>) -> bool {
    let _guard = match FlockGuard::acquire(SHADOW_CLUSTER_LOCK_GUARD) {
        Ok(g) => g,
        Err(_) => return false,
    };
    if let Ok(c) = fs::read_to_string(SHADOW_CLUSTER_LOCK) {
        if let Ok(lease) = serde_json::from_str::<ShadowClusterLease>(&c) {
            if lease.symbol == symbol {
                if let Some(tok) = token {
                    if lease.lease_token != tok {
                        return false; // Token mismatch: lease was already superseded
                    }
                }
                return fs::remove_file(SHADOW_CLUSTER_LOCK).is_ok();
            }
        }
    }
    false
}

pub fn shadow_state_path(symbol: &str) -> String {
    format!("hl_shadow_state_{}.json", symbol)
}

pub fn save_shadow_trade(symbol: &str, trade: Option<&ShadowTrade>) {
    let path = shadow_state_path(symbol);
    if let Some(t) = trade {
        if let Ok(c) = serde_json::to_string_pretty(t) {
            let tmp_path = format!("{}.tmp.{}", path, std::process::id());
            if let Ok(mut file) = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp_path)
            {
                if file.write_all(c.as_bytes()).is_ok() && file.sync_all().is_ok() {
                    let _ = fs::rename(&tmp_path, &path);
                    sync_parent_dir(&path);
                }
            }
        }
    } else {
        let _ = fs::remove_file(&path);
        sync_parent_dir(&path);
    }
}

pub fn load_shadow_trade(symbol: &str) -> Option<ShadowTrade> {
    let path = shadow_state_path(symbol);
    if !std::path::Path::new(&path).exists() {
        return None;
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

pub fn record_shadow_entry(
    symbol: &str,
    side: Side,
    entry_price: f64,
    size: f64,
    atr: f64,
    atr_mult: f64,
) -> ShadowTrade {
    let now = Utc::now();
    let initial_stop = match side {
        Side::Long => entry_price - atr_mult * atr,
        Side::Short => entry_price + atr_mult * atr,
    };
    let (is_portfolio_selected, lease_token) = try_acquire_shadow_cluster_lease(symbol);

    let trade = ShadowTrade {
        symbol: symbol.to_string(),
        side,
        entry_time: now.to_rfc3339(),
        entry_price,
        size,
        initial_stop,
        current_stop: initial_stop,
        extreme_favorable_price: entry_price,
        atr_at_entry: atr,
        active_multiplier: atr_mult,
        is_active: true,
        exit_time: None,
        exit_price: None,
        pnl_usdc: None,
        return_bps: None,
        exit_reason: None,
        is_portfolio_selected,
        lease_token,
    };

    save_shadow_trade(symbol, Some(&trade));

    // Append to JSONL Journal
    let log_entry = serde_json::json!({
        "timestamp": trade.entry_time,
        "event": "SHADOW_ENTRY",
        "symbol": trade.symbol,
        "side": trade.side.to_string(),
        "entry_price": trade.entry_price,
        "size": trade.size,
        "initial_stop": trade.initial_stop,
        "atr": trade.atr_at_entry,
        "is_portfolio_selected": trade.is_portfolio_selected,
    });
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(SHADOW_TREND_JOURNAL)
    {
        let _ = writeln!(
            f,
            "{}",
            serde_json::to_string(&log_entry).unwrap_or_default()
        );
    }

    trade
}

pub fn record_shadow_exit(trade: &mut ShadowTrade, exit_price: f64, exit_reason: &str) -> f64 {
    let now = Utc::now();
    let is_long = trade.side == Side::Long;

    // Gross PnL
    let gross_pnl = if is_long {
        (exit_price - trade.entry_price) * trade.size
    } else {
        (trade.entry_price - exit_price) * trade.size
    };

    // Institutional Hyperliquid fee and slippage modeling:
    // Maker entry: 0.0144%, Taker stop exit: 0.0432%, Conservative stop slippage: 0.0500% (5 bps)
    let notional_entry = trade.entry_price * trade.size;
    let notional_exit = exit_price * trade.size;
    let maker_entry_fee = notional_entry * 0.000144;
    let taker_exit_fee = notional_exit * 0.000432;
    let stop_slippage = notional_exit * 0.0005;
    let total_friction = maker_entry_fee + taker_exit_fee + stop_slippage;
    let net_pnl = gross_pnl - total_friction;

    let return_bps = if notional_entry > 0.0 {
        (net_pnl / notional_entry) * 10000.0
    } else {
        0.0
    };

    trade.is_active = false;
    trade.exit_time = Some(now.to_rfc3339());
    trade.exit_price = Some(exit_price);
    trade.pnl_usdc = Some(net_pnl);
    trade.return_bps = Some(return_bps);
    trade.exit_reason = Some(exit_reason.to_string());

    if trade.is_portfolio_selected {
        release_shadow_cluster_lease(&trade.symbol, trade.lease_token.as_deref());
    }
    save_shadow_trade(&trade.symbol, None); // Clear active file

    // Append exit to JSONL Journal
    let log_entry = serde_json::json!({
        "timestamp": now.to_rfc3339(),
        "event": "SHADOW_EXIT",
        "symbol": trade.symbol,
        "side": trade.side.to_string(),
        "entry_price": trade.entry_price,
        "exit_price": exit_price,
        "size": trade.size,
        "net_pnl_usdc": net_pnl,
        "return_bps": return_bps,
        "exit_reason": exit_reason,
        "is_portfolio_selected": trade.is_portfolio_selected,
    });
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(SHADOW_TREND_JOURNAL)
    {
        let _ = writeln!(
            f,
            "{}",
            serde_json::to_string(&log_entry).unwrap_or_default()
        );
    }

    net_pnl
}

// ============================================================================
// POLICY C: ENGINE B LOSS-PARITY SIZING, SESSION GATING, COILED-SPRING
// SQUEEZE FILTER & TWO-STAGE ALO MAKER RETEST STATE MACHINE
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendDirection {
    Long,
    Short,
}

impl TrendDirection {
    #[inline(always)]
    pub fn sign(self) -> f64 {
        match self {
            TrendDirection::Long => 1.0,
            TrendDirection::Short => -1.0,
        }
    }

    #[inline(always)]
    pub fn to_side(self) -> Side {
        match self {
            TrendDirection::Long => Side::Long,
            TrendDirection::Short => Side::Short,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendAssetTier {
    CryptoMajor,
    CryptoAlt,
    Hip3UsEquity,
    Hip3Commodity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Live,
    Shadow { reason: &'static str },
}

/// Configuration for Engine B Risk Budgeting, Session Gating, and Squeeze/Retest Filters (Policy C).
#[derive(Debug, Clone, Copy)]
pub struct TrendEngineConfig {
    pub tier: TrendAssetTier,
    pub equity_fraction_eng_a: f64,   // 0.35 Crypto, 0.32 HIP-3
    pub trend_risk_fraction: f64,     // 0.28 Majors/Commodities, 0.25 Alts, 0.30 Equities
    pub max_trend_notional_usdc: f64, // Hard cap: $55.00 Majors/Equities, $50.00 Alts, $52.00 Commodities
    pub min_trade_notional_usdc: f64, // Hyperliquid floor guard: $12.00 USDC (> $10 MinTradeNtl)
    pub live_start_min_utc: u16, // 795 (13:15 UTC) Crypto; 875 (14:35 UTC EST) Equities; 750 (12:30 UTC) Commodities
    pub live_end_min_utc: u16, // 1230 (20:30 UTC) Crypto; 975 (16:15 UTC EST) Equities; 1170 (19:30 UTC) Commodities
    pub use_us_dst_shift: bool, // Shift -60m during US EDT for Equities
    pub offpeak_min_ker: Option<f64>,
    pub offpeak_min_rel_vol: Option<f64>,
    pub offpeak_min_btc_ret_60m: Option<f64>,
    pub max_squeeze_ratio: f64,       // 0.88 - 0.92
    pub min_rel_volume: f64,          // 1.65 - 1.80
    pub min_hurst_60m: f64,           // 0.48 - 0.50
    pub min_clv_conviction: f64,      // 0.45 - 0.50
    pub min_obi_retest: f64,          // +0.22 to +0.28
    pub retest_limit_offset_atr: f64, // 0.04 - 0.05
    pub retest_stop_atr: f64,         // 0.32 - 0.35
    pub retest_ttl_secs: u64,         // 600 to 900 seconds (10-15 mins)
}

impl TrendEngineConfig {
    /// Returns the calibrated Policy C configuration for any symbol in the 15-bot cluster.
    /// `env_equity_fraction` defaults to 0.35 and `env_trend_risk_fraction` defaults to 0.28.
    pub fn for_symbol(
        symbol: &str,
        env_equity_fraction: f64,
        env_trend_risk_fraction: f64,
    ) -> Self {
        let s = symbol.to_uppercase();
        let env_eq = if env_equity_fraction.is_finite() && env_equity_fraction > 0.0 {
            env_equity_fraction
        } else {
            0.35
        };
        let env_scale = if env_trend_risk_fraction.is_finite() && env_trend_risk_fraction > 0.0 {
            (env_trend_risk_fraction / 0.28).clamp(0.25, 1.25)
        } else {
            1.0
        };

        match s.as_str() {
            "BTC" | "ETH" | "SOL" => Self {
                tier: TrendAssetTier::CryptoMajor,
                equity_fraction_eng_a: env_eq,
                trend_risk_fraction: (0.28 * env_scale).clamp(0.10, 0.35),
                max_trend_notional_usdc: 55.0,
                min_trade_notional_usdc: 12.0,
                live_start_min_utc: 795, // 13:15 UTC
                live_end_min_utc: 1230,  // 20:30 UTC
                use_us_dst_shift: false,
                offpeak_min_ker: Some(0.62),
                offpeak_min_rel_vol: Some(2.80),
                offpeak_min_btc_ret_60m: Some(0.0175),
                max_squeeze_ratio: 0.90,
                min_rel_volume: 1.65,
                min_hurst_60m: 0.48,
                min_clv_conviction: 0.45,
                min_obi_retest: 0.22,
                retest_limit_offset_atr: 0.05,
                retest_stop_atr: 0.35,
                retest_ttl_secs: 720, // 12 mins
            },
            "XYZ:NVDA" | "NVDA" | "XYZ:TSLA" | "TSLA" | "XYZ:XYZ100" | "XYZ100" | "XYZ:AAPL"
            | "AAPL" => Self {
                tier: TrendAssetTier::Hip3UsEquity,
                equity_fraction_eng_a: env_eq.min(0.32),
                trend_risk_fraction: (0.30 * env_scale).clamp(0.10, 0.35),
                max_trend_notional_usdc: 55.0,
                min_trade_notional_usdc: 12.0,
                live_start_min_utc: 875, // 14:35 UTC EST (13:35 UTC EDT)
                live_end_min_utc: 975,   // 16:15 UTC EST (15:15 UTC EDT)
                use_us_dst_shift: true,
                offpeak_min_ker: None,
                offpeak_min_rel_vol: None,
                offpeak_min_btc_ret_60m: None,
                max_squeeze_ratio: 0.92,
                min_rel_volume: 1.75,
                min_hurst_60m: 0.48,
                min_clv_conviction: 0.45,
                min_obi_retest: 0.25,
                retest_limit_offset_atr: 0.05,
                retest_stop_atr: 0.35,
                retest_ttl_secs: 720, // 12 mins
            },
            "XYZ:GOLD" | "GOLD" | "XYZ:SILVER" | "SILVER" => Self {
                tier: TrendAssetTier::Hip3Commodity,
                equity_fraction_eng_a: env_eq.min(0.32),
                trend_risk_fraction: (0.28 * env_scale).clamp(0.10, 0.35),
                max_trend_notional_usdc: 52.0,
                min_trade_notional_usdc: 12.0,
                live_start_min_utc: 750, // 12:30 UTC
                live_end_min_utc: 1170,  // 19:30 UTC
                use_us_dst_shift: false,
                offpeak_min_ker: Some(0.65),
                offpeak_min_rel_vol: Some(3.00),
                offpeak_min_btc_ret_60m: None,
                max_squeeze_ratio: 0.90,
                min_rel_volume: 1.65,
                min_hurst_60m: 0.48,
                min_clv_conviction: 0.45,
                min_obi_retest: 0.22,
                retest_limit_offset_atr: 0.05,
                retest_stop_atr: 0.32,
                retest_ttl_secs: 900, // 15 mins
            },
            // Crypto Alts: AVAX, HYPE, NEAR, XRP, SUI, LINK, ZEC
            _ => Self {
                tier: TrendAssetTier::CryptoAlt,
                equity_fraction_eng_a: env_eq,
                trend_risk_fraction: (0.25 * env_scale).clamp(0.10, 0.32),
                max_trend_notional_usdc: 50.0,
                min_trade_notional_usdc: 12.0,
                live_start_min_utc: 795, // 13:15 UTC
                live_end_min_utc: 1230,  // 20:30 UTC
                use_us_dst_shift: false,
                offpeak_min_ker: None,
                offpeak_min_rel_vol: None,
                offpeak_min_btc_ret_60m: None,
                max_squeeze_ratio: 0.88,
                min_rel_volume: 1.80,
                min_hurst_60m: 0.50,
                min_clv_conviction: 0.50,
                min_obi_retest: 0.28,
                retest_limit_offset_atr: 0.04,
                retest_stop_atr: 0.35,
                retest_ttl_secs: 600, // 10 mins
            },
        }
    }

    /// Computes the loss-parity & fractional-Kelly capped Engine B notional in USDC.
    #[inline]
    pub fn compute_trend_notional_usdc(&self, account_equity_usdc: f64) -> f64 {
        if !account_equity_usdc.is_finite() || account_equity_usdc <= 0.0 {
            return self.min_trade_notional_usdc;
        }
        let eng_a_notional = account_equity_usdc * self.equity_fraction_eng_a;
        let raw_trend_notional = eng_a_notional * self.trend_risk_fraction;
        raw_trend_notional
            .min(self.max_trend_notional_usdc)
            .max(self.min_trade_notional_usdc)
    }
}

/// Evaluates whether Engine B is permitted to execute `Live` or must route to `Shadow`
/// (CounterfactualTracker) based on UTC session hours, US DST, RWA schedules, and Macro Shock Override criteria.
pub struct SessionTrendGate;

impl SessionTrendGate {
    #[inline]
    pub fn evaluate_session_mode(
        cfg: &TrendEngineConfig,
        timestamp_sec: u64,
        weekday_utc: u8,
        is_mode_mean_reversion: bool,
        ker_15m: f64,
        rel_vol_15m: f64,
        btc_ret_60m_abs: f64,
    ) -> ExecutionMode {
        // 1. Respect Engine A MODE_MEANREVERSION exclusivity lock (Alpha-Protective)
        if is_mode_mean_reversion {
            return ExecutionMode::Shadow {
                reason: "BLOCKED_BY_MODE_MEANREVERSION",
            };
        }

        let min_of_day = ((timestamp_sec / 60) % 1440) as u16;

        // 2. Weekend / CME Globex maintenance lockout for HIP-3 Equities and Commodities
        match cfg.tier {
            TrendAssetTier::Hip3UsEquity => {
                if weekday_utc == 0 || weekday_utc == 6 {
                    return ExecutionMode::Shadow {
                        reason: "WEEKEND_RWA_LOCKOUT",
                    };
                }
            }
            TrendAssetTier::Hip3Commodity => {
                let cme_closed = weekday_utc == 6
                    || (weekday_utc == 0 && min_of_day < 1395)
                    || (weekday_utc == 5 && min_of_day >= 1245)
                    || (1315..=1390).contains(&min_of_day);
                if cme_closed {
                    return ExecutionMode::Shadow {
                        reason: "WEEKEND_RWA_LOCKOUT",
                    };
                }
            }
            _ => {}
        }

        // Adjust primary window by -60m during US Daylight Saving Time (EDT) for US Equities
        let dst_shift_mins: u16 = if cfg.use_us_dst_shift {
            if let chrono::LocalResult::Single(dt) = Utc.timestamp_opt(timestamp_sec as i64, 0) {
                if crate::ou_process::is_us_daylight_saving_date(dt.year(), dt.month(), dt.day()) {
                    60
                } else {
                    0
                }
            } else {
                0
            }
        } else {
            0
        };

        let start_min = cfg.live_start_min_utc.saturating_sub(dst_shift_mins);
        let end_min = cfg.live_end_min_utc.saturating_sub(dst_shift_mins);
        let in_primary_window = min_of_day >= start_min && min_of_day <= end_min;

        if in_primary_window {
            return ExecutionMode::Live;
        }

        // 3. Off-Peak Asian/European Chop Window:
        // Force Shadow unless tier supports verified 3-sigma Macro Volatility Ignition override
        if let (Some(min_ker), Some(min_rv)) = (cfg.offpeak_min_ker, cfg.offpeak_min_rel_vol) {
            let btc_ok = match cfg.offpeak_min_btc_ret_60m {
                Some(min_btc) => btc_ret_60m_abs >= min_btc,
                None => true,
            };
            if ker_15m >= min_ker && rel_vol_15m >= min_rv && btc_ok {
                return ExecutionMode::Live;
            }
        }

        ExecutionMode::Shadow {
            reason: "OFF_PEAK_ASIAN_CHOP_SHADOW_GATE",
        }
    }
}

/// Evaluates the Coiled-Spring Pre-Breakout Squeeze, Volume Impulse, and Microstructure Filter.
pub struct BreakoutFilter;

#[derive(Debug, Clone, Copy)]
pub struct BreakoutTelemetry {
    #[allow(dead_code)]
    pub bar_open: f64,
    pub bar_high: f64,
    pub bar_low: f64,
    pub bar_close: f64,
    pub donchian_high_12_15m: f64,
    pub donchian_low_12_15m: f64,
    pub atr_15m: f64,
    pub min_squeeze_ratio_90m: f64, // min_{k in 1..=6} (2.0 * sigma_{20,15m} / (1.5 * ATR_{20,15m}))
    pub rel_vol_15m: f64,
    pub hurst_60m: f64,
    pub l2_obi: f64,
    pub oi_delta_15m_pct: f64,
}

impl BreakoutFilter {
    #[inline]
    pub fn check_coiled_spring_gates(
        cfg: &TrendEngineConfig,
        t: &BreakoutTelemetry,
        dir: TrendDirection,
    ) -> Result<(), &'static str> {
        if !t.atr_15m.is_finite()
            || t.atr_15m <= 0.0
            || !t.bar_close.is_finite()
            || !t.bar_high.is_finite()
            || !t.bar_low.is_finite()
        {
            return Err("REJECT_INVALID_TELEMETRY");
        }
        if !t.min_squeeze_ratio_90m.is_finite() || t.min_squeeze_ratio_90m > cfg.max_squeeze_ratio {
            return Err("REJECT_NO_PRE_BREAKOUT_SQUEEZE");
        }
        if !t.rel_vol_15m.is_finite() || t.rel_vol_15m < cfg.min_rel_volume {
            return Err("REJECT_LOW_VOLUME_DRIFT_BREAKOUT");
        }
        if !t.hurst_60m.is_finite() || t.hurst_60m < cfg.min_hurst_60m {
            return Err("REJECT_LOW_HURST_MEAN_REVERTING_REGIME");
        }
        if !t.oi_delta_15m_pct.is_finite() || t.oi_delta_15m_pct < -0.15 {
            return Err("REJECT_LIQUIDATION_FLUSH_COLLAPSING_OI");
        }

        let range = (t.bar_high - t.bar_low).max(1e-12);
        let clv = ((2.0 * t.bar_close - t.bar_high - t.bar_low) / range).clamp(-1.0, 1.0);
        let d = dir.sign();

        if d * clv < cfg.min_clv_conviction {
            return Err("REJECT_WEAK_CANDLE_CLOSE_CONVICTION");
        }
        let body_acceptance = match dir {
            TrendDirection::Long => t.bar_close - t.donchian_high_12_15m,
            TrendDirection::Short => t.donchian_low_12_15m - t.bar_close,
        };
        if body_acceptance < 0.08 * t.atr_15m {
            return Err("REJECT_INSUFFICIENT_BOUNDARY_ACCEPTANCE");
        }

        Ok(())
    }
}

/// Two-Stage Persistence + Passive `Alo` Maker Retest State Machine for Engine B.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RetestState {
    Idle,
    Stage1ConfirmingPersistence {
        dir: TrendDirection,
        broken_donchian_px: f64,
        confirm_bars_5m: u8,
        armed_at_sec: u64,
    },
    Stage2WaitingMakerRetest {
        dir: TrendDirection,
        broken_donchian_px: f64,
        alo_limit_px: f64,
        post_fill_stop_px: f64,
        initial_tp1_px: f64,
        posted_at_sec: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendOrderAction {
    None,
    LogShadowCounterfactual {
        dir: TrendDirection,
        virtual_entry_px: f64,
        reason: &'static str,
    },
    PostAloMakerRetestOrder {
        dir: TrendDirection,
        limit_px: f64,
        stop_loss_px: f64,
        tp1_px: f64,
        notional_usdc: f64,
        ttl_secs: u64,
    },
    CancelRestingRetestOrder {
        reason: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct EngineBTrendManager {
    pub state: RetestState,
}

impl Default for EngineBTrendManager {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineBTrendManager {
    pub fn new() -> Self {
        Self {
            state: RetestState::Idle,
        }
    }

    #[inline]
    pub fn is_waiting_retest(&self) -> bool {
        matches!(self.state, RetestState::Stage2WaitingMakerRetest { .. })
    }

    #[inline]
    pub fn is_stage1_armed(&self) -> bool {
        matches!(self.state, RetestState::Stage1ConfirmingPersistence { .. })
    }

    #[inline]
    pub fn on_order_filled(&mut self) {
        if matches!(self.state, RetestState::Stage2WaitingMakerRetest { .. }) {
            self.state = RetestState::Idle;
        }
    }

    #[inline]
    pub fn on_order_cancelled_or_rejected(&mut self) {
        if matches!(self.state, RetestState::Stage2WaitingMakerRetest { .. }) {
            self.state = RetestState::Idle;
        }
    }

    #[inline]
    pub fn disarm(&mut self) {
        self.state = RetestState::Idle;
    }

    /// Evaluates 5-minute close persistence and manages Stage 2 `Alo` Maker Retest orders.
    #[allow(clippy::too_many_arguments)]
    pub fn on_5m_bar_close(
        &mut self,
        cfg: &TrendEngineConfig,
        t: &BreakoutTelemetry,
        timestamp_sec: u64,
        weekday_utc: u8,
        account_equity_usdc: f64,
        is_mode_mean_reversion: bool,
        ker_15m: f64,
        btc_ret_60m_abs: f64,
    ) -> TrendOrderAction {
        let candidate_dir = if t.bar_close > t.donchian_high_12_15m {
            Some((TrendDirection::Long, t.donchian_high_12_15m))
        } else if t.bar_close < t.donchian_low_12_15m {
            Some((TrendDirection::Short, t.donchian_low_12_15m))
        } else {
            None
        };

        match self.state {
            RetestState::Idle => {
                let Some((dir, bound_px)) = candidate_dir else {
                    return TrendOrderAction::None;
                };

                // 1. Check Session Gating first
                let mode = SessionTrendGate::evaluate_session_mode(
                    cfg,
                    timestamp_sec,
                    weekday_utc,
                    is_mode_mean_reversion,
                    ker_15m,
                    t.rel_vol_15m,
                    btc_ret_60m_abs,
                );
                if let ExecutionMode::Shadow { reason } = mode {
                    return TrendOrderAction::LogShadowCounterfactual {
                        dir,
                        virtual_entry_px: t.bar_close,
                        reason,
                    };
                }

                // 2. Check Coiled-Spring Squeeze & Volume Impulse Filter
                if let Err(reject_reason) = BreakoutFilter::check_coiled_spring_gates(cfg, t, dir) {
                    return TrendOrderAction::LogShadowCounterfactual {
                        dir,
                        virtual_entry_px: t.bar_close,
                        reason: reject_reason,
                    };
                }

                // First 5m bar confirmed outside Donchian channel: Enter Stage 1
                self.state = RetestState::Stage1ConfirmingPersistence {
                    dir,
                    broken_donchian_px: bound_px,
                    confirm_bars_5m: 1,
                    armed_at_sec: timestamp_sec,
                };
                TrendOrderAction::None
            }

            RetestState::Stage1ConfirmingPersistence {
                dir,
                broken_donchian_px,
                confirm_bars_5m,
                armed_at_sec,
            } => {
                // Re-verify session mode before promoting Stage 1 -> Stage 2
                let mode = SessionTrendGate::evaluate_session_mode(
                    cfg,
                    timestamp_sec,
                    weekday_utc,
                    is_mode_mean_reversion,
                    ker_15m,
                    t.rel_vol_15m,
                    btc_ret_60m_abs,
                );
                if let ExecutionMode::Shadow { reason } = mode {
                    self.state = RetestState::Idle;
                    return TrendOrderAction::LogShadowCounterfactual {
                        dir,
                        virtual_entry_px: t.bar_close,
                        reason,
                    };
                }

                let d = dir.sign();
                let still_outside = d * (t.bar_close - broken_donchian_px) >= 0.05 * t.atr_15m;
                if !still_outside || timestamp_sec.saturating_sub(armed_at_sec) > 600 {
                    // Failed 2-bar persistence (Immediate fakeout snapback caught!)
                    self.state = RetestState::Idle;
                    return TrendOrderAction::LogShadowCounterfactual {
                        dir,
                        virtual_entry_px: t.bar_close,
                        reason: "STAGE1_PERSISTENCE_SNAPBACK_AVOIDED",
                    };
                }

                if confirm_bars_5m + 1 >= 2 {
                    if d * t.l2_obi >= cfg.min_obi_retest {
                        // 2 consecutive 5m bars confirmed + L2 OBI aligned: Post Stage 2 Alo Retest Order!
                        // When confirmation close has expanded > 0.35 * ATR beyond broken_donchian_px,
                        // anchor dynamically to a shallow 35% flag pullback so Stage 2 fills real runners.
                        let extension = d * (t.bar_close - broken_donchian_px);
                        let dynamic_offset = if extension > 0.35 * t.atr_15m {
                            (0.35 * extension).min(extension - 0.10 * t.atr_15m)
                        } else {
                            cfg.retest_limit_offset_atr * t.atr_15m
                        };
                        let eff_offset =
                            dynamic_offset.max(cfg.retest_limit_offset_atr * t.atr_15m);
                        let alo_limit_px = broken_donchian_px + d * eff_offset;
                        let total_stop_dist =
                            (cfg.retest_limit_offset_atr + cfg.retest_stop_atr) * t.atr_15m;
                        let post_fill_stop_px = alo_limit_px - d * total_stop_dist;
                        let initial_tp1_px = alo_limit_px + d * 1.50 * t.atr_15m;
                        let notional_usdc = cfg.compute_trend_notional_usdc(account_equity_usdc);

                        self.state = RetestState::Stage2WaitingMakerRetest {
                            dir,
                            broken_donchian_px,
                            alo_limit_px,
                            post_fill_stop_px,
                            initial_tp1_px,
                            posted_at_sec: timestamp_sec,
                        };

                        return TrendOrderAction::PostAloMakerRetestOrder {
                            dir,
                            limit_px: alo_limit_px,
                            stop_loss_px: post_fill_stop_px,
                            tp1_px: initial_tp1_px,
                            notional_usdc,
                            ttl_secs: cfg.retest_ttl_secs,
                        };
                    } else {
                        self.state = RetestState::Idle;
                        return TrendOrderAction::LogShadowCounterfactual {
                            dir,
                            virtual_entry_px: t.bar_close,
                            reason: "STAGE1_OBI_RETEST_HURDLE_FAIL",
                        };
                    }
                }

                self.state = RetestState::Idle;
                TrendOrderAction::None
            }

            RetestState::Stage2WaitingMakerRetest {
                dir,
                broken_donchian_px,
                posted_at_sec,
                ..
            } => {
                let d = dir.sign();
                // Cancel resting Alo retest order if TTL expires, L2 OBI flips adverse, or price falls back inside channel
                if timestamp_sec.saturating_sub(posted_at_sec) >= cfg.retest_ttl_secs {
                    self.state = RetestState::Idle;
                    return TrendOrderAction::CancelRestingRetestOrder {
                        reason: "RETEST_TTL_EXPIRED",
                    };
                }
                if d * t.l2_obi < 0.0 || d * (t.bar_close - broken_donchian_px) < -0.10 * t.atr_15m
                {
                    self.state = RetestState::Idle;
                    return TrendOrderAction::CancelRestingRetestOrder {
                        reason: "FAST_SENTINEL_OBI_OR_STRUCTURE_ABORT",
                    };
                }
                TrendOrderAction::None
            }
        }
    }
}

/// Computes the Policy C 90-minute rolling minimum Bollinger-to-Keltner squeeze ratio:
/// `min_{k in 1..=6} (2.0 * sigma_{20, 15m}(t - k) / (1.5 * ATR_{20, 15m}(t - k)))`.
/// Returns `1.0` (no squeeze) when fewer than 21 15m bars or valid ATRs are available.
pub fn calculate_min_squeeze_ratio_90m(bars_15m: &[CandleBar]) -> f64 {
    let n15 = bars_15m.len();
    if n15 < 21 {
        return 1.0;
    }
    let mut min_sq = f64::MAX;
    for k in 1..=6 {
        let end = n15.saturating_sub(k);
        if end >= 20 {
            let sub = &bars_15m[end - 20..end];
            let mean_c = sub.iter().map(|b| b.close).sum::<f64>() / 20.0;
            let var_c = sub.iter().map(|b| (b.close - mean_c).powi(2)).sum::<f64>() / 20.0;
            let std_20 = var_c.sqrt();
            let atr_opt = if end >= 21 {
                calculate_atr(&bars_15m[..end], 20)
            } else {
                calculate_atr(sub, 19)
            };
            if let Some(atr_sub) = atr_opt.filter(|&a| a.is_finite() && a > 1e-9) {
                let sq = (2.0 * std_20) / (1.5 * atr_sub);
                if sq.is_finite() && sq < min_sq {
                    min_sq = sq;
                }
            }
        }
    }
    if min_sq < f64::MAX {
        min_sq
    } else {
        1.0
    }
}

/// Computes the causal 15-minute relative volume impulse `R_V = V_15m / EMA_20(V_15m)`.
/// Uses `finalized_15m` when a 15m bar just closed, or the sum of the last 3 completed 5m bars
/// on intermediate 5m closes so volume is always measured over a full 15-minute window.
pub fn calculate_rel_vol_15m(
    prior_15m_bars: &[CandleBar],
    recent_5m_bars: &[CandleBar],
    finalized_15m: Option<&CandleBar>,
) -> f64 {
    let v_curr = if let Some(c15) = finalized_15m {
        c15.volume
    } else if recent_5m_bars.len() >= 3 {
        recent_5m_bars[recent_5m_bars.len() - 3..]
            .iter()
            .map(|b| b.volume)
            .sum()
    } else {
        return 0.0;
    };

    if !v_curr.is_finite() || v_curr <= 0.0 || prior_15m_bars.is_empty() {
        return 0.0;
    }

    let vols: Vec<f64> = prior_15m_bars
        .iter()
        .map(|b| b.volume)
        .filter(|&v| v.is_finite() && v > 0.0)
        .collect();
    if vols.is_empty() {
        return 0.0;
    }

    let ema_vol = if vols.len() >= 20 {
        calculate_ema(&vols, 20).unwrap_or(0.0)
    } else {
        vols.iter().sum::<f64>() / (vols.len() as f64)
    };

    if ema_vol > 1e-9 {
        v_curr / ema_vol
    } else {
        0.0
    }
}

/// Computes Kaufman Efficiency Ratio over `period` bars (`KER in [0.0, 1.0]`).
pub fn calculate_ker_n(bars: &[CandleBar], period: usize) -> f64 {
    if period == 0 || bars.len() <= period {
        return 0.0;
    }
    let slice = &bars[bars.len() - period - 1..];
    let net_disp = (slice[period].close - slice[0].close).abs();
    let mut path_sum = 0.0;
    for i in 1..=period {
        path_sum += (slice[i].close - slice[i - 1].close).abs();
    }
    if path_sum <= 1e-12 {
        0.0
    } else {
        (net_disp / path_sum).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_donchian_breakout_detection() {
        let mut bars = Vec::new();
        for i in 0..25 {
            bars.push(CandleBar {
                time_open: i * 900_000,
                open: 100.0,
                high: 100.0 + (i as f64),
                low: 95.0,
                close: 99.0 + (i as f64),
                volume: 10.0,
            });
        }

        let channel = calculate_donchian(&bars, 20).expect("Channel calculated");
        // For the last 20 bars (indices 5..24), max high is 100 + 24 = 124.0, min low is 95.0
        assert_eq!(channel.upper, 124.0);
        assert_eq!(channel.lower, 95.0);
        assert_eq!(channel.middle, (124.0 + 95.0) / 2.0);
    }

    #[test]
    fn test_wilder_atr_calculation() {
        let mut bars = Vec::new();
        for i in 0..30 {
            bars.push(CandleBar {
                time_open: i * 900_000,
                open: 100.0,
                high: 105.0,
                low: 95.0, // TR = 10.0 every bar
                close: 100.0,
                volume: 10.0,
            });
        }

        let atr = calculate_atr(&bars, 20).expect("ATR calculated");
        assert!(
            (atr - 10.0).abs() < 1e-4,
            "ATR should converge to 10.0, got: {}",
            atr
        );
    }

    #[test]
    fn test_ema_trend_alignment() {
        let mut closes = Vec::new();
        for i in 0..80 {
            // Strong uptrend
            closes.push(100.0 + (i as f64) * 2.0);
        }

        let alignment = check_trend_alignment(&closes);
        assert_eq!(alignment, TrendAlignment::Bullish);

        // Strong downtrend
        let downtrend: Vec<f64> = closes.iter().rev().cloned().collect();
        let down_alignment = check_trend_alignment(&downtrend);
        assert_eq!(down_alignment, TrendAlignment::Bearish);
    }

    #[test]
    fn test_obi_score_calculation() {
        let bids = vec![(100.0, 10.0), (99.0, 15.0), (98.0, 5.0)]; // sum = 30.0
        let asks = vec![(101.0, 5.0), (102.0, 5.0)]; // sum = 10.0

        let obi = calculate_obi_score(&bids, &asks, 5);
        // (30 - 10) / (30 + 10) = 20 / 40 = 0.50
        assert!((obi - 0.50).abs() < 1e-4);

        // Reverse: asks dominate
        let obi_rev = calculate_obi_score(&asks, &bids, 5);
        assert!((obi_rev - (-0.50)).abs() < 1e-4);
    }

    #[test]
    fn test_chandelier_stop_ratchet_never_loosens() {
        let mut chandelier = ChandelierExitState::new(Side::Long, 100.0, 2.0, 3.0);
        // Initial stop: 100.0 - 3.0 * 2.0 = 94.0
        assert_eq!(chandelier.current_stop, 94.0);

        // Price rises to 110.0 with ATR 2.0, P_T = 0.90
        chandelier.update(110.0, 102.0, 2.0, 0.90);
        // New candidate stop: 110.0 - 3.0 * 2.0 = 104.0
        assert_eq!(chandelier.current_stop, 104.0);

        // Price drops to 105.0 (pullback)
        chandelier.update(106.0, 105.0, 2.0, 0.90);
        // Peak is still 110.0, candidate is still 104.0 -> stop NEVER loosens!
        assert_eq!(chandelier.current_stop, 104.0);
        assert!(!chandelier.is_stopped_out(105.0));

        // Price crosses below stop
        assert!(chandelier.is_stopped_out(103.5));
    }

    #[test]
    fn test_chandelier_probability_compression_legacy_3x() {
        // Legacy test validating linear contraction when initialized with base multiplier = 3.0x
        let mut chandelier = ChandelierExitState::new(Side::Long, 100.0, 2.0, 3.0);
        chandelier.update(110.0, 105.0, 2.0, 0.90);
        assert_eq!(chandelier.current_stop, 104.0);

        // HMM trend probability decays to 0.40 (< 0.60 threshold)
        // Multiplier compresses from 3.0 to 1.5 ATR
        // Candidate stop: 110.0 - 1.5 * 2.0 = 107.0!
        chandelier.update(110.0, 108.0, 2.0, 0.40);
        assert_eq!(chandelier.current_stop, 107.0);
    }

    #[test]
    fn test_chandelier_probability_compression_calibrated_2x() {
        // Base multiplier = 2.0x, ATR = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Long, 100.0, 2.0, 2.0);
        // Initial stop: 100.0 - 2.0 * 2.0 = 96.0
        assert_eq!(chandelier.current_stop, 96.0);

        // Price rises to 110.0 with P_T = 0.90 (active mult = 2.0)
        chandelier.update(110.0, 105.0, 2.0, 0.90);
        // Stop: 110.0 - 2.0 * 2.0 = 106.0
        assert_eq!(chandelier.current_stop, 106.0);

        // HMM trend probability decays to 0.40 (< 0.55 threshold)
        // Multiplier compresses from 2.0 to 1.0 ATR
        // Candidate stop: 110.0 - 1.0 * 2.0 = 108.0!
        chandelier.update(110.0, 108.0, 2.0, 0.40);
        assert_eq!(chandelier.current_stop, 108.0);
        assert_eq!(chandelier.active_multiplier, 1.0);
    }

    #[test]
    fn test_chandelier_short_locks_profit_with_2x_base() {
        // Short entry @ 10.77, ATR = 0.10, base mult = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Short, 10.77, 0.10, 2.0);
        // Initial stop: 10.77 + 2.0 * 0.10 = 10.97
        assert!((chandelier.current_stop - 10.97).abs() < 1e-6);

        // Price drops to 10.63, P_T decays to 0.30
        // Extreme becomes 10.63, mult contracts to 1.0 ATR
        // Stop: 10.63 + 1.0 * 0.10 = 10.73! (STRICTLY BELOW entry 10.77!)
        chandelier.update(10.70, 10.63, 0.10, 0.30);
        assert!((chandelier.current_stop - 10.73).abs() < 1e-6);
        assert!(chandelier.current_stop < chandelier.entry_price);
    }

    #[test]
    fn test_chandelier_breakeven_floor_short() {
        // Short entry @ 10.00, ATR = 0.20, base_multiplier = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Short, 10.00, 0.20, 2.0);
        // Initial stop: 10.00 + 2.0 * 0.20 = 10.40
        assert_eq!(chandelier.current_stop, 10.40);

        // Price drops to 9.80 (favorable excursion = 0.20 = 1.0 * ATR)
        // P_T remains high (0.85 -> active mult = 2.0)
        // Normal Chandelier stop would be: 9.80 + 2.0 * 0.20 = 10.20
        // BUT with enable_breakeven_ratchet: true,
        // excursion >= 1.0 ATR triggers Breakeven Floor: entry - 10 bps = 10.00 * 0.9990 = 9.99!
        let stop = chandelier.update_with_ratchet_and_cap(9.85, 9.80, 0.20, 0.85, None, true);
        assert!(
            stop <= 9.99,
            "Breakeven floor must lock stop below entry covering fees (stop: {})",
            stop
        );
        assert!(stop < chandelier.entry_price);
    }

    #[test]
    fn test_chandelier_profit_giveback_ratchet_long() {
        // Long entry @ 100.00, ATR = 2.00, base_multiplier = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Long, 100.00, 2.00, 2.0);
        // Initial stop: 100.00 - 2.0 * 2.0 = 96.00
        assert_eq!(chandelier.current_stop, 96.00);

        // Price rallies to 104.00 (favorable excursion = 4.00 = 2.0 * ATR >= 1.5 * ATR)
        // P_T remains high (0.85 -> active mult = 2.0)
        // Normal Chandelier stop would be: 104.00 - 2.0 * 2.0 = 100.00 (breakeven)
        // BUT with 50% profit giveback ratchet:
        // protects at least 50% of peak progress: 100.00 + 0.5 * 4.00 = 102.00!
        let stop = chandelier.update_with_ratchet_and_cap(104.00, 102.00, 2.00, 0.85, None, true);
        assert!(
            stop >= 102.00,
            "50% profit giveback ratchet must protect at least 102.00 (stop: {})",
            stop
        );
    }

    #[test]
    fn test_chandelier_tiered_profit_lock_ladder() {
        // Short entry @ 10.00, ATR = 0.10, base_multiplier = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Short, 10.00, 0.10, 2.0);

        // 1. Gain reaches +2.0% (price drops to 9.80): stop locks at least +1.0% profit (<= 9.90)
        let stop_tier2 = chandelier.update_with_ratchet_and_cap(9.85, 9.80, 0.10, 0.85, None, true);
        assert!(
            stop_tier2 <= 9.90,
            "Tier 2 (+2% gain) must lock in at least +1.0% profit (stop: {})",
            stop_tier2
        );

        // 2. Gain reaches +3.0% (price drops to 9.70): stop locks at least +2.0% profit (<= 9.80)
        let stop_tier3 = chandelier.update_with_ratchet_and_cap(9.75, 9.70, 0.10, 0.85, None, true);
        assert!(
            stop_tier3 <= 9.80,
            "Tier 3 (+3% gain) must lock in at least +2.0% profit (stop: {})",
            stop_tier3
        );

        // 3. Gain reaches +5.0% (price drops to 9.50): delta_fav = 0.50, locks 70% of peak progress (0.50 * 0.70 = 0.35 -> stop <= 9.65)
        let stop_tier4 = chandelier.update_with_ratchet_and_cap(9.55, 9.50, 0.10, 0.85, None, true);
        assert!(
            stop_tier4 <= 9.65,
            "Tier 4 (Super-Trend) must protect at least 70% of peak gain (stop: {})",
            stop_tier4
        );
    }

    #[test]
    fn test_shadow_trade_pnl_calculation() {
        let mut trade = ShadowTrade {
            symbol: "SOL".to_string(),
            side: Side::Long,
            entry_time: Utc::now().to_rfc3339(),
            entry_price: 100.0,
            size: 1.0,
            initial_stop: 94.0,
            current_stop: 94.0,
            extreme_favorable_price: 100.0,
            atr_at_entry: 2.0,
            active_multiplier: 3.0,
            is_active: true,
            exit_time: None,
            exit_price: None,
            pnl_usdc: None,
            return_bps: None,
            exit_reason: None,
            is_portfolio_selected: true,
            lease_token: Some("test_token".to_string()),
        };

        // Exit at 105.0
        let pnl = record_shadow_exit(&mut trade, 105.0, "Chandelier Stop");
        // Gross = $5.0, friction = $0.11226 (maker entry + taker exit + 5 bps slippage) -> net ~ $4.8877
        assert!(pnl > 4.80 && pnl < 4.95);
        assert!(!trade.is_active);
    }

    #[test]
    fn test_l2_obi_tracker_physical_time_decay() {
        let mut tracker = L2ObiTracker::default();
        let bids = vec![(100.0, 10.0), (99.0, 10.0)];
        let asks = vec![(101.0, 5.0), (102.0, 5.0)];

        let t0 = 1_000_000;
        tracker.update(&bids, &asks, t0, t0);
        assert!(tracker.instant_obi > 0.0, "Bids outweigh asks");
        assert_eq!(tracker.ewma_1s_obi, tracker.instant_obi);

        // 1 second (1000ms) later, asks heavily surge
        let t1 = t0 + 1000;
        let surge_asks = vec![(101.0, 50.0), (102.0, 50.0)];
        tracker.update(&bids, &surge_asks, t1, t1);
        assert!(
            tracker.instant_obi < -0.50,
            "Instantaneous flipped heavily bearish"
        );
        // 1s EWMA should smoothly decay towards negative, but not instantly equal instant_obi
        assert!(tracker.ewma_1s_obi > tracker.instant_obi);

        // Check freshness
        assert!(tracker.is_fresh_and_uncrossed(t1 + 500, 7500));
        assert!(
            !tracker.is_fresh_and_uncrossed(t1 + 8000, 7500),
            "Should expire stale books > 7500ms"
        );
    }

    #[test]
    fn test_feed_quality_telemetry() {
        let mut tracker = L2ObiTracker::default();
        let bids = vec![(100.0, 10.0)];
        let asks = vec![(101.0, 10.0)];

        for i in 1..=20 {
            let t_exch = i * 5400;
            let t_local = t_exch + 250; // 250ms transport lag
            tracker.update(&bids, &asks, t_exch, t_local);
        }

        let qual = tracker
            .compute_feed_quality()
            .expect("Quality stats computed");
        assert_eq!(qual.status, "SIGNAL_GOOD");
        assert!((qual.mean_lag_ms - 250.0).abs() < 1.0);
        assert!((qual.p50_ms - 5400.0).abs() < 50.0);
    }

    #[test]
    fn test_chandelier_single_update_contraction_cap() {
        let mut chandelier = ChandelierExitState::new(Side::Long, 100.0, 2.0, 3.0);
        chandelier.update_with_cap(110.0, 105.0, 2.0, 0.90, Some(0.5));
        assert_eq!(chandelier.current_stop, 104.0);
        assert_eq!(chandelier.active_multiplier, 3.0);

        // Sudden drop in P_T to 0.40 (target is 1.5 ATR)
        // With cap = 0.5, active multiplier contracts from 3.0 to 2.5 (not straight to 1.5)
        chandelier.update_with_cap(110.0, 108.0, 2.0, 0.40, Some(0.5));
        assert_eq!(chandelier.active_multiplier, 2.5);
        // Candidate stop: 110.0 - 2.5 * 2.0 = 105.0
        assert_eq!(chandelier.current_stop, 105.0);

        // Next 15m update: contracts further by 0.5 to 2.0
        chandelier.update_with_cap(110.0, 108.0, 2.0, 0.40, Some(0.5));
        assert_eq!(chandelier.active_multiplier, 2.0);
        assert_eq!(chandelier.current_stop, 106.0);
    }

    #[test]
    fn test_dynamic_ewma_recalibration_low_latency_to_jitter() {
        let mut tracker = L2ObiTracker::default();
        let bids = vec![(100.0, 10.0)];
        let asks = vec![(101.0, 10.0)];

        // Phase 1: feed arrives with nominal low latency snapshot cadence (p50 = 5400ms, MAD ~ 10ms)
        for i in 1..=12 {
            let t = i * 5400;
            tracker.update(&bids, &asks, t, t + 200);
        }
        assert_eq!(tracker.current_regime_label, "LowLatency");
        assert_eq!(tracker.tau_1_ms, 10_000.0);
        assert_eq!(tracker.tau_2_ms, 30_000.0);

        // Phase 2: feed experiences high jitter and packet gaps (p95 > 7500ms, e.g. 8500ms / 8000ms)
        // Fail-Safe: switches instantly on cycle 1 to JitterSubsampled
        for i in 13..=24 {
            let dt = if i % 2 == 0 { 8500 } else { 8000 };
            let t = tracker.last_local_recv_ms + dt;
            tracker.update(&bids, &asks, t - 200, t);
        }
        assert_eq!(tracker.current_regime_label, "JitterSubsampled");
        assert_eq!(tracker.tau_1_ms, 20_000.0);
        assert_eq!(tracker.tau_2_ms, 60_000.0);

        // Phase 3 (Asymmetric Hysteresis Recovery):
        // 1st cycle of clean traffic (updates 25..=36): must NOT recover immediately (prevents flapping!)
        for _ in 25..=36 {
            let t = tracker.last_local_recv_ms + 5400;
            tracker.update(&bids, &asks, t - 200, t);
        }
        assert_eq!(
            tracker.current_regime_label, "JitterSubsampled",
            "Hysteresis blocks premature flip on 1st cycle"
        );

        // Subsequent clean cycles until >=2 cycles and 60s of stable clean traffic has elapsed:
        for _ in 37..=72 {
            let t = tracker.last_local_recv_ms + 5400;
            tracker.update(&bids, &asks, t - 200, t);
        }
        assert_eq!(
            tracker.current_regime_label, "LowLatency",
            "Recovered to LowLatency after consecutive cycles & 60s"
        );
        assert_eq!(tracker.tau_1_ms, 10_000.0);
        assert_eq!(tracker.tau_2_ms, 30_000.0);
    }

    #[test]
    fn test_satisfies_entry_hurdle_warmup_and_freshness() {
        let mut tracker = L2ObiTracker::default();
        let bids = vec![(100.0, 50.0), (99.0, 50.0)];
        let asks = vec![(101.0, 10.0), (102.0, 10.0)];

        let t0 = 100_000;
        tracker.update(&bids, &asks, t0, t0);

        // 1. Stale book check (> 7500ms)
        assert!(!tracker.satisfies_entry_hurdle(&Side::Long, t0 + 8000).0);

        // 2. Warmup check (<3 updates, <15s continuous)
        let (ok, reason) = tracker.satisfies_entry_hurdle(&Side::Long, t0 + 1000);
        assert!(!ok);
        assert!(reason.contains("Warming Up"));

        // 2nd update at +5400ms
        tracker.update(&bids, &asks, t0 + 5400, t0 + 5400);
        let (ok2, _) = tracker.satisfies_entry_hurdle(&Side::Long, t0 + 6000);
        assert!(!ok2);

        // 3rd update at +16200ms (> 15s elapsed, >= 3 updates)
        tracker.update(&bids, &asks, t0 + 16200, t0 + 16200);
        let (ok3, reason3) = tracker.satisfies_entry_hurdle(&Side::Long, t0 + 16300);
        assert!(ok3);
        assert_eq!(reason3, "Bullish Multiscale Depth Alignment");
    }

    #[test]
    fn test_taker_sweep_obi_asymmetry_satisfies_hurdle() {
        let mut tracker = L2ObiTracker::default();
        let t0 = 100_000;
        tracker.updates_since_reconnect = 10;
        tracker.reconnect_time_ms = t0 - 60_000;
        tracker.last_update_ms = t0;
        tracker.best_bid = 2500.0;
        tracker.best_ask = 2500.1;

        // Simulate ETH taker sweep condition that hit Take Profit (+30.8 bps)
        // inst: -0.052, 1s: -0.044, 5s: -0.024
        tracker.instant_obi = -0.052;
        tracker.ewma_1s_obi = -0.044;
        tracker.ewma_5s_obi = -0.024;

        let (ok, reason) = tracker.satisfies_entry_hurdle(&Side::Long, t0 + 100);
        assert!(
            ok,
            "Taker sweep asymmetry must not strangle valid breakout entry"
        );
        assert_eq!(reason, "Bullish Multiscale Depth Alignment");

        // Toxic sell wall dumping (-0.25) must be vetoed!
        tracker.instant_obi = -0.25;
        let (toxic_ok, toxic_reason) = tracker.satisfies_entry_hurdle(&Side::Long, t0 + 100);
        assert!(!toxic_ok);
        assert!(toxic_reason.contains("Insufficient Bid Depth"));
    }

    static TEST_SHADOW_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_shadow_lease_flock_concurrency_and_fencing() {
        let _guard = TEST_SHADOW_MUTEX.lock().unwrap();
        let _ = fs::remove_file(SHADOW_CLUSTER_LOCK);
        let _ = fs::remove_file(SHADOW_CLUSTER_LOCK_GUARD);
        let _ = fs::remove_file(SHADOW_CLUSTER_COUNTER);

        // 1. Symbol BTC acquires lease
        let (btc_ok, btc_tok) = try_acquire_shadow_cluster_lease("BTC");
        assert!(btc_ok);
        let tok1 = btc_tok.expect("BTC token");

        // 2. While BTC holds lease, ETH cannot acquire
        let (eth_ok, _) = try_acquire_shadow_cluster_lease("ETH");
        assert!(!eth_ok);

        // 3. Refresh by BTC succeeds
        assert!(refresh_shadow_cluster_lease("BTC", &tok1));

        // 4. Stale token refresh fails
        assert!(!refresh_shadow_cluster_lease("BTC", "invalid_stale_token"));

        // 5. Stale token cannot release lease
        release_shadow_cluster_lease("BTC", Some("invalid_token"));
        assert!(std::path::Path::new(SHADOW_CLUSTER_LOCK).exists());

        // 6. Valid release removes lease
        release_shadow_cluster_lease("BTC", Some(&tok1));
        assert!(!std::path::Path::new(SHADOW_CLUSTER_LOCK).exists());

        // 7. ETH can now acquire with incremented fencing token (strictly monotonic across release)
        let (eth_ok2, eth_tok) = try_acquire_shadow_cluster_lease("ETH");
        assert!(eth_ok2);
        let eth_tok_str = eth_tok.expect("ETH token");
        assert!(eth_tok_str.contains("ETH"));

        let eth_lease: ShadowClusterLease =
            serde_json::from_str(&fs::read_to_string(SHADOW_CLUSTER_LOCK).unwrap()).unwrap();
        assert_eq!(
            eth_lease.fencing_token, 2,
            "Fencing token monotonically preserved across releases"
        );

        release_shadow_cluster_lease("ETH", Some(&eth_tok_str));
        let _ = fs::remove_file(SHADOW_CLUSTER_LOCK);
        let _ = fs::remove_file(SHADOW_CLUSTER_LOCK_GUARD);
        let _ = fs::remove_file(SHADOW_CLUSTER_COUNTER);
    }

    #[test]
    fn test_shadow_lease_heartbeat_loss_unselection() {
        let mut trade = ShadowTrade {
            symbol: "AVAX".to_string(),
            side: Side::Long,
            entry_time: "2026-10-08T12:00:00Z".to_string(),
            entry_price: 30.0,
            size: 1.0,
            initial_stop: 28.0,
            current_stop: 28.0,
            extreme_favorable_price: 30.0,
            atr_at_entry: 1.0,
            active_multiplier: 2.0,
            is_active: true,
            exit_time: None,
            exit_price: None,
            pnl_usdc: None,
            return_bps: None,
            exit_reason: None,
            is_portfolio_selected: true,
            lease_token: Some("old_stale_token".to_string()),
        };

        // When heartbeat refresh fails (e.g. lease lost / expired), unselection occurs
        let refresh_ok =
            refresh_shadow_cluster_lease(&trade.symbol, trade.lease_token.as_deref().unwrap());
        if !refresh_ok {
            trade.is_portfolio_selected = false;
            trade.lease_token = None;
        }

        assert!(!trade.is_portfolio_selected);
        assert_eq!(trade.lease_token, None);
        assert!(trade.is_active); // Still tracked independently in asset journal
    }

    #[test]
    fn test_candle_monotonicity_and_gap_detection() {
        let mut last_processed_1m_open: u64 = 60_000;

        // 1. Normal consecutive candle (+60s) -> valid, no gap
        let bar1_open = 120_000;
        assert!(bar1_open > last_processed_1m_open);
        let is_gap1 = bar1_open > last_processed_1m_open + 60_000;
        assert!(!is_gap1);
        last_processed_1m_open = bar1_open;

        // 2. Duplicate candle (same open_ms) -> dropped
        let bar2_dup = 120_000;
        assert!(bar2_dup <= last_processed_1m_open, "Duplicate bar rejected");

        // 3. Out-of-order candle (backwards in time) -> dropped
        let bar3_backwards = 60_000;
        assert!(
            bar3_backwards <= last_processed_1m_open,
            "Backwards bar rejected"
        );

        // 4. Gap candle (> 60s gap, e.g. 5 minutes missing) -> detected as gap
        let bar4_gap = 480_000;
        assert!(bar4_gap > last_processed_1m_open);
        let is_gap4 = bar4_gap > last_processed_1m_open + 60_000;
        assert!(is_gap4, "Gap identified correctly");
    }

    #[test]
    fn test_ohlcv_geometric_and_positivity_invariants() {
        let validate_candle =
            |open: f64, high: f64, low: f64, close: f64, vol: f64| -> Result<(), &'static str> {
                if !open.is_finite()
                    || open <= 0.0
                    || !high.is_finite()
                    || high <= 0.0
                    || !low.is_finite()
                    || low <= 0.0
                    || !close.is_finite()
                    || close <= 0.0
                    || !vol.is_finite()
                    || vol < 0.0
                {
                    return Err("Non-finite or non-positive value");
                }
                if high < open.max(close) || low > open.min(close) || high < low {
                    return Err("Geometric violation");
                }
                Ok(())
            };

        // 1. Valid candle
        assert!(validate_candle(100.0, 105.0, 95.0, 102.0, 50.0).is_ok());

        // 2. High lower than open/close
        assert!(validate_candle(100.0, 99.0, 95.0, 98.0, 10.0).is_err());

        // 3. Low higher than open/close
        assert!(validate_candle(100.0, 105.0, 101.0, 102.0, 10.0).is_err());

        // 4. Inverted high < low
        assert!(validate_candle(100.0, 90.0, 110.0, 95.0, 10.0).is_err());

        // 5. Zero or negative price
        assert!(validate_candle(0.0, 10.0, 0.0, 5.0, 1.0).is_err());
        assert!(validate_candle(-100.0, 105.0, 95.0, 102.0, 1.0).is_err());

        // 6. Negative volume
        assert!(validate_candle(100.0, 105.0, 95.0, 102.0, -1.0).is_err());

        // 7. NaN / Infinity
        assert!(validate_candle(f64::NAN, 105.0, 95.0, 102.0, 1.0).is_err());
        assert!(validate_candle(100.0, f64::INFINITY, 95.0, 102.0, 1.0).is_err());
    }

    #[test]
    fn test_chandelier_breakeven_never_overshoots_price_under_tight_atr() {
        // BTC at $100,000 with tight 10 bps ATR ($100.0)
        let mut chandelier = ChandelierExitState::new(Side::Long, 100_000.0, 100.0, 2.0);
        let prev_stop = chandelier.current_stop;
        // Price advances +10 bps ($100,100.0 = 1.0 * ATR).
        // Stop must NOT jump to $100,150.0 (+15 bps) above the high of $100,100.0!
        let stop =
            chandelier.update_with_ratchet_and_cap(100_100.0, 100_020.0, 100.0, 0.85, None, true);
        assert!(
            stop < 100_100.0,
            "Breakeven stop {} must never exceed extreme high 100100.0",
            stop
        );
        assert!(!chandelier.is_bar_stopped_out(prev_stop, 100_100.0, 100_020.0, 100_100.0));
    }

    #[test]
    fn test_crowding_hazard_overlay_scales_and_vetoes_squeeze() {
        let mut tracker = MarketCrowdingTracker::default();
        // Baseline normal funding (0.0000125 = 0.125 bps/hr) and steady OI (1000.0)
        for i in 0..30 {
            let jitter = if i % 2 == 0 { 1e-6 } else { -1e-6 };
            tracker.update(
                0.0000125 + jitter,
                1000.0 + (i as f64) * 0.5,
                100.01,
                100.00,
            );
        }
        assert!((tracker.crowding_hazard() - 1.0).abs() < 1e-4);
        assert_eq!(tracker.directional_trend_multiplier(&Side::Long), 1.0);

        // Now inject an extreme positive funding spike (+8 bps/hr = 0.0008), OI surge (1250.0), and wide mark-oracle basis
        tracker.update(0.0008, 1250.0, 100.80, 100.00);
        let mult_long = tracker.directional_trend_multiplier(&Side::Long);
        let mult_short = tracker.directional_trend_multiplier(&Side::Short);
        assert!(
            mult_long < 0.45,
            "Extreme crowded positive funding + OI surge must suppress Long breakout (mult={})",
            mult_long
        );
        assert_eq!(
            mult_short, 1.0,
            "Opposite Short breakout should not be penalized by positive funding crowding"
        );
    }

    #[test]
    fn test_pre_breakout_budget_approves_coiled_and_rejects_exhausted_cascade() {
        // 1. Coiled range on SOL: 20 bars oscillating tightly between 106.00 and 107.00 (width = 1.00 = 2.5x ATR of 0.40)
        let mut bars = Vec::with_capacity(20);
        for i in 0..20 {
            let base = if i % 2 == 0 { 106.35 } else { 106.65 };
            bars.push(CandleBar {
                time_open: i as u64 * 900_000,
                open: base,
                high: base + 0.30,
                low: base - 0.30,
                close: base,
                volume: 100.0,
            });
        }
        let donchian_coiled = DonchianChannel {
            upper: 106.95,
            lower: 106.05,
            middle: 106.50,
        };
        let verdict_fresh = evaluate_pre_breakout_budget(
            "SOL",
            &bars,
            &donchian_coiled,
            0.40,
            107.02, // 0.07 overshoot = 0.175x ATR
            1.15,   // calm Z60
            &Side::Long,
        );
        assert!(
            verdict_fresh.approved,
            "Coiled fresh breakout must be approved (rmb={})",
            verdict_fresh.rmb_atr
        );
        assert!(
            verdict_fresh.is_coiled,
            "Tight DCW20=2.25x and low BBW must be classified as coiled"
        );

        // 2. Exhausted cascade on SOL: channel already expanded to $3.85 (6.0x ATR of 0.64) and Z60 = -3.76
        let donchian_wide = DonchianChannel {
            upper: 107.89,
            lower: 104.04,
            middle: 105.965,
        };
        let verdict_exhausted = evaluate_pre_breakout_budget(
            "SOL",
            &bars,
            &donchian_wide,
            0.64,
            103.98,
            -3.76,
            &Side::Short,
        );
        assert!(
            !verdict_exhausted.approved,
            "Exhausted cascade with DCW20=6.0x and Z60=-3.76 must be rejected"
        );
    }

    fn make_policy_c_telemetry(
        close: f64,
        donchian_high: f64,
        donchian_low: f64,
        atr: f64,
    ) -> BreakoutTelemetry {
        BreakoutTelemetry {
            bar_open: close - 0.30 * atr,
            bar_high: close + 0.05 * atr,
            bar_low: close - 0.35 * atr,
            bar_close: close,
            donchian_high_12_15m: donchian_high,
            donchian_low_12_15m: donchian_low,
            atr_15m: atr,
            min_squeeze_ratio_90m: 0.84,
            rel_vol_15m: 1.95,
            hurst_60m: 0.54,
            l2_obi: 0.34,
            oi_delta_15m_pct: 0.15,
        }
    }

    #[test]
    fn test_policy_c_loss_parity_notional_across_all_tiers() {
        let eq = 560.44;

        let btc = TrendEngineConfig::for_symbol("BTC", 0.35, 0.28);
        assert_eq!(btc.tier, TrendAssetTier::CryptoMajor);
        let btc_ntl = btc.compute_trend_notional_usdc(eq);
        // 560.44 * 0.35 * 0.28 = $54.92312 (< $55 cap)
        assert!((btc_ntl - 54.92312).abs() < 0.01);

        let sui = TrendEngineConfig::for_symbol("SUI", 0.35, 0.28);
        assert_eq!(sui.tier, TrendAssetTier::CryptoAlt);
        let sui_ntl = sui.compute_trend_notional_usdc(eq);
        // 560.44 * 0.35 * 0.25 = $49.0385 (< $50 cap)
        assert!((sui_ntl - 49.0385).abs() < 0.01);

        let nvda = TrendEngineConfig::for_symbol("xyz:NVDA", 0.35, 0.28);
        assert_eq!(nvda.tier, TrendAssetTier::Hip3UsEquity);
        let nvda_ntl = nvda.compute_trend_notional_usdc(eq);
        // 560.44 * 0.32 * 0.30 = $53.80224 (< $55 cap)
        assert!((nvda_ntl - 53.80224).abs() < 0.01);

        let gold = TrendEngineConfig::for_symbol("xyz:GOLD", 0.35, 0.28);
        assert_eq!(gold.tier, TrendAssetTier::Hip3Commodity);
        let gold_ntl = gold.compute_trend_notional_usdc(eq);
        // 560.44 * 0.32 * 0.28 = $50.215424 (< $52 cap)
        assert!((gold_ntl - 50.215424).abs() < 0.01);

        // Hard cap holds on large equity & floor guard holds on tiny equity
        assert_eq!(btc.compute_trend_notional_usdc(5000.0), 55.0);
        assert_eq!(sui.compute_trend_notional_usdc(5000.0), 50.0);
        assert_eq!(btc.compute_trend_notional_usdc(50.0), 12.0);
    }

    #[test]
    fn test_policy_c_session_gating_dst_and_macro_override() {
        let btc_cfg = TrendEngineConfig::for_symbol("BTC", 0.35, 0.28);
        let alt_cfg = TrendEngineConfig::for_symbol("XRP", 0.35, 0.28);
        let nvda_cfg = TrendEngineConfig::for_symbol("xyz:NVDA", 0.35, 0.28);
        let gold_cfg = TrendEngineConfig::for_symbol("xyz:GOLD", 0.35, 0.28);

        // 01:15 UTC on Wednesday (Asian chop) -> Shadow for BTC & XRP
        let ts_0115_est = chrono::Utc
            .with_ymd_and_hms(2026, 11, 11, 1, 15, 0)
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &btc_cfg,
                ts_0115_est,
                3,
                false,
                0.45,
                1.2,
                0.004
            ),
            ExecutionMode::Shadow {
                reason: "OFF_PEAK_ASIAN_CHOP_SHADOW_GATE"
            }
        );

        // Macro Shock Override at 01:15 UTC (KER >= 0.62, R_V >= 2.80, |BTC 1h| >= 1.75%)
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &btc_cfg,
                ts_0115_est,
                3,
                false,
                0.65,
                3.0,
                0.020
            ),
            ExecutionMode::Live
        );
        // Alts remain strictly Shadow off-peak even on Macro Shock
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &alt_cfg,
                ts_0115_est,
                3,
                false,
                0.65,
                3.0,
                0.020
            ),
            ExecutionMode::Shadow {
                reason: "OFF_PEAK_ASIAN_CHOP_SHADOW_GATE"
            }
        );

        // 14:45 UTC on Wednesday (US institutional window) -> Live, unless MODE_MEANREVERSION
        let ts_1445_est = chrono::Utc
            .with_ymd_and_hms(2026, 11, 11, 14, 45, 0)
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &btc_cfg,
                ts_1445_est,
                3,
                false,
                0.30,
                1.7,
                0.005
            ),
            ExecutionMode::Live
        );
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &btc_cfg,
                ts_1445_est,
                3,
                true,
                0.80,
                3.5,
                0.03
            ),
            ExecutionMode::Shadow {
                reason: "BLOCKED_BY_MODE_MEANREVERSION"
            }
        );

        // US Equities DST awareness:
        // Nov 11 (EST): 13:45 UTC is pre-open -> Shadow; 14:45 UTC is ORB -> Live
        let ts_1345_est = chrono::Utc
            .with_ymd_and_hms(2026, 11, 11, 13, 45, 0)
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &nvda_cfg,
                ts_1345_est,
                3,
                false,
                0.5,
                2.0,
                0.0
            ),
            ExecutionMode::Shadow {
                reason: "OFF_PEAK_ASIAN_CHOP_SHADOW_GATE"
            }
        );
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &nvda_cfg,
                ts_1445_est,
                3,
                false,
                0.5,
                2.0,
                0.0
            ),
            ExecutionMode::Live
        );

        // July 15 (EDT): 13:45 UTC is ORB -> Live; 15:45 UTC is post-window -> Shadow
        let ts_1345_edt = chrono::Utc
            .with_ymd_and_hms(2026, 7, 15, 13, 45, 0)
            .unwrap()
            .timestamp() as u64;
        let ts_1545_edt = chrono::Utc
            .with_ymd_and_hms(2026, 7, 15, 15, 45, 0)
            .unwrap()
            .timestamp() as u64;
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &nvda_cfg,
                ts_1345_edt,
                3,
                false,
                0.5,
                2.0,
                0.0
            ),
            ExecutionMode::Live
        );
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &nvda_cfg,
                ts_1545_edt,
                3,
                false,
                0.5,
                2.0,
                0.0
            ),
            ExecutionMode::Shadow {
                reason: "OFF_PEAK_ASIAN_CHOP_SHADOW_GATE"
            }
        );

        // Weekend RWA lockout for Equities & Commodities
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &nvda_cfg,
                ts_1445_est,
                6,
                false,
                0.8,
                4.0,
                0.03
            ),
            ExecutionMode::Shadow {
                reason: "WEEKEND_RWA_LOCKOUT"
            }
        );
        assert_eq!(
            SessionTrendGate::evaluate_session_mode(
                &gold_cfg,
                ts_1445_est,
                6,
                false,
                0.8,
                4.0,
                0.03
            ),
            ExecutionMode::Shadow {
                reason: "WEEKEND_RWA_LOCKOUT"
            }
        );
    }

    #[test]
    fn test_policy_c_coiled_spring_gates_and_two_stage_alo_retest_lifecycle() {
        let cfg = TrendEngineConfig::for_symbol("BTC", 0.35, 0.28);
        let ts_us = chrono::Utc
            .with_ymd_and_hms(2026, 11, 11, 14, 30, 0)
            .unwrap()
            .timestamp() as u64;

        // 1. Reject missing squeeze
        let mut tel_no_sq = make_policy_c_telemetry(73_500.0, 73_400.0, 73_000.0, 250.0);
        tel_no_sq.min_squeeze_ratio_90m = 1.05;
        assert_eq!(
            BreakoutFilter::check_coiled_spring_gates(&cfg, &tel_no_sq, TrendDirection::Long),
            Err("REJECT_NO_PRE_BREAKOUT_SQUEEZE")
        );

        // 2. Reject low volume drift
        let mut tel_low_vol = make_policy_c_telemetry(73_500.0, 73_400.0, 73_000.0, 250.0);
        tel_low_vol.rel_vol_15m = 1.05;
        assert_eq!(
            BreakoutFilter::check_coiled_spring_gates(&cfg, &tel_low_vol, TrendDirection::Long),
            Err("REJECT_LOW_VOLUME_DRIFT_BREAKOUT")
        );

        // 3. Stage 1 -> Snapback fakeout caught
        let mut mgr = EngineBTrendManager::new();
        let tel_bar1 = make_policy_c_telemetry(73_450.0, 73_400.0, 73_000.0, 250.0);
        let a1 = mgr.on_5m_bar_close(&cfg, &tel_bar1, ts_us, 3, 560.44, false, 0.45, 0.005);
        assert_eq!(a1, TrendOrderAction::None);
        assert!(mgr.is_stage1_armed());

        let tel_snapback = make_policy_c_telemetry(73_380.0, 73_400.0, 73_000.0, 250.0);
        let a_snap = mgr.on_5m_bar_close(
            &cfg,
            &tel_snapback,
            ts_us + 300,
            3,
            560.44,
            false,
            0.45,
            0.005,
        );
        assert!(matches!(
            a_snap,
            TrendOrderAction::LogShadowCounterfactual {
                reason: "STAGE1_PERSISTENCE_SNAPBACK_AVOIDED",
                ..
            }
        ));
        assert_eq!(mgr.state, RetestState::Idle);

        // 4. Full Stage 1 -> Stage 2 -> Structure Abort & TTL Expiry
        let a1_ok = mgr.on_5m_bar_close(&cfg, &tel_bar1, ts_us, 3, 560.44, false, 0.45, 0.005);
        assert_eq!(a1_ok, TrendOrderAction::None);
        let tel_bar2 = make_policy_c_telemetry(73_480.0, 73_420.0, 73_000.0, 250.0);
        let a2 = mgr.on_5m_bar_close(&cfg, &tel_bar2, ts_us + 300, 3, 560.44, false, 0.45, 0.005);
        match a2 {
            TrendOrderAction::PostAloMakerRetestOrder {
                dir,
                limit_px,
                stop_loss_px,
                notional_usdc,
                ttl_secs,
                ..
            } => {
                assert_eq!(dir, TrendDirection::Long);
                // B = 73_400.0, ATR = 250.0 -> limit = 73_400 + 0.05*250 = 73_412.5, stop = 73_400 - 0.35*250 = 73_312.5
                assert!((limit_px - 73_412.5).abs() < 1e-6);
                assert!((stop_loss_px - 73_312.5).abs() < 1e-6);
                assert!((notional_usdc - 54.92312).abs() < 0.05);
                assert_eq!(ttl_secs, 720);
            }
            other => panic!("Expected PostAloMakerRetestOrder, got {:?}", other),
        }
        assert!(mgr.is_waiting_retest());

        // TTL expiry cancels resting order
        let a_ttl = mgr.on_5m_bar_close(
            &cfg,
            &tel_bar2,
            ts_us + 300 + 720,
            3,
            560.44,
            false,
            0.45,
            0.005,
        );
        assert_eq!(
            a_ttl,
            TrendOrderAction::CancelRestingRetestOrder {
                reason: "RETEST_TTL_EXPIRED"
            }
        );
        assert_eq!(mgr.state, RetestState::Idle);
    }

    #[test]
    fn test_policy_c_squeeze_rel_vol_and_oi_delta_helpers() {
        let mut bars_15m = Vec::with_capacity(30);
        for i in 0..29 {
            // Very tight closing range (std ~ 0.15) vs 1.0 ATR -> squeeze ratio ~ 0.20 <= 0.90
            let c = 100.0 + if i % 2 == 0 { 0.15 } else { -0.15 };
            bars_15m.push(CandleBar {
                time_open: i as u64 * 900_000,
                open: 100.0,
                high: 100.5,
                low: 99.5,
                close: c,
                volume: 1000.0,
            });
        }
        let breakout_15m = CandleBar {
            time_open: 29 * 900_000,
            open: 100.0,
            high: 102.0,
            low: 99.9,
            close: 101.8,
            volume: 2200.0,
        };
        bars_15m.push(breakout_15m.clone());

        let sq = calculate_min_squeeze_ratio_90m(&bars_15m);
        assert!(sq <= 0.90, "Expected tight squeeze <= 0.90, got {}", sq);

        let rv = calculate_rel_vol_15m(&bars_15m[..29], &[], Some(&breakout_15m));
        assert!(
            (rv - 2.20).abs() < 0.05,
            "Expected rel_vol ~ 2.20, got {}",
            rv
        );

        let mut tracker = MarketCrowdingTracker::default();
        let t0 = 1_760_000_000_000u64;
        tracker.update_with_time(0.00001, 10_000.0, 100.0, 100.0, t0);
        assert_eq!(tracker.last_mark_update_ms, t0);
        tracker.update_with_time(0.00001, 9_950.0, 100.0, 100.0, t0 + 15 * 60 * 1000);
        let oi_pct = tracker.oi_delta_15m_pct(t0 + 15 * 60 * 1000);
        assert!((oi_pct - (-0.50)).abs() < 1e-6);
    }

    #[test]
    fn test_dynamic_flag_pullback_anchor_for_extended_breakouts() {
        let cfg = TrendEngineConfig::for_symbol("BTC", 0.35, 0.28);
        let mut mgr = EngineBTrendManager::new();
        let t0 = chrono::Utc
            .with_ymd_and_hms(2026, 11, 11, 14, 30, 0)
            .unwrap()
            .timestamp() as u64; // Wednesday 14:30 UTC (primary window)

        // Stage 1: break above 100.0 with ATR = 2.0
        let bar1 = BreakoutTelemetry {
            bar_open: 99.8,
            bar_high: 100.5,
            bar_low: 99.7,
            bar_close: 100.4,
            donchian_high_12_15m: 100.0,
            donchian_low_12_15m: 96.0,
            atr_15m: 2.0,
            min_squeeze_ratio_90m: 0.65,
            rel_vol_15m: 2.2,
            hurst_60m: 0.56,
            l2_obi: 0.35,
            oi_delta_15m_pct: 0.25,
        };
        assert_eq!(
            mgr.on_5m_bar_close(&cfg, &bar1, t0, 3, 560.0, false, 0.50, 0.005),
            TrendOrderAction::None
        );

        // Stage 2: Strong continuation bar extends +1.6 ATR (103.2 vs 100.0 Donchian, extension = 3.2 > 0.70)
        let mut bar2 = bar1;
        bar2.bar_close = 103.2;
        let act = mgr.on_5m_bar_close(&cfg, &bar2, t0 + 300, 3, 560.0, false, 0.55, 0.005);
        match act {
            TrendOrderAction::PostAloMakerRetestOrder {
                limit_px,
                stop_loss_px,
                ..
            } => {
                // Anchors dynamically at 100.0 + 0.35 * 3.2 = 101.12 (vs stale 100.08)
                assert!(limit_px > 101.0 && limit_px < 103.0, "got {}", limit_px);
                let stop_width = limit_px - stop_loss_px;
                let expected_width = (cfg.retest_limit_offset_atr + cfg.retest_stop_atr) * 2.0;
                assert!((stop_width - expected_width).abs() < 1e-6);
            }
            other => panic!("Expected PostAloMakerRetestOrder, got {:?}", other),
        }
    }
}
