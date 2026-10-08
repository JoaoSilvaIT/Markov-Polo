use ndarray::{Array1, Array2};
use std::f64::consts::PI;

#[allow(dead_code)]
pub struct GaussianHMM {
    pub n_components: usize,
    pub means: Array2<f64>,
    pub covars: Array2<f64>,
    pub transmat: Array2<f64>,
    pub startprob: Array1<f64>,
}

fn logsumexp(arr: &Array1<f64>) -> f64 {
    let max = arr.fold(f64::NEG_INFINITY, |m, &v| m.max(v));
    if max == f64::NEG_INFINITY { return max; }
    let sum: f64 = arr.iter().map(|&x| (x - max).exp()).sum();
    max + sum.ln()
}

impl GaussianHMM {
    pub fn new(n_components: usize, n_features: usize) -> Self {
        Self {
            n_components,
            means: Array2::zeros((n_components, n_features)),
            covars: Array2::ones((n_components, n_features)),
            transmat: Array2::from_elem((n_components, n_components), 1.0 / n_components as f64),
            startprob: Array1::from_elem(n_components, 1.0 / n_components as f64),
        }
    }

    fn log_gaussian_prob(&self, x: &Array2<f64>) -> Array2<f64> {
        let n_samples = x.nrows();
        let n_features = x.ncols();
        let mut log_prob = Array2::zeros((n_samples, self.n_components));

        for c in 0..self.n_components {
            for t in 0..n_samples {
                let mut p = 0.0;
                for d in 0..n_features {
                    let mu = self.means[[c, d]];
                    let sigma = self.covars[[c, d]].max(1e-6);
                    p += -0.5 * ((2.0 * PI * sigma).ln() + (x[[t, d]] - mu).powi(2) / sigma);
                }
                log_prob[[t, c]] = p;
            }
        }
        log_prob
    }

    pub fn fit(&mut self, x: &Array2<f64>, n_iter: usize) {
        let n_samples = x.nrows();
        let n_features = x.ncols();
        
        // Quantile initialization for means to seed distinct market regimes
        let step = (n_samples / (self.n_components + 1)).max(1);
        for c in 0..self.n_components {
            let sample_idx = ((c + 1) * step).min(n_samples - 1);
            for d in 0..n_features {
                self.means[[c, d]] = x[[sample_idx, d]];
            }
        }

        for _ in 0..n_iter {
            let log_emiss = self.log_gaussian_prob(x);
            let log_transmat = self.transmat.mapv(|v| v.max(1e-15).ln());
            let log_startprob = self.startprob.mapv(|v| v.max(1e-15).ln());

            let mut log_alpha = Array2::zeros((n_samples, self.n_components));
            for c in 0..self.n_components {
                log_alpha[[0, c]] = log_startprob[c] + log_emiss[[0, c]];
            }

            for t in 1..n_samples {
                for c in 0..self.n_components {
                    let mut prev = Array1::zeros(self.n_components);
                    for i in 0..self.n_components {
                        prev[i] = log_alpha[[t - 1, i]] + log_transmat[[i, c]];
                    }
                    log_alpha[[t, c]] = logsumexp(&prev) + log_emiss[[t, c]];
                }
            }

            let mut log_beta = Array2::zeros((n_samples, self.n_components));
            for t in (0..n_samples - 1).rev() {
                for c in 0..self.n_components {
                    let mut next = Array1::zeros(self.n_components);
                    for i in 0..self.n_components {
                        next[i] = log_transmat[[c, i]] + log_emiss[[t + 1, i]] + log_beta[[t + 1, i]];
                    }
                    log_beta[[t, c]] = logsumexp(&next);
                }
            }

            let mut log_gamma = Array2::zeros((n_samples, self.n_components));
            for t in 0..n_samples {
                let mut row = Array1::zeros(self.n_components);
                for c in 0..self.n_components {
                    row[c] = log_alpha[[t, c]] + log_beta[[t, c]];
                }
                let denom = logsumexp(&row);
                for c in 0..self.n_components {
                    log_gamma[[t, c]] = row[c] - denom;
                }
            }
            
            let gamma = log_gamma.mapv(|v| v.exp());

            // M-Step
            // startprob
            for c in 0..self.n_components {
                self.startprob[c] = gamma[[0, c]];
            }
            let sum_sp = self.startprob.sum();
            self.startprob.mapv_inplace(|v| v / sum_sp);

            // transmat: Calculate normalized xi_t(i, j) in log space before exponentiation
            let mut xi_sum = Array2::<f64>::zeros((self.n_components, self.n_components));
            for t in 0..n_samples - 1 {
                // Compute normalization constant C_t = logsumexp_{i, j}(...)
                let mut joint_terms = Vec::with_capacity(self.n_components * self.n_components);
                for i in 0..self.n_components {
                    for j in 0..self.n_components {
                        let term = log_alpha[[t, i]] + log_transmat[[i, j]] + log_emiss[[t + 1, j]] + log_beta[[t + 1, j]];
                        joint_terms.push(term);
                    }
                }
                let joint_arr = Array1::from_vec(joint_terms);
                let c_t = logsumexp(&joint_arr);

                for i in 0..self.n_components {
                    for j in 0..self.n_components {
                        let log_xi = log_alpha[[t, i]] + log_transmat[[i, j]] + log_emiss[[t + 1, j]] + log_beta[[t + 1, j]] - c_t;
                        xi_sum[[i, j]] += log_xi.exp();
                    }
                }
            }

            for i in 0..self.n_components {
                let denom = gamma.slice(ndarray::s![0..n_samples - 1, i]).sum();
                if denom > 1e-10 {
                    for j in 0..self.n_components {
                        self.transmat[[i, j]] = (xi_sum[[i, j]] / denom).max(1e-15);
                    }
                }
                // Normalize row
                let row_sum: f64 = self.transmat.row(i).sum();
                if row_sum > 0.0 {
                    let mut r = self.transmat.row_mut(i);
                    r.mapv_inplace(|v| v / row_sum);
                }
            }

            // Pooled feature variance target T_d
            let mut pooled_var = Array1::<f64>::zeros(n_features);
            for d in 0..n_features {
                let mean_d: f64 = x.column(d).mean().unwrap_or(0.0);
                let mut sum_sq = 0.0;
                for t in 0..n_samples {
                    sum_sq += (x[[t, d]] - mean_d).powi(2);
                }
                pooled_var[d] = (sum_sq / (n_samples as f64).max(1.0)).max(1e-3);
            }

            // Means and Covars with State-Weighted Ledoit-Wolf Linear Shrinkage
            for c in 0..self.n_components {
                let mut denom = 0.0;
                for t in 0..n_samples { denom += gamma[[t, c]]; }
                if denom > 1e-10 {
                    // Effective sample size Neff_k = (sum gamma)^2 / (sum gamma^2)
                    let mut sum_sq_gamma = 0.0;
                    for t in 0..n_samples {
                        sum_sq_gamma += gamma[[t, c]].powi(2);
                    }
                    let n_eff = (denom * denom) / sum_sq_gamma.max(1e-10);

                    // Base shrinkage rho = 0.10, scaling up if effective sample size is small (< 80)
                    let rho_k = if n_eff < 80.0 {
                        (1.0 - (n_eff / 80.0) * 0.90).clamp(0.10, 0.90)
                    } else {
                        0.10
                    };

                    for d in 0..n_features {
                        let mut num_mean = 0.0;
                        for t in 0..n_samples { num_mean += gamma[[t, c]] * x[[t, d]]; }
                        self.means[[c, d]] = num_mean / denom;

                        let mut num_var = 0.0;
                        for t in 0..n_samples {
                            num_var += gamma[[t, c]] * (x[[t, d]] - self.means[[c, d]]).powi(2);
                        }
                        let sample_var = (num_var / denom).max(1e-6);

                        // Linear shrinkage toward pooled target T_d
                        let target_var = pooled_var[d];
                        let shrunk_var = (1.0 - rho_k) * sample_var + rho_k * target_var;

                        // Eigenvalue / variance floor at 1e-3
                        self.covars[[c, d]] = shrunk_var.max(1e-3);
                    }
                }
            }

            // Condition number guard: ensure max_var / min_var <= 1e4
            for c in 0..self.n_components {
                let min_v = (0..n_features).map(|d| self.covars[[c, d]]).fold(f64::INFINITY, f64::min);
                let max_v = (0..n_features).map(|d| self.covars[[c, d]]).fold(f64::NEG_INFINITY, f64::max);
                if min_v > 0.0 && (max_v / min_v) > 1e4 {
                    let floor_v = max_v / 1e4;
                    for d in 0..n_features {
                        self.covars[[c, d]] = self.covars[[c, d]].max(floor_v);
                    }
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn predict(&self, x: &Array2<f64>) -> usize {
        let log_emiss = self.log_gaussian_prob(x);
        let n_samples = x.nrows();
        let log_transmat = self.transmat.mapv(|v| v.max(1e-15).ln());
        let log_startprob = self.startprob.mapv(|v| v.max(1e-15).ln());

        let mut viterbi = Array2::zeros((n_samples, self.n_components));
        let mut backpointer = Array2::<usize>::zeros((n_samples, self.n_components));

        for c in 0..self.n_components {
            viterbi[[0, c]] = log_startprob[c] + log_emiss[[0, c]];
        }

        for t in 1..n_samples {
            for c in 0..self.n_components {
                let mut max_val = f64::NEG_INFINITY;
                let mut max_state = 0;
                for i in 0..self.n_components {
                    let val = viterbi[[t - 1, i]] + log_transmat[[i, c]];
                    if val > max_val {
                        max_val = val;
                        max_state = i;
                    }
                }
                viterbi[[t, c]] = max_val + log_emiss[[t, c]];
                backpointer[[t, c]] = max_state;
            }
        }

        let mut best_last = 0;
        let mut best_val = f64::NEG_INFINITY;
        for c in 0..self.n_components {
            if viterbi[[n_samples - 1, c]] > best_val {
                best_val = viterbi[[n_samples - 1, c]];
                best_last = c;
            }
        }
        best_last
    }

    pub fn filtered_posteriors(&self, x: &Array2<f64>) -> Array1<f64> {
        let n_samples = x.nrows();
        if n_samples == 0 {
            return Array1::from_elem(self.n_components, 1.0 / self.n_components as f64);
        }

        let log_emiss = self.log_gaussian_prob(x);
        let log_transmat = self.transmat.mapv(|v| v.max(1e-15).ln());
        let log_startprob = self.startprob.mapv(|v| v.max(1e-15).ln());

        let mut log_alpha = Array2::zeros((n_samples, self.n_components));
        for c in 0..self.n_components {
            log_alpha[[0, c]] = log_startprob[c] + log_emiss[[0, c]];
        }

        for t in 1..n_samples {
            for c in 0..self.n_components {
                let mut prev = Array1::zeros(self.n_components);
                for i in 0..self.n_components {
                    prev[i] = log_alpha[[t - 1, i]] + log_transmat[[i, c]];
                }
                log_alpha[[t, c]] = logsumexp(&prev) + log_emiss[[t, c]];
            }
        }

        let last_row = log_alpha.row(n_samples - 1).to_owned();
        let denom = logsumexp(&last_row);
        let mut post = Array1::zeros(self.n_components);
        for c in 0..self.n_components {
            post[c] = (last_row[c] - denom).exp();
        }
        post
    }

    pub fn sort_semantically(&mut self) -> bool {
        // Institutional semantic state labeling:
        // - State 0: Consolidation (low ER, calm/moderate volatility)
        // - State 1: Directional Trend (high ER, high directional drift |D|)
        // - State 2: Volatility Shock (extreme volatility / return energy)
        if self.n_components != 3 || self.means.ncols() < 4 {
            return false;
        }

        // 1. Identify Shock state: state with maximum volatility (Feature 1: ln_sigma)
        let mut shock_state = 0;
        let mut max_vol = f64::NEG_INFINITY;
        for c in 0..3 {
            let vol_score = self.means[[c, 1]];
            if vol_score > max_vol {
                max_vol = vol_score;
                shock_state = c;
            }
        }

        // 2. Between the remaining two states, identify Trend vs Consolidation by ER
        let remaining: Vec<usize> = (0..3).filter(|&c| c != shock_state).collect();
        let s_a = remaining[0];
        let s_b = remaining[1];

        let er_a = self.means[[s_a, 2]];
        let er_b = self.means[[s_b, 2]];

        let (consol_state, trend_state) = if er_a <= er_b {
            (s_a, s_b)
        } else {
            (s_b, s_a)
        };

        // If both states have virtually indistinguishable ER, verify if directional drift |D| separates them
        let er_diff = (self.means[[trend_state, 2]] - self.means[[consol_state, 2]]).abs();
        if er_diff < 1e-4 {
            let d_trend = self.means[[trend_state, 3]];
            let d_consol = self.means[[consol_state, 3]];
            if (d_trend - d_consol).abs() < 1e-4 {
                return false;
            }
        }

        let order = [consol_state, trend_state, shock_state];
        let mut new_means = self.means.clone();
        let mut new_covars = self.covars.clone();
        let mut new_startprob = self.startprob.clone();
        let mut new_transmat = self.transmat.clone();

        for (new_idx, &old_idx) in order.iter().enumerate() {
            new_means.row_mut(new_idx).assign(&self.means.row(old_idx));
            new_covars.row_mut(new_idx).assign(&self.covars.row(old_idx));
            new_startprob[new_idx] = self.startprob[old_idx];

            for (new_col, &old_col) in order.iter().enumerate() {
                new_transmat[[new_idx, new_col]] = self.transmat[[old_idx, old_col]];
            }
        }

        self.means = new_means;
        self.covars = new_covars;
        self.startprob = new_startprob;
        self.transmat = new_transmat;
        true
    }

    #[allow(dead_code)]
    pub fn sort_by_energy(&mut self) {
        let _ = self.sort_semantically();
    }
}

use serde::{Serialize, Deserialize};
use std::fs;

#[derive(Serialize, Deserialize)]
pub struct HmmWeights {
    pub n_components: usize,
    pub n_features: usize,
    pub means: Vec<Vec<f64>>,
    pub covars: Vec<Vec<f64>>,
    pub transmat: Vec<Vec<f64>>,
    pub startprob: Vec<f64>,
}

impl GaussianHMM {
    pub fn save_weights(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let n_features = self.means.ncols();
        let mut weights = HmmWeights {
            n_components: self.n_components,
            n_features,
            means: vec![vec![0.0; n_features]; self.n_components],
            covars: vec![vec![0.0; n_features]; self.n_components],
            transmat: vec![vec![0.0; self.n_components]; self.n_components],
            startprob: self.startprob.to_vec(),
        };

        for i in 0..self.n_components {
            for j in 0..n_features {
                weights.means[i][j] = self.means[[i, j]];
                weights.covars[i][j] = self.covars[[i, j]];
            }
            for j in 0..self.n_components {
                weights.transmat[i][j] = self.transmat[[i, j]];
            }
        }

        let json = serde_json::to_string_pretty(&weights)?;
        fs::write(path, json)?;
        Ok(())
    }

    pub fn load_weights(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let json = fs::read_to_string(path)?;
        let weights: HmmWeights = serde_json::from_str(&json)?;
        if weights.n_features != 4 || weights.n_components != 3 {
            return Err("Incompatible HMM weights shape: requires 3 components and 4 features".into());
        }

        let mut hmm = Self::new(weights.n_components, weights.n_features);
        hmm.startprob = Array1::from_vec(weights.startprob);

        for i in 0..weights.n_components {
            for j in 0..weights.n_features {
                hmm.means[[i, j]] = weights.means[i][j];
                hmm.covars[[i, j]] = weights.covars[i][j];
            }
            for j in 0..weights.n_components {
                hmm.transmat[[i, j]] = weights.transmat[i][j];
            }
        }

        Ok(hmm)
    }
}
use serde_json;

/// Extracts 4D causal observation features:
/// 0: Normalized return (r_t / sigma_t)
/// 1: Log volatility ln(sigma_t)
/// 2: Kaufman Efficiency Ratio ER_t(16)
/// 3: Absolute Directional Drift |D_t(16)|
pub fn extract_4d_features(closes: &[f64], n_lookback: usize) -> Option<Array2<f64>> {
    let n = closes.len();
    if n <= n_lookback + 2 {
        return None;
    }

    let mut returns = Vec::with_capacity(n - 1);
    for i in 1..n {
        returns.push((closes[i] / closes[i - 1]).ln());
    }

    let n_samples = n - 1 - n_lookback;
    let mut x = Array2::zeros((n_samples, 4));

    for (sample_idx, t) in (n_lookback..n - 1).enumerate() {
        // Prior window of returns for volatility estimation
        let start_idx = t + 1 - n_lookback;
        let ret_slice = &returns[start_idx..=t];
        let mean_ret: f64 = ret_slice.iter().sum::<f64>() / ret_slice.len() as f64;
        let var_ret: f64 = ret_slice.iter().map(|&r| (r - mean_ret).powi(2)).sum::<f64>()
            / (ret_slice.len() - 1).max(1) as f64;
        let sigma = var_ret.sqrt().max(1e-6);

        let r_t = returns[t];
        let norm_ret = r_t / sigma;
        let ln_sigma = sigma.ln();

        // Kaufman Efficiency Ratio (KER):
        // Net change over lookback window / sum of absolute incremental changes
        let p_end = closes[t + 1].ln();
        let p_start = closes[t + 1 - n_lookback].ln();
        let net_change = (p_end - p_start).abs();

        let mut path_length = 0.0;
        for i in (t + 1 - n_lookback + 1)..=t + 1 {
            path_length += (closes[i].ln() - closes[i - 1].ln()).abs();
        }
        let er = (net_change / (path_length + 1e-8)).clamp(0.0, 1.0);

        // Absolute Directional Drift:
        let d_t = net_change / (sigma * (n_lookback as f64).sqrt());

        x[[sample_idx, 0]] = norm_ret;
        x[[sample_idx, 1]] = ln_sigma;
        x[[sample_idx, 2]] = er;
        x[[sample_idx, 3]] = d_t;
    }

    Some(x)
}

pub fn train_hmm_offline(closes: &[f64]) -> Option<GaussianHMM> {
    let x = extract_4d_features(closes, 16)?;
    if x.nrows() < 10 {
        return None;
    }

    let mut hmm = GaussianHMM::new(3, 4);
    hmm.fit(&x, 100);
    if !hmm.sort_semantically() {
        // Degenerate state moments: fail safe
        return None;
    }

    Some(hmm)
}

pub fn compute_hmm_posteriors(hmm: &GaussianHMM, closes: &[f64]) -> Option<(f64, f64, f64)> {
    let x = extract_4d_features(closes, 16)?;
    if x.nrows() == 0 {
        return None;
    }

    let post = hmm.filtered_posteriors(&x);
    if post.len() >= 3 {
        Some((post[0], post[1], post[2]))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hmm_filtered_posteriors_sum_to_one() {
        let mut closes = Vec::new();
        let mut p = 100.0;
        // Generate price series with trending and consolidation phases
        for i in 0..120 {
            if i < 40 {
                p += 0.8 + 0.1 * (i as f64 * 0.2).sin(); // Steady trend
            } else if i < 80 {
                p += 0.05 * (i as f64 * 0.8).sin(); // Calm consolidation
            } else {
                p += 1.5 * (i as f64 * 1.5).cos(); // High vol shock
            }
            closes.push(p);
        }

        let hmm_opt = train_hmm_offline(&closes);
        assert!(hmm_opt.is_some(), "HMM training should succeed on structured series");
        let hmm = hmm_opt.unwrap();
        assert_eq!(hmm.means.ncols(), 4, "Should have 4 feature dimensions");

        let posteriors = compute_hmm_posteriors(&hmm, &closes);
        assert!(posteriors.is_some());
        let (p_c, p_t, p_s) = posteriors.unwrap();
        let sum = p_c + p_t + p_s;
        assert!((sum - 1.0).abs() < 1e-4, "Posteriors should sum to 1.0: {}", sum);
        assert!(p_c >= 0.0 && p_c <= 1.0);
        assert!(p_t >= 0.0 && p_t <= 1.0);
        assert!(p_s >= 0.0 && p_s <= 1.0);
    }
}
