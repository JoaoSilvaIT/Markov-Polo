use reqwest::Client;
use serde_json::Value;

pub async fn fetch_market_news(api_key: &str, secret_key: &str, symbol: &str) -> String {
    let client = Client::new();
    
    let query_sym = symbol.split('/').next().unwrap_or(symbol);
    let url = format!("https://data.alpaca.markets/v1beta1/news?symbols={}&limit=3", query_sym);
    
    let res = client
        .get(&url)
        .header("APCA-API-KEY-ID", api_key)
        .header("APCA-API-SECRET-KEY", secret_key)
        .send()
        .await;

    match res {
        Ok(response) => {
            if let Ok(json) = response.json::<Value>().await {
                let mut headlines = Vec::new();
                if let Some(news) = json.get("news").and_then(|n| n.as_array()) {
                    for article in news {
                        if let Some(headline) = article.get("headline").and_then(|h| h.as_str()) {
                            headlines.push(headline.to_string());
                        }
                    }
                }
                serde_json::to_string(&serde_json::json!({
                    "symbol": symbol,
                    "headlines": headlines
                })).unwrap_or_else(|_| "{}".to_string())
            } else {
                serde_json::json!({ "error": "Failed to parse news JSON" }).to_string()
            }
        }
        Err(e) => serde_json::json!({ "error": e.to_string() }).to_string()
    }
}
