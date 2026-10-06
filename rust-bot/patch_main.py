import re

with open('src/main.rs', 'r') as f:
    content = f.read()

# Add state_file_path to state_clone
content = content.replace(
    'let active_state = Arc::new(Mutex::new(state_map));',
    'let active_state = Arc::new(Mutex::new(state_map));\n    let state_file_path = config.state_file_path.clone();'
)

# Load state correctly
content = content.replace(
    'let state_map: StateMap = load_state().unwrap_or_default();',
    'let state_map: StateMap = load_state(&config.state_file_path).unwrap_or_default();'
)

# Replace "BTCUSDC"
content = content.replace('"BTCUSDC"', '&config_trainer_symbol')

# Set up variables for cold path
content = content.replace(
    'let config_trainer_path = config.hmm_config_path.clone();',
    'let config_trainer_path = config.hmm_config_path.clone();\n    let config_trainer_symbol = config.trading_symbol.clone();'
)

# Set up variables for hot path
content = content.replace(
    'if let Ok(klines) = binance.fetch_ohlcv(&config_trainer_symbol, "1m", 480).await {',
    'let trading_symbol = config.trading_symbol.clone();\n    if let Ok(klines) = binance.fetch_ohlcv(&trading_symbol, "1m", 480).await {'
)

# Fix ws url
content = content.replace(
    'let ws_url = "wss://stream.binance.com:9443/ws/btcusdc@kline_1m";',
    'let ws_url = format!("wss://stream.binance.com:9443/ws/{}@kline_1m", trading_symbol.to_lowercase());'
)
content = content.replace('connect_async(ws_url)', 'connect_async(&ws_url)')

# Set up cloned vars for spawn
content = content.replace(
    'let discord_clone = discord_url.clone();',
    'let discord_clone = discord_url.clone();\n                            let symbol_clone = trading_symbol.clone();\n                            let state_path_clone = state_file_path.clone();'
)

# Replace print BTC Price
content = content.replace(
    'BTC Price:',
    '{} Price:", symbol_clone'
)
content = content.replace(
    'println!("💓 [HOT-PATH] {} Price:", symbol_clone ${:.2} | Target Buy: ${:.2} | Regime: {} | Drift: ${:.2}/m", current_price, thresholds.entry_long, regime_str, slope);',
    'println!("💓 [HOT-PATH] {} Price: ${:.2} | Target Buy: ${:.2} | Regime: {} | Drift: ${:.2}/m", symbol_clone, current_price, thresholds.entry_long, regime_str, slope);'
)
# A more robust regex for the print line
content = re.sub(
    r'println!\("💓 \[HOT-PATH\] BTC Price: \$\{\:.2\} \| Target Buy: \$\{\:.2\} \| Regime: \{\} \| Drift: \$\{\:.2\}/m", current_price, thresholds.entry_long, regime_str, slope\);',
    r'println!("💓 [HOT-PATH] {} Price: ${:.2} | Target Buy: ${:.2} | Regime: {} | Drift: ${:.2}/m", symbol_clone, current_price, thresholds.entry_long, regime_str, slope);',
    content
)


# Replace "BTC/USDC" with &symbol_clone
content = content.replace('"BTC/USDC"', '&symbol_clone')

# Fix save_state
content = content.replace(
    'let _ = crate::state::save_state(&state_lock);',
    'let _ = crate::state::save_state(&state_path_clone, &state_lock);'
)

# Fix order functions
content = content.replace(
    'emergency_flatten(&binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;',
    'emergency_flatten(&state_path_clone, &binance_clone, &mut state_lock, &symbol_clone, discord_clone.clone()).await;'
)
content = content.replace(
    'place_ou_limit_buy(\n                                                    &binance_clone, \n                                                    &mut state_lock, \n                                                    &symbol_clone, ',
    'place_ou_limit_buy(\n                                                    &state_path_clone,\n                                                    &binance_clone, \n                                                    &mut state_lock, \n                                                    &symbol_clone, '
)

with open('src/main.rs', 'w') as f:
    f.write(content)

