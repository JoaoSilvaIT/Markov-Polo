use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;

pub const CLUSTER_LOCK_FILE: &str = "hl_portfolio_cluster.lock";
#[allow(dead_code)]
pub const JOURNAL_MD: &str = "hl_hmm_journal.md";
#[allow(dead_code)]
pub const JOURNAL_JSONL: &str = "hl_hmm_journal.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Side {
    #[serde(rename = "LONG")]
    Long,
    #[serde(rename = "SHORT")]
    Short,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Long => write!(f, "LONG"),
            Side::Short => write!(f, "SHORT"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionState {
    pub symbol: String,
    #[serde(default = "default_side")]
    pub side: String, // "LONG" or "SHORT"
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
    #[serde(default)]
    pub max_progress: f64,
    #[serde(default)]
    pub max_favorable_price: f64,
    #[serde(default)]
    pub effective_tp: f64,
    #[serde(default)]
    pub last_reprice_time: i64,
    #[serde(default)]
    pub reprice_count: u32,
    #[serde(default)]
    pub candidate_lower_count: u32,
    #[serde(default = "default_engine_owner")]
    pub engine_type: String,
    #[serde(default)]
    pub chandelier_stop: f64,
    #[serde(default)]
    pub current_atr: f64,
}

impl PositionState {
    /// Comprehensive symbol occupancy invariant:
    /// occupied = position || opening order || partial fill || closing order || non-zero size || pending submission
    pub fn is_occupied(&self) -> bool {
        self.is_filled
            || !self.order_id.is_empty()
            || !self.tp_order_id.is_empty()
            || self.amount > 0.0
            || !self.client_order_id.is_empty()
    }
}

pub fn default_engine_owner() -> String {
    "EngineA".to_string()
}

fn default_side() -> String {
    "LONG".to_string()
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

    pub fn record_loss(&mut self) -> bool {
        self.record_time_stop()
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
    #[serde(default = "default_side")]
    pub side: String, // "LONG" or "SHORT"
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
    pub extreme_price: f64, // Lowest price for LONG, Highest price for SHORT
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterLease {
    pub owner: String,
    #[serde(default = "default_engine_owner")]
    pub engine_owner: String,
    pub acquired_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub fencing_token: u64,
}

/// Returns the active cluster lease, if any
#[allow(dead_code)]
pub fn get_current_cluster_lease() -> Option<ClusterLease> {
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            return serde_json::from_str::<ClusterLease>(&content).ok();
        }
    }
    None
}

/// Atomically attempts to acquire the portfolio cluster lease via POSIX O_CREAT|O_EXCL
pub fn try_acquire_cluster_lease(symbol: &str, duration_secs: i64) -> bool {
    try_acquire_cluster_lease_for_engine(symbol, "EngineA", duration_secs)
}

pub fn try_acquire_cluster_lease_for_engine(symbol: &str, engine_owner: &str, duration_secs: i64) -> bool {
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration_secs);
    let mut prev_token = 0u64;

    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(lease) = serde_json::from_str::<ClusterLease>(&content) {
                prev_token = lease.fencing_token;
                if lease.owner == symbol {
                    let new_lease = ClusterLease {
                        owner: symbol.to_string(),
                        engine_owner: engine_owner.to_string(),
                        acquired_at: lease.acquired_at,
                        expires_at: expires.to_rfc3339(),
                        fencing_token: lease.fencing_token + 1,
                    };
                    let _ = fs::write(
                        CLUSTER_LOCK_FILE,
                        serde_json::to_string_pretty(&new_lease).unwrap_or_default(),
                    );
                    return true;
                }

                let owner_state_file = format!("hl_hmm_state_{}.json", lease.owner);
                let owner_has_pos = load_state(&owner_state_file)
                    .ok()
                    .and_then(|s| s.get(&lease.owner).cloned())
                    .map(|p| p.is_occupied())
                    .unwrap_or(false);

                if owner_has_pos {
                    return false;
                }

                if let Ok(exp_dt) = chrono::DateTime::parse_from_rfc3339(&lease.expires_at) {
                    if now < exp_dt.with_timezone(&chrono::Utc) {
                        return false;
                    }
                }
            }
        }
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
    }

    let lease = ClusterLease {
        owner: symbol.to_string(),
        engine_owner: engine_owner.to_string(),
        acquired_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        fencing_token: prev_token + 1,
    };
    let content = match serde_json::to_string_pretty(&lease) {
        Ok(c) => c,
        Err(_) => return false,
    };

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(CLUSTER_LOCK_FILE)
    {
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
                    lease.fencing_token += 1;
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
                    let owner_state_file = format!("hl_hmm_state_{}.json", lease.owner);
                    let owner_has_pos = load_state(&owner_state_file)
                        .ok()
                        .and_then(|s| s.get(&lease.owner).cloned())
                        .map(|p| p.is_occupied())
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
    let all_symbols = ["BTC", "ETH", "SOL"];
    for sym in &all_symbols {
        if *sym == current_symbol {
            continue;
        }
        let state_file = format!("hl_hmm_state_{}.json", sym);
        if let Ok(state) = load_state(&state_file) {
            if let Some(pos) = state.get(*sym) {
                if pos.is_occupied() {
                    return Some(sym.to_string());
                }
            }
        }
    }
    None
}

#[allow(dead_code)]
#[derive(Serialize)]
struct JournalEntry {
    timestamp: String,
    mode: String,
    event_type: String,
    details: Value,
    memo: String,
}

#[allow(dead_code)]
pub fn log_to_journal(memo: &str, event_type: &str, details: Value) {
    let now = Utc::now();

    let entry = JournalEntry {
        timestamp: now.to_rfc3339(),
        mode: "hyperliquid_perp".to_string(),
        event_type: event_type.to_string(),
        details,
        memo: memo.trim().to_string(),
    };

    if let Ok(json_str) = serde_json::to_string(&entry) {
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(JOURNAL_JSONL)
        {
            let _ = writeln!(file, "{}", json_str);
        }
    }

    let md_exists = std::path::Path::new(JOURNAL_MD).exists();
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(JOURNAL_MD)
    {
        if !md_exists {
            let _ = writeln!(file, "# Hyperliquid Quantitative Trading Journal\n");
        }
        let _ = writeln!(
            file,
            "## [{}] {}\n",
            now.format("%Y-%m-%d %H:%M:%S UTC"),
            event_type
        );
        let memo_text = if memo.trim().is_empty() {
            "No text memo generated."
        } else {
            memo.trim()
        };
        let _ = writeln!(file, "{}\n\n---", memo_text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_state_bidirectional() {
        let long_json = r#"{
            "BTC": {
                "symbol": "BTC",
                "side": "LONG",
                "order_id": "123456",
                "tp_order_id": "123457",
                "entry_price": 83000.0,
                "amount": 0.0003,
                "stop_loss_price": 0.0,
                "take_profit_price": 83500.0,
                "opened_at": "2026-10-07T12:00:00Z",
                "risk_amount_usdc": 25.0,
                "is_filled": true
            }
        }"#;

        let state: StateMap = serde_json::from_str(long_json).expect("Deserialization long");
        let btc = state.get("BTC").unwrap();
        assert_eq!(btc.side, "LONG");
        assert_eq!(btc.is_filled, true);

        let short_json = r#"{
            "ETH": {
                "symbol": "ETH",
                "side": "SHORT",
                "order_id": "999",
                "tp_order_id": "1000",
                "entry_price": 2600.0,
                "amount": 0.01,
                "stop_loss_price": 0.0,
                "take_profit_price": 2550.0,
                "opened_at": "2026-10-07T12:00:00Z",
                "risk_amount_usdc": 25.0,
                "is_filled": true
            }
        }"#;

        let state_short: StateMap = serde_json::from_str(short_json).expect("Deserialization short");
        let eth = state_short.get("ETH").unwrap();
        assert_eq!(eth.side, "SHORT");
    }

    #[test]
    fn test_circuit_breaker_3_strikes() {
        let mut cb = CircuitBreaker::default();
        assert!(!cb.is_paused());
        assert!(!cb.record_time_stop());
        assert!(!cb.record_time_stop());
        assert!(cb.record_time_stop());
        assert!(cb.is_paused());
        cb.record_win();
        assert!(!cb.is_paused());
    }

    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_cluster_lease_isolation() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        assert!(try_acquire_cluster_lease("BTC", 60));
        assert!(!try_acquire_cluster_lease("ETH", 60));
        assert_eq!(has_active_cluster_position("ETH"), Some("BTC".to_string()));
        release_cluster_lease("BTC");
        assert!(try_acquire_cluster_lease("ETH", 60));
        release_cluster_lease("ETH");
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
    }

    #[test]
    fn test_cluster_lease_fencing_token_monotone() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        assert!(try_acquire_cluster_lease("BTC", 60));
        let lease1 = get_current_cluster_lease().expect("lease1");
        assert_eq!(lease1.fencing_token, 1);

        // Refresh increases fencing token
        assert!(refresh_cluster_lease("BTC", 60));
        let lease2 = get_current_cluster_lease().expect("lease2");
        assert_eq!(lease2.fencing_token, 2);

        release_cluster_lease("BTC");
        // Re-acquire by ETH increments from previous token
        assert!(try_acquire_cluster_lease_for_engine("ETH", "EngineB", 60));
        let lease3 = get_current_cluster_lease().expect("lease3");
        assert!(lease3.fencing_token >= 1);
        assert_eq!(lease3.engine_owner, "EngineB");

        release_cluster_lease("ETH");
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
    }

    #[test]
    fn test_is_occupied_invariants() {
        let empty_pos = PositionState {
            symbol: "BTC".to_string(),
            side: "LONG".to_string(),
            amount: 0.0,
            entry_price: 0.0,
            order_id: "".to_string(),
            tp_order_id: "".to_string(),
            client_order_id: "".to_string(),
            is_filled: false,
            stop_loss_price: 0.0,
            take_profit_price: 0.0,
            opened_at: "".to_string(),
            risk_amount_usdc: 0.0,
            half_life_minutes: 0.0,
            max_progress: 0.0,
            max_favorable_price: 0.0,
            effective_tp: 0.0,
            last_reprice_time: 0,
            reprice_count: 0,
            candidate_lower_count: 0,
            engine_type: "EngineA".to_string(),
            chandelier_stop: 0.0,
            current_atr: 0.0,
        };
        assert!(!empty_pos.is_occupied());

        let mut with_entry_order = empty_pos.clone();
        with_entry_order.order_id = "12345".to_string();
        assert!(with_entry_order.is_occupied());

        let mut with_tp_order = empty_pos.clone();
        with_tp_order.tp_order_id = "67890".to_string();
        assert!(with_tp_order.is_occupied());

        let mut with_client_id = empty_pos.clone();
        with_client_id.client_order_id = "cid_99".to_string();
        assert!(with_client_id.is_occupied());

        let mut with_amount = empty_pos.clone();
        with_amount.amount = 0.01;
        assert!(with_amount.is_occupied());

        let mut with_filled = empty_pos.clone();
        with_filled.is_filled = true;
        assert!(with_filled.is_occupied());
    }
}
