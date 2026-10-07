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
        
        // K-Means-ish init for means
        for c in 0..self.n_components {
            for d in 0..n_features {
                self.means[[c, d]] = x[[c % n_samples, d]]; // Dumb init
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

            // transmat (Skipping full xi matrix for simplicity, just normalizing)
            for i in 0..self.n_components {
                let mut denom = 0.0;
                for t in 0..n_samples-1 { denom += gamma[[t, i]]; }
                if denom > 0.0 {
                    for j in 0..self.n_components {
                        let mut num = 0.0;
                        for t in 0..n_samples-1 {
                            let p = log_alpha[[t, i]] + log_transmat[[i, j]] + log_emiss[[t+1, j]] + log_beta[[t+1, j]];
                            num += p.exp();
                        }
                        self.transmat[[i, j]] = num / denom;
                    }
                }
                // Normalize row
                let row_sum: f64 = self.transmat.row(i).sum();
                if row_sum > 0.0 {
                    let mut r = self.transmat.row_mut(i);
                    r.mapv_inplace(|v| v / row_sum);
                }
            }

            // Means and Covars
            for c in 0..self.n_components {
                let mut denom = 0.0;
                for t in 0..n_samples { denom += gamma[[t, c]]; }
                if denom > 1e-10 {
                    for d in 0..n_features {
                        let mut num_mean = 0.0;
                        for t in 0..n_samples { num_mean += gamma[[t, c]] * x[[t, d]]; }
                        self.means[[c, d]] = num_mean / denom;

                        let mut num_var = 0.0;
                        for t in 0..n_samples {
                            num_var += gamma[[t, c]] * (x[[t, d]] - self.means[[c, d]]).powi(2);
                        }
                        self.covars[[c, d]] = (num_var / denom).max(1e-6);
                    }
                }
            }
        }
    }

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

    pub fn sort_by_energy(&mut self) {
        // Institutional state labeling (Research consensus):
        // Sort states by emission return-energy: q_k = mu_k^2 + sigma_k^2
        // - State 0: Lowest return-energy / variance (Calm Consolidation)
        // - State 1: Moderate energy (Directional / Transition)
        // - State 2: Highest return-energy / variance (High-volatility cascade / stress shock)
        let mut energies = vec![(0, 0.0); self.n_components];
        for c in 0..self.n_components {
            let mean = self.means[[c, 0]]; // Feature 0 is return
            let var = self.covars[[c, 0]].max(1e-8);
            let energy = mean.powi(2) + var;
            energies[c] = (c, energy);
        }
        energies.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut new_means = self.means.clone();
        let mut new_covars = self.covars.clone();
        let mut new_startprob = self.startprob.clone();
        let mut new_transmat = self.transmat.clone();

        for (i, &(old_idx, _)) in energies.iter().enumerate() {
            new_means.row_mut(i).assign(&self.means.row(old_idx));
            new_covars.row_mut(i).assign(&self.covars.row(old_idx));
            new_startprob[i] = self.startprob[old_idx];
            
            for (j, &(old_col, _)) in energies.iter().enumerate() {
                new_transmat[[i, j]] = self.transmat[[old_idx, old_col]];
            }
        }

        self.means = new_means;
        self.covars = new_covars;
        self.startprob = new_startprob;
        self.transmat = new_transmat;
    }

    #[allow(dead_code)]
    pub fn sort_by_sharpe(&mut self) {
        self.sort_by_energy();
    }
}

#[allow(dead_code)]
pub fn detect_hmm_regime(closes: &[f64]) -> i32 {
    if closes.len() < 12 { return 0; }
    
    // Create returns
    let mut returns = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        returns.push(closes[i] / closes[i-1] - 1.0);
    }
    
    // Create rolling vol (10-period)
    let mut vols = vec![0.0; returns.len()];
    for i in 10..returns.len() {
        let slice = &returns[i-10..i];
        let mean: f64 = slice.iter().sum::<f64>() / 10.0;
        let var: f64 = slice.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / 10.0;
        vols[i] = var.sqrt();
    }
    
    let valid_start = 10;
    if returns.len() - valid_start < 5 { return 0; }
    
    let n_samples = returns.len() - valid_start;
    let mut x = Array2::zeros((n_samples, 2));
    for i in 0..n_samples {
        x[[i, 0]] = returns[i + valid_start];
        x[[i, 1]] = vols[i + valid_start];
    }
    
    let mut hmm = GaussianHMM::new(3, 2);
    hmm.fit(&x, 100);
    hmm.sort_by_energy(); // Energy-based state labeling
    
    hmm.predict(&x) as i32
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

pub fn train_hmm_offline(closes: &[f64]) -> Option<GaussianHMM> {
    if closes.len() < 12 { return None; }
    
    let mut returns = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        returns.push(closes[i] / closes[i-1] - 1.0);
    }
    
    let mut vols = vec![0.0; returns.len()];
    for i in 10..returns.len() {
        let slice = &returns[i-10..i];
        let mean: f64 = slice.iter().sum::<f64>() / 10.0;
        let var: f64 = slice.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / 10.0;
        vols[i] = var.sqrt();
    }
    
    let valid_start = 10;
    if returns.len() - valid_start < 5 { return None; }
    
    let n_samples = returns.len() - valid_start;
    let mut x = Array2::zeros((n_samples, 2));
    for i in 0..n_samples {
        x[[i, 0]] = returns[i + valid_start];
        x[[i, 1]] = vols[i + valid_start];
    }
    
    let mut hmm = GaussianHMM::new(3, 2);
    hmm.fit(&x, 100);
    hmm.sort_by_energy();
    
    Some(hmm)
}

pub fn compute_hmm_posteriors(hmm: &GaussianHMM, closes: &[f64]) -> Option<(f64, f64, f64)> {
    if closes.len() < 15 {
        return None;
    }

    let mut returns = Vec::with_capacity(closes.len() - 1);
    for i in 1..closes.len() {
        returns.push(closes[i] / closes[i - 1] - 1.0);
    }

    let mut vols = vec![0.0; returns.len()];
    for i in 10..returns.len() {
        let slice = &returns[i - 10..i];
        let mean: f64 = slice.iter().sum::<f64>() / 10.0;
        let var: f64 = slice.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / 10.0;
        vols[i] = var.sqrt();
    }

    let valid_start = 10;
    if returns.len() - valid_start < 5 {
        return None;
    }

    let n_samples = returns.len() - valid_start;
    let mut x = Array2::zeros((n_samples, 2));
    for i in 0..n_samples {
        x[[i, 0]] = returns[i + valid_start];
        x[[i, 1]] = vols[i + valid_start];
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
        for i in 0..100 {
            p += (i as f64 * 0.1).sin();
            closes.push(p);
        }

        let hmm = train_hmm_offline(&closes).expect("Trained HMM");
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
