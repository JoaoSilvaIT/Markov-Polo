use crate::hyperliquid::{
    reconcile_unified_account_equity, ClearinghouseDexEquitySummary,
};
use crate::state::{configured_portfolio_symbols, resolve_lock_guard_path};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use futures_util::{SinkExt, StreamExt};
use hyperliquid_rust_sdk::{Message, Subscription};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc::UnboundedSender, RwLock};
use tokio_tungstenite::{accept_async, connect_async, tungstenite::protocol::Message as WsMessage};

pub const DEFAULT_MUX_BIND_ADDR: &str = "0.0.0.0:9001";
pub const SHARED_ACCOUNT_STATE_FILE: &str = "shared_account_state.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedAccountState {
    pub updated_at_ms: u64,
    pub is_mainnet: bool,
    pub wallet: String,
    pub reconciled_equity_usdc: f64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SubKey {
    L2Book(String),
    ActiveAssetCtx(String),
    Candle(String, String),
    OrderUpdates,
    UserFills,
}

#[derive(Debug, Clone)]
struct MuxSnapshotEntry {
    raw_json: String,
    updated_at_ms: u64,
}

struct MuxHub {
    tx: broadcast::Sender<Arc<(SubKey, String)>>,
    snapshots: RwLock<HashMap<SubKey, MuxSnapshotEntry>>,
}

impl MuxHub {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(8192);
        Self {
            tx,
            snapshots: RwLock::new(HashMap::new()),
        }
    }

    async fn clear_snapshots_by_kind(&self, clear_market_data: bool, clear_candles: bool) {
        let mut guard = self.snapshots.write().await;
        guard.retain(|k, _| match k {
            SubKey::L2Book(_) | SubKey::ActiveAssetCtx(_) => !clear_market_data,
            SubKey::Candle(_, _) => !clear_candles,
            SubKey::OrderUpdates | SubKey::UserFills => true,
        });
    }

    async fn ingest_upstream_text(&self, txt: String) {
        let Some((key, cacheable)) = extract_frame_sub_key(&txt) else {
            return;
        };
        if cacheable {
            let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
            let mut guard = self.snapshots.write().await;
            guard.insert(
                key.clone(),
                MuxSnapshotEntry {
                    raw_json: txt.clone(),
                    updated_at_ms: now_ms,
                },
            );
        }
        let _ = self.tx.send(Arc::new((key, txt)));
    }
}

/// Parses a client subscription JSON object (`{"type": "...", ...}`) into a `SubKey`.
pub fn parse_subscription_value(sub_val: &serde_json::Value) -> Option<SubKey> {
    let sub_type = sub_val.get("type")?.as_str()?;
    match sub_type {
        "l2Book" => {
            let coin = sub_val.get("coin")?.as_str()?;
            Some(SubKey::L2Book(coin.to_string()))
        }
        "activeAssetCtx" => {
            let coin = sub_val.get("coin")?.as_str()?;
            Some(SubKey::ActiveAssetCtx(coin.to_string()))
        }
        "candle" => {
            let coin = sub_val.get("coin")?.as_str()?;
            let interval = sub_val
                .get("interval")
                .and_then(|v| v.as_str())
                .unwrap_or("1m");
            Some(SubKey::Candle(coin.to_string(), interval.to_string()))
        }
        "orderUpdates" => Some(SubKey::OrderUpdates),
        "userFills" => Some(SubKey::UserFills),
        _ => None,
    }
}

/// Extracts `(SubKey, is_cacheable_snapshot)` from a raw Hyperliquid WebSocket text frame.
pub fn extract_frame_sub_key(raw_json: &str) -> Option<(SubKey, bool)> {
    let val: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    let channel = val.get("channel")?.as_str()?;
    let data = val.get("data");
    match channel {
        "l2Book" => {
            let coin = data?.get("coin")?.as_str()?;
            Some((SubKey::L2Book(coin.to_string()), true))
        }
        "activeAssetCtx" => {
            let coin = data?.get("coin")?.as_str()?;
            Some((SubKey::ActiveAssetCtx(coin.to_string()), true))
        }
        "candle" => {
            let d = data?;
            let coin = d
                .get("s")
                .or_else(|| d.get("coin"))
                .and_then(|v| v.as_str())?;
            let interval = d.get("i").and_then(|v| v.as_str()).unwrap_or("1m");
            Some((
                SubKey::Candle(coin.to_string(), interval.to_string()),
                true,
            ))
        }
        "orderUpdates" => Some((SubKey::OrderUpdates, false)),
        "userFills" => Some((SubKey::UserFills, false)),
        _ => None,
    }
}

/// Reads the shared reconciled account equity written by `hl_mux` if fresh (`<= max_age_ms`).
/// Returns `None` if absent, stale, disabled, or mismatched, causing callers to fall back to direct REST.
pub fn try_read_shared_equity(address: H160, is_mainnet: bool, max_age_ms: u64) -> Option<f64> {
    if std::env::var("HL_MUX_DISABLE_STATE_CACHE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return None;
    }
    let path = resolve_lock_guard_path(SHARED_ACCOUNT_STATE_FILE);
    try_read_shared_equity_from_path(&path, address, is_mainnet, max_age_ms)
}

pub fn try_read_shared_equity_from_path(
    path: &std::path::Path,
    address: H160,
    is_mainnet: bool,
    max_age_ms: u64,
) -> Option<f64> {
    let content = std::fs::read_to_string(path).ok()?;
    let parsed: SharedAccountState = serde_json::from_str(&content).ok()?;
    if parsed.is_mainnet != is_mainnet {
        return None;
    }
    let expected_wallet = format!("{:?}", address);
    if !parsed.wallet.eq_ignore_ascii_case(&expected_wallet) {
        return None;
    }
    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    if now_ms.saturating_sub(parsed.updated_at_ms) > max_age_ms {
        return None;
    }
    if parsed.reconciled_equity_usdc.is_finite() && parsed.reconciled_equity_usdc > 0.0 {
        Some(parsed.reconciled_equity_usdc)
    } else {
        None
    }
}

fn write_shared_account_state_to_path(
    target: &std::path::Path,
    state: &SharedAccountState,
) -> std::io::Result<()> {
    let tmp = target.with_extension("json.tmp");
    let json_bytes = serde_json::to_vec(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, json_bytes)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}

fn write_shared_account_state_atomic(state: &SharedAccountState) -> std::io::Result<()> {
    let target = resolve_lock_guard_path(SHARED_ACCOUNT_STATE_FILE);
    write_shared_account_state_to_path(&target, state)
}

async fn fetch_dex_summary_via_client(
    client: &reqwest::Client,
    info_url: &str,
    address: H160,
    dex: &str,
) -> Result<ClearinghouseDexEquitySummary, String> {
    let payload = if dex.is_empty() {
        serde_json::json!({
            "type": "clearinghouseState",
            "user": format!("{:?}", address),
        })
    } else {
        serde_json::json!({
            "type": "clearinghouseState",
            "user": format!("{:?}", address),
            "dex": dex,
        })
    };
    let resp = tokio::time::timeout(
        Duration::from_secs(6),
        client.post(info_url).json(&payload).send(),
    )
    .await
    .map_err(|_| format!("Timeout clearinghouseState({})", dex))?
    .map_err(|e| format!("Request clearinghouseState({}): {}", dex, e))?;

    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {} clearinghouseState({})",
            resp.status(),
            dex
        ));
    }

    let data = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("JSON clearinghouseState({}): {}", dex, e))?;

    let ms = data.get("marginSummary");
    let account_value = ms
        .and_then(|m| m.get("accountValue"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let total_raw_usd = ms
        .and_then(|m| m.get("totalRawUsd"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let total_margin_used = ms
        .and_then(|m| m.get("totalMarginUsed"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let unrealized_pnl = data
        .get("assetPositions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ap| {
                    ap.get("position")
                        .and_then(|p| p.get("unrealizedPnl"))
                        .and_then(|u| u.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                })
                .sum()
        })
        .unwrap_or(0.0);

    Ok(ClearinghouseDexEquitySummary {
        account_value,
        total_raw_usd,
        total_margin_used,
        unrealized_pnl,
    })
}

async fn fetch_spot_usdc_via_client(
    client: &reqwest::Client,
    info_url: &str,
    address: H160,
) -> Result<(f64, f64), String> {
    let payload = serde_json::json!({
        "type": "spotClearinghouseState",
        "user": format!("{:?}", address),
    });
    let resp = tokio::time::timeout(
        Duration::from_secs(6),
        client.post(info_url).json(&payload).send(),
    )
    .await
    .map_err(|_| "Timeout spotClearinghouseState".to_string())?
    .map_err(|e| format!("Request spotClearinghouseState: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {} spotClearinghouseState", resp.status()));
    }

    let data = resp
        .json::<serde_json::Value>()
        .await
        .map_err(|e| format!("JSON spotClearinghouseState: {}", e))?;

    let mut total = 0.0;
    let mut hold = 0.0;
    if let Some(balances) = data.get("balances").and_then(|b| b.as_array()) {
        for b in balances {
            if b.get("coin").and_then(|c| c.as_str()) == Some("USDC") {
                total += b
                    .get("total")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0)
                    .max(0.0);
                hold += b
                    .get("hold")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0)
                    .max(0.0);
            }
        }
    }
    Ok((total, hold))
}

async fn run_account_state_poller(address: H160, is_mainnet: bool) {
    let info_url = if is_mainnet {
        "https://api.hyperliquid.xyz/info"
    } else {
        "https://api.hyperliquid-testnet.xyz/info"
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .unwrap_or_default();
    let wallet_str = format!("{:?}", address);
    let mut err_streak = 0u32;
    let mut write_err_streak = 0u32;
    let mut logged_ready = false;

    loop {
        let (native_res, spot_res, xyz_res) = tokio::join!(
            fetch_dex_summary_via_client(&client, info_url, address, ""),
            fetch_spot_usdc_via_client(&client, info_url, address),
            fetch_dex_summary_via_client(&client, info_url, address, "xyz"),
        );

        match (native_res, spot_res, xyz_res) {
            (Ok(native_sum), Ok((spot_total, spot_hold)), Ok(xyz_sum)) => {
                let eq = reconcile_unified_account_equity(
                    spot_total,
                    spot_hold,
                    native_sum,
                    xyz_sum,
                );
                if eq.is_finite() && eq > 0.0 {
                    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                    let state = SharedAccountState {
                        updated_at_ms: now_ms,
                        is_mainnet,
                        wallet: wallet_str.clone(),
                        reconciled_equity_usdc: eq,
                    };
                    if let Err(e) = write_shared_account_state_atomic(&state) {
                        write_err_streak = write_err_streak.saturating_add(1);
                        if write_err_streak == 1 || write_err_streak % 20 == 0 {
                            eprintln!(
                                "⚠️ [HL_MUX POLLER] Failed writing shared state at {}: {} (streak {})",
                                resolve_lock_guard_path(SHARED_ACCOUNT_STATE_FILE).display(),
                                e,
                                write_err_streak
                            );
                        }
                    } else {
                        if !logged_ready || err_streak > 0 || write_err_streak > 0 {
                            println!(
                                "✅ [HL_MUX POLLER] Shared account state active at {} (equity: ${:.2} USDC).",
                                resolve_lock_guard_path(SHARED_ACCOUNT_STATE_FILE).display(),
                                eq
                            );
                            logged_ready = true;
                        }
                        write_err_streak = 0;
                    }
                    err_streak = 0;
                }
            }
            (r1, r2, r3) => {
                err_streak = err_streak.saturating_add(1);
                if err_streak == 1 || err_streak % 10 == 0 {
                    eprintln!(
                        "⚠️ [HL_MUX POLLER] Account poll error (streak {}): native={:?}, spot={:?}, xyz={:?}",
                        err_streak,
                        r1.err(),
                        r2.err(),
                        r3.err()
                    );
                }
            }
        }

        let sleep_ms = if err_streak >= 3 { 4000 } else { 1500 };
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
    }
}

async fn run_upstream_ws_loop(
    hub: Arc<MuxHub>,
    ws_url: String,
    subscriptions: Vec<serde_json::Value>,
    label: &'static str,
    clear_market_data: bool,
    clear_candles: bool,
    read_timeout_secs: u64,
) {
    let mut backoff_secs = 1u64;

    loop {
        match tokio::time::timeout(Duration::from_secs(12), connect_async(&ws_url)).await {
            Ok(Ok((ws_stream, _))) => {
                backoff_secs = 1;
                println!(
                    "📡 [HL_MUX {}] Connected to upstream {} ({} subscriptions).",
                    label,
                    ws_url,
                    subscriptions.len()
                );
                let (mut write, mut read) = ws_stream.split();

                let mut sub_ok = true;
                for sub in &subscriptions {
                    let payload = serde_json::json!({
                        "method": "subscribe",
                        "subscription": sub,
                    });
                    if let Err(e) = write.send(WsMessage::Text(payload.to_string())).await {
                        eprintln!("❌ [HL_MUX {}] Failed sending subscribe: {}", label, e);
                        sub_ok = false;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }

                if sub_ok {
                    let mut ping_interval = tokio::time::interval(Duration::from_secs(45));
                    ping_interval.tick().await; // consume initial immediate tick

                    loop {
                        tokio::select! {
                            _ = ping_interval.tick() => {
                                let ping_payload = serde_json::json!({ "method": "ping" }).to_string();
                                if let Err(e) = write.send(WsMessage::Text(ping_payload)).await {
                                    eprintln!("⚠️ [HL_MUX {}] Ping send failed: {}", label, e);
                                    break;
                                }
                            }
                            frame_res = tokio::time::timeout(Duration::from_secs(read_timeout_secs), read.next()) => {
                                match frame_res {
                                    Ok(Some(Ok(WsMessage::Text(txt)))) => {
                                        hub.ingest_upstream_text(txt).await;
                                    }
                                    Ok(Some(Ok(WsMessage::Ping(p)))) => {
                                        if write.send(WsMessage::Pong(p)).await.is_err() {
                                            break;
                                        }
                                    }
                                    Ok(Some(Ok(WsMessage::Pong(_)))) => {}
                                    Ok(Some(Ok(WsMessage::Close(c)))) => {
                                        eprintln!("⚠️ [HL_MUX {}] Upstream closed: {:?}", label, c);
                                        break;
                                    }
                                    Ok(Some(Ok(_))) => {}
                                    Ok(Some(Err(e))) => {
                                        eprintln!("⚠️ [HL_MUX {}] Upstream read error: {}", label, e);
                                        break;
                                    }
                                    Ok(None) => {
                                        eprintln!("⚠️ [HL_MUX {}] Upstream EOF reached.", label);
                                        break;
                                    }
                                    Err(_) => {
                                        eprintln!("⚠️ [HL_MUX {}] Upstream watchdog timed out ({}s). Reconnecting...", label, read_timeout_secs);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                eprintln!(
                    "❌ [HL_MUX {}] Upstream connect error: {}. Retrying in {}s...",
                    label, e, backoff_secs
                );
            }
            Err(_) => {
                eprintln!(
                    "❌ [HL_MUX {}] Upstream connect timed out. Retrying in {}s...",
                    label, backoff_secs
                );
            }
        }

        hub.clear_snapshots_by_kind(clear_market_data, clear_candles)
            .await;
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(15);
    }
}

async fn handle_mux_client(stream: TcpStream, hub: Arc<MuxHub>) {
    let ws_stream = match accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let (mut write, mut read) = ws_stream.split();
    let mut bcast_rx = hub.tx.subscribe();
    let mut client_subs: HashSet<SubKey> = HashSet::new();

    loop {
        tokio::select! {
            inbound = read.next() => {
                match inbound {
                    Some(Ok(WsMessage::Text(txt))) => {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&txt) {
                            match val.get("method").and_then(|v| v.as_str()) {
                                Some("ping") => {
                                    let pong = serde_json::json!({ "channel": "pong" }).to_string();
                                    if write.send(WsMessage::Text(pong)).await.is_err() {
                                        break;
                                    }
                                }
                                Some("subscribe") => {
                                    if let Some(sub_val) = val.get("subscription") {
                                        if let Some(key) = parse_subscription_value(sub_val) {
                                            client_subs.insert(key.clone());
                                            let ack = serde_json::json!({
                                                "channel": "subscriptionResponse",
                                                "data": {
                                                    "method": "subscribe",
                                                    "subscription": sub_val,
                                                }
                                            }).to_string();
                                            if write.send(WsMessage::Text(ack)).await.is_err() {
                                                break;
                                            }
                                            let snap_opt = {
                                                let guard = hub.snapshots.read().await;
                                                guard.get(&key).cloned()
                                            };
                                            if let Some(snap) = snap_opt {
                                                let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                                                let max_snap_age = match key {
                                                    SubKey::L2Book(_) => 15_000,
                                                    SubKey::ActiveAssetCtx(_) | SubKey::Candle(_, _) => 65_000,
                                                    _ => 0,
                                                };
                                                if now_ms.saturating_sub(snap.updated_at_ms) <= max_snap_age
                                                    && write.send(WsMessage::Text(snap.raw_json)).await.is_err()
                                                {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                Some("unsubscribe") => {
                                    if let Some(sub_val) = val.get("subscription") {
                                        if let Some(key) = parse_subscription_value(sub_val) {
                                            client_subs.remove(&key);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        if write.send(WsMessage::Pong(p)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
            bcast_res = bcast_rx.recv() => {
                match bcast_res {
                    Ok(frame) => {
                        if client_subs.contains(&frame.0)
                            && write.send(WsMessage::Text(frame.1.clone())).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        eprintln!("⚠️ [HL_MUX] Slow client lagged by {} frames; continuing with latest stream.", skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Runs the `hl_mux` Local Rust WS/REST Multiplexer Sidecar (`hyperliquid-bot --mux`).
/// Maintains strictly 2 upstream WebSocket connections to Hyperliquid across all 15 coins + wallet events
/// and polls reconciled Unified Account equity every 1.5s into `/var/run/hl_cluster_lock/shared_account_state.json`.
pub async fn run_mux_server() {
    dotenvy::dotenv().ok();

    let is_mainnet = std::env::var("HL_IS_MAINNET")
        .map(|v| v.to_lowercase() != "false" && v != "0")
        .unwrap_or(true);
    let bind_addr = std::env::var("HL_MUX_BIND")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MUX_BIND_ADDR.to_string());
    let upstream_ws_url = if is_mainnet {
        "wss://api.hyperliquid.xyz/ws".to_string()
    } else {
        "wss://api.hyperliquid-testnet.xyz/ws".to_string()
    };

    let wallet_addr: H160 = std::env::var("HL_WALLET_ADDRESS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|s| H160::from_str(&s).ok())
        .or_else(|| {
            std::env::var("HL_PRIVATE_KEY")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .and_then(|pk| pk.parse::<LocalWallet>().ok())
                .map(|w| w.address())
        })
        .expect("hl_mux requires valid HL_WALLET_ADDRESS or HL_PRIVATE_KEY");

    let symbols = configured_portfolio_symbols();
    let user_hex = format!("{:?}", wallet_addr);

    println!(
        "🚀 [HL_MUX START] Local Rust WS/REST Multiplexer Sidecar listening on ws://{}/ws | Mainnet: {} | Wallet: {} | Coins ({}): {:?}",
        bind_addr, is_mainnet, user_hex, symbols.len(), symbols
    );

    let mut market_subs = Vec::with_capacity(symbols.len() * 2);
    let mut candle_user_subs = Vec::with_capacity(symbols.len() + 2);
    for sym in &symbols {
        market_subs.push(serde_json::json!({ "type": "l2Book", "coin": sym }));
        market_subs.push(serde_json::json!({ "type": "activeAssetCtx", "coin": sym }));
        candle_user_subs.push(serde_json::json!({ "type": "candle", "coin": sym, "interval": "1m" }));
    }
    candle_user_subs.push(serde_json::json!({ "type": "orderUpdates", "user": user_hex }));
    candle_user_subs.push(serde_json::json!({ "type": "userFills", "user": user_hex }));

    let hub = Arc::new(MuxHub::new());

    // Socket 1: Market Data L2 + AssetCtx
    tokio::spawn(run_upstream_ws_loop(
        hub.clone(),
        upstream_ws_url.clone(),
        market_subs,
        "SOCKET-1(L2+CTX)",
        true,
        false,
        60,
    ));

    // Socket 2: Candles + User Events
    tokio::spawn(run_upstream_ws_loop(
        hub.clone(),
        upstream_ws_url,
        candle_user_subs,
        "SOCKET-2(CANDLE+USER)",
        false,
        true,
        90,
    ));

    // Shared Account State REST Poller
    tokio::spawn(run_account_state_poller(wallet_addr, is_mainnet));

    let listener = TcpListener::bind(&bind_addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind hl_mux listener on {}: {}", bind_addr, e));
    println!("✅ [HL_MUX READY] Multiplexer active on ws://{}/ws (2 upstream WS total).", bind_addr);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let _ = stream.set_nodelay(true);
                tokio::spawn(handle_mux_client(stream, hub.clone()));
            }
            Err(e) => {
                eprintln!("⚠️ [HL_MUX ACCEPT ERROR] {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Connects to `ws_endpoint` (e.g. `ws://hl_mux:9001/ws`), sends all `subscriptions`,
/// and pumps parsed `hyperliquid_rust_sdk::Message` frames into `sender` until disconnect/timeout.
pub async fn stream_via_ws_endpoint(
    ws_endpoint: &str,
    subscriptions: &[Subscription],
    sender: UnboundedSender<Message>,
    read_timeout_secs: u64,
    stream_label: &str,
) -> Result<(), String> {
    let (ws_stream, _) = tokio::time::timeout(Duration::from_secs(6), connect_async(ws_endpoint))
        .await
        .map_err(|_| format!("Timed out connecting {} to {}", stream_label, ws_endpoint))?
        .map_err(|e| format!("Failed connecting {} to {}: {}", stream_label, ws_endpoint, e))?;

    let (mut write, mut read) = ws_stream.split();

    for sub in subscriptions {
        let sub_val = serde_json::to_value(sub)
            .map_err(|e| format!("Failed serializing subscription: {}", e))?;
        let req = serde_json::json!({
            "method": "subscribe",
            "subscription": sub_val,
        })
        .to_string();
        write
            .send(WsMessage::Text(req))
            .await
            .map_err(|e| format!("Failed sending subscription to {}: {}", ws_endpoint, e))?;
    }

    let mut ping_interval = tokio::time::interval(Duration::from_secs(30));
    ping_interval.tick().await;

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                let ping = serde_json::json!({ "method": "ping" }).to_string();
                if let Err(e) = write.send(WsMessage::Text(ping)).await {
                    return Err(format!("Ping error on {}: {}", ws_endpoint, e));
                }
            }
            frame_res = tokio::time::timeout(Duration::from_secs(read_timeout_secs), read.next()) => {
                match frame_res {
                    Ok(Some(Ok(WsMessage::Text(txt)))) => {
                        match serde_json::from_str::<Message>(&txt) {
                            Ok(Message::Pong) | Ok(Message::SubscriptionResponse) => {}
                            Ok(msg) => {
                                if sender.send(msg).is_err() {
                                    return Ok(());
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    Ok(Some(Ok(WsMessage::Ping(p)))) => {
                        if let Err(e) = write.send(WsMessage::Pong(p)).await {
                            return Err(format!("Pong write error: {}", e));
                        }
                    }
                    Ok(Some(Ok(WsMessage::Close(c)))) => {
                        return Err(format!("Mux closed connection: {:?}", c));
                    }
                    Ok(Some(Ok(_))) => {}
                    Ok(Some(Err(e))) => {
                        return Err(format!("Mux read error: {}", e));
                    }
                    Ok(None) => {
                        return Err("Mux stream EOF".to_string());
                    }
                    Err(_) => {
                        return Err(format!("{} watchdog timeout ({}s)", stream_label, read_timeout_secs));
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mux_sub_key_extraction_and_routing() {
        let sub_l2 = serde_json::json!({"type": "l2Book", "coin": "xyz:GOLD"});
        assert_eq!(
            parse_subscription_value(&sub_l2),
            Some(SubKey::L2Book("xyz:GOLD".to_string()))
        );

        let sub_candle = serde_json::json!({"type": "candle", "coin": "BTC", "interval": "1m"});
        assert_eq!(
            parse_subscription_value(&sub_candle),
            Some(SubKey::Candle("BTC".to_string(), "1m".to_string()))
        );

        let raw_l2 = r#"{"channel":"l2Book","data":{"coin":"xyz:GOLD","time":12345,"levels":[[],[]]}}"#;
        assert_eq!(
            extract_frame_sub_key(raw_l2),
            Some((SubKey::L2Book("xyz:GOLD".to_string()), true))
        );

        let raw_candle = r#"{"channel":"candle","data":{"T":1,"c":"74000","h":"74100","i":"1m","l":"73900","n":10,"o":"73950","s":"BTC","t":0,"v":"1.5"}}"#;
        assert_eq!(
            extract_frame_sub_key(raw_candle),
            Some((SubKey::Candle("BTC".to_string(), "1m".to_string()), true))
        );

        let raw_upd = r#"{"channel":"orderUpdates","data":[]}"#;
        assert_eq!(
            extract_frame_sub_key(raw_upd),
            Some((SubKey::OrderUpdates, false))
        );

        // Verify hyperliquid_rust_sdk::Message deserializes cleanly from mux frames
        let parsed_candle: Message = serde_json::from_str(raw_candle).unwrap();
        assert!(matches!(parsed_candle, Message::Candle(_)));
        let parsed_l2: Message = serde_json::from_str(raw_l2).unwrap();
        assert!(matches!(parsed_l2, Message::L2Book(_)));
    }

    #[tokio::test]
    async fn test_mux_server_fanout_and_client_subscription_loopback() {
        let hub = Arc::new(MuxHub::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ws_url = format!("ws://{}/ws", addr);

        let hub_srv = hub.clone();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                handle_mux_client(stream, hub_srv).await;
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let subs = vec![
            Subscription::L2Book {
                coin: "BTC".to_string(),
            },
            Subscription::Candle {
                coin: "BTC".to_string(),
                interval: "1m".to_string(),
            },
        ];
        let ws_url_c = ws_url.clone();
        tokio::spawn(async move {
            let _ = stream_via_ws_endpoint(&ws_url_c, &subs, tx, 10, "TEST-MUX").await;
        });

        // Give client 80ms to connect and subscribe
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Publish ETH (should NOT be routed) and BTC (MUST be routed)
        hub.ingest_upstream_text(
            r#"{"channel":"l2Book","data":{"coin":"ETH","time":100,"levels":[[{"px":"2000.0","sz":"1.0","n":1}],[{"px":"2001.0","sz":"1.0","n":1}]]}}"#.to_string(),
        ).await;
        hub.ingest_upstream_text(
            r#"{"channel":"l2Book","data":{"coin":"BTC","time":101,"levels":[[{"px":"74000.0","sz":"1.0","n":1}],[{"px":"74001.0","sz":"1.0","n":1}]]}}"#.to_string(),
        ).await;
        hub.ingest_upstream_text(
            r#"{"channel":"candle","data":{"T":1,"c":"74000","h":"74100","i":"1m","l":"73900","n":10,"o":"73950","s":"BTC","t":0,"v":"1.5"}}"#.to_string(),
        ).await;

        let m1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match m1 {
            Message::L2Book(b) => assert_eq!(b.data.coin, "BTC"),
            other => panic!("Expected BTC L2Book, got {:?}", other),
        }

        let m2 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match m2 {
            Message::Candle(c) => assert_eq!(c.data.coin, "BTC"),
            other => panic!("Expected BTC Candle, got {:?}", other),
        }
    }

    #[test]
    fn test_shared_account_state_atomic_read_write_and_staleness() {
        let addr = H160::from_str("0xc2c536344f321b0f2c8ecec08c507b552c0f3e3b").unwrap();
        let other_addr = H160::from_str("0x1111111111111111111111111111111111111111").unwrap();
        let test_path = std::env::temp_dir().join("hl_mux_test_shared_state.json");
        let _ = std::fs::remove_file(&test_path);

        let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let fresh_state = SharedAccountState {
            updated_at_ms: now_ms,
            is_mainnet: true,
            wallet: format!("{:?}", addr),
            reconciled_equity_usdc: 559.04,
        };
        write_shared_account_state_to_path(&test_path, &fresh_state).unwrap();

        assert_eq!(
            try_read_shared_equity_from_path(&test_path, addr, true, 5_000),
            Some(559.04)
        );
        // Reject wrong network or wrong wallet
        assert_eq!(
            try_read_shared_equity_from_path(&test_path, addr, false, 5_000),
            None
        );
        assert_eq!(
            try_read_shared_equity_from_path(&test_path, other_addr, true, 5_000),
            None
        );

        // Reject stale state (> 5s)
        let stale_state = SharedAccountState {
            updated_at_ms: now_ms.saturating_sub(10_000),
            is_mainnet: true,
            wallet: format!("{:?}", addr),
            reconciled_equity_usdc: 559.04,
        };
        write_shared_account_state_to_path(&test_path, &stale_state).unwrap();
        assert_eq!(
            try_read_shared_equity_from_path(&test_path, addr, true, 5_000),
            None
        );

        let _ = std::fs::remove_file(&test_path);
    }
}
