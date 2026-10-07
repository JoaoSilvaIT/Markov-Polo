use serde_json::json;

pub async fn send_alert(webhook_url: Option<&str>, message: &str) {
    if let Some(url) = webhook_url {
        if url.trim().is_empty() {
            return;
        }
        let client = reqwest::Client::new();
        let _ = client
            .post(url)
            .json(&json!({ "content": message }))
            .send()
            .await;
    }
}
