use reqwest::Client;
use serde_json::Value;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

pub type BinanceResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct BinanceClient {
    pub api_key: String,
    pub secret: String,
    pub client: Client,
}

impl BinanceClient {
    pub fn new(api_key: String, secret: String) -> Self {
        Self {
            api_key,
            secret,
            client: Client::new(),
        }
    }

    fn sign_request(&self, params: &mut Vec<(&str, String)>) -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap()
            .as_millis().to_string();
        params.push(("recvWindow", "10000".to_string()));
        params.push(("timestamp", timestamp));
        
        let query = params.iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>().join("&");
        
        let mut mac = HmacSha256::new_from_slice(self.secret.as_bytes()).unwrap();
        mac.update(query.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        
        format!("{}&signature={}", query, signature)
    }

    pub async fn fetch_ohlcv(&self, symbol: &str, interval: &str, limit: usize) -> BinanceResult<Vec<Value>> {
        let url = format!("https://api.binance.com/api/v3/klines?symbol={}&interval={}&limit={}", symbol.replace("/", ""), interval, limit);
        let res: Vec<Value> = self.client.get(&url).send().await?.json().await?;
        Ok(res)
    }

    pub async fn fetch_ticker(&self, symbol: &str) -> BinanceResult<f64> {
        let url = format!("https://api.binance.com/api/v3/ticker/price?symbol={}", symbol.replace("/", ""));
        let res: Value = self.client.get(&url).send().await?.json().await?;
        let price = res["price"].as_str().unwrap().parse::<f64>()?;
        Ok(price)
    }

    pub async fn fetch_funding_rate(&self, symbol: &str) -> BinanceResult<f64> {
        let perp_symbol = symbol.replace("/", "").replace("USDC", "USDT");
        let url = format!("https://fapi.binance.com/fapi/v1/premiumIndex?symbol={}", perp_symbol);
        let res = self.client.get(&url).send().await?;
        
        if res.status().is_success() {
            let json: Value = res.json().await?;
            if let Some(funding) = json["lastFundingRate"].as_str() {
                return Ok(funding.parse::<f64>()?);
            }
        }
        Ok(0.0)
    }

    pub async fn fetch_balance(&self) -> BinanceResult<Value> {
        let mut params = vec![];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/account?{}", query);
        
        let res: Value = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn fetch_open_orders(&self, symbol: &str) -> BinanceResult<Vec<Value>> {
        let mut params = vec![("symbol", symbol.replace("/", ""))];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/openOrders?{}", query);
        
        let res: Vec<Value> = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn fetch_order(&self, symbol: &str, order_id: &str) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("orderId", order_id.to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn create_market_buy(&self, symbol: &str, quantity: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "BUY".to_string()),
            ("type", "MARKET".to_string()),
            ("quantity", quantity.to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn create_market_sell(&self, symbol: &str, quantity: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "SELL".to_string()),
            ("type", "MARKET".to_string()),
            ("quantity", quantity.to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn create_limit_sell(&self, symbol: &str, quantity: f64, price: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "SELL".to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTC".to_string()),
            ("quantity", quantity.to_string()),
            ("price", format!("{:.2}", price)), // Make sure precision is handled by caller or string format
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn cancel_order(&self, symbol: &str, order_id: &str) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("orderId", order_id.to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.delete(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }

    pub async fn create_oco_sell(&self, symbol: &str, quantity: f64, price: f64, stop_price: f64, stop_limit_price: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "SELL".to_string()),
            ("quantity", quantity.to_string()),
            ("price", price.to_string()),
            ("stopPrice", stop_price.to_string()),
            ("stopLimitPrice", stop_limit_price.to_string()),
            ("stopLimitTimeInForce", "GTC".to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order/oco?{}", query);
        
        let res: Value = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        Ok(res)
    }
}

impl BinanceClient {
    pub async fn create_limit_buy(&self, symbol: &str, quantity: f64, price: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "BUY".to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTC".to_string()),
            ("quantity", quantity.to_string()),
            ("price", format!("{:.2}", price)),
        ];
        
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let resp = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?;
            
        let json: Value = resp.json().await?;
        if json.get("code").is_some() {
            return Err(format!("Binance API Error: {}", json).into());
        }
        Ok(json)
    }
}
