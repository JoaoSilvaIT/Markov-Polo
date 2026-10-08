use crate::state::Side;
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

/// Real-time L2 continuous order book tracker with basis-point distance weighting and multiscale physical-time EWMA
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L2ObiTracker {
    pub instant_obi: f64,
    pub ewma_250ms_obi: f64,
    pub ewma_1s_obi: f64,
    pub ewma_5s_obi: f64,
    pub last_update_ms: u64,
    pub best_bid: f64,
    pub best_ask: f64,
    #[serde(default)]
    pub updates_since_reconnect: u32,
}

impl Default for L2ObiTracker {
    fn default() -> Self {
        Self {
            instant_obi: 0.0,
            ewma_250ms_obi: 0.0,
            ewma_1s_obi: 0.0,
            ewma_5s_obi: 0.0,
            last_update_ms: 0,
            best_bid: 0.0,
            best_ask: 0.0,
            updates_since_reconnect: 0,
        }
    }
}

impl L2ObiTracker {
    pub fn update(&mut self, bids: &[(f64, f64)], asks: &[(f64, f64)], now_ms: u64) {
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

        let dt_ms = if self.last_update_ms > 0 && now_ms > self.last_update_ms {
            (now_ms - self.last_update_ms) as f64
        } else {
            0.0
        };

        if dt_ms > 0.0 && dt_ms <= 3000.0 {
            let ln2 = std::f64::consts::LN_2;

            // Multiscale physical-time EWMA decay: alpha = 1 - exp(-ln2 * dt / h)
            // 250ms: immediate queue pressure
            let alpha_250ms = 1.0 - (-ln2 * dt_ms / 250.0).exp();
            self.ewma_250ms_obi = (1.0 - alpha_250ms) * self.ewma_250ms_obi + alpha_250ms * obi_inst;

            // 1000ms (1s): primary execution feature
            let alpha_1s = 1.0 - (-ln2 * dt_ms / 1000.0).exp();
            self.ewma_1s_obi = (1.0 - alpha_1s) * self.ewma_1s_obi + alpha_1s * obi_inst;

            // 5000ms (5s): institutional persistence filter
            let alpha_5s = 1.0 - (-ln2 * dt_ms / 5000.0).exp();
            self.ewma_5s_obi = (1.0 - alpha_5s) * self.ewma_5s_obi + alpha_5s * obi_inst;

            self.updates_since_reconnect = self.updates_since_reconnect.saturating_add(1);
        } else {
            // Disconnect or stale interval (>3s): invalidate stale memory and reinitialize
            self.ewma_250ms_obi = obi_inst;
            self.ewma_1s_obi = obi_inst;
            self.ewma_5s_obi = obi_inst;
            self.updates_since_reconnect = 1;
        }

        self.last_update_ms = now_ms;
    }

    pub fn is_fresh_and_uncrossed(&self, now_ms: u64, max_latency_ms: u64) -> bool {
        if self.last_update_ms == 0 || now_ms < self.last_update_ms {
            return false;
        }
        let latency = now_ms - self.last_update_ms;
        latency <= max_latency_ms && self.best_ask > self.best_bid && self.best_bid > 0.0
    }

    pub fn satisfies_entry_hurdle(&self, side: &Side, now_ms: u64) -> (bool, &'static str) {
        if !self.is_fresh_and_uncrossed(now_ms, 2500) {
            return (false, "L2 Book Stale or Crossed");
        }
        if self.updates_since_reconnect < 3 {
            return (false, "L2 Book Warming Up After Reconnect");
        }

        match side {
            Side::Long => {
                // Multiscale hurdle: instant >= +0.15, 1s >= +0.10, and persistent 5s > 0.0
                if self.instant_obi >= 0.15 && self.ewma_1s_obi >= 0.10 && self.ewma_5s_obi > 0.0 {
                    (true, "Bullish Multiscale Depth Alignment")
                } else {
                    (false, "Insufficient Bid Depth / Fleeting Ask Asymmetry")
                }
            }
            Side::Short => {
                // Multiscale hurdle: instant <= -0.15, 1s <= -0.10, and persistent 5s < 0.0
                if self.instant_obi <= -0.15 && self.ewma_1s_obi <= -0.10 && self.ewma_5s_obi < 0.0 {
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
        // Guard against non-finite or invalid inputs
        if current_atr <= 0.0 || current_atr.is_nan() || current_atr.is_infinite()
            || p_trend.is_nan() || p_trend.is_infinite()
            || current_high.is_nan() || current_low.is_nan()
        {
            return self.current_stop;
        }

        let decay_factor = ((p_trend - 0.55) / (0.85 - 0.55)).clamp(0.0, 1.0);
        let target_multiplier = 1.5 + (self.base_multiplier - 1.5) * decay_factor;

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
                let candidate_stop = self.extreme_favorable_price - self.active_multiplier * current_atr;
                // Monotone Ratchet: stop strictly moves UP
                self.current_stop = f64::max(self.current_stop, candidate_stop);
            }
            Side::Short => {
                if current_low < self.extreme_favorable_price {
                    self.extreme_favorable_price = current_low;
                }
                let candidate_stop = self.extreme_favorable_price + self.active_multiplier * current_atr;
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
    3.0
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
    fn test_chandelier_probability_compression() {
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
        };

        // Exit at 105.0
        let pnl = record_shadow_exit(&mut trade, 105.0, "Chandelier Stop");
        // Gross = $5.0, friction = $0.11226 (maker entry + taker exit + 5 bps slippage) -> net ~ $4.8877
        assert!(pnl > 4.80 && pnl < 4.95);
        assert_eq!(trade.is_active, false);
    }

    #[test]
    fn test_l2_obi_tracker_physical_time_decay() {
        let mut tracker = L2ObiTracker::default();
        let bids = vec![(100.0, 10.0), (99.0, 10.0)];
        let asks = vec![(101.0, 5.0), (102.0, 5.0)];

        let t0 = 1_000_000;
        tracker.update(&bids, &asks, t0);
        assert!(tracker.instant_obi > 0.0, "Bids outweigh asks");
        assert_eq!(tracker.ewma_1s_obi, tracker.instant_obi);

        // 1 second (1000ms) later, asks heavily surge
        let t1 = t0 + 1000;
        let surge_asks = vec![(101.0, 50.0), (102.0, 50.0)];
        tracker.update(&bids, &surge_asks, t1);
        assert!(tracker.instant_obi < -0.50, "Instantaneous flipped heavily bearish");
        // 1s EWMA should smoothly decay towards negative, but not instantly equal instant_obi
        assert!(tracker.ewma_1s_obi > tracker.instant_obi);

        // Check freshness
        assert!(tracker.is_fresh_and_uncrossed(t1 + 500, 2500));
        assert!(!tracker.is_fresh_and_uncrossed(t1 + 3000, 2500), "Should expire stale books > 2500ms");
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
}
