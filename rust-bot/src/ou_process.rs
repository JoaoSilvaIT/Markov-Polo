use std::f64;

/// Represents the mathematical parameters of an Ornstein-Uhlenbeck stochastic process.
#[derive(Debug, Clone)]
pub struct OuParameters {
    pub theta: f64, // Mean-reversion speed (how aggressively it pulls back to the mean)
    pub mu: f64,    // Equilibrium mean (the long-term average price)
    pub sigma: f64, // Volatility (amplitude of the random Wiener process noise)
}

/// Represents the mathematically optimal entry and exit thresholds.
#[derive(Debug, Clone)]
pub struct OptimalThresholds {
    pub entry_long: f64,
    pub exit_long: f64,
    pub entry_short: f64,
    pub exit_short: f64,
}

/// Estimates the Ornstein-Uhlenbeck parameters using Ordinary Least Squares (OLS)
/// applied to an exact Autoregressive AR(1) discretization.
/// 
/// `data`: The rolling window of price data (e.g., 480 1-minute close prices).
/// `dt`: The discrete time step (e.g., 1.0 for 1-minute bars).
pub fn estimate_ou_parameters(data: &[f64], dt: f64) -> Option<OuParameters> {
    if data.len() < 3 {
        return None;
    }
    
    let n = data.len() - 1;
    let mut sum_x = 0.0;
    let mut sum_y = 0.0;
    let mut sum_xx = 0.0;
    let mut sum_xy = 0.0;
    
    // We regress X_{t} against X_{t-1}
    // Y represents X_{t} and X represents X_{t-1}
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
        return None; // Cannot regress a perfectly flat line
    }
    
    let b = cov_xy / var_x;
    let a = mean_y - b * mean_x;
    
    // If b is >= 1.0, the process is a random walk or explosive (not mean-reverting).
    // If b is <= 0.0, the process is oscillating chaotically.
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
    
    // Calculate variance of the residuals
    let var_err = sum_err_sq / (nf - 2.0).max(1.0);
    let sigma_eps = var_err.sqrt();
    
    // Extract continuous-time O-U parameters from the discrete AR(1) coefficients
    let theta = -b.ln() / dt;
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
    // A standard optimal starting point is ~1.5 standard deviations from the mean.
    // We enforce that the minimum gross profit is at least 2.5x the transaction cost to avoid fee bleed.
    let min_required_profit = transaction_cost * 2.5;
    let optimal_deviation = (1.5 * sigma_eq).max(min_required_profit);
    
    // In strict mean-reversion, the optimal exit is exactly at the equilibrium mean (mu).
    OptimalThresholds {
        entry_long: params.mu - optimal_deviation,
        exit_long: params.mu,
        entry_short: params.mu + optimal_deviation,
        exit_short: params.mu,
    }
}
