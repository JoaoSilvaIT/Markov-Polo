use reqwest::Client;
use serde_json::{json, Value};
use gcp_auth::{provider, TokenProvider};
use std::sync::Arc;

pub struct GeminiClient {
    pub client: Client,
    pub auth_manager: Option<Arc<dyn TokenProvider>>,
    pub project_id: String,
    pub location: String,
}

impl GeminiClient {
    pub async fn new() -> Self {
        let project_id = std::env::var("GOOGLE_CLOUD_PROJECT").unwrap_or_else(|_| "your-gcp-project-id".to_string());
        let location = std::env::var("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|_| "us-central1".to_string());
        
        let auth_manager = match provider().await {
            Ok(manager) => Some(Arc::from(manager)),
            Err(e) => {
                println!("WARNING: Failed to init GCP auth: {}", e);
                None
            }
        };

        Self {
            client: Client::new(),
            auth_manager,
            project_id,
            location,
        }
    }

    pub async fn send_conversation(&self, contents: &[Value]) -> Result<Value, String> {
        let auth = self.auth_manager.as_ref().ok_or("GCP Auth not initialized")?;
        let token = auth.token(&["https://www.googleapis.com/auth/cloud-platform"]).await.map_err(|e| e.to_string())?;

        let url = format!(
            "https://{}-aiplatform.googleapis.com/v1/projects/{}/locations/{}/publishers/google/models/gemini-2.5-pro:generateContent",
            self.location, self.project_id, self.location
        );

        let tools = vec![
            json!({
                "functionDeclarations": [
                    {
                        "name": "scan_market_opportunities",
                        "description": "Scans all active trading pairs and returns technical data.",
                        "parameters": { "type": "OBJECT", "properties": {} }
                    },
                    {
                        "name": "fetch_market_news",
                        "description": "Fetches recent news headlines for a symbol",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "symbol": { "type": "STRING" }
                            },
                            "required": ["symbol"]
                        }
                    },
                    {
                        "name": "place_order",
                        "description": "Places a market buy with native OCO exit",
                        "parameters": {
                            "type": "OBJECT",
                            "properties": {
                                "symbol": { "type": "STRING" },
                                "take_profit_price": { "type": "NUMBER" },
                                "stop_loss_price": { "type": "NUMBER" },
                                "risk_pct": { "type": "NUMBER" }
                            },
                            "required": ["symbol", "take_profit_price", "stop_loss_price", "risk_pct"]
                        }
                    }
                ]
            })
        ];

        let mut system_instruction_text = format!("You are a Binance Quantitative Portfolio Manager. The current UTC time is {}. You have a strict mandate: Beat 2.5% APY while prioritizing extreme capital preservation. Always write a final MEMO containing 'Decision: PASS' or 'Decision: EXECUTE'.", chrono::Utc::now());
        
        if std::env::var("FORCE_TEST_TRADE").unwrap_or_default() == "true" {
            system_instruction_text = format!("{} FOR THIS CYCLE ONLY: You are in TEST MODE. You MUST execute a test trade immediately on the asset with the best setup, regardless of strict risk limits. Do not pass. We are testing the API execution.", system_instruction_text);
        }

        let payload = json!({
            "contents": contents,
            "tools": tools,
            "systemInstruction": {
                "parts": [{"text": system_instruction_text}]
            }
        });

        let res = self.client.post(&url)
            .header("Authorization", format!("Bearer {}", token.as_str()))
            .json(&payload)
            .send().await.map_err(|e| e.to_string())?;

        let json_res: Value = res.json().await.map_err(|e| e.to_string())?;
        Ok(json_res)
    }
}
