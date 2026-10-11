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
    #[serde(default)]
    pub cooldown_until_ms: u64,
    #[serde(default)]
    pub model_generation: u64,
    #[serde(default)]
    pub peak_shock_z: f64,
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
            cooldown_until_ms: 0,
            model_generation: 1,
            peak_shock_z: 0.0,
        }
    }
}

impl RegimeController {
    /// Forces immediate shock downgrade to Neutral with a 2-bar and 30-minute wall-clock cooldown.
    /// Preserves active cooldown countdown on 1m bars unless severe secondary escalation occurs.
    pub fn force_shock_cooldown(&mut self, z_15: f64, z_60: f64, now_ms: u64) {
        let current_shock_z = z_15.abs().max(z_60.abs());
        let was_different = self.current_mode != ControllerMode::Neutral;
        let _prev_cd = self.shock_cooldown_bars;
        let prev_time_active = self.cooldown_until_ms > now_ms;

        self.current_mode = ControllerMode::Neutral;
        self.trend_streak = 0;
        self.consolidation_streak = 0;
        self.breakout_seen_in_streak = false;
        self.shock_cooldown_bars = 2;

        if !prev_time_active {
            // Fresh shock initiation: start 30m countdown clock
            self.cooldown_until_ms = now_ms + 30 * 60 * 1000;
            self.peak_shock_z = current_shock_z;
            self.last_reason = format!(
                "Immediate Shock Downgrade (|Z15|={:.2}, |Z60|={:.2}) -> Neutral Cooldown (2 bars & 30m)",
                z_15, z_60
            );
            self.last_transition_time = Utc::now().to_rfc3339();
            self.epoch += 1;
        } else if current_shock_z >= self.peak_shock_z + 1.5 {
            // Severe secondary shock escalation: restart clock
            self.cooldown_until_ms = now_ms + 30 * 60 * 1000;
            self.peak_shock_z = current_shock_z;
            self.last_reason = format!(
                "Secondary Shock Escalation (|Z15|={:.2}, |Z60|={:.2} vs peak {:.2}) -> Cooldown Clock Restarted (2 bars & 30m)",
                z_15, z_60, self.peak_shock_z
            );
            self.last_transition_time = Utc::now().to_rfc3339();
            self.epoch += 1;
        } else {
            // Already in active cooldown from ongoing shock - preserve wall-clock countdown!
            self.peak_shock_z = self.peak_shock_z.max(current_shock_z);
            let rem_mins = ((self.cooldown_until_ms.saturating_sub(now_ms)) as f64 / 60_000.0).ceil() as u64;
            self.last_reason = format!(
                "Ongoing Shock Cooldown Active (|Z15|={:.2}, |Z60|={:.2}) -> Remaining: ~{}m (Clock preserved)",
                z_15, z_60, rem_mins
            );
            if was_different {
                self.last_transition_time = Utc::now().to_rfc3339();
                self.epoch += 1;
            }
        }
    }

    /// Resets evidence streaks upon cold-path HMM model recalibration hot-swap
    pub fn notify_model_hotswap(&mut self) {
        self.model_generation += 1;
        self.trend_streak = 0;
        self.consolidation_streak = 0;
        self.breakout_seen_in_streak = false;
        self.last_reason = format!(
            "HMM Model Recalibrated (Gen {}) -> Streaks Reset",
            self.model_generation
        );
        self.epoch += 1;
    }

    /// Forces immediate fallback to Neutral due to feed gaps or data quality anomalies
    pub fn force_neutral_data_quality(&mut self) {
        let was_different = self.current_mode != ControllerMode::Neutral;
        self.current_mode = ControllerMode::Neutral;
        self.trend_streak = 0;
        self.consolidation_streak = 0;
        self.breakout_seen_in_streak = false;
        self.last_reason = "Feed Gap or Data Quality Degradation -> Forced Neutral".to_string();
        if was_different {
            self.last_transition_time = Utc::now().to_rfc3339();
            self.epoch += 1;
        }
    }

    /// Evaluates 15-minute bar metrics idempotently by checking bar close timestamp.
    /// If bar_close_ms <= self.last_evaluated_close_ms, returns None without state change.
    pub fn evaluate_15m_bar_idempotent(
        &mut self,
        bar_close_ms: u64,
        now_ms: u64,
        p_consolidation: f64,
        p_trend: f64,
        z_15: f64,
        z_60: f64,
        has_donchian_breakout: bool,
        is_ou_stationary: bool,
        is_half_life_valid: bool,
        is_severe_shock: bool,
    ) -> Option<ControllerMode> {
        if bar_close_ms > 0 && bar_close_ms <= self.last_evaluated_close_ms {
            return None;
        }
        self.last_evaluated_close_ms = bar_close_ms;
        Some(self.evaluate_15m_bar(
            now_ms,
            p_consolidation,
            p_trend,
            z_15,
            z_60,
            has_donchian_breakout,
            is_ou_stationary,
            is_half_life_valid,
            is_severe_shock,
        ))
    }

    /// Evaluates 15-minute bar metrics and advances the state machine.
    pub fn evaluate_15m_bar(
        &mut self,
        now_ms: u64,
        p_consolidation: f64,
        p_trend: f64,
        z_15: f64,
        z_60: f64,
        has_donchian_breakout: bool,
        is_ou_stationary: bool,
        is_half_life_valid: bool,
        is_severe_shock: bool,
    ) -> ControllerMode {
        // 1. SEVERE SHOCK OVERRIDE (100% Cash Protection):
        if is_severe_shock {
            let was_different = self.current_mode != ControllerMode::Neutral;
            let prev_cd = self.shock_cooldown_bars;
            let prev_time_active = self.cooldown_until_ms > now_ms;
            let current_shock_z = z_15.abs().max(z_60.abs());
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.breakout_seen_in_streak = false;
            self.shock_cooldown_bars = 2; // 2 bars = minimum
            self.cooldown_until_ms = now_ms + 30 * 60 * 1000; // 30-min real wall-clock minimum
            self.peak_shock_z = current_shock_z;
            self.last_reason = if prev_cd > 0 || prev_time_active {
                format!(
                    "Secondary Severe Shock in Cooldown (|Z15|={:.2}, |Z60|={:.2}) -> Cooldown Clock Restarted (2 bars & 30m)",
                    z_15, z_60
                )
            } else {
                format!(
                    "Severe Volatility Shock Veto (|Z15|={:.2}, |Z60|={:.2}) -> Safe Neutral Cash Buffer (Cooldown 2 bars & 30m)",
                    z_15, z_60
                )
            };
            if was_different || prev_cd > 0 || prev_time_active {
                self.last_transition_time = Utc::now().to_rfc3339();
                self.epoch += 1;
            }
            return self.current_mode;
        }

        // 2. SHOCK COOLDOWN PACING (Hybrid: 2 bars AND 30 minutes real wall-clock minimum)
        let wall_clock_cd_active = self.cooldown_until_ms > 0 && now_ms < self.cooldown_until_ms;
        if self.shock_cooldown_bars > 0 || wall_clock_cd_active {
            if self.shock_cooldown_bars > 0 {
                self.shock_cooldown_bars -= 1;
            }
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.breakout_seen_in_streak = false;
            let rem_mins = if wall_clock_cd_active {
                ((self.cooldown_until_ms - now_ms) as f64 / 60_000.0).ceil() as u64
            } else {
                0
            };
            self.last_reason = format!(
                "Shock Re-Entry Cooldown Active ({} bars, ~{}m remaining) -> Forced Neutral",
                self.shock_cooldown_bars, rem_mins
            );
            return self.current_mode;
        }

        // 2b. LINGERING MACRO SHOCK GATES (Multi-hour macro liquidation dislocation)
        let extreme_macro_shock = z_60.abs() >= 2.5;

        if extreme_macro_shock {
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = self.trend_streak.saturating_sub(1);
            self.consolidation_streak = self.consolidation_streak.saturating_sub(1);
            if self.trend_streak == 0 {
                self.breakout_seen_in_streak = false;
            }
            self.last_reason = format!(
                "Extreme Macro Volatility Shock (|Z60|={:.2}>=2.5) -> Hold Neutral",
                z_60
            );
            return self.current_mode;
        }

        // 3. UPDATE STREAK COUNTERS (Asymmetric Dwell + Leaky Accumulator)
        let has_breakout = has_donchian_breakout || self.breakout_seen_in_streak;
        let p_shock = (1.0 - p_consolidation - p_trend).max(0.0);

        if p_shock >= 0.50 {
            // HMM Shock Regime: immediate hard reset of all regime streaks (no leaky decay during shock)
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.breakout_seen_in_streak = false;
        } else if p_trend >= 0.82 && !extreme_macro_shock {
            // Engine B accumulates momentum breakouts
            self.trend_streak += 1;
            self.consolidation_streak = 0; // Opposite regime actively contradicts
            if has_donchian_breakout {
                self.breakout_seen_in_streak = true;
            }
        } else if p_consolidation >= 0.72 && z_15.abs() < 2.2 && !extreme_macro_shock {
            // Engine A requires normalized, low volatility (|Z15| < 2.2 and |Z60| < 2.5)
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
                // Asymmetric Multi-Tier Trend Trigger:
                // - With 15m Donchian breakout (fast impulse breakout):
                //     * Strong trend (P_T >= 0.90) -> 1 bar dwell!
                //     * Moderate trend (P_T >= 0.82) -> 2 bars dwell!
                // - Without 15m Donchian breakout (sustained macro momentum expansion):
                //     * Strong trend (P_T >= 0.90) -> 2 bars dwell!
                //     * Moderate trend (P_T >= 0.82) -> 3 bars dwell!
                let valid_trend_trigger = if has_breakout {
                    (p_trend >= 0.90 && self.trend_streak >= 1) || (p_trend >= 0.82 && self.trend_streak >= 2)
                } else {
                    (p_trend >= 0.90 && self.trend_streak >= 2) || (p_trend >= 0.82 && self.trend_streak >= 3)
                };

                if valid_trend_trigger {
                    self.current_mode = ControllerMode::Trend;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = if has_breakout {
                        format!(
                            "Trend confirmed: P_T={:.1}% (streak {} bars) + Donchian breakout",
                            p_trend * 100.0,
                            self.trend_streak
                        )
                    } else {
                        format!(
                            "Trend confirmed: P_T={:.1}% (streak {} bars) momentum expansion",
                            p_trend * 100.0,
                            self.trend_streak
                        )
                    };
                    self.epoch += 1;
                }
                // Asymmetric Consolidation Trigger (Researcher 1 & 2):
                // - Strong consolidation (P_C >= 0.80) -> 2 bars dwell!
                // - Moderate consolidation (P_C >= 0.72) -> 3 bars dwell!
                else if ((p_consolidation >= 0.80 && self.consolidation_streak >= 2)
                    || (p_consolidation >= 0.72 && self.consolidation_streak >= 3))
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
                    let required_t_dwell = if has_breakout {
                        if p_trend >= 0.90 { 1 } else { 2 }
                    } else {
                        if p_trend >= 0.90 { 2 } else { 3 }
                    };
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
                // MeanReversion -> Neutral if volatility exceeds gate, trend rises, consolidation drops, or O-U breaks
                if z_15.abs() >= 2.2
                    || z_60.abs() >= 2.5
                    || p_trend >= 0.65
                    || p_consolidation < 0.60
                    || !is_ou_stationary
                    || !is_half_life_valid
                {
                    self.current_mode = ControllerMode::Neutral;
                    self.consolidation_streak = 0;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Consolidation invalidated (Z15={:.2}, Z60={:.2}, P_T={:.2}, P_C={:.2}, stationary={}, hl_valid={}) -> Neutral",
                        z_15, z_60, p_trend, p_consolidation, is_ou_stationary, is_half_life_valid
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
        now_ms: u64,
    ) -> (String, String) {
        let has_breakout = has_donchian_breakout || self.breakout_seen_in_streak;
        let required_t_dwell = if p_trend >= 0.90 && has_breakout { 1 } else { 2 };
        let required_c_dwell = if p_consolidation >= 0.80 { 2 } else { 3 };
        let wall_clock_cd_active = self.cooldown_until_ms > 0 && now_ms < self.cooldown_until_ms;

        let engine_a_status = if self.shock_cooldown_bars > 0 || wall_clock_cd_active {
            let rem_mins = if wall_clock_cd_active {
                ((self.cooldown_until_ms - now_ms) as f64 / 60_000.0).ceil() as u64
            } else {
                0
            };
            format!("BLOCKED(Cooldown {}b, ~{}m)", self.shock_cooldown_bars, rem_mins)
        } else if z_15.abs() >= 2.2 || z_60.abs() >= 2.5 {
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

        let engine_b_status = if self.shock_cooldown_bars > 0 || wall_clock_cd_active {
            let rem_mins = if wall_clock_cd_active {
                ((self.cooldown_until_ms - now_ms) as f64 / 60_000.0).ceil() as u64
            } else {
                0
            };
            format!("BLOCKED(Cooldown {}b, ~{}m)", self.shock_cooldown_bars, rem_mins)
        } else if z_60.abs() >= 2.5 {
            format!("BLOCKED(Z60={:.2}>=2.5)", z_60)
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

    const BASE_TIME_MS: u64 = 1_700_000_000_000;

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
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS, 0.05, 0.92, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 1);
        assert!(controller.last_reason.contains("Trend confirmed"));
    }

    #[test]
    fn test_asymmetric_dwell_moderate_trend_requires_2_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Moderate trend (P_T = 0.85) with breakout -> 1/2 dwell required
        let mode1 = controller.evaluate_15m_bar(BASE_TIME_MS, 0.10, 0.85, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);

        // Bar 2: Moderate trend continuation (P_T = 0.86) -> Confirmed on bar 2!
        let mode2 = controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.10, 0.86, 1.2, 0.6, true, false, false, false);
        assert_eq!(mode2, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 2);
    }

    #[test]
    fn test_breakout_seen_in_streak_triggers_trend_on_bar_2() {
        let mut controller = RegimeController::default();

        // Bar 1: P_T = 0.85 WITH initial breakout
        let mode1 = controller.evaluate_15m_bar(BASE_TIME_MS, 0.10, 0.85, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);
        assert!(controller.breakout_seen_in_streak);

        // Bar 2: P_T = 0.85 WITHOUT new breakout (continuation) -> Triggers on bar 2!
        let mode2 = controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.10, 0.85, 1.2, 0.6, false, false, false, false);
        assert_eq!(mode2, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 2);
    }

    #[test]
    fn test_strong_trend_without_15m_breakout_triggers_on_bar_2() {
        let mut controller = RegimeController::default();

        // Bar 1: P_T = 0.95 without 15m breakout (streak 1 -> awaits 2)
        let mode1 = controller.evaluate_15m_bar(BASE_TIME_MS, 0.05, 0.95, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);
        assert!(!controller.breakout_seen_in_streak);

        // Bar 2: P_T = 0.95 without 15m breakout -> triggers on bar 2 via momentum expansion!
        let mode2 = controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.05, 0.95, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode2, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 2);
        assert!(controller.last_reason.contains("momentum expansion"));
    }

    #[test]
    fn test_moderate_trend_without_15m_breakout_triggers_on_bar_3() {
        let mut controller = RegimeController::default();

        // Bar 1: P_T = 0.85 without 15m breakout -> Neutral (1/3)
        let mode1 = controller.evaluate_15m_bar(BASE_TIME_MS, 0.10, 0.85, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1);

        // Bar 2: P_T = 0.85 without 15m breakout -> Neutral (2/3)
        let mode2 = controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.10, 0.85, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode2, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 2);

        // Bar 3: P_T = 0.85 without 15m breakout -> Confirmed on bar 3 (3/3)!
        let mode3 = controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.10, 0.85, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode3, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 3);
        assert!(controller.last_reason.contains("momentum expansion"));
    }

    #[test]
    fn test_hmm_shock_regime_resets_streaks() {
        let mut controller = RegimeController::default();

        // 2 bars of strong trend
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(controller.trend_streak, 2);

        // Bar 3: HMM detects severe regime shock (P_Shock = 1.0, P_T = 0, P_C = 0)
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.0, 0.0, 1.0, 0.5, false, false, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 0); // Must be zero, NOT decremented!
    }

    #[test]
    fn test_hysteresis_deadband_leaky_decay() {
        let mut controller = RegimeController::default();

        // 2 bars of moderate trend (without breakout, so staying in Neutral)
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.10, 0.85, 1.0, 0.5, false, false, false, false);
        controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.10, 0.85, 1.0, 0.5, false, false, false, false);
        assert_eq!(controller.trend_streak, 2);

        // Bar 3 enters deadband (P_T = 0.70, P_C = 0.20): leaky decay decrements to 1 (NOT zero!)
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.20, 0.70, 0.5, 0.2, false, false, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 1); // Leaky decay saved 1 streak!

        // Bar 4 enters deadband again: decays to 0
        controller.evaluate_15m_bar(BASE_TIME_MS + 2_700_000, 0.20, 0.70, 0.5, 0.2, false, false, false, false);
        assert_eq!(controller.trend_streak, 0);
    }

    #[test]
    fn test_asymmetric_dwell_strong_consolidation_requires_2_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Strong consolidation P_C = 0.82
        let mode1 = controller.evaluate_15m_bar(BASE_TIME_MS, 0.82, 0.12, 0.2, 0.1, false, true, true, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 1);

        // Bar 2: Strong consolidation P_C = 0.84 -> Confirms on bar 2!
        let mode2 = controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.84, 0.10, 0.1, 0.0, false, true, true, false);
        assert_eq!(mode2, ControllerMode::MeanReversion);
        assert_eq!(controller.consolidation_streak, 2);
    }

    #[test]
    fn test_asymmetric_dwell_moderate_consolidation_requires_3_bars() {
        let mut controller = RegimeController::default();

        // Bar 1: Moderate consolidation P_C = 0.75
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.75, 0.15, 0.2, 0.1, false, true, true, false);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 1);

        // Bar 2: Moderate consolidation P_C = 0.76
        controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.76, 0.15, 0.1, 0.1, false, true, true, false);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 2);

        // Bar 3: Moderate consolidation P_C = 0.75 -> Confirms on bar 3!
        controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.75, 0.15, 0.0, 0.1, false, true, true, false);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);
        assert_eq!(controller.consolidation_streak, 3);
    }

    #[test]
    fn test_severe_shock_drops_to_neutral() {
        let mut controller = RegimeController::default();
        // Transition into MeanReversion
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);

        // Flash crash shock occurs (|Z_15| = -3.5, is_severe_shock = true)
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS + 2_700_000, 0.70, 0.20, -3.5, -2.0, false, true, true, true);
        assert_eq!(mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Severe Volatility Shock"));
    }

    #[test]
    fn test_severe_shock_blocks_trend_override() {
        let mut controller = RegimeController::default();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);

        // Single bar with P_T = 0.95, Z_15 = +3.4, breakout, and is_severe_shock = true
        // Severe shock MUST route to Neutral (100% Cash Buffer), NOT Trend!
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS, 0.05, 0.95, 3.4, 1.5, true, false, false, true);
        assert_eq!(mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Severe Volatility Shock"));
    }

    #[test]
    fn test_notify_trend_stopped_out() {
        let mut controller = RegimeController {
            current_mode: ControllerMode::Trend,
            ..Default::default()
        };

        controller.notify_trend_stopped_out();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Chandelier Stop"));
    }

    #[test]
    fn test_severe_shock_cooldown_bars_and_wall_clock() {
        let mut controller = RegimeController::default();
        let shock_time = BASE_TIME_MS;

        // Severe shock hits
        let mode = controller.evaluate_15m_bar(shock_time, 0.05, 0.95, 3.4, 1.5, true, false, false, true);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);
        assert_eq!(controller.cooldown_until_ms, shock_time + 30 * 60 * 1000);

        // Bar 1 of cooldown (15 minutes later): cooldown is active (1 bar remaining, 15m remaining)
        let mode1 = controller.evaluate_15m_bar(shock_time + 15 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 1);
        assert!(controller.last_reason.contains("Shock Re-Entry Cooldown"));

        // Bar 2 of cooldown (30 minutes later): bars decrement to 0 and wall-clock reached
        let mode2 = controller.evaluate_15m_bar(shock_time + 30 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode2, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 0);

        // Bar 3 (45 minutes later): Cooldown is completely finished. Strong trend confirms on bar 1!
        let mode3 = controller.evaluate_15m_bar(shock_time + 45 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode3, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 1);
    }

    #[test]
    fn test_wall_clock_cooldown_blocks_even_if_bars_tick_early() {
        let mut controller = RegimeController::default();
        let shock_time = BASE_TIME_MS;

        // Shock hits
        controller.evaluate_15m_bar(shock_time, 0.05, 0.95, 3.5, 1.0, true, false, false, true);
        assert_eq!(controller.shock_cooldown_bars, 2);

        // Suppose two bars evaluate rapidly (e.g. within 10 minutes)
        controller.evaluate_15m_bar(shock_time + 5 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        let mode_early = controller.evaluate_15m_bar(shock_time + 10 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        // Bars have reached 0, BUT wall clock cooldown still has 20m left!
        assert_eq!(mode_early, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 0);
        assert!(controller.last_reason.contains("remaining) -> Forced Neutral"));
    }

    #[test]
    fn test_fast_path_force_shock_cooldown_in_neutral() {
        let mut controller = RegimeController::default();
        assert_eq!(controller.current_mode, ControllerMode::Neutral);

        // Fast-path shock detected while already in Neutral
        controller.force_shock_cooldown(3.2, 1.5, BASE_TIME_MS);
        assert_eq!(controller.shock_cooldown_bars, 2);
        assert_eq!(controller.cooldown_until_ms, BASE_TIME_MS + 30 * 60 * 1000);
        assert!(controller.last_reason.contains("Immediate Shock Downgrade"));

        // Next evaluation within cooldown must be forced Neutral
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS + 10 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
    }

    #[test]
    fn test_fast_path_shock_cooldown_preserves_countdown() {
        let mut controller = RegimeController::default();
        let shock_start = BASE_TIME_MS;

        // 1m bar 1: Initial shock hits
        controller.force_shock_cooldown(3.2, 1.5, shock_start);
        let initial_expiry = controller.cooldown_until_ms;
        assert_eq!(initial_expiry, shock_start + 30 * 60 * 1000);

        // 1m bar 2 (60s later): Z15 is still 3.2 because rolling window hasn't cleared.
        // Wall-clock timer must NOT be extended!
        controller.force_shock_cooldown(3.2, 1.5, shock_start + 60_000);
        assert_eq!(controller.cooldown_until_ms, initial_expiry);
        assert!(controller.last_reason.contains("Clock preserved"));

        // 1m bar 10 (10m later): Z15 is still 3.1.
        // Wall-clock timer must STILL be preserved!
        controller.force_shock_cooldown(3.1, 1.4, shock_start + 600_000);
        assert_eq!(controller.cooldown_until_ms, initial_expiry);
        assert!(controller.last_reason.contains("Clock preserved"));

        // 1m bar 12: A catastrophic escalation hits (Z = 5.0 >= peak 3.2 + 1.5)
        // Clock restarts!
        controller.force_shock_cooldown(5.0, 1.5, shock_start + 720_000);
        assert_eq!(controller.cooldown_until_ms, shock_start + 720_000 + 30 * 60 * 1000);
        assert!(controller.last_reason.contains("Secondary Shock Escalation"));
    }

    #[test]
    fn test_consolidation_invalidated_by_continuous_volatility() {
        let mut controller = RegimeController::default();

        // Establish MeanReversion mode
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.82, 0.10, 0.2, 0.1, false, true, true, false);
        controller.evaluate_15m_bar(BASE_TIME_MS + 900_000, 0.84, 0.10, 0.1, 0.0, false, true, true, false);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);

        // Next bar has |Z_15| = 2.3 (>= 2.2 threshold) -> Immediately drops to Neutral
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS + 1_800_000, 0.82, 0.10, 2.3, 0.5, false, true, true, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert!(controller.last_reason.contains("Consolidation invalidated (Z15="));
    }

    #[test]
    fn test_notify_model_hotswap_resets_streaks() {
        let mut controller = RegimeController::default();
        controller.evaluate_15m_bar(BASE_TIME_MS, 0.85, 0.10, 0.2, 0.1, false, true, true, false);
        assert_eq!(controller.consolidation_streak, 1);

        controller.notify_model_hotswap();
        assert_eq!(controller.consolidation_streak, 0);
        assert_eq!(controller.trend_streak, 0);
        assert_eq!(controller.model_generation, 2);
    }

    #[test]
    fn test_idempotent_bar_evaluation() {
        let mut controller = RegimeController::default();
        let bar_ts = 1712000000_000u64;

        // First evaluation succeeds
        let res1 = controller.evaluate_15m_bar_idempotent(bar_ts, bar_ts, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        assert!(res1.is_some());
        assert_eq!(controller.last_evaluated_close_ms, bar_ts);

        // Duplicate evaluation with same timestamp is dropped
        let res2 = controller.evaluate_15m_bar_idempotent(bar_ts, bar_ts, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        assert!(res2.is_none());

        // Stale evaluation with earlier timestamp is dropped
        let res3 = controller.evaluate_15m_bar_idempotent(bar_ts - 900_000, bar_ts, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        assert!(res3.is_none());

        // Next bar (later timestamp) succeeds
        let res4 = controller.evaluate_15m_bar_idempotent(bar_ts + 900_000, bar_ts + 900_000, 0.80, 0.15, 0.2, 0.1, false, true, true, false);
        assert!(res4.is_some());
        assert_eq!(controller.last_evaluated_close_ms, bar_ts + 900_000);
    }

    #[test]
    fn test_secondary_shock_restarts_cooldown_clock() {
        let mut controller = RegimeController::default();
        let shock_time = BASE_TIME_MS;

        // 1. Initial shock hits
        let mode1 = controller.evaluate_15m_bar(shock_time, 0.05, 0.95, 3.4, 1.5, true, false, false, true);
        assert_eq!(mode1, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);

        // 2. Bar 1 of cooldown: 1 bar remaining
        let mode2 = controller.evaluate_15m_bar(shock_time + 15 * 60 * 1000, 0.05, 0.95, 1.0, 0.5, true, false, false, false);
        assert_eq!(mode2, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 1);

        // 3. Secondary shock hits during cooldown! Clock must restart to full 2 bars and 30m
        let mode3 = controller.evaluate_15m_bar(shock_time + 20 * 60 * 1000, 0.05, 0.95, -3.8, -2.0, true, false, false, true);
        assert_eq!(mode3, ControllerMode::Neutral);
        assert_eq!(controller.shock_cooldown_bars, 2);
        assert_eq!(controller.cooldown_until_ms, shock_time + 20 * 60 * 1000 + 30 * 60 * 1000);
        assert!(controller.last_reason.contains("Secondary Severe Shock in Cooldown"));
    }

    #[test]
    fn test_inspect_gates_telemetry() {
        let mut controller = RegimeController::default();
        let (gate_a, gate_b) = controller.inspect_gates(0.85, 0.10, 0.5, 1.0, false, true, true, BASE_TIME_MS);
        assert_eq!(gate_a, "DWELL(0/2)");
        assert_eq!(gate_b, "BLOCKED(P_T=10.0% < 82%)");

        controller.force_shock_cooldown(3.5, 1.0, BASE_TIME_MS);
        let (gate_a_shock, gate_b_shock) = controller.inspect_gates(0.85, 0.10, 0.5, 1.0, false, true, true, BASE_TIME_MS);
        assert!(gate_a_shock.contains("BLOCKED(Cooldown 2b"));
        assert!(gate_b_shock.contains("BLOCKED(Cooldown 2b"));
    }

    #[test]
    fn test_force_neutral_data_quality() {
        let mut controller = RegimeController::default();
        controller.current_mode = ControllerMode::MeanReversion;
        controller.consolidation_streak = 3;
        controller.breakout_seen_in_streak = true;

        controller.force_neutral_data_quality();

        assert_eq!(controller.current_mode, ControllerMode::Neutral);
        assert_eq!(controller.consolidation_streak, 0);
        assert_eq!(controller.trend_streak, 0);
        assert!(!controller.breakout_seen_in_streak);
        assert!(controller.last_reason.contains("Feed Gap or Data Quality"));
    }

    #[test]
    fn test_trend_not_blocked_by_z15_momentum() {
        let mut controller = RegimeController::default();

        // Momentum breakout bar: P_T = 0.92, Donchian breakout = true, Z15 = +2.8 (momentum), Z60 = 1.0, not severe shock
        // Must NOT be blocked by Z15 = 2.8!
        let mode = controller.evaluate_15m_bar(BASE_TIME_MS, 0.05, 0.92, 2.8, 1.0, true, false, false, false);
        assert_eq!(mode, ControllerMode::Trend);
        assert_eq!(controller.trend_streak, 1);
        assert!(controller.last_reason.contains("Trend confirmed"));
    }
}
