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
    pub last_transition_time: String,
    pub last_reason: String,
    pub epoch: u64,
}

impl Default for RegimeController {
    fn default() -> Self {
        Self {
            current_mode: ControllerMode::Neutral,
            trend_streak: 0,
            consolidation_streak: 0,
            last_transition_time: Utc::now().to_rfc3339(),
            last_reason: "Initial system startup (Safe Cash Buffer)".to_string(),
            epoch: 1,
        }
    }
}

impl RegimeController {
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
        // They exclusively route to Neutral (safe cash buffer) and reset streaks.
        if is_severe_shock {
            let was_different = self.current_mode != ControllerMode::Neutral;
            self.current_mode = ControllerMode::Neutral;
            self.trend_streak = 0;
            self.consolidation_streak = 0;
            self.last_reason = format!(
                "Severe Volatility Shock Veto (|Z15|={:.2}, |Z60|={:.2}) -> Safe Neutral Cash Buffer",
                z_15, z_60
            );
            if was_different {
                self.last_transition_time = Utc::now().to_rfc3339();
                self.epoch += 1;
            }
            return self.current_mode;
        }

        // 3. UPDATE STREAK COUNTERS (Score-Margin Hysteresis)
        if p_trend >= 0.85 {
            self.trend_streak += 1;
            self.consolidation_streak = 0;
        } else if p_consolidation >= 0.75 {
            self.consolidation_streak += 1;
            self.trend_streak = 0;
        } else {
            // Deadband region: uncertainty buffer
            self.trend_streak = 0;
            self.consolidation_streak = 0;
        }

        // 4. STATE MACHINE TRANSITIONS
        match self.current_mode {
            ControllerMode::Neutral => {
                // Neutral -> Trend requires 3-bar confirmation (45m dwell) AND Donchian breakout
                if self.trend_streak >= 3 && has_donchian_breakout {
                    self.current_mode = ControllerMode::Trend;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Trend confirmed: P_T >= 0.85 for {} bars + Donchian breakout",
                        self.trend_streak
                    );
                    self.epoch += 1;
                }
                // Neutral -> MeanReversion requires 3-bar confirmation (45m dwell) + valid O-U
                else if self.consolidation_streak >= 3
                    && is_ou_stationary
                    && is_half_life_valid
                    && !is_severe_shock
                {
                    self.current_mode = ControllerMode::MeanReversion;
                    self.last_transition_time = Utc::now().to_rfc3339();
                    self.last_reason = format!(
                        "Consolidation confirmed: P_C >= 0.75 for {} bars + O-U valid",
                        self.consolidation_streak
                    );
                    self.epoch += 1;
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
    fn test_dwell_time_3_bar_requirement_for_trend() {
        let mut controller = RegimeController::default();

        // Bar 1: P_T = 0.88 with breakout
        let mode1 = controller.evaluate_15m_bar(0.10, 0.88, 1.0, 0.5, true, false, false);
        assert_eq!(mode1, ControllerMode::Neutral); // Bar 1: Not yet, dwell time required!
        assert_eq!(controller.trend_streak, 1);

        // Bar 2: P_T = 0.90 with breakout
        let mode2 = controller.evaluate_15m_bar(0.08, 0.90, 1.5, 0.8, true, false, false);
        assert_eq!(mode2, ControllerMode::Neutral); // Bar 2: Still holding neutral buffer
        assert_eq!(controller.trend_streak, 2);

        // Bar 3: P_T = 0.86 with breakout
        let mode3 = controller.evaluate_15m_bar(0.12, 0.86, 1.8, 1.0, true, false, false);
        assert_eq!(mode3, ControllerMode::Trend); // Bar 3: Confirmed! Transition to Trend
        assert_eq!(controller.trend_streak, 3);
    }

    #[test]
    fn test_hysteresis_deadband_prevents_whipsaw() {
        let mut controller = RegimeController::default();

        // 2 bars of trend
        controller.evaluate_15m_bar(0.10, 0.88, 1.0, 0.5, true, false, false);
        controller.evaluate_15m_bar(0.10, 0.88, 1.0, 0.5, true, false, false);
        assert_eq!(controller.trend_streak, 2);

        // Bar 3 enters deadband (P_T = 0.70, P_C = 0.20)
        let mode = controller.evaluate_15m_bar(0.20, 0.70, 0.5, 0.2, true, false, false);
        assert_eq!(mode, ControllerMode::Neutral);
        assert_eq!(controller.trend_streak, 0); // Streak reset by deadband!
    }

    #[test]
    fn test_dwell_time_3_bar_requirement_for_mean_reversion() {
        let mut controller = RegimeController::default();

        // Bar 1: P_C = 0.80
        controller.evaluate_15m_bar(0.80, 0.15, 0.2, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);

        // Bar 2: P_C = 0.82
        controller.evaluate_15m_bar(0.82, 0.12, 0.1, 0.0, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::Neutral);

        // Bar 3: P_C = 0.78
        controller.evaluate_15m_bar(0.78, 0.16, 0.0, 0.1, false, true, true);
        assert_eq!(controller.current_mode, ControllerMode::MeanReversion);
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
}
