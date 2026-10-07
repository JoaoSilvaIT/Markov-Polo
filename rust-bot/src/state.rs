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
    #[serde(default)]
    pub is_filled: bool,
    #[serde(default)]
    pub half_life_minutes: f64,
    #[serde(default)]
    pub client_order_id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CircuitBreaker {
    pub consecutive_time_stops: u32,
    pub paused_until: Option<String>, // RFC3339 timestamp
}

impl CircuitBreaker {
    pub fn is_paused(&self) -> bool {
        if let Some(ref ts) = self.paused_until {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
                return chrono::Utc::now() < dt.with_timezone(&chrono::Utc);
            }
        }
        false
    }

    pub fn record_time_stop(&mut self) -> bool {
        self.consecutive_time_stops += 1;
        if self.consecutive_time_stops >= 3 {
            let pause_dt = chrono::Utc::now() + chrono::Duration::hours(24);
            self.paused_until = Some(pause_dt.to_rfc3339());
            return true;
        }
        false
    }

    pub fn record_win(&mut self) {
        self.consecutive_time_stops = 0;
        self.paused_until = None;
    }
}

pub fn load_circuit_breaker(file_path: &str) -> CircuitBreaker {
    if !std::path::Path::new(file_path).exists() {
        return CircuitBreaker::default();
    }
    fs::read_to_string(file_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn save_circuit_breaker(file_path: &str, cb: &CircuitBreaker) {
    if let Ok(content) = serde_json::to_string_pretty(cb) {
        let temp_path = format!("{}.tmp", file_path);
        let _ = fs::write(&temp_path, content);
        let _ = fs::rename(&temp_path, file_path);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArmedSignal {
    pub is_armed: bool,
    pub armed_at: Option<String>,
    pub entry_threshold: f64,
    #[serde(default)]
    pub exit_threshold: f64,
    #[serde(default)]
    pub unitless_exit_dev: f64,
    #[serde(default)]
    pub unitless_entry_dev: f64,
    #[serde(default)]
    pub half_life_m: f64,
    pub lowest_price: f64,
}

pub fn load_armed_signal(file_path: &str) -> ArmedSignal {
    if !std::path::Path::new(file_path).exists() {
        return ArmedSignal::default();
    }
    fs::read_to_string(file_path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

pub fn save_armed_signal(file_path: &str, armed: &ArmedSignal) {
    if let Ok(content) = serde_json::to_string_pretty(armed) {
        let temp_path = format!("{}.tmp", file_path);
        let _ = fs::write(&temp_path, content);
        let _ = fs::rename(&temp_path, file_path);
    }
}

pub const CLUSTER_LOCK_FILE: &str = "binance_portfolio_cluster.lock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterLease {
    pub owner: String,
    pub acquired_at: String,
    pub expires_at: String,
}

/// Atomically attempts to acquire the portfolio cluster lease via POSIX O_CREAT|O_EXCL
pub fn try_acquire_cluster_lease(symbol: &str, duration_secs: i64) -> bool {
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration_secs);

    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner == symbol {
                    // Already owned by this symbol, refresh expiration
                    let new_lease = ClusterLease {
                        owner: symbol.to_string(),
                        acquired_at: lease.acquired_at,
                        expires_at: expires.to_rfc3339(),
                    };
                    let _ = fs::write(CLUSTER_LOCK_FILE, serde_json::to_string_pretty(&new_lease).unwrap_or_default());
                    return true;
                }
                
                // Check if current lease owner has an active open position in state file
                let owner_state_file = format!("binance_hmm_state_{}.json", lease.owner);
                let owner_has_pos = load_state(&owner_state_file)
                    .ok()
                    .and_then(|s| s.get(&lease.owner).cloned())
                    .map(|p| !p.order_id.is_empty())
                    .unwrap_or(false);

                if owner_has_pos {
                    return false; // Actively held by open position
                }

                if let Ok(exp_dt) = chrono::DateTime::parse_from_rfc3339(&lease.expires_at) {
                    if now < exp_dt.with_timezone(&chrono::Utc) {
                        // Active lease held by another symbol
                        return false;
                    }
                }
            }
        }
        // Stale or expired lock file; clean it up
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
    }

    let lease = ClusterLease {
        owner: symbol.to_string(),
        acquired_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
    };
    let content = match serde_json::to_string_pretty(&lease) {
        Ok(c) => c,
        Err(_) => return false,
    };

    match OpenOptions::new().write(true).create_new(true).open(CLUSTER_LOCK_FILE) {
        Ok(mut f) => {
            let _ = f.write_all(content.as_bytes());
            true
        }
        Err(_) => false,
    }
}

/// Refreshes/renews the active cluster lease while position or entry order is open
pub fn refresh_cluster_lease(symbol: &str, duration_secs: i64) -> bool {
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration_secs);
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(mut lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner == symbol {
                    lease.expires_at = expires.to_rfc3339();
                    if let Ok(pretty) = serde_json::to_string_pretty(&lease) {
                        let tmp = format!("{}.tmp_{}", CLUSTER_LOCK_FILE, symbol);
                        let _ = fs::write(&tmp, pretty);
                        let _ = fs::rename(&tmp, CLUSTER_LOCK_FILE);
                        return true;
                    }
                }
            }
        }
    }
    try_acquire_cluster_lease(symbol, duration_secs)
}

/// Releases the portfolio cluster lease if held by symbol
pub fn release_cluster_lease(symbol: &str) {
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner == symbol {
                    let _ = fs::remove_file(CLUSTER_LOCK_FILE);
                }
            }
        }
    }
}

/// Checks if any other symbol in the portfolio cluster currently has an active open position or active atomic lease
pub fn has_active_cluster_position(current_symbol: &str) -> Option<String> {
    let now = chrono::Utc::now();
    // 1. Check atomic lease lock
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner != current_symbol {
                    // Check if lease owner has an active position in state
                    let owner_state_file = format!("binance_hmm_state_{}.json", lease.owner);
                    let owner_has_pos = load_state(&owner_state_file)
                        .ok()
                        .and_then(|s| s.get(&lease.owner).cloned())
                        .map(|p| !p.order_id.is_empty())
                        .unwrap_or(false);

                    if owner_has_pos {
                        return Some(lease.owner);
                    }

                    if let Ok(exp_dt) = chrono::DateTime::parse_from_rfc3339(&lease.expires_at) {
                        if now < exp_dt.with_timezone(&chrono::Utc) {
                            return Some(lease.owner);
                        }
                    }
                }
            }
        }
    }

    // 2. Check position state files across all symbols
    let all_symbols = ["BTCUSDC", "ETHUSDC", "SOLUSDC"];
    for sym in &all_symbols {
        if *sym == current_symbol {
            continue;
        }
        let state_file = format!("binance_hmm_state_{}.json", sym);
        if let Ok(state) = load_state(&state_file) {
            if let Some(pos) = state.get(*sym) {
                if !pos.order_id.is_empty() {
                    return Some(sym.to_string());
                }
            }
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_state_backward_compatibility() {
        let old_json = r#"{
            "BTC/USDC": {
                "symbol": "BTC/USDC",
                "order_id": "123456",
                "tp_order_id": "",
                "entry_price": 85000.0,
                "amount": 0.00014,
                "stop_loss_price": 0.0,
                "take_profit_price": 85500.0,
                "opened_at": "2026-10-06T12:00:00Z",
                "risk_amount_usdc": 12.0
            }
        }"#;

        let state: StateMap = serde_json::from_str(old_json).expect("Deserialization must succeed");
        let btc = state.get("BTC/USDC").expect("Must contain BTC/USDC");
        assert_eq!(btc.is_filled, false);
        assert_eq!(btc.order_id, "123456");
        assert_eq!(btc.amount, 0.00014);
    }

    #[test]
    fn test_position_state_with_is_filled() {
        let new_json = r#"{
            "ETH/USDC": {
                "symbol": "ETH/USDC",
                "order_id": "999999",
                "tp_order_id": "888888",
                "entry_price": 2680.0,
                "amount": 0.0044,
                "stop_loss_price": 0.0,
                "take_profit_price": 2700.0,
                "opened_at": "2026-10-06T12:00:00Z",
                "risk_amount_usdc": 12.0,
                "is_filled": true
            }
        }"#;

        let state: StateMap = serde_json::from_str(new_json).expect("Deserialization must succeed");
        let eth = state.get("ETH/USDC").expect("Must contain ETH/USDC");
        assert_eq!(eth.is_filled, true);
        assert_eq!(eth.tp_order_id, "888888");
    }

    #[test]
    fn test_circuit_breaker_3_strike_rule() {
        let mut cb = CircuitBreaker::default();
        assert_eq!(cb.is_paused(), false);
        
        assert_eq!(cb.record_time_stop(), false); // 1 strike
        assert_eq!(cb.is_paused(), false);
        
        assert_eq!(cb.record_time_stop(), false); // 2 strikes
        assert_eq!(cb.is_paused(), false);
        
        assert_eq!(cb.record_time_stop(), true); // 3 strikes -> Tripped!
        assert_eq!(cb.is_paused(), true);
        
        cb.record_win(); // Win resets breaker
        assert_eq!(cb.is_paused(), false);
        assert_eq!(cb.consecutive_time_stops, 0);
    }

    #[test]
    fn test_armed_signal_state_lifecycle() {
        let mut arm = ArmedSignal::default();
        assert_eq!(arm.is_armed, false);

        // Stage 1: Arm on dip with unitless deviations relative to SMA
        let sma_arm = 121.0_f64;
        arm.is_armed = true;
        arm.armed_at = Some(chrono::Utc::now().to_rfc3339());
        arm.entry_threshold = 120.26;
        arm.exit_threshold = 122.50;
        arm.unitless_entry_dev = (120.26_f64 / sma_arm).ln();
        arm.unitless_exit_dev = (122.50_f64 / sma_arm).ln();
        arm.half_life_m = 35.0;
        arm.lowest_price = 120.14;
        assert_eq!(arm.is_armed, true);
        assert_eq!(arm.exit_threshold, 122.50);

        // Stage 2: Deeper dip updates lowest price
        if 119.80 < arm.lowest_price {
            arm.lowest_price = 119.80;
        }
        assert_eq!(arm.lowest_price, 119.80);

        // Stage 2 execution: baseline dragged to 120.0
        let sma_bounce = 120.0;
        let dynamic_tp = sma_bounce * arm.unitless_exit_dev.exp();
        // Dynamic TP should be lower than original 122.50 because baseline dragged down
        assert!(dynamic_tp < 122.50);
        assert!((dynamic_tp - (120.0 * (122.50 / 121.0))).abs() < 1e-4);

        // Stage 3: Disarm on submit
        arm.is_armed = false;
        assert_eq!(arm.is_armed, false);
    }

    #[test]
    fn test_atomic_cluster_lease() {
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        // Symbol A acquires lease
        assert!(try_acquire_cluster_lease("BTCUSDC", 60));
        // Symbol B fails to acquire lease
        assert!(!try_acquire_cluster_lease("ETHUSDC", 60));
        // Symbol A has active cluster position from ETH's perspective
        assert_eq!(has_active_cluster_position("ETHUSDC"), Some("BTCUSDC".to_string()));
        // Symbol A releases lease
        release_cluster_lease("BTCUSDC");
        // Symbol B can now acquire lease
        assert!(try_acquire_cluster_lease("ETHUSDC", 60));
        release_cluster_lease("ETHUSDC");
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
    }
}
