use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use uuid::Uuid;

pub const COUNTERFACTUAL_JOURNAL: &str = "hl_counterfactual_trades.jsonl";

/// Minimum tick size for strict trade-through verification
pub fn get_tick_size(symbol: &str) -> f64 {
    match symbol.to_uppercase().as_str() {
        "BTC" => 1.0,
        "ETH" => 0.10,
        "SOL" => 0.01,
        "AVAX" => 0.001,
        _ => 0.01,
    }
}

/// Official Hyperliquid Base Fee Structure (Perpetuals)
pub const HYPERLIQUID_BASE_MAKER_FEE_BPS: f64 = 1.0; // 0.010% (1.0 bps)
pub const HYPERLIQUID_BASE_TAKER_FEE_BPS: f64 = 3.5; // 0.035% (3.5 bps)
pub const ESTIMATED_SLIPPAGE_BPS: f64 = 2.0;

/// Veto Classification per Researcher 1 & 2 Consensus
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum VetoClassification {
    /// MAE breached theoretical Stop-Loss before MFE hit Take-Profit (Veto protected capital)
    AlphaProtective,
    /// MFE hit theoretical Take-Profit before MAE breached Stop-Loss (Veto cost missed alpha)
    AlphaStrangling,
    /// Neither TP nor SL hit during the evaluation horizon (Trade scratched / timed out)
    NeutralNoise,
    /// Pending resolution (position still open in shadow simulation)
    Pending,
}

impl std::fmt::Display for VetoClassification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VetoClassification::AlphaProtective => write!(f, "ALPHA_PROTECTIVE"),
            VetoClassification::AlphaStrangling => write!(f, "ALPHA_STRANGLING"),
            VetoClassification::NeutralNoise => write!(f, "NEUTRAL_NOISE"),
            VetoClassification::Pending => write!(f, "PENDING"),
        }
    }
}

/// Status of the counterfactual fill model
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum FillStatus {
    Unfilled,
    TouchOnly,
    TradeThroughConfirmed,
}

/// Block 1: Decision Details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterfactualDecision {
    pub accepted: bool,
    pub primary_veto: String,
    pub all_vetos: Vec<String>,
    pub threshold_sigma: f64,
    pub signal_value: f64,
    pub hmm_probabilities: HmmProbs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HmmProbs {
    pub consolidation: f64,
    pub trend: f64,
    pub shock: f64,
}

/// Block 2: Market State at Decision
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketAtDecision {
    pub reference_price: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub l2_age_ms: u64,
    pub obi: f64,
    pub volatility_z15: f64,
    pub volatility_z60: Option<f64>,
}

/// Block 3: Hypothetical Order
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HypotheticalOrder {
    pub order_type: String,
    pub limit_price: f64,
    pub size: f64,
    pub time_in_force: String,
    pub assumed_cancel_rule: String,
    pub queue_ahead_estimate: Option<f64>,
}

/// Block 4: Fill Model (Bounds & Strict-Through)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillModel {
    pub version: String,
    pub class: String,
    pub touch_only: bool,
    pub trade_through_filled: bool,
    pub fill_price: Option<f64>,
    pub fill_timestamp_utc: Option<String>,
    pub assumptions: Vec<String>,
}

/// Block 5: Outcome & Excursion Tracking (MFE / MAE measured from hypothetical fill)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterfactualOutcome {
    pub horizon_seconds: u64,
    pub exit_rule: String,
    pub fill_status: FillStatus,
    pub exit_trigger: Option<String>,
    pub exit_price: Option<f64>,
    pub mfe_bps: Option<f64>,
    pub mae_bps: Option<f64>,
    pub gross_pnl_bps: Option<f64>,
    pub fees_bps: Option<f64>,
    pub funding_bps: Option<f64>,
    pub spread_slippage_bps: Option<f64>,
    pub counterfactual_net_pnl_bps: Option<f64>,
    pub veto_classification: VetoClassification,
    pub avoided_loss: bool,
    pub missed_profit: bool,
}

/// Block 6: Data Quality
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataQuality {
    pub book_source: String,
    pub trade_stream_available: bool,
    pub missing_intervals: u64,
}

/// The Complete R2-Compliant Counterfactual Record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterfactualRecord {
    pub schema_version: u32,
    pub event_id: String,
    pub opportunity_id: String,
    pub ts_utc: String,
    pub asset: String,
    pub venue: String,
    pub engine: String,
    pub side: String,
    pub mode: String,

    pub decision: CounterfactualDecision,
    pub market_at_decision: MarketAtDecision,
    pub hypothetical_order: HypotheticalOrder,
    pub fill_model: FillModel,
    pub outcome: CounterfactualOutcome,
    pub data_quality: DataQuality,
}

/// In-memory active tracking state for an unresolved counterfactual opportunity
#[derive(Debug, Clone)]
pub struct ActiveOpportunity {
    pub opportunity_id: String,
    pub asset: String,
    pub engine: String,
    pub side: String, // "BUY" or "SELL"
    pub mode: String,
    pub created_at_ms: u64,
    pub timeout_at_ms: u64,
    pub horizon_seconds: u64,

    // Decision info
    pub decision: CounterfactualDecision,
    pub market_at_decision: MarketAtDecision,
    pub hypothetical_order: HypotheticalOrder,

    // Targets
    pub limit_price: f64,
    pub take_profit_price: f64,
    pub stop_loss_price: f64,

    // Fill tracking
    pub touch_only_observed: bool,
    pub trade_through_filled: bool,
    pub fill_price: Option<f64>,
    pub fill_timestamp_utc: Option<String>,
    pub fill_timestamp_ms: Option<u64>,

    // Excursions (measured strictly after fill)
    pub max_favorable_price: Option<f64>,
    pub max_adverse_price: Option<f64>,

    // Resolution
    pub is_resolved: bool,
    pub final_record: Option<CounterfactualRecord>,
}

impl ActiveOpportunity {
    pub fn new(
        asset: String,
        engine: String,
        side: String,
        mode: String,
        now_ms: u64,
        horizon_seconds: u64,
        decision: CounterfactualDecision,
        market_at_decision: MarketAtDecision,
        hypothetical_order: HypotheticalOrder,
        take_profit_price: f64,
        stop_loss_price: f64,
    ) -> Self {
        let opp_id = format!("opp_{}_{}_{}", asset, now_ms, Uuid::new_v4().simple());
        let limit_price = hypothetical_order.limit_price;

        Self {
            opportunity_id: opp_id,
            asset,
            engine,
            side,
            mode,
            created_at_ms: now_ms,
            timeout_at_ms: now_ms + (horizon_seconds * 1000),
            horizon_seconds,
            decision,
            market_at_decision,
            hypothetical_order,
            limit_price,
            take_profit_price,
            stop_loss_price,
            touch_only_observed: false,
            trade_through_filled: false,
            fill_price: None,
            fill_timestamp_utc: None,
            fill_timestamp_ms: None,
            max_favorable_price: None,
            max_adverse_price: None,
            is_resolved: false,
            final_record: None,
        }
    }

    /// Evaluates incoming 1m candle (high, low, close) to track fills, MFE/MAE, and exits
    pub fn update_with_candle(&mut self, high: f64, low: f64, close: f64, now_ms: u64) -> Option<CounterfactualRecord> {
        if self.is_resolved {
            return None;
        }

        let is_buy = self.side.to_uppercase() == "BUY" || self.side.to_uppercase() == "LONG";
        let tick_size = get_tick_size(&self.asset);

        // 1. STEP 1: FILL EVALUATION (if not yet filled)
        if !self.trade_through_filled {
            if is_buy {
                // For a Buy limit order at limit_price:
                // Touch: market traded at limit_price
                if low <= self.limit_price {
                    self.touch_only_observed = true;
                }
                // Trade-through: market traded strictly through limit_price - 1 tick
                if low <= (self.limit_price - tick_size) {
                    self.trade_through_filled = true;
                    self.fill_price = Some(self.limit_price);
                    self.fill_timestamp_utc = Some(Utc::now().to_rfc3339());
                    self.fill_timestamp_ms = Some(now_ms);
                    self.max_favorable_price = Some(self.limit_price);
                    self.max_adverse_price = Some(low);
                }
            } else {
                // For a Sell limit order at limit_price:
                if high >= self.limit_price {
                    self.touch_only_observed = true;
                }
                if high >= (self.limit_price + tick_size) {
                    self.trade_through_filled = true;
                    self.fill_price = Some(self.limit_price);
                    self.fill_timestamp_utc = Some(Utc::now().to_rfc3339());
                    self.fill_timestamp_ms = Some(now_ms);
                    self.max_favorable_price = Some(self.limit_price);
                    self.max_adverse_price = Some(high);
                }
            }

            // Check if unfilled opportunity timed out without fill
            if !self.trade_through_filled && now_ms >= self.timeout_at_ms {
                return Some(self.finalize_unfilled(close));
            }

            if !self.trade_through_filled {
                return None; // Still waiting for fill
            }
        }

        // 2. STEP 2: POSITION MONITORING (post-fill)
        let fill_px = match self.fill_price {
            Some(px) => px,
            None => return None,
        };

        if is_buy {
            // Update MFE (highest high post-fill)
            if let Some(ref mut fav) = self.max_favorable_price {
                if high > *fav {
                    *fav = high;
                }
            }
            // Update MAE (lowest low post-fill)
            if let Some(ref mut adv) = self.max_adverse_price {
                if low < *adv {
                    *adv = low;
                }
            }

            // Check Stop-Loss breach:
            let sl_breached = low <= self.stop_loss_price;
            // Check Take-Profit breach:
            let tp_reached = high >= self.take_profit_price;

            if sl_breached {
                return Some(self.finalize_filled(
                    fill_px,
                    self.stop_loss_price,
                    "STOP_LOSS",
                    VetoClassification::AlphaProtective,
                    true,
                    false,
                ));
            } else if tp_reached {
                return Some(self.finalize_filled(
                    fill_px,
                    self.take_profit_price,
                    "TAKE_PROFIT",
                    VetoClassification::AlphaStrangling,
                    false,
                    true,
                ));
            } else if now_ms >= self.timeout_at_ms {
                return Some(self.finalize_filled(
                    fill_px,
                    close,
                    "HORIZON_TIMEOUT",
                    VetoClassification::NeutralNoise,
                    false,
                    false,
                ));
            }
        } else {
            // Short position
            if let Some(ref mut fav) = self.max_favorable_price {
                if low < *fav {
                    *fav = low;
                }
            }
            if let Some(ref mut adv) = self.max_adverse_price {
                if high > *adv {
                    *adv = high;
                }
            }

            let sl_breached = high >= self.stop_loss_price;
            let tp_reached = low <= self.take_profit_price;

            if sl_breached {
                return Some(self.finalize_filled(
                    fill_px,
                    self.stop_loss_price,
                    "STOP_LOSS",
                    VetoClassification::AlphaProtective,
                    true,
                    false,
                ));
            } else if tp_reached {
                return Some(self.finalize_filled(
                    fill_px,
                    self.take_profit_price,
                    "TAKE_PROFIT",
                    VetoClassification::AlphaStrangling,
                    false,
                    true,
                ));
            } else if now_ms >= self.timeout_at_ms {
                return Some(self.finalize_filled(
                    fill_px,
                    close,
                    "HORIZON_TIMEOUT",
                    VetoClassification::NeutralNoise,
                    false,
                    false,
                ));
            }
        }

        None
    }

    fn finalize_unfilled(&mut self, _current_price: f64) -> CounterfactualRecord {
        self.is_resolved = true;
        let fill_status = if self.touch_only_observed {
            FillStatus::TouchOnly
        } else {
            FillStatus::Unfilled
        };

        let record = CounterfactualRecord {
            schema_version: 1,
            event_id: format!("cf_{}_{}", self.opportunity_id, Uuid::new_v4().simple()),
            opportunity_id: self.opportunity_id.clone(),
            ts_utc: Utc::now().to_rfc3339(),
            asset: self.asset.clone(),
            venue: "hyperliquid".to_string(),
            engine: self.engine.clone(),
            side: self.side.clone(),
            mode: self.mode.clone(),
            decision: self.decision.clone(),
            market_at_decision: self.market_at_decision.clone(),
            hypothetical_order: self.hypothetical_order.clone(),
            fill_model: FillModel {
                version: "shadow-bounds-v1".to_string(),
                class: "bounds".to_string(),
                touch_only: self.touch_only_observed,
                trade_through_filled: false,
                fill_price: None,
                fill_timestamp_utc: None,
                assumptions: vec![
                    "L2 queue position not observable".to_string(),
                    "Strict trade-through required for conservative fill".to_string(),
                ],
            },
            outcome: CounterfactualOutcome {
                horizon_seconds: self.horizon_seconds,
                exit_rule: "unfilled_timeout".to_string(),
                fill_status,
                exit_trigger: Some("EXPIRED_UNFILLED".to_string()),
                exit_price: None,
                mfe_bps: None,
                mae_bps: None,
                gross_pnl_bps: Some(0.0),
                fees_bps: Some(0.0),
                funding_bps: Some(0.0),
                spread_slippage_bps: Some(0.0),
                counterfactual_net_pnl_bps: Some(0.0),
                veto_classification: VetoClassification::NeutralNoise,
                avoided_loss: false,
                missed_profit: false,
            },
            data_quality: DataQuality {
                book_source: "l2Book".to_string(),
                trade_stream_available: true,
                missing_intervals: 0,
            },
        };

        self.final_record = Some(record.clone());
        record
    }

    fn finalize_filled(
        &mut self,
        fill_px: f64,
        exit_px: f64,
        exit_trigger: &str,
        veto_class: VetoClassification,
        avoided_loss: bool,
        missed_profit: bool,
    ) -> CounterfactualRecord {
        self.is_resolved = true;
        let is_buy = self.side.to_uppercase() == "BUY" || self.side.to_uppercase() == "LONG";

        // Excursions calculated STRICTLY from fill_price:
        let fav_px = self.max_favorable_price.unwrap_or(fill_px);
        let adv_px = self.max_adverse_price.unwrap_or(fill_px);

        let (mfe_bps, mae_bps, gross_pnl_bps) = if is_buy {
            let mfe = ((fav_px - fill_px) / fill_px) * 10_000.0;
            let mae = ((adv_px - fill_px) / fill_px) * 10_000.0;
            let gross = ((exit_px - fill_px) / fill_px) * 10_000.0;
            (mfe, mae, gross)
        } else {
            let mfe = ((fill_px - fav_px) / fill_px) * 10_000.0;
            let mae = ((fill_px - adv_px) / fill_px) * 10_000.0;
            let gross = ((fill_px - exit_px) / fill_px) * 10_000.0;
            (mfe, mae, gross)
        };

        // Realistic friction: Maker entry fee + Taker exit fee (if SL) or Maker exit fee (if TP)
        let exit_fee_bps = if exit_trigger == "TAKE_PROFIT" {
            HYPERLIQUID_BASE_MAKER_FEE_BPS
        } else {
            HYPERLIQUID_BASE_TAKER_FEE_BPS
        };
        let fees_bps = HYPERLIQUID_BASE_MAKER_FEE_BPS + exit_fee_bps;
        let slippage_bps = if exit_trigger == "STOP_LOSS" {
            ESTIMATED_SLIPPAGE_BPS
        } else {
            0.0
        };
        let net_pnl_bps = gross_pnl_bps - fees_bps - slippage_bps;

        let record = CounterfactualRecord {
            schema_version: 1,
            event_id: format!("cf_{}_{}", self.opportunity_id, Uuid::new_v4().simple()),
            opportunity_id: self.opportunity_id.clone(),
            ts_utc: Utc::now().to_rfc3339(),
            asset: self.asset.clone(),
            venue: "hyperliquid".to_string(),
            engine: self.engine.clone(),
            side: self.side.clone(),
            mode: self.mode.clone(),
            decision: self.decision.clone(),
            market_at_decision: self.market_at_decision.clone(),
            hypothetical_order: self.hypothetical_order.clone(),
            fill_model: FillModel {
                version: "shadow-bounds-v1".to_string(),
                class: "bounds".to_string(),
                touch_only: self.touch_only_observed,
                trade_through_filled: true,
                fill_price: Some(fill_px),
                fill_timestamp_utc: self.fill_timestamp_utc.clone(),
                assumptions: vec![
                    "Conservative trade-through confirmed (1 tick through)".to_string(),
                    "Adverse selection accounted for post-fill".to_string(),
                ],
            },
            outcome: CounterfactualOutcome {
                horizon_seconds: self.horizon_seconds,
                exit_rule: "strategy_default".to_string(),
                fill_status: FillStatus::TradeThroughConfirmed,
                exit_trigger: Some(exit_trigger.to_string()),
                exit_price: Some(exit_px),
                mfe_bps: Some(mfe_bps),
                mae_bps: Some(mae_bps),
                gross_pnl_bps: Some(gross_pnl_bps),
                fees_bps: Some(fees_bps),
                funding_bps: Some(0.0),
                spread_slippage_bps: Some(slippage_bps),
                counterfactual_net_pnl_bps: Some(net_pnl_bps),
                veto_classification: veto_class,
                avoided_loss,
                missed_profit,
            },
            data_quality: DataQuality {
                book_source: "l2Book".to_string(),
                trade_stream_available: true,
                missing_intervals: 0,
            },
        };

        self.final_record = Some(record.clone());
        record
    }
}

/// Global Coordinator for Counterfactual Evaluation across all assets
pub struct CounterfactualTracker {
    pub active_opportunities: Vec<ActiveOpportunity>,
    pub journal_path: String,
    pub de_duplication_cooldown_ms: u64,
}

impl CounterfactualTracker {
    pub fn new(journal_path: &str) -> Self {
        Self {
            active_opportunities: Vec::new(),
            journal_path: journal_path.to_string(),
            // Minimum time between logging candidates for the same asset & engine to prevent episode autocorrelation
            de_duplication_cooldown_ms: 15 * 60 * 1000, // 15 minutes
        }
    }

    /// Checks if an active episode already exists for this asset & engine
    pub fn has_active_episode(&self, asset: &str, engine: &str, now_ms: u64) -> bool {
        self.active_opportunities.iter().any(|opp| {
            opp.asset == asset
                && opp.engine == engine
                && !opp.is_resolved
                && (now_ms - opp.created_at_ms) < self.de_duplication_cooldown_ms
        })
    }

    /// Registers a new counterfactual candidate (subject to episode de-duplication)
    pub fn register_opportunity(&mut self, opp: ActiveOpportunity) -> bool {
        if self.has_active_episode(&opp.asset, &opp.engine, opp.created_at_ms) {
            return false; // Suppress duplicate candidate in same clustering block
        }
        self.active_opportunities.push(opp);
        true
    }

    /// Updates all active opportunities with a new 1m candle bar
    pub fn on_candle_bar(&mut self, asset: &str, high: f64, low: f64, close: f64, now_ms: u64) {
        let mut resolved_records = Vec::new();

        for opp in self.active_opportunities.iter_mut() {
            if opp.asset == asset && !opp.is_resolved {
                if let Some(record) = opp.update_with_candle(high, low, close, now_ms) {
                    resolved_records.push(record);
                }
            }
        }

        // Write resolved records to JSONL journal
        for record in resolved_records {
            self.append_to_journal(&record);
            println!(
                "🔬 [COUNTERFACTUAL RESOLVED] {} {} | Veto: {} | Status: {:?} | Outcome: {} | Net PnL: {:.1} bps (MFE: {:.1}, MAE: {:.1}) | Class: {}",
                record.asset,
                record.engine,
                record.decision.primary_veto,
                record.outcome.fill_status,
                record.outcome.exit_trigger.as_deref().unwrap_or("N/A"),
                record.outcome.counterfactual_net_pnl_bps.unwrap_or(0.0),
                record.outcome.mfe_bps.unwrap_or(0.0),
                record.outcome.mae_bps.unwrap_or(0.0),
                record.outcome.veto_classification
            );
        }

        // Retain only unresolved opportunities
        self.active_opportunities.retain(|opp| !opp.is_resolved);
    }

    fn append_to_journal(&self, record: &CounterfactualRecord) {
        if let Ok(serialized) = serde_json::to_string(record) {
            if let Ok(mut file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.journal_path)
            {
                let _ = writeln!(file, "{}", serialized);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_opportunity(side: &str, limit_px: f64, sl_px: f64, tp_px: f64) -> ActiveOpportunity {
        ActiveOpportunity::new(
            "ETH".to_string(),
            "EngineA_MeanReversion".to_string(),
            side.to_string(),
            "NEUTRAL".to_string(),
            1000_000,
            1800,
            CounterfactualDecision {
                accepted: false,
                primary_veto: "SHOCK_SENTINEL".to_string(),
                all_vetos: vec!["SHOCK_SENTINEL".to_string(), "COOLDOWN_ACTIVE".to_string()],
                threshold_sigma: 2.0,
                signal_value: -3.35,
                hmm_probabilities: HmmProbs {
                    consolidation: 0.44,
                    trend: 0.56,
                    shock: 0.0,
                },
            },
            MarketAtDecision {
                reference_price: 2485.50,
                bid: 2485.40,
                ask: 2485.60,
                spread_bps: 0.80,
                l2_age_ms: 450,
                obi: -0.12,
                volatility_z15: -3.35,
                volatility_z60: Some(-2.11),
            },
            HypotheticalOrder {
                order_type: "post_only_limit".to_string(),
                limit_price: limit_px,
                size: 0.01,
                time_in_force: "GTC".to_string(),
                assumed_cancel_rule: "strategy_default".to_string(),
                queue_ahead_estimate: None,
            },
            tp_px,
            sl_px,
        )
    }

    #[test]
    fn test_strict_through_fill_rule_and_touch_only() {
        // ETH tick size = 0.10. Limit buy at 2487.75.
        // Touch only: low is 2487.75.
        // Trade through: low is <= 2487.65.
        let mut opp = create_test_opportunity("BUY", 2487.75, 2465.0, 2510.0);

        // Candle 1: High 2490.0, Low 2487.75 (exact touch)
        let res1 = opp.update_with_candle(2490.0, 2487.75, 2488.0, 1060_000);
        assert!(res1.is_none());
        assert!(opp.touch_only_observed);
        assert!(!opp.trade_through_filled);
        assert_eq!(opp.fill_price, None);

        // Candle 2: High 2488.0, Low 2487.60 (trades through by 0.15 > 0.10 tick)
        let res2 = opp.update_with_candle(2488.0, 2487.60, 2487.80, 1120_000);
        assert!(res2.is_none());
        assert!(opp.trade_through_filled);
        assert_eq!(opp.fill_price, Some(2487.75));
    }

    #[test]
    fn test_alpha_protective_classification_on_stop_loss() {
        // Scenario matching today's flash dump: Buy ETH limit at 2487.75, SL at 2475.0, TP at 2510.0
        let mut opp = create_test_opportunity("BUY", 2487.75, 2475.0, 2510.0);

        // Candle 1: Fills through at 2487.60
        opp.update_with_candle(2489.0, 2487.60, 2488.0, 1060_000);
        assert!(opp.trade_through_filled);

        // Candle 2: Market drops to 2474.0 (hits SL 2475.0)
        let res = opp.update_with_candle(2480.0, 2474.0, 2476.0, 1120_000);
        assert!(res.is_some());
        let record = res.unwrap();

        assert_eq!(record.outcome.veto_classification, VetoClassification::AlphaProtective);
        assert!(record.outcome.avoided_loss);
        assert!(!record.outcome.missed_profit);
        assert_eq!(record.outcome.exit_trigger, Some("STOP_LOSS".to_string()));
        assert_eq!(record.outcome.exit_price, Some(2475.0));
        assert!(record.outcome.counterfactual_net_pnl_bps.unwrap() < 0.0);
    }

    #[test]
    fn test_alpha_strangling_classification_on_take_profit() {
        // Buy limit at 2487.75, SL at 2475.0, TP at 2510.0
        let mut opp = create_test_opportunity("BUY", 2487.75, 2475.0, 2510.0);

        // Candle 1: Fills through
        opp.update_with_candle(2489.0, 2487.50, 2488.0, 1060_000);

        // Candle 2: Market surges to 2512.0 (hits TP 2510.0)
        let res = opp.update_with_candle(2512.0, 2488.0, 2510.0, 1120_000);
        assert!(res.is_some());
        let record = res.unwrap();

        assert_eq!(record.outcome.veto_classification, VetoClassification::AlphaStrangling);
        assert!(!record.outcome.avoided_loss);
        assert!(record.outcome.missed_profit);
        assert_eq!(record.outcome.exit_trigger, Some("TAKE_PROFIT".to_string()));
        assert_eq!(record.outcome.exit_price, Some(2510.0));
        assert!(record.outcome.counterfactual_net_pnl_bps.unwrap() > 0.0);
    }

    #[test]
    fn test_de_duplication_episode_clustering() {
        let mut tracker = CounterfactualTracker::new("/tmp/test_cf_journal.jsonl");

        let opp1 = create_test_opportunity("BUY", 2487.75, 2475.0, 2510.0);
        let accepted1 = tracker.register_opportunity(opp1);
        assert!(accepted1);

        // Attempting to register another opportunity for the same asset & engine within cooldown:
        let opp2 = create_test_opportunity("BUY", 2486.00, 2475.0, 2510.0);
        let accepted2 = tracker.register_opportunity(opp2);
        assert!(!accepted2); // Blocked by episode clustering!
    }
}
