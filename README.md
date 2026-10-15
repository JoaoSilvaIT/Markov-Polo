# Markov Polo — Quantitative Perpetuals Research & Execution Stack

This repository contains the production Rust execution engine, real-time telemetry terminal, and quantitative research papers for an autonomous multi-asset trading system deployed on the **Hyperliquid L1 DEX** (`BTC`, `ETH`, `SOL`, `AVAX`).

---

## Repository Structure

- **[`hyperliquid-bot/`](./hyperliquid-bot/README.md)** — Production asynchronous Rust (`tokio`) Dual-Engine trading system:
  - **Engine A (Mean Reversion):** Continuous-time Ornstein-Uhlenbeck (O-U) process in log-price space with Phillips-Yu (2005) Jackknife bias reduction, hourly funding carry threshold perturbation, two-stage reversal-bounce confirmation, and 100% Post-Only (`Alo`) maker execution.
  - **Engine B (Trend Momentum):** 15-minute Donchian channel breakout with `EMA 16/64` alignment, multiscale distance-weighted L2 Order Book Imbalance (`OBI`) confirmation, exogenous Funding/OI crowding hazard scaling, and a monotone 4-tier profit-locking Chandelier ATR trailing stop.
  - **Regime Controller:** 4-feature Gaussian Hidden Markov Model (`GaussianHMM` with Ledoit-Wolf covariance shrinkage) driving a 3-mode state machine (`MeanReversion`, `Neutral`, `Trend`) with asymmetric multi-tier dwell and shock cooldowns.
  - **Distributed Systems Safety:** POSIX `flock(2)` cross-container lease fencing, monotonic epoch tokens, `fsync`-hardened Write-Ahead Order Intent Logs (WAL), event-driven WebSocket fill/stop supervision, and a counterfactual shadow-trade attribution engine.
- **[`research/`](./research/)** — Quantitative research briefs, mathematical derivations, post-mortem reviews, and institutional audit reports backing every production iteration.
- **`rust-bot/`** — Legacy single-engine Spot implementation (archived reference).

---

## Quick Start

```bash
cd hyperliquid-bot

# 1. Run the 89-test verification suite
cargo test

# 2. Launch the 4-asset cluster + web terminal (port 8088)
docker compose up -d --build
```

For full mathematical and operational details, see **[`hyperliquid-bot/README.md`](./hyperliquid-bot/README.md)** and **[`hyperliquid-bot/INSTITUTIONAL_CODE_AND_QUANT_AUDIT_REPORT.md`](./hyperliquid-bot/INSTITUTIONAL_CODE_AND_QUANT_AUDIT_REPORT.md)**.
