use std::collections::VecDeque;
use std::f64;

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
        let pred_r_alt =
            phi[0] * self.theta[0] + phi[1] * self.theta[1] + phi[2] * self.theta[2];
        let u_innov = r_alt - pred_r_alt;

        // 2. Huber clipping for RLS parameter stability against liquidation wicks
        let sigma_u = self.idio_ret_var.sqrt().max(1e-5);
        let u_clipped = u_innov.clamp(-3.5 * sigma_u, 3.5 * sigma_u);

        // 3. Compute P_{t-1} * phi
        let mut p_phi = [0.0; 3];
        for i in 0..3 {
            p_phi[i] = self.p_mat[i][0] * phi[0]
                + self.p_mat[i][1] * phi[1]
                + self.p_mat[i][2] * phi[2];
        }

        // Denominator: lambda + phi^T * P_{t-1} * phi
        let denom = (self.lambda
            + phi[0] * p_phi[0]
            + phi[1] * p_phi[1]
            + phi[2] * p_phi[2])
            .max(1e-12);

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
            phi_t_p[j] = phi[0] * self.p_mat[0][j]
                + phi[1] * self.p_mat[1][j]
                + phi[2] * self.p_mat[2][j];
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
    let tp_offset_bps = compute_safe_tp_offset_bps(mean_target_price, bar_close, tick_size).unwrap_or(0.0);
    let c_req = (min_net_edge_bps + 2.0 * maker_fee_bps + expected_funding_drag_bps.max(0.0) + tp_offset_bps) * 1e-4;
    let m_star = mean_target_price.ln();

    let eff_bid = if best_bid > 0.0 { best_bid } else { bar_close };
    let eff_ask = if best_ask > 0.0 { best_ask } else { bar_close + tick_size };

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

// ============================================================================
// 3. TWO-TIER POSITION INVALIDATION LOGIC (SOFT MULTI-BAR + HARD CRASH STOP)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InvalidationDecision {
    HoldPosition {
        updated_tp_price: Option<f64>,
    },
    TriggerSoftInvalidationFlatten {
        reason: &'static str,
    },
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

        // 4. Time-Decay Ratchet: tighten Maker TP to fee-adjusted breakeven at 1.8 * t_{1/2},
        //    and force flatten at min(3.0 * t_{1/2}, 360.0) if mean reversion has stalled.
        let max_hold_mins = (3.0 * self.half_life_minutes).clamp(60.0, 360.0);
        if self.elapsed_minutes >= max_hold_mins {
            return InvalidationDecision::TriggerTimeDecayExit;
        }

        let mut updated_tp_price = None;
        let breakeven_ratchet_mins = (1.8 * self.half_life_minutes).clamp(36.0, 216.0);
        if !self.breakeven_ratchet_armed && self.elapsed_minutes >= breakeven_ratchet_mins {
            self.breakeven_ratchet_armed = true;
            let round_trip_fee = 2.0 * maker_fee_bps * 1e-4;
            let be_px = match self.side {
                Side::Long => {
                    let raw = self.entry_price * (1.0 + round_trip_fee);
                    (raw / tick_size.max(1e-9)).ceil() * tick_size.max(1e-9)
                }
                Side::Short => {
                    let raw = self.entry_price * (1.0 - round_trip_fee);
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
    let candidate_windows = [cfg.lookback_5m_bars.min(n_5m), 72usize.min(n_5m)];
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
                    let sigma_eq_raw_log =
                        ((params.sigma / params.mu.max(1e-9)).powi(2) / (2.0 * params.theta)).sqrt();
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
pub fn calculate_thresholds_at_sigma(params: &OuParameters, transaction_cost: f64, num_sigma: f64) -> OptimalThresholds {
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
pub fn calculate_optimal_thresholds(params: &OuParameters, transaction_cost: f64) -> OptimalThresholds {
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
    if !is_weekend && (13..17).contains(&utc_hour) {
        0.10
    } else if is_weekend || utc_hour < 8 {
        -0.05
    } else {
        0.0
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
pub fn calculate_rogers_satchell_sigma(opens: &[f64], highs: &[f64], lows: &[f64], closes: &[f64]) -> f64 {
    let n = opens.len().min(highs.len()).min(lows.len()).min(closes.len());
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
pub fn calculate_garman_klass_sigma(opens: &[f64], highs: &[f64], lows: &[f64], closes: &[f64]) -> f64 {
    let n = opens.len().min(highs.len()).min(lows.len()).min(closes.len());
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
}

#[allow(dead_code)]
pub fn calculate_features(closes_history: &[f64], eval_close: f64, symbol: &str) -> Option<FeatureSnapshot> {
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
            if hourly_funding_rate.is_finite() && hourly_funding_rate.abs() > 1e-7 && ou_params.theta > 1e-5 {
                let carry_shift = (-hourly_funding_rate / (60.0 * ou_params.theta)).clamp(-0.0015, 0.0015);
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
                    let now_utc = chrono::Utc::now();
                    let is_weekend = matches!(now_utc.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
                    utc_session_sigma_offset(now_utc.hour(), is_weekend)
                } else {
                    0.0
                };
                let th = calculate_carry_adjusted_thresholds(
                    &ou_params,
                    tx_cost,
                    cfg.base_entry_sigma + session_offset,
                    hourly_funding_rate,
                    cfg.min_entry_sigma,
                    cfg.max_entry_sigma,
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
        "BTC" => 0.020,
        "ETH" => 0.025,
        "SOL" => 0.040,
        "AVAX" => 0.050,
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
        "BTC" => (0.0075, 0.0150),
        "ETH" => (0.0100, 0.0200),
        "SOL" => (0.0150, 0.0300),
        "AVAX" => (0.0200, 0.0400),
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
    let is_valid_half_life = half_life_m >= cfg.min_half_life_m && half_life_m <= cfg.max_half_life_m;

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
        sigma_eq_log,
        sigma_1m_prev: sigma_mad,
        kalman_std_slope: llt_out.standardized_slope,
        kalman_slope: llt_out.updated_slope,
        kalman_drift_veto: llt_out.drift_veto_active,
        cusum_pos: llt_out.cusum_pos,
        cusum_neg: llt_out.cusum_neg,
    })
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
        assert!(!feat_quiet.is_upside_shock, "28 bps move must not trigger shock veto despite high Z!");

        let feat_real_shock = calculate_features(&history, 101.50, "BTC").expect("features");
        assert!(feat_real_shock.z_15 > 3.0);
        assert!(feat_real_shock.is_upside_shock, "150 bps flash move must trigger shock veto!");

        let feat_dump = calculate_features(&history, 98.00, "BTC").expect("features");
        assert!(feat_dump.z_15 < -3.0);
        assert!(feat_dump.is_downside_shock, "-200 bps flash crash must trigger downside shock!");
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
        let pos_fund = calculate_carry_adjusted_thresholds(&params, tx_cost, 1.65, 0.0005, 1.45, 1.95);
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
        assert!(feat.is_stationary, "5m stationary process must be stationary");
        assert!(feat.is_valid_half_life, "t_half in 20m-135m band must pass is_valid_half_life");
        assert!(feat.is_consolidation, "Non-drifting stationary series must be valid consolidation");
        assert!(feat.sigma_eq_log >= 0.0055 - 1e-6, "SOL sigma_eq_log must respect 55 bps floor");
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
        assert!(veto_fired, "Kalman LLT filter must trigger waterfall drift veto during steep sell-off");
        assert!(llt.cusum_neg > 2.0, "Downside innovation CUSUM must accumulate during waterfall");
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
            OrderPlacementAction::PostAloJoinBook { side, limit_price, entry_ttl_mins, .. } => {
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
            40.0,  // 40m half-life
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
        assert!(matches!(d4, InvalidationDecision::TriggerSoftInvalidationFlatten { .. }));
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
}
