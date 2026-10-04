use ndarray::{Array1, Array2, Axis, s};
use std::f64::consts::PI;

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

    pub fn sort_by_sharpe(&mut self) {
        let mut sharpes = vec![(0, 0.0); self.n_components];
        for c in 0..self.n_components {
            let mean = self.means[[c, 0]]; // Feature 0 is return
            let var = self.covars[[c, 0]].max(1e-8);
            let sharpe = mean / var.sqrt();
            sharpes[c] = (c, sharpe);
        }
        sharpes.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        let mut new_means = self.means.clone();
        let mut new_covars = self.covars.clone();
        let mut new_startprob = self.startprob.clone();
        let mut new_transmat = self.transmat.clone();

        for (i, &(old_idx, _)) in sharpes.iter().enumerate() {
            new_means.row_mut(i).assign(&self.means.row(old_idx));
            new_covars.row_mut(i).assign(&self.covars.row(old_idx));
            new_startprob[i] = self.startprob[old_idx];
            
            for (j, &(old_col, _)) in sharpes.iter().enumerate() {
                new_transmat[[i, j]] = self.transmat[[old_idx, old_col]];
            }
        }

        self.means = new_means;
        self.covars = new_covars;
        self.startprob = new_startprob;
        self.transmat = new_transmat;
    }
}

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
    hmm.sort_by_sharpe(); // Label switching fix by Sharpe ratio
    
    hmm.predict(&x) as i32
}
