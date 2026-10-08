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
}

pub fn save_telemetry(symbol: &str, telemetry: &LiveTelemetry) {
    let file_path = format!("hl_telemetry_{}.json", symbol);
    if let Ok(content) = serde_json::to_string_pretty(telemetry) {
        let temp = format!("{}.tmp", file_path);
        let _ = fs::write(&temp, content);
        let _ = fs::rename(&temp, file_path);
    }
}
