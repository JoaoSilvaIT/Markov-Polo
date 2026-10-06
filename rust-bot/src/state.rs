use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

const STATE_FILE: &str = "../binance_hmm_state.json";
const JOURNAL_MD: &str = "../binance_hmm_journal.md";
const JOURNAL_JSONL: &str = "../binance_hmm_journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionState {
    pub symbol: String,
    pub order_id: String,
    pub tp_order_id: String,
    pub entry_price: f64,
    pub amount: f64,
    pub stop_loss_price: f64,
    pub take_profit_price: f64,
    pub opened_at: String,
    pub risk_amount_usdc: f64,
}

pub type StateMap = HashMap<String, PositionState>;

pub fn load_state(file_path: &str) -> Result<StateMap, Box<dyn std::error::Error>> {
    if !std::path::Path::new(file_path).exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(file_path)?;
    if content.trim().is_empty() {
        return Ok(HashMap::new());
    }
    let state: StateMap = serde_json::from_str(&content)?;
    Ok(state)
}

pub fn save_state(file_path: &str, state: &StateMap) -> Result<(), Box<dyn std::error::Error>> {
    let content = serde_json::to_string_pretty(state)?;
    // Atomic file save to prevent corruption on panic/power loss
    let temp_path = format!("{}.tmp", file_path);
    fs::write(&temp_path, content)?;
    fs::rename(&temp_path, file_path)?;
    Ok(())
}

#[derive(Serialize)]
struct JournalEntry {
    timestamp: String,
    mode: String,
    event_type: String,
    details: Value,
    memo: String,
}

pub fn log_to_journal(memo: &str, event_type: &str, details: Value) {
    let now = Utc::now();
    
    let entry = JournalEntry {
        timestamp: now.to_rfc3339(),
        mode: "binance_crypto".to_string(),
        event_type: event_type.to_string(),
        details,
        memo: memo.trim().to_string(),
    };

    if let Ok(json_str) = serde_json::to_string(&entry) {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(JOURNAL_JSONL) {
            let _ = writeln!(file, "{}", json_str);
        }
    }

    let md_exists = std::path::Path::new(JOURNAL_MD).exists();
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(JOURNAL_MD) {
        if !md_exists {
            let _ = writeln!(file, "# Binance Quantitative Trading Journal\n");
        }
        let _ = writeln!(file, "## [{}] {}\n", now.format("%Y-%m-%d %H:%M:%S UTC"), event_type);
        let memo_text = if memo.trim().is_empty() { "No text memo generated." } else { memo.trim() };
        let _ = writeln!(file, "{}\n\n---", memo_text);
    }
}
