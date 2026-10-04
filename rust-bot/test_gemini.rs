use reqwest::Client;
use serde_json::{json, Value};

#[tokio::main]
async fn main() {
    let api_key = std::env::var("GEMINI_API_KEY").unwrap();
    let url = format!("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent?key={}", api_key);
    let payload = json!({
        "contents": [{"role": "user", "parts": [{"text": "Hello"}]}]
    });
    let res: Value = Client::new().post(&url).json(&payload).send().await.unwrap().json().await.unwrap();
    println!("{}", serde_json::to_string_pretty(&res).unwrap());
}
