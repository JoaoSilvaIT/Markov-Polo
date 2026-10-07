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

    pub async fn fetch_book_ticker(&self, symbol: &str) -> BinanceResult<(f64, f64)> {
        let url = format!("https://api.binance.com/api/v3/ticker/bookTicker?symbol={}", symbol.replace("/", ""));
        let res: Value = self.client.get(&url).send().await?.json().await?;
        let bid = res["bidPrice"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        let ask = res["askPrice"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
        Ok((bid, ask))
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
        Ok(res)
    }

    pub async fn fetch_free_balance(&self, asset: &str) -> BinanceResult<f64> {
        let balance = self.fetch_balance().await?;
        if let Some(balances) = balance.get("balances").and_then(|b| b.as_array()) {
            for b in balances {
                if b.get("asset").and_then(|a| a.as_str()) == Some(asset) {
                    let free_str = b.get("free").and_then(|f| f.as_str()).unwrap_or("0");
                    return Ok(free_str.parse::<f64>().unwrap_or(0.0));
                }
            }
        }
        Ok(0.0)
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
        Ok(res)
    }

    pub async fn fetch_order_by_client_id(&self, symbol: &str, client_order_id: &str) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("origClientOrderId", client_order_id.to_string()),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.get(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
        Ok(res)
    }

    pub async fn create_limit_sell(&self, symbol: &str, quantity: f64, price: f64) -> BinanceResult<Value> {
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "SELL".to_string()),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "GTC".to_string()),
            ("quantity", quantity.to_string()),
            ("price", format!("{:.2}", price)),
        ];
        let query = self.sign_request(&mut params);
        let url = format!("https://api.binance.com/api/v3/order?{}", query);
        
        let res: Value = self.client.post(&url)
            .header("X-MBX-APIKEY", &self.api_key)
            .send().await?.json().await?;
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
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
        if res.get("code").is_some() {
            return Err(format!("Binance API Error: {}", res).into());
        }
        Ok(res)
    }

    pub async fn create_limit_buy(&self, symbol: &str, quantity: f64, price: f64, client_order_id: Option<&str>) -> BinanceResult<Value> {
        // Enforce LIMIT_MAKER (Post-Only) to strictly capture maker rebates and prevent taker fees.
        // LIMIT_MAKER rejects timeInForce parameter on Binance Spot.
        let mut params = vec![
            ("symbol", symbol.replace("/", "")),
            ("side", "BUY".to_string()),
            ("type", "LIMIT_MAKER".to_string()),
            ("quantity", quantity.to_string()),
            ("price", format!("{:.2}", price)),
        ];
        if let Some(cid) = client_order_id {
            params.push(("newClientOrderId", cid.to_string()));
        }
        
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

pub fn get_symbol_lot_precision(symbol: &str) -> i32 {
    let clean = symbol.replace("/", "").to_uppercase();
    match clean.as_str() {
        "BTCUSDC" => 5,
        "ETHUSDC" => 4,
        "SOLUSDC" => 2,
        _ => 3,
    }
}

pub fn quantize_lot_size(amount: f64, precision: i32) -> f64 {
    let multiplier = 10_f64.powi(precision);
    ((amount * multiplier) + 1e-8).floor() / multiplier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lot_precision_and_quantization() {
        assert_eq!(get_symbol_lot_precision("BTCUSDC"), 5);
        assert_eq!(get_symbol_lot_precision("BTC/USDC"), 5);
        assert_eq!(get_symbol_lot_precision("ETHUSDC"), 4);
        assert_eq!(get_symbol_lot_precision("ETH/USDC"), 4);
        assert_eq!(get_symbol_lot_precision("SOLUSDC"), 2);
        assert_eq!(get_symbol_lot_precision("SOL/USDC"), 2);

        // BTC: 5 decimal places
        assert_eq!(quantize_lot_size(0.000141234, 5), 0.00014);
        // ETH: 4 decimal places
        assert_eq!(quantize_lot_size(0.0044567, 4), 0.0044);
        // SOL: 2 decimal places
        assert_eq!(quantize_lot_size(0.0991, 2), 0.09);
    }
}
