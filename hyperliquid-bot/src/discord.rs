use serde_json::json;
use std::time::Duration;

pub async fn send_alert(webhook_url: Option<&str>, message: &str) {
    if let Some(url) = webhook_url {
        if url.trim().is_empty() {
            return;
        }
        let client = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(8))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(8),
            client.post(url).json(&json!({ "content": message })).send(),
        )
        .await;
    }
}
