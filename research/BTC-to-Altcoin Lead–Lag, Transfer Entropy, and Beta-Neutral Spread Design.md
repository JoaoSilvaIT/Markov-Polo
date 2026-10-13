# BTC-to-Altcoin Lead–Lag, Transfer Entropy, and Beta-Neutral Spread Design
## Executive answer
The proposed production design should **not assume that BTC always leads ETH, SOL, and AVAX by a stable number of seconds**. Published high-frequency evidence is mixed: asynchronous lead–lag exists, but recent tick-level crypto work finds that BTC can sometimes lag, while one-minute research finds large- and medium-cap altcoins often react within the same bar and delayed reactions are more evident in less-liquid small caps. On Hyperliquid, the correct architecture is therefore an estimated, confidence-gated BTC impulse model—not a hard-coded propagation delay.[^1][^2][^3]

For the current account and two-position cap, the preferred rollout is:

1. **Immediately:** retain single-leg execution, but generate Engine A signals from a BTC-factor residual rather than raw altcoin log price; use the BTC impulse score as both an entry veto and a residual-model safety switch.
2. **Shadow mode:** record native book and trade events and estimate 5–60 second BTC-to-alt lead–lag, effective transfer entropy, and event-conditioned beta.
3. **Only after residual validation:** enable two-leg trading for pairs whose residual remains stationary out of sample after fees, funding, partial fills, and adverse-selection costs. Execute the alt leg passively and hedge each partial fill in BTC immediately with IOC; two independent ALO orders are not a beta-neutral execution method.

A short Hyperliquid one-minute pilot covering 5,132 aligned minutes from 10–13 October 2026 found zero-minute peak correlation for all three pairs. Normal-regime return betas were approximately 1.17 for ETH, 1.58 for SOL, and 1.57 for AVAX; top-1% BTC-move betas rose to approximately 1.46, 1.76, and 1.78, respectively. This is a **local calibration sample, not a production estimate**: the stress subsample has only 52 observations, no sub-minute inference is possible, and dynamic residuals did not reject a unit root in this very short sample.
## Evidence limits
A claim such as “informed flow normally hits BTC first and reaches ETH, SOL, and AVAX after 1–60 seconds” is a hypothesis to test, not a safe prior. Tick-level research documents rapid cross-asset and cross-venue lead–lag, but also reports cases in which BTC is the lagger; liquidity, fee structure, venue, volatility, and time of day all affect the direction and strength. At one-minute frequency, recent evidence suggests immediate responses for large- and medium-cap altcoins and delays of several minutes mainly for lower-trade-count small caps.[^1][^2][^4]

Hyperliquid provides live `l2Book`, `trades`, one-minute candles, BBO, and asset-context subscriptions. Its standard L2 feed is a snapshot pushed on eligible blocks, while the optional fast book contains five levels and the slow book twenty levels. Historical S3 L2 data is requester-pays, uploaded approximately monthly, may be incomplete, and does not include historical candles; this prevents a defensible sub-minute backtest without either authenticated archive access or a prospective recorder.[^5][^6][^7]

The one-minute pilot should consequently be interpreted only as an initialization aid. Its full metrics, cost scenarios, and residual diagnostics are supplied as machine-readable companion files.
## Causal data design
### Clock and sampling
Retain every native `l2Book` and `trades` message for research even if the live strategy updates features only every five seconds. Store both exchange timestamp and local monotonic receive timestamp, plus sequence diagnostics, reconnect boundaries, and stale-feed flags. Five-second resampling has a five-second lag grid and cannot distinguish simultaneous transmission from a 1–4 second lead.

For each asset, construct:

- Best bid and ask, midpoint, spread, and microprice.
- Depth imbalance at 1, 5, and 20 levels.
- Book additions, cancellations, and depletion by side.
- Signed taker notional from the trades feed.
- Five-second and one-minute log returns.
- Funding, open interest, mark–oracle premium, and data-quality flags.

The size-weighted microprice is

\[
m_t=\frac{a_t q^b_t+b_t q^a_t}{q^b_t+q^a_t},
\]

where \(a_t,b_t\) are best ask and bid and \(q^a_t,q^b_t\) their displayed sizes. For depth \(D\), use distance weights \(w_i=\exp(-\eta d_i)\):

\[
O^{(D)}_t=
\frac{\sum_{i=1}^{D}w_i q^b_{i,t}-\sum_{i=1}^{D}w_i q^a_{i,t}}
{\sum_{i=1}^{D}w_i q^b_{i,t}+\sum_{i=1}^{D}w_i q^a_{i,t}}.
\]

Order-book and trade-flow imbalance have predictive content at very short horizons, but that content decays quickly; maker fills also exhibit adverse selection because high fill probability is negatively associated with subsequent return. This supports using OBI for a seconds-scale veto or hedge trigger, not treating it as a durable directional forecast.[^8][^9]
### Lead–lag estimator
If native event timestamps are retained, estimate the lagged Hayashi–Yoshida correlation between BTC microprice increments \(\Delta X_i\) and alt increments \(\Delta Y_j\):

\[
\widehat\rho_{HY}(\ell)=
\frac{\sum_{i,j}\Delta X_i\Delta Y_j
\mathbf 1\{(t_{i-1},t_i]\cap(s_{j-1}+\ell,s_j+\ell]\neq\varnothing\}}
{\sqrt{\sum_i(\Delta X_i)^2\sum_j(\Delta Y_j)^2}}.
\]

The estimated lead is

\[
\widehat\ell=\arg\max_{\ell\in\{-60,-59,\ldots,60\}\text{ s}}
|\widehat\rho_{HY}(\ell)|.
\]

HY is appropriate for asynchronous observations and avoids previous-tick synchronization; its lagged contrast has a consistent lead–lag interpretation under general nonsynchronous sampling. With regularly sampled five-second snapshots, ordinary lagged correlation or a distributed-lag regression is simpler and HY adds little.[^10][^11]

Define the lead–lag ratio using the convention that positive \(\ell\) means BTC is shifted earlier and therefore leads:

\[
LLR_{BTC\rightarrow k}=
\frac{\sum_{\ell=1}^{L}\widehat\rho^2(+\ell)}
{\sum_{\ell=1}^{L}\widehat\rho^2(-\ell)}.
\]

An LLR above one indicates more correlation mass in the BTC-leading direction; it does **not** establish structural causality. Use stationary-block bootstrap confidence intervals by day, not IID standard errors.[^12][^11]
### Transfer entropy
For discretized BTC state \(X_t\), future alt return state \(Y_{t+h}\), and conditioning state \(C_t\), estimate conditional transfer entropy:

\[
TE_{BTC\rightarrow k}(h)=
I\!\left(Y_{t+h};X_t^{(p)}\mid Y_t^{(q)},C_t\right)
=
\sum p(y',\mathbf y,\mathbf x,c)
\log\frac{p(y'\mid\mathbf y,\mathbf x,c)}{p(y'\mid\mathbf y,c)}.
\]

Use three equiprobable states for BTC return, BTC OBI, and alt future return; \(p=q=1\); horizons 5, 10, 15, 30, and 60 seconds; and a minimum rolling sample of 10,000 observations. Add Dirichlet pseudocount \(1/2\) and exponential decay only after the initial window is populated. Higher-dimensional binning will be too sparse for a low-latency estimator.

Finite-sample TE is upward biased. Compute effective transfer entropy as observed TE minus the mean TE from at least 200 block-preserving or phase-randomized BTC surrogates, and retain a direction only when observed TE exceeds the 95th percentile of the null distribution. TE should refresh every 5–15 minutes in a background task and supply a confidence/regime flag; a linear distributed-lag model should drive the hot path.[^13][^14][^15]
### Distributed-lag model
At five-second resolution fit, causally and separately by regime,

\[
r^k_t=a_k+
\sum_{j=0}^{12}b_{k,j}r^{BTC}_{t-j}+
\sum_{j=0}^{12}c_{k,j}\bar O^{BTC}_{t-j}
+\phi_k r^k_{t-1}+u^k_t.
\]

The propagation horizon is the smallest \(h\) containing 90% of absolute lag mass:

\[
h_{90}=5\min\left\{m:
\frac{\sum_{j=0}^{m}|b_{k,j}|}{\sum_{j=0}^{12}|b_{k,j}|}\ge0.9
\right\}\text{ seconds}.
\]

Report \(h_{90}\), the peak coefficient lag, LLR, effective TE, and net out-of-sample improvement over the alt-only autoregression. A lead is tradable only if it survives spread, fees, queue position, and latency; research on fragmented BTC venues shows that fees materially determine both measured leadership and realizable taker PnL.[^4]
## BTC impulse score
### Exact formulation
At decision time \(t\), compute strictly backward-looking BTC returns from microprice:

\[
r^{BTC}_{h,t}=\log m^{BTC}_t-\log m^{BTC}_{t-h},
\qquad h\in\{5,15,60\}\text{ seconds}.
\]

Winsorize each return at eight causal EWMA standard deviations. Define the multiscale OBI composite

\[
\bar O_t=0.50\,EWMA_{1s}(O^{(5)}_t)
+0.30\,EWMA_{5s}(O^{(5)}_t)
+0.20\,EWMA_{30s}(O^{(20)}_t).
\]

The signed BTC factor shock, in log-return units, is

\[
S^{BTC}_t=
0.50r^{BTC}_{5,t}+0.30r^{BTC}_{15,t}+0.20r^{BTC}_{60,t}
+\gamma_t\widehat\sigma^{BTC}_{5,t}\bar O_t.
\]

Estimate \(\gamma_t\) by robust RLS in the BTC-only equation \(r^{BTC}_{t,t+5s}=a+\gamma\sigma^{BTC}_{5,t}\bar O_t+e_t\), clamp it to \([0,1]\), and initialize it at 0.25. This makes the OBI contribution dimensionally comparable to returns instead of assigning an arbitrary z-score coefficient.

For altcoin \(k\), define

\[
I_{BTC\rightarrow k}(t)=
\operatorname{clip}\!\left(
\frac{\widehat\beta^{eff}_{k,t}S^{BTC}_t}
{\sqrt{\widehat\sigma^2_{\epsilon,k,5s,t}+
\widehat\beta_{k,t}^{2}\widehat\sigma^2_{model,BTC,5s,t}}},
-8,8\right).
\]

Here \(\widehat\sigma_{\epsilon,k,5s,t}\) is causal idiosyncratic residual volatility and \(\widehat\sigma_{model,BTC,5s,t}\) is uncertainty in the BTC shock forecast. The effective beta blends normal and cascade filters:

\[
\widehat\beta^{eff}_{k,t}=(1-p^{tail}_t)\widehat\beta^N_{k,t}
+p^{tail}_t\widehat\beta^T_{k,t},
\]

where \(p^{tail}_t\) is a logistic score from BTC volatility, absolute taker imbalance, spread, depth depletion, open-interest change, and liquidation messages. In the absence of a reliable tail filter, use the normal beta and lower the long-entry veto threshold during stress; do not multiply beta by an unsupported fixed “cascade multiplier.”
### Decision policy
Recommended shadow defaults are:

- Veto an Engine A alt long when \(I\le-1.0\); hard veto and cancel resting long entries when \(I\le-1.5\).
- Apply symmetric thresholds to shorts when \(I\ge+1.0\) and \(+1.5\).
- Require effective TE to be positive and significant, LLR above 1.1, and the distributed-lag model to beat the alt-only baseline before allowing the “BTC leads” label. If reliability fails, the score remains a contemporaneous systematic-risk indicator but not a lead signal.
- Suppress mean reversion entirely when the standardized residual and BTC factor shock have the same adverse sign and \(p^{tail}_t>0.5\).
- Recalibrate thresholds by walk-forward net PnL rather than mapping them mechanically to the existing `BTC Z_15` thresholds.
## Dynamic beta model
### Robust online RLS
For a lightweight production beta, use completed one-minute returns:

\[
r^k_t=\alpha_{k,t}+\beta_{k,t}r^{BTC}_t+\epsilon_{k,t}.
\]

With \(x_t=[1,r^{BTC}_t]^\top\), forgetting factor \(\lambda\), and Huber-clipped innovation \(e_t\), update

\[
K_t=\frac{P_{t-1}x_t}{\lambda+x_t^\top P_{t-1}x_t},
\]

\[
\theta_t=\theta_{t-1}+K_t e_t,
\qquad
P_t=\lambda^{-1}(P_{t-1}-K_tx_t^\top P_{t-1}).
\]

Set \(\lambda=\exp(\log(0.5)/2880)\), a two-day half-life at one-minute frequency. Maintain a fast six-hour covariance beta and a slow seven-day RLS beta; shrink the fast estimate toward the slow one according to effective sample size. Freeze beta updates during data gaps, detected cascades, and open pair positions. A changing beta is useful, but dynamic models do not guarantee out-of-sample profitability and can hide relationship breakdown.[^16][^17]
### Partial-cointegration Kalman model
For a true residual spread, avoid a Kalman regression whose random-walk beta can absorb every divergence. Use a constrained partial-cointegration state:

\[
\begin{aligned}
p^k_t &= \alpha_t+\beta_t x^{BTC}_t+s_t+v_t,\\
\alpha_t&=\alpha_{t-1}+w^\alpha_t,\\
\beta_t&=\beta_{t-1}+w^\beta_t,\\
s_t&=\mu+\phi(s_{t-1}-\mu)+w^s_t,
\end{aligned}
\]

where \(p^k_t=\log P^k_t\) and \(x^{BTC}_t=\log P^{BTC}_t-\log P^{BTC}_{anchor}\). Centering BTC log price reduces numerical collinearity between intercept and beta.

The state vector \(z_t=[\alpha_t,\beta_t,s_t]^\top\) has

\[
F=\begin{bmatrix}1&0&0\\0&1&0\\0&0&\phi\end{bmatrix},
\quad
c=\begin{bmatrix}0\\0\\(1-\phi)\mu\end{bmatrix},
\quad
H_t=\begin{bmatrix}1&x^{BTC}_t&1\end{bmatrix}.
\]

Standard Kalman prediction and update are

\[
\widehat z^-_t=F\widehat z_{t-1}+c,
\quad P^-_t=FP_{t-1}F^\top+Q,
\]

\[
\nu_t=p^k_t-H_t\widehat z^-_t,
\quad S_t=H_tP^-_tH_t^\top+R_t,
\]

\[
K_t=P^-_tH_t^\top S_t^{-1},
\quad \widehat z_t=\widehat z^-_t+K_t\nu_t,
\quad P_t=(I-K_tH_t)P^-_t.
\]

Trade the filtered spread \(\widehat s_t\), with z-score

\[
z^s_t=\frac{\widehat s_t-\widehat\mu_t}
{\sqrt{P_{t,ss}+\widehat\sigma^2_{OU,t}}},
\]

not the raw post-regression residual alone. Initialize \(\phi=\exp[-\log(2)/120]\) for a 120-minute half-life, then estimate it with the existing jackknife-corrected OU routine. The OU half-life is \(-\log(2)/\log\phi\); jackknifing can reduce finite-sample bias but increases variance.

Suggested initial process scales per one-minute step are:

- Intercept random-walk standard deviation: 0.5 basis points.
- Beta random-walk standard deviation: \(0.002/\sqrt{1440}\) per minute.
- Observation-noise standard deviation: max of one basis point and half the relative bid–ask spread.
- OU process variance: estimated from the rolling innovation variance conditional on \(\phi\).

These are starting values for likelihood or walk-forward tuning, not universal constants. Reject the model if beta hits its clamp repeatedly, Kalman normalized innovations fail whiteness checks, or the residual loses stationarity.
### Stationarity gate
A high price correlation is not enough. Before enabling pair orders, require on each rolling training window:

- Engle–Granger or ADF residual p-value below 1%, with multiplicity control across pairs and parameter trials.
- KPSS stationarity not rejected.
- Positive OU speed and half-life between 10 minutes and 8 hours for this strategy cadence.
- Beta not at a clamp and no more than 25% change over one estimated half-life.
- Out-of-sample mean crossing and first-passage distributions compatible with the fitted OU.
- Positive expected edge after measured fills, fees, funding, and forced hedge costs.

The short pilot’s RLS residual ADF p-values were 0.62 for ETH, 0.67 for SOL, and 0.72 for AVAX, so it supplies **no current evidence** that these adaptive residuals are stationary. The result is inconclusive because the sample is only about 3.6 days, but it is sufficient to reject immediate live activation.
## Empirical beta priors
The following one-minute estimates are suitable only as seeds and safety clamps. “Normal” means BTC absolute one-minute return below its sample 80th percentile; “stress” means the top 1% of BTC absolute one-minute returns.

| Pair | Normal beta | Stress beta | Stress / normal | Peak lag | Peak correlation | 1–5 min LLR |
|---|---:|---:|---:|---:|---:|---:|
| ETH/BTC | 1.17 | 1.46 | 1.25 | 0 min | 0.85 | 1.28 |
| SOL/BTC | 1.58 | 1.76 | 1.12 | 0 min | 0.79 | 2.14 |
| AVAX/BTC | 1.57 | 1.78 | 1.13 | 0 min | 0.61 | 4.56 |



The zero-minute peak means one-minute bars cannot establish a sub-minute tradable delay. Positive LLR values suggest asymmetric lag mass, especially for SOL and AVAX, but the sample is too short for a stable conclusion. Broader evidence also indicates that altcoin beta and correlation can rise in liquidity stress, while liquidation-cascade research finds price impact itself can rise materially during cascades.

Recommended beta initialization is therefore:

| Asset | Seed beta | Hard clamp | Tail seed | Minimum one-leg notional |
|---|---:|---:|---:|---:|
| ETH | 1.20 | 0.60–2.00 | 1.45 | 11 USDC |
| SOL | 1.60 | 0.70–2.80 | 1.80 | 11 USDC |
| AVAX | 1.60 | 0.70–3.20 | 1.80 | 11 USDC |

The tail seeds should be used only until enough event-conditioned data exists. A hierarchical shrinkage estimate is preferable: shrink each sparse tail beta toward \(1.15\beta^N\), then let observed cascade data progressively dominate.
## Option analysis
### Statistical exposure
For a single long alt position of notional \(N_A\), first-order PnL is

\[
d\Pi_{single}\approx N_A(\beta_t r^{BTC}_t+\epsilon_t).
\]

A BTC veto avoids entering against a detected factor shock, but once filled the position remains exposed to subsequent BTC moves. A beta-neutral long-residual trade holds long alt and short BTC:

\[
d\Pi_{pair}\approx N_A r^A_t-N_Br^{BTC}_t,
\qquad N_B=|\beta_t|N_A,
\]

so first-order systematic exposure cancels and

\[
d\Pi_{pair}\approx N_A\epsilon_t.
\]

For gross notional \(G=N_A+N_B\), size

\[
N_A=\frac{G}{1+|\beta_t|},
\qquad N_B=\frac{|\beta_t|G}{1+|\beta_t|}.
\]
### Operational trade-offs
| Dimension | Single-leg beta-gated OU | Two-leg residual OU |
|---|---|---|
| Factor risk | Reduced only at entry | Approximately neutral after both fills |
| Signal quality | Raw alt OU can mistake market moves for reversion | Directly targets idiosyncratic spread |
| Fees and spread | One leg | Two legs; forced hedge often taker |
| Funding | One asset | Differential funding on both legs |
| Execution | Simple ALO | Partial-fill and legging state machine required |
| Position cap | Uses one of two slots | Consumes both slots |
| Small account | Better operationally | Feasible notionally but higher friction concentration |
| Model risk | Heuristic veto and residual contamination | Cointegration failure, beta drift, and false stationarity |
| Best use | Current live-test phase | Validated relative-value deployment |

The relative-value model is mathematically cleaner but not automatically more profitable. It replaces systematic price risk with basis-model risk, two-sided adverse selection, synchronization risk, and differential funding.
## Costs and minimum size
### Fee correction
The stated 1.0 basis point maker and 3.5 basis point taker schedule does not match the current published validator-operated perp table. As of the cited Hyperliquid documentation, base tier is 1.5 basis points maker and 4.5 basis points taker; Tier 1 above 5 million USD of 14-day weighted volume is 1.2 and 4.0 basis points before applicable discounts. Production code should query the account’s `userFees` response and never hard-code either schedule.

For equal fee \(f\) on all fills, pair round-trip cost is

\[
C_{RT}=2f(N_A+N_B)=2fG.
\]

Measured as basis points of alt-leg notional, it is \(2f(1+|\beta|)\). With current base fees, maker–maker costs about 6.6 basis points of ETH-leg notional at beta 1.2 and 7.8 basis points for SOL or AVAX at beta 1.6. If the alt leg is maker and the BTC hedge is taker, the round-trip cost is

\[
C_{RT}=2(f_MN_A+f_TN_B),
\]

or 13.8 basis points of ETH-leg notional at beta 1.2 and 17.4 basis points at beta 1.6 under current base rates. These figures exclude spread crossing, slippage, adverse selection, and funding.



The claim that every pair needs only 22 USDC gross assumes beta exactly one. If each leg must be at least 11 USDC, then

\[
G_{min}=(1+|\beta|)\max\left(11,\frac{11}{|\beta|}\right).
\]

At the recommended seeds, minimum gross is approximately 24.2 USDC for ETH/BTC and 28.6 USDC for SOL/BTC or AVAX/BTC. This remains affordable, but the pair consumes both allowed positions and leaves less room for rounding, price movement, and emergency resizing.
### Funding differential
Hyperliquid funding is paid hourly; positive funding is paid by longs to shorts, and the payment uses position size times oracle price times funding rate. For a long-alt/short-BTC spread, expected hourly cost is

\[
C^{fund}_h=N_A f^A_h-N_B f^{BTC}_h.
\]

For the opposite spread, the sign reverses. At entry, add expected funding over the OU expected holding time plus a stress buffer to the required edge. Block entries shortly before the hour when expected funding exceeds 20% of gross modeled edge; do not assume a pair is funding-neutral merely because it is beta-neutral.
## Execution protocol
Hyperliquid ALO orders rest and do not execute immediately; IOC cancels any unfilled amount. There is no atomic multi-asset pair fill in the cited API, so execution neutrality must be created by a state machine.

Recommended sequence:

1. Validate residual signal, stationarity gate, fee/funding edge, margin, minimum notionals, and both books’ freshness.
2. Submit **only the alt leg** as ALO at a price consistent with the required residual edge.
3. On every partial alt fill \(\Delta N_A\), immediately submit BTC IOC for \(|\beta_{frozen}|\Delta N_A\).
4. Freeze beta at trade admission; never chase a moving hedge ratio while entry is partially filled.
5. If BTC hedge fill is incomplete after 250–500 ms, replace with a more aggressive IOC within a predefined slippage cap.
6. If still unhedged at the hard deadline, cancel the remaining alt ALO and flatten the unmatched alt quantity. Safety outranks fee optimization.
7. Track actual hedge error \(E=N_B-\beta_{frozen}N_A\); rebalance only when its expected risk exceeds fee plus spread cost.
8. Exit using the same incremental protocol. Place the less-liquid alt reduce-only ALO first and hedge BTC IOC as alt fills.

Submitting two ALO legs simultaneously creates an uncontrolled option: one leg can fill because it is adversely selected while the other remains queued. Maker-order research in BTC perpetuals documents precisely this fill-probability/adverse-return relationship. The execution backtest must therefore simulate queue position and partial fills rather than assuming every touched maker quote fills.
## Rust blueprint
### Core types
```rust
use std::collections::VecDeque;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub enum Asset { Btc, Eth, Sol, Avax }

#[derive(Clone, Copy, Debug)]
pub struct BookFeatures {
    pub exchange_ms: i64,
    pub recv_at: Instant,
    pub mid: f64,
    pub micro: f64,
    pub spread_bps: f64,
    pub obi_5: f64,
    pub obi_20: f64,
    pub taker_imb_5s: f64,
    pub stale: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct AssetParams {
    pub beta_seed: f64,
    pub beta_min: f64,
    pub beta_max: f64,
    pub beta_tail_seed: f64,
    pub impulse_veto: f64,
    pub impulse_hard: f64,
    pub ou_half_life_seed_min: f64,
    pub min_leg_usdc: f64,
}

pub const ETH_PARAMS: AssetParams = AssetParams {
    beta_seed: 1.20, beta_min: 0.60, beta_max: 2.00,
    beta_tail_seed: 1.45, impulse_veto: 1.0, impulse_hard: 1.5,
    ou_half_life_seed_min: 120.0, min_leg_usdc: 11.0,
};
pub const SOL_PARAMS: AssetParams = AssetParams {
    beta_seed: 1.60, beta_min: 0.70, beta_max: 2.80,
    beta_tail_seed: 1.80, impulse_veto: 1.0, impulse_hard: 1.5,
    ou_half_life_seed_min: 120.0, min_leg_usdc: 11.0,
};
pub const AVAX_PARAMS: AssetParams = AssetParams {
    beta_seed: 1.60, beta_min: 0.70, beta_max: 3.20,
    beta_tail_seed: 1.80, impulse_veto: 1.0, impulse_hard: 1.5,
    ou_half_life_seed_min: 120.0, min_leg_usdc: 11.0,
};
```
### Robust RLS beta
```rust
#[derive(Clone, Debug)]
pub struct RlsBeta {
    theta: [f64; 2],       // alpha, beta
    p: [[f64; 2]; 2],
    lambda: f64,
    beta_min: f64,
    beta_max: f64,
    huber_sigma: f64,
}

impl RlsBeta {
    pub fn update(&mut self, btc_ret: f64, alt_ret: f64, sigma: f64) -> f64 {
        let x = [1.0, btc_ret];
        let px = [
            self.p * x + self.p[^1] * x[^1],
            self.p[^1] * x + self.p[^1][^1] * x[^1],
        ];
        let denom = self.lambda + x * px + x[^1] * px[^1];
        let k = [px / denom, px[^1] / denom];
        let raw = alt_ret - self.theta - self.theta[^1] * btc_ret;
        let cap = self.huber_sigma * sigma.max(1e-8);
        let err = raw.clamp(-cap, cap);
        self.theta += k * err;
        self.theta[^1] += k[^1] * err;

        let old = self.p;
        for i in 0..2 {
            for j in 0..2 {
                let x_tp_j = x * old[j] + x[^1] * old[^1][j];
                self.p[i][j] = (old[i][j] - k[i] * x_tp_j) / self.lambda;
            }
        }
        self.theta[^1] = self.theta[^1].clamp(self.beta_min, self.beta_max);
        raw
    }

    pub fn beta(&self) -> f64 { self.theta[^1] }
}
```

Use `lambda = (0.5_f64).powf(1.0 / 2880.0)`, initialize `P` diagonally at `[1e-4, 10.0]`, and update only on completed, valid one-minute bars. Persist state atomically and restore it only if the saved timestamp and configuration hash match.
### Impulse engine
```rust
#[derive(Clone, Debug)]
pub struct BtcImpulse {
    micro_hist: VecDeque<(i64, f64)>,
    obi_1s: f64,
    obi_5s: f64,
    obi_30s: f64,
    gamma: f64,
    btc_sigma_5s: f64,
    model_sigma_5s: f64,
}

impl BtcImpulse {
    fn log_return(&self, now_ms: i64, horizon_ms: i64) -> Option<f64> {
        let now = self.micro_hist.back()?.1;
        let then = self.micro_hist.iter()
            .rev()
            .find(|(ts, _)| *ts <= now_ms - horizon_ms)?.1;
        Some((now / then).ln())
    }

    pub fn factor_shock(&self, now_ms: i64) -> Option<f64> {
        let r5 = self.log_return(now_ms, 5_000)?;
        let r15 = self.log_return(now_ms, 15_000)?;
        let r60 = self.log_return(now_ms, 60_000)?;
        let obi = 0.50 * self.obi_1s + 0.30 * self.obi_5s + 0.20 * self.obi_30s;
        Some(0.50 * r5 + 0.30 * r15 + 0.20 * r60
            + self.gamma.clamp(0.0, 1.0) * self.btc_sigma_5s * obi)
    }

    pub fn score(
        &self, now_ms: i64, beta_eff: f64,
        alt_idio_sigma_5s: f64,
    ) -> Option<f64> {
        let shock = self.factor_shock(now_ms)?;
        let var = alt_idio_sigma_5s.powi(2)
            + beta_eff.powi(2) * self.model_sigma_5s.powi(2);
        Some((beta_eff * shock / var.sqrt().max(1e-8)).clamp(-8.0, 8.0))
    }
}
```

Each feature update must carry an `as_of` timestamp. Reject a trading decision if BTC or target-alt book age exceeds two expected block intervals, if candle closure is incomplete, or if a reconnect gap overlaps any feature horizon.
### Pair state machine
```rust
#[derive(Clone, Debug)]
pub enum PairExecState {
    Idle,
    AltAloWorking {
        pair_id: u128,
        beta_frozen: f64,
        target_alt_usdc: f64,
        filled_alt_usdc: f64,
        hedged_btc_usdc: f64,
        started: Instant,
    },
    HedgeIocPending {
        pair_id: u128,
        beta_frozen: f64,
        unhedged_alt_usdc: f64,
        deadline: Instant,
    },
    Open {
        pair_id: u128,
        beta_frozen: f64,
        alt_usdc: f64,
        btc_usdc: f64,
    },
    UnwindEmergency { pair_id: u128 },
}

pub struct PairAdmission {
    pub residual_z: f64,
    pub impulse: f64,
    pub stationarity_ok: bool,
    pub expected_edge_bps: f64,
    pub all_in_cost_bps: f64,
    pub funding_buffer_bps: f64,
    pub books_fresh: bool,
}

impl PairAdmission {
    pub fn allowed(&self) -> bool {
        self.stationarity_ok
            && self.books_fresh
            && self.expected_edge_bps
                > self.all_in_cost_bps + self.funding_buffer_bps
            && self.impulse.abs() < 1.5
    }
}
```

Run market-data ingestion, model updates, signal evaluation, order routing, and reconciliation as separate `tokio` tasks connected by bounded channels. The risk task owns the authoritative pair state; order tasks may propose transitions but cannot mutate exposure directly. Use client order IDs encoding pair ID, leg, direction, and sequence, and reconcile from `orderUpdates` plus `userFills` after every reconnect.
## Backtest and rollout
### Required experiment
Collect at least 30 days of native events and preferably 90 days spanning quiet trading and several liquidation episodes. Hyperliquid’s archive warning means local prospective capture should be treated as the source of truth.

Use nested walk-forward evaluation:

- Train beta, OU, impulse coefficients, and thresholds only on prior data.
- Purge at least the maximum 60-second feature horizon plus expected holding overlap around folds.
- Define normal and tail regimes without future information.
- Bootstrap complete days and complete cascade events.
- Simulate book queue, ALO rejection, partial fills, IOC slippage, message latency, funding timestamps, and the exact account fee response.
- Compare raw-alt OU, raw-alt OU plus current veto, residual-driven single leg, and fully hedged residual pair.

Primary metrics should be net expectancy per completed cycle, worst unhedged exposure, fill-conditioned markout, turnover, funding drag, expected shortfall, and probability that both legs satisfy minimum notional after rounding. Sharpe alone is insufficient because pair failures concentrate in synchronization and cascade tails.
### Promotion gates
Promote the residual model as a single-leg signal filter when it reduces adverse entries and drawdown out of sample without materially lowering maker fill quality. Promote the two-leg pair only if all of the following hold:

- Net expectancy remains positive under current base fees, not only the assumed discounted schedule.
- At least 1,000 completed simulated pair cycles and 100 live shadow opportunities exist per pair.
- Emergency unhedged duration stays below 500 ms at the 99th percentile.
- Forced-hedge and partial-fill costs are included, with no assumed atomicity.
- Stationarity gates pass in at least 80% of rolling windows and after major stress events.
- Performance remains positive with beta perturbed by ±20%, latency doubled, and maker fill probability reduced.

For a 150 USDC account, the recommended production choice is therefore **single-leg residual-driven Engine A plus the causal BTC impulse veto**. At 1,000–2,500 USDC, two-leg trading becomes operationally more attractive, but only after the pair-specific evidence clears the promotion gates; account size does not repair a nonstationary residual or unsafe execution protocol.

---

## References

1. [Price deviation in the stablecoin market and lead-lag relationships in ...](https://eprints.soton.ac.uk/504578/) - This thesis offers new insights on the exogenous and endogenous drivers of volatility and price devi...

2. [Price Transmission from Bitcoin to Altcoins: High-Frequency ...](https://link.springer.com/article/10.1007/s10690-026-09589-z?error=cookies_not_supported&code=4f0b6dc8-de4d-4507-978b-f91e05ab516c) - This study investigates the lagged price transmission from Bitcoin (BTC) to altcoins (ALTs) using hi...

3. [Lead-lag and Volatility Point Change Estimations for Cryptocurrencies](http://cejsh.icm.edu.pl/cejsh/element/bwmeta1.element.ojs-doi-10_7172_2353-6845_jbfe_2024_1_5)

4. [Fragmentation, Price Formation and Cross-Impact in Bitcoin Markets](https://www.tandfonline.com/doi/full/10.1080/1350486X.2022.2080083) - In the light of micro-scale inefficiencies due to the highly fragmented bitcoin trading landscape, w...

5. [llms-full.txt](https://hyperliquid.gitbook.io/hyperliquid-docs/llms-full.txt)

6. [Subscriptions - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)

7. [Historical data | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data)

8. [[PDF] Forecasting short term returns in Cryptocurrency Futures Using ...](https://dspace.cuni.cz/bitstream/handle/20.500.11956/213492/130479105.pdf?sequence=3&isAllowed=y)

9. [[2502.18625] To Make, or to Take, That Is the Question: Impact of ...](https://ar5iv.labs.arxiv.org/html/2502.18625) - Working at a very granular level, using data from a live trading experiment on the Binance linear Bi...

10. [Estimation of the lead-lag parameter from non-synchronous data](https://arxiv.org/html/1303.4871v1)

11. [LeadLagCopy.pdf](https://arxiv.org/vc/arxiv/papers/1111/1111.7103v1.pdf)

12. [High Frequency Lead/lag Relationships - Empirical facts](https://www.alphaxiv.org/abs/1111.7103v2) - An empirical study by Nicolas Huth and Frédéric Abergel identifies and characterizes high-frequency ...

13. [Rényi Transfer Entropy Estimators for Financial Time Series - MDPI](https://www.mdpi.com/2673-4591/5/1/33) - In this paper, we discuss the statistical coherence between financial time series in terms of Rényi’...

14. [4. Results](https://pmc.ncbi.nlm.nih.gov/articles/PMC13409483/) - In light of growing concerns about sustainable development and green innovation, the green market ha...

15. [Using transfer entropy to measure information flows between financial markets](https://www.econstor.eu/bitstream/10419/79614/1/722172478.pdf)

16. [From Cointegration to Out-of-Sample Failure: A Pairs-Trading Case Study on PEP-KO](https://arxiv.org/pdf/2609.35359v1.pdf)

17. [Cointegration-Based Pairs Trading with a Dynamic Hedge ...](https://www.hse.ru/en/edu/vkr/1160300975) - The thesis examines a cointegration-based pairs trading strategy with static and dynamic hedge ratio...

