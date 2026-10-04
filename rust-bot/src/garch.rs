use std::f64::consts::PI;
use statrs::function::gamma::ln_gamma;

#[derive(Debug)]
pub struct GarchResult {
    pub omega: f64,
    pub alpha: f64,
    pub beta: f64,
    pub nu: f64,
    pub forecast_variance: f64,
}

fn student_t_log_likelihood(returns: &[f64], params: &[f64]) -> f64 {
    let omega = params[0];
    let alpha = params[1];
    let beta = params[2];
    let nu = params[3];

    // Penalty for out of bounds
    if omega <= 1e-8 || alpha < 0.0 || beta < 0.0 || (alpha + beta) >= 0.9999 || nu <= 2.1 {
        return f64::NEG_INFINITY;
    }

    let mut ll = 0.0;
    let mut sigma2 = omega / (1.0 - alpha - beta);

    let half_nu_plus_1 = (nu + 1.0) / 2.0;
    let half_nu = nu / 2.0;
    let log_gamma_diff = ln_gamma(half_nu_plus_1) - ln_gamma(half_nu);
    let log_nu_minus_2_pi = ((nu - 2.0) * PI).ln();

    for &r in returns {
        if sigma2 <= 0.0 || sigma2.is_nan() { return f64::NEG_INFINITY; }

        let log_sigma2 = sigma2.ln();
        let term = (1.0 + (r * r) / ((nu - 2.0) * sigma2)).ln();
        
        ll += log_gamma_diff - 0.5 * log_sigma2 - 0.5 * log_nu_minus_2_pi - half_nu_plus_1 * term;

        // Update variance for next step
        sigma2 = omega + alpha * r * r + beta * sigma2;
    }

    ll
}

// Simple Nelder-Mead for 4 variables
fn nelder_mead(returns: &[f64], init: [f64; 4], max_iter: usize) -> [f64; 4] {
    let mut simplex = [
        init,
        [init[0] * 1.5, init[1], init[2], init[3]],
        [init[0], init[1] + 0.02, init[2], init[3]],
        [init[0], init[1], init[2] - 0.05, init[3]],
        [init[0], init[1], init[2], init[3] + 1.0],
    ];

    let mut f_vals = [0.0; 5];
    for i in 0..5 {
        f_vals[i] = -student_t_log_likelihood(returns, &simplex[i]); // Minimize negative LL
    }

    let alpha = 1.0;
    let gamma = 2.0;
    let rho = 0.5;
    let sigma = 0.5;

    for _ in 0..max_iter {
        // Sort simplex (inefficient but small N=5)
        let mut indices = [0, 1, 2, 3, 4];
        indices.sort_by(|&a, &b| f_vals[a].partial_cmp(&f_vals[b]).unwrap());
        
        let mut new_simplex = [[0.0; 4]; 5];
        let mut new_f = [0.0; 5];
        for i in 0..5 {
            new_simplex[i] = simplex[indices[i]];
            new_f[i] = f_vals[indices[i]];
        }
        simplex = new_simplex;
        f_vals = new_f;

        // Centroid of best 4
        let mut centroid = [0.0; 4];
        for i in 0..4 {
            for j in 0..4 {
                centroid[j] += simplex[i][j] / 4.0;
            }
        }

        let worst = simplex[4];
        
        // Reflection
        let mut reflected = [0.0; 4];
        for j in 0..4 {
            reflected[j] = centroid[j] + alpha * (centroid[j] - worst[j]);
        }
        let f_r = -student_t_log_likelihood(returns, &reflected);

        if f_r >= f_vals[0] && f_r < f_vals[3] {
            simplex[4] = reflected;
            f_vals[4] = f_r;
            continue;
        }

        // Expansion
        if f_r < f_vals[0] {
            let mut expanded = [0.0; 4];
            for j in 0..4 {
                expanded[j] = centroid[j] + gamma * (reflected[j] - centroid[j]);
            }
            let f_e = -student_t_log_likelihood(returns, &expanded);
            if f_e < f_r {
                simplex[4] = expanded;
                f_vals[4] = f_e;
            } else {
                simplex[4] = reflected;
                f_vals[4] = f_r;
            }
            continue;
        }

        // Contraction
        let mut contracted = [0.0; 4];
        for j in 0..4 {
            contracted[j] = centroid[j] + rho * (worst[j] - centroid[j]);
        }
        let f_c = -student_t_log_likelihood(returns, &contracted);
        
        if f_c < f_vals[4] {
            simplex[4] = contracted;
            f_vals[4] = f_c;
            continue;
        }

        // Shrink
        for i in 1..5 {
            for j in 0..4 {
                simplex[i][j] = simplex[0][j] + sigma * (simplex[i][j] - simplex[0][j]);
            }
            f_vals[i] = -student_t_log_likelihood(returns, &simplex[i]);
        }
    }

    simplex[0] // Best params
}

pub fn forecast_garch(closes: &[f64]) -> f64 {
    if closes.len() < 2 { return 0.01; }
    
    // 1. Log returns * 100 for stability
    let mut returns = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        returns.push((closes[i] / closes[i-1]).ln() * 100.0);
    }

    // 2. Optimize
    let init = [0.01, 0.05, 0.90, 5.0];
    let best_params = nelder_mead(&returns, init, 300);

    let omega = best_params[0];
    let alpha = best_params[1];
    let beta = best_params[2];
    let nu = best_params[3];

    // 3. Forecast Next Variance
    let mut sigma2 = omega / (1.0 - alpha - beta).max(1e-6);
    for &r in &returns {
        sigma2 = omega + alpha * r * r + beta * sigma2;
    }

    // 4. Fat-tail adjustment
    let mut adj_variance = sigma2;
    if nu > 2.0 {
        adj_variance *= nu / (nu - 2.0);
    }

    // 5. Unscale and return volatility
    (adj_variance.sqrt() / 100.0)
}
