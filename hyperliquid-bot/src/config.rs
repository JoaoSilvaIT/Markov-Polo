use dotenvy::dotenv;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub hl_private_key: String,
    pub hl_wallet_address: Option<String>,
    pub hl_is_mainnet: bool,
    pub trading_symbol: String,
    pub position_size_usdc: f64,
    pub hl_leverage: u32,
    pub hl_is_cross: bool,
    pub discord_webhook_url: Option<String>,
    pub hmm_config_path: String,
    pub state_file_path: String,
    pub trend_engine_mode: String,
    pub trend_risk_fraction: f64,
    pub donchian_period: usize,
    pub chandelier_atr_mult: f64,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok(); // Load .env if present

        let symbol = env::var("HL_TRADING_SYMBOL")
            .or_else(|_| env::var("TRADING_SYMBOL"))
            .unwrap_or_else(|_| "BTC".to_string())
            // Normalize symbol: remove USDC or /USDC if provided
            .replace("USDC", "")
            .replace("USDT", "")
            .replace("/", "")
            .to_uppercase();

        let is_mainnet = env::var("HL_IS_MAINNET")
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);

        let size_usdc = env::var("POSITION_SIZE_USDC")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(25.0);

        let leverage = env::var("HL_LEVERAGE")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(3);

        let is_cross = env::var("HL_IS_CROSS")
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);

        let hmm_path = env::var("HMM_CONFIG_PATH")
            .unwrap_or_else(|_| format!("hl_hmm_weights_{}.json", symbol));
        let state_path = format!("hl_hmm_state_{}.json", symbol);

        let trend_mode = env::var("HL_TREND_ENGINE_MODE")
            .unwrap_or_else(|_| "shadow".to_string())
            .to_lowercase();
        let trend_risk = env::var("HL_TREND_RISK_FRACTION")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.25);
        let donchian_period = env::var("HL_DONCHIAN_PERIOD")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(20);
        let chandelier_mult = env::var("HL_CHANDELIER_ATR_MULT")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(3.0);

        Config {
            hl_private_key: env::var("HL_PRIVATE_KEY").unwrap_or_else(|_| {
                // Fallback dummy key for testing without panicking during offline tests
                "0x0000000000000000000000000000000000000000000000000000000000000001".to_string()
            }),
            hl_wallet_address: env::var("HL_WALLET_ADDRESS").ok(),
            hl_is_mainnet: is_mainnet,
            trading_symbol: symbol,
            position_size_usdc: size_usdc,
            hl_leverage: leverage,
            hl_is_cross: is_cross,
            discord_webhook_url: env::var("DISCORD_WEBHOOK_URL").ok(),
            hmm_config_path: hmm_path,
            state_file_path: state_path,
            trend_engine_mode: trend_mode,
            trend_risk_fraction: trend_risk,
            donchian_period,
            chandelier_atr_mult: chandelier_mult,
        }
    }
}
