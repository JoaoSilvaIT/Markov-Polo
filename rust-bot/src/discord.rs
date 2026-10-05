use serde_json::json;
use serenity::async_trait;
use serenity::model::channel::Message;
use serenity::prelude::*;
use crate::binance::BinanceClient;
use crate::state::StateMap;
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;

pub async fn send_alert(webhook_url: Option<&str>, message: &str) {
    if let Some(url) = webhook_url {
        let client = reqwest::Client::new();
        let _ = client
            .post(url)
            .json(&json!({ "content": message }))
            .send()
            .await;
    }
}

struct Handler {
    binance: Arc<BinanceClient>,
    state: Arc<TokioMutex<StateMap>>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: Message) {
        if msg.content.trim() == "!stop" {
            let _ = msg.channel_id.say(&ctx.http, "🚨 **EMERGENCY STOP INITIATED!** Liquidating all open positions and shutting down...").await;
            
            let mut st = self.state.lock().await;
            for (sym, pos) in st.iter() {
                // Cancel resting TP limit order
                if !pos.tp_order_id.is_empty() {
                    let _ = self.binance.cancel_order(sym, &pos.tp_order_id).await;
                }
                // Market sell to liquidate
                let _ = self.binance.create_market_sell(sym, pos.amount).await;
            }
            
            st.clear();
            let _ = crate::state::save_state(&st);
            
            let _ = msg.channel_id.say(&ctx.http, "✅ **All positions liquidated.** Bot process terminated.").await;
            
            std::process::exit(0);
        }
    }
}

pub async fn run_discord_bot(token: String, binance: Arc<BinanceClient>, state: Arc<TokioMutex<StateMap>>) {
    let intents = GatewayIntents::GUILD_MESSAGES | GatewayIntents::DIRECT_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { binance, state })
        .await
        .expect("Error creating Discord client");

    if let Err(why) = client.start().await {
        println!("Discord client error: {:?}", why);
    }
}
