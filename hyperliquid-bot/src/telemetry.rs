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
    #[serde(default)]
    pub account_equity: f64,
    #[serde(default)]
    pub high_water_mark: f64,
}

pub fn save_telemetry(symbol: &str, telemetry: &LiveTelemetry) {
    let file_path = format!("hl_telemetry_{}.json", symbol);
    if let Ok(content) = serde_json::to_string_pretty(telemetry) {
        let temp = format!("{}.tmp.{}", file_path, std::process::id());
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
        {
            use std::io::Write;
            if file.write_all(content.as_bytes()).is_ok() && file.sync_all().is_ok() {
                let _ = fs::rename(&temp, &file_path);
                crate::state::sync_parent_dir(&file_path);
            }
        }
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
        let temp = format!("{}.tmp.{}", file_path, std::process::id());
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
        {
            use std::io::Write;
            if file.write_all(content.as_bytes()).is_ok() && file.sync_all().is_ok() {
                let _ = fs::rename(&temp, &file_path);
                crate::state::sync_parent_dir(&file_path);
            }
        }
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

/// Returns the empirical Hyperliquid BTC-to-altcoin beta seed for causal impulse scaling.
pub fn empirical_btc_beta(symbol: &str, is_stress: bool) -> f64 {
    match (symbol.to_uppercase().as_str(), is_stress) {
        ("ETH", false) => 1.20,
        ("ETH", true) => 1.45,
        ("SOL", false) => 1.60,
        ("SOL", true) => 1.80,
        ("AVAX", false) => 1.60,
        ("AVAX", true) => 1.80,
        _ => 1.20,
    }
}

/// Evaluates cross-asset BTC lead-lag impulse spillover using the Beta-Scaled Multiscale Impulse Score I_{BTC->k}(t).
/// Fails open (returns None) if current symbol is BTC or if BTC telemetry is missing/stale (>90s).
pub fn check_btc_lead_lag_veto(
    current_symbol: &str,
    proposed_is_long: bool,
    is_trend_engine: bool,
    now_ms: u64,
) -> Option<String> {
    if current_symbol.eq_ignore_ascii_case("BTC") {
        return None;
    }
    let btc_telem: LiveTelemetry = fs::read_to_string("hl_telemetry_BTC.json")
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())?;

    // Only enforce lead-lag filter if BTC telemetry snapshot is fresh within 90 seconds
    if now_ms.saturating_sub(btc_telem.timestamp_ms) > 90_000 {
        return None;
    }

    let is_stress = btc_telem.is_severe_shock || btc_telem.z_15.abs() >= 2.5;
    let beta = empirical_btc_beta(current_symbol, is_stress);
    // Multiscale BTC OBI composite: 0.50 * inst + 0.30 * 1s + 0.20 * 5s
    let obi_composite = 0.50 * btc_telem.obi_instant + 0.30 * btc_telem.obi_1s + 0.20 * btc_telem.obi_5s;
    // Causal Cross-Asset Impulse Score I_{BTC->k}(t) = beta_k * (0.65 * Z_BTC + 0.35 * Z_OBI)
    let impulse_score = beta * (0.65 * btc_telem.z_15 + 0.35 * (2.5 * obi_composite));

    if is_trend_engine {
        // Engine B (Trend Breakout): veto altcoin breakout if BTC is actively crashing/surging in opposite direction
        if proposed_is_long && (btc_telem.z_15 <= -2.0 || impulse_score <= -2.2) {
            return Some(format!(
                "BTC Lead-Lag Downside Impulse (I={:.2}, BTC Z15={:.2})",
                impulse_score, btc_telem.z_15
            ));
        } else if !proposed_is_long && (btc_telem.z_15 >= 2.0 || impulse_score >= 2.2) {
            return Some(format!(
                "BTC Lead-Lag Upside Impulse (I={:.2}, BTC Z15={:.2})",
                impulse_score, btc_telem.z_15
            ));
        }
    } else {
        // Engine A (Mean Reversion): veto catching an altcoin falling knife while BTC impulse is adverse
        if proposed_is_long {
            if impulse_score <= -1.35
                || btc_telem.z_15 <= -1.8
                || (btc_telem.is_severe_shock && btc_telem.z_15 < -1.0)
            {
                return Some(format!(
                    "BTC Lead-Lag Cascade Veto (I={:.2}, beta={:.2}, BTC Z15={:.2}, OBI_1s={:.2})",
                    impulse_score, beta, btc_telem.z_15, btc_telem.obi_1s
                ));
            }
        } else if impulse_score >= 1.35
            || btc_telem.z_15 >= 1.8
            || (btc_telem.is_severe_shock && btc_telem.z_15 > 1.0)
        {
            return Some(format!(
                "BTC Lead-Lag Squeeze Veto (I={:.2}, beta={:.2}, BTC Z15={:.2}, OBI_1s={:.2})",
                impulse_score, beta, btc_telem.z_15, btc_telem.obi_1s
            ));
        }
    }
    None
}

/// Persistent ledger record for a completed real exchange trade on Hyperliquid L1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RealTradeRecord {
    pub symbol: String,
    pub side: String,
    pub engine: String,
    pub amount: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub opened_at: String,
    pub closed_at: String,
    pub pnl_usd: f64,
    pub pnl_bps: f64,
    pub exit_reason: String,
    pub is_win: bool,
}

/// Atomically appends a completed real trade to `hl_real_trades.jsonl` with cross-process flock + fsync.
pub fn append_real_trade_record(record: &RealTradeRecord) {
    let lock_path = "hl_real_trades.jsonl.lock";
    let _guard = crate::state::FlockGuard::acquire(lock_path);
    if let Ok(line) = serde_json::to_string(record) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("hl_real_trades.jsonl")
        {
            use std::io::Write;
            let _ = writeln!(file, "{}", line);
            let _ = file.sync_all();
            crate::state::sync_parent_dir("hl_real_trades.jsonl");
        }
    }
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
        assert_eq!(telem.account_equity, 0.0);
    }

    #[test]
    fn test_real_trade_record_roundtrip() {
        let rec = RealTradeRecord {
            symbol: "SOL".to_string(),
            side: "SHORT".to_string(),
            engine: "EngineA".to_string(),
            amount: 0.67,
            entry_price: 107.12,
            exit_price: 106.53,
            opened_at: "2026-10-16T08:10:00Z".to_string(),
            closed_at: "2026-10-16T08:42:03Z".to_string(),
            pnl_usd: 0.3953,
            pnl_bps: 55.08,
            exit_reason: "PROFIT_LOCK_EXIT".to_string(),
            is_win: true,
        };
        let s = serde_json::to_string(&rec).unwrap();
        let parsed: RealTradeRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed.symbol, "SOL");
        assert!(parsed.is_win);
        assert!((parsed.pnl_usd - 0.3953).abs() < 1e-6);
    }
}
