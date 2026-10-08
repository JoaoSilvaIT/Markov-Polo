use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerMode {
    MeanReversion,
    Neutral,
    Trend,
}

impl std::fmt::Display for ControllerMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControllerMode::MeanReversion => write!(f, "MEAN_REVERSION"),
            ControllerMode::Neutral => write!(f, "NEUTRAL"),
            ControllerMode::Trend => write!(f, "TREND"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeController {
    pub current_mode: ControllerMode,
    pub trend_streak: u32,
    pub consolidation_streak: u32,
    pub shock_cooldown_bars: u32,
    pub last_evaluated_close_ms: u64,
    pub last_transition_time: String,
    pub last_reason: String,
    pub epoch: u64,
    #[serde(default)]
    pub breakout_seen_in_streak: bool,
}

impl Default for RegimeController {
    fn default() -> Self {
        Self {
            current_mode: ControllerMode::Neutral,
            trend_streak: 0,
            consolidation_streak: 0,
            shock_cooldown_bars: 0,
            last_evaluated_close_ms: 0,
            last_transition_time: Utc::now().to_rfc3339(),
            last_reason: "Initial system startup (Safe Cash Buffer)".to_string(),
            epoch: 1,
            breakout_seen_in_streak: false,
        }
    }
}

impl RegimeController {
    /// Forces immediate shock downgrade to Neutral with a 2-bar (30m) cooldown.
    /// If already in cooldown, restarts the clock to full 2 bars (30m).
    pub fn force_shock_cooldown(&mut self, z_15: f64, z_60: f64) {
        let was_different = self.current_mode != ControllerMode::Neutral;
        let prev_cd = self.shock_cooldown_bars;
        self.current_mode = ControllerMode::Neutral;
        self.trend_streak = 0;
        self.consolidation_streak = 0;
        self.breakout_seen_in_streak = false;
        self.shock_cooldown_bars = 2;
        self.last_reason = if prev_cd > 0 {
            format!(
                "Secondary Shock in Cooldown (|Z15|={:.2}, |Z60|={:.2}) -> Cooldown Clock Restarted to 2 bars (30m)",
                z_15, z_60
            )
        } else {
            format!(
                "Immediate Shock Downgrade (|Z15|={:.2}, |Z60|={:.2}) -> Neutral Cooldown (2 bars)",
                z_15, z_60
            )
        };
        if was_different || prev_cd > 0 {
            self.last_transition_time = Utc::now().to_rfc3339();
            self.epoch += 1;
        }
    }

    /// Evaluates 15-minute bar metrics idempotently by checking bar close timestamp.
    /// If bar_close_ms <= self.last_evaluated_close_ms, returns None without state change.
    pub fn evaluate_15m_bar_idempotent(
        &mut self,
        bar_close_ms: u64,
        p_consolidation: f64,
        p_trend: f64,
        z_15: f64,
        z_60: f64,
        has_donchian_breakout: bool,
        is_ou_stationary: bool,
        is_half_life_valid: bool,
    ) -> Option<ControllerMode> {
        if bar_close_ms > 0 && bar_close_ms <= self.last_evaluated_close_ms {
            return None;
        }
        self.last_evaluated_close_ms = bar_close_ms;
        Some(self.evaluate_15m_bar(
            p_consolidation,
            p_trend,
            z_15,
            z_60,
            has_donchian_breakout,
            is_ou_stationary,
            is_half_life_valid,
        ))
    }

    /// Evaluates 15-minute bar metrics and advances the state machine.
    ///
    /// Parameters:
    /// - `p_consolidation`: Causal HMM filtered probability of Consolidation (State 0)
    /// - `p_trend`: Causal HMM filtered probability of Trend (State 1)
    /// - `z_15`: Standardized 15-minute return shock score
    /// - `z_60`: Standardized 60-minute return shock score
    /// - `has_donchian_breakout`: True if close broke beyond 20-period Donchian channel
    /// - `is_ou_stationary`: True if O-U calibration yielded valid mean-reverting parameters
    /// - `is_half_life_valid`: True if 15m <= t_half <= 120m
    pub fn evaluate_15m_bar(
        &mut self,
        p_consolidation: f64,
        p_trend: f64,
        z_15: f64,
        z_60: f64,
        has_donchian_breakout: bool,
        is_ou_stationary: bool,
        is_half_life_valid: bool,
    ) -> ControllerMode {
        let is_severe_shock = z_15.abs() >= 3.0 || z_60.abs() >= 3.0;

        // 1. SEVERE SHOCK OVERRIDE (100% Cash Protection):
        // Severe volatility spikes (|Z| >= 3.0) and liquidation cascades must NEVER trigger
        // an immediate Trend entry (which risks buying the top of a squeeze).
        // They exclusively route to Neutral (safe cash buffer), reset streaks, and trigger cooldown.
        if is_severe_shock {
            let was_different = self.current_mode != ControllerMode::Neutral;
            let prev_cd = self.shock_cooldown_bars;
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.breakout_seen_in_streak = false;
            self.shock_cooldown_bars = 2; // 2 bars = 30-minute re-entry freeze
            self.last_reason = if prev_cd > 0 {
                format!(
                    "Secondary Severe Shock in Cooldown (|Z15|={:.2}, |Z60|={:.2}) -> Cooldown Clock Restarted to 2 bars (30m)",
                    z_15, z_60
                )
            } else {
                format!(
                    "Severe Volatility Shock Veto (|Z15|={:.2}, |Z60|={:.2}) -> Safe Neutral Cash Buffer (Cooldown 2 bars)",
                    z_15, z_60
                )
            };
            if was_different || prev_cd > 0 {
                self.last_transition_time = Utc::now().to_rfc3339();
                self.epoch += 1;
            }
            return self.current_mode;
        }

        // 2. SHOCK COOLDOWN PACING (Hybrid Policy: 30-min minimum floor + volatility normalization gate)
        if self.shock_cooldown_bars > 0 {
            self.shock_cooldown_bars -= 1;
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.breakout_seen_in_streak = false;
            if self.shock_cooldown_bars > 0 {
                self.last_reason = format!(
                    "Shock Re-Entry Cooldown Active ({} bars remaining) -> Forced Neutral",
                    self.shock_cooldown_bars
                );
            } else {
                self.last_reason = "Shock Re-Entry Cooldown Completed -> Holding Neutral Awaiting Valid Regime Setup".to_string();
            }
            return self.current_mode;
        }

        // 2b. LINGERING SHOCK GATES (Engine-Separated Recovery)
        // Extreme short-term shock (|Z15| >= 2.5) blocks all regime accumulation.
        let extreme_short_term_shock = z_15.abs() >= 2.5;
        let extreme_macro_shock = z_60.abs() >= 2.5;

        if extreme_short_term_shock {
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = self.trend_streak.saturating_sub(1);
            self.consolidation_streak = self.consolidation_streak.saturating_sub(1);
            if self.trend_streak == 0 {
                self.breakout_seen_in_streak = false;
            }
            self.last_reason = format!(
                "Extreme Short-Term Volatility Shock (|Z15|={:.2}>=2.5) -> Hold Neutral",
                z_15
            );
            return self.current_mode;
        }

        // 3. UPDATE STREAK COUNTERS (Asymmetric Dwell + Leaky Accumulator)
        let has_breakout = has_donchian_breakout || self.breakout_seen_in_streak;

        if p_trend >= 0.82 && !extreme_short_term_shock {
            self.trend_streak += 1;
            self.consolidation_streak = 0; // Opposite regime actively contradicts
            if has_donchian_breakout {
                self.breakout_seen_in_streak = true;
            }
        } else if p_consolidation >= 0.72 && z_15.abs() < 1.5 && !extreme_macro_shock {
            // Engine A requires normalized, low volatility (|Z15| < 1.5 and |Z60| < 2.5)
            self.consolidation_streak += 1;
            self.trend_streak = 0; // Opposite regime actively contradicts
            self.breakout_seen_in_streak = false;
        } else {
            // Deadband / hesitation region: Leaky Integrator decay (perdoar quedas marginais)
            self.trend_streak = self.trend_streak.saturating_sub(1);
            self.consolidation_streak = self.consolidation_streak.saturating_sub(1);
            if self.trend_streak == 0 {
                self.breakout_seen_in_streak = false;
            }
        }

        // 4. STATE MACHINE TRANSITIONS (Asymmetric Multi-Tier Dwell)
        match self.current_mode {
            ControllerMode::Neutral => {
                // Asymmetric Trend Trigger (Researcher 1 & 2):
                // - Strong trend (P_T >= 0.90) + Donchian breakout -> 1 bar dwell!
                // - Moderate trend (P_T >= 0.82) + Donchian breakout -> 2 bars dwell!
                // - Macro sustained trend dwell -> 3 bars!
                let valid_trend_trigger = (p_trend >= 0.90 && self.trend_streak >= 1 && has_breakout)
                    || (self.trend_streak >= 2 && has_breakout)
                    || (self.trend_streak >= 3);

                if valid_trend_trigger {
                    self.current_mode = ControllerMode::Trend;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Trend confirmed: P_T={:.1}% (streak {} bars) + Donchian breakout",
                        p_trend * 100.0,
                        self.trend_streak
                    );
                    self.epoch += 1;
                }
                // Asymmetric Consolidation Trigger (Researcher 1 & 2):
                // - Strong consolidation (P_C >= 0.80) -> 2 bars dwell!
                // - Moderate consolidation (P_C >= 0.72) -> 3 bars dwell!
                else if ((p_consolidation >= 0.80 && self.consolidation_streak >= 2) || self.consolidation_streak >= 3)
                    && is_ou_stationary
                    && is_half_life_valid
                    && !is_severe_shock
                {
                    self.current_mode = ControllerMode::MeanReversion;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Consolidation confirmed: P_C={:.1}% (streak {} bars) + O-U valid",
                        p_consolidation * 100.0,
                        self.consolidation_streak
                    );
                    self.epoch += 1;
                } else {
                    let required_t_dwell = if p_trend >= 0.90 && has_breakout { 1 } else { 2 };
                    let required_c_dwell = if p_consolidation >= 0.80 { 2 } else { 3 };
                    self.last_reason = format!(
                        "Awaiting regime confirmation (Trend streak: {}/{}, Consolidation streak: {}/{})",
                        self.trend_streak, required_t_dwell, self.consolidation_streak, required_c_dwell
                    );
                }
            }

            ControllerMode::Trend => {
                // Trend -> Neutral if trend probability decays below 0.55
                if p_trend < 0.55 {
                    self.current_mode = ControllerMode::Neutral;
                    self.trend_streak = 0;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Trend momentum decayed (P_T={:.2} < 0.55) -> Neutral buffer",
                        p_trend
                    );
                    self.epoch += 1;
                }
            }

            ControllerMode::MeanReversion => {
                // MeanReversion -> Neutral if trend probability rises, consolidation drops, or O-U breaks
                if p_trend >= 0.65
                    || p_consolidation < 0.60
                    || !is_ou_stationary
                    || !is_half_life_valid
                {
                    self.current_mode = ControllerMode::Neutral;
                    self.consolidation_streak = 0;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Consolidation invalidated (P_T={:.2}, P_C={:.2}, stationary={}, hl_valid={}) -> Neutral",
                        p_trend, p_consolidation, is_ou_stationary, is_half_life_valid
                    );
                    self.epoch += 1;
                }
            }
        }

        self.current_mode
    }

    /// Called when an active trend trade is closed / stopped out by Chandelier exit
    pub fn notify_trend_stopped_out(&mut self) {
        if self.current_mode == ControllerMode::Trend {
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.last_transition_time = Utc::now().to_rfc3339();
            self.last_reason = "Trend position closed via Chandelier Stop -> Returning to Neutral buffer".to_string();
            self.epoch += 1;
        }
    }

    /// Returns gate funnel inspection status for diagnostic telemetry
    pub fn inspect_gates(
        &self,
        p_consolidation: f64,
        p_trend: f64,
        z_15: f64,
        z_60: f64,
        has_donchian_breakout: bool,
        is_ou_stationary: bool,
        is_half_life_valid: bool,
    ) -> (String, String) {
        let has_breakout = has_donchian_breakout || self.breakout_seen_in_streak;
        let required_t_dwell = if p_trend >= 0.90 && has_breakout { 1 } else { 2 };
        let required_c_dwell = if p_consolidation >= 0.80 { 2 } else { 3 };

        let engine_a_status = if self.shock_cooldown_bars > 0 {
            format!("BLOCKED(Cooldown {}b)", self.shock_cooldown_bars)
        } else if z_15.abs() >= 1.5 || z_60.abs() >= 2.5 {
            format!("BLOCKED(Vol Z15={:.2}, Z60={:.2})", z_15, z_60)
        } else if p_consolidation < 0.72 {
            format!("BLOCKED(P_C={:.1}% < 72%)", p_consolidation * 100.0)
        } else if !is_ou_stationary || !is_half_life_valid {
            "BLOCKED(O-U non-stationary)".to_string()
        } else if self.consolidation_streak < required_c_dwell {
            format!("DWELL({}/{})", self.consolidation_streak, required_c_dwell)
        } else {
            "ELIGIBLE".to_string()
        };

        let engine_b_status = if self.shock_cooldown_bars > 0 {
            format!("BLOCKED(Cooldown {}b)", self.shock_cooldown_bars)
        } else if z_15.abs() >= 2.5 {
            format!("BLOCKED(Z15={:.2}>=2.5)", z_15)
        } else if p_trend < 0.82 {
            format!("BLOCKED(P_T={:.1}% < 82%)", p_trend * 100.0)
        } else if !has_breakout {
            "BLOCKED(NoBreakout)".to_string()
        } else if self.trend_streak < required_t_dwell {
            format!("DWELL({}/{})", self.trend_streak, required_t_dwell)
        } else {
            "ELIGIBLE".to_string()
        };

        (engine_a_status, engine_b_status)
    }
}

pub fn controller_state_path(symbol: &str) -> String {
    format!("hl_controller_state_{}.json", symbol)
}

pub fn save_controller_state(symbol: &str, controller: &RegimeController) {
    let path = controller_state_path(symbol);
    if let Ok(c) = serde_json::to_string_pretty(controller) {
        let temp = format!("{}.tmp", path);
        let _ = fs::write(&temp, c);
        let _ = fs::rename(&temp, &path);
    }
}

pub fn load_controller_state(symbol: &str) -> RegimeController {
    let path = controller_state_path(symbol);
    if !std::path::Path::new(&path).exists() {
        return RegimeController::default();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state_is_neutral() {
        let controller = RegimeController::default();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 0);
        assert_eq!(controller.consolidation_streak, 0);
    }

    #[test]
    fn test_asymmetric_dwell_strong_trend_triggers_on_bar_1() {
        let mut controller = RegimeController::default();

        // Strong trend (P_T >= 0.90) with Donchian breakout triggers in 1 bar
        let mode = controller.evaluate_15m_bar(0.05, 0.92, 1.0, 0.5, true, false, false);
        assert_eq!(mode, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 1);
        assert!(controller.last_reason.contains("Trend confirmed"));
    }

    #[test]
    fn test_asymmetric_dwell_moderate_trend_requires_2_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Moderate trend (P_T = 0.85) with breakout -> 1/2 dwell required
        let mode1 = controller.evaluate_15m_bar(0.10, 0.85, 1.0, 0.5, true, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);

        // Bar 2: Moderate trend continuation (P_T = 0.86) -> Confirmed on bar 2!
        let mode2 = controller.evaluate_15m_bar(0.10, 0.86, 1.2, 0.6, true, false, false);
        assert_eq!(mode2, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 2);
    }

    #[test]
    fn test_breakout_seen_in_streak_triggers_trend_on_bar_2() {
        let mut controller = RegimeController::default();

        // Bar 1: P_T = 0.85 WITH initial breakout
        let mode1 = controller.evaluate_15m_bar(0.10, 0.85, 1.0, 0.5, true, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);
        assert!(controller.breakout_seen_in_streak);

        // Bar 2: P_T = 0.85 WITHOUT new breakout (continuation) -> Triggers on bar 2!
        let mode2 = controller.evaluate_15m_bar(0.10, 0.85, 1.2, 0.6, false, false, false);
        assert_eq!(mode2, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 2);
    }

    #[test]
    fn test_hysteresis_deadband_leaky_decay() {
        let mut controller = RegimeController::default();

        // 2 bars of moderate trend (without breakout, so staying in Neutral)
        controller.evaluate_15m_bar(0.10, 0.85, 1.0, 0.5, false, false, false);
        controller.evaluate_15m_bar(0.10, 0.85, 1.0, 0.5, false, false, false);
        assert_eq!(controller.trend_streak, 2);

        // Bar 3 enters deadband (P_T = 0.70, P_C = 0.20): leaky decay decrements to 1 (NOT zero!)
        let mode = controller.evaluate_15m_bar(0.20, 0.70, 0.5, 0.2, false, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1); // Leaky decay saved 1 streak!

        // Bar 4 enters deadband again: decays to 0
        controller.evaluate_15m_bar(0.20, 0.70, 0.5, 0.2, false, false, false);
        assert_eq!(controller.trend_streak, 0);
    }

    #[test]
    fn test_asymmetric_dwell_strong_consolidation_requires_2_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Strong consolidation P_C = 0.82
        let mode1 = controller.evaluate_15m_bar(0.82, 0.12, 0.2, 0.1, false, true, true);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 1);

        // Bar 2: Strong consolidation P_C = 0.84 -> Confirms on bar 2!
        let mode2 = controller.evaluate_15m_bar(0.84, 0.10, 0.1, 0.0, false, true, true);
        assert_eq!(mode2, ControllerMode::MeanReversion);
        assert_eq!(controller.consolidation_streak, 2);
    }

    #[test]
    fn test_asymmetric_dwell_moderate_consolidation_requires_3_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Moderate consolidation P_C = 0.75
        controller.evaluate_15m_bar(0.75, 0.15, 0.2, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 1);

        // Bar 2: Moderate consolidation P_C = 0.76
        controller.evaluate_15m_bar(0.76, 0.15, 0.1, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 2);

        // Bar 3: Moderate consolidation P_C = 0.75 -> Confirms on bar 3!
        controller.evaluate_15m_bar(0.75, 0.15, 0.0, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);
        assert_eq!(controller.consolidation_streak, 3);
    }

    #[test]
    fn test_severe_shock_drops_to_neutral() {
        let mut controller = RegimeController::default();
        // Transition into MeanReversion
        controller.evaluate_15m_bar(0.80, 0.15, 0.2, 0.1, false, true, true);
        controller.evaluate_15m_bar(0.80, 0.15, 0.2, 0.1, false, true, true);
        controller.evaluate_15m_bar(0.80, 0.15, 0.2, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);

        // Flash crash shock occurs (|Z_15| = -3.5)
        let mode = controller.evaluate_15m_bar(0.70, 0.20, -3.5, -2.0, false, true, true);
        assert_eq!(mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Severe Volatility Shock"));
    }

    #[test]
    fn test_severe_shock_blocks_trend_override() {
        let mut controller = RegimeController::default();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);

        // Single bar with P_T = 0.95, Z_15 = +3.4 (severe shock), and breakout
        // Severe shock MUST route to Neutral (100% Cash Buffer), NOT Trend!
        let mode = controller.evaluate_15m_bar(0.05, 0.95, 3.4, 1.5, true, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Severe Volatility Shock"));
    }

    #[test]
    fn test_notify_trend_stopped_out() {
        let mut controller = RegimeController::default();
        controller.current_mode = ControllerMode::Trend;

        controller.notify_trend_stopped_out();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Chandelier Stop"));
    }

    #[test]
    fn test_severe_shock_cooldown_bars() {
        let mut controller = RegimeController::default();
        // Severe shock hits
        let mode = controller.evaluate_15m_bar(0.05, 0.95, 3.4, 1.5, true, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);

        // Next bar: strong trend signal arrives, but cooldown is active (bar 1 of cooldown)
        let mode1 = controller.evaluate_15m_bar(0.05, 0.95, 1.0, 0.5, true, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 1);
        assert!(controller.last_reason.contains("Shock Re-Entry Cooldown"));

        // Bar 2 of cooldown: still forced neutral
        let mode2 = controller.evaluate_15m_bar(0.05, 0.95, 1.0, 0.5, true, false, false);
        assert_eq!(mode2, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 0);

        // Now cooldown is finished. Strong trend (P_T = 0.95) with breakout confirms in 1 bar!
        let mode3 = controller.evaluate_15m_bar(0.05, 0.95, 1.0, 0.5, true, false, false);
        assert_eq!(mode3, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 1);
    }

    #[test]
    fn test_idempotent_bar_evaluation() {
        let mut controller = RegimeController::default();
        let bar_ts = 1712000000_000u64;

        // First evaluation succeeds
        let res1 = controller.evaluate_15m_bar_idempotent(bar_ts, 0.80, 0.15, 0.2, 0.1, false, true, true);
        assert!(res1.is_some());
        assert_eq!(controller.last_evaluated_close_ms, bar_ts);

        // Duplicate evaluation with same timestamp is dropped
        let res2 = controller.evaluate_15m_bar_idempotent(bar_ts, 0.80, 0.15, 0.2, 0.1, false, true, true);
        assert!(res2.is_none());

        // Stale evaluation with earlier timestamp is dropped
        let res3 = controller.evaluate_15m_bar_idempotent(bar_ts - 900_000, 0.80, 0.15, 0.2, 0.1, false, true, true);
        assert!(res3.is_none());

        // Next bar (later timestamp) succeeds
        let res4 = controller.evaluate_15m_bar_idempotent(bar_ts + 900_000, 0.80, 0.15, 0.2, 0.1, false, true, true);
        assert!(res4.is_some());
        assert_eq!(controller.last_evaluated_close_ms, bar_ts + 900_000);
    }

    #[test]
    fn test_secondary_shock_restarts_cooldown_clock() {
        let mut controller = RegimeController::default();
        // 1. Initial shock hits
        let mode1 = controller.evaluate_15m_bar(0.05, 0.95, 3.4, 1.5, true, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);

        // 2. Bar 1 of cooldown: 1 bar remaining
        let mode2 = controller.evaluate_15m_bar(0.05, 0.95, 1.0, 0.5, true, false, false);
        assert_eq!(mode2, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 1);

        // 3. Secondary shock hits during cooldown! Clock must restart to full 2 bars (30m)
        let mode3 = controller.evaluate_15m_bar(0.05, 0.95, -3.8, -2.0, true, false, false);
        assert_eq!(mode3, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);
        assert!(controller.last_reason.contains("Secondary Severe Shock in Cooldown"));
    }
}
