use std::f64::consts::PI;
use statrs::function::gamma::ln_gamma;

#[derive(Debug)]
pub struct GarchResult {
    pub omega: f64,
    pub alpha: f64,
    pub gamma: f64,
    pub beta: f64,
    pub nu: f64,
    pub forecast_variance: f64,
}

// Evaluate Negative Log-Likelihood for EGARCH(1,1) with Student-t
fn egarch_student_t_nll(returns: &[f64], params: &[f64]) -> f64 {
    let omega = params[0];
    let alpha = params[1];
    let gamma = params[2];
    let beta = params[3];
    let nu = params[4];

    // Constraints penalty
    if nu <= 2.01 || beta.abs() >= 0.999 || alpha < 0.0 {
        return f64::INFINITY;
    }

    let mut nll = 0.0;
    
    // Initial variance = unconditional variance approx
    let mut log_sigma2 = omega / (1.0 - beta); // Assuming alpha * E|z| is 0 for init
    
    let half_nu_plus_1 = (nu + 1.0) / 2.0;
    let half_nu = nu / 2.0;
    let log_gamma_diff = ln_gamma(half_nu_plus_1) - ln_gamma(half_nu);
    let log_nu_minus_2_pi = ((nu - 2.0) * PI).ln();

    for &r in returns {
        let sigma2 = log_sigma2.exp();
        if sigma2 <= 0.0 || sigma2.is_nan() || sigma2.is_infinite() { return f64::INFINITY; }
        let sigma = sigma2.sqrt();

        let term = (1.0 + (r * r) / ((nu - 2.0) * sigma2)).ln();
        
        // Log-likelihood contribution
        let ll = log_gamma_diff - 0.5 * log_sigma2 - 0.5 * log_nu_minus_2_pi - half_nu_plus_1 * term;
        nll -= ll;

        // EGARCH update
        let z = r / sigma;
        log_sigma2 = omega + beta * log_sigma2 + alpha * z.abs() + gamma * z;
    }

    nll
}

// Numerical gradient
fn compute_gradient(returns: &[f64], params: &[f64], eps: f64) -> [f64; 5] {
    let mut grad = [0.0; 5];
    let base_nll = egarch_student_t_nll(returns, params);
    
    for i in 0..5 {
        let mut p_forward = params.to_vec();
        p_forward[i] += eps;
        let nll_forward = egarch_student_t_nll(returns, &p_forward);
        
        grad[i] = (nll_forward - base_nll) / eps;
    }
    grad
}

// Adam Optimizer with box constraints
fn adam_optimizer(returns: &[f64], init: [f64; 5], max_iter: usize) -> [f64; 5] {
    let mut params = init;
    let mut m = [0.0; 5];
    let mut v = [0.0; 5];
    
    let lr = 0.01;
    let beta1 = 0.9;
    let beta2 = 0.999;
    let eps = 1e-8;
    let grad_eps = 1e-5;

    for t in 1..=max_iter {
        let grad = compute_gradient(returns, &params, grad_eps);
        
        let mut update_magnitude = 0.0;
        
        for i in 0..5 {
            if grad[i].is_nan() || grad[i].is_infinite() { continue; }
            
            m[i] = beta1 * m[i] + (1.0 - beta1) * grad[i];
            v[i] = beta2 * v[i] + (1.0 - beta2) * grad[i] * grad[i];
            
            let m_hat = m[i] / (1.0 - beta1.powi(t as i32));
            let v_hat = v[i] / (1.0 - beta2.powi(t as i32));
            
            let update = lr * m_hat / (v_hat.sqrt() + eps);
            params[i] -= update;
            update_magnitude += update.abs();
        }
        
        // Apply box constraints
        params[1] = params[1].max(1e-6); // alpha > 0
        params[3] = params[3].clamp(-0.999, 0.999); // |beta| < 1
        params[4] = params[4].max(2.01); // nu > 2.01
        
        if update_magnitude < 1e-6 {
            break;
        }
    }

    params
}

pub fn forecast_garch(closes: &[f64]) -> f64 {
    if closes.len() < 2 { return 0.01; }
    
    // 1. Log returns * 100 for stability
    let mut returns = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        returns.push((closes[i] / closes[i-1]).ln() * 100.0);
    }

    // 2. Optimize
    // init: [omega, alpha, gamma, beta, nu]
    let init = [0.01, 0.1, -0.05, 0.90, 5.0];
    let best_params = adam_optimizer(&returns, init, 500);

    let omega = best_params[0];
    let alpha = best_params[1];
    let gamma = best_params[2];
    let beta = best_params[3];
    let nu = best_params[4];

    // 3. Forecast Next Variance
    let mut log_sigma2 = omega / (1.0 - beta).max(1e-6);
    
    for &r in &returns {
        let sigma2 = log_sigma2.exp();
        let sigma = sigma2.sqrt();
        let z = r / sigma;
        log_sigma2 = omega + beta * log_sigma2 + alpha * z.abs() + gamma * z;
    }
    
    let mut next_sigma2 = log_sigma2.exp();

    // 4. Fat-tail adjustment
    if nu > 2.0 {
        next_sigma2 *= nu / (nu - 2.0);
    }

    // 5. Unscale and return volatility
    (next_sigma2.sqrt() / 100.0)
}
