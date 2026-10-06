use dotenvy::dotenv;
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub binance_api_key: String,
    pub binance_secret_key: String,
    pub gemini_api_key: String,
    pub alpaca_api_key: Option<String>,
    pub alpaca_secret_key: Option<String>,
    pub discord_webhook_url: Option<String>,
    pub discord_bot_token: Option<String>,
    pub trading_symbol: String,
    pub hmm_config_path: String,
    pub state_file_path: String,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok(); // Ignore if .env doesn't exist
        
        let symbol = env::var("TRADING_SYMBOL").unwrap_or_else(|_| "BTCUSDC".to_string());
        
        // Dynamically name state and weights based on the symbol to prevent Docker collisions
        let hmm_path = env::var("HMM_CONFIG_PATH").unwrap_or_else(|_| format!("hmm_weights_{}.json", symbol));
        let state_path = format!("binance_hmm_state_{}.json", symbol);

        Config {
            binance_api_key: env::var("BINANCE_API_KEY").expect("BINANCE_API_KEY must be set"),
            binance_secret_key: env::var("BINANCE_SECRET_KEY").expect("BINANCE_SECRET_KEY must be set"),
            gemini_api_key: env::var("GEMINI_API_KEY").unwrap_or_default(),
            alpaca_api_key: env::var("ALPACA_API_KEY").ok(),
            alpaca_secret_key: env::var("ALPACA_SECRET_KEY").ok(),
            discord_webhook_url: env::var("DISCORD_WEBHOOK_URL").ok(),
            discord_bot_token: env::var("DISCORD_BOT_TOKEN").ok(),
            trading_symbol: symbol,
            hmm_config_path: hmm_path,
            state_file_path: state_path,
        }
    }
}
