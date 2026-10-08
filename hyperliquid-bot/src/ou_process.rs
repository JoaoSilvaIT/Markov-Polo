use std::f64;

/// Represents the mathematical parameters of an Ornstein-Uhlenbeck stochastic process.
#[derive(Debug, Clone)]
pub struct OuParameters {
    pub theta: f64, // Mean-reversion speed (how aggressively it pulls back to the mean)
    pub mu: f64,    // Equilibrium mean (the long-term average price)
    pub sigma: f64, // Volatility (amplitude of the random Wiener process noise)
}

impl OuParameters {
    pub fn half_life_minutes(&self) -> f64 {
        if self.theta > 1e-6 {
            (2.0_f64.ln()) / self.theta
        } else {
            120.0
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
    let mut sum_xx = 0.0;
    let mut sum_xy = 0.0;
    
    for i in 0..n {
        let x = data[i];
        let y = data[i+1];
        sum_x += x;
        sum_y += y;
        sum_xx += x * x;
        sum_xy += x * y;
    }
    
    let nf = n as f64;
    let mean_x = sum_x / nf;
    let mean_y = sum_y / nf;
    
    let cov_xy = (sum_xy / nf) - (mean_x * mean_y);
    let var_x = (sum_xx / nf) - (mean_x * mean_x);
    
    if var_x <= 0.0 {
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
        let y = data[i+1];
        let err = y - (a + b * x);
        sum_err_sq += err * err;
    }
    
    let var_err = sum_err_sq / (nf - 2.0).max(1.0);
    let sigma_eps = var_err.sqrt();
    
    Some((a, b, sigma_eps))
}

/// Estimates the Ornstein-Uhlenbeck parameters using Ordinary Least Squares (OLS)
/// applied to an exact Autoregressive AR(1) discretization with Phillips & Yu (2005)
/// Jackknife bias reduction to eliminate the Hurwicz upward bias on theta.
/// 
/// `data`: The rolling window of price data (e.g., 480 1-minute close prices).
/// `dt`: The discrete time step (e.g., 1.0 for 1-minute bars).
pub fn estimate_ou_parameters(data: &[f64], dt: f64) -> Option<OuParameters> {
    let (a, b, sigma_eps) = estimate_ar1_raw(data)?;
    
    let theta_full = -b.ln() / dt;
    
    // Apply Phillips & Yu (2005) 2-split Jackknife bias correction:
    // theta_jack = 2 * theta_full - 0.5 * (theta_1 + theta_2)
    // This removes the O(1/T) Hurwicz bias that artificially inflates mean-reversion speed.
    let mid = data.len() / 2;
    let theta = if mid >= 4 {
        let part1 = estimate_ar1_raw(&data[..mid]);
        let part2 = estimate_ar1_raw(&data[mid..]);
        
        if let (Some((_, b1, _)), Some((_, b2, _))) = (part1, part2) {
            let theta1 = -b1.ln() / dt;
            let theta2 = -b2.ln() / dt;
            let th_jack = 2.0 * theta_full - 0.5 * (theta1 + theta2);
            if th_jack > 1e-5 {
                th_jack
            } else {
                theta_full
            }
        } else {
            theta_full
        }
    } else {
        theta_full
    };
    
    let mu = a / (1.0 - b);
    let sigma = sigma_eps * ((2.0 * theta) / (1.0 - b * b)).sqrt();
    
    Some(OuParameters { theta, mu, sigma })
}

/// Calculates the optimal entry and exit boundaries using a closed-form approximation 
/// derived from the Bertram Optimization Model for Statistical Arbitrage.
/// Maximizes the expected return per unit of time based on mean-reversion speed.
pub fn calculate_optimal_thresholds(params: &OuParameters, transaction_cost: f64) -> OptimalThresholds {
    // The equilibrium variance of the stationary O-U distribution is sigma^2 / (2 * theta).
    // This gives us the standard deviation (noise band) of the process.
    let sigma_eq = (params.sigma * params.sigma / (2.0 * params.theta)).sqrt();
    
    // Bertram's exact closed-form calculus requires numerical root finding for the absolute maximum, 
    // but a highly robust analytic approximation scales the threshold based on the cost-to-noise ratio.
    // If costs are high, the threshold widens to ensure expected profit > costs.
    
    // We base the entry deviation on the equilibrium standard deviation, adjusted for transaction costs.
    // A standard optimal starting point is ~2.0 standard deviations from the mean (increased from 1.5 for higher win rate).
    // We enforce that the minimum gross profit is at least 2.5x the transaction cost to avoid fee bleed.
    let min_required_profit = transaction_cost * 2.5;
    let optimal_deviation = (2.0 * sigma_eq).max(min_required_profit);
    
    // In strict mean-reversion, the optimal exit is exactly at the equilibrium mean (mu).
    OptimalThresholds {
        entry_long: params.mu - optimal_deviation,
        exit_long: params.mu,
        entry_short: params.mu + optimal_deviation,
        exit_short: params.mu,
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

/// Computes the net expected edge in basis points between entry and exit price:
/// edge_bps = ln(exit_target / entry_price) * 10,000
pub fn calculate_net_edge_bps(entry_price: f64, exit_target: f64) -> f64 {
    if entry_price > 0.0 && exit_target > entry_price {
        (exit_target / entry_price).ln() * 10_000.0
    } else {
        0.0
    }
}

/// Feature snapshot capturing O-U, drift, and standardized Z-scores at a specific time cutoff.
#[derive(Debug, Clone)]
pub struct FeatureSnapshot {
    pub thresholds_opt: Option<OptimalThresholds>,
    pub total_drift_pct: f64,
    #[allow(dead_code)]
    pub is_drifting: bool,
    pub half_life_m: f64,
    pub z_15: f64,
    pub z_60: f64,
    pub is_downside_shock: bool,
    pub is_upside_shock: bool,
    pub hist_last_price: f64,
    pub is_stationary: bool,
    pub is_consolidation: bool,
    pub is_valid_half_life: bool,
}

/// Evaluates stochastic O-U, drift, and Z-score features at an exact time cutoff.
pub fn calculate_features(closes_history: &[f64], eval_close: f64) -> Option<FeatureSnapshot> {
    if closes_history.len() < 100 || eval_close <= 0.0 {
        return None;
    }
    let hist_last_price = *closes_history.last().unwrap_or(&eval_close);

    // O-U Parameter Estimation with Phillips & Yu Jackknife bias correction
    let (thresholds_opt, half_life_m) =
        if let Some(ou_params) = estimate_ou_parameters(closes_history, 1.0) {
            let hl = ou_params.half_life_minutes();
            let tx_cost = hist_last_price * 0.0005; // ~5 bps roundtrip on Hyperliquid Maker
            let th = calculate_optimal_thresholds(&ou_params, tx_cost);
            (Some(th), hl)
        } else {
            (None, 999.0)
        };

    // OLS Slope drift calculation
    let n_drift = closes_history.len() as f64;
    let mut sum_i = 0.0;
    let mut sum_p = 0.0;
    let mut sum_ip = 0.0;
    let mut sum_ii = 0.0;
    for (i, &p) in closes_history.iter().enumerate() {
        let idx = i as f64;
        sum_i += idx;
        sum_p += p;
        sum_ip += idx * p;
        sum_ii += idx * idx;
    }
    let slope = (n_drift * sum_ip - sum_i * sum_p)
        / (n_drift * sum_ii - sum_i * sum_i);
    let total_drift_pct = (slope * n_drift) / hist_last_price;
    let is_drifting = total_drift_pct.abs() > 0.015;

    // Standardized Downside/Upside Shock Calculation with robust MAD
    let n = closes_history.len();
    let mut z_15 = 0.0;
    let mut z_60 = 0.0;
    if n >= 65 {
        let mut returns_1m = Vec::with_capacity(n - 1);
        for i in 1..n {
            returns_1m.push((closes_history[i] / closes_history[i - 1]).ln());
        }
        let sigma_mad = calculate_mad_sigma(&returns_1m);

        let ret_15 = (eval_close / closes_history[n - 15]).ln();
        z_15 = ret_15 / (sigma_mad * (15.0_f64).sqrt());

        let ret_60 = (eval_close / closes_history[n - 60]).ln();
        z_60 = ret_60 / (sigma_mad * (60.0_f64).sqrt());
    }
    let is_downside_shock = z_15 < -3.0 || z_60 < -3.0;
    let is_upside_shock = z_15 > 3.0 || z_60 > 3.0;
    let is_stationary = thresholds_opt.is_some();
    let is_consolidation = !is_drifting && is_stationary;
    let is_valid_half_life = (15.0..=120.0).contains(&half_life_m);

    Some(FeatureSnapshot {
        thresholds_opt,
        total_drift_pct,
        is_drifting,
        half_life_m,
        z_15,
        z_60,
        is_downside_shock,
        is_upside_shock,
        hist_last_price,
        is_stationary,
        is_consolidation,
        is_valid_half_life,
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
        // t_half = ln(2) / 0.02 = 0.693147 / 0.02 ≈ 34.657 mins
        let hl = params.half_life_minutes();
        assert!((hl - 34.657).abs() < 0.01);

        // Near-zero theta fallback
        let slow_params = OuParameters {
            theta: 0.0,
            mu: 100.0,
            sigma: 1.0,
        };
        assert_eq!(slow_params.half_life_minutes(), 120.0);
    }

    #[test]
    fn test_optimal_thresholds_contain_cost_barrier() {
        let params = OuParameters {
            theta: 0.05,
            mu: 2700.0,
            sigma: 5.0,
        };
        let tx_cost = 2.70; // $2.70 fee
        let thresholds = calculate_optimal_thresholds(&params, tx_cost);
        
        assert!(thresholds.entry_long < params.mu);
        assert_eq!(thresholds.exit_long, params.mu);
        assert!(params.mu - thresholds.entry_long >= tx_cost * 2.5);
    }

    #[test]
    fn test_mad_sigma_robust_against_outliers() {
        // Normal returns with low variance
        let mut returns = vec![0.001, -0.001, 0.0012, -0.0009, 0.0008, -0.0011, 0.0002];
        let sigma_base = calculate_mad_sigma(&returns);
        
        // Add an extreme outlier flash-crash return (-5%)
        returns.push(-0.05);
        let sigma_with_outlier = calculate_mad_sigma(&returns);
        
        // MAD should be virtually unaffected by a single outlier, preventing denominator inflation
        assert!((sigma_with_outlier - sigma_base).abs() < 0.0005);
    }

    #[test]
    fn test_calculate_net_edge_bps() {
        // Less than 25 bps hurdle: 20 bps
        let low_edge = calculate_net_edge_bps(100.0, 100.20);
        assert!(low_edge < 25.0);
        assert!((low_edge - 19.98).abs() < 0.1);

        // Above 25 bps hurdle: 35 bps
        let good_edge = calculate_net_edge_bps(100.0, 100.35);
        assert!(good_edge >= 25.0);

        // Inverted or flat price: 0 bps
        assert_eq!(calculate_net_edge_bps(100.0, 99.0), 0.0);
        assert_eq!(calculate_net_edge_bps(100.0, 100.0), 0.0);
    }

    #[test]
    fn test_calculate_features() {
        let mut history = Vec::new();
        for i in 0..120 {
            history.push(100.0 + (i as f64 * 0.01).sin() * 2.0);
        }
        let feat = calculate_features(&history, 100.5).expect("Features calculated");
        assert!(feat.half_life_m > 0.0);
        assert!(feat.z_15.is_finite());
        assert!(feat.z_60.is_finite());
    }
}
