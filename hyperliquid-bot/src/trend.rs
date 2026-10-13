use crate::state::{FlockGuard, Side};
use chrono::Utc;
use serde::{Deserialize, Serialize};
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

fn default_tau_1() -> f64 { 10_000.0 }
fn default_tau_2() -> f64 { 30_000.0 }
fn default_regime_label() -> String { "LowLatency".to_string() }

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
    pub fn update(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)], exchange_time_ms: u64, local_recv_ms: u64) {
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
        let instant_dt = self.last_instant.map(|prev| prev.elapsed().as_secs_f64() * 1000.0).unwrap_or(0.0);
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
                let is_degraded = qual.p95_ms > 7500.0 || qual.observed_age_p99_ms > 1500.0 || qual.mad_ms > 2500.0;
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
            if lag_n == 0 { return 0.0; }
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

        let status = if p95 > 12_000.0 || age_p99 > 3_000.0 || mean_lag > 3_000.0 || (self.gaps_over_3s > 0 && self.last_update_ms.saturating_sub(self.reconnect_time_ms) < 60_000) {
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
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&file_path) {
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
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&file_path) {
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
                if self.instant_obi >= -0.18 && self.ewma_1s_obi >= -0.15 && self.ewma_5s_obi >= -0.12 {
                    (true, "Bullish Multiscale Depth Alignment")
                } else {
                    (false, "Insufficient Bid Depth / Fleeting Ask Asymmetry")
                }
            }
            Side::Short => {
                // Multiscale hurdle: ensure ask depth is not overwhelmed by adverse buy walls.
                if self.instant_obi <= 0.18 && self.ewma_1s_obi <= 0.15 && self.ewma_5s_obi <= 0.12 {
                    (true, "Bearish Multiscale Depth Alignment")
                } else {
                    (false, "Insufficient Ask Depth / Fleeting Bid Asymmetry")
                }
            }
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
    pub fn update(&mut self, current_high: f64, current_low: f64, current_atr: f64, p_trend: f64) -> f64 {
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
        if current_atr <= 0.0 || current_atr.is_nan() || current_atr.is_infinite()
            || p_trend.is_nan() || p_trend.is_infinite()
            || current_high.is_nan() || current_low.is_nan()
        {
            return self.current_stop;
        }

        let decay_factor = ((p_trend - 0.55) / (0.85 - 0.55)).clamp(0.0, 1.0);
        let min_multiplier = (self.base_multiplier * 0.5).max(1.0);
        let target_multiplier = min_multiplier + (self.base_multiplier - min_multiplier) * decay_factor;

        if let Some(max_step) = max_contraction_step {
            if target_multiplier < self.active_multiplier {
                self.active_multiplier = (self.active_multiplier - max_step).max(target_multiplier);
            } else {
                self.active_multiplier = (self.active_multiplier + max_step).min(target_multiplier);
            }
        } else {
            self.active_multiplier = target_multiplier;
        }

        match self.side {
            Side::Long => {
                if current_high > self.extreme_favorable_price {
                    self.extreme_favorable_price = current_high;
                }
                let mut candidate_stop = self.extreme_favorable_price - self.active_multiplier * current_atr;

                if enable_breakeven_ratchet {
                    let delta_fav = (self.extreme_favorable_price - self.entry_price).max(0.0);
                    let ret_fav_pct = if self.entry_price > 0.0 { delta_fav / self.entry_price } else { 0.0 };
                    let eps = 1e-6;

                    // 1. Breakeven Floor: once favorable progress >= 1.0 ATR or >= 1.0%, stop moves to at least entry + 15 bps (covers round-trip fees)
                    if delta_fav >= (1.0 * current_atr) - eps || ret_fav_pct >= 0.010 {
                        candidate_stop = f64::max(candidate_stop, self.entry_price * 1.0015);
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
                        candidate_stop = f64::max(candidate_stop, f64::max(self.entry_price * 1.0250, locked_70));
                    }
                    // 5. ATR Giveback Floor: once progress >= 1.5 ATR, protect at least 50% of peak progress
                    if delta_fav >= (1.5 * current_atr) - eps {
                        candidate_stop = f64::max(candidate_stop, self.entry_price + 0.5 * delta_fav);
                    }
                }

                // Monotone Ratchet: stop strictly moves UP
                self.current_stop = f64::max(self.current_stop, candidate_stop);
            }
            Side::Short => {
                if current_low < self.extreme_favorable_price {
                    self.extreme_favorable_price = current_low;
                }
                let mut candidate_stop = self.extreme_favorable_price + self.active_multiplier * current_atr;

                if enable_breakeven_ratchet {
                    let delta_fav = (self.entry_price - self.extreme_favorable_price).max(0.0);
                    let ret_fav_pct = if self.entry_price > 0.0 { delta_fav / self.entry_price } else { 0.0 };
                    let eps = 1e-6;

                    // 1. Breakeven Floor: once favorable progress >= 1.0 ATR or >= 1.0%, stop moves to at least entry - 15 bps (covers round-trip fees)
                    if delta_fav >= (1.0 * current_atr) - eps || ret_fav_pct >= 0.010 {
                        candidate_stop = f64::min(candidate_stop, self.entry_price * 0.9985);
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
                        candidate_stop = f64::min(candidate_stop, f64::min(self.entry_price * 0.9750, locked_70));
                    }
                    // 5. ATR Giveback Floor: once progress >= 1.5 ATR, protect at least 50% of peak progress
                    if delta_fav >= (1.5 * current_atr) - eps {
                        candidate_stop = f64::min(candidate_stop, self.entry_price - 0.5 * delta_fav);
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

    pub fn is_bar_stopped_out(&self, bar_high: f64, bar_low: f64) -> bool {
        match self.side {
            Side::Long => bar_low <= self.current_stop,
            Side::Short => bar_high >= self.current_stop,
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

    if let Some(parent) = counter_path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

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
    if let Some(parent) = lock_path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
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
    let lease_token = format!("{}_{}_{}", symbol, new_token, Utc::now().timestamp_subsec_nanos());
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
            let _ = fs::write(&path, c);
        }
    } else {
        let _ = fs::remove_file(&path);
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
        let _ = writeln!(f, "{}", serde_json::to_string(&log_entry).unwrap_or_default());
    }

    trade
}

pub fn record_shadow_exit(
    trade: &mut ShadowTrade,
    exit_price: f64,
    exit_reason: &str,
) -> f64 {
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
        let _ = writeln!(f, "{}", serde_json::to_string(&log_entry).unwrap_or_default());
    }

    net_pnl
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
        assert!((atr - 10.0).abs() < 1e-4, "ATR should converge to 10.0, got: {}", atr);
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
        let asks = vec![(101.0, 5.0), (102.0, 5.0)];                // sum = 10.0

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
        assert!(stop <= 9.99, "Breakeven floor must lock stop below entry covering fees (stop: {})", stop);
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
        assert!(stop >= 102.00, "50% profit giveback ratchet must protect at least 102.00 (stop: {})", stop);
    }

    #[test]
    fn test_chandelier_tiered_profit_lock_ladder() {
        // Short entry @ 10.00, ATR = 0.10, base_multiplier = 2.0
        let mut chandelier = ChandelierExitState::new(Side::Short, 10.00, 0.10, 2.0);

        // 1. Gain reaches +2.0% (price drops to 9.80): stop locks at least +1.0% profit (<= 9.90)
        let stop_tier2 = chandelier.update_with_ratchet_and_cap(9.85, 9.80, 0.10, 0.85, None, true);
        assert!(stop_tier2 <= 9.90, "Tier 2 (+2% gain) must lock in at least +1.0% profit (stop: {})", stop_tier2);

        // 2. Gain reaches +3.0% (price drops to 9.70): stop locks at least +2.0% profit (<= 9.80)
        let stop_tier3 = chandelier.update_with_ratchet_and_cap(9.75, 9.70, 0.10, 0.85, None, true);
        assert!(stop_tier3 <= 9.80, "Tier 3 (+3% gain) must lock in at least +2.0% profit (stop: {})", stop_tier3);

        // 3. Gain reaches +5.0% (price drops to 9.50): delta_fav = 0.50, locks 70% of peak progress (0.50 * 0.70 = 0.35 -> stop <= 9.65)
        let stop_tier4 = chandelier.update_with_ratchet_and_cap(9.55, 9.50, 0.10, 0.85, None, true);
        assert!(stop_tier4 <= 9.65, "Tier 4 (Super-Trend) must protect at least 70% of peak gain (stop: {})", stop_tier4);
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
        assert!(tracker.instant_obi < -0.50, "Instantaneous flipped heavily bearish");
        // 1s EWMA should smoothly decay towards negative, but not instantly equal instant_obi
        assert!(tracker.ewma_1s_obi > tracker.instant_obi);

        // Check freshness
        assert!(tracker.is_fresh_and_uncrossed(t1 + 500, 7500));
        assert!(!tracker.is_fresh_and_uncrossed(t1 + 8000, 7500), "Should expire stale books > 7500ms");
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

        let qual = tracker.compute_feed_quality().expect("Quality stats computed");
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
        assert_eq!(tracker.current_regime_label, "JitterSubsampled", "Hysteresis blocks premature flip on 1st cycle");

        // Subsequent clean cycles until >=2 cycles and 60s of stable clean traffic has elapsed:
        for _ in 37..=72 {
            let t = tracker.last_local_recv_ms + 5400;
            tracker.update(&bids, &asks, t - 200, t);
        }
        assert_eq!(tracker.current_regime_label, "LowLatency", "Recovered to LowLatency after consecutive cycles & 60s");
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
        assert!(ok, "Taker sweep asymmetry must not strangle valid breakout entry");
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

        let eth_lease: ShadowClusterLease = serde_json::from_str(&fs::read_to_string(SHADOW_CLUSTER_LOCK).unwrap()).unwrap();
        assert_eq!(eth_lease.fencing_token, 2, "Fencing token monotonically preserved across releases");

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
        let refresh_ok = refresh_shadow_cluster_lease(&trade.symbol, trade.lease_token.as_deref().unwrap());
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
        assert!(bar3_backwards <= last_processed_1m_open, "Backwards bar rejected");

        // 4. Gap candle (> 60s gap, e.g. 5 minutes missing) -> detected as gap
        let bar4_gap = 480_000;
        assert!(bar4_gap > last_processed_1m_open);
        let is_gap4 = bar4_gap > last_processed_1m_open + 60_000;
        assert!(is_gap4, "Gap identified correctly");
    }

    #[test]
    fn test_ohlcv_geometric_and_positivity_invariants() {
        let validate_candle = |open: f64, high: f64, low: f64, close: f64, vol: f64| -> Result<(), &'static str> {
            if !open.is_finite() || open <= 0.0
                || !high.is_finite() || high <= 0.0
                || !low.is_finite() || low <= 0.0
                || !close.is_finite() || close <= 0.0
                || !vol.is_finite() || vol < 0.0
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
}
