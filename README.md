# Markov Polo — Quantitative Perpetuals Research & Execution Stack

[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![Hyperliquid L1](https://img.shields.io/badge/Hyperliquid-L1%20Perpetuals-brightgreen.svg)](https://hyperliquid.xyz)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Referral Discount](https://img.shields.io/badge/Hyperliquid%20Referral-Code%20%22JONBOT%22%20(4%25%20Discount)-green)](https://app.hyperliquid.xyz/join/JONBOT)

> 🎁 **Trading on Hyperliquid?** Use referral code **[`JONBOT`](https://app.hyperliquid.xyz/join/JONBOT)** or register through [**app.hyperliquid.xyz/join/JONBOT**](https://app.hyperliquid.xyz/join/JONBOT) to receive a **lifetime 4% discount** on all maker and taker fees across all perpetual and spot markets.

This repository contains the production Rust execution engine, real-time telemetry terminal, discrete-event simulated exchange backtester, and quantitative research library for an autonomous multi-asset trading system deployed on the **Hyperliquid L1 DEX** (`BTC`, `ETH`, `SOL`, `AVAX`).

---

## Repository Structure

- **[`hyperliquid-bot/`](./hyperliquid-bot/README.md)** — Production asynchronous Rust (`tokio`) Dual-Engine trading system:
  - **Engine A (Mean Reversion):** Continuous-time Ornstein-Uhlenbeck (O-U) process in log-price space with Phillips-Yu (2005) Jackknife bias reduction, hourly funding carry threshold perturbation, two-stage reversal-bounce confirmation, and 100% Post-Only (`Alo`) maker execution.
  - **Engine B (Trend Momentum):** 15-minute Donchian channel breakout with `EMA 16/64` alignment, multiscale distance-weighted L2 Order Book Imbalance (`OBI`) confirmation, exogenous Funding/OI crowding hazard scaling, and a monotone 4-tier profit-locking Chandelier ATR trailing stop.
  - **Regime Controller:** 4-feature Gaussian Hidden Markov Model (`GaussianHMM` with Ledoit-Wolf covariance shrinkage) driving a 3-mode state machine (`MeanReversion`, `Neutral`, `Trend`) with asymmetric multi-tier dwell and shock cooldowns.
  - **Backtester (`src/backtest.rs`):** NautilusTrader-inspired discrete-event simulated exchange matching engine replaying historical 1-minute order book snapshots and continuous L2 OBI feeds with microsecond-level FIFO queue modeling.
  - **Distributed Systems Safety:** POSIX `flock(2)` cross-container lease fencing, monotonic epoch tokens, `fsync`-hardened Write-Ahead Order Intent Logs (WAL), event-driven WebSocket fill/stop supervision, and a counterfactual shadow-trade attribution engine.
- **[`research/`](./research/)** — Quantitative research briefs, mathematical derivations, post-mortem reviews, and institutional audit reports backing every production iteration.
- **`rust-bot/`** — Legacy single-engine Spot implementation (archived reference).

---

## Quick Start

```bash
cd hyperliquid-bot

# 1. Run the verification test suite (100+ unit & property tests passing)
cargo test

# 2. Run the discrete-event backtester across stored 1m market candles
cargo run --release -- --backtest

# 3. Launch the 4-asset cluster + web terminal (port 8088)
docker compose up -d --build
```

For full mathematical and operational details, see **[`hyperliquid-bot/README.md`](./hyperliquid-bot/README.md)** and **[`hyperliquid-bot/INSTITUTIONAL_CODE_AND_QUANT_AUDIT_REPORT.md`](./hyperliquid-bot/INSTITUTIONAL_CODE_AND_QUANT_AUDIT_REPORT.md)**.

---

## 🎁 Hyperliquid Fee Discount

To run this system with minimal fee friction, link your wallet using referral code **[`JONBOT`](https://app.hyperliquid.xyz/join/JONBOT)** on [Hyperliquid](https://app.hyperliquid.xyz/join/JONBOT). 

This applies a **permanent 4% discount** across all maker, taker, and liquidation fees.

---

## ⚠️ Financial & Trading Risk Disclaimer

This software is provided strictly for educational, research, and technical experimentation purposes. 
Cryptocurrency perpetual futures and decentralized derivatives trading involve substantial risk of financial loss. 
Past simulated or real performance is not indicative of future results. 
The authors and contributors bear absolutely zero responsibility or liability for any financial losses, 
liquidations, lost profits, system downtime, smart contract bugs, or exchange-side execution anomalies 
incurred through the use or deployment of this software. Trade at your own risk.

---

## License

This project is licensed under the terms of the [MIT License](LICENSE). Copyright (c) 2026 Joao Silva.
