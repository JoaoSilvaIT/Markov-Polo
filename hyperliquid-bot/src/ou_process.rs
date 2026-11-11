use std::collections::{HashMap, VecDeque};
use std::f64;
use std::sync::{OnceLock, RwLock};

use crate::state::Side;

/// Asset-specific production calibration from Section 4 of `O-U Mean Reversion Audit.md`
/// and Section 6 of `Hyperliquid OU Audit Payoff Geometry, Causal Residuals and Rust Controls.md`.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct AssetOuConfig {
    pub symbol: &'static str,
    /// Number of 5-minute bars in the rolling O-U estimation window (144 = 12h, 120 = 10h)
    pub lookback_5m_bars: usize,
    /// Valid O-U half-life lower bound in minutes
    pub min_half_life_m: f64,
    /// Valid O-U half-life upper bound in minutes
    pub max_half_life_m: f64,
    /// Minimum equilibrium standard deviation floor (\sigma_{eq}) in basis points
    pub min_sigma_eq_bps: f64,
    /// Base Bertram entry multiple a_0^* (in \sigma_{eq} units)
    pub base_entry_sigma: f64,
    /// Carry-adjusted minimum entry multiple clamp
    pub min_entry_sigma: f64,
    /// Carry-adjusted maximum entry multiple clamp
    pub max_entry_sigma: f64,
    /// Rebound confirmation volatility scaling parameter \gamma
    pub rebound_gamma: f64,
    /// Maximum excursion consumption ratio for Join-Bid/Join-Ask policy (\eta_{join})
    pub max_join_consumption_ratio: f64,
    /// Maximum excursion consumption ratio for Passive Retest policy (\eta_{retest})
    pub max_retest_consumption_ratio: f64,
    /// Minimum required net edge hurdle in basis points (after round-trip Maker fees + funding drag)
    pub min_net_edge_bps: f64,
    /// Maximum target notional cap in USDC on a $145 micro-account
    pub target_notional_cap_usdc: f64,
    /// Tier 1 soft invalidation threshold in frozen \sigma_{eq} units (evaluated over K=2 completed 5m bars)
    pub soft_z_ou_threshold: f64,
    /// Tier 1 persistent 15m MAD shock threshold
    pub soft_z15_shock_threshold: f64,
    /// Tier 2 Exchange-Native Stop-Market hard disaster stop distance in basis points from entry
    pub hard_disaster_stop_bps: f64,
}

impl AssetOuConfig {
    /// Dynamically scales target notional cap with live account equity.
    /// Baseline is calibrated to $142.50 USDC equity:
    /// - BTC / ETH: $68.0 base (scales to ~$240 at $500 equity)
    /// - SOL: $60.0 base (scales to ~$210 at $500 equity)
    /// - AVAX: $55.0 base (scales to ~$190 at $500 equity)
    /// Clamped between 0.35x and 10.0x scale factor to protect margin safety.
    pub fn dynamic_notional_cap(&self, live_equity: f64) -> f64 {
        if live_equity.is_finite() && live_equity > 0.0 {
            let scale = (live_equity / 142.5).clamp(0.35, 10.0);
            (self.target_notional_cap_usdc * scale).round()
        } else {
            self.target_notional_cap_usdc
        }
    }

    /// Computes the equity- and volatility-adjusted notional ceiling (Inverse Volatility Scaling) for Engine A.
    /// Automatically contracts maximum position notional when stationary 5m O-U volatility (\sigma_{eq})
    /// expands above the asset's normal baseline, keeping dollar risk constant across volatility regimes.
    pub fn volatility_adjusted_notional_cap(
        &self,
        live_equity: f64,
        current_sigma_eq_log: f64,
    ) -> f64 {
        let eq_cap = self.dynamic_notional_cap(live_equity);
        let ref_sigma_bps = self.min_sigma_eq_bps * 1.25;
        let curr_sigma_bps = (current_sigma_eq_log * 1e4).max(self.min_sigma_eq_bps);
        if curr_sigma_bps.is_finite() && curr_sigma_bps > 0.0 {
            let vol_mult = (ref_sigma_bps / curr_sigma_bps).clamp(0.45, 1.05);
            (eq_cap * vol_mult).round().max(11.0)
        } else {
            eq_cap
        }
    }

    /// Computes the equity- and ATR-adjusted notional ceiling (Inverse ATR Scaling) for Engine B (15m Trend Breakout).
    /// Scales down position notional ceiling when 15m Wilder ATR expands during high-volatility storms.
    pub fn volatility_adjusted_trend_notional_cap(
        &self,
        live_equity: f64,
        close_price: f64,
        current_atr_15m: f64,
    ) -> f64 {
        let eq_cap = self.dynamic_notional_cap(live_equity);
        if close_price <= 0.0
            || !close_price.is_finite()
            || current_atr_15m <= 0.0
            || !current_atr_15m.is_finite()
        {
            return eq_cap;
        }
        let atr_bps = (current_atr_15m / close_price) * 1e4;
        let ref_atr_bps = match self.symbol.to_uppercase().as_str() {
            "XYZ:GOLD" => 22.0,
            "XYZ:XYZ100" => 25.0,
            "BTC" | "XYZ:SILVER" => 35.0,
            "XYZ:NVDA" => 40.0,
            "ETH" => 45.0,
            "XYZ:TSLA" => 50.0,
            "XRP" => 55.0,
            "SOL" | "LINK" | "ZEC" => 65.0,
            "HYPE" => 70.0,
            "AVAX" | "SUI" => 75.0,
            "NEAR" => 85.0,
            _ => 45.0,
        };
        let vol_mult = (ref_atr_bps / atr_bps.max(10.0)).clamp(0.45, 1.05);
        (eq_cap * vol_mult).round().max(11.0)
    }
}

pub fn asset_ou_config(symbol: &str) -> AssetOuConfig {
    match symbol.to_uppercase().as_str() {
        "BTC" => AssetOuConfig {
            symbol: "BTC",
            lookback_5m_bars: 144,
            min_half_life_m: 15.0,
            max_half_life_m: 150.0,
            min_sigma_eq_bps: 22.0,
            base_entry_sigma: 1.60,
            min_entry_sigma: 1.40,
            max_entry_sigma: 1.90,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            min_net_edge_bps: 22.0,
            target_notional_cap_usdc: 68.0,
            soft_z_ou_threshold: 2.75,
            soft_z15_shock_threshold: 3.00,
            hard_disaster_stop_bps: 150.0,
        },
        "ETH" => AssetOuConfig {
            symbol: "ETH",
            lookback_5m_bars: 144,
            min_half_life_m: 15.0,
            max_half_life_m: 150.0,
            min_sigma_eq_bps: 26.0,
            base_entry_sigma: 1.60,
            min_entry_sigma: 1.40,
            max_entry_sigma: 1.90,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            min_net_edge_bps: 25.0,
            target_notional_cap_usdc: 68.0,
            soft_z_ou_threshold: 2.75,
            soft_z15_shock_threshold: 3.00,
            hard_disaster_stop_bps: 175.0,
        },
        "SOL" => AssetOuConfig {
            symbol: "SOL",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 30.0,
            base_entry_sigma: 1.65,
            min_entry_sigma: 1.45,
            max_entry_sigma: 1.95,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 28.0,
            target_notional_cap_usdc: 60.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 200.0,
        },
        "AVAX" => AssetOuConfig {
            symbol: "AVAX",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 34.0,
            base_entry_sigma: 1.70,
            min_entry_sigma: 1.50,
            max_entry_sigma: 2.00,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 32.0,
            target_notional_cap_usdc: 55.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 220.0,
        },
        "HYPE" => AssetOuConfig {
            symbol: "HYPE",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 34.0,
            base_entry_sigma: 1.70,
            min_entry_sigma: 1.50,
            max_entry_sigma: 2.00,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 32.0,
            target_notional_cap_usdc: 55.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 220.0,
        },
        "NEAR" => AssetOuConfig {
            symbol: "NEAR",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 38.0,
            base_entry_sigma: 1.75,
            min_entry_sigma: 1.55,
            max_entry_sigma: 2.05,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 35.0,
            target_notional_cap_usdc: 50.0,
            soft_z_ou_threshold: 2.90,
            soft_z15_shock_threshold: 3.30,
            hard_disaster_stop_bps: 240.0,
        },
        "XRP" => AssetOuConfig {
            symbol: "XRP",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 30.0,
            base_entry_sigma: 1.65,
            min_entry_sigma: 1.45,
            max_entry_sigma: 1.95,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 28.0,
            target_notional_cap_usdc: 60.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 200.0,
        },
        "SUI" => AssetOuConfig {
            symbol: "SUI",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 35.0,
            base_entry_sigma: 1.70,
            min_entry_sigma: 1.50,
            max_entry_sigma: 2.00,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 32.0,
            target_notional_cap_usdc: 55.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 220.0,
        },
        "LINK" => AssetOuConfig {
            symbol: "LINK",
            lookback_5m_bars: 120,
            min_half_life_m: 15.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 32.0,
            base_entry_sigma: 1.68,
            min_entry_sigma: 1.48,
            max_entry_sigma: 1.98,
            rebound_gamma: 0.30,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.45,
            min_net_edge_bps: 30.0,
            target_notional_cap_usdc: 58.0,
            soft_z_ou_threshold: 2.85,
            soft_z15_shock_threshold: 3.20,
            hard_disaster_stop_bps: 210.0,
        },
        "XYZ:NVDA" | "NVDA" => AssetOuConfig {
            symbol: "xyz:NVDA",
            lookback_5m_bars: 120,
            min_half_life_m: 8.0,
            max_half_life_m: 75.0,
            min_sigma_eq_bps: 20.0,
            base_entry_sigma: 1.58,
            min_entry_sigma: 1.40,
            max_entry_sigma: 1.88,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            min_net_edge_bps: 19.0,
            target_notional_cap_usdc: 65.0,
            soft_z_ou_threshold: 2.50,
            soft_z15_shock_threshold: 2.80,
            hard_disaster_stop_bps: 140.0,
        },
        "XYZ:XYZ100" | "XYZ100" => AssetOuConfig {
            symbol: "xyz:XYZ100",
            lookback_5m_bars: 120,
            min_half_life_m: 8.0,
            max_half_life_m: 75.0,
            min_sigma_eq_bps: 16.0,
            base_entry_sigma: 1.52,
            min_entry_sigma: 1.38,
            max_entry_sigma: 1.82,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            min_net_edge_bps: 16.0,
            target_notional_cap_usdc: 72.0,
            soft_z_ou_threshold: 2.45,
            soft_z15_shock_threshold: 2.75,
            hard_disaster_stop_bps: 115.0,
        },
        "XYZ:GOLD" | "GOLD" => AssetOuConfig {
            symbol: "xyz:GOLD",
            lookback_5m_bars: 144,
            min_half_life_m: 8.0,
            max_half_life_m: 60.0,
            min_sigma_eq_bps: 16.0,
            base_entry_sigma: 2.05,
            min_entry_sigma: 1.85,
            max_entry_sigma: 2.35,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            // Non-growth HIP-3 GOLD pays 5.76 bps MM / 12.12 bps MT RT: reserve Engine A for extreme >=2.05σ tails
            min_net_edge_bps: 26.0,
            target_notional_cap_usdc: 75.0,
            soft_z_ou_threshold: 2.40,
            soft_z15_shock_threshold: 2.70,
            hard_disaster_stop_bps: 120.0,
        },
        "XYZ:SILVER" | "SILVER" => AssetOuConfig {
            symbol: "xyz:SILVER",
            lookback_5m_bars: 120,
            min_half_life_m: 8.0,
            max_half_life_m: 60.0,
            min_sigma_eq_bps: 20.0,
            base_entry_sigma: 1.62,
            min_entry_sigma: 1.42,
            max_entry_sigma: 1.92,
            rebound_gamma: 0.34,
            max_join_consumption_ratio: 0.24,
            max_retest_consumption_ratio: 0.48,
            min_net_edge_bps: 20.0,
            target_notional_cap_usdc: 64.0,
            soft_z_ou_threshold: 2.55,
            soft_z15_shock_threshold: 2.85,
            hard_disaster_stop_bps: 145.0,
        },
        "XYZ:TSLA" | "TSLA" => AssetOuConfig {
            symbol: "xyz:TSLA",
            lookback_5m_bars: 120,
            min_half_life_m: 8.0,
            max_half_life_m: 75.0,
            min_sigma_eq_bps: 22.0,
            base_entry_sigma: 1.65,
            min_entry_sigma: 1.42,
            max_entry_sigma: 1.95,
            rebound_gamma: 0.32,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.48,
            min_net_edge_bps: 22.0,
            target_notional_cap_usdc: 60.0,
            soft_z_ou_threshold: 2.60,
            soft_z15_shock_threshold: 2.90,
            hard_disaster_stop_bps: 150.0,
        },
        "ZEC" => AssetOuConfig {
            symbol: "ZEC",
            lookback_5m_bars: 96,
            min_half_life_m: 10.0,
            max_half_life_m: 135.0,
            min_sigma_eq_bps: 26.0,
            base_entry_sigma: 1.68,
            min_entry_sigma: 1.45,
            max_entry_sigma: 1.98,
            rebound_gamma: 0.34,
            max_join_consumption_ratio: 0.22,
            max_retest_consumption_ratio: 0.48,
            min_net_edge_bps: 24.0,
            target_notional_cap_usdc: 65.0,
            soft_z_ou_threshold: 2.70,
            soft_z15_shock_threshold: 3.00,
            hard_disaster_stop_bps: 180.0,
        },
        _ => AssetOuConfig {
            symbol: "ETH",
            lookback_5m_bars: 144,
            min_half_life_m: 15.0,
            max_half_life_m: 150.0,
            min_sigma_eq_bps: 26.0,
            base_entry_sigma: 1.60,
            min_entry_sigma: 1.40,
            max_entry_sigma: 1.90,
            rebound_gamma: 0.35,
            max_join_consumption_ratio: 0.25,
            max_retest_consumption_ratio: 0.50,
            min_net_edge_bps: 25.0,
            target_notional_cap_usdc: 68.0,
            soft_z_ou_threshold: 2.75,
            soft_z15_shock_threshold: 3.00,
            hard_disaster_stop_bps: 175.0,
        },
    }
}

// ============================================================================
// 1. CAUSAL RESIDUAL SPECIFICATION (KALMAN LLT + EW-RLS LEAD-LAG FILTER)
// ============================================================================

/// Two-State Causal Kalman Local Linear Trend (LLT) Filter with Joseph covariance update
/// and standardized one-step innovation CUSUM tracking.
/// Separates macro log-price level (g_t) and local drift velocity (b_t)
/// from stationary intraday oscillations without Slutsky-Yule echo cycles.
#[derive(Debug, Clone)]
pub struct KalmanLltFilter {
    /// State vector: [g_t (level), b_t (slope per 5m bar)]
    pub g: f64,
    pub b: f64,
    /// 2x2 symmetric state covariance matrix P = [[p00, p01], [p10, p11]]
    pub p00: f64,
    pub p01: f64,
    pub p11: f64,
    /// Dimensionless state-to-measurement noise ratios
    pub q_g_ratio: f64,
    pub q_b_ratio: f64,
    /// Causal EWMA estimate of 5m diffusive return variance
    pub ewma_var: f64,
    pub var_decay: f64,
    pub prev_obs: Option<f64>,
    /// One-sided innovation CUSUM statistics (k = 0.5 reference shift)
    pub cusum_pos: f64,
    pub cusum_neg: f64,
}

/// Output of a single 5-minute bar update of the Kalman LLT filter.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct LltOutput {
    /// Causal residual y_t = obs_t - g_{t|t-1}
    pub causal_residual: f64,
    /// A priori predicted equilibrium log-price g_{t|t-1}
    pub predicted_level: f64,
    /// A posteriori updated equilibrium log-price g_{t|t}
    pub updated_level: f64,
    /// A posteriori local drift velocity b_{t|t} (log-return per 5m bar)
    pub updated_slope: f64,
    /// Standardized slope velocity |b_{t|t}| / sigma_{v,t}
    pub standardized_slope: f64,
    /// True if |b_{t|t}| / sigma_{v,t} exceeds the waterfall drift veto threshold
    pub drift_veto_active: bool,
    /// Standardized one-step prediction innovation \xi_t = (obs_t - g_{t|t-1}) / \sqrt{S_t}
    pub std_innovation: f64,
    /// Upside innovation CUSUM C_t^+
    pub cusum_pos: f64,
    /// Downside innovation CUSUM C_t^-
    pub cusum_neg: f64,
}

impl KalmanLltFilter {
    pub fn new(initial_sigma_5m: f64, q_g_ratio: f64, q_b_ratio: f64) -> Self {
        let init_var = (initial_sigma_5m.max(1e-4)).powi(2);
        Self {
            g: 0.0,
            b: 0.0,
            p00: init_var,
            p01: 0.0,
            p11: init_var * 0.1,
            q_g_ratio,
            q_b_ratio,
            ewma_var: init_var,
            var_decay: 0.94,
            prev_obs: None,
            cusum_pos: 0.0,
            cusum_neg: 0.0,
        }
    }

    /// Processes a completed 5-minute log-price observation (or cumulative idiosyncratic index).
    pub fn update(&mut self, obs: f64, max_std_slope_veto: f64) -> LltOutput {
        let prev = match self.prev_obs {
            Some(p) => p,
            None => {
                self.prev_obs = Some(obs);
                self.g = obs;
                self.b = 0.0;
                return LltOutput {
                    causal_residual: 0.0,
                    predicted_level: obs,
                    updated_level: obs,
                    updated_slope: 0.0,
                    standardized_slope: 0.0,
                    drift_veto_active: false,
                    std_innovation: 0.0,
                    cusum_pos: 0.0,
                    cusum_neg: 0.0,
                };
            }
        };

        // Causal measurement variance R_t uses variance known at t-1
        let r_t = self.ewma_var.max(1e-10);
        let q_g_var = (self.q_g_ratio * self.q_g_ratio) * r_t;
        let q_b_var = (self.q_b_ratio * self.q_b_ratio) * r_t;

        // 1. A priori state prediction: x_{t|t-1} = F * x_{t-1|t-1}
        let g_pred = self.g + self.b;
        let b_pred = self.b;

        // Covariance prediction: P_{t|t-1} = F * P_{t-1|t-1} * F^T + Q_t
        let p00_pred = self.p00 + 2.0 * self.p01 + self.p11 + q_g_var;
        let p01_pred = self.p01 + self.p11;
        let p11_pred = self.p11 + q_b_var;

        // 2. Causal innovation (predictive residual before incorporating obs_t)
        let innovation = obs - g_pred;
        let s_t = (p00_pred + r_t).max(1e-12);
        let std_innov = (innovation / s_t.sqrt()).clamp(-8.0, 8.0);

        // Update one-step standardized innovation CUSUMs (k = 0.5)
        self.cusum_pos = (self.cusum_pos + std_innov - 0.5).max(0.0);
        self.cusum_neg = (self.cusum_neg - std_innov - 0.5).max(0.0);

        // 3. Kalman gain: K_t = P_{t|t-1} * H^T / S_t
        let k_g = p00_pred / s_t;
        let k_b = p01_pred / s_t;

        // 4. A posteriori state update
        self.g = g_pred + k_g * innovation;
        self.b = b_pred + k_b * innovation;

        // 5. Joseph stabilized covariance update: P^+ = (I - K H) P^- (I - K H)' + K R K'
        let c0 = 1.0 - k_g;
        let new_p00 = c0 * c0 * p00_pred + k_g * k_g * r_t;
        let new_p01 = c0 * (p01_pred - k_b * p00_pred) + k_g * k_b * r_t;
        let new_p11 = p11_pred - 2.0 * k_b * p01_pred + k_b * k_b * (p00_pred + r_t);
        self.p00 = new_p00.max(1e-12);
        self.p01 = new_p01;
        self.p11 = new_p11.max(1e-12);

        // Update causal EWMA diffusive return variance for bar t+1
        let ret = obs - prev;
        let clipped_ret_sq = ret.powi(2).min(16.0 * r_t);
        self.ewma_var = self.var_decay * self.ewma_var + (1.0 - self.var_decay) * clipped_ret_sq;
        self.prev_obs = Some(obs);

        let sigma_v = self.ewma_var.sqrt().max(1e-6);
        let std_slope = self.b.abs() / sigma_v;
        // A waterfall/melt-up drift veto requires both an elevated standardized slope velocity
        // and cumulative one-step innovation pressure (CUSUM >= 4.0) in the same direction,
        // separating genuine multi-bar waterfall cascades from normal O-U band excursions.
        let same_dir_cusum = if self.b < 0.0 {
            self.cusum_neg
        } else {
            self.cusum_pos
        };
        let drift_veto = std_slope > max_std_slope_veto && same_dir_cusum >= 4.0;

        LltOutput {
            causal_residual: innovation,
            predicted_level: g_pred,
            updated_level: self.g,
            updated_slope: self.b,
            standardized_slope: std_slope,
            drift_veto_active: drift_veto,
            std_innovation: std_innov,
            cusum_pos: self.cusum_pos,
            cusum_neg: self.cusum_neg,
        }
    }
}

/// Huber-Robust Exponentially Weighted Recursive Least Squares (EW-RLS)
/// for Altcoin-to-BTC Dynamic Beta & Lag-1 Lead-Lag Orthogonalization.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AltcoinCausalResidualEngine {
    /// Parameter vector theta = [alpha, beta_0 (contemp), beta_1 (lag-1 BTC)]
    pub theta: [f64; 3],
    /// 3x3 covariance matrix P_rls stored in row-major array
    pub p_mat: [[f64; 3]; 3],
    /// Forgetting factor lambda_rls (e.g., 0.996 for ~250-bar effective memory)
    pub lambda: f64,
    /// Causal EWMA variance of idiosyncratic return innovation u_{k,t}
    pub idio_ret_var: f64,
    /// Cumulative idiosyncratic log-price index U_{k,t}
    pub cumulative_idio_index: f64,
    /// Inner Kalman LLT filter operating on U_{k,t}
    pub llt_filter: KalmanLltFilter,
    pub prev_alt_log_px: Option<f64>,
    pub prev_btc_log_px: Option<f64>,
    pub prev_btc_ret: f64,
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct AltcoinResidualOutput {
    /// Final orthogonalized causal residual y_{k,t} for O-U fitting
    pub y_residual: f64,
    /// Contemporaneous + Lag-1 total BTC beta (\hat{\beta}_0 + \hat{\beta}_1)
    pub total_btc_beta: f64,
    /// Idiosyncratic LLT drift veto
    pub idio_drift_veto: bool,
}

#[allow(dead_code)]
impl AltcoinCausalResidualEngine {
    pub fn new(lambda: f64, init_beta0: f64, init_beta1: f64, init_sigma_5m: f64) -> Self {
        let mut p_mat = [[0.0; 3]; 3];
        p_mat[0][0] = 1e-2;
        p_mat[1][1] = 1.0;
        p_mat[2][2] = 1.0;
        Self {
            theta: [0.0, init_beta0, init_beta1],
            p_mat,
            lambda,
            idio_ret_var: (init_sigma_5m.max(1e-4)).powi(2),
            cumulative_idio_index: 0.0,
            llt_filter: KalmanLltFilter::new(init_sigma_5m, 0.25, 0.0625),
            prev_alt_log_px: None,
            prev_btc_log_px: None,
            prev_btc_ret: 0.0,
        }
    }

    /// Updates the EW-RLS and Kalman LLT state on a completed 5-minute bar.
    pub fn update_5m(
        &mut self,
        alt_log_px: f64,
        btc_log_px: f64,
        max_std_slope_veto: f64,
    ) -> AltcoinResidualOutput {
        let (prev_alt, prev_btc) = match (self.prev_alt_log_px, self.prev_btc_log_px) {
            (Some(pa), Some(pb)) => (pa, pb),
            _ => {
                self.prev_alt_log_px = Some(alt_log_px);
                self.prev_btc_log_px = Some(btc_log_px);
                let _ = self.llt_filter.update(0.0, max_std_slope_veto);
                return AltcoinResidualOutput {
                    y_residual: 0.0,
                    total_btc_beta: self.theta[1] + self.theta[2],
                    idio_drift_veto: false,
                };
            }
        };

        let r_alt = alt_log_px - prev_alt;
        let r_btc = btc_log_px - prev_btc;
        let phi = [1.0, r_btc, self.prev_btc_ret];

        // 1. Causal a priori idiosyncratic return innovation u_{k,t}
        let pred_r_alt = phi[0] * self.theta[0] + phi[1] * self.theta[1] + phi[2] * self.theta[2];
        let u_innov = r_alt - pred_r_alt;

        // 2. Huber clipping for RLS parameter stability against liquidation wicks
        let sigma_u = self.idio_ret_var.sqrt().max(1e-5);
        let u_clipped = u_innov.clamp(-3.5 * sigma_u, 3.5 * sigma_u);

        // 3. Compute P_{t-1} * phi
        let mut p_phi = [0.0; 3];
        for i in 0..3 {
            p_phi[i] =
                self.p_mat[i][0] * phi[0] + self.p_mat[i][1] * phi[1] + self.p_mat[i][2] * phi[2];
        }

        // Denominator: lambda + phi^T * P_{t-1} * phi
        let denom =
            (self.lambda + phi[0] * p_phi[0] + phi[1] * p_phi[1] + phi[2] * p_phi[2]).max(1e-12);

        let k_gain = [p_phi[0] / denom, p_phi[1] / denom, p_phi[2] / denom];

        // 4. Update parameter vector theta (with physical sanity bounds on beta)
        for i in 0..3 {
            self.theta[i] += k_gain[i] * u_clipped;
        }
        self.theta[1] = self.theta[1].clamp(0.10, 3.50);
        self.theta[2] = self.theta[2].clamp(-0.50, 1.50);

        // 5. Update RLS covariance matrix: P_t = (P_{t-1} - k * (phi^T * P_{t-1})) / lambda
        let mut phi_t_p = [0.0; 3];
        for j in 0..3 {
            phi_t_p[j] =
                phi[0] * self.p_mat[0][j] + phi[1] * self.p_mat[1][j] + phi[2] * self.p_mat[2][j];
        }
        let inv_lambda = 1.0 / self.lambda;
        for i in 0..3 {
            for j in 0..3 {
                self.p_mat[i][j] = (self.p_mat[i][j] - k_gain[i] * phi_t_p[j]) * inv_lambda;
            }
        }

        // 6. Accumulate unclipped idiosyncratic return & filter via Kalman LLT
        self.idio_ret_var = 0.94 * self.idio_ret_var + 0.06 * u_clipped.powi(2);
        self.cumulative_idio_index += u_innov;
        let llt_out = self
            .llt_filter
            .update(self.cumulative_idio_index, max_std_slope_veto);

        self.prev_alt_log_px = Some(alt_log_px);
        self.prev_btc_log_px = Some(btc_log_px);
        self.prev_btc_ret = r_btc;

        AltcoinResidualOutput {
            y_residual: llt_out.causal_residual,
            total_btc_beta: self.theta[1] + self.theta[2],
            idio_drift_veto: llt_out.drift_veto_active,
        }
    }

    /// Evaluates the 1-minute BTC Lead-Lag Impulse Score I_{BTC -> k}(t)
    /// and returns true if altcoin entries must be vetoed.
    pub fn check_btc_impulse_veto(
        &self,
        side: Side,
        btc_3m_log_return: f64,
        btc_sigma_1m: f64,
        btc_llt_drift_veto: bool,
    ) -> bool {
        if btc_llt_drift_veto {
            return true;
        }
        let denom = (3.0_f64.sqrt() * btc_sigma_1m).max(1e-6);
        let btc_z_3m = btc_3m_log_return / denom;
        let total_beta = (self.theta[1] + self.theta[2]).max(0.5);
        let impulse_score = total_beta * btc_z_3m.abs();

        // Hard impulse veto if BTC moves violently in either direction
        if impulse_score >= 2.20 {
            return true;
        }
        // Directional contagion veto: never fade a 1.5-sigma 3m BTC move
        match side {
            Side::Long if btc_z_3m < -1.50 => true,
            Side::Short if btc_z_3m > 1.50 => true,
            _ => false,
        }
    }
}

// ============================================================================
// 2. MULTI-BAR REVERSAL CONFIRMATION & ALO ORDER-BOOK PLACEMENT STATE MACHINE
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum ConfirmationState {
    Idle,
    Armed {
        side: Side,
        arm_price: f64,
        mean_target_price: f64,
        extreme_price: f64,
        bars_since_extreme: usize,
        total_armed_bars: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum OrderPlacementAction {
    Wait,
    DisarmAbort,
    PostAloJoinBook {
        side: Side,
        limit_price: f64,
        consumed_ratio: f64,
        entry_ttl_mins: f64,
    },
    PostAloRetestLimit {
        side: Side,
        limit_price: f64,
        consumed_ratio: f64,
        entry_ttl_mins: f64,
    },
}

#[allow(dead_code)]
pub struct ReversalConfirmationMachine {
    pub state: ConfirmationState,
    pub gamma: f64,
    pub max_join_consumption_ratio: f64,
    pub max_retest_consumption_ratio: f64,
    pub min_net_edge_bps: f64,
    pub maker_fee_bps: f64,
    pub max_armed_minutes: usize,
    pub recent_1m_closes: VecDeque<f64>,
}

impl ReversalConfirmationMachine {
    #[allow(dead_code)]
    pub fn new(
        gamma: f64,
        max_join_consumption_ratio: f64,
        max_retest_consumption_ratio: f64,
        min_net_edge_bps: f64,
        maker_fee_bps: f64,
    ) -> Self {
        Self {
            state: ConfirmationState::Idle,
            gamma,
            max_join_consumption_ratio,
            max_retest_consumption_ratio,
            min_net_edge_bps,
            maker_fee_bps,
            max_armed_minutes: 15,
            recent_1m_closes: VecDeque::with_capacity(4),
        }
    }

    #[allow(dead_code)]
    pub fn arm(&mut self, side: Side, current_price: f64, mean_target_price: f64) {
        if matches!(self.state, ConfirmationState::Idle) {
            self.state = ConfirmationState::Armed {
                side,
                arm_price: current_price,
                mean_target_price,
                extreme_price: current_price,
                bars_since_extreme: 0,
                total_armed_bars: 0,
            };
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.state = ConfirmationState::Idle;
    }

    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn on_1m_bar_close(
        &mut self,
        bar_high: f64,
        bar_low: f64,
        bar_close: f64,
        best_bid: f64,
        best_ask: f64,
        tick_size: f64,
        sigma_1m_prev: f64,
    ) -> OrderPlacementAction {
        self.recent_1m_closes.push_back(bar_close);
        if self.recent_1m_closes.len() > 4 {
            self.recent_1m_closes.pop_front();
        }

        let (
            side,
            arm_price,
            mean_target_price,
            mut extreme_price,
            mut bars_since_extreme,
            mut total_armed_bars,
        ) = match self.state.clone() {
            ConfirmationState::Idle => return OrderPlacementAction::Wait,
            ConfirmationState::Armed {
                side,
                arm_price,
                mean_target_price,
                extreme_price,
                bars_since_extreme,
                total_armed_bars,
            } => (
                side,
                arm_price,
                mean_target_price,
                extreme_price,
                bars_since_extreme,
                total_armed_bars,
            ),
        };

        let closes_vec: Vec<f64> = self.recent_1m_closes.iter().copied().collect();
        let action = evaluate_armed_reversal_1m_step(
            &side,
            arm_price,
            mean_target_price,
            &mut extreme_price,
            &mut bars_since_extreme,
            &mut total_armed_bars,
            self.max_armed_minutes,
            self.gamma,
            self.max_join_consumption_ratio,
            self.max_retest_consumption_ratio,
            self.min_net_edge_bps,
            self.maker_fee_bps,
            0.0,
            bar_high,
            bar_low,
            bar_close,
            &closes_vec,
            best_bid,
            best_ask,
            tick_size,
            sigma_1m_prev,
        );

        match action {
            OrderPlacementAction::Wait => {
                self.state = ConfirmationState::Armed {
                    side,
                    arm_price,
                    mean_target_price,
                    extreme_price,
                    bars_since_extreme,
                    total_armed_bars,
                };
            }
            _ => {
                self.reset();
            }
        }
        action
    }

    #[inline]
    pub fn round_down_to_tick(price: f64, tick_size: f64) -> f64 {
        if tick_size <= 0.0 {
            return price;
        }
        (price / tick_size).floor() * tick_size
    }

    #[inline]
    pub fn round_up_to_tick(price: f64, tick_size: f64) -> f64 {
        if tick_size <= 0.0 {
            return price;
        }
        (price / tick_size).ceil() * tick_size
    }
}

/// Stateless/persisted step evaluator for the Multi-Bar 1-Minute Reversal Confirmation Machine
/// (`O-U Mean Reversion Audit.md` Section 3 + `Hyperliquid OU Audit Payoff Geometry...md` Section 5).
#[allow(clippy::too_many_arguments)]
pub fn evaluate_armed_reversal_1m_step(
    side: &Side,
    arm_price: f64,
    mean_target_price: f64,
    extreme_price: &mut f64,
    bars_since_extreme: &mut usize,
    total_armed_bars: &mut usize,
    max_armed_minutes: usize,
    gamma: f64,
    max_join_consumption_ratio: f64,
    max_retest_consumption_ratio: f64,
    min_net_edge_bps: f64,
    maker_fee_bps: f64,
    expected_funding_drag_bps: f64,
    bar_high: f64,
    bar_low: f64,
    bar_close: f64,
    recent_1m_closes: &[f64],
    best_bid: f64,
    best_ask: f64,
    tick_size: f64,
    sigma_1m_prev: f64,
) -> OrderPlacementAction {
    *total_armed_bars += 1;
    if *total_armed_bars > max_armed_minutes {
        return OrderPlacementAction::DisarmAbort;
    }

    // 1. Track 2-bar extreme persistence:
    // Reset only on a strictly more extreme intrabar print (Per Researcher 1 line 384: "Equal extremes do not reset the count").
    let eps = tick_size * 0.1;
    match side {
        Side::Long => {
            if bar_low < *extreme_price - eps {
                *extreme_price = bar_low;
                *bars_since_extreme = 0;
            } else {
                *bars_since_extreme += 1;
            }
        }
        Side::Short => {
            if bar_high > *extreme_price + eps {
                *extreme_price = bar_high;
                *bars_since_extreme = 0;
            } else {
                *bars_since_extreme += 1;
            }
        }
    }

    // Require at least 2 completed 1m bars without a new extreme and at least 3 closes for the 3-bar reversal return
    if *bars_since_extreme < 2 || recent_1m_closes.len() < 3 {
        return OrderPlacementAction::Wait;
    }

    // 2. Compute dimensionless rebound hurdle b_t (in bps)
    let mid_px = if best_bid > 0.0 && best_ask > 0.0 {
        0.5 * (best_bid + best_ask)
    } else {
        bar_close.max(1e-6)
    };
    let spread_bps = if best_bid > 0.0 && best_ask >= best_bid {
        ((best_ask - best_bid).max(tick_size) / mid_px) * 1e4
    } else {
        (tick_size / mid_px) * 1e4
    };
    let tick_bps = (tick_size / mid_px) * 1e4;
    let vol_rebound_bps = gamma * sigma_1m_prev * 1e4;
    let b_t_bps = (3.0 * spread_bps).max(2.0 * tick_bps).max(vol_rebound_bps);

    // Actual log-return rebound from extreme in bps (Researcher 1 line 397: s*p_t - E_t >= b_t / 10^4)
    let rebound_bps = match side {
        Side::Long => (bar_close / (*extreme_price).max(1e-9)).ln() * 1e4,
        Side::Short => ((*extreme_price).max(1e-9) / bar_close.max(1e-9)).ln() * 1e4,
    };
    if rebound_bps < b_t_bps {
        return OrderPlacementAction::Wait;
    }

    // 3. Frozen arm boundary recross check
    let recrossed = match side {
        Side::Long => bar_close >= arm_price,
        Side::Short => bar_close <= arm_price,
    };
    if !recrossed {
        return OrderPlacementAction::Wait;
    }

    // 4. 3-bar reversal cumulative log return sign check (from trough bar close t-2 to confirming close t)
    let n_c = recent_1m_closes.len();
    let close_trough_bar = recent_1m_closes[n_c - 3];
    if close_trough_bar <= 0.0 || bar_close <= 0.0 {
        return OrderPlacementAction::Wait;
    }
    let cum_log_ret_3m = (bar_close / close_trough_bar).ln();
    let return_sign_valid = match side {
        Side::Long => cum_log_ret_3m > 0.0,
        Side::Short => cum_log_ret_3m < 0.0,
    };
    if !return_sign_valid {
        return OrderPlacementAction::Wait;
    }

    // 5. Evaluate Edge-Consumption Ratio and compute Alo Limit Price
    let total_excursion = (mean_target_price - *extreme_price).abs().max(tick_size);
    let consumed_excursion = (bar_close - *extreme_price).abs();
    let consumed_ratio = consumed_excursion / total_excursion;

    if consumed_ratio > max_retest_consumption_ratio {
        return OrderPlacementAction::DisarmAbort;
    }

    // Deduct expected TP front-running offset before applying the gross/net-edge hurdle (Researcher 1 line 447)
    let tp_offset_bps =
        compute_safe_tp_offset_bps(mean_target_price, bar_close, tick_size).unwrap_or(0.0);
    let c_req = (min_net_edge_bps
        + 2.0 * maker_fee_bps
        + expected_funding_drag_bps.max(0.0)
        + tp_offset_bps)
        * 1e-4;
    let m_star = mean_target_price.ln();

    let eff_bid = if best_bid > 0.0 { best_bid } else { bar_close };
    let eff_ask = if best_ask > 0.0 {
        best_ask
    } else {
        bar_close + tick_size
    };

    if consumed_ratio <= max_join_consumption_ratio {
        // Regime A: Controlled rebound -> Join-Bid / Join-Ask Policy capped by exp(m* -/+ c_req)
        let limit_price = match side {
            Side::Long => {
                let max_allowed_px = (m_star - c_req).exp();
                let book_join_px = eff_bid.min(eff_ask - tick_size);
                ReversalConfirmationMachine::round_down_to_tick(
                    book_join_px.min(max_allowed_px),
                    tick_size,
                )
            }
            Side::Short => {
                let min_allowed_px = (m_star + c_req).exp();
                let book_join_px = eff_ask.max(eff_bid + tick_size);
                ReversalConfirmationMachine::round_up_to_tick(
                    book_join_px.max(min_allowed_px),
                    tick_size,
                )
            }
        };
        OrderPlacementAction::PostAloJoinBook {
            side: side.clone(),
            limit_price,
            consumed_ratio,
            entry_ttl_mins: 2.0,
        }
    } else {
        // Regime B: Over-extended V-bounce -> Passive Retest Limit Policy (3-minute TTL)
        let limit_price = match side {
            Side::Long => {
                let max_allowed_px = (m_star - c_req).exp();
                let retest_anchor = arm_price
                    .max(*extreme_price * (1.0 + b_t_bps * 1e-4))
                    .max(mean_target_price - 0.80 * (mean_target_price - *extreme_price));
                let safe_alo_px = retest_anchor.min(eff_ask - tick_size).min(max_allowed_px);
                ReversalConfirmationMachine::round_down_to_tick(safe_alo_px, tick_size)
            }
            Side::Short => {
                let min_allowed_px = (m_star + c_req).exp();
                let retest_anchor = arm_price
                    .min(*extreme_price * (1.0 - b_t_bps * 1e-4))
                    .min(mean_target_price + 0.80 * (*extreme_price - mean_target_price));
                let safe_alo_px = retest_anchor.max(eff_bid + tick_size).max(min_allowed_px);
                ReversalConfirmationMachine::round_up_to_tick(safe_alo_px, tick_size)
            }
        };
        OrderPlacementAction::PostAloRetestLimit {
            side: side.clone(),
            limit_price,
            consumed_ratio,
            entry_ttl_mins: 3.0,
        }
    }
}

/// Repairs the TP front-running offset clamp per `Hyperliquid OU Audit Payoff Geometry...md` (lines 443–448):
/// L = max(2 * tick_bps, 2.0), U = min(10.0, 0.15 * D_rem_bps).
/// If L > U, returns None; otherwise returns Some(min(max(5.0, L), U)).
pub fn compute_safe_tp_offset_bps(target_mu: f64, entry_price: f64, tick_size: f64) -> Option<f64> {
    if target_mu <= 0.0 || entry_price <= 0.0 || tick_size <= 0.0 {
        return None;
    }
    let d_rem_bps = ((target_mu - entry_price).abs() / entry_price) * 1e4;
    let tick_bps = (tick_size / target_mu) * 1e4;
    let lower = (2.0 * tick_bps).max(2.0);
    let upper = 10.0_f64.min(0.15 * d_rem_bps);
    if lower > upper {
        None
    } else {
        Some(5.0_f64.max(lower).min(upper))
    }
}

/// Phase 2 Dynamic Volatility- and Queue-Adaptive TP Shave (\eta^*):
/// Shaves a smaller fraction (~5.0 bps) when O-U half-life is fast (<= 35m), and scales up the shave
/// toward 12-15% of `d_rem_bps` (up to 10.0 bps) when `half_life_m` is slow (> 55m), increasing maker
/// TP fill rate before the Profit-Harvest Ratchet or HMM regime transitions occur.
#[allow(dead_code)]
pub fn compute_adaptive_tp_offset_bps(
    target_mu: f64,
    entry_price: f64,
    tick_size: f64,
    half_life_m: f64,
) -> Option<f64> {
    if target_mu <= 0.0 || entry_price <= 0.0 || tick_size <= 0.0 {
        return None;
    }
    let d_rem_bps = ((target_mu - entry_price).abs() / entry_price) * 1e4;
    let tick_bps = (tick_size / target_mu) * 1e4;
    let lower = (2.0 * tick_bps).max(2.0);
    let upper = 10.0_f64.min(0.15 * d_rem_bps);
    if lower > upper {
        return None;
    }
    let hl_clamped = half_life_m.clamp(15.0, 120.0);
    let eta_star = (0.04 + 0.08 * ((hl_clamped - 25.0).max(0.0) / 95.0)).clamp(0.04, 0.14);
    let target_shave_bps = (eta_star * d_rem_bps).max(5.0);
    Some(target_shave_bps.max(lower).min(upper))
}

/// Phase 2 Fee-Adjusted Fractional Kelly Tranche Scalar (\kappa_{Kelly} \in [0.65, 1.00]):
/// Modulates base tranche size according to the net payoff odds ratio after round-trip maker fees
/// and expected funding drag.
pub fn compute_fee_adjusted_kelly_scalar(
    gross_edge_bps: f64,
    min_net_edge_bps: f64,
    maker_fee_bps: f64,
    funding_drag_bps: f64,
) -> f64 {
    let cost_bps = 2.0 * maker_fee_bps + funding_drag_bps.max(0.0);
    let net_edge_bps = (gross_edge_bps - cost_bps).max(0.0);
    let ref_net_bps = (min_net_edge_bps * 1.15).max(15.0);
    (net_edge_bps / ref_net_bps).sqrt().clamp(0.65, 1.00)
}

#[derive(Debug, Clone, PartialEq)]
pub struct HourlySqueezeVerdict {
    pub approved: bool,
    pub side: Side,
    pub limit_px: f64,
    pub target_tp_px: f64,
    pub hard_stop_px: f64,
}

/// Phase 3 Candidate Engine C (`HourlySqueezeHarvester`):
/// Exploits Hyperliquid L1's deterministic hourly `:00` UTC peer-to-peer funding settlement cycle.
/// Enters Long during the Pre-Settlement Accumulation Window (`:42` to `:55` UTC) when 1h funding
/// reaches extreme negative levels (`<= -5.5 bps/hr` ~ `-48%` APR), Open Interest is non-contracting,
/// and price holds >= 0.25 * \sigma_{eq} above the 30-minute absorption low while trading at a discount
/// to the CEX spot oracle.
pub fn evaluate_engine_c_hourly_squeeze(
    timestamp_ms: u64,
    funding_1h: f64,
    close_px: f64,
    best_bid: f64,
    oracle_px: f64,
    low_30m: f64,
    sigma_eq_log: f64,
    min_net_edge_bps: f64,
    z_15: f64,
    z_60: f64,
    tick_size: f64,
) -> Option<HourlySqueezeVerdict> {
    if close_px <= 0.0 || oracle_px <= 0.0 || low_30m <= 0.0 || tick_size <= 0.0 {
        return None;
    }
    let minute_of_hour = (timestamp_ms / 60_000) % 60;
    if !(42..=55).contains(&minute_of_hour) {
        return None;
    }
    // Require extreme negative hourly funding (<= -5.5 bps/hr = -0.00055) and non-crash volatility
    if funding_1h > -0.00055 || z_15 <= -1.85 || z_60 <= -2.10 {
        return None;
    }
    // Require price absorption above 30m low by at least 0.25 * \sigma_{eq} and discount/parity to spot oracle
    let absorption_floor = low_30m * (1.0 + 0.25 * sigma_eq_log.max(0.0020));
    if close_px < absorption_floor || close_px > oracle_px * 1.0002 {
        return None;
    }
    let eff_bid = if best_bid > 0.0 {
        best_bid
    } else {
        close_px - tick_size
    };
    let min_tp = eff_bid * (1.0 + (min_net_edge_bps + 3.0) * 1e-4);
    let target_tp_px = oracle_px.max(min_tp);
    let hard_stop_px = (low_30m - 2.0 * tick_size).min(eff_bid * (1.0 - 0.0085));
    Some(HourlySqueezeVerdict {
        approved: true,
        side: Side::Long,
        limit_px: eff_bid,
        target_tp_px,
        hard_stop_px,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CascadeReversalVerdict {
    pub approved: bool,
    pub side: Side,
    pub limit_px: f64,
    pub target_tp_px: f64,
    pub hard_stop_px: f64,
}

/// Phase 3 Candidate Engine D (`CascadeAbsorptionReversal`):
/// Detects the exhaustion point of Hyperliquid's Stage-1 on-book market liquidation waterfall
/// as it transitions to Stage-2 off-book HLP vault absorption:
/// 1. Severe 15m downside shock (`z_15 <= -2.5`),
/// 2. Open Interest deleveraging contraction (`dlog_oi_3m <= -0.012`),
/// 3. Perp-to-Oracle discount dislocation (`(close - oracle) / oracle <= -0.0025`, i.e. <= -25 bps),
/// 4. Stoikov Micro-Price / L2 OBI reversal confirmation (`obi_instant >= 0.22` and bullish 1m close).
pub fn evaluate_engine_d_cascade_reversal(
    close_px: f64,
    open_px: f64,
    low_px: f64,
    best_bid: f64,
    oracle_px: f64,
    dlog_oi_3m: f64,
    z_15: f64,
    obi_instant: f64,
    min_net_edge_bps: f64,
    tick_size: f64,
) -> Option<CascadeReversalVerdict> {
    if close_px <= 0.0 || oracle_px <= 0.0 || low_px <= 0.0 || tick_size <= 0.0 {
        return None;
    }
    let oracle_dislocation = (close_px - oracle_px) / oracle_px;
    if z_15 <= -2.50
        && dlog_oi_3m <= -0.012
        && oracle_dislocation <= -0.0025
        && obi_instant >= 0.22
        && close_px > open_px
    {
        let eff_bid = if best_bid > 0.0 {
            best_bid
        } else {
            close_px - tick_size
        };
        let min_tp = eff_bid * (1.0 + (min_net_edge_bps + 3.0) * 1e-4);
        let target_tp_px = oracle_px.max(min_tp);
        let hard_stop_px = (low_px - 3.0 * tick_size).min(eff_bid * (1.0 - 0.0075));
        return Some(CascadeReversalVerdict {
            approved: true,
            side: Side::Long,
            limit_px: eff_bid,
            target_tp_px,
            hard_stop_px,
        });
    }
    None
}

// ============================================================================
// 3. TWO-TIER POSITION INVALIDATION LOGIC (SOFT MULTI-BAR + HARD CRASH STOP)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InvalidationDecision {
    HoldPosition { updated_tp_price: Option<f64> },
    TriggerSoftInvalidationFlatten { reason: &'static str },
    TriggerTimeDecayExit,
}

/// Manages active Engine A positions using the Option 1B + 1C synthesis:
/// - Tier 1 (Soft Multi-Bar Invalidation): Exits only if adverse frozen Z_OU >= z_soft_limit
///   for K = 2 consecutive completed 5m bars, OR if |Z_15| >= z_shock_limit for 2 bars,
///   OR 1-bar Z_OU breach + active Kalman LLT waterfall slope / CUSUM alarm.
/// - Tier 2 (Hard Crash Stop): Exchange-native Stop-Market order resting at d_hard_bps.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TwoTierInvalidationManager {
    pub side: Side,
    pub entry_price: f64,
    pub hard_stop_bps: f64,
    pub z_ou_soft_threshold: f64,
    pub required_consecutive_5m_bars: u32,
    pub z15_shock_threshold: f64,
    pub half_life_minutes: f64,
    pub consecutive_ou_breach_bars: u32,
    pub consecutive_shock_bars: u32,
    pub elapsed_minutes: f64,
    pub breakeven_ratchet_armed: bool,
}

#[allow(dead_code)]
impl TwoTierInvalidationManager {
    pub fn new(
        side: Side,
        entry_price: f64,
        hard_stop_bps: f64,
        z_ou_soft_threshold: f64,
        z15_shock_threshold: f64,
        half_life_minutes: f64,
    ) -> Self {
        Self {
            side,
            entry_price,
            hard_stop_bps,
            z_ou_soft_threshold,
            required_consecutive_5m_bars: 2,
            z15_shock_threshold,
            half_life_minutes,
            consecutive_ou_breach_bars: 0,
            consecutive_shock_bars: 0,
            elapsed_minutes: 0.0,
            breakeven_ratchet_armed: false,
        }
    }

    /// Computes the exact exchange-native Stop-Market trigger price to submit at entry.
    pub fn exchange_hard_stop_price(&self, tick_size: f64) -> f64 {
        if tick_size <= 0.0 {
            return self.entry_price;
        }
        match self.side {
            Side::Long => {
                let raw = self.entry_price * (1.0 - self.hard_stop_bps * 1e-4);
                (raw / tick_size).floor() * tick_size
            }
            Side::Short => {
                let raw = self.entry_price * (1.0 + self.hard_stop_bps * 1e-4);
                (raw / tick_size).ceil() * tick_size
            }
        }
    }

    /// Evaluates Tier 1 Soft Multi-Bar Invalidation on each completed 5-minute bar.
    pub fn on_5m_bar_close(
        &mut self,
        current_z_ou: f64,
        z_15_mad_shock: f64,
        kalman_drift_veto_against_position: bool,
        maker_fee_bps: f64,
        tick_size: f64,
    ) -> InvalidationDecision {
        self.elapsed_minutes += 5.0;

        // 1. Check adverse O-U Z-score breach on completed 5m bar (adverse direction only)
        let ou_breached = match self.side {
            Side::Long => current_z_ou <= -self.z_ou_soft_threshold,
            Side::Short => current_z_ou >= self.z_ou_soft_threshold,
        };

        if ou_breached {
            self.consecutive_ou_breach_bars += 1;
        } else {
            self.consecutive_ou_breach_bars = 0;
        }

        if self.consecutive_ou_breach_bars >= self.required_consecutive_5m_bars {
            return InvalidationDecision::TriggerSoftInvalidationFlatten {
                reason: "K=2 Consecutive 5m Bars |Z_OU| >= Soft Structural Break Boundary",
            };
        }

        // 2. Check persistent 15m MAD standardized shock sentinel (Z_15)
        let shock_against_pos = match self.side {
            Side::Long => z_15_mad_shock <= -self.z15_shock_threshold,
            Side::Short => z_15_mad_shock >= self.z15_shock_threshold,
        };

        if shock_against_pos {
            self.consecutive_shock_bars += 1;
        } else {
            self.consecutive_shock_bars = 0;
        }

        if self.consecutive_shock_bars >= 2 {
            return InvalidationDecision::TriggerSoftInvalidationFlatten {
                reason: "Persistent 2-Bar MAD Shock Sentinel |Z_15| Breach",
            };
        }

        // 3. Check combined single-bar O-U breach + active Kalman LLT waterfall slope veto
        if self.consecutive_ou_breach_bars >= 1 && kalman_drift_veto_against_position {
            return InvalidationDecision::TriggerSoftInvalidationFlatten {
                reason: "5m |Z_OU| Breach Confirmed by Kalman LLT Waterfall Slope Veto",
            };
        }

        // 4. Time-Decay Ratchet: tighten Maker TP to a +12 bps net harvest floor at 2.4 * t_{1/2} (clamped to [75m, 240m]),
        //    and force flatten at min(3.5 * t_{1/2}, 360.0) (floored at 90m) if mean reversion has stalled.
        let max_hold_mins = (3.5 * self.half_life_minutes).clamp(90.0, 360.0);
        if self.elapsed_minutes >= max_hold_mins {
            return InvalidationDecision::TriggerTimeDecayExit;
        }

        let mut updated_tp_price = None;
        let breakeven_ratchet_mins = (1.15 * self.half_life_minutes).clamp(38.0, 90.0);
        if self.elapsed_minutes >= breakeven_ratchet_mins {
            self.breakeven_ratchet_armed = true;
            let min_harvest_margin = (2.0 * maker_fee_bps + 12.0) * 1e-4;
            let be_px = match self.side {
                Side::Long => {
                    let raw = self.entry_price * (1.0 + min_harvest_margin);
                    (raw / tick_size.max(1e-9)).ceil() * tick_size.max(1e-9)
                }
                Side::Short => {
                    let raw = self.entry_price * (1.0 - min_harvest_margin);
                    (raw / tick_size.max(1e-9)).floor() * tick_size.max(1e-9)
                }
            };
            updated_tp_price = Some(be_px);
        }

        InvalidationDecision::HoldPosition { updated_tp_price }
    }
}

// ============================================================================
// 4. O-U PARAMETER ESTIMATION (5M KALMAN LLT RESIDUAL + PHILLIPS-YU JACKKNIFE)
// ============================================================================

/// Represents the mathematical parameters of an Ornstein-Uhlenbeck stochastic process.
#[derive(Debug, Clone)]
pub struct OuParameters {
    pub theta: f64, // Mean-reversion speed per minute
    pub mu: f64,    // Equilibrium mean in price space
    pub sigma: f64, // Volatility in price space
}

impl OuParameters {
    pub fn half_life_minutes(&self) -> f64 {
        if self.theta > 1e-6 {
            (2.0_f64.ln()) / self.theta
        } else {
            999.0
        }
    }
}

/// Represents the mathematically optimal entry and exit thresholds.
#[derive(Debug, Clone)]
pub struct OptimalThresholds {
    pub entry_long: f64,
    pub exit_long: f64,
    pub entry_short: f64,
    pub exit_short: f64,
}

/// Estimates raw AR(1) regression coefficients (a, b, sigma_eps) over a dataset.
fn estimate_ar1_raw(data: &[f64]) -> Option<(f64, f64, f64)> {
    if data.len() < 4 {
        return None;
    }
    let n = data.len() - 1;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;

    for i in 0..n {
        sum_x += data[i];
        sum_y += data[i + 1];
    }

    let nf = n as f64;
    let mean_x = sum_x / nf;
    let mean_y = sum_y / nf;

    let mut sum_dx_dy = 0.0;
    let mut sum_dx_sq = 0.0;
    for i in 0..n {
        let dx = data[i] - mean_x;
        let dy = data[i + 1] - mean_y;
        sum_dx_dy += dx * dy;
        sum_dx_sq += dx * dx;
    }
    let cov_xy = sum_dx_dy / nf;
    let var_x = sum_dx_sq / nf;

    if var_x <= 1e-18 {
        return None;
    }

    let b = cov_xy / var_x;
    let a = mean_y - b * mean_x;

    if b <= 0.0 || b >= 1.0 {
        return None;
    }

    let mut sum_err_sq = 0.0;
    for i in 0..n {
        let x = data[i];
        let y = data[i + 1];
        let err = y - (a + b * x);
        sum_err_sq += err * err;
    }

    let var_err = sum_err_sq / (nf - 2.0).max(1.0);
    let sigma_eps = var_err.sqrt();

    Some((a, b, sigma_eps))
}

/// Estimates the Ornstein-Uhlenbeck parameters using Ordinary Least Squares (OLS)
/// applied to an Autoregressive AR(1) discretization with Phillips & Yu (2005)
/// non-overlapping 2-split Jackknife bias reduction.
pub fn estimate_ou_parameters(data: &[f64], dt: f64) -> Option<OuParameters> {
    if data.iter().any(|&p| !p.is_finite() || p <= 0.0) {
        return None;
    }

    let log_data: Vec<f64> = data.iter().map(|&p| p.ln()).collect();
    let (a_log, b_log, sigma_eps_log) = estimate_ar1_raw(&log_data)?;
    let theta_full = -b_log.ln() / dt;

    let mid = log_data.len() / 2;
    let theta = if mid >= 4 {
        let part1 = estimate_ar1_raw(&log_data[..mid]);
        let part2 = estimate_ar1_raw(&log_data[mid..]);

        if let (Some((_, b1, _)), Some((_, b2, _))) = (part1, part2) {
            let theta1 = -b1.ln() / dt;
            let theta2 = -b2.ln() / dt;
            let th_jack = 2.0 * theta_full - 0.5 * (theta1 + theta2);
            if th_jack > 1e-5 {
                th_jack
            } else {
                return None;
            }
        } else {
            return None;
        }
    } else {
        theta_full
    };

    let mu_log = a_log / (1.0 - b_log);
    let mu = mu_log.exp();
    if !mu.is_finite() || mu <= 0.0 {
        return None;
    }

    let b_jack = (-theta * dt).exp();
    let denom_sq = (1.0 - b_jack * b_jack).max(1e-12);
    let sigma_log = sigma_eps_log * ((2.0 * theta) / denom_sq).sqrt();
    let sigma = mu * sigma_log;

    Some(OuParameters { theta, mu, sigma })
}

/// Estimates 5-minute O-U parameters on causal log-price series using both:
/// 1. Direct 5-minute log-price non-overlapping Phillips-Yu Jackknife AR(1), and
/// 2. 2-State Causal Kalman Local Linear Trend (LLT) Filter (`g_t, b_t`) for waterfall drift-velocity
///    gating and innovation CUSUM tracking (`O-U Mean Reversion Audit.md` Section 2 & 4).
pub fn estimate_ou_5m_kalman(
    closes_5m: &[f64],
    cfg: &AssetOuConfig,
) -> (Option<OuParameters>, f64, LltOutput) {
    let default_llt = LltOutput {
        causal_residual: 0.0,
        predicted_level: 0.0,
        updated_level: 0.0,
        updated_slope: 0.0,
        standardized_slope: 0.0,
        drift_veto_active: false,
        std_innovation: 0.0,
        cusum_pos: 0.0,
        cusum_neg: 0.0,
    };
    if closes_5m.len() < 20 || closes_5m.iter().any(|&p| !p.is_finite() || p <= 0.0) {
        return (None, cfg.min_sigma_eq_bps * 1e-4, default_llt);
    }

    // Run 2-state Causal Kalman LLT Filters across the 5-minute log-price history:
    // - `llt_fast` (q_g = 0.25, q_b = 0.0625) tracks fast local waterfall slope velocity and innovation CUSUM.
    // - `llt_macro` (q_g = 0.06, q_b = 0.005) tracks the slow macro equilibrium level g_t so that
    //   intraday O-U oscillations (15m-150m half-life) remain intact in y_t = ln(P_t) - g_{t|t-1}.
    let mut llt_fast = KalmanLltFilter::new(0.0020, 0.25, 0.0625);
    let mut llt_macro = KalmanLltFilter::new(0.0020, 0.06, 0.005);
    let mut residuals = Vec::with_capacity(closes_5m.len());
    let mut last_llt = default_llt;
    let mut last_macro_level = 0.0;
    for &px in closes_5m {
        let lp = px.ln();
        last_llt = llt_fast.update(lp, 0.45);
        let macro_out = llt_macro.update(lp, 0.45);
        last_macro_level = macro_out.updated_level;
        residuals.push(macro_out.causal_residual);
    }

    let n_5m = closes_5m.len();
    let fallback_w = ((cfg.lookback_5m_bars * 2) / 3).clamp(64, 72);
    let candidate_windows = [cfg.lookback_5m_bars.min(n_5m), fallback_w.min(n_5m)];
    let sigma_eq_floor_log = cfg.min_sigma_eq_bps * 1e-4;
    let dt = 5.0;

    let mut ou_opt: Option<(OuParameters, f64)> = None;

    // 1. Primary: Fit 5m O-U on the Causal Kalman LLT residual y_t = ln(P_t) - g_{t|t-1}
    //    (requires valid half-life inside [min_half_life_m, max_half_life_m] and no active waterfall veto).
    if !last_llt.drift_veto_active {
        for &w in &candidate_windows {
            if w < 24 {
                continue;
            }
            let res_slice = &residuals[n_5m - w..];
            if let Some((_a_r, b_r, sig_eps_r)) = estimate_ar1_raw(res_slice) {
                let theta_full = -b_r.ln() / dt;
                let mid = res_slice.len() / 2;
                let part1 = estimate_ar1_raw(&res_slice[..mid]);
                let part2 = estimate_ar1_raw(&res_slice[mid..]);
                if let (Some((_, b1, _)), Some((_, b2, _))) = (part1, part2) {
                    let th1 = -b1.ln() / dt;
                    let th2 = -b2.ln() / dt;
                    let th_jack = 2.0 * theta_full - 0.5 * (th1 + th2);
                    if th_jack > 1e-5 {
                        let hl = 2.0_f64.ln() / th_jack;
                        if hl >= cfg.min_half_life_m && hl <= cfg.max_half_life_m {
                            let b_jack = (-th_jack * dt).exp();
                            let denom_sq = (1.0 - b_jack * b_jack).max(1e-12);
                            let sigma_eq_raw = sig_eps_r / denom_sq.sqrt();
                            let sigma_eq_eff = sigma_eq_raw.max(sigma_eq_floor_log);
                            let mu_px = last_macro_level.exp();
                            if mu_px.is_finite() && mu_px > 0.0 {
                                let sigma_px = mu_px * sigma_eq_eff * (2.0 * th_jack).sqrt();
                                ou_opt = Some((
                                    OuParameters {
                                        theta: th_jack,
                                        mu: mu_px,
                                        sigma: sigma_px,
                                    },
                                    sigma_eq_eff,
                                ));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Fallback: Direct 5-minute log-price non-overlapping Phillips-Yu Jackknife O-U
    //    (only accepted if its half-life also lies within [min_half_life_m, max_half_life_m]).
    if ou_opt.is_none() {
        for &w in &candidate_windows {
            if w < 24 {
                continue;
            }
            let slice_5m = &closes_5m[n_5m - w..];
            if let Some(mut params) = estimate_ou_parameters(slice_5m, dt) {
                let hl = params.half_life_minutes();
                if hl >= cfg.min_half_life_m && hl <= cfg.max_half_life_m {
                    let sigma_eq_raw_log = ((params.sigma / params.mu.max(1e-9)).powi(2)
                        / (2.0 * params.theta))
                        .sqrt();
                    let sigma_eq_eff_log = sigma_eq_raw_log.max(sigma_eq_floor_log);
                    params.sigma = params.mu * sigma_eq_eff_log * (2.0 * params.theta).sqrt();
                    ou_opt = Some((params, sigma_eq_eff_log));
                    break;
                }
            }
        }
    }

    match ou_opt {
        Some((p, sig_eq)) => (Some(p), sig_eq, last_llt),
        None => (None, cfg.min_sigma_eq_bps * 1e-4, last_llt),
    }
}

#[allow(dead_code)]
pub fn calculate_thresholds_at_sigma(
    params: &OuParameters,
    transaction_cost: f64,
    num_sigma: f64,
) -> OptimalThresholds {
    let sigma_eq = (params.sigma * params.sigma / (2.0 * params.theta)).sqrt();
    let min_required_profit = transaction_cost * 2.5;
    let deviation = (num_sigma * sigma_eq).max(min_required_profit);
    OptimalThresholds {
        entry_long: params.mu - deviation,
        exit_long: params.mu,
        entry_short: params.mu + deviation,
        exit_short: params.mu,
    }
}

#[allow(dead_code)]
pub fn calculate_optimal_thresholds(
    params: &OuParameters,
    transaction_cost: f64,
) -> OptimalThresholds {
    calculate_thresholds_at_sigma(params, transaction_cost, 1.65)
}

/// Calculates asymmetric carry-shifted Bertram O-U entry/exit thresholds per Section 4 of
/// `O-U Mean Reversion Audit.md` (\gamma_c = 0.50, \Delta Z_c clamped to [-0.30\sigma, +0.30\sigma]):
pub fn calculate_carry_adjusted_thresholds(
    params: &OuParameters,
    transaction_cost: f64,
    base_sigma: f64,
    hourly_funding_rate: f64,
    min_sigma: f64,
    max_sigma: f64,
) -> OptimalThresholds {
    let sigma_eq = (params.sigma * params.sigma / (2.0 * params.theta)).sqrt();
    let min_required_profit = transaction_cost * 2.5;

    let (sigma_long, sigma_short) = if hourly_funding_rate.is_finite()
        && hourly_funding_rate.abs() > 1e-7
        && params.mu > 0.0
        && params.theta > 1e-5
    {
        let sigma_stat_log = (sigma_eq / params.mu).max(1e-6);
        let kappa_1h = params.theta * 60.0;
        let denom = (kappa_1h * sigma_stat_log).max(1e-8);
        // Long carry is -funding_1h; Short carry is +funding_1h
        // Per Section 4 Table (`O-U Mean Reversion Audit.md` line 146):
        // \gamma_c = 0.50, with total carry shift \gamma_c * \Delta Z_c clamped to [-0.30, +0.30].
        let shift_long = (-0.50 * hourly_funding_rate / denom).clamp(-0.30, 0.30);
        let shift_short = (0.50 * hourly_funding_rate / denom).clamp(-0.30, 0.30);
        let s_long = (base_sigma - shift_long).clamp(min_sigma, max_sigma);
        let s_short = (base_sigma - shift_short).clamp(min_sigma, max_sigma);
        (s_long, s_short)
    } else {
        (base_sigma, base_sigma)
    };

    let dev_long = (sigma_long * sigma_eq).max(min_required_profit);
    let dev_short = (sigma_short * sigma_eq).max(min_required_profit);

    OptimalThresholds {
        entry_long: params.mu - dev_long,
        exit_long: params.mu,
        entry_short: params.mu + dev_short,
        exit_short: params.mu,
    }
}

/// Returns the UTC session-conditioned O-U sigma offset:
pub fn utc_session_sigma_offset(utc_hour: u32, is_weekend: bool) -> f64 {
    if !is_weekend && (13..15).contains(&utc_hour) {
        0.00
    } else if is_weekend || utc_hour < 8 {
        -0.05
    } else {
        -0.05
    }
}

/// Piecewise-deterministic exogenous macro hazard state \lambda_{exo}(t) \in [0.0, 1.0].
#[derive(Debug, Clone, Copy, Default)]
pub struct MacroHazardState {
    pub lambda_exo: f64,
    pub is_hard_blackout: bool,
    pub event_name: &'static str,
}

const FOMC_DECISION_DATES: &[(i32, u32, u32)] = &[
    (2025, 1, 29),
    (2025, 3, 19),
    (2025, 5, 7),
    (2025, 6, 18),
    (2025, 7, 30),
    (2025, 9, 17),
    (2025, 10, 29),
    (2025, 12, 10),
    (2026, 1, 28),
    (2026, 3, 18),
    (2026, 4, 29),
    (2026, 6, 17),
    (2026, 7, 29),
    (2026, 9, 16),
    (2026, 11, 4),
    (2026, 12, 9),
    (2027, 1, 27),
    (2027, 3, 17),
    (2027, 4, 28),
    (2027, 6, 9),
    (2027, 7, 28),
    (2027, 9, 15),
    (2027, 10, 27),
    (2027, 12, 8),
];

const US_CPI_RELEASE_DATES: &[(i32, u32, u32)] = &[
    (2025, 1, 15),
    (2025, 2, 12),
    (2025, 3, 12),
    (2025, 4, 10),
    (2025, 5, 13),
    (2025, 6, 11),
    (2025, 7, 15),
    (2025, 8, 12),
    (2025, 9, 11),
    (2025, 10, 15),
    (2025, 11, 13),
    (2025, 12, 10),
    (2026, 1, 13),
    (2026, 2, 11),
    (2026, 3, 11),
    (2026, 4, 14),
    (2026, 5, 12),
    (2026, 6, 10),
    (2026, 7, 14),
    (2026, 8, 12),
    (2026, 9, 11),
    (2026, 10, 14),
    (2026, 11, 12),
    (2026, 12, 10),
    (2027, 1, 13),
    (2027, 2, 10),
    (2027, 3, 10),
    (2027, 4, 13),
    (2027, 5, 12),
    (2027, 6, 10),
    (2027, 7, 14),
    (2027, 8, 11),
    (2027, 9, 14),
    (2027, 10, 13),
    (2027, 11, 10),
    (2027, 12, 14),
];

/// Returns true if a given UTC calendar date falls within US Daylight Saving Time (EDT, UTC-4).
/// US DST begins on the second Sunday of March and ends on the first Sunday of November.
pub fn is_us_daylight_saving_date(year: i32, month: u32, day: u32) -> bool {
    use chrono::{Datelike, NaiveDate};
    if (4..=10).contains(&month) {
        return true;
    }
    if !(3..=11).contains(&month) {
        return false;
    }
    if month == 3 {
        if let Some(mar1) = NaiveDate::from_ymd_opt(year, 3, 1) {
            let wd = mar1.weekday().num_days_from_sunday();
            let second_sunday = 1 + ((7 - wd) % 7) + 7;
            return day >= second_sunday;
        }
    } else if month == 11 {
        if let Some(nov1) = NaiveDate::from_ymd_opt(year, 11, 1) {
            let wd = nov1.weekday().num_days_from_sunday();
            let first_sunday = 1 + ((7 - wd) % 7);
            return day < first_sunday;
        }
    }
    false
}

/// Returns the NYSE / US Equity Cash Open bell minute of the UTC day:
/// 810 (13:30 UTC) during US Daylight Saving Time (EDT), or 870 (14:30 UTC) during Standard Time (EST).
pub fn us_equity_bell_minute_utc(timestamp_ms: u64) -> u64 {
    use chrono::{Datelike, TimeZone, Utc};
    if let chrono::LocalResult::Single(dt) = Utc.timestamp_millis_opt(timestamp_ms as i64) {
        if is_us_daylight_saving_date(dt.year(), dt.month(), dt.day()) {
            810
        } else {
            870
        }
    } else {
        870
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Evaluates the Piecewise-Deterministic Exogenous Macro Hazard Intensity \lambda_{exo}(t) \in [0.0, 1.0]
/// across scheduled Tier-1 US macroeconomic releases (FOMC, CPI, NFP, Core PCE).
/// Zero network dependencies and 100% bit-for-bit identical in `src/backtest.rs` and `src/main.rs`.
pub fn evaluate_macro_hazard(ts_ms: u64) -> MacroHazardState {
    use chrono::{Datelike, NaiveDate, TimeZone, Utc, Weekday};
    let dt = match Utc.timestamp_millis_opt(ts_ms as i64).single() {
        Some(d) => d,
        None => return MacroHazardState::default(),
    };
    let year = dt.year();
    let month = dt.month();
    let day = dt.day();
    let weekday = dt.weekday();
    let is_edt = is_us_daylight_saving_date(year, month, day);

    let mut best = MacroHazardState::default();

    let eval_kernel = |release_ts_ms: i64,
                       hold_mins: f64,
                       half_life_mins: f64,
                       event_name: &'static str,
                       best: &mut MacroHazardState| {
        let diff_mins = (ts_ms as i64 - release_ts_ms) as f64 / 60_000.0;
        let lambda = if (-30.0..-15.0).contains(&diff_mins) {
            0.60 * ((diff_mins + 30.0) / 15.0)
        } else if (-15.0..=hold_mins).contains(&diff_mins) {
            1.0
        } else if diff_mins > hold_mins && diff_mins <= hold_mins + 5.0 * half_life_mins {
            let raw = (-std::f64::consts::LN_2 * (diff_mins - hold_mins) / half_life_mins).exp();
            if raw >= 0.05 {
                raw
            } else {
                0.0
            }
        } else {
            0.0
        };
        if lambda > best.lambda_exo {
            best.lambda_exo = lambda;
            best.is_hard_blackout = lambda >= 0.60;
            best.event_name = event_name;
        }
    };

    if let Some(base_date) = NaiveDate::from_ymd_opt(year, month, day) {
        // 1. Check FOMC (14:00 America/New_York = 18:00 UTC in EDT, 19:00 UTC in EST)
        if FOMC_DECISION_DATES
            .binary_search(&(year, month, day))
            .is_ok()
        {
            let fomc_utc_hour = if is_edt { 18 } else { 19 };
            if let Some(rel_dt) = base_date.and_hms_opt(fomc_utc_hour, 0, 0) {
                let rel_ms = Utc.from_utc_datetime(&rel_dt).timestamp_millis();
                eval_kernel(rel_ms, 60.0, 20.0, "US_FOMC_DECISION", &mut best);
            }
        }

        // 2. Check 08:30 America/New_York Tier-1 prints (12:30 UTC in EDT, 13:30 UTC in EST)
        let print_0830_utc_hour = if is_edt { 12 } else { 13 };
        if let Some(rel_dt) = base_date.and_hms_opt(print_0830_utc_hour, 30, 0) {
            let rel_ms = Utc.from_utc_datetime(&rel_dt).timestamp_millis();
            if US_CPI_RELEASE_DATES
                .binary_search(&(year, month, day))
                .is_ok()
            {
                eval_kernel(rel_ms, 15.0, 15.0, "US_CPI_PRINT", &mut best);
            } else if weekday == Weekday::Fri && day <= 7 {
                eval_kernel(rel_ms, 15.0, 15.0, "US_NFP_PAYROLLS", &mut best);
            } else if weekday == Weekday::Fri && (day + 7) > days_in_month(year, month) {
                eval_kernel(rel_ms, 15.0, 15.0, "US_CORE_PCE", &mut best);
            }
        }
    }

    best
}

/// Returns a descriptive veto string if `ts_ms` falls inside a Tier-1 US Macro Hard Blackout window (\lambda_{exo} >= 0.60).
pub fn check_macro_calendar_veto(ts_ms: u64) -> Option<String> {
    let h = evaluate_macro_hazard(ts_ms);
    if h.is_hard_blackout {
        Some(format!(
            "Tier-1 US Macro Shield ({}, lambda={:.2})",
            h.event_name, h.lambda_exo
        ))
    } else {
        None
    }
}

/// Computes the robust Median Absolute Deviation (MAD) of a return series,
/// scaled by 1.4826 for normal consistency and floored at 1e-5.
pub fn calculate_mad_sigma(returns: &[f64]) -> f64 {
    if returns.is_empty() {
        return 1e-5;
    }
    let mut sorted_r = returns.to_vec();
    sorted_r.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted_r.len() / 2;
    let med_r = if sorted_r.len() % 2 == 0 {
        (sorted_r[mid - 1] + sorted_r[mid]) / 2.0
    } else {
        sorted_r[mid]
    };
    let mut abs_devs: Vec<f64> = sorted_r.iter().map(|&r| (r - med_r).abs()).collect();
    abs_devs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med_dev = if abs_devs.len() % 2 == 0 {
        (abs_devs[mid - 1] + abs_devs[mid]) / 2.0
    } else {
        abs_devs[mid]
    };
    (1.4826 * med_dev).max(1e-5)
}

/// Computes the directional expected edge in basis points between entry and exit price:
pub fn calculate_directional_edge_bps(entry_price: f64, exit_target: f64, is_buy: bool) -> f64 {
    if entry_price <= 0.0 || exit_target <= 0.0 {
        return 0.0;
    }
    if is_buy {
        if exit_target > entry_price {
            (exit_target / entry_price).ln() * 10_000.0
        } else {
            0.0
        }
    } else if entry_price > exit_target {
        (entry_price / exit_target).ln() * 10_000.0
    } else {
        0.0
    }
}

/// Computes the net expected edge in basis points between entry and exit price (Long default):
#[allow(dead_code)]
pub fn calculate_net_edge_bps(entry_price: f64, exit_target: f64) -> f64 {
    calculate_directional_edge_bps(entry_price, exit_target, true)
}

#[allow(dead_code)]
pub fn calculate_rogers_satchell_sigma(
    opens: &[f64],
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
) -> f64 {
    let n = opens
        .len()
        .min(highs.len())
        .min(lows.len())
        .min(closes.len());
    if n < 2 {
        return 0.0;
    }
    let mut sum_rs = 0.0;
    let mut valid_count = 0;
    for i in 0..n {
        let o = opens[i];
        let h = highs[i];
        let l = lows[i];
        let c = closes[i];
        if o <= 0.0 || h <= 0.0 || l <= 0.0 || c <= 0.0 || h < l {
            continue;
        }
        let u = (h / c).ln();
        let d = (l / c).ln();
        let u_o = (h / o).ln();
        let d_o = (l / o).ln();
        sum_rs += u * u_o + d * d_o;
        valid_count += 1;
    }
    if valid_count < 2 {
        return 0.0;
    }
    (sum_rs / valid_count as f64).max(0.0).sqrt()
}

#[allow(dead_code)]
pub fn calculate_garman_klass_sigma(
    opens: &[f64],
    highs: &[f64],
    lows: &[f64],
    closes: &[f64],
) -> f64 {
    let n = opens
        .len()
        .min(highs.len())
        .min(lows.len())
        .min(closes.len());
    if n < 2 {
        return 0.0;
    }
    let ln2_term = 2.0 * 2.0_f64.ln() - 1.0;
    let mut sum_gk = 0.0;
    let mut valid_count = 0;
    for i in 0..n {
        let o = opens[i];
        let h = highs[i];
        let l = lows[i];
        let c = closes[i];
        if o <= 0.0 || h <= 0.0 || l <= 0.0 || c <= 0.0 || h < l {
            continue;
        }
        let hl = (h / l).ln();
        let co = (c / o).ln();
        sum_gk += 0.5 * hl * hl - ln2_term * co * co;
        valid_count += 1;
    }
    if valid_count < 2 {
        return 0.0;
    }
    (sum_gk / valid_count as f64).max(0.0).sqrt()
}

/// Feature snapshot capturing 5m Causal Kalman LLT O-U, drift velocity, and standardized Z-scores.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FeatureSnapshot {
    pub thresholds_opt: Option<OptimalThresholds>,
    pub thresholds_175_opt: Option<OptimalThresholds>,
    pub total_drift_pct: f64,
    pub is_drifting: bool,
    pub half_life_m: f64,
    pub z_15: f64,
    pub z_60: f64,
    #[allow(dead_code)]
    pub ret_15: f64,
    #[allow(dead_code)]
    pub ret_60: f64,
    pub is_downside_shock: bool,
    pub is_upside_shock: bool,
    pub hist_last_price: f64,
    pub is_stationary: bool,
    pub is_consolidation: bool,
    pub is_valid_half_life: bool,
    /// Current 5m O-U Z-score relative to equilibrium mean: ln(eval_close / mu) / sigma_eq_log
    pub z_ou: f64,
    /// Effective 5m stationary standard deviation in log-return units (floored at asset min_sigma_eq_bps)
    pub sigma_eq_log: f64,
    /// Lagged 1-minute diffusive volatility (robust MAD/std) for reversal confirmation hurdle b_t
    pub sigma_1m_prev: f64,
    /// Kalman LLT standardized slope velocity |b_{t|t}| / sigma_{v,t}
    pub kalman_std_slope: f64,
    /// Kalman LLT local drift slope sign (per 5m bar)
    pub kalman_slope: f64,
    /// True if Kalman LLT standardized slope > 0.45 (waterfall drift veto)
    pub kalman_drift_veto: bool,
    /// One-step standardized innovation CUSUMs (upside and downside)
    pub cusum_pos: f64,
    pub cusum_neg: f64,
    /// Rolling 60m Variance-Ratio Hurst Exponent H_t in [0.15, 0.85]
    pub hurst_60m: f64,
    /// 30-bar Kaufman Efficiency Ratio in [0.0, 1.0]
    pub ker_30m: f64,
    /// Composite Micro-Regime Index \Phi_{regime}(t) in [0.0, 1.0]
    pub phi_regime: f64,
    /// Multi-horizon macro drift score Z_{macro}(t)
    pub z_macro: f64,
    /// Dynamic regime-adaptive entry threshold for Longs (in \sigma_{eq} units)
    pub dynamic_sigma_long: f64,
    /// Dynamic regime-adaptive entry threshold for Shorts (in \sigma_{eq} units)
    pub dynamic_sigma_short: f64,
    /// Continuous directional bias multiplier \lambda_{dir} for Longs in [0.0, 1.0]
    pub lambda_dir_long: f64,
    /// Continuous directional bias multiplier \lambda_{dir} for Shorts in [0.0, 1.0]
    pub lambda_dir_short: f64,
    /// Causal Kalman LLT equilibrium log-price level g_{t|t}
    pub kalman_level_log: f64,
    /// Standardized 1-step Kalman innovation \tilde{\epsilon}_t
    pub kalman_std_innov: f64,
    /// Bias-corrected O-U mean-reversion rate \theta (per minute)
    pub ou_theta: f64,
}

#[allow(dead_code)]
pub fn calculate_features(
    closes_history: &[f64],
    eval_close: f64,
    symbol: &str,
) -> Option<FeatureSnapshot> {
    calculate_features_with_funding(closes_history, eval_close, symbol, 0.0)
}

/// Evaluates stochastic 5m O-U (Option 1B + 1C synthesis), Kalman LLT drift velocity,
/// and 1m Z-score features incorporating Hyperliquid hourly funding rate carry drift.
pub fn calculate_features_with_funding(
    closes_history: &[f64],
    eval_close: f64,
    symbol: &str,
    hourly_funding_rate: f64,
) -> Option<FeatureSnapshot> {
    calculate_features_with_funding_at(
        closes_history,
        eval_close,
        symbol,
        hourly_funding_rate,
        None,
    )
}

/// Evaluates stochastic 5m O-U features at a specific bar timestamp (`eval_timestamp_ms`)
/// for deterministic discrete-event backtesting and live bar evaluation.
pub fn calculate_features_with_funding_at(
    closes_history: &[f64],
    eval_close: f64,
    symbol: &str,
    hourly_funding_rate: f64,
    eval_timestamp_ms: Option<u64>,
) -> Option<FeatureSnapshot> {
    if closes_history.len() < 100 || eval_close <= 0.0 {
        return None;
    }
    let hist_last_price = *closes_history.last().unwrap_or(&eval_close);
    let cfg = asset_ou_config(symbol);

    // Build causal 5-minute completed bar series from closes_history (stepping by 5 minutes anchored to end of history)
    let n_total = closes_history.len();
    let rem = n_total % 5;
    let start_idx = if rem == 0 { 4 } else { rem - 1 };
    let mut closes_5m: Vec<f64> = closes_history
        .iter()
        .skip(start_idx)
        .step_by(5)
        .copied()
        .collect();
    if closes_5m.len() < 20 {
        // If total length is between 100 and 104, ensure at least 20 5m bars
        closes_5m = closes_history.iter().step_by(5).copied().collect();
    }

    let (ou_5m_opt, sigma_eq_log, llt_out) = estimate_ou_5m_kalman(&closes_5m, &cfg);

    let (thresholds_opt, thresholds_175_opt, half_life_m, z_ou) =
        if let Some(mut ou_params) = ou_5m_opt {
            let hl = ou_params.half_life_minutes();
            let is_stationary_hl = hl > 0.0 && hl <= cfg.max_half_life_m;
            if hourly_funding_rate.is_finite()
                && hourly_funding_rate.abs() > 1e-7
                && ou_params.theta > 1e-5
            {
                let carry_shift =
                    (-hourly_funding_rate / (60.0 * ou_params.theta)).clamp(-0.0015, 0.0015);
                ou_params.mu *= 1.0 + carry_shift;
            }
            let mu_dev = (ou_params.mu - hist_last_price).abs() / hist_last_price;
            let is_mu_ok = mu_dev <= 0.07;

            let z_ou_curr = if ou_params.mu > 0.0 && sigma_eq_log > 1e-6 {
                (eval_close / ou_params.mu).ln() / sigma_eq_log
            } else {
                0.0
            };

            if is_stationary_hl && is_mu_ok {
                let tx_cost = hist_last_price * 0.0005;
                let session_offset = if hourly_funding_rate.abs() > 1e-9 {
                    use chrono::{Datelike, Timelike};
                    let ref_utc = eval_timestamp_ms
                        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms as i64))
                        .unwrap_or_else(chrono::Utc::now);
                    let is_weekend = matches!(
                        ref_utc.weekday(),
                        chrono::Weekday::Sat | chrono::Weekday::Sun
                    );
                    utc_session_sigma_offset(ref_utc.hour(), is_weekend)
                } else {
                    0.0
                };
                let macro_state = eval_timestamp_ms
                    .map(evaluate_macro_hazard)
                    .unwrap_or_default();
                let hazard_mult = if macro_state.is_hard_blackout {
                    1.0
                } else {
                    1.0 + 0.35 * macro_state.lambda_exo
                };
                let th = calculate_carry_adjusted_thresholds(
                    &ou_params,
                    tx_cost,
                    (cfg.base_entry_sigma + session_offset) * hazard_mult,
                    hourly_funding_rate,
                    cfg.min_entry_sigma,
                    cfg.max_entry_sigma * hazard_mult,
                );
                // Per both research audits, Tier 2 (1.25\sigma) premature entries are disabled in production;
                // `thresholds_175_opt` is kept populated for counterfactual telemetry compatibility.
                let th_125 = calculate_carry_adjusted_thresholds(
                    &ou_params,
                    tx_cost,
                    1.25 + session_offset * 0.5,
                    hourly_funding_rate,
                    1.15,
                    1.45,
                );
                (Some(th), Some(th_125), hl, z_ou_curr)
            } else {
                (None, None, hl, z_ou_curr)
            }
        } else {
            (None, None, 999.0, 0.0)
        };

    // Scale-Invariant Log-Price OLS Slope Drift Test with Student's t-statistic
    let valid_lps: Vec<(f64, f64)> = closes_history
        .iter()
        .enumerate()
        .filter(|(_, &p)| p > 0.0 && p.is_finite())
        .map(|(i, &p)| (i as f64, p.ln()))
        .collect();
    let n_drift = valid_lps.len() as f64;
    let mean_i = valid_lps.iter().map(|(i, _)| *i).sum::<f64>() / n_drift.max(1.0);
    let mean_lp = valid_lps.iter().map(|(_, lp)| *lp).sum::<f64>() / n_drift.max(1.0);

    let mut ss_xx = 0.0_f64;
    let mut ss_xy = 0.0_f64;
    for &(idx, lp) in &valid_lps {
        let di = idx - mean_i;
        let dlp = lp - mean_lp;
        ss_xx += di * di;
        ss_xy += di * dlp;
    }
    let (slope_log, intercept_log) = if ss_xx > 1e-12 {
        let slope = ss_xy / ss_xx;
        let intercept = mean_lp - slope * mean_i;
        (slope, intercept)
    } else {
        (0.0, mean_lp)
    };

    let mut sum_sq_residuals = 0.0_f64;
    for &(idx, lp) in &valid_lps {
        let y_hat = intercept_log + slope_log * idx;
        sum_sq_residuals += (lp - y_hat).powi(2);
    }
    let df = (n_drift - 2.0).max(1.0);
    let s_err_sq = sum_sq_residuals / df;
    let se_slope = (s_err_sq / ss_xx.max(1e-12)).sqrt();
    let t_stat_slope = if se_slope > 1e-12 {
        slope_log / se_slope
    } else {
        0.0
    };

    let total_drift_pct = slope_log * n_drift;
    let max_economic_drift = match symbol.to_uppercase().as_str() {
        "XYZ:GOLD" | "GOLD" => 0.014,
        "XYZ:XYZ100" | "XYZ100" => 0.018,
        "BTC" => 0.020,
        "XYZ:SILVER" | "SILVER" => 0.023,
        "ETH" => 0.025,
        "XYZ:NVDA" | "NVDA" => 0.030,
        "XRP" | "LINK" => 0.035,
        "XYZ:TSLA" | "TSLA" => 0.038,
        "SOL" | "HYPE" | "ZEC" => 0.040,
        "AVAX" | "SUI" => 0.050,
        "NEAR" => 0.055,
        _ => 0.025,
    };

    let is_drifting = t_stat_slope.abs() >= 2.58 && total_drift_pct.abs() > max_economic_drift;

    // Standardized Downside/Upside Shock Calculation with robust MAD
    let n = closes_history.len();
    let mut z_15 = 0.0;
    let mut z_60 = 0.0;
    let mut ret_15 = 0.0;
    let mut ret_60 = 0.0;
    let mut sigma_mad = 0.0007;
    if n >= 65 {
        let mut returns_1m = Vec::with_capacity(n - 1);
        for i in 1..n {
            returns_1m.push((closes_history[i] / closes_history[i - 1]).ln());
        }
        sigma_mad = calculate_mad_sigma(&returns_1m);

        ret_15 = (eval_close / closes_history[n - 15]).ln();
        z_15 = ret_15 / (sigma_mad * (15.0_f64).sqrt());

        ret_60 = (eval_close / closes_history[n - 60]).ln();
        z_60 = ret_60 / (sigma_mad * (60.0_f64).sqrt());
    }

    let (hurdle_15, hurdle_60) = match symbol.to_uppercase().as_str() {
        "XYZ:GOLD" | "GOLD" => (0.0045, 0.0085),
        "XYZ:XYZ100" | "XYZ100" => (0.0055, 0.0110),
        "XYZ:SILVER" | "SILVER" => (0.0075, 0.0145),
        "BTC" => (0.0075, 0.0150),
        "XYZ:NVDA" | "NVDA" => (0.0095, 0.0185),
        "ETH" => (0.0100, 0.0200),
        "XYZ:TSLA" | "TSLA" => (0.0120, 0.0230),
        "XRP" | "LINK" | "ZEC" => (0.0140, 0.0280),
        "SOL" | "HYPE" => (0.0150, 0.0300),
        "AVAX" | "SUI" => (0.0200, 0.0400),
        "NEAR" => (0.0220, 0.0440),
        _ => (0.0100, 0.0200),
    };

    let shock_15_down = ret_15 <= -hurdle_15 && z_15 <= -cfg.soft_z15_shock_threshold;
    let shock_15_up = ret_15 >= hurdle_15 && z_15 >= cfg.soft_z15_shock_threshold;
    let shock_60_down = ret_60 <= -hurdle_60 && z_60 <= -cfg.soft_z15_shock_threshold;
    let shock_60_up = ret_60 >= hurdle_60 && z_60 >= cfg.soft_z15_shock_threshold;

    let is_downside_shock = shock_15_down || shock_60_down;
    let is_upside_shock = shock_15_up || shock_60_up;
    let is_stationary = thresholds_opt.is_some();
    let is_consolidation = !is_drifting && is_stationary;
    let is_valid_half_life =
        half_life_m >= cfg.min_half_life_m && half_life_m <= cfg.max_half_life_m;

    let cal = asset_calibration(symbol);
    let hurst_60m = calculate_rolling_hurst_60m(closes_history);
    let ker_30m = calculate_ker_30m(closes_history);
    let z_macro = calculate_z_macro(closes_history, eval_close, sigma_mad, llt_out.updated_slope);
    let gk_eff_15m = if n >= 15 {
        (ret_15.abs() / (sigma_mad * (15.0_f64).sqrt()).max(1e-6)).clamp(0.0, 1.0)
    } else {
        0.35
    };
    let ou_theta = if half_life_m > 0.0 && half_life_m < 900.0 {
        std::f64::consts::LN_2 / half_life_m
    } else {
        0.0
    };
    let kalman_level_log = if llt_out.updated_level.abs() > 1e-6 {
        llt_out.updated_level
    } else {
        eval_close.ln()
    };

    let micro_regime = MicroRegimeState {
        hurst_60m,
        ker_30m,
        gk_efficiency_15m: gk_eff_15m,
        kalman_level: kalman_level_log,
        kalman_slope: llt_out.updated_slope / 5.0, // convert 5m slope to 1m slope
        sigma_1m: sigma_mad,
        sigma_eq: sigma_eq_log,
        z_macro,
    };
    let phi_regime = micro_regime.phi_regime();
    let dynamic_sigma_long = micro_regime.dynamic_entry_sigma(&cal, Direction::Long);
    let dynamic_sigma_short = micro_regime.dynamic_entry_sigma(&cal, Direction::Short);
    let lambda_dir_long = micro_regime.directional_bias_multiplier(&cal, Direction::Long);
    let lambda_dir_short = micro_regime.directional_bias_multiplier(&cal, Direction::Short);

    // Modulate thresholds_opt with the quadratic Dynamic Entry Frontier penalty \Phi_{regime}(t)^2
    // and enforce Horizon Scaling Law \tau_H^* & Minimum Gross Entry Floor \Delta_{floor, bps} (Section 4.2)
    let horizon_scale = if eval_timestamp_ms.is_some() && cal.ou_horizon_mins > 5 {
        (cal.ou_horizon_mins as f64 / 5.0).powf(hurst_60m.clamp(0.35, 0.55))
    } else {
        1.0
    };
    let eff_sigma_eq_log = sigma_eq_log * horizon_scale;
    let thresholds_opt = thresholds_opt.map(|mut th| {
        if phi_regime > 0.35 && eff_sigma_eq_log > 1e-6 {
            let extra_sigma = 0.30 * (phi_regime * phi_regime);
            th.entry_long *= (-extra_sigma * eff_sigma_eq_log).exp();
            th.entry_short *= (extra_sigma * eff_sigma_eq_log).exp();
        }
        if eval_timestamp_ms.is_some() {
            let min_floor_frac = cal.min_gross_entry_bps * 1e-4;
            th.entry_long = th.entry_long.min(th.exit_long * (1.0 - min_floor_frac));
            th.entry_short = th.entry_short.max(th.exit_short * (1.0 + min_floor_frac));
        }
        th
    });

    Some(FeatureSnapshot {
        thresholds_opt,
        thresholds_175_opt,
        total_drift_pct,
        is_drifting,
        half_life_m,
        z_15,
        z_60,
        ret_15,
        ret_60,
        is_downside_shock,
        is_upside_shock,
        hist_last_price,
        is_stationary,
        is_consolidation,
        is_valid_half_life,
        z_ou,
        sigma_eq_log: eff_sigma_eq_log,
        sigma_1m_prev: sigma_mad,
        kalman_std_slope: llt_out.standardized_slope,
        kalman_slope: llt_out.updated_slope,
        kalman_drift_veto: llt_out.drift_veto_active,
        cusum_pos: llt_out.cusum_pos,
        cusum_neg: llt_out.cusum_neg,
        hurst_60m,
        ker_30m,
        phi_regime,
        z_macro,
        dynamic_sigma_long,
        dynamic_sigma_short,
        lambda_dir_long,
        lambda_dir_short,
        kalman_level_log,
        kalman_std_innov: llt_out.std_innovation,
        ou_theta,
    })
}

// ============================================================================
// PHASE 1 & PHASE 2: O-U SCALE FUNCTION, DYNAMIC FRONTIER & SPRT MULTI-TRANCHE
// ============================================================================

/// Asset-specific quantitative calibration parameters from Section 5 of Follow-Up Research Reports.
#[derive(Debug, Clone, Copy)]
pub struct AssetCalibration {
    pub symbol: &'static str,
    pub sz_decimals: u32,
    pub ou_horizon_mins: usize,
    pub min_gross_entry_bps: f64,
    pub sigma_min: f64,
    pub sigma_max: f64,
    pub beta_overshoot: f64,
    pub w2_min: f64,
    pub w2_max: f64,
    pub stall_trigger_mins: usize,
    pub cusum_delta_1m: f64,
    pub cusum_b_1m_star: f64,
    pub sprt_min_loss_gate_bps: f64,
    pub touch_rebound_min: f64,
    pub squeeze_ratio_max: f64,
    pub sprt_delta: f64, // 5m drift parameter (scaled by 1/sqrt(5) for native 1m bars)
    pub sprt_b_upper: f64,
    pub z_disaster_offset: f64, // Offset beyond |entry_z| for intrabar High/Low catastrophe guard
    pub k_dir: f64,
    pub u_veto: f64,
    pub tau_obi_eng_a: f64,
    pub tau_obi_eng_b: f64,
    pub funding_trigger_bps: f64,
    pub funding_z24h_min: f64,
    pub oi_cascade_pct: f64,
    pub orb_min_range_bps: f64,
}

pub type HardenedAssetCalibration = AssetCalibration;

/// Returns the revised institutional calibration from Section 5 for any of the 9 portfolio perpetuals.
pub fn asset_calibration(symbol: &str) -> AssetCalibration {
    match symbol.to_uppercase().as_str() {
        "BTC" => AssetCalibration {
            symbol: "BTC",
            sz_decimals: 5,
            ou_horizon_mins: 15,
            min_gross_entry_bps: 26.0,
            sigma_min: 1.60,
            sigma_max: 2.20,
            beta_overshoot: 0.58,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 4,
            cusum_delta_1m: 0.34,
            cusum_b_1m_star: 4.10,
            sprt_min_loss_gate_bps: -18.0,
            touch_rebound_min: 0.32,
            squeeze_ratio_max: 0.92,
            sprt_delta: 0.85,
            sprt_b_upper: 3.25,
            z_disaster_offset: 1.85,
            k_dir: 1.60,
            u_veto: -2.30,
            tau_obi_eng_a: 0.28,
            tau_obi_eng_b: 0.25,
            funding_trigger_bps: 3.2,
            funding_z24h_min: 2.25,
            oi_cascade_pct: 1.0,
            orb_min_range_bps: 42.0,
        },
        "ETH" => AssetCalibration {
            symbol: "ETH",
            sz_decimals: 4,
            ou_horizon_mins: 10,
            min_gross_entry_bps: 24.0,
            sigma_min: 1.58,
            sigma_max: 2.20,
            beta_overshoot: 0.58,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 4,
            cusum_delta_1m: 0.35,
            cusum_b_1m_star: 4.00,
            sprt_min_loss_gate_bps: -18.0,
            touch_rebound_min: 0.34,
            squeeze_ratio_max: 0.92,
            sprt_delta: 0.85,
            sprt_b_upper: 3.20,
            z_disaster_offset: 1.85,
            k_dir: 1.65,
            u_veto: -2.25,
            tau_obi_eng_a: 0.30,
            tau_obi_eng_b: 0.25,
            funding_trigger_bps: 3.5,
            funding_z24h_min: 2.25,
            oi_cascade_pct: 1.1,
            orb_min_range_bps: 48.0,
        },
        "SOL" => AssetCalibration {
            symbol: "SOL",
            sz_decimals: 2,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 22.0,
            sigma_min: 1.55,
            sigma_max: 2.25,
            beta_overshoot: 0.62,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.36,
            cusum_b_1m_star: 3.85,
            sprt_min_loss_gate_bps: -19.0,
            touch_rebound_min: 0.35,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.80,
            sprt_b_upper: 3.10,
            z_disaster_offset: 1.90,
            k_dir: 1.75,
            u_veto: -2.15,
            tau_obi_eng_a: 0.32,
            tau_obi_eng_b: 0.28,
            funding_trigger_bps: 3.8,
            funding_z24h_min: 2.30,
            oi_cascade_pct: 1.2,
            orb_min_range_bps: 58.0,
        },
        "XRP" => AssetCalibration {
            symbol: "XRP",
            sz_decimals: 0,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 22.0,
            sigma_min: 1.58,
            sigma_max: 2.28,
            beta_overshoot: 0.55,
            w2_min: 0.35,
            w2_max: 0.62,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.36,
            cusum_b_1m_star: 3.85,
            sprt_min_loss_gate_bps: -19.0,
            touch_rebound_min: 0.35,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.85,
            sprt_b_upper: 3.05,
            z_disaster_offset: 1.90,
            k_dir: 1.80,
            u_veto: -2.10,
            tau_obi_eng_a: 0.35,
            tau_obi_eng_b: 0.30,
            funding_trigger_bps: 4.0,
            funding_z24h_min: 2.30,
            oi_cascade_pct: 1.2,
            orb_min_range_bps: 60.0,
        },
        "SUI" => AssetCalibration {
            symbol: "SUI",
            sz_decimals: 1,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 24.0,
            sigma_min: 1.60,
            sigma_max: 2.32,
            beta_overshoot: 0.65,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.35,
            cusum_b_1m_star: 3.80,
            sprt_min_loss_gate_bps: -20.0,
            touch_rebound_min: 0.36,
            squeeze_ratio_max: 0.88,
            sprt_delta: 0.78,
            sprt_b_upper: 2.95,
            z_disaster_offset: 1.95,
            k_dir: 1.90,
            u_veto: -2.00,
            tau_obi_eng_a: 0.35,
            tau_obi_eng_b: 0.32,
            funding_trigger_bps: 4.2,
            funding_z24h_min: 2.35,
            oi_cascade_pct: 1.3,
            orb_min_range_bps: 68.0,
        },
        "AVAX" => AssetCalibration {
            symbol: "AVAX",
            sz_decimals: 2,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 22.0,
            sigma_min: 1.58,
            sigma_max: 2.28,
            beta_overshoot: 0.58,
            w2_min: 0.35,
            w2_max: 0.62,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.36,
            cusum_b_1m_star: 3.85,
            sprt_min_loss_gate_bps: -19.0,
            touch_rebound_min: 0.35,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.82,
            sprt_b_upper: 3.00,
            z_disaster_offset: 1.90,
            k_dir: 1.85,
            u_veto: -2.05,
            tau_obi_eng_a: 0.35,
            tau_obi_eng_b: 0.30,
            funding_trigger_bps: 4.0,
            funding_z24h_min: 2.30,
            oi_cascade_pct: 1.2,
            orb_min_range_bps: 62.0,
        },
        "LINK" => AssetCalibration {
            symbol: "LINK",
            sz_decimals: 1,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 22.0,
            sigma_min: 1.58,
            sigma_max: 2.28,
            beta_overshoot: 0.55,
            w2_min: 0.35,
            w2_max: 0.62,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.36,
            cusum_b_1m_star: 3.85,
            sprt_min_loss_gate_bps: -19.0,
            touch_rebound_min: 0.35,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.82,
            sprt_b_upper: 3.00,
            z_disaster_offset: 1.90,
            k_dir: 1.85,
            u_veto: -2.05,
            tau_obi_eng_a: 0.35,
            tau_obi_eng_b: 0.30,
            funding_trigger_bps: 4.0,
            funding_z24h_min: 2.30,
            oi_cascade_pct: 1.2,
            orb_min_range_bps: 62.0,
        },
        "NEAR" => AssetCalibration {
            symbol: "NEAR",
            sz_decimals: 1,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 25.0,
            sigma_min: 1.64,
            sigma_max: 2.38,
            beta_overshoot: 0.62,
            w2_min: 0.35,
            w2_max: 0.60,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.33,
            cusum_b_1m_star: 3.75,
            sprt_min_loss_gate_bps: -20.0,
            touch_rebound_min: 0.38,
            squeeze_ratio_max: 0.88,
            sprt_delta: 0.72,
            sprt_b_upper: 2.85,
            z_disaster_offset: 2.00,
            k_dir: 2.05,
            u_veto: -1.85,
            tau_obi_eng_a: 0.40,
            tau_obi_eng_b: 0.35,
            funding_trigger_bps: 4.5,
            funding_z24h_min: 2.40,
            oi_cascade_pct: 1.4,
            orb_min_range_bps: 75.0,
        },
        "HYPE" => AssetCalibration {
            symbol: "HYPE",
            sz_decimals: 2,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 26.0,
            sigma_min: 1.66,
            sigma_max: 2.42,
            beta_overshoot: 0.68,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.32,
            cusum_b_1m_star: 3.75,
            sprt_min_loss_gate_bps: -21.0,
            touch_rebound_min: 0.38,
            squeeze_ratio_max: 0.88,
            sprt_delta: 0.70,
            sprt_b_upper: 2.80,
            z_disaster_offset: 2.05,
            k_dir: 2.10,
            u_veto: -1.80,
            tau_obi_eng_a: 0.42,
            tau_obi_eng_b: 0.35,
            funding_trigger_bps: 4.8,
            funding_z24h_min: 2.45,
            oi_cascade_pct: 1.5,
            orb_min_range_bps: 82.0,
        },
        "XYZ:NVDA" | "NVDA" => AssetCalibration {
            symbol: "xyz:NVDA",
            sz_decimals: 3,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 18.0,
            sigma_min: 1.55,
            sigma_max: 2.25,
            beta_overshoot: 0.55,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.35,
            cusum_b_1m_star: 3.85,
            sprt_min_loss_gate_bps: -18.0,
            touch_rebound_min: 0.35,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.82,
            sprt_b_upper: 3.10,
            z_disaster_offset: 1.80,
            k_dir: 1.75,
            u_veto: -2.00,
            tau_obi_eng_a: 0.30,
            tau_obi_eng_b: 0.25,
            funding_trigger_bps: 3.0,
            funding_z24h_min: 2.20,
            oi_cascade_pct: 1.0,
            orb_min_range_bps: 40.0,
        },
        "XYZ:GOLD" | "GOLD" => AssetCalibration {
            symbol: "xyz:GOLD",
            sz_decimals: 4,
            ou_horizon_mins: 15,
            min_gross_entry_bps: 25.0,
            sigma_min: 1.52,
            sigma_max: 2.15,
            beta_overshoot: 0.50,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 4,
            cusum_delta_1m: 0.38,
            cusum_b_1m_star: 4.15,
            sprt_min_loss_gate_bps: -16.0,
            touch_rebound_min: 0.32,
            squeeze_ratio_max: 0.92,
            sprt_delta: 0.80,
            sprt_b_upper: 3.15,
            z_disaster_offset: 1.75,
            k_dir: 1.50,
            u_veto: -2.30,
            tau_obi_eng_a: 0.25,
            tau_obi_eng_b: 0.20,
            funding_trigger_bps: 2.5,
            funding_z24h_min: 2.10,
            oi_cascade_pct: 0.8,
            orb_min_range_bps: 30.0,
        },
        "XYZ:SILVER" | "SILVER" => AssetCalibration {
            symbol: "xyz:SILVER",
            sz_decimals: 2,
            ou_horizon_mins: 10,
            min_gross_entry_bps: 18.0,
            sigma_min: 1.56,
            sigma_max: 2.25,
            beta_overshoot: 0.54,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.35,
            cusum_b_1m_star: 3.90,
            sprt_min_loss_gate_bps: -18.0,
            touch_rebound_min: 0.34,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.82,
            sprt_b_upper: 3.10,
            z_disaster_offset: 1.85,
            k_dir: 1.65,
            u_veto: -2.10,
            tau_obi_eng_a: 0.28,
            tau_obi_eng_b: 0.24,
            funding_trigger_bps: 3.0,
            funding_z24h_min: 2.15,
            oi_cascade_pct: 0.9,
            orb_min_range_bps: 38.0,
        },
        "XYZ:XYZ100" | "XYZ100" => AssetCalibration {
            symbol: "xyz:XYZ100",
            sz_decimals: 4,
            ou_horizon_mins: 10,
            min_gross_entry_bps: 16.0,
            sigma_min: 1.52,
            sigma_max: 2.15,
            beta_overshoot: 0.52,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 4,
            cusum_delta_1m: 0.36,
            cusum_b_1m_star: 4.05,
            sprt_min_loss_gate_bps: -16.0,
            touch_rebound_min: 0.32,
            squeeze_ratio_max: 0.92,
            sprt_delta: 0.84,
            sprt_b_upper: 3.15,
            z_disaster_offset: 1.75,
            k_dir: 1.60,
            u_veto: -2.20,
            tau_obi_eng_a: 0.26,
            tau_obi_eng_b: 0.22,
            funding_trigger_bps: 2.5,
            funding_z24h_min: 2.15,
            oi_cascade_pct: 0.8,
            orb_min_range_bps: 32.0,
        },
        "XYZ:TSLA" | "TSLA" => AssetCalibration {
            symbol: "xyz:TSLA",
            sz_decimals: 3,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 20.0,
            sigma_min: 1.62,
            sigma_max: 2.35,
            beta_overshoot: 0.58,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.34,
            cusum_b_1m_star: 3.80,
            sprt_min_loss_gate_bps: -19.0,
            touch_rebound_min: 0.36,
            squeeze_ratio_max: 0.88,
            sprt_delta: 0.84,
            sprt_b_upper: 3.15,
            z_disaster_offset: 1.85,
            k_dir: 1.80,
            u_veto: -2.05,
            tau_obi_eng_a: 0.32,
            tau_obi_eng_b: 0.28,
            funding_trigger_bps: 3.5,
            funding_z24h_min: 2.25,
            oi_cascade_pct: 1.1,
            orb_min_range_bps: 50.0,
        },
        "ZEC" => AssetCalibration {
            symbol: "ZEC",
            sz_decimals: 3,
            ou_horizon_mins: 5,
            min_gross_entry_bps: 24.0,
            sigma_min: 1.60,
            sigma_max: 2.32,
            beta_overshoot: 0.60,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 3,
            cusum_delta_1m: 0.34,
            cusum_b_1m_star: 3.80,
            sprt_min_loss_gate_bps: -20.0,
            touch_rebound_min: 0.36,
            squeeze_ratio_max: 0.90,
            sprt_delta: 0.83,
            sprt_b_upper: 3.10,
            z_disaster_offset: 1.95,
            k_dir: 1.80,
            u_veto: -2.10,
            tau_obi_eng_a: 0.32,
            tau_obi_eng_b: 0.28,
            funding_trigger_bps: 3.8,
            funding_z24h_min: 2.25,
            oi_cascade_pct: 1.2,
            orb_min_range_bps: 55.0,
        },
        _ => AssetCalibration {
            symbol: "ETH",
            sz_decimals: 4,
            ou_horizon_mins: 10,
            min_gross_entry_bps: 24.0,
            sigma_min: 1.58,
            sigma_max: 2.20,
            beta_overshoot: 0.58,
            w2_min: 0.35,
            w2_max: 0.65,
            stall_trigger_mins: 4,
            cusum_delta_1m: 0.35,
            cusum_b_1m_star: 4.00,
            sprt_min_loss_gate_bps: -18.0,
            touch_rebound_min: 0.34,
            squeeze_ratio_max: 0.92,
            sprt_delta: 0.85,
            sprt_b_upper: 3.20,
            z_disaster_offset: 1.85,
            k_dir: 1.65,
            u_veto: -2.25,
            tau_obi_eng_a: 0.30,
            tau_obi_eng_b: 0.25,
            funding_trigger_bps: 3.5,
            funding_z24h_min: 2.25,
            oi_cascade_pct: 1.1,
            orb_min_range_bps: 48.0,
        },
    }
}

/// Trade direction enum: +1.0 for Long, -1.0 for Short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    #[inline(always)]
    pub fn sign(self) -> f64 {
        match self {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        }
    }

    #[inline(always)]
    pub fn opposite(self) -> Self {
        match self {
            Direction::Long => Direction::Short,
            Direction::Short => Direction::Long,
        }
    }

    #[inline(always)]
    pub fn from_side(side: &Side) -> Self {
        match side {
            Side::Long => Direction::Long,
            Side::Short => Direction::Short,
        }
    }

    #[inline(always)]
    pub fn to_side(self) -> Side {
        match self {
            Direction::Long => Side::Long,
            Direction::Short => Side::Short,
        }
    }
}

/// Fast composite Simpson quadrature of the imaginary error function erfi(x) = -i * erf(i*x)
/// for |x| <= 3.8, used in the Ornstein-Uhlenbeck scale function S(z).
#[inline]
#[allow(dead_code)]
pub fn erfi_approx(x: f64) -> f64 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let z = x.abs().min(3.8);
    const N: usize = 16;
    let h = z / (N as f64);
    let mut sum = 1.0 + (z * z).exp();
    for i in 1..N {
        let t = (i as f64) * h;
        let weight = if i % 2 == 1 { 4.0 } else { 2.0 };
        sum += weight * (t * t).exp();
    }
    let integral = (h / 3.0) * (2.0 / std::f64::consts::PI.sqrt()) * sum;
    sign * integral
}

/// Ornstein-Uhlenbeck natural scale function S(z) = sqrt(pi/2) * erfi(z / sqrt(2)).
#[inline]
#[allow(dead_code)]
pub fn ou_scale_function(z: f64) -> f64 {
    (std::f64::consts::PI / 2.0).sqrt() * erfi_approx(z / 2.0_f64.sqrt())
}

/// Exact two-sided First-Passage-Time probability P(tau_tp < tau_sl | z_0 = entry_z)
/// for a normalized O-U residual process dz_t = -theta * z_t dt + sqrt(2*theta) dW_t.
#[inline]
#[allow(dead_code)]
pub fn ou_first_passage_prob(entry_z: f64, tp_z: f64, sl_z: f64) -> f64 {
    let s_entry = ou_scale_function(entry_z);
    let s_tp = ou_scale_function(tp_z);
    let s_sl = ou_scale_function(sl_z);
    let denom = (s_tp - s_sl).abs().max(1e-12);
    ((s_entry - s_sl).abs() / denom).clamp(0.0, 1.0)
}

/// Computes the Rolling Variance-Ratio Hurst Exponent H_t in [0.15, 0.85]
/// over a rolling W=60-minute window comparing 5-minute return variance against 1-minute variance.
pub fn calculate_rolling_hurst_60m(closes: &[f64]) -> f64 {
    let n = closes.len();
    if n < 61 {
        return 0.45;
    }
    let w = 60usize;
    let q = 5usize;
    let start = n - w - 1;
    let slice = &closes[start..];

    let mut sum_sq_1m = 0.0;
    let mut cnt_1m = 0usize;
    for i in 1..slice.len() {
        if slice[i] > 0.0 && slice[i - 1] > 0.0 {
            let r = (slice[i] / slice[i - 1]).ln();
            sum_sq_1m += r * r;
            cnt_1m += 1;
        }
    }

    let mut sum_sq_qm = 0.0;
    let mut cnt_qm = 0usize;
    for i in q..slice.len() {
        if slice[i] > 0.0 && slice[i - q] > 0.0 {
            let r_q = (slice[i] / slice[i - q]).ln();
            sum_sq_qm += r_q * r_q;
            cnt_qm += 1;
        }
    }

    if cnt_1m == 0 || cnt_qm == 0 || sum_sq_1m <= 1e-16 || sum_sq_qm <= 1e-16 {
        return 0.45;
    }

    let var_1m = sum_sq_1m / (cnt_1m as f64);
    let var_qm = sum_sq_qm / (cnt_qm as f64);
    let ratio = (var_qm / var_1m).max(1e-6);
    let h = 0.5 * (ratio.ln() / (q as f64).ln());
    h.clamp(0.15, 0.85)
}

/// Computes the 30-bar Kaufman Efficiency Ratio KER_t in [0.0, 1.0].
pub fn calculate_ker_30m(closes: &[f64]) -> f64 {
    let n = closes.len();
    if n < 31 {
        return 0.25;
    }
    let w = 30usize;
    let slice = &closes[n - w - 1..];
    let net_disp = (slice[w] - slice[0]).abs();
    let mut path_sum = 0.0;
    for i in 1..=w {
        path_sum += (slice[i] - slice[i - 1]).abs();
    }
    if path_sum <= 1e-12 {
        0.0
    } else {
        (net_disp / path_sum).clamp(0.0, 1.0)
    }
}

/// Computes the Multi-Horizon Macro Drift Score Z_{macro}(t) from Section 4.3.
pub fn calculate_z_macro(
    closes: &[f64],
    eval_close: f64,
    sigma_1m: f64,
    kalman_slope_5m: f64,
) -> f64 {
    let n = closes.len();
    if n < 60 || eval_close <= 0.0 {
        return 0.0;
    }
    let lookback_240 = n.min(240);
    let alpha = 2.0 / (lookback_240 as f64 + 1.0);
    let mut ema_240 = closes[n - lookback_240];
    for &p in &closes[n - lookback_240 + 1..] {
        ema_240 = alpha * p + (1.0 - alpha) * ema_240;
    }

    // Approximate 1-hour ATR in price units using 1m diffusive volatility * sqrt(60)
    let atr_60_px = (eval_close * sigma_1m.max(1e-5) * (60.0_f64).sqrt()).max(eval_close * 0.0015);
    let term1 = (eval_close - ema_240) / (atr_60_px * 2.0); // sqrt(4) = 2.0

    // 60-minute net displacement of causal Kalman level (12 x 5m bars)
    let g_disp_60m = kalman_slope_5m * 12.0;
    let term2 = g_disp_60m / (sigma_1m.max(1e-5) * (60.0_f64).sqrt());

    0.55 * term1 + 0.45 * term2
}

/// Real-time local micro-regime state inputs for the Dynamic Entry Frontier.
#[derive(Debug, Clone, Copy)]
pub struct MicroRegimeState {
    pub hurst_60m: f64,         // Rolling variance-ratio Hurst exponent H_t in (0, 1)
    pub ker_30m: f64,           // 30-bar Kaufman Efficiency Ratio in [0, 1]
    pub gk_efficiency_15m: f64, // Garman-Klass body-to-volatility ratio rho_{GK, t}
    pub kalman_level: f64,      // Causal Kalman LLT level g_{t|t} (in log-price space)
    pub kalman_slope: f64,      // Causal Kalman LLT slope b_{t|t} (per 1m bar)
    pub sigma_1m: f64,          // 1-minute return standard deviation
    pub sigma_eq: f64,          // O-U stationary residual std dev: sigma / sqrt(2*theta)
    pub z_macro: f64,           // Multi-horizon macro drift score Z_{macro}(t)
}

impl MicroRegimeState {
    /// Computes the composite micro-regime index Phi_{regime}(t) in [0.0, 1.0].
    #[inline]
    pub fn phi_regime(&self) -> f64 {
        let h_term = 0.45 * ((self.hurst_60m - 0.35) / 0.25);
        let k_term = 0.35 * ((self.ker_30m - 0.20) / 0.40);
        let gk_term = 0.20 * ((self.gk_efficiency_15m - 0.35) / 0.45);
        (h_term + k_term + gk_term).clamp(0.0, 1.0)
    }

    /// Computes the dynamic regime-adaptive entry threshold sigma_{entry}(t, d) in units of sigma_eq.
    #[inline]
    pub fn dynamic_entry_sigma(&self, cal: &AssetCalibration, dir: Direction) -> f64 {
        let phi = self.phi_regime();
        let d = dir.sign();
        let std_slope = self.kalman_slope / self.sigma_1m.max(1e-8);
        let adverse_slope_penalty = 0.25 * (-d * std_slope).max(0.0);

        // Minimum fee-adjusted volatility hurdle: 18.0 bps (3.0 bps maker RT + 15.0 bps min edge)
        let min_hurdle_sigma = 0.0018 / self.sigma_eq.max(1e-6);

        let adaptive_sigma =
            cal.sigma_min + (cal.sigma_max - cal.sigma_min) * (phi * phi) + adverse_slope_penalty;

        adaptive_sigma
            .max(min_hurdle_sigma)
            .min(cal.sigma_max + 0.35)
    }

    /// Computes the continuous directional bias multiplier lambda_{dir}(t, d) in [0.0, 1.0].
    #[inline]
    pub fn directional_bias_multiplier(&self, cal: &AssetCalibration, dir: Direction) -> f64 {
        let u_t = dir.sign() * self.z_macro;
        if u_t <= cal.u_veto {
            return 0.0; // Parabolic runaway circuit breaker (prevents live NEAR short trap)
        }
        let lambda_floor = 0.25;
        let u_0 = 0.60;
        let logistic = 1.0 / (1.0 + (-cal.k_dir * (u_t + u_0)).exp());
        (lambda_floor + (1.0 - lambda_floor) * logistic).clamp(0.0, 1.0)
    }

    /// Computes the Closed-Form Kelly-Optimal Tranche Split (w_1*, w_2*) and Overshoot Ratio beta_ov*(t).
    #[inline]
    pub fn compute_optimal_tranche_split(
        &self,
        cal: &AssetCalibration,
        entry_z_abs: f64,
    ) -> TrancheAllocation {
        let phi = self.phi_regime();
        let beta_ov = (cal.beta_overshoot + 0.15 * (0.42 - self.hurst_60m)
            - 0.25 * (self.gk_efficiency_15m - 0.35).max(0.0))
        .clamp(0.35, 0.78);

        // Conditional probability q_ov(t) of hitting +beta_ov from z=0 before hitting the lock-in floor -(entry_z_abs - 0.30)
        let gamma_ratchet = 0.30;
        let z_lock_dist = (entry_z_abs - gamma_ratchet).max(0.50);
        let q_raw = ou_first_passage_prob(0.0, beta_ov, -z_lock_dist);
        let q_ov = (q_raw * (1.0 - 0.45 * phi)).clamp(0.10, 0.85);

        // Reward-to-giveback ratio Lambda(t) at stopping time tau_0 with Micro-Chandelier trailing (VIP0 fees: C_MM=2.88 bps, C_MT=6.26 bps)
        let fee_diff = 0.000626 - 0.000288;
        let gain_extra = beta_ov * self.sigma_eq;
        let giveback_risk = (0.45 * z_lock_dist * self.sigma_eq) + fee_diff;
        let lambda_ratio = gain_extra / giveback_risk.max(1e-6);

        let kelly_num = q_ov * lambda_ratio - (1.0 - q_ov);
        let kelly_den = q_ov * (1.0 - q_ov) * (1.0 + lambda_ratio).powi(2);
        let kelly_signal = (kelly_num / kelly_den.max(1e-6)).clamp(0.0, 1.0);

        let w2 = (cal.w2_min + (cal.w2_max - cal.w2_min) * kelly_signal * (1.0 - phi))
            .clamp(cal.w2_min, cal.w2_max);
        let w1 = 1.0 - w2;

        TrancheAllocation {
            w1_mean_fraction: w1,
            w2_runner_fraction: w2,
            beta_overshoot: beta_ov,
            conditional_overshoot_prob: q_ov,
        }
    }
}

/// Dynamic Tranche Allocation output (w_1, w_2, and adaptive overshoot ratio beta_ov).
#[derive(Debug, Clone, Copy)]
pub struct TrancheAllocation {
    pub w1_mean_fraction: f64,
    pub w2_runner_fraction: f64,
    pub beta_overshoot: f64,
    pub conditional_overshoot_prob: f64,
}

/// 1-Minute Bar Telemetry passed to the Wick-Filtered SPRT Stop Manager.
#[derive(Debug, Clone, Copy)]
pub struct OneMinBarTick {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub std_kalman_innov_1m: f64,
    pub ou_step_innov_1m: f64,
    pub smoothed_obi: f64,
}

/// Multi-Tranche Position & SPRT Dynamic Stop-Loss Manager for Engine A.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AsymmetricExitManager {
    pub dir: Direction,
    pub entry_price: f64,
    pub entry_log_px: f64,
    pub entry_z: f64,
    pub w1_fraction: f64,
    pub w2_fraction: f64,
    pub tranche1_filled: bool,
    pub tranche2_filled: bool,
    pub sprt_stat: f64,
    pub sprt_history: [f64; 4],
    pub innov_ring: [f64; 3],
    pub step_count: usize,
    pub trailing_stop_log_px: f64,
    pub hard_disaster_stop_log_px: f64,
    pub extreme_favorable_log_px: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitSignal {
    Hold {
        tp1_price: Option<f64>,
        tp2_price: f64,
        current_sl_price: f64,
        trend_prob: f64,
    },
    FillTranche1AndRatchet {
        tp2_price: f64,
        new_lock_sl_price: f64,
    },
    FillTranche2Complete,
    StopOutTranche2Trailing,
    StopOutFullSprtBreak {
        sprt_stat: f64,
        avg_adverse_innov_3m: f64,
    },
    StopOutIntrabarDisaster {
        exit_price: f64,
    },
}

#[allow(dead_code)]
impl AsymmetricExitManager {
    pub fn new(dir: Direction, entry_price: f64, entry_z: f64, sigma_eq: f64) -> Self {
        let entry_log_px = entry_price.max(1e-9).ln();
        let d = dir.sign();
        let disaster_offset = (1.90 * sigma_eq).max(0.0068);
        let hard_disaster_stop_log_px = entry_log_px - d * disaster_offset;
        Self {
            dir,
            entry_price,
            entry_log_px,
            entry_z,
            w1_fraction: 0.50,
            w2_fraction: 0.50,
            tranche1_filled: false,
            tranche2_filled: false,
            sprt_stat: 0.0,
            sprt_history: [0.0; 4],
            innov_ring: [0.0; 3],
            step_count: 0,
            trailing_stop_log_px: hard_disaster_stop_log_px,
            hard_disaster_stop_log_px,
            extreme_favorable_log_px: entry_log_px,
        }
    }

    pub fn new_with_alloc(
        dir: Direction,
        entry_price: f64,
        entry_z: f64,
        sigma_eq: f64,
        cal: &AssetCalibration,
        alloc: TrancheAllocation,
    ) -> Self {
        let entry_log_px = entry_price.max(1e-9).ln();
        let d = dir.sign();
        let disaster_offset = (cal.z_disaster_offset * sigma_eq).max(0.0068);
        let hard_disaster_stop_log_px = entry_log_px - d * disaster_offset;
        Self {
            dir,
            entry_price,
            entry_log_px,
            entry_z,
            w1_fraction: alloc.w1_mean_fraction,
            w2_fraction: alloc.w2_runner_fraction,
            tranche1_filled: false,
            tranche2_filled: false,
            sprt_stat: 0.0,
            sprt_history: [0.0; 4],
            innov_ring: [0.0; 3],
            step_count: 0,
            trailing_stop_log_px: hard_disaster_stop_log_px,
            hard_disaster_stop_log_px,
            extreme_favorable_log_px: entry_log_px,
        }
    }

    /// Evaluates the Two-Layer Stop Architecture & Multi-Tranche Exits at native 1-minute resolution
    /// with Wick-Absorption Deflation (\psi_{wick}) and Huber Saturation (Section 2.2 & 6.1).
    pub fn on_1m_bar(
        &mut self,
        bar: &OneMinBarTick,
        regime: &MicroRegimeState,
        cal: &AssetCalibration,
        ou_half_life_mins: f64,
    ) -> ExitSignal {
        let d = self.dir.sign();
        let close_log_px = bar.close.max(1e-9).ln();
        let high_log_px = bar.high.max(1e-9).ln();
        let low_log_px = bar.low.max(1e-9).ln();
        let range = (bar.high - bar.low).max(1e-12);
        let clv = ((2.0 * bar.close - bar.high - bar.low) / range).clamp(-1.0, 1.0);

        // 1. Compute Wick-Absorption Deflator psi_{wick, t} in [0.15, 1.00]
        let adverse_body_ratio = (-d * (bar.close - bar.open) / range).max(0.0);
        let adverse_clv = (-d * clv).max(0.0);
        let psi_wick = (adverse_body_ratio + 0.45 * adverse_clv).clamp(0.15, 1.00);

        // 2. Huber-saturated 1-minute composite innovation & sqrt(5) time-scaled SPRT update
        let raw_e = 0.60 * bar.std_kalman_innov_1m + 0.40 * bar.ou_step_innov_1m;
        let huber_c = 2.40;
        let eff_e = psi_wick * huber_c * (raw_e / huber_c).tanh();

        let delta_1m = cal.sprt_delta / 5.0_f64.sqrt();
        let obi_penalty = 0.10 * psi_wick * (-d * bar.smoothed_obi).max(0.0);
        let s_inc = -d * delta_1m * eff_e - 0.5 * delta_1m.powi(2) + obi_penalty;

        let decay = if s_inc >= 0.0 { 1.00 } else { 0.92 };
        self.sprt_stat = (decay * self.sprt_stat + s_inc).max(0.0);

        self.innov_ring[self.step_count % 3] = bar.std_kalman_innov_1m;
        self.sprt_history[self.step_count % 4] = self.sprt_stat;
        self.step_count += 1;

        let time_decay_mult = if (self.step_count as f64) > 1.5 * ou_half_life_mins.max(5.0) {
            0.72
        } else {
            1.00
        };
        let eff_b_upper = cal.sprt_b_upper * time_decay_mult;
        let trend_prob = 1.0 / (1.0 + (-2.0 * (self.sprt_stat - 0.5 * eff_b_upper)).exp());

        let alloc = regime.compute_optimal_tranche_split(cal, self.entry_z.abs());
        let tp1_log_px = regime.kalman_level;
        let tp2_log_px = regime.kalman_level + d * alloc.beta_overshoot * regime.sigma_eq;

        if !self.tranche1_filled {
            // Layer 1: Check Unconditioned Intrabar Catastrophe Guard on bar.low / bar.high
            let intrabar_adverse_log_px = if d > 0.0 { low_log_px } else { high_log_px };
            if d * (intrabar_adverse_log_px - self.hard_disaster_stop_log_px) <= 0.0 {
                return ExitSignal::StopOutIntrabarDisaster {
                    exit_price: self.hard_disaster_stop_log_px.exp(),
                };
            }

            // Layer 2: Check 1-Minute Close-Confirmed Dynamic SPRT Stop on bar.close
            let z_disaster_mag = self.entry_z.abs() + cal.z_disaster_offset;
            let contraction = (self.sprt_stat / eff_b_upper).clamp(0.0, 1.0).powf(1.20);
            let dyn_sl_z_mag = z_disaster_mag
                - (z_disaster_mag - self.entry_z.abs() - 0.15).max(0.0) * contraction;
            let sprt_close_sl_log_px = regime.kalman_level - d * dyn_sl_z_mag * regime.sigma_eq;

            if self.sprt_stat >= eff_b_upper || d * (close_log_px - sprt_close_sl_log_px) <= 0.0 {
                let avg_adverse_innov = -d * (self.innov_ring.iter().sum::<f64>() / 3.0);
                return ExitSignal::StopOutFullSprtBreak {
                    sprt_stat: self.sprt_stat,
                    avg_adverse_innov_3m: avg_adverse_innov,
                };
            }

            // Check Maker Touch-Fill on Tranche 1 at Kalman Mean mu
            let intrabar_fav_log_px = if d > 0.0 { high_log_px } else { low_log_px };
            if d * (intrabar_fav_log_px - tp1_log_px) >= 0.0 {
                self.tranche1_filled = true;
                self.extreme_favorable_log_px = intrabar_fav_log_px;
                let lock_offset = (0.30 * regime.sigma_eq).max(0.0012);
                self.trailing_stop_log_px = self.entry_log_px + d * lock_offset;
                return ExitSignal::FillTranche1AndRatchet {
                    tp2_price: tp2_log_px.exp(),
                    new_lock_sl_price: self.trailing_stop_log_px.exp(),
                };
            }

            return ExitSignal::Hold {
                tp1_price: Some(tp1_log_px.exp()),
                tp2_price: tp2_log_px.exp(),
                current_sl_price: sprt_close_sl_log_px.exp(),
                trend_prob,
            };
        }

        // Tranche 1 already filled: Manage Tranche 2 Runner & Micro-Chandelier Ratchet
        let intrabar_fav_log_px = if d > 0.0 { high_log_px } else { low_log_px };
        if d * (intrabar_fav_log_px - self.extreme_favorable_log_px) > 0.0 {
            self.extreme_favorable_log_px = intrabar_fav_log_px;
        }

        let lock_offset = (0.30 * regime.sigma_eq).max(0.0012);
        let lock_log_px = self.entry_log_px + d * lock_offset;
        if d * (self.extreme_favorable_log_px - lock_log_px) > 0.45 * regime.sigma_eq {
            let candidate_trail = self.extreme_favorable_log_px - d * (0.30 * regime.sigma_eq);
            if d * (candidate_trail - self.trailing_stop_log_px) > 0.0 {
                self.trailing_stop_log_px = candidate_trail;
            }
        }

        if d * (intrabar_fav_log_px - tp2_log_px) >= 0.0 {
            self.tranche2_filled = true;
            return ExitSignal::FillTranche2Complete;
        }

        let intrabar_adverse_log_px = if d > 0.0 { low_log_px } else { high_log_px };
        if d * (close_log_px - self.trailing_stop_log_px) <= 0.0
            || d * (intrabar_adverse_log_px - self.hard_disaster_stop_log_px) <= 0.0
        {
            return ExitSignal::StopOutTranche2Trailing;
        }

        ExitSignal::Hold {
            tp1_price: None,
            tp2_price: tp2_log_px.exp(),
            current_sl_price: self.trailing_stop_log_px.exp(),
            trend_prob,
        }
    }

    /// Updates the SPRT changepoint statistic and evaluates multi-tranche exit boundaries.
    pub fn on_bar_or_sentinel_tick(
        &mut self,
        current_price: f64,
        regime: &MicroRegimeState,
        cal: &AssetCalibration,
        std_kalman_innov: f64,
        ou_step_innov: f64,
        live_obi: f64,
    ) -> ExitSignal {
        let d = self.dir.sign();
        let log_px = current_price.max(1e-9).ln();
        let residual_x = log_px - regime.kalman_level;
        let current_z = residual_x / regime.sigma_eq.max(1e-6);

        // Update 3-bar innovation ring buffer for Handoff Gate 1
        self.innov_ring[self.step_count % 3] = std_kalman_innov;
        self.sprt_history[self.step_count % 4] = self.sprt_stat;
        self.step_count += 1;

        // 1. Update Sequential Probability Ratio Test (SPRT) log-likelihood statistic S_t
        let composite_e = 0.60 * std_kalman_innov + 0.40 * ou_step_innov;
        let obi_penalty = 0.45 * (-d * live_obi).max(0.0);
        let s_increment =
            -d * cal.sprt_delta * composite_e - 0.5 * cal.sprt_delta.powi(2) + obi_penalty;
        self.sprt_stat = (self.sprt_stat + s_increment).max(0.0);

        let trend_prob = 1.0 / (1.0 + (-2.0 * (self.sprt_stat - 0.5 * cal.sprt_b_upper)).exp());

        // 2. Compute dynamic TP1 (at Kalman mean mu=0) and TP2 (overshoot into opposite band)
        let beta_ov =
            (cal.beta_overshoot - 0.60 * (regime.hurst_60m - 0.35).max(0.0)).clamp(0.35, 0.75);
        let tp1_log_px = regime.kalman_level;
        let tp2_log_px = regime.kalman_level + d * beta_ov * regime.sigma_eq;

        // 3. Evaluate dynamic SPRT stop boundary contraction prior to Tranche 1 fill
        if !self.tranche1_filled {
            let z_sl_max = 2.65;
            let delta_min = 0.15;
            let contraction_ratio = (self.sprt_stat / cal.sprt_b_upper)
                .clamp(0.0, 1.0)
                .powf(1.25);
            let dynamic_sl_z_mag =
                z_sl_max - (z_sl_max - self.entry_z.abs() - delta_min).max(0.0) * contraction_ratio;
            let dynamic_sl_log_px = regime.kalman_level - d * dynamic_sl_z_mag * regime.sigma_eq;

            // Check if SPRT regime shift or dynamic stop boundary is breached
            if self.sprt_stat >= cal.sprt_b_upper || d * (log_px - dynamic_sl_log_px) <= 0.0 {
                let avg_adverse_innov = -d * (self.innov_ring.iter().sum::<f64>() / 3.0);
                return ExitSignal::StopOutFullSprtBreak {
                    sprt_stat: self.sprt_stat,
                    avg_adverse_innov_3m: avg_adverse_innov,
                };
            }

            // Check if Tranche 1 hits the Kalman equilibrium mean (z_TP1 = 0.00)
            if d * current_z >= 0.0 {
                self.tranche1_filled = true;
                self.extreme_favorable_log_px = log_px;
                // Jump stop-loss to Fee-Plus-Profit Lock-In Floor (+0.30 sigma_eq or +12 bps)
                let lock_offset = (0.30 * regime.sigma_eq).max(0.0012);
                self.trailing_stop_log_px = self.entry_log_px + d * lock_offset;
                return ExitSignal::FillTranche1AndRatchet {
                    tp2_price: tp2_log_px.exp(),
                    new_lock_sl_price: self.trailing_stop_log_px.exp(),
                };
            }

            return ExitSignal::Hold {
                tp1_price: Some(tp1_log_px.exp()),
                tp2_price: tp2_log_px.exp(),
                current_sl_price: dynamic_sl_log_px.exp(),
                trend_prob,
            };
        }

        // 4. Tranche 1 is already filled: Manage Tranche 2 Runner & Micro-Chandelier Ratchet
        if d * (log_px - self.extreme_favorable_log_px) > 0.0 {
            self.extreme_favorable_log_px = log_px;
        }
        let lock_offset = (0.30 * regime.sigma_eq).max(0.0012);
        let lock_log_px = self.entry_log_px + d * lock_offset;
        if d * (self.extreme_favorable_log_px - lock_log_px) > 0.50 * regime.sigma_eq {
            let candidate_trail = self.extreme_favorable_log_px - d * (0.30 * regime.sigma_eq);
            if d * (candidate_trail - self.trailing_stop_log_px) > 0.0 {
                self.trailing_stop_log_px = candidate_trail;
            }
        }

        if d * (log_px - tp2_log_px) >= 0.0 {
            self.tranche2_filled = true;
            return ExitSignal::FillTranche2Complete;
        }

        if d * (log_px - self.trailing_stop_log_px) <= 0.0 {
            return ExitSignal::StopOutTranche2Trailing;
        }

        ExitSignal::Hold {
            tp1_price: None,
            tp2_price: tp2_log_px.exp(),
            current_sl_price: self.trailing_stop_log_px.exp(),
            trend_prob,
        }
    }

    /// Returns the SPRT statistic 3 bars ago for Handoff Gate 1 velocity calculation.
    #[inline]
    pub fn sprt_stat_3m_ago(&self) -> f64 {
        if self.step_count < 3 {
            0.0
        } else {
            self.sprt_history[(self.step_count - 3) % 4]
        }
    }
}

/// Evaluates the Post-Touch Intrabar Rebound Ratio R_touch(t, d) on bar t when a resting
/// Alo Maker order at `limit_px` is touched. Fixes the 75% synchronous OBI abort bottleneck!
#[inline]
pub fn verify_maker_touch_absorption(
    dir: Direction,
    limit_px: f64,
    bar_high: f64,
    bar_low: f64,
    bar_close: f64,
    atr_1m: f64,
    prev_bar_obi_smooth: f64,
    cal: &HardenedAssetCalibration,
) -> bool {
    let d = dir.sign();
    // 1. Pre-touch bar must not be in an extreme adverse waterfall
    if d * prev_bar_obi_smooth < -0.28 {
        return false;
    }
    // 2. Post-touch intrabar rebound ratio
    let denom_pad = (0.30 * atr_1m).max(1e-10);
    let r_touch = match dir {
        Direction::Long => {
            let overshoot = (limit_px - bar_low).max(0.0);
            (bar_close - bar_low).max(0.0) / (overshoot + denom_pad)
        }
        Direction::Short => {
            let overshoot = (bar_high - limit_px).max(0.0);
            (bar_high - bar_close).max(0.0) / (overshoot + denom_pad)
        }
    };
    r_touch >= cal.touch_rebound_min
}

/// State-Contingent MFE Stall-Hazard & Siegmund-Corrected 1m CUSUM Exit Manager.
#[derive(Debug, Clone)]
pub struct StallHazardExitManager {
    pub dir: Direction,
    pub entry_px: f64,
    pub mfe_bps: f64,
    pub elapsed_mins: usize,
    pub last_peak_min: usize,
    pub tranche1_filled: bool,
    pub tranche2_filled: bool,
    pub cusum_stat_1m: f64,
    pub hard_disaster_stop_px: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HardenedExitDecision {
    Hold {
        tp1_maker_px: Option<f64>,
        tp2_maker_px: f64,
        hard_stop_px: f64,
    },
    FillTranche1Maker {
        fill_px: f64,
        new_tp2_maker_px: f64,
    },
    FillTranche2Maker {
        fill_px: f64,
        is_pegged_profit_lock: bool,
    },
    StopOutSprt1mClose {
        exit_px: f64,
        cusum_val: f64,
    },
    StopOutIntrabarDisaster {
        exit_px: f64,
    },
}

impl StallHazardExitManager {
    pub fn new(dir: Direction, entry_px: f64, disaster_stop_bps: f64) -> Self {
        let d = dir.sign();
        let hard_disaster_stop_px = entry_px * (1.0 - d * (disaster_stop_bps.max(42.0) * 1e-4));
        Self {
            dir,
            entry_px,
            mfe_bps: 0.0,
            elapsed_mins: 0,
            last_peak_min: 0,
            tranche1_filled: false,
            tranche2_filled: false,
            cusum_stat_1m: 0.0,
            hard_disaster_stop_px,
        }
    }

    /// Evaluates 1-minute bar using:
    /// 1) Velocity-Gated MFE Stall-Hazard Maker TP trajectory (never caps expanding winners),
    /// 2) Passive Alo Inside-Peg Profit Lock (avoids 6.26 bps Maker-Taker stop-market fee drag),
    /// 3) Siegmund/Wald AR(1) + Kurtosis-corrected 1m CUSUM stop gated at -18 to -21 bps.
    #[allow(clippy::too_many_arguments)]
    pub fn on_1m_bar(
        &mut self,
        bar_open: f64,
        bar_high: f64,
        bar_low: f64,
        bar_close: f64,
        prev_close: f64,
        kalman_mean_px: f64,
        sigma_eq_bps: f64,
        std_innov_1m: f64,
        obi_smooth: f64,
        spread_bps: f64,
        cal: &HardenedAssetCalibration,
    ) -> HardenedExitDecision {
        let d = self.dir.sign();
        self.elapsed_mins += 1;

        // 1. Update Intratrade MFE (in bps) and Stall Clock tau_stall(t)
        let fav_px = if d > 0.0 { bar_high } else { bar_low };
        let bar_fav_bps = d * ((fav_px - self.entry_px) / self.entry_px) * 10_000.0;
        if bar_fav_bps > self.mfe_bps + 0.5 {
            self.mfe_bps = bar_fav_bps;
            self.last_peak_min = self.elapsed_mins;
        }
        let tau_stall = self.elapsed_mins.saturating_sub(self.last_peak_min);

        let curr_unreal_bps = d * ((bar_close - self.entry_px) / self.entry_px) * 10_000.0;
        let range = (bar_high - bar_low).max(1e-12);
        let clv = ((2.0 * bar_close - bar_high - bar_low) / range).clamp(-1.0, 1.0);
        let price_vel_sign = (d * (bar_close - prev_close)).signum();

        // Reversion Momentum Score M_rev(t) in [-1.0, +1.0]
        let m_rev = 0.50 * (d * clv) + 0.30 * (d * obi_smooth) + 0.20 * price_vel_sign;

        // 2. Compute Target Mean Distance and Runner Overshoot Target
        let mean_dist_bps = (d * ((kalman_mean_px - self.entry_px) / self.entry_px) * 10_000.0)
            .max(cal.min_gross_entry_bps * 0.85);
        let tp2_overshoot_bps = mean_dist_bps + cal.beta_overshoot * sigma_eq_bps;

        // 3. State-Contingent Stall-Hazard Trajectory for Tranche 1 Maker Limit
        let is_expanding = tau_stall < cal.stall_trigger_mins || m_rev >= 0.22;
        let tp1_target_bps = if is_expanding || self.mfe_bps < 9.5 {
            // Active expansion: DO NOT CAP WINNER! Keep TP1 at full Kalman equilibrium mean.
            mean_dist_bps
        } else {
            // Verified mid-air stall: Harvest passively at upper-quartile of achieved MFE
            let stall_steps = (tau_stall.saturating_sub(cal.stall_trigger_mins - 1)).min(12) as f64;
            let mfe_retention = 0.88 - 0.045 * stall_steps;
            let stalled_bps = (self.mfe_bps * mfe_retention).max(2.88 + 3.8); // Min +3.8 bps net after 2.88 bps MM fee
            stalled_bps.min(mean_dist_bps)
        };

        let tp1_px = self.entry_px * (1.0 + d * tp1_target_bps * 1e-4);
        let mut tp2_px = self.entry_px * (1.0 + d * tp2_overshoot_bps * 1e-4);

        // 4. Check Intrabar Catastrophe Guard on bar_low / bar_high
        let adv_px = if d > 0.0 { bar_low } else { bar_high };
        if d * (adv_px - self.hard_disaster_stop_px) <= 0.0 {
            return HardenedExitDecision::StopOutIntrabarDisaster {
                exit_px: self.hard_disaster_stop_px.clamp(bar_low, bar_high),
            };
        }

        // 5. Siegmund/Wald AR(1) + Kurtosis-Corrected 1m Page-CUSUM Update
        if curr_unreal_bps >= 0.0 {
            // Reset CUSUM when trade is in profit so noise never builds up prior to drawdown
            self.cusum_stat_1m = 0.0;
        } else {
            let adv_body = (-d * (bar_close - bar_open) / range).max(0.0);
            let adv_clv = (-d * clv).max(0.0);
            let psi_wick = (adv_body + 0.45 * adv_clv).clamp(0.15, 1.00);

            let huber_c = 2.40;
            let eff_e = psi_wick * huber_c * (std_innov_1m / huber_c).tanh();
            let d1 = cal.cusum_delta_1m;

            // Exact AR(1) (rho_1 = -0.06 -> gamma = 0.8868) + Fourth-Cumulant Kurtosis Drift k_ref*
            let k_ref_star = 0.4434 * d1 * d1 + 0.50 * d1.powi(3);
            // Mean-centered OBI term (-d * obi - 0.15) prevents positive drift under H0!
            let obi_centered = 0.08 * psi_wick * (-d * obi_smooth - 0.15);
            let s_inc = -d * d1 * eff_e - k_ref_star + obi_centered;

            // Only accumulate positive increments when drawdown exceeds -6.0 bps
            if s_inc < 0.0 || curr_unreal_bps <= -6.0 {
                let decay = if s_inc >= 0.0 { 1.00 } else { 0.90 };
                self.cusum_stat_1m = (decay * self.cusum_stat_1m + s_inc).max(0.0);
            }
        }

        // Check 1m Close-Confirmed SPRT Stop (Gated at cal.sprt_min_loss_gate_bps = -18 to -21 bps)
        if !self.tranche1_filled
            && self.cusum_stat_1m >= cal.cusum_b_1m_star
            && curr_unreal_bps <= cal.sprt_min_loss_gate_bps
            && d * clv <= -0.20
        {
            return HardenedExitDecision::StopOutSprt1mClose {
                exit_px: bar_close,
                cusum_val: self.cusum_stat_1m,
            };
        }

        // 6. Check Tranche 1 Maker Touch-Fill
        if !self.tranche1_filled && d * (fav_px - tp1_px) >= 0.0 {
            self.tranche1_filled = true;
            return HardenedExitDecision::FillTranche1Maker {
                fill_px: tp1_px.clamp(bar_low, bar_high),
                new_tp2_maker_px: tp2_px,
            };
        }

        // 7. Manage Tranche 2: Check Overshoot Fill OR Passive Alo Inside-Peg Profit Lock
        if self.tranche1_filled {
            if d * (fav_px - tp2_px) >= 0.0 {
                self.tranche2_filled = true;
                return HardenedExitDecision::FillTranche2Maker {
                    fill_px: tp2_px.clamp(bar_low, bar_high),
                    is_pegged_profit_lock: false,
                };
            }

            // Passive Alo Inside-Peg Conversion: Avoids 6.26 bps Maker-Taker stop-market fee!
            let lock_floor_bps = (0.58 * self.mfe_bps).max(6.5);
            if curr_unreal_bps <= lock_floor_bps && m_rev <= -0.15 {
                let passive_peg_offset = (spread_bps * 0.35).max(0.5) * 1e-4;
                let pegged_maker_px = bar_close * (1.0 + d * passive_peg_offset);
                if d * (fav_px - pegged_maker_px) >= 0.0 || curr_unreal_bps >= 4.5 {
                    self.tranche2_filled = true;
                    return HardenedExitDecision::FillTranche2Maker {
                        fill_px: pegged_maker_px.clamp(bar_low, bar_high),
                        is_pegged_profit_lock: true,
                    };
                }
                tp2_px = pegged_maker_px;
            }
        }

        HardenedExitDecision::Hold {
            tp1_maker_px: if self.tranche1_filled {
                None
            } else {
                Some(tp1_px)
            },
            tp2_maker_px: tp2_px,
            hard_stop_px: self.hard_disaster_stop_px,
        }
    }
}

// =========================================================================
// SECTION: HIP-3 BUILDER PERPETUALS & MULTI-ASSET CROSS-UNIVERSE EXTENSIONS
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum AssetClass {
    CryptoMajor,
    CryptoAlt,
    UsEquityHip3,
    EquityIndexHip3,
    CommodityHip3,
    ForexHip3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ClearinghouseDex {
    NativeMain, // "" (default cross-margin clearinghouse)
    Hip3Xyz,    // "xyz" (Trade.xyz isolated-margin clearinghouse)
}

#[allow(dead_code)]
impl ClearinghouseDex {
    #[inline(always)]
    pub fn as_api_str(self) -> &'static str {
        match self {
            ClearinghouseDex::NativeMain => "",
            ClearinghouseDex::Hip3Xyz => "xyz",
        }
    }
}

/// Session schedule governing trading windows, US Open ORB phase, and mandatory Flat-at-Close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TradingSessionSchedule {
    Continuous24x7,
    UsEquityRth {
        open_min_utc: u16,          // 870 (14:30 UTC EST) or 810 (13:30 UTC EDT)
        orb_end_min_utc: u16,       // open + 60m (15:30 UTC EST / 14:30 UTC EDT)
        entry_freeze_min_utc: u16,  // close - 25m (20:35 UTC EST / 19:35 UTC EDT)
        maker_flatten_min_utc: u16, // close - 10m (20:50 UTC EST / 19:50 UTC EDT)
        hard_flat_min_utc: u16,     // close - 5m  (20:55 UTC EST / 19:55 UTC EDT)
    },
    CmeGlobex24x5 {
        daily_maint_start_min_utc: u16, // 1315 (21:55 UTC)
        daily_maint_end_min_utc: u16,   // 1390 (23:10 UTC)
        friday_flat_min_utc: u16,       // 1245 (20:45 UTC Friday)
        sunday_open_min_utc: u16,       // 1395 (23:15 UTC Sunday)
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionState {
    ActiveNormal,
    ActiveOpeningMomentum, // 14:35 - 15:30 UTC: Engine B prioritized, Engine A +0.35 sigma wider
    EntryFrozenHoldOnly,   // 20:35 - 20:50 UTC: Manage open trades, no new entries
    MandatoryMakerFlatten, // 20:50 - 20:55 UTC: Peg Alo limit at inside spread to exit
    MandatoryHardFlat,     // >= 20:55 UTC or Weekend: Immediate IOC reduce-only close
    Closed,
}

#[allow(dead_code)]
impl TradingSessionSchedule {
    /// Evaluates current UTC timestamp (seconds) into a deterministic SessionState.
    /// weekday_utc: 0 = Sunday, 1 = Monday, ..., 5 = Friday, 6 = Saturday.
    pub fn evaluate_session(&self, timestamp_sec: u64, weekday_utc: u8) -> SessionState {
        let min_of_day = ((timestamp_sec / 60) % 1440) as u16;

        match *self {
            TradingSessionSchedule::Continuous24x7 => SessionState::ActiveNormal,

            TradingSessionSchedule::UsEquityRth {
                open_min_utc,
                orb_end_min_utc,
                entry_freeze_min_utc,
                maker_flatten_min_utc,
                hard_flat_min_utc,
            } => {
                if weekday_utc == 0 || weekday_utc == 6 {
                    return SessionState::Closed;
                }
                // Lock out first 5 minutes after 09:30 ET bell (opening auction stabilization)
                if min_of_day < open_min_utc + 5 || min_of_day > hard_flat_min_utc + 5 {
                    SessionState::Closed
                } else if min_of_day >= hard_flat_min_utc {
                    SessionState::MandatoryHardFlat
                } else if min_of_day >= maker_flatten_min_utc {
                    SessionState::MandatoryMakerFlatten
                } else if min_of_day >= entry_freeze_min_utc {
                    SessionState::EntryFrozenHoldOnly
                } else if min_of_day < orb_end_min_utc {
                    SessionState::ActiveOpeningMomentum
                } else {
                    SessionState::ActiveNormal
                }
            }

            TradingSessionSchedule::CmeGlobex24x5 {
                daily_maint_start_min_utc,
                daily_maint_end_min_utc,
                friday_flat_min_utc,
                sunday_open_min_utc,
            } => {
                if weekday_utc == 6 {
                    return SessionState::Closed;
                }
                if weekday_utc == 0 && min_of_day < sunday_open_min_utc {
                    return SessionState::Closed;
                }
                if weekday_utc == 5 && min_of_day >= friday_flat_min_utc {
                    return SessionState::MandatoryHardFlat;
                }
                if min_of_day >= daily_maint_start_min_utc && min_of_day <= daily_maint_end_min_utc
                {
                    return SessionState::EntryFrozenHoldOnly;
                }
                SessionState::ActiveNormal
            }
        }
    }
}

/// Complete configuration for both Native Perps and HIP-3 Builder Perps.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct HIP3PerpConfig {
    pub ticker: &'static str, // e.g. "xyz:NVDA", "xyz:GOLD", "BTC"
    pub dex: ClearinghouseDex,
    pub asset_class: AssetClass,
    pub sz_decimals: u32,
    pub isolated_only: bool,
    pub target_leverage: u32,
    pub growth_mode_enabled: bool,
    pub deployer_fee_scale: f64, // 1.0 = 100% standard share
    pub schedule: TradingSessionSchedule,
    pub ou_horizon_mins: usize,
    pub half_life_mins: f64,
    pub min_gross_floor_growth_bps: f64,
    pub min_gross_floor_std_bps: f64,
    pub sigma_min: f64,
    pub sigma_max: f64,
    pub cusum_b_1m_star: f64,
    pub max_spread_bps: f64,
    pub beta_to_index: f64, // Beta to xyz:XYZ100 for equities, or BTC for crypto alts
}

#[allow(dead_code)]
impl HIP3PerpConfig {
    /// Computes exact VIP0 (with 4% referral discount) round-trip Maker-Maker and Maker-Taker
    /// fees in basis points, accounting for HIP-3 `growthMode` and `deployer_fee_scale`.
    #[inline]
    pub fn effective_fees_bps(&self) -> (f64, f64) {
        let base_maker_bps = 1.44;
        let base_taker_bps = 4.32;
        let stop_slippage_bps = 0.50;

        let fee_mult = match self.dex {
            ClearinghouseDex::NativeMain => 1.0,
            ClearinghouseDex::Hip3Xyz => {
                if self.growth_mode_enabled {
                    if self.deployer_fee_scale <= 1.0 {
                        0.10 * (1.0 + self.deployer_fee_scale)
                    } else {
                        0.20 * self.deployer_fee_scale
                    }
                } else if self.deployer_fee_scale <= 1.0 {
                    1.0 + self.deployer_fee_scale
                } else {
                    2.0 * self.deployer_fee_scale
                }
            }
        };

        let eff_maker = base_maker_bps * fee_mult;
        let eff_taker = base_taker_bps * fee_mult;
        let rt_maker_maker = 2.0 * eff_maker;
        let rt_maker_taker = eff_maker + eff_taker + stop_slippage_bps;
        (rt_maker_maker, rt_maker_taker)
    }

    #[inline]
    pub fn active_min_gross_floor_bps(&self) -> f64 {
        if self.dex == ClearinghouseDex::Hip3Xyz && self.growth_mode_enabled {
            self.min_gross_floor_growth_bps
        } else {
            self.min_gross_floor_std_bps
        }
    }
}

/// Registry of calibrated HIP-3 and Expansion Perpetuals
#[allow(dead_code)]
pub fn hip3_asset_config(symbol: &str) -> Option<HIP3PerpConfig> {
    let s_upper = symbol.to_uppercase();
    let s = s_upper.as_str();
    match s {
        "XYZ:NVDA" | "NVDA" => Some(HIP3PerpConfig {
            ticker: "xyz:NVDA",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::UsEquityHip3,
            sz_decimals: 3,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: true,
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::UsEquityRth {
                open_min_utc: 870,
                orb_end_min_utc: 930,
                entry_freeze_min_utc: 1235,
                maker_flatten_min_utc: 1250,
                hard_flat_min_utc: 1255,
            },
            ou_horizon_mins: 5,
            half_life_mins: 11.5,
            min_gross_floor_growth_bps: 18.0,
            min_gross_floor_std_bps: 28.0,
            sigma_min: 1.55,
            sigma_max: 2.25,
            cusum_b_1m_star: 3.85,
            max_spread_bps: 1.80,
            beta_to_index: 1.45,
        }),
        "XYZ:TSLA" | "TSLA" => Some(HIP3PerpConfig {
            ticker: "xyz:TSLA",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::UsEquityHip3,
            sz_decimals: 3,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: true,
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::UsEquityRth {
                open_min_utc: 870,
                orb_end_min_utc: 930,
                entry_freeze_min_utc: 1235,
                maker_flatten_min_utc: 1250,
                hard_flat_min_utc: 1255,
            },
            ou_horizon_mins: 5,
            half_life_mins: 13.0,
            min_gross_floor_growth_bps: 20.0,
            min_gross_floor_std_bps: 30.0,
            sigma_min: 1.62,
            sigma_max: 2.35,
            cusum_b_1m_star: 3.80,
            max_spread_bps: 2.20,
            beta_to_index: 1.60,
        }),
        "XYZ:AAPL" | "AAPL" => Some(HIP3PerpConfig {
            ticker: "xyz:AAPL",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::UsEquityHip3,
            sz_decimals: 3,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: true,
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::UsEquityRth {
                open_min_utc: 870,
                orb_end_min_utc: 930,
                entry_freeze_min_utc: 1235,
                maker_flatten_min_utc: 1250,
                hard_flat_min_utc: 1255,
            },
            ou_horizon_mins: 15,
            half_life_mins: 18.0,
            min_gross_floor_growth_bps: 16.0,
            min_gross_floor_std_bps: 26.0,
            sigma_min: 1.58,
            sigma_max: 2.20,
            cusum_b_1m_star: 4.00,
            max_spread_bps: 1.60,
            beta_to_index: 1.05,
        }),
        "XYZ:XYZ100" | "XYZ100" => Some(HIP3PerpConfig {
            ticker: "xyz:XYZ100",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::EquityIndexHip3,
            sz_decimals: 4,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: true,
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::UsEquityRth {
                open_min_utc: 870,
                orb_end_min_utc: 930,
                entry_freeze_min_utc: 1235,
                maker_flatten_min_utc: 1250,
                hard_flat_min_utc: 1255,
            },
            ou_horizon_mins: 10,
            half_life_mins: 14.5,
            min_gross_floor_growth_bps: 16.0,
            min_gross_floor_std_bps: 25.0,
            sigma_min: 1.55,
            sigma_max: 2.20,
            cusum_b_1m_star: 4.05,
            max_spread_bps: 1.20,
            beta_to_index: 1.00,
        }),
        "XYZ:GOLD" | "GOLD" => Some(HIP3PerpConfig {
            ticker: "xyz:GOLD",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::CommodityHip3,
            sz_decimals: 4,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: false,
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::CmeGlobex24x5 {
                daily_maint_start_min_utc: 1315,
                daily_maint_end_min_utc: 1390,
                friday_flat_min_utc: 1245,
                sunday_open_min_utc: 1395,
            },
            ou_horizon_mins: 15,
            half_life_mins: 22.0,
            min_gross_floor_growth_bps: 15.0,
            min_gross_floor_std_bps: 24.0,
            sigma_min: 1.52,
            sigma_max: 2.15,
            cusum_b_1m_star: 4.15,
            max_spread_bps: 0.80,
            beta_to_index: 0.0,
        }),
        "XYZ:SILVER" | "SILVER" => Some(HIP3PerpConfig {
            ticker: "xyz:SILVER",
            dex: ClearinghouseDex::Hip3Xyz,
            asset_class: AssetClass::CommodityHip3,
            sz_decimals: 2,
            isolated_only: true,
            target_leverage: 3,
            growth_mode_enabled: true, // Verified enabled on Mainnet (lastFeeScaleChangeTime: 2025-12-27)
            deployer_fee_scale: 1.0,
            schedule: TradingSessionSchedule::CmeGlobex24x5 {
                daily_maint_start_min_utc: 1315,
                daily_maint_end_min_utc: 1390,
                friday_flat_min_utc: 1245,
                sunday_open_min_utc: 1395,
            },
            ou_horizon_mins: 10,
            half_life_mins: 16.0,
            min_gross_floor_growth_bps: 18.0,
            min_gross_floor_std_bps: 27.0,
            sigma_min: 1.56,
            sigma_max: 2.25,
            cusum_b_1m_star: 3.90,
            max_spread_bps: 1.50,
            beta_to_index: 0.0,
        }),
        "ZEC" => Some(HIP3PerpConfig {
            ticker: "ZEC",
            dex: ClearinghouseDex::NativeMain,
            asset_class: AssetClass::CryptoAlt,
            sz_decimals: 3,
            isolated_only: false,
            target_leverage: 3,
            growth_mode_enabled: false,
            deployer_fee_scale: 0.0,
            schedule: TradingSessionSchedule::Continuous24x7,
            ou_horizon_mins: 5,
            half_life_mins: 9.5,
            min_gross_floor_growth_bps: 24.0,
            min_gross_floor_std_bps: 24.0,
            sigma_min: 1.60,
            sigma_max: 2.32,
            cusum_b_1m_star: 3.80,
            max_spread_bps: 2.80,
            beta_to_index: 0.85,
        }),
        _ => None,
    }
}

/// Evaluates asset-class-specific systematic macro lead-lag vetoes:
/// - Crypto Alts -> BTC Z_15m & velocity veto
/// - US Equities -> xyz:XYZ100 Z_15m, Kalman slope, and L2 OBI veto
/// - Commodities/FX -> Co-movement USD shock veto
#[inline]
#[allow(dead_code)]
pub fn passes_systematic_lead_lag_filter(
    cfg: &HIP3PerpConfig,
    dir_sign: f64,
    btc_z15m: f64,
    xyz100_z15m: f64,
    xyz100_std_slope: f64,
    xyz100_obi: f64,
    gold_z15m: f64,
    eur_z15m: f64,
) -> bool {
    match cfg.asset_class {
        AssetClass::CryptoMajor => true,
        AssetClass::CryptoAlt => dir_sign * btc_z15m > -1.80,
        AssetClass::UsEquityHip3 | AssetClass::EquityIndexHip3 => {
            let eq_impulse = dir_sign * xyz100_z15m;
            let eq_velocity = dir_sign * xyz100_std_slope + 0.50 * (dir_sign * xyz100_obi);
            eq_impulse > -1.65 && eq_velocity > -1.40
        }
        AssetClass::CommodityHip3 | AssetClass::ForexHip3 => {
            let gold_adverse = dir_sign * gold_z15m <= -1.75;
            let eur_adverse = dir_sign * eur_z15m <= -1.75;
            !(gold_adverse && eur_adverse)
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct CandidateSignal {
    pub cfg: &'static HIP3PerpConfig,
    pub dir_sign: f64,
    pub expected_gross_bps: f64,
    pub sigma_eq_bps: f64,
    pub lambda_dir: f64,
    pub spread_bps: f64,
    pub top5_depth_usdc: f64,
    pub corr_with_active_slot: f64,
}

#[allow(dead_code)]
pub struct DualClearinghouseSlotArbiter {
    pub native_slot_active: bool,
    pub xyz_slot_active: bool,
    pub target_notional_usdc: f64, // $175.00 USDC
}

#[allow(dead_code)]
impl DualClearinghouseSlotArbiter {
    pub fn new(target_notional_usdc: f64) -> Self {
        Self {
            native_slot_active: false,
            xyz_slot_active: false,
            target_notional_usdc,
        }
    }

    /// Selects the highest risk-adjusted candidate for available clearinghouse slots,
    /// enforcing liquidity screening, session schedules, and cross-correlation penalties.
    pub fn select_next_execution<'a>(
        &self,
        candidates: &'a [CandidateSignal],
        timestamp_sec: u64,
        weekday_utc: u8,
    ) -> Option<&'a CandidateSignal> {
        let mut best_candidate: Option<&'a CandidateSignal> = None;
        let mut best_score = 0.0_f64;

        for cand in candidates {
            // 1. Check Clearinghouse Slot Availability (Slot 1: NativeMain, Slot 2: Hip3Xyz)
            match cand.cfg.dex {
                ClearinghouseDex::NativeMain if self.native_slot_active => continue,
                ClearinghouseDex::Hip3Xyz if self.xyz_slot_active => continue,
                _ => {}
            }

            // 2. Check Session Schedule Gate
            let session = cand
                .cfg
                .schedule
                .evaluate_session(timestamp_sec, weekday_utc);
            if session != SessionState::ActiveNormal
                && session != SessionState::ActiveOpeningMomentum
            {
                continue;
            }

            // 3. Enforce Microstructure Liquidity & Spread Rubric
            if cand.spread_bps > cand.cfg.max_spread_bps {
                continue;
            }
            if cand.top5_depth_usdc < 1_000.0 * self.target_notional_usdc {
                continue;
            }

            // 4. Compute Fee- and Correlation-Penalized Utility Score U_i(t)
            let (rt_mm_fee_bps, _) = cand.cfg.effective_fees_bps();
            let min_floor = cand.cfg.active_min_gross_floor_bps();
            if cand.expected_gross_bps < min_floor {
                continue;
            }

            let net_edge_bps = cand.expected_gross_bps - rt_mm_fee_bps;
            if net_edge_bps <= 4.0 {
                continue;
            }

            let corr_penalty = 1.0 + cand.corr_with_active_slot.max(0.0);
            let score =
                (net_edge_bps / cand.sigma_eq_bps.max(5.0)) * cand.lambda_dir / corr_penalty;

            if score > best_score {
                best_score = score;
                best_candidate = Some(cand);
            }
        }

        best_candidate
    }
}

// =========================================================================
// SECTION: DYNAMIC HIP-3 FEE SCHEDULE & DST-AWARE SESSION LIFECYCLE GATE
// =========================================================================

/// Returns the minimum adverse excursion from entry (in bps) required before
/// `TwoTierInvalidationManager` permits a 5m soft structural invalidation flatten.
#[inline]
pub fn engine_a_min_soft_inv_excursion_bps(symbol: &str) -> f64 {
    match symbol.to_uppercase().as_str() {
        "XYZ:GOLD" | "GOLD" => 32.0,
        "XYZ:XYZ100" | "XYZ100" => 35.0,
        "XYZ:SILVER" | "SILVER" => 42.0,
        "BTC" => 45.0,
        "ETH" | "XYZ:NVDA" | "NVDA" => 48.0,
        "XRP" | "LINK" | "XYZ:TSLA" | "TSLA" => 58.0,
        "SOL" => 55.0,
        "AVAX" => 60.0,
        "ZEC" => 62.0,
        "SUI" | "NEAR" => 65.0,
        "HYPE" => 68.0,
        _ => 50.0,
    }
}

/// Computes the Engine A Exchange-Native / Fast-Sentinel catastrophic disaster stop distance
/// strictly outside `TwoTierInvalidationManager` (`>= max(2.5 * tp_dist, min_soft_inv + 35 bps, 85 bps)`),
/// preventing 1.5s L2 noise from prematurely front-running 5m statistical invalidation.
#[inline]
pub fn compute_engine_a_disaster_stop_distance(symbol: &str, entry_px: f64, tp_px: f64) -> f64 {
    let ou_cfg = asset_ou_config(symbol);
    let min_soft_inv_bps = engine_a_min_soft_inv_excursion_bps(symbol);
    let min_disaster_bps = (min_soft_inv_bps + 35.0).max(85.0);
    let edge_dist = (entry_px - tp_px).abs();
    let calibrated_sl = (2.5 * edge_dist).max(entry_px * (min_disaster_bps * 1e-4));
    calibrated_sl.min(entry_px * (ou_cfg.hard_disaster_stop_bps * 1e-4))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AssetFeeSchedule {
    pub maker_bps: f64,
    pub taker_bps: f64,
    pub stop_slippage_bps: f64,
}

static LIVE_FEE_SCHEDULE_REGISTRY: OnceLock<RwLock<HashMap<String, AssetFeeSchedule>>> =
    OnceLock::new();

fn live_fee_registry() -> &'static RwLock<HashMap<String, AssetFeeSchedule>> {
    LIVE_FEE_SCHEDULE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Registers a live fee schedule discovered from Hyperliquid `metaAndAssetCtxs`.
pub fn update_live_fee_schedule(symbol: &str, schedule: AssetFeeSchedule) {
    if let Ok(mut w) = live_fee_registry().write() {
        w.insert(symbol.to_uppercase(), schedule);
    }
}

impl AssetFeeSchedule {
    /// Computes exact Hyperliquid HIP-3 VIP0 (with 4% referral discount) fees from live metadata.
    pub fn from_hip3_meta(growth_mode_enabled: bool, deployer_fee_scale: f64) -> Self {
        let base_maker_bps = 1.44; // 1.5 bps * 0.96 referral discount
        let base_taker_bps = 4.32; // 4.5 bps * 0.96 referral discount
        let scale = if deployer_fee_scale.is_finite() && deployer_fee_scale >= 0.0 {
            deployer_fee_scale
        } else {
            1.0
        };
        let mult = if growth_mode_enabled {
            if scale <= 1.0 {
                0.10 * (1.0 + scale)
            } else {
                0.20 * scale
            }
        } else if scale <= 1.0 {
            1.0 + scale
        } else {
            2.0 * scale
        };
        Self {
            maker_bps: base_maker_bps * mult,
            taker_bps: base_taker_bps * mult,
            stop_slippage_bps: if growth_mode_enabled { 0.50 } else { 0.60 },
        }
    }

    /// Returns the live-discovered fee schedule if registered at startup, or the verified
    /// Mainnet fallback for `symbol`.
    pub fn for_symbol(symbol: &str) -> Self {
        let upper = symbol.to_uppercase();
        if let Ok(r) = live_fee_registry().read() {
            if let Some(live) = r.get(&upper) {
                return *live;
            }
        }
        Self::static_fallback_for_symbol(&upper)
    }

    pub fn static_fallback_for_symbol(symbol_upper: &str) -> Self {
        match symbol_upper {
            // Non-growth HIP-3 GOLD: scaleIfHip3 = 2.0, growthModeScale = 1.0
            "XYZ:GOLD" | "GOLD" => Self {
                maker_bps: 2.88,
                taker_bps: 8.64,
                stop_slippage_bps: 0.60,
            },
            // Growth-Mode HIP-3 Equities, Index & SILVER: scaleIfHip3 = 2.0, growthModeScale = 0.1
            "XYZ:NVDA" | "NVDA" | "XYZ:TSLA" | "TSLA" | "XYZ:XYZ100" | "XYZ100" | "XYZ:SILVER"
            | "SILVER" | "XYZ:AAPL" | "AAPL" => Self {
                maker_bps: 0.288,
                taker_bps: 0.864,
                stop_slippage_bps: 0.50,
            },
            // Native Validator Perps (VIP0 + 4% Referral Discount)
            _ => Self {
                maker_bps: 1.44,
                taker_bps: 4.32,
                stop_slippage_bps: 0.50,
            },
        }
    }

    #[inline]
    pub fn maker_maker_rt_bps(&self) -> f64 {
        2.0 * self.maker_bps
    }

    #[inline]
    pub fn maker_taker_rt_bps(&self) -> f64 {
        self.maker_bps + self.taker_bps + self.stop_slippage_bps
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleAction {
    AllowNormalTrading,
    BlockNewEntriesAndRatchetTp(&'static str),
    ForceFlat(&'static str),
}

/// Evaluates DST-aware RWA session lifecycle rules for Engine A / active position management:
/// - US Equities (`xyz:NVDA`, `xyz:TSLA`, `xyz:XYZ100`): 09:35-15:40 ET normal entries,
///   15:40-15:57 ET entry freeze + TP breakeven ratchet, >=15:57 ET or outside RTH/weekend ForceFlat.
/// - CME Metals (`xyz:GOLD`, `xyz:SILVER`): 17:00-18:00 ET daily maintenance & weekend lockout.
/// - Native Crypto: 24/7 AllowNormalTrading.
pub fn evaluate_session_lifecycle_gate(
    symbol: &str,
    utc_timestamp_ms: i64,
) -> SessionLifecycleAction {
    use chrono::{Datelike, TimeZone, Utc};
    let s_upper = symbol.to_uppercase();
    let is_us_equity = matches!(
        s_upper.as_str(),
        "XYZ:NVDA" | "NVDA" | "XYZ:TSLA" | "TSLA" | "XYZ:XYZ100" | "XYZ100" | "XYZ:AAPL" | "AAPL"
    );
    let is_cme_metal = matches!(
        s_upper.as_str(),
        "XYZ:GOLD" | "GOLD" | "XYZ:SILVER" | "SILVER"
    );

    if !is_us_equity && !is_cme_metal {
        return SessionLifecycleAction::AllowNormalTrading;
    }

    let dt = match Utc.timestamp_millis_opt(utc_timestamp_ms).single() {
        Some(d) => d,
        None => return SessionLifecycleAction::AllowNormalTrading,
    };
    let is_us_dst = is_us_daylight_saving_date(dt.year(), dt.month(), dt.day());
    let total_day_mins = ((utc_timestamp_ms.max(0) as u64 / 60_000) % 1440) as u32;
    let day_of_week = dt.weekday().num_days_from_sunday(); // 0=Sun, 6=Sat

    if is_us_equity {
        if day_of_week == 0 || day_of_week == 6 {
            return SessionLifecycleAction::ForceFlat("US_EQUITY_WEEKEND");
        }
        // US RTH: 09:30 - 16:00 ET -> EDT: 810..1200 UTC mins, EST: 870..1260 UTC mins
        let (rth_open, rth_close) = if is_us_dst { (810, 1200) } else { (870, 1260) };
        if total_day_mins < rth_open || total_day_mins >= rth_close - 3 {
            return SessionLifecycleAction::ForceFlat("US_EQUITY_RTH_CLOSE");
        }
        if total_day_mins < rth_open + 5 {
            return SessionLifecycleAction::BlockNewEntriesAndRatchetTp(
                "US_EQUITY_OPEN_AUCTION_5M",
            );
        }
        if total_day_mins >= rth_close - 20 {
            return SessionLifecycleAction::BlockNewEntriesAndRatchetTp("US_EQUITY_PRE_CLOSE_20M");
        }
    } else if is_cme_metal {
        // CME Globex daily maintenance: 17:00 - 18:00 ET -> EDT: 1260..1320, EST: 1320..1380
        let (maint_start, maint_end) = if is_us_dst {
            (1260, 1320)
        } else {
            (1320, 1380)
        };
        if day_of_week == 6
            || (day_of_week == 5 && total_day_mins >= maint_start - 5)
            || (day_of_week == 0 && total_day_mins < maint_end + 5)
        {
            return SessionLifecycleAction::ForceFlat("CME_WEEKEND_CLOSED");
        }
        if total_day_mins >= maint_start - 5 && total_day_mins < maint_end + 5 {
            return SessionLifecycleAction::ForceFlat("CME_DAILY_MAINTENANCE");
        }
        if total_day_mins >= maint_start - 20 && total_day_mins < maint_start - 5 {
            return SessionLifecycleAction::BlockNewEntriesAndRatchetTp("CME_PRE_MAINT_20M");
        }
    }

    SessionLifecycleAction::AllowNormalTrading
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_half_life_calculation() {
        let params = OuParameters {
            theta: 0.02,
            mu: 100.0,
            sigma: 1.0,
        };
        let hl = params.half_life_minutes();
        assert!((hl - 34.657).abs() < 0.01);

        let slow_params = OuParameters {
            theta: 0.0,
            mu: 100.0,
            sigma: 1.0,
        };
        assert_eq!(slow_params.half_life_minutes(), 999.0);
    }

    #[test]
    fn test_optimal_thresholds_contain_cost_barrier() {
        let params = OuParameters {
            theta: 0.05,
            mu: 2700.0,
            sigma: 5.0,
        };
        let tx_cost = 2.70;
        let thresholds = calculate_optimal_thresholds(&params, tx_cost);

        assert!(thresholds.entry_long < params.mu);
        assert_eq!(thresholds.exit_long, params.mu);
        assert!(params.mu - thresholds.entry_long >= tx_cost * 2.5);
    }

    #[test]
    fn test_mad_sigma_robust_against_outliers() {
        let mut returns = vec![0.001, -0.001, 0.0012, -0.0009, 0.0008, -0.0011, 0.0002];
        let sigma_base = calculate_mad_sigma(&returns);
        returns.push(-0.05);
        let sigma_with_outlier = calculate_mad_sigma(&returns);
        assert!((sigma_with_outlier - sigma_base).abs() < 0.0005);
    }

    #[test]
    fn test_calculate_net_edge_bps() {
        let low_edge = calculate_net_edge_bps(100.0, 100.20);
        assert!(low_edge < 25.0);
        assert!((low_edge - 19.98).abs() < 0.1);

        let good_edge = calculate_net_edge_bps(100.0, 100.35);
        assert!(good_edge >= 25.0);

        assert_eq!(calculate_net_edge_bps(100.0, 99.0), 0.0);
        assert_eq!(calculate_net_edge_bps(100.0, 100.0), 0.0);

        let short_edge = calculate_directional_edge_bps(100.0, 99.65, false);
        assert!(short_edge >= 25.0);
        assert!((short_edge - 35.06).abs() < 0.1);

        assert_eq!(calculate_directional_edge_bps(100.0, 100.5, false), 0.0);
        assert_eq!(calculate_directional_edge_bps(0.0, 100.0, true), 0.0);
        assert_eq!(calculate_directional_edge_bps(100.0, -5.0, false), 0.0);
    }

    #[test]
    fn test_calculate_features() {
        let mut history = Vec::new();
        for i in 0..720 {
            history.push(100.0 + (i as f64 * 0.06).sin() * 1.2);
        }
        let feat = calculate_features(&history, 100.5, "BTC").expect("Features calculated");
        assert!(feat.half_life_m > 0.0);
        assert!(feat.z_15.is_finite());
        assert!(feat.z_60.is_finite());
    }

    #[test]
    fn test_rogers_satchell_and_garman_klass() {
        let opens = vec![100.0, 101.0, 100.5, 99.8, 100.2];
        let highs = vec![101.5, 102.0, 101.2, 100.6, 101.0];
        let lows = vec![99.5, 100.2, 99.7, 99.1, 99.9];
        let closes = vec![101.0, 100.5, 99.8, 100.2, 100.8];

        let rs = calculate_rogers_satchell_sigma(&opens, &highs, &lows, &closes);
        let gk = calculate_garman_klass_sigma(&opens, &highs, &lows, &closes);

        assert!(rs > 0.0 && rs.is_finite());
        assert!(gk > 0.0 && gk.is_finite());
        assert!((rs - gk).abs() < 0.02);
    }

    #[test]
    fn test_conditional_ladder_thresholds_relationship() {
        let params = OuParameters {
            theta: 0.03,
            mu: 100.0,
            sigma: 1.5,
        };
        let tx_cost = 0.05;
        let th_150 = calculate_optimal_thresholds(&params, tx_cost);
        let th_125 = calculate_thresholds_at_sigma(&params, tx_cost, 1.25);

        assert!(th_125.entry_long > th_150.entry_long);
        assert!(th_125.entry_long < params.mu);
        assert!(th_150.entry_long < params.mu);
        assert!(th_125.entry_short < th_150.entry_short);
        assert!(th_125.entry_short > params.mu);
        assert!(th_150.entry_short > params.mu);
        assert_eq!(th_125.exit_long, params.mu);
        assert_eq!(th_150.exit_long, params.mu);
    }

    #[test]
    fn test_shock_physical_return_hurdle() {
        let mut history = vec![100.0; 120];
        for i in 0..120 {
            if i % 2 == 0 {
                history[i] = 100.01;
            } else {
                history[i] = 99.99;
            }
        }

        let feat_quiet = calculate_features(&history, 100.28, "BTC").expect("features");
        assert!(feat_quiet.z_15 > 3.0);
        assert!(
            !feat_quiet.is_upside_shock,
            "28 bps move must not trigger shock veto despite high Z!"
        );

        let feat_real_shock = calculate_features(&history, 101.50, "BTC").expect("features");
        assert!(feat_real_shock.z_15 > 3.0);
        assert!(
            feat_real_shock.is_upside_shock,
            "150 bps flash move must trigger shock veto!"
        );

        let feat_dump = calculate_features(&history, 98.00, "BTC").expect("features");
        assert!(feat_dump.z_15 < -3.0);
        assert!(
            feat_dump.is_downside_shock,
            "-200 bps flash crash must trigger downside shock!"
        );
    }

    #[test]
    fn test_monotone_trend_rejected_by_ou_jackknife() {
        let mut trend_prices = Vec::with_capacity(200);
        for i in 0..200 {
            trend_prices.push(100.0 * (0.001 * i as f64).exp());
        }
        assert!(estimate_ou_parameters(&trend_prices, 1.0).is_none());
    }

    #[test]
    fn test_carry_adjusted_asymmetric_ou_thresholds() {
        let params = OuParameters {
            theta: 0.02,
            mu: 100.0,
            sigma: 0.20,
        };
        let tx_cost = 0.05;
        let neutral = calculate_carry_adjusted_thresholds(&params, tx_cost, 1.65, 0.0, 1.45, 1.95);
        let pos_fund =
            calculate_carry_adjusted_thresholds(&params, tx_cost, 1.65, 0.0005, 1.45, 1.95);
        assert!(pos_fund.entry_short < neutral.entry_short);
        assert!(pos_fund.entry_long < neutral.entry_long);
    }

    #[test]
    fn test_5m_ou_preserves_stationarity_and_enforces_sigma_eq_floor() {
        // Generate a 720-minute (144 x 5m bars) stationary series with t_half around ~35m
        let mut history = Vec::with_capacity(720);
        for i in 0..720 {
            history.push(100.0 + (i as f64 * 0.055).sin() * 0.85);
        }
        let feat = calculate_features(&history, 100.0, "SOL").expect("Features calculated");
        assert!(
            feat.half_life_m >= 20.0 && feat.half_life_m <= 135.0,
            "Expected 5m t_half in 20m-135m band, got {}",
            feat.half_life_m
        );
        assert!(
            feat.is_stationary,
            "5m stationary process must be stationary"
        );
        assert!(
            feat.is_valid_half_life,
            "t_half in 20m-135m band must pass is_valid_half_life"
        );
        assert!(
            feat.is_consolidation,
            "Non-drifting stationary series must be valid consolidation"
        );
        assert!(
            feat.sigma_eq_log >= 0.0055 - 1e-6,
            "SOL sigma_eq_log must respect 55 bps floor"
        );
    }

    #[test]
    fn test_kalman_llt_waterfall_drift_veto_and_cusum() {
        let mut llt = KalmanLltFilter::new(0.0015, 0.25, 0.0625);
        let mut p = 100.0_f64.ln();
        // Warmup with 20 calm oscillating 5m bars
        for i in 0..20 {
            let obs = p + 0.0008 * ((i as f64) * 0.7).sin();
            let out = llt.update(obs, 0.45);
            assert!(!out.drift_veto_active);
        }
        // Simulate a -300 bps waterfall drop over 6 five-minute bars (-50 bps per 5m bar)
        let mut veto_fired = false;
        for _ in 0..6 {
            p -= 0.0050;
            let out = llt.update(p, 0.45);
            if out.drift_veto_active {
                veto_fired = true;
            }
        }
        assert!(
            veto_fired,
            "Kalman LLT filter must trigger waterfall drift veto during steep sell-off"
        );
        assert!(
            llt.cusum_neg > 2.0,
            "Downside innovation CUSUM must accumulate during waterfall"
        );
    }

    #[test]
    fn test_multi_bar_reversal_confirmation_join_vs_retest_and_abort() {
        let mut machine = ReversalConfirmationMachine::new(0.35, 0.22, 0.50, 38.0, 1.5);
        // Seed 3 prior closes before arming: 100.20, 100.00, 99.80
        let _ = machine.on_1m_bar_close(100.25, 100.15, 100.20, 100.19, 100.21, 0.01, 0.0007);
        let _ = machine.on_1m_bar_close(100.05, 99.95, 100.00, 99.99, 100.01, 0.01, 0.0007);
        let _ = machine.on_1m_bar_close(99.85, 99.75, 99.80, 99.79, 99.81, 0.01, 0.0007);

        // Arm Long at $99.30 with mean target $100.00 (70 bps gross excursion)
        machine.arm(Side::Long, 99.30, 100.00);

        // Bar 1 after arm: prints new low at $99.25, closes at $99.28 -> resets bars_since_extreme = 0 -> Wait
        let a1 = machine.on_1m_bar_close(99.31, 99.25, 99.28, 99.27, 99.29, 0.01, 0.0007);
        assert_eq!(a1, OrderPlacementAction::Wait);

        // Bar 2: low $99.26 > $99.25 (bars_since_extreme = 1 < 2) -> Wait
        let a2 = machine.on_1m_bar_close(99.32, 99.26, 99.31, 99.30, 99.32, 0.01, 0.0007);
        assert_eq!(a2, OrderPlacementAction::Wait);

        // Bar 3: low $99.28 > $99.25 (bars_since_extreme = 2), close $99.35 >= arm $99.30,
        // 3m return ln(99.35 / 99.28) > 0, consumed = (99.35 - 99.25) / (100.00 - 99.25) = 13.3% <= 22% -> PostAloJoinBook!
        let a3 = machine.on_1m_bar_close(99.36, 99.28, 99.35, 99.34, 99.36, 0.01, 0.0007);
        match a3 {
            OrderPlacementAction::PostAloJoinBook {
                side,
                limit_price,
                entry_ttl_mins,
                ..
            } => {
                assert_eq!(side, Side::Long);
                assert!((limit_price - 99.34).abs() < 1e-6);
                assert_eq!(entry_ttl_mins, 2.0);
            }
            other => panic!("Expected PostAloJoinBook, got {:?}", other),
        }
    }

    #[test]
    fn test_two_tier_invalidation_ignores_1bar_wick_and_triggers_on_2bar_5m_breach() {
        let mut mgr = TwoTierInvalidationManager::new(
            Side::Long,
            100.0,
            150.0, // 150 bps disaster stop -> $98.50
            2.75,  // 2.75\sigma soft 5m threshold
            3.00,
            40.0, // 40m half-life
        );
        assert!((mgr.exchange_hard_stop_price(0.01) - 98.50).abs() < 1e-6);

        // 5m Bar 1: single-bar spike to Z_OU = -2.90 -> must HOLD (consecutive = 1 < 2)
        let d1 = mgr.on_5m_bar_close(-2.90, -1.5, false, 1.5, 0.01);
        assert!(matches!(d1, InvalidationDecision::HoldPosition { .. }));
        assert_eq!(mgr.consecutive_ou_breach_bars, 1);

        // 5m Bar 2: reverts to Z_OU = -1.80 -> resets streak to 0!
        let d2 = mgr.on_5m_bar_close(-1.80, -1.0, false, 1.5, 0.01);
        assert!(matches!(d2, InvalidationDecision::HoldPosition { .. }));
        assert_eq!(mgr.consecutive_ou_breach_bars, 0);

        // 5m Bars 3 & 4: two consecutive completed 5m bars with Z_OU <= -2.75 -> triggers soft flatten!
        let _ = mgr.on_5m_bar_close(-2.80, -1.2, false, 1.5, 0.01);
        let d4 = mgr.on_5m_bar_close(-2.95, -1.4, false, 1.5, 0.01);
        assert!(matches!(
            d4,
            InvalidationDecision::TriggerSoftInvalidationFlatten { .. }
        ));
    }

    #[test]
    fn test_safe_tp_offset_bps_repairs_inverted_clamp() {
        // When remaining distance is only 10 bps (< 13.33 bps), 0.15 * 10 = 1.5 bps < 2.0 bps lower bound -> returns None
        assert!(compute_safe_tp_offset_bps(100.10, 100.0, 0.01).is_none());
        // When remaining distance is 80 bps, 0.15 * 80 = 12 bps -> clamped to min(10, 12) = 10 bps, offset = 5 bps
        let off = compute_safe_tp_offset_bps(100.80, 100.0, 0.01).expect("Valid offset");
        assert!((off - 5.0).abs() < 1e-6);
    }

    #[test]
    fn test_5m_kalman_recovers_stationarity_after_2pct_jump() {
        // Simulate 500 minutes around $81,500, then a +2% step jump to $83,130 followed by 220 minutes of consolidation
        let mut history = Vec::with_capacity(720);
        for i in 0..500 {
            history.push(81_500.0 * (1.0 + 0.0045 * ((i as f64) * 0.07).sin()));
        }
        for i in 0..220 {
            history.push(83_130.0 * (1.0 + 0.0045 * ((i as f64) * 0.07).sin()));
        }
        let eval_px = *history.last().unwrap();
        let feat = calculate_features(&history, eval_px, "BTC").expect("BTC features");
        assert!(
            feat.is_stationary && feat.is_valid_half_life,
            "5m Kalman LLT must recover valid half-life after a 2% step shift (got t_half={:.1}m)",
            feat.half_life_m
        );
    }

    #[test]
    fn test_volatility_adjusted_notional_caps_contract_under_high_vol() {
        let sol_cfg = asset_ou_config("SOL");
        // At $534 equity, base dynamic cap for SOL is ~$225
        let base_cap = sol_cfg.dynamic_notional_cap(534.0);
        assert!((base_cap - 225.0).abs() <= 1.0);

        // Normal volatility (32 bps <= 37.5 bps reference) -> full cap (1.0 - 1.05x)
        let calm_cap = sol_cfg.volatility_adjusted_notional_cap(534.0, 0.0032);
        assert!(calm_cap >= base_cap);

        // High volatility (75 bps = 2x normal reference) -> contracts cap to ~50% (~$113)
        let storm_cap = sol_cfg.volatility_adjusted_notional_cap(534.0, 0.0075);
        assert!((storm_cap - 113.0).abs() <= 2.0);

        // Trend Engine B ATR scaling: SOL at $100 with $0.60 ATR (60 bps <= 65 bps ref) vs $1.30 ATR (130 bps)
        let trend_calm = sol_cfg.volatility_adjusted_trend_notional_cap(534.0, 100.0, 0.60);
        let trend_storm = sol_cfg.volatility_adjusted_trend_notional_cap(534.0, 100.0, 1.30);
        assert!(trend_calm >= base_cap);
        assert!((trend_storm - 113.0).abs() <= 2.0);
    }

    #[test]
    fn test_macro_calendar_hazard_fomc_cpi_nfp_pce_and_dst() {
        use chrono::{TimeZone, Utc};
        // 1. US DST transitions in 2026: EDT starts March 8, 2026 and ends Nov 1, 2026
        assert!(!is_us_daylight_saving_date(2026, 3, 7));
        assert!(is_us_daylight_saving_date(2026, 3, 8));
        assert!(is_us_daylight_saving_date(2026, 10, 30));
        assert!(!is_us_daylight_saving_date(2026, 11, 4));

        // 2. Friday Oct 30, 2026 (Last Friday of Oct -> US Core PCE at 08:30 EDT = 12:30 UTC):
        // At 12:20 UTC (T - 10m): hard blackout (lambda = 1.0)
        let pce_pre_ms = Utc
            .with_ymd_and_hms(2026, 10, 30, 12, 20, 0)
            .unwrap()
            .timestamp_millis() as u64;
        let h_pce = evaluate_macro_hazard(pce_pre_ms);
        assert!(h_pce.is_hard_blackout);
        assert_eq!(h_pce.event_name, "US_CORE_PCE");
        assert!((h_pce.lambda_exo - 1.0).abs() < 1e-6);

        // At 13:00 UTC (T + 30m, 15m after 12:45 plateau = 1 half-life): lambda = 0.50 (soft band expansion, not hard blackout)
        let pce_decay_ms = Utc
            .with_ymd_and_hms(2026, 10, 30, 13, 0, 0)
            .unwrap()
            .timestamp_millis() as u64;
        let h_decay = evaluate_macro_hazard(pce_decay_ms);
        assert!(!h_decay.is_hard_blackout);
        assert!((h_decay.lambda_exo - 0.50).abs() < 0.02);

        // 3. Wednesday Nov 4, 2026 (FOMC Decision in EST -> 14:00 EST = 19:00 UTC):
        // At 19:45 UTC (T + 45m, during Powell Press Conference): still hard blackout (60m plateau)!
        let fomc_powell_ms = Utc
            .with_ymd_and_hms(2026, 11, 4, 19, 45, 0)
            .unwrap()
            .timestamp_millis() as u64;
        let h_fomc = evaluate_macro_hazard(fomc_powell_ms);
        assert!(h_fomc.is_hard_blackout);
        assert_eq!(h_fomc.event_name, "US_FOMC_DECISION");
    }

    #[test]
    fn test_adaptive_tp_shave_and_engine_c_and_d_evaluators() {
        // 1. Fast half-life (20m) shaves 5.0 bps on 80 bps excursion; slow half-life (110m) shaves more (~9.0 bps)
        let fast_shave = compute_adaptive_tp_offset_bps(100.80, 100.0, 0.01, 20.0).unwrap();
        let slow_shave = compute_adaptive_tp_offset_bps(100.80, 100.0, 0.01, 110.0).unwrap();
        assert!((fast_shave - 5.0).abs() < 1e-6);
        assert!(slow_shave > fast_shave && slow_shave <= 10.0);

        // 2. Kelly scalar scales down borderline edge and gives 1.0x to strong excursions
        let k_low = compute_fee_adjusted_kelly_scalar(26.0, 28.0, 1.5, 0.0);
        let k_high = compute_fee_adjusted_kelly_scalar(48.0, 28.0, 1.5, 0.0);
        assert!(k_low < 0.90 && k_low >= 0.65);
        assert!((k_high - 1.0).abs() < 1e-6);

        // 3. Engine C (HourlySqueezeHarvester): fires at minute :48 UTC with -8 bps/hr funding and absorption
        let ts_min_48 = 48 * 60_000;
        let sig_c = evaluate_engine_c_hourly_squeeze(
            ts_min_48, -0.0008, // -8.0 bps/hr
            99.80, 99.79, 100.20, // oracle above perp
            99.50,  // 30m low
            0.0035, 28.0, -0.8, -1.0, 0.01,
        );
        assert!(
            sig_c.is_some(),
            "Engine C must fire during :42-:55 UTC pre-settlement window"
        );
        assert!(evaluate_engine_c_hourly_squeeze(
            15 * 60_000, // minute :15 UTC -> outside window
            -0.0008,
            99.80,
            99.79,
            100.20,
            99.50,
            0.0035,
            28.0,
            -0.8,
            -1.0,
            0.01,
        )
        .is_none());

        // 4. Engine D (CascadeAbsorptionReversal): fires on severe shock + OI destruction + oracle discount + OBI flip
        let sig_d = evaluate_engine_d_cascade_reversal(
            99.50, 99.35, 99.20, 99.49, 99.90,  // -40 bps discount to CEX spot oracle
            -0.025, // -2.5% OI destruction in 3m
            -3.20,  // Z_15 = -3.20 shock
            0.35,   // strong bid refill OBI
            28.0, 0.01,
        );
        assert!(
            sig_d.is_some(),
            "Engine D must fire when all 4 HLP cascade exhaustion gates align"
        );
    }

    #[test]
    fn test_ou_scale_function_and_first_passage_hitting_probabilities() {
        // Verify odd symmetry and monotonicity of O-U scale function S(z)
        assert!((ou_scale_function(0.0)).abs() < 1e-12);
        let s_pos = ou_scale_function(1.80);
        let s_neg = ou_scale_function(-1.80);
        assert!((s_pos + s_neg).abs() < 1e-10);
        assert!(ou_scale_function(2.50) > s_pos);

        // Verify exact First-Passage-Time table from Section 2.1 of Research Report:
        // Entry z_0 = -1.80, Stop z_SL = -2.50:
        // - Target z_TP = 0.00 (Strict Mean) -> ~69.33% hitting probability
        // - Target z_TP = +0.50 (Optimal Runner) -> ~66.30% hitting probability (only ~3.03% drop for +27.78% gross lift!)
        let p_mean = ou_first_passage_prob(-1.80, 0.00, -2.50);
        let p_runner = ou_first_passage_prob(-1.80, 0.50, -2.50);
        assert!(
            (p_mean - 0.6933).abs() < 0.005,
            "Expected ~69.33% hitting prob at mean, got {:.4}",
            p_mean
        );
        assert!(
            (p_runner - 0.6630).abs() < 0.005,
            "Expected ~66.30% hitting prob at +0.50 overshoot, got {:.4}",
            p_runner
        );
        assert!((p_mean - p_runner) < 0.035);
    }

    #[test]
    fn test_dynamic_entry_frontier_relaxes_in_sideways_and_tightens_in_trend() {
        let btc_cal = asset_calibration("BTC");
        let calm_regime = MicroRegimeState {
            hurst_60m: 0.34,
            ker_30m: 0.18,
            gk_efficiency_15m: 0.25,
            kalman_level: 80_000.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0008,
            sigma_eq: 0.0022,
            z_macro: 0.0,
        };
        assert!(calm_regime.phi_regime() <= 0.18);
        let sig_calm = calm_regime.dynamic_entry_sigma(&btc_cal, Direction::Long);
        assert!(
            sig_calm >= 1.45 && sig_calm <= 1.66,
            "Expected calm sideways sigma in [1.45, 1.66], got {:.3}",
            sig_calm
        );

        let storm_regime = MicroRegimeState {
            hurst_60m: 0.68,
            ker_30m: 0.72,
            gk_efficiency_15m: 0.85,
            kalman_level: 80_000.0_f64.ln(),
            kalman_slope: -0.0012,
            sigma_1m: 0.0008,
            sigma_eq: 0.0022,
            z_macro: -1.8,
        };
        assert!((storm_regime.phi_regime() - 1.0).abs() < 1e-6);
        let sig_storm = storm_regime.dynamic_entry_sigma(&btc_cal, Direction::Long);
        assert!(
            sig_storm >= 2.15,
            "Expected turbulent adverse trend sigma >= 2.15, got {:.3}",
            sig_storm
        );
    }

    #[test]
    fn test_continuous_directional_bias_multiplier_neutralizes_parabolic_near_trap() {
        let near_cal = asset_calibration("NEAR");
        // Parabolic runaway pump on NEAR (Z_macro = +1.90): shorting (d = -1) gives u_t = -1.90 <= u_veto (-1.85) -> 0.0!
        let pump_regime = MicroRegimeState {
            hurst_60m: 0.55,
            ker_30m: 0.50,
            gk_efficiency_15m: 0.60,
            kalman_level: 3.65_f64.ln(),
            kalman_slope: 0.001,
            sigma_1m: 0.0015,
            sigma_eq: 0.0038,
            z_macro: 1.90,
        };
        let mult_short = pump_regime.directional_bias_multiplier(&near_cal, Direction::Short);
        let mult_long = pump_regime.directional_bias_multiplier(&near_cal, Direction::Long);
        assert_eq!(
            mult_short, 0.0,
            "Counter-trend short during parabolic NEAR pump must be hard-vetoed (0.0)"
        );
        assert!(
            mult_long >= 0.90,
            "Trend-aligned dip buy must receive >= 90% size allocation, got {:.3}",
            mult_long
        );
    }

    #[test]
    fn test_asymmetric_two_tranche_exit_ratchets_lock_floor_and_harvests_overshoot() {
        let sol_cal = asset_calibration("SOL");
        let regime = MicroRegimeState {
            hurst_60m: 0.35,
            ker_30m: 0.20,
            gk_efficiency_15m: 0.30,
            kalman_level: 100.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0010,
            sigma_eq: 0.0040, // 40 bps sigma_eq
            z_macro: 0.0,
        };
        // Enter Long at -1.80 sigma_eq ($99.2826)
        let entry_px = 100.0 * (-1.80 * 0.0040_f64).exp();
        let mut mgr = AsymmetricExitManager::new(Direction::Long, entry_px, -1.80, 0.0040);

        // Step 1: Price reaches Kalman equilibrium mean ($100.00) -> fills Tranche 1 and jumps stop to Lock Floor!
        let sig1 = mgr.on_bar_or_sentinel_tick(100.00, &regime, &sol_cal, 0.2, 0.2, 0.15);
        match sig1 {
            ExitSignal::FillTranche1AndRatchet {
                tp2_price,
                new_lock_sl_price,
            } => {
                assert!(mgr.tranche1_filled);
                assert!(
                    new_lock_sl_price > entry_px * 1.0011,
                    "Lock-in floor must be strictly above entry + 12 bps"
                );
                assert!(
                    tp2_price > 100.20,
                    "Tranche 2 overshoot target must lie in the opposite band above mean"
                );
            }
            other => panic!("Expected FillTranche1AndRatchet, got {:?}", other),
        }

        // Step 2: Price overshoots into opposite band ($100.25) -> fills Tranche 2!
        let sig2 = mgr.on_bar_or_sentinel_tick(100.25, &regime, &sol_cal, 0.3, 0.3, 0.20);
        assert_eq!(sig2, ExitSignal::FillTranche2Complete);
        assert!(mgr.tranche2_filled);
    }

    #[test]
    fn test_sprt_changepoint_stop_contracts_and_terminates_waterfall_within_3_bars() {
        let near_cal = asset_calibration("NEAR");
        let regime = MicroRegimeState {
            hurst_60m: 0.45,
            ker_30m: 0.35,
            gk_efficiency_15m: 0.40,
            kalman_level: 100.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0015,
            sigma_eq: 0.0040,
            z_macro: 0.0,
        };
        let entry_px = 100.0 * (1.80 * 0.0040_f64).exp();
        let mut mgr = AsymmetricExitManager::new(Direction::Short, entry_px, 1.80, 0.0040);

        // Inject 3 consecutive adverse upward innovations (+2.2 sigma) + bullish OBI (+0.50) against Short
        let mut triggered = false;
        for step in 1..=3 {
            let px = entry_px * (1.0 + 0.0015 * (step as f64));
            let sig = mgr.on_bar_or_sentinel_tick(px, &regime, &near_cal, 2.2, 2.0, 0.50);
            if let ExitSignal::StopOutFullSprtBreak {
                sprt_stat,
                avg_adverse_innov_3m,
            } = sig
            {
                assert!(sprt_stat >= near_cal.sprt_b_upper);
                assert!(avg_adverse_innov_3m >= 1.40);
                triggered = true;
                break;
            }
        }
        assert!(
            triggered,
            "SPRT must detect structural breakout against Short within 3 bars"
        );
    }

    #[test]
    fn test_kelly_tranche_allocation_and_1m_wick_filtered_sprt() {
        let btc_cal = asset_calibration("BTC");
        let calm_regime = MicroRegimeState {
            hurst_60m: 0.30,
            ker_30m: 0.15,
            gk_efficiency_15m: 0.25,
            kalman_level: 100.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0008,
            sigma_eq: 0.0030,
            z_macro: 0.0,
        };
        let alloc_calm = calm_regime.compute_optimal_tranche_split(&btc_cal, 1.80);
        assert!(
            alloc_calm.w2_runner_fraction > 0.35 && alloc_calm.w2_runner_fraction <= 0.65,
            "Anti-persistent regime must tilt allocation toward Tranche 2 runner, got {:.3}",
            alloc_calm.w2_runner_fraction
        );
        assert!((alloc_calm.w1_mean_fraction + alloc_calm.w2_runner_fraction - 1.0).abs() < 1e-9);

        let noisy_regime = MicroRegimeState {
            hurst_60m: 0.62,
            ker_30m: 0.65,
            gk_efficiency_15m: 0.80,
            kalman_level: 100.0_f64.ln(),
            kalman_slope: 0.0,
            sigma_1m: 0.0008,
            sigma_eq: 0.0030,
            z_macro: 0.0,
        };
        let alloc_noisy = noisy_regime.compute_optimal_tranche_split(&btc_cal, 1.80);
        assert!(
            (alloc_noisy.w2_runner_fraction - 0.35).abs() < 1e-6,
            "Noisy trend regime must clamp Tranche 2 runner to defensive 35% floor"
        );

        // Verify 1m Wick-Absorption Deflator (\psi_{wick} = 0.15) ignores a 1m lower wick that closes green!
        let entry_px = 100.0 * (-1.80 * 0.0030_f64).exp();
        let mut mgr = AsymmetricExitManager::new_with_alloc(
            Direction::Long,
            entry_px,
            -1.80,
            0.0030,
            &btc_cal,
            alloc_calm,
        );
        // Bar wicks down -45 bps to 99.01, but rebounds to close green above open (99.48 > 99.45)
        let wick_tick = OneMinBarTick {
            open: 99.45,
            high: 99.50,
            low: 99.01,
            close: 99.48,
            std_kalman_innov_1m: -2.0,
            ou_step_innov_1m: -2.0,
            smoothed_obi: 0.25,
        };
        let sig_wick = mgr.on_1m_bar(&wick_tick, &calm_regime, &btc_cal, 30.0);
        assert!(
            matches!(sig_wick, ExitSignal::Hold { .. }),
            "1m absorbed wick closing green must be deflated by psi_wick=0.15 and NOT stop out!"
        );
    }

    #[test]
    fn test_stall_hazard_maker_policy_and_siegmund_cusum_and_touch_absorption() {
        let sol_cal = asset_calibration("SOL");

        // 1. Verify R_touch post-touch rebound absorption
        assert!(
            !verify_maker_touch_absorption(
                Direction::Long,
                100.00,
                100.10,
                99.50,
                99.50, // closes on dead low -> falling knife!
                0.20,
                0.0,
                &sol_cal,
            ),
            "Falling knife closing on dead low must be rejected by R_touch"
        );
        assert!(
            verify_maker_touch_absorption(
                Direction::Long,
                100.00,
                100.10,
                99.80,
                99.96, // dips to 99.80 and rebounds to 99.96 -> absorbed!
                0.20,
                0.05,
                &sol_cal,
            ),
            "Intrabar bounce off low must pass R_touch absorption check"
        );

        // 2. Verify StallHazardExitManager keeps TP1 at full Kalman mean during active expansion,
        // then ratchets to Upper-Quartile MFE Re-Touch Price after tau_stall >= 3 mins!
        let mut stall_mgr = StallHazardExitManager::new(Direction::Long, 100.00, 42.0);
        // Minute 1: expands to +18 bps (100.18), Kalman mean is at 100.35 (+35 bps)
        let d1 = stall_mgr.on_1m_bar(
            100.00, 100.18, 99.98, 100.16, 100.00, 100.35, 25.0, 0.5, 0.25, 1.5, &sol_cal,
        );
        match d1 {
            HardenedExitDecision::Hold {
                tp1_maker_px: Some(tp1),
                ..
            } => {
                assert!(
                    (tp1 - 100.35).abs() < 1e-4,
                    "During active expansion, TP1 must remain unshaved at full Kalman mean 100.35, got {:.4}",
                    tp1
                );
            }
            other => panic!("Expected Hold at full mean, got {:?}", other),
        }
        // Minutes 2, 3, 4: trade stalls below 100.18 with waning momentum -> tau_stall reaches 3 mins!
        let _ = stall_mgr.on_1m_bar(
            100.16, 100.17, 100.10, 100.12, 100.16, 100.35, 25.0, -0.2, -0.05, 1.5, &sol_cal,
        );
        let _ = stall_mgr.on_1m_bar(
            100.12, 100.14, 100.08, 100.10, 100.12, 100.35, 25.0, -0.2, -0.05, 1.5, &sol_cal,
        );
        let d4 = stall_mgr.on_1m_bar(
            100.10, 100.12, 100.07, 100.08, 100.10, 100.35, 25.0, -0.3, -0.10, 1.5, &sol_cal,
        );
        match d4 {
            HardenedExitDecision::Hold {
                tp1_maker_px: Some(tp1_stalled),
                ..
            } => {
                assert!(
                    tp1_stalled < 100.20 && tp1_stalled > 100.12,
                    "After 3m stall, TP1 must ratchet down to upper-quartile of MFE (~100.142), got {:.4}",
                    tp1_stalled
                );
            }
            other => panic!("Expected Hold with ratcheted TP1, got {:?}", other),
        }
    }

    #[test]
    fn test_hip3_session_schedule_and_slot_arbiter() {
        let nvda_cfg = hip3_asset_config("xyz:NVDA").expect("NVDA config");
        assert_eq!(nvda_cfg.asset_class, AssetClass::UsEquityHip3);
        assert_eq!(nvda_cfg.dex, ClearinghouseDex::Hip3Xyz);
        assert_eq!(nvda_cfg.sz_decimals, 3);
        assert!(nvda_cfg.growth_mode_enabled);

        // 1. Fee calculation: growthMode enabled slashes fees by ~80-90%
        let (mm_fee, mt_fee) = nvda_cfg.effective_fees_bps();
        assert!(
            mm_fee < 1.0,
            "Growth mode maker-maker fee must be < 1 bps, got {:.3}",
            mm_fee
        );
        assert!(
            mt_fee < 3.0,
            "Growth mode maker-taker fee must be < 3 bps, got {:.3}",
            mt_fee
        );

        // 2. US Equity Session Schedule:
        // Weekday 2 (Tuesday):
        // 14:00 UTC (840 min) -> Closed (Pre-market)
        let s_pre = nvda_cfg.schedule.evaluate_session(840 * 60, 2);
        assert_eq!(s_pre, SessionState::Closed);

        // 14:32 UTC (872 min) -> Closed (First 5m auction stabilization)
        let s_auc = nvda_cfg.schedule.evaluate_session(872 * 60, 2);
        assert_eq!(s_auc, SessionState::Closed);

        // 15:00 UTC (900 min) -> ActiveOpeningMomentum (14:35 - 15:30 UTC)
        let s_orb = nvda_cfg.schedule.evaluate_session(900 * 60, 2);
        assert_eq!(s_orb, SessionState::ActiveOpeningMomentum);

        // 16:30 UTC (990 min) -> ActiveNormal (Midday stationary core)
        let s_mid = nvda_cfg.schedule.evaluate_session(990 * 60, 2);
        assert_eq!(s_mid, SessionState::ActiveNormal);

        // 20:38 UTC (1238 min) -> EntryFrozenHoldOnly (close - 25m)
        let s_freeze = nvda_cfg.schedule.evaluate_session(1238 * 60, 2);
        assert_eq!(s_freeze, SessionState::EntryFrozenHoldOnly);

        // 20:52 UTC (1252 min) -> MandatoryMakerFlatten (close - 10m)
        let s_flat_maker = nvda_cfg.schedule.evaluate_session(1252 * 60, 2);
        assert_eq!(s_flat_maker, SessionState::MandatoryMakerFlatten);

        // 20:56 UTC (1256 min) -> MandatoryHardFlat (close - 5m)
        let s_flat_hard = nvda_cfg.schedule.evaluate_session(1256 * 60, 2);
        assert_eq!(s_flat_hard, SessionState::MandatoryHardFlat);

        // Weekend (Saturday, weekday 6) -> Closed
        let s_wknd = nvda_cfg.schedule.evaluate_session(990 * 60, 6);
        assert_eq!(s_wknd, SessionState::Closed);

        // 3. Systematic macro veto:
        // xyz:NVDA Long (dir = +1.0) vetoed when xyz:XYZ100 is plunging (Z_15m <= -1.65)
        assert!(!passes_systematic_lead_lag_filter(
            &nvda_cfg, 1.0, 0.0, -1.85, 0.0, 0.0, 0.0, 0.0
        ));
        // Passes when XYZ100 is healthy (Z_15m = 0.20)
        assert!(passes_systematic_lead_lag_filter(
            &nvda_cfg, 1.0, 0.0, 0.20, 0.0, 0.0, 0.0, 0.0
        ));

        // 4. DualClearinghouseSlotArbiter:
        let mut arbiter = DualClearinghouseSlotArbiter::new(175.0);
        let zec_cfg = hip3_asset_config("ZEC").expect("ZEC config");

        let cands = vec![
            CandidateSignal {
                cfg: Box::leak(Box::new(nvda_cfg)),
                dir_sign: 1.0,
                expected_gross_bps: 22.0,
                sigma_eq_bps: 20.0,
                lambda_dir: 1.0,
                spread_bps: 0.60,
                top5_depth_usdc: 300_000.0,
                corr_with_active_slot: 0.10,
            },
            CandidateSignal {
                cfg: Box::leak(Box::new(zec_cfg)),
                dir_sign: 1.0,
                expected_gross_bps: 26.0,
                sigma_eq_bps: 24.0,
                lambda_dir: 1.0,
                spread_bps: 1.20,
                top5_depth_usdc: 250_000.0,
                corr_with_active_slot: 0.15,
            },
        ];

        // Midday Tuesday: both slots empty -> picks candidate with highest risk-adjusted edge
        let chosen = arbiter.select_next_execution(&cands, 990 * 60, 2);
        assert!(chosen.is_some());

        // Fill xyz slot: only NativeMain candidate can now be selected
        arbiter.xyz_slot_active = true;
        let chosen_slot2_full = arbiter.select_next_execution(&cands, 990 * 60, 2);
        assert!(chosen_slot2_full.is_some());
        assert_eq!(
            chosen_slot2_full.unwrap().cfg.dex,
            ClearinghouseDex::NativeMain
        );
    }

    #[test]
    fn test_asset_fee_schedule_and_dst_session_lifecycle_gate() {
        // 1. Verified Hyperliquid Mainnet Fee Schedule
        let gold_fee = AssetFeeSchedule::static_fallback_for_symbol("XYZ:GOLD");
        assert!((gold_fee.maker_maker_rt_bps() - 5.76).abs() < 1e-6);
        assert!((gold_fee.maker_taker_rt_bps() - 12.12).abs() < 1e-6);

        let silver_fee = AssetFeeSchedule::from_hip3_meta(true, 1.0);
        assert!((silver_fee.maker_maker_rt_bps() - 0.576).abs() < 1e-6);
        assert!((silver_fee.maker_taker_rt_bps() - 1.652).abs() < 1e-6);

        let btc_fee = AssetFeeSchedule::static_fallback_for_symbol("BTC");
        assert!((btc_fee.maker_maker_rt_bps() - 2.88).abs() < 1e-6);
        assert!((btc_fee.maker_taker_rt_bps() - 6.26).abs() < 1e-6);

        // 2. DST-Aware US Equity Session Lifecycle Gate:
        use chrono::{TimeZone, Utc};
        // EST (Nov 11, 2026): RTH 14:30 - 21:00 UTC
        let ts_est_open_auction = Utc
            .with_ymd_and_hms(2026, 11, 11, 14, 32, 0)
            .unwrap()
            .timestamp_millis();
        let ts_est_active = Utc
            .with_ymd_and_hms(2026, 11, 11, 15, 30, 0)
            .unwrap()
            .timestamp_millis();
        let ts_est_preclose = Utc
            .with_ymd_and_hms(2026, 11, 11, 20, 45, 0)
            .unwrap()
            .timestamp_millis();
        let ts_est_flat = Utc
            .with_ymd_and_hms(2026, 11, 11, 20, 58, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:NVDA", ts_est_open_auction),
            SessionLifecycleAction::BlockNewEntriesAndRatchetTp("US_EQUITY_OPEN_AUCTION_5M")
        );
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:NVDA", ts_est_active),
            SessionLifecycleAction::AllowNormalTrading
        );
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:NVDA", ts_est_preclose),
            SessionLifecycleAction::BlockNewEntriesAndRatchetTp("US_EQUITY_PRE_CLOSE_20M")
        );
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:NVDA", ts_est_flat),
            SessionLifecycleAction::ForceFlat("US_EQUITY_RTH_CLOSE")
        );

        // EDT (July 15, 2026): RTH 13:30 - 20:00 UTC
        let ts_edt_active = Utc
            .with_ymd_and_hms(2026, 7, 15, 13, 45, 0)
            .unwrap()
            .timestamp_millis();
        let ts_edt_flat = Utc
            .with_ymd_and_hms(2026, 7, 15, 19, 58, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:TSLA", ts_edt_active),
            SessionLifecycleAction::AllowNormalTrading
        );
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:TSLA", ts_edt_flat),
            SessionLifecycleAction::ForceFlat("US_EQUITY_RTH_CLOSE")
        );

        // CME Metal maintenance: 17:00-18:00 ET (EST = 22:00-23:00 UTC)
        let ts_cme_maint = Utc
            .with_ymd_and_hms(2026, 11, 11, 22, 10, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            evaluate_session_lifecycle_gate("xyz:GOLD", ts_cme_maint),
            SessionLifecycleAction::ForceFlat("CME_DAILY_MAINTENANCE")
        );

        // Native Crypto always AllowNormalTrading
        assert_eq!(
            evaluate_session_lifecycle_gate("BTC", ts_est_flat),
            SessionLifecycleAction::AllowNormalTrading
        );

        // 3. F-07 Engine A Disaster Stop decoupling (> min_soft_inv_bps + 35 bps, >= 85 bps)
        let btc_sl_dist = compute_engine_a_disaster_stop_distance("BTC", 70_000.0, 70_175.0); // 25 bps TP
        let btc_sl_bps = (btc_sl_dist / 70_000.0) * 1e4;
        assert!(
            btc_sl_bps >= 85.0 - 1e-6,
            "BTC disaster stop must be >= 85 bps so 5m invalidation (45 bps) runs first; got {:.1}",
            btc_sl_bps
        );

        // 4. Post-Package2 calibration: ZEC step-trend window [96 bars, 10..135m] & GOLD tail-only (>=2.05σ)
        let zec_ou = asset_ou_config("ZEC");
        assert_eq!(zec_ou.lookback_5m_bars, 96);
        assert!((zec_ou.min_half_life_m - 10.0).abs() < 1e-9);
        assert!((zec_ou.max_half_life_m - 135.0).abs() < 1e-9);
        let gold_ou = asset_ou_config("xyz:GOLD");
        assert!(gold_ou.base_entry_sigma >= 2.05 - 1e-9);
        assert!(gold_ou.min_net_edge_bps >= 26.0 - 1e-9);
        assert!(gold_ou.min_sigma_eq_bps >= 16.0 - 1e-9);
    }
}
