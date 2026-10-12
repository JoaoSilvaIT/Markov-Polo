use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveTelemetry {
    pub symbol: String,
    pub timestamp_ms: u64,
    pub price: f64,
    pub long_buy: Option<f64>,
    pub short_sell: Option<f64>,
    pub exit_long: Option<f64>,
    pub exit_short: Option<f64>,
    pub mode: String,
    pub streak_consolidation: u32,
    pub streak_trend: u32,
    pub mode_reason: String,
    pub shock_cooldown_bars: u32,
    pub p_consolidation: f64,
    pub p_trend: f64,
    pub p_shock: f64,
    pub drift_pct: f64,
    pub half_life_m: f64,
    pub is_half_life_valid: bool,
    pub is_stationary: bool,
    pub z_15: f64,
    pub z_60: f64,
    pub is_severe_shock: bool,
    pub obi_instant: f64,
    pub obi_1s: f64,
    pub obi_5s: f64,
    pub l2_fresh: bool,
    pub has_position: bool,
    pub position_side: String,
    pub position_amount: f64,
    pub position_entry_price: f64,
    pub tp_price: f64,
    pub sl_price: f64,
    pub chandelier_stop: f64,
    pub trade_proximity_long_pct: f64,
    pub trade_proximity_short_pct: f64,
    pub overall_proximity_pct: f64,
    pub proximity_summary: String,

    // Engine B (Trend Momentum) Breakout Channel & Proximity
    #[serde(default)]
    pub donchian_upper: Option<f64>,
    #[serde(default)]
    pub donchian_lower: Option<f64>,
    #[serde(default)]
    pub donchian_middle: Option<f64>,
    #[serde(default)]
    pub donchian_status: String,
    #[serde(default)]
    pub donchian_dist_upper_pct: f64,
    #[serde(default)]
    pub donchian_dist_lower_pct: f64,
    #[serde(default)]
    pub trend_alignment: String,
    #[serde(default)]
    pub active_engine: String,
}

pub fn save_telemetry(symbol: &str, telemetry: &LiveTelemetry) {
    let file_path = format!("hl_telemetry_{}.json", symbol);
    if let Ok(content) = serde_json::to_string_pretty(telemetry) {
        let temp = format!("{}.tmp", file_path);
        let _ = fs::write(&temp, content);
        let _ = fs::rename(&temp, file_path);
    }
}

/// Comprehensive funnel drop-off telemetry separating regime gating from execution opportunity gating
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct FunnelAuditCounters {
    pub symbol: String,
    pub total_1m_ticks: u64,

    // Macro & Feed Quality Gates
    pub feed_quality_degraded: u64,
    pub candle_gap_detected: u64,
    pub shock_cooldown_active: u64,
    pub short_term_shock_veto: u64,     // |Z15| >= 2.5
    pub lingering_macro_shock_veto: u64, // |Z60| >= 2.5
    pub portfolio_lock_congested: u64,
    pub circuit_breaker_paused: u64,

    // Engine A (Mean Reversion) Funnel
    pub engine_a_mode_ineligible: u64,       // Not in MeanReversion mode (Neutral or Trend)
    pub engine_a_volatility_ineligible: u64,  // |Z15| >= 1.5
    pub engine_a_half_life_ineligible: u64,   // t_half < 15m or > 360m
    pub engine_a_stationarity_ineligible: u64,
    pub engine_a_outside_band: u64,          // Price inside channel, not touching entry band
    pub engine_a_armed_waiting_bounce: u64,  // Armed, waiting for reversal bounce confirmation
    pub engine_a_reversal_hurdle_veto: u64,  // Net edge < 25 bps
    pub engine_a_orders_placed: u64,
    pub engine_a_orders_rejected: u64,

    // Engine B (Trend Momentum) Funnel
    pub engine_b_mode_ineligible: u64,       // Not in Trend mode
    pub engine_b_no_breakout: u64,           // Price inside Donchian channel
    pub engine_b_alignment_veto: u64,        // EMA alignment contradicts breakout
    pub engine_b_obi_hurdle_veto: u64,       // Continuous L2 EWMA OBI below hurdle
    pub engine_b_orders_placed: u64,
    pub engine_b_orders_rejected: u64,
}

impl FunnelAuditCounters {
    pub fn new(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            ..Default::default()
        }
    }

    pub fn format_summary(&self) -> String {
        format!(
            "📊 [FUNNEL AUDIT] {} | Total 1m Ticks: {} | Cooldown: {} | Lock Congested: {}\n   \
             Engine A: Mode Inelig: {} | Vol Inelig: {} | Outside Band: {} | Armed Waiting Bounce: {} | Hurdle Veto: {} | Orders Placed: {}\n   \
             Engine B: Mode Inelig: {} | No Breakout: {} | Align Veto: {} | OBI Veto: {} | Orders Placed: {}",
            self.symbol, self.total_1m_ticks, self.shock_cooldown_active, self.portfolio_lock_congested,
            self.engine_a_mode_ineligible, self.engine_a_volatility_ineligible, self.engine_a_outside_band,
            self.engine_a_armed_waiting_bounce, self.engine_a_reversal_hurdle_veto, self.engine_a_orders_placed,
            self.engine_b_mode_ineligible, self.engine_b_no_breakout, self.engine_b_alignment_veto,
            self.engine_b_obi_hurdle_veto, self.engine_b_orders_placed
        )
    }
}

pub fn funnel_counters_path(symbol: &str) -> String {
    format!("hl_funnel_{}.json", symbol)
}

pub fn save_funnel_counters(symbol: &str, counters: &FunnelAuditCounters) {
    let file_path = funnel_counters_path(symbol);
    if let Ok(content) = serde_json::to_string_pretty(counters) {
        let temp = format!("{}.tmp", file_path);
        let _ = fs::write(&temp, content);
        let _ = fs::rename(&temp, file_path);
    }
}

pub fn load_funnel_counters(symbol: &str) -> FunnelAuditCounters {
    let file_path = funnel_counters_path(symbol);
    if !std::path::Path::new(&file_path).exists() {
        return FunnelAuditCounters::new(symbol);
    }
    fs::read_to_string(&file_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_else(|| FunnelAuditCounters::new(symbol))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_funnel_counters_format_and_persistence() {
        let mut counters = FunnelAuditCounters::new("BTC");
        counters.total_1m_ticks = 100;
        counters.engine_a_mode_ineligible = 20;
        counters.engine_a_outside_band = 75;
        counters.engine_a_armed_waiting_bounce = 5;
        counters.engine_b_no_breakout = 90;

        let summary = counters.format_summary();
        assert!(summary.contains("Total 1m Ticks: 100"));
        assert!(summary.contains("Outside Band: 75"));
        assert!(summary.contains("Armed Waiting Bounce: 5"));
        assert!(summary.contains("No Breakout: 90"));

        let test_symbol = "TEST_BTC_FUNNEL";
        save_funnel_counters(test_symbol, &counters);
        let loaded = load_funnel_counters(test_symbol);
        assert_eq!(loaded.total_1m_ticks, 100);
        assert_eq!(loaded.engine_a_outside_band, 75);
        let _ = fs::remove_file(funnel_counters_path(test_symbol));
    }

    #[test]
    fn test_live_telemetry_backward_compatibility() {
        // Minimal JSON without the new Donchian fields
        let legacy_json = r#"{
            "symbol": "ETH",
            "timestamp_ms": 1700000000000,
            "price": 2500.0,
            "long_buy": 2480.0,
            "short_sell": 2520.0,
            "exit_long": null,
            "exit_short": null,
            "mode": "TREND",
            "streak_consolidation": 0,
            "streak_trend": 2,
            "mode_reason": "Streak",
            "shock_cooldown_bars": 0,
            "p_consolidation": 0.05,
            "p_trend": 0.90,
            "p_shock": 0.05,
            "drift_pct": 0.5,
            "half_life_m": 60.0,
            "is_half_life_valid": true,
            "is_stationary": true,
            "z_15": 0.5,
            "z_60": -0.5,
            "is_severe_shock": false,
            "obi_instant": 0.1,
            "obi_1s": 0.1,
            "obi_5s": 0.1,
            "l2_fresh": true,
            "has_position": false,
            "position_side": "",
            "position_amount": 0.0,
            "position_entry_price": 0.0,
            "tp_price": 0.0,
            "sl_price": 0.0,
            "chandelier_stop": 0.0,
            "trade_proximity_long_pct": 50.0,
            "trade_proximity_short_pct": 50.0,
            "overall_proximity_pct": 50.0,
            "proximity_summary": "Testing"
        }"#;

        let telem: LiveTelemetry = serde_json::from_str(legacy_json).expect("Deserialization must succeed with defaults");
        assert_eq!(telem.symbol, "ETH");
        assert_eq!(telem.donchian_upper, None);
        assert_eq!(telem.donchian_status, "");
        assert_eq!(telem.donchian_dist_upper_pct, 0.0);
        assert_eq!(telem.active_engine, "");
    }
}
