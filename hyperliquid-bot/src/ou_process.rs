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

    let (_a_log, b_log, sigma_eps_log) = estimate_ar1_raw(&log_data)?;

    let theta_full = -b_log.ln() / dt;
    let (_, is_monotone_drift) = evaluate_ols_drift(data, 0.005);

    // Apply Phillips & Yu (2005) 2-split Jackknife bias correction on log-price AR(1):
    // theta_jack = 2 * theta_full - 0.5 * (theta_1 + theta_2)
    // When the two halves of a non-drifting range have slightly different local sample means,
    // separate half-sample demeaning can artificially inflate (theta_1 + theta_2) above 2*theta_full,
    // causing `th_jack` to jump wildly or flip negative on a 1-tick move. For non-drifting series
    // with b_log < 0.985, floor `th_jack` at `0.60 * theta_full` to preserve bar-to-bar continuity.
    let mid = log_data.len() / 2;
    let theta = if mid >= 4 {
        let part1 = estimate_ar1_raw(&log_data[..=mid]);
        let part2 = estimate_ar1_raw(&log_data[mid..]);

        if let (Some((_, b1, _)), Some((_, b2, _))) = (part1, part2) {
            let theta1 = -b1.ln() / dt;
            let theta2 = -b2.ln() / dt;
            let th_jack = 2.0 * theta_full - 0.5 * (theta1 + theta2);
            if is_monotone_drift || b_log >= 0.985 {
                if th_jack > 1e-5 && !is_monotone_drift {
                    th_jack
                } else {
                    return None;
                }
            } else {
                let floored = th_jack.max(0.60 * theta_full);
                if floored > 1e-5 {
                    floored
                } else {
                    return None;
                }
            }
        } else {
            return None;
        }
    } else {
        theta_full
    };

    // Local equilibrium mean in log-space -> convert to price space.
    // Using the trailing 45m-90m window (scaled to 2.5 * t_half and excluding the final 3 bars
    // of the active excursion) prevents `mu` from lagging 2-4 hours behind a post-crash recovery
    // (which otherwise causes false shorts into a rising market) while keeping `mu` invariant
    // to the immediate 3-bar dip/spike.
    let n_log = log_data.len();
    let hl_prelim = (2.0_f64.ln() / theta.max(1e-5)).clamp(15.0, 90.0);
    let mu_lookback = ((2.5 * hl_prelim).round() as usize).clamp(45, 90).min(n_log);
    let mu_end = if n_log > 10 { n_log - 3 } else { n_log };
    let mu_start = mu_end.saturating_sub(mu_lookback);
    let mu_slice = &log_data[mu_start..mu_end];
    let mu_log = mu_slice.iter().sum::<f64>() / (mu_slice.len().max(1) as f64);
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
    // Guard against discrete-tick zero collapse (e.g. AVAX $0.01 ticks where >=50% of 1m bars have r=0)
    // by computing the 80%-trimmed Mean Absolute Deviation (excluding top 20% tail outliers).
    let trim_len = ((abs_devs.len() as f64 * 0.80).ceil() as usize).clamp(1, abs_devs.len());
    let trimmed_mean_dev = abs_devs[..trim_len].iter().sum::<f64>() / (trim_len as f64);
    let trimmed_sigma = 1.2533 * trimmed_mean_dev;

    (1.4826 * med_dev).max(0.75 * trimmed_sigma).max(1e-5)
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

/// Evaluates Scale-Invariant Log-Price OLS Slope Drift Test with Student's t-statistic (Two-Pass Centered).
/// Returns `(total_drift_pct, is_drifting)`.
fn evaluate_ols_drift(slice: &[f64], max_economic_drift: f64) -> (f64, bool) {
    let valid_lps: Vec<(f64, f64)> = slice
        .iter()
        .enumerate()
        .filter(|(_, &p)| p > 0.0 && p.is_finite())
        .map(|(i, &p)| (i as f64, p.ln()))
        .collect();
    let n_drift = valid_lps.len() as f64;
    if n_drift < 10.0 {
        return (0.0, false);
    }
    let mean_i = valid_lps.iter().map(|(i, _)| *i).sum::<f64>() / n_drift;
    let mean_lp = valid_lps.iter().map(|(_, lp)| *lp).sum::<f64>() / n_drift;

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
    let is_drifting = t_stat_slope.abs() >= 2.58 && total_drift_pct.abs() > max_economic_drift;
    (total_drift_pct, is_drifting)
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

    // Operating half-life bounds (5m to 120m):
    // - Lower bound (min_hl = 5.0m): filters 1-2m bid-ask bounce while allowing fast intraday consolidation (6m-30m)
    //   without kicking ETH/AVAX out of MeanReversion when reversion speed accelerates below 15m.
    // - Upper bound (max_hl = 120.0m): ensures mean reversion materializes within the 3.5*t_half horizon without bleeding funding/time-stop fees.
    let min_hl = 5.0_f64;
    let max_hl = 120.0_f64;

    // Asset-specific maximum allowable economic drift over the lookback window:
    // BTC: 2.0%, ETH: 2.5%, SOL: 4.0%, AVAX: 5.0%, Default: 2.5%
    let max_economic_drift = match symbol.to_uppercase().as_str() {
        "BTC"  => 0.020,
        "ETH"  => 0.025,
        "SOL"  => 0.040,
        "AVAX" => 0.050,
        _      => 0.025,
    };

    // Consistent Intraday O-U & Drift Horizon (240m = 4h primary, matching the 16x15m HMM lookback):
    // Evaluating the 4-hour (240m) local horizon first prevents 1-minute ping-ponging between
    // 16-hour macro bands (when 960m t_half hovers near 118m-122m) and 4-hour intraday bands.
    let n_total = closes_history.len();
    let candidate_windows = [
        240usize.min(n_total),
        180usize.min(n_total),
        120usize.min(n_total),
        360usize.min(n_total),
        n_total,
    ];

    let (mut total_drift_pct, mut is_drifting) =
        evaluate_ols_drift(&closes_history[n_total - candidate_windows[0]..], max_economic_drift * 0.50);

    let mut selected_ou: Option<OuParameters> = None;
    let mut best_fallback_hl: Option<(OuParameters, f64, bool)> = None;
    let mut seen_w = Vec::with_capacity(4);

    for &w in &candidate_windows {
        if w < 100 || seen_w.contains(&w) {
            continue;
        }
        seen_w.push(w);
        let sub_slice = &closes_history[n_total - w..];
        let local_max_drift =
            max_economic_drift * ((w as f64 / 960.0).sqrt().clamp(0.45, 1.0));
        let (sub_drift, sub_drifting) = evaluate_ols_drift(sub_slice, local_max_drift);
        if let Some(sub_params) = estimate_ou_parameters(sub_slice, 1.0) {
            let hl = sub_params.half_life_minutes();
            let mu_dev = (sub_params.mu - hist_last_price).abs() / hist_last_price;
            let max_mu_dev = if w <= 360 { 0.045 } else { 0.07 };
            if !sub_drifting && hl >= min_hl && hl <= max_hl && mu_dev <= max_mu_dev {
                selected_ou = Some(sub_params);
                total_drift_pct = sub_drift;
                is_drifting = false;
                best_fallback_hl = None;
                break;
            } else if best_fallback_hl.as_ref().map_or(true, |(prev, _, _)| {
                hl >= min_hl && hl < prev.half_life_minutes()
            }) {
                best_fallback_hl = Some((sub_params, sub_drift, sub_drifting));
            }
        }
    }
    if selected_ou.is_none() {
        if let Some((fb_params, fb_drift, fb_drifting)) = best_fallback_hl {
            selected_ou = Some(fb_params);
            total_drift_pct = fb_drift;
            is_drifting = fb_drifting;
        }
    }

    let (thresholds_opt, thresholds_175_opt, half_life_m) =
        if let Some(mut ou_params) = selected_ou {
            let hl = ou_params.half_life_minutes();
            let is_stationary_hl = hl > 0.0 && hl <= max_hl;
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

            if is_stationary_hl && is_mu_ok {
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

    #[test]
    fn test_fast_reversion_preserves_stationarity_and_validates_15_to_120m_band() {
        // Generate a fast-reverting stationary sine-wave series with t_half around ~20m
        let mut history = Vec::with_capacity(300);
        for i in 0..300 {
            history.push(100.0 + (i as f64 * 0.26).sin() * 0.6);
        }
        let feat = calculate_features(&history, 100.0, "SOL").expect("Features calculated");
        assert!(feat.half_life_m >= 15.0 && feat.half_life_m < 30.0, "Expected t_half in 15m-30m band, got {}", feat.half_life_m);
        assert!(feat.is_stationary, "Fast mean-reverting process in 15m-30m band must be stationary");
        assert!(feat.is_valid_half_life, "t_half in 15m-30m band must pass is_valid_half_life");
        assert!(feat.is_consolidation, "Non-drifting stationary series must be valid consolidation");
        assert!(feat.thresholds_opt.is_some(), "Thresholds must be populated for 15m-30m half-life");
    }

    #[test]
    fn test_post_structural_break_consolidation_recovers_ou_stationarity() {
        // Reproduce live market level shift:
        // First 720m (12h) oscillating around $103.00, then a drop to $99.80 followed by
        // 240m (4h) of clean stationary mean reversion around $99.80.
        let mut history = Vec::with_capacity(960);
        for i in 0..720 {
            history.push(103.0 + (i as f64 * 0.22).sin() * 0.45);
        }
        for i in 0..240 {
            history.push(99.80 + (i as f64 * 0.22).sin() * 0.45);
        }
        let eval_px = *history.last().unwrap();
        let feat = calculate_features(&history, eval_px, "AVAX").expect("Features should compute");
        assert!(
            feat.is_stationary && feat.is_valid_half_life,
            "Post-break 4h consolidation must recover stationarity (got half_life={:.1}m, is_stationary={})",
            feat.half_life_m,
            feat.is_stationary
        );
        let th = feat.thresholds_opt.expect("Thresholds must be populated around new equilibrium");
        assert!(
            (th.exit_long - 99.80).abs() < 0.50,
            "Equilibrium mu should anchor to post-break level (~99.80), got {:.2}",
            th.exit_long
        );
    }

    #[test]
    fn test_end_to_end_post_break_consolidation_arm_and_bounce_no_catch22() {
        // End-to-end simulation of the full Engine A pipeline after a structural level shift:
        // 1. 720m around $2450, then 240m consolidating around $2410.
        let mut history = Vec::with_capacity(960);
        for i in 0..720 {
            history.push(2450.0 + (i as f64 * 0.23).sin() * 6.0);
        }
        for i in 0..233 {
            history.push(2410.0 + (i as f64 * 0.23).sin() * 6.0);
        }
        // Hold near equilibrium mean ($2410) for 6 bars, then dip smoothly over 9 bars to the lower 1.50*sigma band (~$2399.80)
        let step_dips = [
            2410.2, 2409.9, 2410.1, 2409.8, 2410.0, 2409.5,
            2408.2, 2406.8, 2405.4, 2404.0, 2402.8, 2401.7, 2400.8, 2400.1, 2399.6,
        ];
        for &px in &step_dips {
            history.push(px);
        }
        let dip_px = *history.last().unwrap();

        let feat_dip = calculate_features(&history, dip_px, "ETH").expect("Dip features");
        assert!(feat_dip.is_stationary && feat_dip.is_valid_half_life);
        let th = feat_dip.thresholds_opt.expect("Thresholds at dip");
        assert!(
            dip_px <= th.entry_long,
            "Dip price {:.2} should touch/breach entry_long {:.2}",
            dip_px,
            th.entry_long
        );
        // Verify no Catch-22 between Z15/Z60 and Engine A eligibility gates at the band touch
        let consolidation_vol_ok = feat_dip.z_15.abs() < 2.40 && feat_dip.z_60.abs() < 1.85;
        let can_mean_revert_long = feat_dip.z_60 > -1.75
            && feat_dip.z_15 > -2.35
            && feat_dip.total_drift_pct > -0.0160;
        assert!(
            consolidation_vol_ok && can_mean_revert_long,
            "Band touch must not self-veto on Z15/Z60 (z15={:.2}, z60={:.2}, drift={:.4})",
            feat_dip.z_15,
            feat_dip.z_60,
            feat_dip.total_drift_pct
        );

        // 3. Next 1m bar: +8 bps green bounce off the dip low
        let bounce_px = dip_px * 1.0008;
        let edge_bps = calculate_net_edge_bps(bounce_px, th.exit_long);
        assert!(
            edge_bps >= 15.0,
            "Post-bounce net edge to equilibrium mu must clear 15 bps hurdle, got {:.1} bps",
            edge_bps
        );
    }
}

