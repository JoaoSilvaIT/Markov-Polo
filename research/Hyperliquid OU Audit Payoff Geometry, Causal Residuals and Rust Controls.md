# Hyperliquid OU Audit

## Scope and evidence

**The defensible development candidate is 1B combined with risk-based sizing and a modified 1C—not 1A or 1C alone. This is a testable architecture recommendation, not a finding that it maximizes out-of-sample EV, Sortino, or Calmar.** Stop-loss constraints and transaction costs change the optimal entry/exit problem; a threshold optimized for a perpetually stationary OU without these controls is not automatically optimal after adding them.[^1][^2]

The supplied ledger summary is treated as user-reported evidence. No fill export, complete wallet address, synchronized mids/marks, order lifecycle, funding ledger, or queue data was supplied. The calculations below are deterministic consequences of the stated assumptions, not an empirical Hyperliquid backtest. In particular, none of the analytical OU win probabilities below is a forecast for BTC, ETH, SOL, or AVAX.

The implementation deliverable is a dependency-free Rust decision-core crate with 24 passing deterministic unit tests, not a certified unattended trading system. It supplies causal filters, reversal confirmation, fee-aware maker quoting, sizing, and persistent invalidation; exchange signing, durable order management, calibration, and portfolio coordination remain integration responsibilities.

## Immediate mathematical corrections

### The volatility contradiction

For a correctly specified OU process,

\[
dX_t=-\kappa(X_t-\mu)dt+\eta\,dW_t,\qquad
\sigma_{\rm eq}^2=\frac{\eta^2}{2\kappa},\quad
\phi_\Delta=e^{-\kappa\Delta},\quad
t_{1/2}=\frac{\ln 2}{\kappa}.
\]

The stationary standard deviation is not the one-minute innovation standard deviation. The exact transition and stationary covariance formulas establish this distinction.[^3]

For a noise-free AR(1),

\[
X_t=c+\phi X_{t-1}+u_t,\quad
\sigma_{\rm eq}=\frac{\sigma_u}{\sqrt{1-\phi^2}},\quad
\mu=\frac{c}{1-\phi}.
\]

**Audit every variable called `sigma` before changing the stop.** Using innovation SD as stationary SD understates the equilibrium width. Conversely, ordinary additive observation noise does not generally explain a stationary variance estimate that is too small: it adds observation variance and biases autocorrelation and speed estimates. The sampled noisy OU is ARMA(1,1), not an ordinary AR(1).[^3]

Your simultaneous claims—stationary SD 12–22 bps, entry around 1.5 stationary SD, and another 30–40 bps producing only a 1.9–2.2 SD extreme—cannot all hold with a frozen center and scale. A further 30 bps is another 1.36–2.50 SD at those scales; an entry at 1.5 SD becomes approximately 2.86–4.00 SD. Moving the baseline/scale during the trade, mixing price and return standard deviations, or comparing mark-triggered stops with last-trade wicks can produce apparently inconsistent telemetry. Hyperliquid stops trigger from mark, not necessarily the candle's trade price.[^4][^5]

The fixed-time five-minute conditional SD is

\[
\sigma_{5|0}=\sigma_{\rm eq}\sqrt{1-e^{-2\kappa\cdot5}}.
\]

Over your stated 12–22 bps stationary SD and 15–120 minute half-life ranges, its endpoint range is approximately 2.84–13.38 bps. A within-five-minute maximum is not the same statistic as this endpoint innovation, so this is not by itself a rejection test. Nevertheless, frequent 30–45 bps adverse continuations against an inward OU drift demand a model/unit/conditioning audit, not a presumption that the OU is accurate.[^6]

### Sampling cannot manufacture edge

Subsampling the SAME stationary OU from one-minute to five-minute or fifteen-minute observations changes its AR coefficient and transition variance, not its stationary variance. Thus 1B works only if it identifies a genuinely different, slower economic component with larger executable-price excursions; changing the candle interval cannot turn a 20-bps stationary process into a 70-bps one.[^7][^6]

### Stops need one definition

An entry at 1.85–2.10 stationary SD with SD 12–22 bps is 22.2–46.2 bps from the center. If `2.25 a*` means a stop distance FROM ENTRY, that is approximately 50.0–104.0 bps, not automatically 120–150 bps. If it means an absolute stop boundary at 2.25 times the entry boundary FROM THE MEAN, the additional adverse distance from entry is only 1.25 times the entry distance, approximately 27.8–57.8 bps.

Use three separate fields: `entry_z_from_center`, `soft_z_from_frozen_center`, and `disaster_stop_bps_from_entry_vwap`. Never multiply dimensionless SD units and bps without explicitly converting.

### TP wins are selection-conditioned

Fifteen profitable TP fills establish that those orders captured favorable exits, not a 100% strategy win probability. Selecting observations by a profitable TP exit largely selects winners by construction; all closed inventory cycles, invalidations, partial fills, timeouts, and open inventory must enter the denominator. Even if these were 15 independent, unselected complete trades, the two-sided 95% exact binomial lower endpoint would be only approximately 78.2%; that calculation does not cure exit-selection bias.

Likewise, 155 fills are not necessarily 155 independent round trips. Reconstruct position episodes from signed inventory, rather than counting entry/exit fills as separate trades.

## Fee-aware payoff geometry

### Exact linear-contract accounting

The published base perpetual rates are 1.5 bps maker and 4.5 bps taker; actual account rates can differ, and account-specific information is available through userFees. The analysis uses your requested base rates, no builder/priority fee, and a 3-bps adverse stop-execution allowance within your reported 2–4-bps range.[^8][^9]

Let side \(s=+1\) for a long and \(s=-1\) for a short, entry price \(P_e\), filled quantity \(q\), and entry notional \(N=qP_e\). With exit price \(P_x\), entry maker rate \(f_m\), and exit fee \(f_x\), the net cash P&L is

\[
\Pi=sq(P_x-P_e)-qP_e f_m-qP_x f_x-F,
\]

where \(F\) is signed net funding/other cost in USDC. These instruments use linear-contract P&L, so fee precision should use actual entry and exit notionals, rather than always charging both legs on entry notional.[^10]

For a gross TP price return \(G\) and adverse stop distance plus execution shortfall \(d=S+\ell\), both in bps,

\[
W_s=G-10^4 f_m\left[2+sG/10^4\right],
\]

\[
L_s=d+10^4\left[f_m+f_t(1-sd/10^4)\right].
\]

These formulas exclude funding and assume maker entry/maker TP versus maker entry/taker stop. To first order, \(W=G-3\) bps and \(L=S+\ell+6\) bps. Slippage here means total adverse execution shortfall relative to the trigger/reference price; do not add spread twice if it is already included in that measurement.

For a two-outcome trade,

\[
E[\Pi]=\frac{N}{10^4}\{pW-(1-p)L\},\qquad
p_{\rm BE}=\frac{L}{W+L}.
\]

Sizing changes dollars at risk, not this break-even win rate. For equity 145 USDC, a 0.20%–0.25% planned risk budget is 0.2900–0.3625 USDC. The midpoint used below is 0.32625 USDC.

### Scenario economics

**These are payoff scenarios selected from your stated ranges, not estimated average trade outcomes.** The table uses first-order bps for readability, a 3-bps stop shortfall, no funding, and risk-sized notionals. The accompanying CSV includes exact side-specific exit-notional fees.

| Policy | Gross TP / stop | Net win / loss | Risk-sized notional | Win / loss, USDC | Break-even win rate |
|---|---:|---:|---:|---:|---:|
| Tight-stop reference | 25 / 35 bps | +22 / −44 bps | 74.15 before any notional cap | +0.163 / −0.326 | 66.67% |
| 1A, deeper entry/wide stop | 25 / 135 bps | +22 / −144 bps | 22.66 | +0.0498 / −0.326 | 86.75% |
| 1B, genuinely wider component | 70 / 95 bps | +67 / −104 bps | 31.37 | +0.210 / −0.326 | 60.82% |
| 1C, disaster-only envelope | 25 / 175 bps | +22 / −184 bps | 17.73 | +0.0390 / −0.326 | 89.32% |

The tight reference would be capped at 68 USDC if that is the notional cap; its win/loss then becomes approximately +0.1496/−0.2992 USDC. The 1C row intentionally excludes soft exits to expose catastrophe geometry; it is NOT the expected loss or break-even rate of the actual multi-exit 1C policy.

Holding the win probability artificially fixed at 85% gives approximate trade EV of −0.0066 USDC for 1A, +0.1297 for 1B, and −0.0158 for disaster-only 1C at those notionals. This is sensitivity analysis, not a prediction: changing the entry/stop/horizon changes the actual win probability and holding time.

Across the user-specified payoff ranges and 2–4 bps slippage, approximate two-outcome break-even bands are 80.0%–90.4% for 1A, 50.3%–69.8% for 1B, and 83.2%–92.5% for disaster-only 1C. Wide stops do not solve poor reward-to-loss geometry; they replace frequent moderate losses with infrequent, consequential losses.

At unchanged 68-USDC notional, the 135-bps-stop scenario loses about 0.979 USDC, while the 175-bps scenario loses about 1.251 USDC, before funding or gap overruns. Merely enlarging the stop without reducing quantity would violate the proposed account risk budget.

### Fees are not automatically under 8%

At a 55–90-bps target, the 3-bps maker/maker round-trip fee is 3.3%–5.5% of gross TP. A maker/taker exit costs 6 bps in fees before slippage; adding 2–4 bps execution shortfall yields 8–10 bps, or 8.9%–18.2% of the 55–90-bps target. Therefore the claim that the full emergency-exit friction is below 8% does not hold over that range. Funding can add or subtract more and is settled hourly.[^11]

## First-passage analysis

### Frozen OU barriers

Use stationary units \(Z=(X-\mu)/\sigma_{\rm eq}\), with

\[
dZ=-\kappa Z\,dt+\sqrt{2\kappa}\,dW_t.
\]

For a long entered at \(z_e\), lower adverse barrier \(l\), and upper TP barrier \(u\), with \(l<z_e<u\), let \(p(z)\) be the probability of reaching the TP first. Applying the OU generator gives

\[
p''(z)-zp'(z)=0,\quad p(l)=0,\ p(u)=1,
\]

\[
p(z_e)=\frac{\int_l^{z_e}e^{v^2/2}\,dv}{\int_l^u e^{v^2/2}\,dv}.
\]

This is a derivation for fixed continuous barriers and fixed known parameters; it is not an empirical win-rate estimator. The underlying first-passage framework and OU generator are well established.[^1][^3]

If TP is at the mean and entry is −1.5 SD, the following are ideal, infinite-horizon probabilities:

| Entry | Adverse boundary | TP boundary | Ideal OU TP-first probability |
|---:|---:|---:|---:|
| −1.5 | −2.00 | 0 | 51.01% |
| −1.5 | −2.20 | 0 | 64.68% |
| −1.5 | −2.75 | 0 | 88.07% |
| −1.5 | −3.00 | 0 | 93.45% |
| −2.0 | −2.20 | 0 | 27.91% |
| −2.0 | −2.75 | 0 | 75.65% |
| −2.0 | −3.00 | 0 | 86.64% |

A deeper entry is not automatically safer: if its adverse barrier is held fixed at −2.2 SD, entering at −2.0 SD leaves very little room. Reversal confirmation also changes the initial state distribution and fill selection; evaluate passage probabilities from the eventual fill state, not from the most favorable wick extreme.

### Ideal scenarios expose misspecification

For an illustrative arithmetic mapping of the scenario economics above, use \(z_e=-a\), \(l=-a-S/\sigma_{\rm eq}\), and \(u=-a+G/\sigma_{\rm eq}\). This uses small-return log approximations and targets five bps short of the mean in the selected cases.

| Scenario | Assumed SD / entry | Ideal OU TP-first probability | Mean exit time at 60m half-life |
|---|---:|---:|---:|
| Tight reference | 20 bps / −1.5 SD | 97.01% | 74.47 min |
| 1A | 15 bps / −2.0 SD | 1 minus approximately 2.54e−25 | 91.42 min |
| 1B | 40 bps / −1.875 SD | 99.82% | 105.48 min |
| 1C disaster envelope | 20 bps / −1.5 SD | 1 minus approximately 3.22e−22 | 78.54 min |

**Do not trade on these near-perfect probabilities.** A 135-bps adverse extension is nine stationary SD when SD is 15 bps. A stationary Gaussian OU assigns an extremely small chance to reaching that distant barrier before a nearby meanward target; actual shock, drift, parameter error, observation mismatch, timeouts, and passive nonfills invalidate that idealization. The contrast with your reported stop frequency is precisely why the model requires falsification rather than another threshold adjustment. Research on noisy OU estimation demonstrates how plausible-looking fitted parameters can be materially misspecified.[^6]

Expected exit time \(T(z)\) solves

\[
\kappa[T''(z)-zT'(z)]=-1,\quad T(l)=T(u)=0.
\]

The reported times are deterministic numerical boundary-value solutions, not simulated market paths. For a fixed stationary geometry, passage probabilities do not depend on half-life, while times scale as \(1/\kappa\). Consequently, widening the stop may improve ideal TP-first probability without improving real risk-adjusted return or portfolio capital productivity.[^1]

### Finite horizons and soft exits

For a maximum holding time \(H\), TP, stop, and timeout are separate outcomes. Define \(p(t,z)=P_z(\tau_u<\tau_l,\tau_u\le t)\). It solves

\[
\partial_t p=\kappa(p_{zz}-zp_z),\quad p(t,l)=0,\ p(t,u)=1,\ p(0,z)=0\ \text{inside the interval}.
\]

Timeout P&L depends on the surviving price distribution, not on the stop loss. Passive TP fills require execution and queue conditions after a price touch; a touch is not a guaranteed maker fill. Hyperliquid's order-priority mechanisms also mean a replay should not assume a universally simple arrival-time queue without considering the applicable order-priority rules.[^12][^13]

A two- or three-completed-bar boundary rule is a different stopping problem. Its Markov state includes price, remaining time, adverse-bar streak, innovation CUSUM, latest independent shock observation, and any moving baseline. Its value function must integrate the exact OU transition or a richer fitted state-space transition while updating those state variables; use a killed transition kernel or continuous-barrier flux for intrabar disaster stops, rather than endpoint observations alone. One-dimensional infinite-horizon passage probabilities cannot certify this policy.

If TP has net gain \(W\), ordinary soft invalidation has mean loss \(S_f\), catastrophe loss is \(L_c\), and probabilities are \(p,q,r\) with \(p+q+r=1\),

\[
E[R]=pW-qS_f-rL_c.
\]

For fixed catastrophe probability \(r\), positive mean soft losses, and no timeouts,

\[
p_{\rm BE}(r)=\frac{S_f+r(L_c-S_f)}{W+S_f}.
\]

This expresses what must be measured for 1C. Neither the average soft-exit loss nor the catastrophe frequency was supplied, so a numeric actual-policy expected win rate, average loss, or break-even rate would be fabricated. Some soft exits can themselves be profitable; in that case retain signed outcome P&L rather than classifying every invalidation as a loss.

### Sortino and Calmar

No option can be declared the joint maximizer of EV, Sortino, and Calmar from these summaries. These are different objectives; Sortino requires the downside return distribution and Calmar the chronological equity/drawdown path. With one portfolio slot, also evaluate net expected cash gain per occupied minute and opportunity cost across both engines. Repeatedly searching thresholds on the same short live sample is an overfitting risk, not a reliable calibration method.[^14]

## Causal BTC residual specification

### Why EWMA is insufficient

There is no universal causal filter or decay constant that removes drift without potentially creating apparent residual mean reversion. The failure can be shown exactly, without market simulation. Let a nonpredictable random-walk log-price satisfy

\[
p_t=p_{t-1}+r_t,\quad E[r_{t+1}\mid\mathcal F_t]=0,
\]

and define a prior-level EWMA residual

\[
y_t=p_t-g_{t-1},\quad g_t=\lambda g_{t-1}+(1-\lambda)p_t.
\]

Then

\[
y_t=\lambda y_{t-1}+r_t.
\]

The residual is stationary AR(1) for \(0<\lambda<1\), even though future raw-price returns have zero conditional mean. Its apparent reversion is the baseline catching up, not a forecast of a tradable price bounce. Causality eliminates future leakage, but does not eliminate this mathematical artifact. An ADF rejection on that manufactured residual is not sufficient evidence of price predictability.

For a diagnostic slow EWMA with 720-minute half-life,

\[
\lambda_\Delta=2^{-\Delta/720}.
\]

It is 0.999037759 on one-minute updates, 0.995198044 on five-minute updates, and 0.985663199 on fifteen-minute updates. These are exact engineering settings for a diagnostic, not optimized trading parameters. Holt-style adaptive trends can similarly absorb the move you expect to trade. The conventional two-sided HP filter is additionally unsuitable for causal live signals, and HP filtering has documented spurious-dynamics and endpoint concerns.[^15]

### Joint latent-level and OU model

The recommended research specification explicitly models the raw observable price rather than first filtering it and treating the resulting AR(1) fit as proof of predictability:

\[
p_t=g_t+x_t+\epsilon_t,
\]

\[
g_t=g_{t-1}+\nu_t,\qquad
x_t=\phi x_{t-1}+\omega_t,
\]

\[
\operatorname{Var}(\nu_t)=Q_g,\quad
\operatorname{Var}(\omega_t)=\sigma_{\rm eq}^2(1-\phi^2),\quad
\operatorname{Var}(\epsilon_t)=R.
\]

This is a proposed two-state extension of the standard causal local-level state-space model. Separate random-walk level dynamics from OU dynamics and measurement noise; do not fit an OU directly to correctly standardized one-step prediction innovations, which should be uncorrelated under a correctly specified model.[^16][^17]

With \(a_t=(g_t,x_t)'\), \(A=\operatorname{diag}(1,\phi)\), and \(H=(1,1)\), the exact recursion is

\[
a_t^-=Aa_{t-1}^+,\quad P_t^-=AP_{t-1}^+A'+Q,
\]

\[
e_t=p_t-Ha_t^-,\quad V_t=HP_t^-H'+R,\quad K_t=P_t^-H'/V_t,
\]

\[
a_t^+=a_t^-+K_te_t,
\]

\[
P_t^+=(I-K_tH)P_t^-(I-K_tH)'+K_tRK_t'.
\]

The last line is the Joseph covariance update implemented in Rust. Define your requested observable prior-baseline residual as

\[
y_t^{\rm obs}=p_t-\hat g_{t|t-1}.
\]

It is a causal observation of the latent OU plus level uncertainty and observation noise—not a noiseless OU. Use the fitted observation/state likelihood rather than applying plain AR(1) to it as if those extra components did not exist.

The executable-return forecast is the necessary economic check:

\[
E[p_{t+h}-p_t\mid\mathcal F_t]
=\hat g_{t|t}+\phi^h\hat x_{t|t}-p_t,
\]

where \(h\) counts model bars. Check that the predicted directional move, after execution and carry, is positive; separately validate that forecast against future raw mid-price returns. A fitted latent oscillation with no out-of-sample price-return predictability is not a trade.

There is intentionally no fixed EWMA `lambda` in this main filter: its level gain is part of the joint Kalman gain. Estimate \(Q_g,R,\phi,\sigma_{\rm eq}\) from the trailing training cut and compare out-of-sample likelihood/predictability against a random-walk-plus-observation-noise null. A latent model is not immune to weak identification; if the level and OU components cannot be separated robustly, reject the model.

### Estimation and eligibility

Use completed five-minute mid observations, a trailing 15-calendar-day window of 4,320 bars, and a scheduled hourly refit while flat. Fit candidate parameters without forcing a valid half-life, then apply the 30–180-minute eligibility gate; reject rather than clamp nonstationary, unidentified, or boundary-hitting estimates. Replay the training filter chronologically after a refit and version the state. Freeze all execution anchors at ARM even if a monitoring filter continues updating.

Fifteen days is an engineering starting point, not evidence of local stationarity over that entire interval. A simple single-OU approximation gives \(N_{\rm eff}\approx n(1-\phi)/(1+\phi)\), approximately 41.6 with 4,320 five-minute bars at a 180-minute half-life. That is only a rough dependence diagnostic, not the HMM's effective state count or a guarantee of parameter precision. Regime weighting or parameter instability can reduce useful information substantially.

A split-sample jackknife addresses specified finite-sample estimator bias, not manufactured residual stationarity, observation noise, structural breaks, or a wrong economic model. For the two-way form, choose the parameter being corrected explicitly; correcting \(\phi\) and then transforming is not identical to correcting \(\kappa\) directly. Invalid corrected estimates must veto the trade rather than be coerced into your accepted band.[^18][^19]

## Altcoin residuals and BTC exposure

### Single-leg P&L is not residual P&L

If a frozen-beta log-price decomposition is

\[
p_{k,t}=\alpha+\beta p_{B,t}+y_{k,t},
\]

then over the holding interval,

\[
\Delta p_k=\beta\Delta p_B+\Delta y_k.
\]

A single altcoin leg keeps the BTC-factor exposure. Its residual can revert because BTC moves while the altcoin barely changes; that is not a cash profit on the unhedged altcoin. The classic residual-stat-arbitrage literature constructs factor-adjusted/market-neutral positions; it does not make single-leg residual P&L automatically hedge-neutral.[^20]

**For the stated one-position system, use asset-specific raw-price forecasts and a BTC-factor impulse veto as the primary live research path.** Treat the BTC residual as a diagnostic/conditioning signal until it demonstrably improves prediction of the ALT'S own executable return. Do not promote a log-price regression residual merely because both series are highly correlated: correlation does not establish cointegration, and adapting alpha/beta can itself make the residual appear bounded.

### Exact return-factor recursion

On synchronized completed one-minute log returns, define \(u_t=r_{B,t}\), \(v_t=r_{k,t}\). Use an EW forgetting factor \(\lambda_B=2^{-1/720}\), a three-day/4,320-minute warmup, and a 15-day training history for initialization and diagnostics. These are explicit shadow settings, not empirically optimal windows.

Using ONLY moments through \(t-1\),

\[
\beta_t^- = \frac{C_{uv,t-1}}{V_{u,t-1}+\rho},\qquad
\alpha_t^- = m_{v,t-1}-\beta_t^-m_{u,t-1},
\]

\[
\varepsilon_{k,t}=v_t-\alpha_t^- -\beta_t^-u_t.
\]

With \(w=1-\lambda_B\), \(d_u=u_t-m_{u,t-1}\), and \(d_v=v_t-m_{v,t-1}\), update AFTER producing the residual:

\[
m_{u,t}=m_{u,t-1}+wd_u,\quad
m_{v,t}=m_{v,t-1}+wd_v,
\]

\[
V_{u,t}=\lambda_B[V_{u,t-1}+wd_u^2],\quad
C_{uv,t}=\lambda_B[C_{uv,t-1}+wd_ud_v].
\]

Set \(\rho=\max(10^{-12},10^{-3}V_{u,\rm train})\) in squared-log-return units and freeze it for the model version. Insufficient variance, synchronization failures, or unstable coefficients veto the signal rather than forcing beta to an arbitrary value.

The cumulative residual \(Y_{k,t}=Y_{k,t-1}+\varepsilon_{k,t}\) can be analyzed as a candidate spread, but is not inherently stationary. Do not sum residuals computed after continuously refitting the past, and do not fit OU to individual residual RETURNS when the economic object is a spread LEVEL. Factor-return decomposition and cumulative residual modeling are established approaches; the proposed one-leg restrictions are essential for this implementation.[^20]

If testing the user's log-price formula instead, use prior coefficients estimated on a fixed prior window, then evaluate with the newly observed synchronized BTC price. The resulting expression is causal at time t, but the coefficients are \(\mathcal F_{t-1}\)-measurable while the BTC input is \(\mathcal F_t\)-measurable. Label that correctly: it is not a forecast using information exclusively from t−1. Refit-induced residual jumps must not be mistaken for tradable alpha.

### Impulse veto definition

A concrete shadow implementation can use horizons \(h\in\{5,15,60\}\) minutes and prior-trained horizon-return variances. For position direction \(s\), define

\[
I_{k,s}(t)=\max_h
\frac{-s\beta_t^- r_{B,h,t}}
{\sqrt{(\beta_t^-)^2V_{B,h,t-1}+V_{\varepsilon_k,h,t-1}}}.
\]

The numerator uses the just-completed return; denominators and beta use only prior data. Train the horizon variances on non-overlapping horizon returns, and synchronize all inputs. A provisional adverse-score veto at 2.0 is a testing setting, not an established causal lead–lag edge. Keep the existing market-wide shock protections separate; this score does not prove BTC leads rather than moves contemporaneously.

Because Engine A is unhedged, require the asset's own forecast net edge to survive adverse BTC-factor scenarios, not merely that the residual forecast is favorable. The beta/factor model must be frozen or explicitly propagated when evaluating the scenario; it must never disappear from single-leg risk accounting.

## Exact reversal and maker policy

### Separate model and execution clocks

Use five-minute bars for model estimation/structural persistence and completed one-minute bars for reversal confirmation. Do not wait two five-minute or fifteen-minute candles merely because the model is slower. At an eligible completed five-minute ARM event, snapshot total executable-price center \(M^*\), fitted stationary scale \(\sigma^*\), entry multiple \(a^*\), side \(s\), and the frozen executable TP log price \(m_{\rm TP}^*\).

For a long, the arm boundary is \(A^*=M^*-a^*\sigma^*\); for a short it is \(A^*=M^*+a^*\sigma^*\). Do not shift this boundary with every new estimate. Arm only when the closed price is beyond the adverse boundary and all hard vetoes are clear.

### Exact completed-bar confirmation

Use the following fully specified shadow configuration: \(\gamma=0.50\), two completed one-minute bars without a new adverse intrabar extreme, ARM lifetime eight minutes, and quote eligibility for no more than two minutes after confirmation. Gamma is not identifiable as an optimum from six stopped trades; test 0.25/0.50/0.75 in a small preregistered grid if sufficient holdout data exists, without tuning to the three named wick peaks.

Let \(q_t=sp_t\) be direction-adjusted log price and \(E_t\) the minimum direction-adjusted intrabar extreme since ARM: a low for a long, negative high for a short. On every completed minute, if a new strictly smaller extreme appears, reset the no-new-extreme count to zero; otherwise increment it. Equal extremes do not reset the count.

The rebound requirement is

\[
b_t=\max\{3\,\mathrm{spread}_{bps,t},\;2\,\mathrm{tick}_{bps,t},\;0.50\,\hat\sigma_{r,1m,t-1}10^4\}.
\]

This is a bps quantity, not literally dimensionless; its ratio to a volatility scale would be dimensionless. Use an SD of one-minute LOG RETURNS estimated through t−1, not stationary OU level SD. Compute spread and tick in bps from the relevant fresh book.

Confirm only when all conditions hold simultaneously:

- At least two completed one-minute bars since the last new adverse extreme.
- \(sp_t-E_t\ge b_t/10^4\).
- \(sp_t\ge sA^*\): price has returned to the frozen boundary or better. This is a state condition, not a requirement that the crossing happen on exactly the same bar as the final confirmation.
- \(s(p_t-p_{t-3})>0\), using four completed closes.
- No shock, crowding, model-validity, stale-data, or portfolio-reservation veto.

Cancel the setup on expiry, missing/out-of-order minute data, or a veto. A new wick restarts the extreme-persistence counter. If a quote already rests, cancel/revalidate it under the same vetoes; a confirmation is not permission to ignore subsequent adverse information.

### Do not spend the edge

Let \(D_0=s(m_{\rm TP}^*-A^*)>0\) be the frozen arm-to-TP log distance and \(D_{\rm rem}(p)=s(m_{\rm TP}^*-\ln p)\). Impose both a remaining-edge hurdle and a consumed-edge cap:

\[
D_{\rm rem}\ge D_{\rm req},\quad
D_{\rm rem}\ge0.75D_0.
\]

The proposed minimum is 55 bps gross executable price distance and at least 40 bps net after exact maker/maker fees and a 4-bps carry/adverse-selection reserve. The reserve is a provisional test allowance, not a calibrated estimate. A 20–35-bps setup cannot satisfy the 55-bps hurdle and is intentionally skipped; shortening confirmation to force an entry is not a solution.

For exact fee affordability, let \(w=(40+4)/10^4\), \(f=0.00015\), and price ratio \(r=P_{\rm TP}/P_e\). A long requires

\[
\ln r\ge\ln\frac{1+f+w}{1-f},
\]

and a short requires

\[
-\ln r\ge\ln\frac{1+f}{1-f-w}.
\]

Combine that side-specific requirement with the 55-bps gross-price threshold converted to log distance and the 75%-remaining-distance cap. In particular, if \(c_{\rm req}\) denotes this TOTAL required log distance, the cap is \(\exp(m_{\rm TP}^*-c_{\rm req})\) for a long, not `exp(model_center - fee_only_cost)` when TP is offset from that center.

### Join versus retest

For a long, post at current best bid only if it is no higher than

\[
P_{\max}=\exp(m_{\rm TP}^*-D_{\rm req}).
\]

For a short, join best ask only if it is no lower than \(\exp(m_{\rm TP}^*+D_{\rm req})\). Re-evaluate the fresh book and affordability before every order action. If joining would consume too much edge, do not chase: long retest price is \(\min(P_{bid},e^{A^*})\); short retest price is \(\max(P_{ask},e^{A^*})\), subject to the same affordability tests and timeout. If even a retest is unaffordable, skip it.

Round buys down and sells up to legal venue prices, preserve post-only semantics, and do not reprice across your cap. ALO cancels instead of taking immediately; GTC has no such protection. Therefore a reduce-only GTC TP is not guaranteed maker on submission or replacement; strictly passive replacements should use ALO or explicitly budget for taker execution.[^21][^22]

A resting retest is not free optionality: fills are selected by subsequent price movement. Replay nonfills, queue position, partial fills, adverse selection, and canceled orders. Hyperliquid has specific order-priority fees, including ALO priority fees charged on placement regardless of fill, so any such fees must be included rather than hidden in a maker-only performance assumption.[^12]

### Repair the TP offset clamp

The supplied offset formula can request an upper bound below its lower bound. For example, \(0.15D_{\rm rem}<2\) bps when remaining distance is under 13.33 bps. A standard clamp with minimum greater than maximum is invalid; two ticks can also exceed your intended maximum offset.

Implement in bps: \(L=\max(2\,tick_{bps},2)\), \(U=\min(10,0.15D_{\rm rem,bps})\). If \(L>U\), skip the offset/setup under that rule; otherwise use \(\epsilon=\min(\max(5,L),U)\). Deduct this offset before applying the gross/net-edge hurdle, so it cannot be counted twice as both executable distance and captured profit.

## Two-tier position invalidation

### Tier one: immediate protection

Exchange-native disaster protection is always active from the first filled quantity, including partial fills. Size against the disaster distance, not the hoped-for earlier soft exit. A 100-bps entry-relative disaster stop is the proposed common shadow specification; 150–200 bps remains a separate wider-stop experiment with correspondingly smaller quantity.

Stop-price fills and the account's planned loss are not guaranteed: Hyperliquid triggers TP/SL from mark, and the documented market TP/SL tolerance is 10%. A stop-limit can instead remain unfilled after a gap. Therefore a 0.326-USDC calculated risk budget is a planned stop-loss amount under assumed shortfall, not a hard maximum loss.[^5][^4]

On every trusted mark/heartbeat, independently evaluate disaster breach, position protection acknowledgement, margin/liquidation buffer, fatal execution/control failures, and holding timeout. Severe confirmed risk events are not required to wait two or three closes. Do not use one-bar HMM/drift flicker as an ordinary market-flatten instruction, but do not disable genuine crash or operational protection to make the backtest smoother.

### Tier two: persistent evidence

Freeze \(M^*,\sigma^*\) at ARM and use

\[
Z_t^*=\frac{p_t-M^*}{\sigma^*},\qquad
Z_{\rm adverse,t}=-sZ_t^*.
\]

Soft structural invalidation occurs at \(Z_{\rm adverse}\ge2.75\) for two consecutive completed FIVE-MINUTE bars. Reset the streak if a bar returns below the threshold. Do not use \(|Z|\): a favorable +3-SD move should not be interpreted as an adverse break for a long. A slowly moving baseline must not erase the distance accumulated during an active trade.

In parallel, compute adverse CUSUM on standardized one-step model INNOVATIONS, not on persistent level z-scores:

\[
C_t=\max\{0,C_{t-1}-s\xi_t-0.5\},\quad \xi_t=e_t/\sqrt{V_t}.
\]

The provisional trigger is \(C_t\ge8\) on the five-minute model clock. The reference 0.5 corresponds to conventional one-SD shift detection, but the threshold 8 here is an explicit shadow hypothesis, not a calibrated Hyperliquid false-alarm rate. NIST's standard CUSUM guidance explains the reference/decision-limit relationship; actual false-alarm and detection-delay distributions must be checked on your innovations.[^23][^24]

Persistent shock invalidation uses two distinct, adjacent, NON-OVERLAPPING completed fifteen-minute return observations satisfying \(-sZ_{15}\ge3.0\). Re-reading the same fifteen-minute snapshot on three one-minute updates does not create three observations. Two overlapping rolling 15-minute windows are also not independent confirmation; choose deliberately whether you want two one-minute updates of the same event or two genuine 15-minute events. This implementation chooses the latter and retains immediate disaster protection for the delay.

Use the earliest of structural invalidation, CUSUM, persistent adverse shock, and a time stop \(H=\min(3t_{1/2},360\text{ min})\). Once an exit intent is latched, cancel remaining entry quantity, preserve protective coverage, and reconcile reduce-only exit attempts until actually flat. Do not treat a locally issued cancellation or market-close request as a completed exchange transition.

## Explicit parameter specification

**No asset-specific optimum is identifiable from the provided aggregate summary.** The honest definitive table is an implementable, common SHADOW specification, not invented per-symbol calibration. Keep all four symbols on the same initial policy to avoid pretending the data establish different optimal thresholds.

| Asset | Model / execution bars | OU training window | Eligible half-life | Base entry | Minimum executable edge |
|---|---|---|---|---:|---|
| BTC | 5m / 1m | 4,320 bars, 15 calendar days | 30–180m | 2.0 stationary SD | 55 bps gross; ≥40 bps net after maker fees + 4-bps reserve |
| ETH | 5m / 1m | Same | 30–180m | 2.0 stationary SD | Same, plus BTC-factor veto |
| SOL | 5m / 1m | Same | 30–180m | 2.0 stationary SD | Same, plus BTC-factor veto |
| AVAX | 5m / 1m | Same | 30–180m | 2.0 stationary SD | Same, plus BTC-factor veto; shadow until validated |

| Asset | Native disaster stop | Structural invalidation | Other soft exits | Initial risk |
|---|---:|---|---|---:|
| BTC | 100 bps from entry VWAP | Adverse frozen z ≥2.75, two completed 5m bars | CUSUM .5/8; two distinct adverse 15m shocks ≥3; time stop | 0.20%–0.25% equity, planned |
| ETH | Same | Same | Same | Same |
| SOL | Same | Same | Same | Same |
| AVAX | Same | Same | Same | Same, shadow only |

The 100-bps hard stop can bind before completion of the soft-boundary persistence condition when adverse movement continues between completed bars, particularly when stationary SD is large. For entry at −2 SD, structural −2.75 SD is only another 0.75 SD away, but waiting two completed bars adds random adverse distance; do not interpret it as a deterministic 0.75-SD loss cap. Reject candidates whose ordinary realized excursion distribution makes the 100-bps disaster stop routine rather than exceptional. Do not automatically enlarge it without rerunning size, execution and tail-risk analysis.

For the concrete 100-bps stop, 3-bps execution allowance, 4-bps carry reserve, base fees, 0.225% equity risk, and no binding notional cap, exact short-side sizing gives approximately 28.86 USDC. Round quantity DOWN to venue size precision; if the resulting entry is below minimum notional, skip it rather than rounding risk upward. Hyperliquid's minimum-order error specifies 10 USDC, and its price/quantity precision rules require metadata-based formatting.[^25][^26]

Entry confirmation settings are gamma 0.50, two quiet one-minute bars, eight-minute ARM lifetime, two-minute quote lifetime, fresh book no older than one second, and at most 25% of initial arm-to-TP log distance consumed. These are uncalibrated engineering test values. The joint filter's Q/R/OU scales are estimated inputs, not arbitrary constants hidden in this table.

## Rust implementation boundaries

The accompanying crate exposes `JointOuFilter`, `EwmaDiagnostic`, `BtcReturnFactor`, `Reversal`, `maker_quote`, `PositionRisk`, and `risk_notional`. `JointOuFilter` uses the causal prediction/update equations above with a Joseph covariance update; `EwmaDiagnostic` is explicitly labeled non-evidentiary; factor coefficients are used before current-return updates. Unit tests cover long/short symmetry, frozen-arm confirmation, timestamp gaps, duplicate observations, covariance positivity, stale books, retest pricing, adverse-only structural invalidation, repeated shock snapshots, first-fill timeouts, and exact fee sizing.

Run:

```bash
cargo test
cargo run --example policy
```

The crate passed 24 tests with rustc 1.99.1. That validates the included deterministic calculations and transitions only; it is not a market-data backtest, live integration test, statistical certification, or proof of profitability.

Integration order is important:

1. Validate exchange-time completed bars and synchronize mid/mark/BTC inputs.
2. Update the five-minute filter with a parameter version trained before the observation; retain the innovation on that clock.
3. Feed EVERY completed one-minute bar into `Reversal`, including idle bars; only then ARM on the eligible five-minute close using the same completed bar and frozen executable-price anchor.
4. Revalidate the confirmation, fresh book, all vetoes, edge affordability, and atomic portfolio lease before placing ALO.
5. Protect each partial fill immediately. Start holding time from FIRST FILL, not submission. Cancel residual entry quantity or follow an explicitly bounded inventory policy.
6. Maintain a single order/position writer across Engines A and B. Persist intent/state, use idempotent client IDs, and reconcile after restart or uncertain acknowledgements.
7. Keep native protection while executing/reconciling soft exits; do not remove the stop before confirmed flat inventory.

The provided float quote is not a wire-ready order price. Hyperliquid permits at most five significant figures and a perpetual decimal limit of `6 - szDecimals`, with special handling for integer prices; quantities follow asset `szDecimals`. Implement final legal rounding with decimal/integer arithmetic and current metadata.[^26]

No repository snapshot was supplied, so the crate is not claimed to be a drop-in patch for your current modules. It deliberately omits networking/signing, persistence serialization, distributed leases, fill-VWAP aggregation, exchange-native stop management, and historical parameter optimization. Those boundaries are why the deliverable is a production-oriented core rather than a falsely certified production system.

## Validation and promotion gates

Before choosing among 1A/1B/1C, reconstruct complete inventory episodes and timestamp-align fills, mids, marks, native trigger prices, model versions, frozen ARM values, fees, funding, submitted/canceled/rejected orders, and residual position quantity. Measure executable-price MAE/MFE and the probability of TP BEFORE each alternative exit; conditioning on eventual winners alone will bias the comparison.

Run chronological nested walk-forward evaluation with a genuinely untouched final segment. Fit filters, scales, factor coefficients, regime models and thresholds on prior cuts only; purge overlapping trades/horizons from selection splits and account for the number of configurations tried. Backtest-overfitting research supports treating the search procedure itself as part of the experiment, not just reporting its best result.[^27][^14]

Use actual recorded L2/order events and conservative queue assumptions. Include full nonfills, ALO rejections, partial inventory, emergency execution shortfall, mark/mid basis, hourly funding and optional priority/builder costs. Do not use favorable wick-followed-by-reversal anecdotes as evidence that an order at the wick would have been filled safely.[^28][^22][^4][^12]

Promotion criteria should require a positive lower confidence bound on net strategy EV, acceptable stress and realized drawdown, robust performance across contiguous dates and parameter neighborhoods, and a demonstration that the raw-price prediction beats the random-walk/null after executable costs. Report expected gain per filled trade AND per portfolio slot-hour, drawdown path, downside return distribution, entry fill probability, loss-tail concentration, and opportunity cost against Engine B/cash.

**The unresolved quantities remain unresolved:** actual out-of-sample win rates by coin, true average soft-exit loss, catastrophe frequency, optimized gamma/decays, and Sortino/Calmar rankings. The supplied ledger summary is enough to identify payoff and specification contradictions, but not enough to estimate those missing distributions honestly.

---

## References

1. [ideas.repec.org › a › wsiOptimal Mean Reversion Trading With Transaction Costs And ...](https://ideas.repec.org/a/wsi/ijtafx/v18y2015i03ns021902491550020x.html) - Modeling the price spread by an Ornstein–Uhlenbeck process, we apply a probabilistic methodology and...

2. [Trading Under the Ornstein-Uhlenbeck Model](https://ideas.repec.org/h/wsi/wschap/9789814725927_0002.html) - We show that the entry region is characterized by a bounded price interval that lies strictly above ...

3. [Estimation of Ornstein–Uhlenbeck process using ultra-high ...](https://link.springer.com/article/10.1007/s10479-025-06855-7?error=cookies_not_supported&code=7580a0e8-e958-4c40-9765-55e7354594db) - We also show that the Ornstein–Uhlenbeck process contaminated by the independent Gaussian white nois...

4. [my-tp-sl-did-not-execute-correctly](https://hyperliquid.gitbook.io/Hyperliquid-docs/support/faq/trade-outcome-looks-incorrect/my-tp-sl-did-not-execute-correctly) - * By default, market TP/SL orders have a 10% slippage tolerance, which means the execution price can...

5. [Robust price indices - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/robust-price-indices) - Mark price is an unbiased and robust estimate of the fair perp price, and is used for margining, liq...

6. [Estimation of Ornstein-Uhlenbeck Process Using Ultra-High-Frequency Data with Application to Intraday Pairs Trading Strategy](https://arxiv.org/abs/1811.09312v4) - # Title: Estimation of Ornstein-Uhlenbeck Process Using Ultra-High-Frequency Data with Application t...

7. [Ornstein-Uhlenbeck Process](https://oeis.org/A249417/a249417.pdf) - process if it is stationary, Gaussian, Markovian, and continuous in probability [1, 2].

8. [userfees](https://nktkas.gitbook.io/hyperliquid/api-reference/info-methods/userfees) - 0-9]+)?$"}},"required":["bpsOfMaxSupply","discount"],"additionalProperties":false},"description":"Ar...

9. [hyperliquid-trading-fees-and-proliquid-builder-fees](https://docs.proliquid.xyz/getting-started/hyperliquid-trading-fees-and-proliquid-builder-fees) - | Perps  |   `0.045%` |   `0.015%` |

10. [Hyperliquid Integration | NautilusTrader](https://nautilustrader.io/hyperliquid/) - Linear perpetuals settled in USDC on a fully on-chain order book, with native trigger orders for sto...

11. [Contract specifications - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/contract-specifications) - Instead, they rely on funding payments to ensure convergence to the underlying spot price over time.

12. [Priority fees | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/priority-fees) - The priority rate has a linear effect on end-to-end latency from 0-8bps, i.e. `p = 80000`.

13. [llms-full.txt](https://hyperliquid.gitbook.io/hyperliquid-docs/llms-full.txt) - Orders are matched in price-time priority.
...
order entry and connectivity, and instead focus on th...

14. [The Probability of Backtest Overfitting - ScholarWorks at WMU](https://scholarworks.wmich.edu/math_pubs/42/) - We propose a general framework to assess the probability of backtest overfitting (PBO).

15. [Why You Should Never Use the Hodrick-Prescott Filter - IDEAS/RePEc](https://ideas.repec.org/a/tpr/restat/v100y2018i5p831-843.html) - **File URL:** http://www.mitpressjournals.org/doi/pdf/10.1162/rest_a_00706

16. [concentrating the scale out of the likelihood function](https://www.statsmodels.org/devel/examples/notebooks/generated/statespace_concentrated_scale.html) - As a specific example, consider the local level model, which can be written as:

17. [Time Series Analysis by State Space Methods statespace](https://www.statsmodels.org/v0.11.1/statespace.html) - An autoregressive model is a good introductory example to putting models in
state space form.

18. [Jackknifing Bond Option Prices](https://ink.library.smu.edu.sg/soe_research/385/) - This article proposes a very general and computationally inexpensive method of bias reduction that i...

19. [[PDF] A Selective Overview of Nonparametric Methods in Financial ...](http://korora.econ.yale.edu/phillips/pubs/art/p1152.pdf) - To reduce the finite sample estimation bias in para- meter estimation as well as the consequential b...

20. [jeremywhittaker.com › wp-content › uploadsStatistical Arbitrage in the U.S. Equities Market](https://jeremywhittaker.com/wp-content/uploads/2021/03/AvellanedaLeeStatArb071108.pdf) - We study model-driven statistical arbitrage strategies in U.S. equities.

21. [modify | Hyperliquid - GitBook](https://nktkas.gitbook.io/hyperliquid/api-reference/exchange-methods/modify) - "],"type":"string","description":"Time-in-force.\n- `\"Gtc\"`: Remains active until filled or cancel...

22. [Place order | Hyperliquid exchange - Chainstack Docs](https://docs.chainstack.com/reference/hyperliquid-exchange-place-order) - * `Alo` — Add liquidity only (post-only), canceled if would immediately match

23. [Cusum Control Chart - Information Technology Laboratory](https://www.itl.nist.gov/div898/software/dataplot/refman1/auxillar/cusum.htm) - The value of *k* is set to one half of the smallest shift in location (in standard deviation units) ...

24. [Cusum Average Run Length](https://www.itl.nist.gov/div898/handbook/pmc/section3/pmc3231.htm) - If we are dealing with normally distributed measurements, we can standardize this shift by

25. [Hyperliquid Constraints | Quote Docs - GitBook](https://quote.gitbook.io/quote-docs/trading-on-hyperliquid/hyperliquid-constraints) - For the rate and how it sits against Hyperliquid's own fees, see [Fees](/quote-docs/your-account/fee...

26. [Tick and lot size | Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/tick-and-lot-size) - Prices can have up to 5 significant figures, but no more than `MAX_DECIMALS - szDecimals` decimal pl...

27. [academic.oup.com · jrssig · articleHow “Backtest Overfitting” in Finance Leads to False ...](https://academic.oup.com/jrssig/article/18/6/22/7038278) - A leading reason for the failure of investment models is *backtest overfitting* (see the Glossary, p...

28. [Funding - Hyperliquid Docs - GitBook](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding) - Funding is purely peer-to-peer and no fees are collected on the payments.

