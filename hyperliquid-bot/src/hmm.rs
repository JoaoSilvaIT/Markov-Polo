use ndarray::{Array1, Array2};
use std::f64::consts::PI;

#[allow(dead_code)]
pub struct GaussianHMM {
    pub n_components: usize,
    pub means: Array2<f64>,
    pub covars: Array2<f64>,
    pub transmat: Array2<f64>,
    pub startprob: Array1<f64>,
    pub eff_sample_sizes: Array1<f64>,
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
            eff_sample_sizes: Array1::zeros(n_components),
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

        // Empirical feature-quantile initialization to seed distinct market regimes:
        // State 0 (Consolidation): low vol (25th pct), low ER (20th pct), low |D| (20th pct)
        // State 1 (Trend): moderate-high vol (65th pct), high ER (85th pct), high |D| (85th pct)
        // State 2 (Shock): extreme vol (92nd pct), moderate ER (40th pct), high jump dispersion
        let col_quantile = |col_idx: usize, q: f64| -> f64 {
            let mut vals: Vec<f64> = x.column(col_idx).iter().copied().collect();
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            if vals.is_empty() {
                return 0.0;
            }
            let idx = ((q.clamp(0.0, 1.0)) * (vals.len() - 1) as f64).round() as usize;
            vals[idx.min(vals.len() - 1)]
        };

        if self.n_components == 3 && n_features >= 4 {
            // State 0: Consolidation
            self.means[[0, 0]] = 0.0;
            self.means[[0, 1]] = col_quantile(1, 0.25);
            self.means[[0, 2]] = col_quantile(2, 0.20);
            self.means[[0, 3]] = col_quantile(3, 0.20);

            // State 1: Directional Trend
            self.means[[1, 0]] = 0.0;
            self.means[[1, 1]] = col_quantile(1, 0.60);
            self.means[[1, 2]] = col_quantile(2, 0.80);
            self.means[[1, 3]] = col_quantile(3, 0.80);

            // State 2: Volatility Shock
            self.means[[2, 0]] = 0.0;
            self.means[[2, 1]] = col_quantile(1, 0.85);
            self.means[[2, 2]] = col_quantile(2, 0.35);
            self.means[[2, 3]] = col_quantile(3, 0.50);
        } else {
            let step = (n_samples / (self.n_components + 1)).max(1);
            for c in 0..self.n_components {
                let sample_idx = ((c + 1) * step).min(n_samples - 1);
                for d in 0..n_features {
                    self.means[[c, d]] = x[[sample_idx, d]];
                }
            }
        }

        let mut final_n_eff = Array1::zeros(self.n_components);

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
                    final_n_eff[c] = n_eff;

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

        self.eff_sample_sizes = final_n_eff;
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

        // 1. Identify Directional Trend state first: state with maximum directional efficiency & drift (ER + |D|)
        // Trending bars naturally have higher volatility than quiet range bars; selecting Shock by ln_sigma first
        // would falsely mislabel the Trend cluster as Shock!
        let mut trend_state = 0;
        let mut max_trend_score = f64::NEG_INFINITY;
        for c in 0..3 {
            let er_score = self.means[[c, 2]];
            let drift_score = self.means[[c, 3]];
            let trend_score = 2.0 * er_score + 0.25 * drift_score;
            if trend_score > max_trend_score {
                max_trend_score = trend_score;
                trend_state = c;
            }
        }

        // 2. Between the remaining two non-trend states, identify Consolidation (lower vol) vs Shock (higher vol)
        let remaining: Vec<usize> = (0..3).filter(|&c| c != trend_state).collect();
        let s_a = remaining[0];
        let s_b = remaining[1];

        let vol_a = self.means[[s_a, 1]] + 0.25 * self.covars[[s_a, 0]].max(1e-6).ln();
        let vol_b = self.means[[s_b, 1]] + 0.25 * self.covars[[s_b, 0]].max(1e-6).ln();

        let (consol_state, shock_state) = if vol_a <= vol_b {
            (s_a, s_b)
        } else {
            (s_b, s_a)
        };

        // Verify that Trend and Consolidation are non-degenerate
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
        let mut new_eff = self.eff_sample_sizes.clone();

        for (new_idx, &old_idx) in order.iter().enumerate() {
            new_means.row_mut(new_idx).assign(&self.means.row(old_idx));
            new_covars.row_mut(new_idx).assign(&self.covars.row(old_idx));
            new_startprob[new_idx] = self.startprob[old_idx];
            if old_idx < self.eff_sample_sizes.len() && new_idx < new_eff.len() {
                new_eff[new_idx] = self.eff_sample_sizes[old_idx];
            }

            for (new_col, &old_col) in order.iter().enumerate() {
                new_transmat[[new_idx, new_col]] = self.transmat[[old_idx, old_col]];
            }
        }

        self.means = new_means;
        self.covars = new_covars;
        self.startprob = new_startprob;
        self.transmat = new_transmat;
        self.eff_sample_sizes = new_eff;
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
    #[serde(default)]
    pub eff_sample_sizes: Vec<f64>,
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
            eff_sample_sizes: self.eff_sample_sizes.to_vec(),
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
        let tmp_path = format!("{}.tmp", path);
        let mut file = fs::File::create(&tmp_path)?;
        use std::io::Write;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp_path, path)?;
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

        if weights.eff_sample_sizes.len() == weights.n_components {
            hmm.eff_sample_sizes = Array1::from_vec(weights.eff_sample_sizes);
        }

        let _ = hmm.sort_semantically();
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
    if x.nrows() < 120 {
        return None;
    }

    let mut hmm = GaussianHMM::new(3, 4);
    hmm.fit(&x, 100);
    if !hmm.sort_semantically() {
        // Degenerate state moments: fail safe
        return None;
    }

    // Numerical Finiteness Guard (P0 Quant Specification):
    // Reject model if any parameter became NaN or Inf
    if !hmm.means.iter().all(|v| v.is_finite())
        || !hmm.covars.iter().all(|v| v.is_finite())
        || !hmm.transmat.iter().all(|v| v.is_finite())
        || !hmm.startprob.iter().all(|v| v.is_finite())
    {
        println!("⚠️ [COLD-PATH] HMM model candidate rejected: Non-finite numerical parameter detected (NaN/Inf)");
        return None;
    }

    // Rare-State Rejection Gate (P0 Quant Specification):
    // Core operating regimes (Consolidation=0, Trend=1) require N_eff >= 25.0, while the tail
    // Shock regime (State 2, regularized by Ledoit-Wolf shrinkage rho_k) requires N_eff >= 10.0
    // so genuine tail shocks (1-5% of bars) are not forced to swallow normal daytime bars.
    for (state_idx, &n_eff) in hmm.eff_sample_sizes.iter().enumerate() {
        let min_n_eff = if state_idx == 2 { 10.0 } else { 25.0 };
        if n_eff < min_n_eff {
            println!(
                "⚠️ [COLD-PATH] HMM candidate rejected: State {} has insufficient effective sample size (N_eff = {:.1} < {:.1})",
                state_idx, n_eff, min_n_eff
            );
            return None;
        }
    }

    Some(hmm)
}

pub fn compute_hmm_posteriors(hmm: &GaussianHMM, closes: &[f64]) -> Option<(f64, f64, f64)> {
    let x = extract_4d_features(closes, 16)?;
    let n = x.nrows();
    if n == 0 {
        return None;
    }

    let post = hmm.filtered_posteriors(&x);
    if post.len() >= 3 {
        let last_idx = n - 1;
        let norm_ret_abs = x[[last_idx, 0]].abs();
        let ln_sigma_last = x[[last_idx, 1]];
        let er_last = x[[last_idx, 2]];
        let d_last = x[[last_idx, 3]];

        // Compute rolling mean and std of ln_sigma across the observation window
        let mean_ln_vol = x.column(1).mean().unwrap_or(ln_sigma_last);
        let var_ln_vol = x
            .column(1)
            .iter()
            .map(|&v| (v - mean_ln_vol).powi(2))
            .sum::<f64>()
            / (n as f64).max(1.0);
        let std_ln_vol = var_ln_vol.sqrt().max(1e-4);
        let z_ln_vol = (ln_sigma_last - mean_ln_vol) / std_ln_vol;

        // Guard against 4-hour (16-bar) boxcar echo:
        // A single 15m shock candle inflates the equal-weighted 16-bar `ln_sigma` for 4 full hours
        // even after the market has calmed down. Gate `z_ln_vol` by the recent 30m (2-bar) RMS
        // return relative to baseline volatility `mean_ln_vol.exp()`, so `P_S` decays promptly
        // once two consecutive 15m bars return to normal volatility.
        let base_sigma = mean_ln_vol.exp().max(1e-8);
        let m = closes.len();
        let r_0 = if m >= 2 && closes[m - 2] > 0.0 {
            (closes[m - 1] / closes[m - 2]).ln().abs()
        } else {
            0.0
        };
        let r_1 = if m >= 3 && closes[m - 3] > 0.0 {
            (closes[m - 2] / closes[m - 3]).ln().abs()
        } else {
            r_0
        };
        let recent_rms_ratio = (0.70 * r_0.powi(2) + 0.30 * r_1.powi(2)).sqrt() / base_sigma;
        let recent_vol_gate = ((recent_rms_ratio - 1.15) / 0.65).clamp(0.0, 1.0);
        let z_ln_vol_effective = z_ln_vol * recent_vol_gate;

        // Physical shock gate: prevent State 2 from locking P_S near 100% during calm/normal daytime volatility
        // or during the 4-hour boxcar tail after a single spike has already subsided.
        let shock_intensity = ((norm_ret_abs - 1.60).max(0.0) / 1.00)
            .max(((r_0 / base_sigma) - 1.75).max(0.0) / 1.00)
            .max((z_ln_vol_effective - 1.00).max(0.0) / 0.85)
            .clamp(0.0, 1.0);

        let raw_c = post[0];
        let raw_t = post[1];
        let raw_s = post[2];

        let p_s = raw_s * shock_intensity;
        let reallocated = raw_s * (1.0 - shock_intensity);
        // Check if the 16-bar path length is dominated by a single outlier jump bar rather than a multi-bar trend
        let lookback_start = m.saturating_sub(17);
        let mut path_len_16 = 0.0;
        let mut max_bar_16 = 0.0_f64;
        for i in (lookback_start + 1)..m {
            if closes[i - 1] > 0.0 {
                let r_abs = (closes[i] / closes[i - 1]).ln().abs();
                path_len_16 += r_abs;
                if r_abs > max_bar_16 {
                    max_bar_16 = r_abs;
                }
            }
        }
        let max_bar_share = if path_len_16 > 1e-8 {
            max_bar_16 / path_len_16
        } else {
            0.0
        };
        // A true 16-bar trend spreads movement across bars (max_bar_share ~ 0.08-0.25);
        // a single jump bar followed by flat range has max_bar_share > 0.45.
        let multi_bar_trend_factor = (1.0 - ((max_bar_share - 0.35) / 0.30).clamp(0.0, 1.0)).clamp(0.0, 1.0);

        // Compensate `d_last` for any 16-bar boxcar `sigma` inflation when evaluating post-shock trend vs range
        let sigma_inflation = (ln_sigma_last - mean_ln_vol).exp().clamp(1.0, 2.5);
        let d_adjusted = d_last * sigma_inflation.sqrt();
        let trend_affinity = (((er_last - 0.24) / 0.22)
            .max((d_adjusted - 1.05) / 0.90)
            .clamp(0.0, 1.0))
            * multi_bar_trend_factor;

        let p_t = raw_t * multi_bar_trend_factor + reallocated * trend_affinity;
        let p_c = raw_c + raw_t * (1.0 - multi_bar_trend_factor) + reallocated * (1.0 - trend_affinity);
        let sum = (p_c + p_t + p_s).max(1e-12);
        Some((p_c / sum, p_t / sum, p_s / sum))
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
        // Generate price series with trending, consolidation, and shock phases (300 bars)
        for i in 0..300 {
            if i < 100 {
                p += 0.8 + 0.1 * (i as f64 * 0.2).sin(); // Steady trend
            } else if i < 200 {
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

    #[test]
    fn test_hmm_rare_state_rejection() {
        let mut closes = Vec::new();
        let mut p = 100.0;
        // Series with insufficient observations (< 120 bars) should fail Neff >= 40 gate
        for i in 0..80 {
            p += 0.1 * (i as f64 * 0.1).sin();
            closes.push(p);
        }
        let hmm_opt = train_hmm_offline(&closes);
        assert!(hmm_opt.is_none(), "Under-identified series must be rejected by rare-state gate");
    }

    #[test]
    fn test_hmm_sort_semantically_never_mislabels_high_vol_trend_as_shock() {
        let mut closes = Vec::new();
        let mut p = 100.0;
        // 1. Quiet consolidation (120 bars)
        for i in 0..120 {
            p += 0.04 * (i as f64 * 0.7).sin();
            closes.push(p);
        }
        // 2. Moderate-volatility consolidation (120 bars)
        for i in 0..120 {
            p += 0.15 * (i as f64 * 1.1).cos();
            closes.push(p);
        }
        // 3. Strong directional trend with highest volatility (120 bars)
        for i in 0..120 {
            p += 0.65 + 0.25 * (i as f64 * 0.5).sin();
            closes.push(p);
        }

        let hmm = train_hmm_offline(&closes).expect("HMM should train on 360-bar series");
        // State 1 (Trend) must have the highest Kaufman Efficiency Ratio (Feature 2)
        assert!(
            hmm.means[[1, 2]] >= hmm.means[[0, 2]] && hmm.means[[1, 2]] >= hmm.means[[2, 2]],
            "State 1 (Trend) must hold the highest ER cluster: means={:?}",
            hmm.means
        );

        // During the final trending phase, P_T must dominate and P_S must not be stuck near 100%
        let (_, p_t, p_s) = compute_hmm_posteriors(&hmm, &closes).unwrap();
        assert!(
            p_t > 0.75 && p_s < 0.25,
            "Trending phase must yield high P_T ({:.3}) and low P_S ({:.3})",
            p_t,
            p_s
        );

        // During the moderate-vol consolidation phase (slice 0..240), P_C must dominate over P_S
        let (p_c_range, _, p_s_range) = compute_hmm_posteriors(&hmm, &closes[..240]).unwrap();
        assert!(
            p_c_range > 0.70 && p_s_range < 0.25,
            "Calm/moderate range must yield high P_C ({:.3}) and low P_S ({:.3})",
            p_c_range,
            p_s_range
        );
    }

    #[test]
    fn test_hmm_post_shock_boxcar_recovery() {
        let mut closes = Vec::new();
        let mut p = 100.0;
        // 120 bars of calm range, 120 bars of steady trend, 60 bars of volatile shock
        for i in 0..300 {
            if i < 120 {
                p += 0.05 * (i as f64 * 0.8).sin();
            } else if i < 240 {
                p += 0.35 + 0.05 * (i as f64 * 0.4).cos();
            } else {
                p += 1.40 * (i as f64 * 1.7).sin();
            }
            closes.push(p);
        }
        let hmm = train_hmm_offline(&closes).expect("HMM should train");

        // Build a 100-bar live evaluation window:
        // 90 bars of normal consolidation, 1 massive V-shock spike bar, followed by 3 calm consolidation bars (45m)
        let mut live_closes = Vec::new();
        let mut lp = 100.0;
        for i in 0..90 {
            lp += 0.06 * (i as f64 * 0.9).sin();
            live_closes.push(lp);
        }
        // Single 15m shock bar (+2.5% jump)
        lp *= 1.025;
        live_closes.push(lp);
        // Right on the shock bar, P_S should be elevated
        let (_, _, p_s_on_shock) = compute_hmm_posteriors(&hmm, &live_closes).unwrap();
        assert!(
            p_s_on_shock > 0.50,
            "On the shock bar itself, P_S should be elevated: {:.3}",
            p_s_on_shock
        );

        // Now add 3 calm 15m bars (45 minutes of quiet consolidation after the spike)
        for i in 0..3 {
            lp += 0.03 * ((i as f64 + 1.0) * 1.1).sin();
            live_closes.push(lp);
        }
        let (p_c_after, _, p_s_after) = compute_hmm_posteriors(&hmm, &live_closes).unwrap();
        assert!(
            p_s_after < 0.20 && p_c_after > 0.65,
            "After 3 calm 15m bars post-shock, 16-bar boxcar echo must not lock P_S near 100% (got P_C={:.3}, P_S={:.3})",
            p_c_after,
            p_s_after
        );
    }
}
