# Intra-Candle Execution vs Close-Only Analysis

This report analyzes the microstructural and mathematical efficiency of a 5-minute execution loop operating on 15-minute timeframe OHLCV data, specifically in the context of a quantitative dual-bot A/B testing environment on Binance Spot.

## 1. Degradation of Statistical Validity in Forecasts

**Does intra-candle execution degrade the statistical validity of the HMM and GARCH forecasts?**

Yes, fundamentally. Both Hidden Markov Models (HMM) and Generalized Autoregressive Conditional Heteroskedasticity (GARCH) models are highly sensitive to the statistical properties of their input data. When trained on historical *closed* 15m data, the models map the variance, transition probabilities, and distribution of finalized periods. 

Feeding unclosed (intra-candle) 15m data—which is inherently incomplete and subject to intra-period noise—into these models creates a structural mismatch:
* **Volatility Mismatch (GARCH):** GARCH(1,1) relies on the finalized squared returns of the prior period to forecast current variance. An unclosed candle does not represent the true period return, leading to artificially low or erratic variance estimates and miscalibrated Kelly scaling position sizing.
* **Regime Misclassification (HMM):** HMM regime detection relies on completed state transitions. Intra-candle price spikes might temporarily mimic a high-volatility regime, triggering a state shift that reverts by the time the candle closes. This introduces high levels of noise and false positives.

## 2. Mathematical Superiority of a Split Architecture

**Is it mathematically superior to split the architecture (1m Risk Management / 15m Trade Entry)?**

Absolutely. This decoupled approach aligns perfectly with institutional quantitative trading architectures. 
* **Trade Entry (15m Closes):** Signal generation (especially ML models like HMM/GARCH and moving average crossovers) relies on the stationarity and consistency of closed data. Executing only at the precise 15m boundaries (:00, :15, :30, :45) ensures zero look-ahead bias and eliminates intra-candle false signals (e.g., a MACD crossover that appears at minute 7 but disappears by minute 15).
* **Risk Management (1m or Tick):** Risk management is fundamentally a continuous-time problem. A trailing stop-loss (like moving to breakeven at 35% profit progress) must react to real-time price action to protect capital. Running risk management every 1 minute minimizes execution latency and drawdown exposure, whereas a 15m or 5m risk loop could miss intra-candle crash events, resulting in slippage well past the intended stop-loss price.

## 3. Optimal Execution Architecture

**What is the optimal execution architecture for minimizing false breakouts while maximizing risk management speed?**

To achieve the best of both worlds, the architecture should be bifurcated into two independent, asynchronous loops (or microservices):

1. **The Alpha Engine (Signal & Entry Loop) - 15m Cron**
   * **Schedule:** Executes strictly at the 15m marks, plus a small buffer (e.g., 2-3 seconds) to ensure the exchange API returns the fully closed candle.
   * **Function:** Fetches closed 15m OHLCV, runs technical confluences (Champion) or HMM/GARCH forecasts (Challenger), determines Kelly position sizing, and fires market/limit entry orders.
   * **Advantage:** Eliminates false intra-candle breakouts and ensures mathematical consistency with historical backtesting.

2. **The Risk Engine (Position Management Loop) - 1m Cron or WebSocket**
   * **Schedule:** Runs every 1 minute via cron, or ideally, listens to a live WebSocket stream of the ticker's price (or 1m/1s candles).
   * **Function:** Only monitors open positions. Evaluates current PnL, triggers trailing stops, executes Stop-Loss/Take-Profit orders, and manages the "breakeven at 35% progress" logic.
   * **Advantage:** Reacts to adverse price action instantly, drastically reducing risk exposure without interfering with the pristine 15m entry signals.

### Conclusion
The current 5m execution loop on 15m data represents an "uncanny valley"—it is too slow for optimal risk management and too frequent for reliable 15m signal generation. Splitting the architecture into a 15m Signal Engine and a 1m Risk Engine is mathematically superior and will significantly improve the Sharpe ratio of both the Champion and Challenger bots.
