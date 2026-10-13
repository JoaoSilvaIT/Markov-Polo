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
/// Operates in **log-price space** (Y_t = ln(S_t)) for three key advantages:
///   1. Log-returns are more homoscedastic than price-level increments (better AR(1) fit).
///   2. sigma_eps is scale-invariant (fractional, ~0.1–0.5% per 1m bar), making
///      sigma_eq a consistent percentage-of-price deviation across all assets and price levels.
///   3. mu = exp(log_mu) respects price positivity and handles asymmetric up/down moves.
///
/// Returns mu/sigma in **price space** for full backward compatibility with all callers.
/// Specifically, sigma_price = mu × sigma_log, so sigma_eq = sigma_price/sqrt(2θ) remains
/// the correct equilibrium standard deviation in price units.
///
/// `data`: The rolling window of price data (e.g., 960 1-minute close prices).
/// `dt`: The discrete time step (e.g., 1.0 for 1-minute bars).
pub fn estimate_ou_parameters(data: &[f64], dt: f64) -> Option<OuParameters> {
    // Validate input: all prices must be positive and finite
    if data.iter().any(|&p| !p.is_finite() || p <= 0.0) {
        return None;
    }

    // Work in log-price space: Y_t = ln(S_t)
    // Log-prices have more homoscedastic increments than raw levels, giving
    // better-conditioned AR(1) estimates across different price magnitudes.
    let log_data: Vec<f64> = data.iter().map(|&p| p.ln()).collect();

    let (a_log, b_log, sigma_eps_log) = estimate_ar1_raw(&log_data)?;

    let theta_full = -b_log.ln() / dt;

    // Apply Phillips & Yu (2005) 2-split Jackknife bias correction on log-price AR(1):
    // theta_jack = 2 * theta_full - 0.5 * (theta_1 + theta_2)
    // This removes the O(1/T) Hurwicz bias that artificially inflates mean-reversion speed.
    let mid = log_data.len() / 2;
    let theta = if mid >= 4 {
        let part1 = estimate_ar1_raw(&log_data[..=mid]);
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

    // mu in log-space → convert to price space.
    // mu_log = a / (1 - b) is the long-run log-price mean; exp() gives the price-space equilibrium.
    let mu_log = a_log / (1.0 - b_log);
    let mu = mu_log.exp();
    if !mu.is_finite() || mu <= 0.0 {
        return None;
    }

    // sigma_log is the OU diffusion coefficient in log-return units (e.g., 0.002 = 0.2%/bar).
    // Convert to price space: sigma_price ≈ mu × sigma_log (first-order Taylor, valid for small σ).
    // This preserves the interface: sigma_eq = sigma_price / sqrt(2θ) gives the correct
    // equilibrium ±1σ price band. For thresholds: entry = mu - 1.75×sigma_eq in price space,
    // which equals mu × (1 - 1.75×sigma_log/sqrt(2θ)) — multiplicatively consistent.
    let b_jack = (-theta * dt).exp();
    let denom_sq = (1.0 - b_jack * b_jack).max(1e-12);
    let sigma_log = sigma_eps_log * ((2.0 * theta) / denom_sq).sqrt();
    let sigma = mu * sigma_log;

    Some(OuParameters { theta, mu, sigma })
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

/// Calculates the optimal entry and exit boundaries using a closed-form approximation 
/// derived from the Bertram Optimization Model for Statistical Arbitrage.
/// Maximizes the expected return per unit of time based on mean-reversion speed.
#[allow(dead_code)]
pub fn calculate_optimal_thresholds(params: &OuParameters, transaction_cost: f64) -> OptimalThresholds {
    calculate_thresholds_at_sigma(params, transaction_cost, 1.50)
}

/// Calculates asymmetric carry-shifted Bertram O-U entry/exit thresholds:
/// Positive carry (receiving funding) relaxes the required entry excursion toward `min_sigma` (e.g. 1.30σ);
/// Negative carry (paying funding) widens the required entry excursion toward `max_sigma` (e.g. 1.75σ).
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
        let dz_long = (-hourly_funding_rate / denom).clamp(-2.0, 2.0);
        let dz_short = (hourly_funding_rate / denom).clamp(-2.0, 2.0);
        let gamma = 0.50;
        let s_long = (base_sigma - gamma * dz_long).clamp(min_sigma, max_sigma);
        let s_short = (base_sigma - gamma * dz_short).clamp(min_sigma, max_sigma);
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
/// - Session 1 (US Macro & Equity Open: 13:00-17:00 UTC Mon-Fri): +0.10σ wider to guard against jump shocks
/// - Session 3 (Asian / Off-Peak / Weekends: 00:00-08:00 UTC or Sat/Sun): -0.05σ tighter for stationary ranges
/// - Session 2 (Normal transition hours): 0.0σ
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
/// For Long (is_buy = true): ln(exit_target / entry_price) * 10,000 (target > entry)
/// For Short (is_buy = false): ln(entry_price / exit_target) * 10,000 (entry > target)
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
    } else {
        if entry_price > exit_target {
            (entry_price / exit_target).ln() * 10_000.0
        } else {
            0.0
        }
    }
}

/// Computes the net expected edge in basis points between entry and exit price (Long default):
/// edge_bps = ln(exit_target / entry_price) * 10,000
pub fn calculate_net_edge_bps(entry_price: f64, exit_target: f64) -> f64 {
    calculate_directional_edge_bps(entry_price, exit_target, true)
}

/// Calculates the Rogers-Satchell realized volatility over a series of OHLC price bars.
/// Rogers-Satchell is non-zero drift independent and ~8x more efficient than standard close-to-close.
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

/// Calculates the Garman-Klass realized volatility over a series of OHLC price bars.
/// ~7.4x more efficient than close-to-close volatility in continuous markets.
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

/// Feature snapshot capturing O-U, drift, and standardized Z-scores at a specific time cutoff.
#[derive(Debug, Clone)]
pub struct FeatureSnapshot {
    pub thresholds_opt: Option<OptimalThresholds>,
    pub thresholds_175_opt: Option<OptimalThresholds>,
    pub total_drift_pct: f64,
    #[allow(dead_code)]
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
}

/// Evaluates stochastic O-U, drift, and Z-score features at an exact time cutoff.
#[allow(dead_code)]
pub fn calculate_features(closes_history: &[f64], eval_close: f64, symbol: &str) -> Option<FeatureSnapshot> {
    calculate_features_with_funding(closes_history, eval_close, symbol, 0.0)
}

/// Evaluates stochastic O-U, drift, and Z-score features incorporating Hyperliquid hourly funding rate carry drift.
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

    // Lookback-relative half-life bounds (Quant Researcher Consensus):
    // Lower bound: max(30.0 min, 0.03 * N) to filter microstructure noise and bid-ask bounce
    // Upper bound: 0.50 * N (e.g. 480 mins for 960 bars) to ensure mean reversion materializes within window
    // Assets with t_half > 0.50 * N (like AVAX at ~770m) are classified as slow/uncertain unit-root and rejected.
    let n_bars = closes_history.len() as f64;
    let min_hl = 30.0_f64.max(0.03 * n_bars);
    let max_hl = 0.50 * n_bars;

    // O-U Parameter Estimation with Phillips & Yu Jackknife bias correction (log-price space)
    let (thresholds_opt, thresholds_175_opt, half_life_m) =
        if let Some(mut ou_params) = estimate_ou_parameters(closes_history, 1.0) {
            let hl = ou_params.half_life_minutes();
            let is_hl_ok = hl >= min_hl && hl <= max_hl;
            // Apply continuous-time hourly funding carry drift adjustment to equilibrium mean mu:
            // mu_carry = mu * (1 - F_1h / (60 * theta)), bounded to +/- 15 bps to prevent spike distortion.
            if hourly_funding_rate.is_finite() && hourly_funding_rate.abs() > 1e-7 && ou_params.theta > 1e-5 {
                let carry_shift = (-hourly_funding_rate / (60.0 * ou_params.theta)).clamp(-0.0015, 0.0015);
                ou_params.mu *= 1.0 + carry_shift;
            }
            // Tighter 7% gate: log-space estimation is more stable, so a wider mu deviation
            // reliably signals a unit-root explosion or a structural break rather than noise.
            let mu_dev = (ou_params.mu - hist_last_price).abs() / hist_last_price;
            let is_mu_ok = mu_dev <= 0.07;

            if is_hl_ok && is_mu_ok {
                let tx_cost = hist_last_price * 0.0005; // ~5 bps roundtrip on Hyperliquid Maker
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
                    1.50 + session_offset,
                    hourly_funding_rate,
                    1.30,
                    1.75,
                );
                let th_125 = calculate_carry_adjusted_thresholds(
                    &ou_params,
                    tx_cost,
                    1.25 + session_offset * 0.5,
                    hourly_funding_rate,
                    1.15,
                    1.45,
                );
                (Some(th), Some(th_125), hl)
            } else {
                (None, None, hl)
            }
        } else {
            (None, None, 999.0)
        };

    // Scale-Invariant Log-Price OLS Slope Drift Test with Student's t-statistic (Two-Pass Centered)
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

    // Compute residual variance and t-statistic for OLS slope significance (H0: slope == 0)
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

    // Cumulative log-return drift over the lookback window
    let total_drift_pct = slope_log * n_drift;

    // Asset-specific maximum allowable economic drift over the lookback window:
    // BTC: 2.0%, ETH: 2.5%, SOL: 4.0%, AVAX: 5.0%, Default: 2.5%
    let max_economic_drift = match symbol.to_uppercase().as_str() {
        "BTC"  => 0.020,
        "ETH"  => 0.025,
        "SOL"  => 0.040,
        "AVAX" => 0.050,
        _      => 0.025,
    };

    // Series is drifting if slope is statistically significant (|t| >= 2.58, p < 0.01)
    // AND the economic cumulative drift exceeds asset hurdle
    let is_drifting = t_stat_slope.abs() >= 2.58 && total_drift_pct.abs() > max_economic_drift;

    // Standardized Downside/Upside Shock Calculation with robust MAD
    let n = closes_history.len();
    let mut z_15 = 0.0;
    let mut z_60 = 0.0;
    let mut ret_15 = 0.0;
    let mut ret_60 = 0.0;
    if n >= 65 {
        let mut returns_1m = Vec::with_capacity(n - 1);
        for i in 1..n {
            returns_1m.push((closes_history[i] / closes_history[i - 1]).ln());
        }
        let sigma_mad = calculate_mad_sigma(&returns_1m);

        ret_15 = (eval_close / closes_history[n - 15]).ln();
        z_15 = ret_15 / (sigma_mad * (15.0_f64).sqrt());

        ret_60 = (eval_close / closes_history[n - 60]).ln();
        z_60 = ret_60 / (sigma_mad * (60.0_f64).sqrt());
    }

    // Asset-specific physical return hurdles for the shock veto.
    // Rationale: a 0.75% 15m move is a mild fluctuation on SOL or AVAX but a meaningful shock
    // on BTC. Using asset-specific thresholds prevents over-vetoing on volatile small-caps
    // while maintaining protective sensitivity on large-caps.
    // Each threshold pair: (shock_15m_pct, shock_60m_pct).
    let (hurdle_15, hurdle_60) = match symbol.to_uppercase().as_str() {
        "BTC"  => (0.0075, 0.0150), // BTC: 0.75% / 1.50% (baseline)
        "ETH"  => (0.0100, 0.0200), // ETH: 1.00% / 2.00% (slightly more volatile)
        "SOL"  => (0.0150, 0.0300), // SOL: 1.50% / 3.00% (significantly more volatile)
        "AVAX" => (0.0200, 0.0400), // AVAX: 2.00% / 4.00% (most volatile in portfolio)
        _      => (0.0100, 0.0200), // Default: ETH-equivalent for unknown assets
    };

    let shock_15_down = ret_15 <= -hurdle_15 && z_15 <= -3.0;
    let shock_15_up   = ret_15 >=  hurdle_15 && z_15 >=  3.0;
    let shock_60_down = ret_60 <= -hurdle_60 && z_60 <= -3.0;
    let shock_60_up   = ret_60 >=  hurdle_60 && z_60 >=  3.0;

    let is_downside_shock = shock_15_down || shock_60_down;
    let is_upside_shock = shock_15_up || shock_60_up;
    let is_stationary = thresholds_opt.is_some();
    let is_consolidation = !is_drifting && is_stationary;
    let is_valid_half_life = half_life_m >= min_hl && half_life_m <= max_hl;

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
        assert_eq!(slow_params.half_life_minutes(), 999.0);
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

        // Directional Short edge: entry 100.0, exit 99.65 -> ~35.06 bps
        let short_edge = calculate_directional_edge_bps(100.0, 99.65, false);
        assert!(short_edge >= 25.0);
        assert!((short_edge - 35.06).abs() < 0.1);

        // Inverted short (exit > entry): 0 bps
        assert_eq!(calculate_directional_edge_bps(100.0, 100.5, false), 0.0);

        // Non-positive or zero prices
        assert_eq!(calculate_directional_edge_bps(0.0, 100.0, true), 0.0);
        assert_eq!(calculate_directional_edge_bps(100.0, -5.0, false), 0.0);
    }

    #[test]
    fn test_calculate_features() {
        let mut history = Vec::new();
        for i in 0..120 {
            history.push(100.0 + (i as f64 * 0.01).sin() * 2.0);
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

        // For Long: 1.25σ entry is closer to the mean than 1.50σ (i.e. th_125.entry_long > th_150.entry_long)
        assert!(th_125.entry_long > th_150.entry_long);
        // Both are below the mean
        assert!(th_125.entry_long < params.mu);
        assert!(th_150.entry_long < params.mu);

        // For Short: 1.25σ entry is closer to the mean than 1.50σ (i.e. th_125.entry_short < th_150.entry_short)
        assert!(th_125.entry_short < th_150.entry_short);
        // Both are above the mean
        assert!(th_125.entry_short > params.mu);
        assert!(th_150.entry_short > params.mu);

        // Both target the same mean reversion exit target
        assert_eq!(th_125.exit_long, params.mu);
        assert_eq!(th_150.exit_long, params.mu);
    }

    #[test]
    fn test_shock_physical_return_hurdle() {
        // Build a baseline of flat/quiet prices (100.0)
        let mut history = vec![100.0; 120];
        // Inject tiny tick noise (0.001) to produce a tiny MAD sigma
        for i in 0..120 {
            if i % 2 == 0 {
                history[i] = 100.01;
            } else {
                history[i] = 99.99;
            }
        }

        // Scenario 1: A 28 bps move (100.0 -> 100.28).
        // Statistical Z will be large (> 3.0) due to tiny sigma_mad, but |ret_15| is only 0.28% < 0.75%.
        // Must NOT trigger is_upside_shock!
        let feat_quiet = calculate_features(&history, 100.28, "BTC").expect("features");
        assert!(feat_quiet.z_15 > 3.0);
        assert!(!feat_quiet.is_upside_shock, "28 bps move must not trigger shock veto despite high Z!");

        // Scenario 2: A genuine 150 bps flash pump (100.0 -> 101.50).
        // Statistical Z > 3.0 AND |ret_15| = 1.5% >= 0.75%.
        // Must trigger is_upside_shock!
        let feat_real_shock = calculate_features(&history, 101.50, "BTC").expect("features");
        assert!(feat_real_shock.z_15 > 3.0);
        assert!(feat_real_shock.is_upside_shock, "150 bps flash move must trigger shock veto!");

        // Scenario 3: A genuine -200 bps flash dump (100.0 -> 98.00).
        // Must trigger is_downside_shock!
        let feat_dump = calculate_features(&history, 98.00, "BTC").expect("features");
        assert!(feat_dump.z_15 < -3.0);
        assert!(feat_dump.is_downside_shock, "-200 bps flash crash must trigger downside shock!");
    }

    #[test]
    fn test_monotone_trend_rejected_by_ou_jackknife() {
        // Pure exponential trend should be rejected as non-stationary (None)
        let mut trend_prices = Vec::with_capacity(200);
        for i in 0..200 {
            trend_prices.push(100.0 * (0.001 * i as f64).exp());
        }
        assert!(estimate_ou_parameters(&trend_prices, 1.0).is_none());
    }

    #[test]
    fn test_carry_adjusted_asymmetric_ou_thresholds() {
        let params = OuParameters {
            theta: 0.02, // 1.2 per hour
            mu: 100.0,
            sigma: 0.20,
        };
        let tx_cost = 0.05;
        let neutral = calculate_carry_adjusted_thresholds(&params, tx_cost, 1.50, 0.0, 1.30, 1.75);
        // Positive hourly funding (+0.05% / hr): longs pay shorts
        // Short receives positive carry -> entry_short moves closer to mu (lower)
        // Long pays negative carry -> entry_long moves farther from mu (lower)
        let pos_fund = calculate_carry_adjusted_thresholds(&params, tx_cost, 1.50, 0.0005, 1.30, 1.75);
        assert!(pos_fund.entry_short < neutral.entry_short, "Positive carry for short should relax short entry threshold closer to mu");
        assert!(pos_fund.entry_long < neutral.entry_long, "Negative carry for long should require deeper discount below mu");
    }
}

