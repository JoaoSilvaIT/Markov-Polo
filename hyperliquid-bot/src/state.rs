use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;

pub const CLUSTER_LOCK_FILE: &str = "hl_portfolio_cluster.lock";
pub const CLUSTER_LOCK_GUARD_FILE: &str = "hl_portfolio_cluster.lock.guard";

/// RAII POSIX File Lock Guard using kernel flock(2) for atomic critical sections
pub struct FlockGuard {
    _file: std::fs::File,
}

impl FlockGuard {
    pub fn acquire(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

impl Drop for FlockGuard {
    fn drop(&mut self) {
        let fd = self._file.as_raw_fd();
        let _ = unsafe { libc::flock(fd, libc::LOCK_UN) };
    }
}

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
    #[serde(default)]
    pub lease_fencing_token: u64,
    #[serde(default)]
    pub lease_owner_uuid: String,
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

pub const CLUSTER_COUNTER_FILE: &str = "hl_portfolio_cluster.counter";

/// Atomically reads and increments the monotonic cluster counter.
/// Fails closed if the counter file exists but is corrupted or unparseable.
pub fn get_and_inc_cluster_counter() -> Result<u64, String> {
    let counter_path = std::path::Path::new(CLUSTER_COUNTER_FILE);
    let current = if counter_path.exists() {
        let content = fs::read_to_string(counter_path)
            .map_err(|e| format!("Failed to read cluster counter file: {}", e))?;
        content
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("Corrupted cluster counter '{}': {}", content.trim(), e))?
    } else {
        0u64
    };

    let next = current
        .checked_add(1)
        .ok_or_else(|| "Cluster counter overflow".to_string())?;

    let tmp_path = format!("{}.tmp.{}", CLUSTER_COUNTER_FILE, std::process::id());
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp counter file: {}", e))?;
        file.write_all(next.to_string().as_bytes())
            .map_err(|e| format!("Failed to write temp counter: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync temp counter: {}", e))?;
    }

    fs::rename(&tmp_path, counter_path)
        .map_err(|e| format!("Failed to atomically rename counter file: {}", e))?;

    if let Some(parent) = counter_path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(next)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterLease {
    pub owner: String,
    #[serde(default = "default_engine_owner")]
    pub engine_owner: String,
    #[serde(default)]
    pub owner_uuid: String,
    pub acquired_at: String,
    pub expires_at: String,
    #[serde(default)]
    pub fencing_token: u64,
}

fn write_lease_atomic(lease: &ClusterLease) -> Result<(), String> {
    let content = serde_json::to_string_pretty(lease)
        .map_err(|e| format!("Failed to serialize cluster lease: {}", e))?;
    let lock_path = std::path::Path::new(CLUSTER_LOCK_FILE);
    let tmp_path = format!("{}.tmp.{}", CLUSTER_LOCK_FILE, std::process::id());
    {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| format!("Failed to open temp lease file: {}", e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| format!("Failed to write temp lease: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to sync temp lease: {}", e))?;
    }
    fs::rename(&tmp_path, lock_path)
        .map_err(|e| format!("Failed to rename lease file: {}", e))?;
    if let Some(parent) = lock_path.parent() {
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
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

/// Atomically attempts to acquire the portfolio cluster lease protected by flock guard
#[allow(dead_code)]
pub fn try_acquire_cluster_lease(symbol: &str, duration_secs: i64) -> bool {
    try_acquire_cluster_lease_for_engine(symbol, "EngineA", duration_secs)
}

#[allow(dead_code)]
pub fn try_acquire_cluster_lease_for_engine(symbol: &str, engine_owner: &str, duration_secs: i64) -> bool {
    let owner_uuid = format!("{}_{}", symbol, chrono::Utc::now().timestamp_micros());
    try_acquire_cluster_lease_for_engine_with_uuid(symbol, engine_owner, &owner_uuid, duration_secs).0
}

pub fn try_acquire_cluster_lease_for_engine_with_uuid(
    symbol: &str,
    engine_owner: &str,
    owner_uuid: &str,
    duration_secs: i64,
) -> (bool, u64) {
    let _guard = match FlockGuard::acquire(CLUSTER_LOCK_GUARD_FILE) {
        Ok(g) => g,
        Err(_) => return (false, 0),
    };
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration_secs);

    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(mut lease) = serde_json::from_str::<ClusterLease>(&content) {
                let exp_dt = chrono::DateTime::parse_from_rfc3339(&lease.expires_at)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .ok();
                let is_unexpired = exp_dt.map(|exp| now < exp).unwrap_or(true);

                if is_unexpired {
                    if lease.owner == symbol {
                        // Same symbol: only the same UUID/process is authorized to refresh!
                        if !lease.owner_uuid.is_empty() && lease.owner_uuid == owner_uuid {
                            lease.engine_owner = engine_owner.to_string();
                            lease.expires_at = expires.to_rfc3339();
                            if write_lease_atomic(&lease).is_ok() {
                                return (true, lease.fencing_token);
                            }
                        } else {
                            eprintln!(
                                "⚠️ [CLUSTER LEASE REJECTED] Active lease for {} held by another UUID ('{}' != '{}'). Rejecting duplicate takeover.",
                                symbol, lease.owner_uuid, owner_uuid
                            );
                            return (false, 0);
                        }
                    }

                    // Different symbol and active: verify if owner has open position
                    let owner_state_file = format!("hl_hmm_state_{}.json", lease.owner);
                    let owner_has_pos = load_state(&owner_state_file)
                        .ok()
                        .and_then(|s| s.get(&lease.owner).cloned())
                        .map(|p| p.is_occupied())
                        .unwrap_or(false);

                    if owner_has_pos || is_unexpired {
                        return (false, 0);
                    }
                }
            }
        }
    }

    // Acquire brand-new lease epoch with incremented monotonic fencing token
    let new_token = match get_and_inc_cluster_counter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("❌ [CLUSTER LEASE ERROR] Counter failure: {}", e);
            return (false, 0);
        }
    };

    let lease = ClusterLease {
        owner: symbol.to_string(),
        engine_owner: engine_owner.to_string(),
        owner_uuid: owner_uuid.to_string(),
        acquired_at: now.to_rfc3339(),
        expires_at: expires.to_rfc3339(),
        fencing_token: new_token,
    };

    if write_lease_atomic(&lease).is_ok() {
        (true, new_token)
    } else {
        (false, 0)
    }
}

/// Refreshes/renews the active cluster lease while position or entry order is open
#[allow(dead_code)]
pub fn refresh_cluster_lease(symbol: &str, duration_secs: i64) -> bool {
    refresh_cluster_lease_authenticated(symbol, None, None, duration_secs)
}

pub fn refresh_cluster_lease_authenticated(
    symbol: &str,
    owner_uuid: Option<&str>,
    fencing_token: Option<u64>,
    duration_secs: i64,
) -> bool {
    let _guard = match FlockGuard::acquire(CLUSTER_LOCK_GUARD_FILE) {
        Ok(g) => g,
        Err(_) => return false,
    };
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration_secs);
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(mut lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner != symbol {
                    return false;
                }
                if let Some(expected_uuid) = owner_uuid {
                    if !lease.owner_uuid.is_empty() && lease.owner_uuid != expected_uuid {
                        return false;
                    }
                }
                if let Some(expected_token) = fencing_token {
                    if lease.fencing_token != expected_token {
                        return false;
                    }
                }
                // PRESERVE fencing_token! Heartbeat extends expiry only without changing epoch.
                lease.expires_at = expires.to_rfc3339();
                return write_lease_atomic(&lease).is_ok();
            }
        }
    }
    false
}

/// Releases the portfolio cluster lease if held by symbol
#[allow(dead_code)]
pub fn release_cluster_lease(symbol: &str) {
    let _ = release_cluster_lease_authenticated(symbol, None, None);
}

pub fn release_cluster_lease_authenticated(
    symbol: &str,
    owner_uuid: Option<&str>,
    fencing_token: Option<u64>,
) -> bool {
    let _guard = match FlockGuard::acquire(CLUSTER_LOCK_GUARD_FILE) {
        Ok(g) => g,
        Err(_) => return false,
    };
    if std::path::Path::new(CLUSTER_LOCK_FILE).exists() {
        if let Ok(content) = fs::read_to_string(CLUSTER_LOCK_FILE) {
            if let Ok(lease) = serde_json::from_str::<ClusterLease>(&content) {
                if lease.owner == symbol {
                    if let Some(expected_uuid) = owner_uuid {
                        if !lease.owner_uuid.is_empty() && lease.owner_uuid != expected_uuid {
                            return false;
                        }
                    }
                    if let Some(expected_token) = fencing_token {
                        if lease.fencing_token != expected_token {
                            return false;
                        }
                    }
                    let _ = fs::remove_file(CLUSTER_LOCK_FILE);
                    return true;
                }
            }
        }
    }
    false
}

/// Returns configured universe of symbols participating in portfolio cluster
pub fn configured_portfolio_symbols() -> Vec<String> {
    if let Ok(val) = std::env::var("HL_PORTFOLIO_SYMBOLS")
        .or_else(|_| std::env::var("PORTFOLIO_SYMBOLS"))
    {
        let syms: Vec<String> = val
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        if !syms.is_empty() {
            return syms;
        }
    }
    vec![
        "BTC".to_string(),
        "ETH".to_string(),
        "SOL".to_string(),
        "AVAX".to_string(),
    ]
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

    // 2. Check position state files across all portfolio symbols
    let all_symbols = configured_portfolio_symbols();
    for sym in &all_symbols {
        if sym == current_symbol {
            continue;
        }
        let state_file = format!("hl_hmm_state_{}.json", sym);
        if let Ok(state) = load_state(&state_file) {
            if let Some(pos) = state.get(sym) {
                if pos.is_occupied() {
                    return Some(sym.to_string());
                }
            }
        }
    }

    // 3. Dynamic scan of any existing state files in directory to prevent orphan leaks
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            if file_name.starts_with("hl_hmm_state_") && file_name.ends_with(".json") {
                let symbol_part = file_name
                    .trim_start_matches("hl_hmm_state_")
                    .trim_end_matches(".json");
                if symbol_part != current_symbol && !all_symbols.iter().any(|s| s == symbol_part) {
                    if let Ok(state) = load_state(&file_name) {
                        if let Some(pos) = state.get(symbol_part) {
                            if pos.is_occupied() {
                                return Some(symbol_part.to_string());
                            }
                        }
                    }
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
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
        assert!(try_acquire_cluster_lease("BTC", 60));
        assert!(!try_acquire_cluster_lease("ETH", 60));
        assert_eq!(has_active_cluster_position("ETH"), Some("BTC".to_string()));
        release_cluster_lease("BTC");
        assert!(try_acquire_cluster_lease("ETH", 60));
        release_cluster_lease("ETH");
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
    }

    #[test]
    fn test_cluster_lease_fencing_token_monotone() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
        let (acq, tok1) = try_acquire_cluster_lease_for_engine_with_uuid("BTC", "EngineA", "uuid_btc", 60);
        assert!(acq);
        let lease1 = get_current_cluster_lease().expect("lease1");
        assert_eq!(lease1.fencing_token, 1);
        assert_eq!(tok1, 1);
        assert_eq!(lease1.owner_uuid, "uuid_btc");

        // Authenticated Refresh PRESERVES fencing token epoch (does NOT change token during heartbeat)
        assert!(refresh_cluster_lease_authenticated("BTC", Some("uuid_btc"), Some(1), 60));
        let lease2 = get_current_cluster_lease().expect("lease2");
        assert_eq!(lease2.fencing_token, 1);

        // Wrong token or UUID rejects refresh
        assert!(!refresh_cluster_lease_authenticated("BTC", Some("wrong_uuid"), Some(1), 60));
        assert!(!refresh_cluster_lease_authenticated("BTC", Some("uuid_btc"), Some(999), 60));

        // Authenticated release succeeds with original token 1
        assert!(release_cluster_lease_authenticated("BTC", Some("uuid_btc"), Some(1)));
        assert!(get_current_cluster_lease().is_none());

        // Re-acquire by ETH increments from persistent counter to token 2 (strictly monotonic epoch)
        let (acq_eth, tok2) = try_acquire_cluster_lease_for_engine_with_uuid("ETH", "EngineB", "uuid_eth", 60);
        assert!(acq_eth);
        let lease3 = get_current_cluster_lease().expect("lease3");
        assert_eq!(lease3.fencing_token, 2);
        assert_eq!(tok2, 2);
        assert_eq!(lease3.engine_owner, "EngineB");

        assert!(release_cluster_lease_authenticated("ETH", Some("uuid_eth"), Some(2)));
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
    }

    #[test]
    fn test_cluster_lease_rejects_duplicate_instance_with_different_uuid() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);

        let (acq1, tok1) = try_acquire_cluster_lease_for_engine_with_uuid("BTC", "EngineA", "process_1_uuid", 60);
        assert!(acq1);
        assert_eq!(tok1, 1);

        // Second instance with SAME symbol "BTC" but DIFFERENT UUID must be REJECTED while lease is valid!
        let (acq2, tok2) = try_acquire_cluster_lease_for_engine_with_uuid("BTC", "EngineA", "process_2_duplicate_uuid", 60);
        assert!(!acq2);
        assert_eq!(tok2, 0);

        // Same UUID is permitted (treated as idempotent re-acquire/refresh)
        let (acq_same, tok_same) = try_acquire_cluster_lease_for_engine_with_uuid("BTC", "EngineA", "process_1_uuid", 60);
        assert!(acq_same);
        assert_eq!(tok_same, 1);

        assert!(release_cluster_lease_authenticated("BTC", Some("process_1_uuid"), Some(1)));
        let _ = fs::remove_file(CLUSTER_LOCK_FILE);
        let _ = fs::remove_file(CLUSTER_LOCK_GUARD_FILE);
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
    }

    #[test]
    fn test_cluster_counter_fails_closed_on_corrupted_file() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);

        // Valid write
        fs::write(CLUSTER_COUNTER_FILE, "corrupted_garbage_text").unwrap();
        let res = get_and_inc_cluster_counter();
        assert!(res.is_err(), "Corrupted counter must return Err, never reset to 0!");

        let _ = fs::remove_file(CLUSTER_COUNTER_FILE);
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
            lease_fencing_token: 0,
            lease_owner_uuid: "".to_string(),
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
