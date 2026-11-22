use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::agent::{AgentCommand, AgentEvent};
use crate::config::Config;

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const INTENTS: u64 = (1 << 0) | (1 << 11) | (1 << 12);
const DISCORD_MESSAGE_LIMIT: usize = 2000;

pub fn should_respond(author: &Value, channel_id: &str, allowed_user_id: &str) -> bool {
    if author.get("bot").and_then(|b| b.as_bool()).unwrap_or(false) {
        return false;
    }
    if !allowed_user_id.is_empty() {
        let id = author.get("id").and_then(|i| i.as_str()).unwrap_or("");
        if id != allowed_user_id {
            return false;
        }
    }
    let _ = channel_id;
    true
}

pub fn chunk_message(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.len() > DISCORD_MESSAGE_LIMIT {
        let mut cut = DISCORD_MESSAGE_LIMIT;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        chunks.push(rest[..cut].to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest.to_string());
    }
    chunks
}

pub fn spawn(
    config: Config,
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    events: broadcast::Sender<AgentEvent>,
) {
    tokio::spawn(reply_relay(config.discord.clone(), events.subscribe()));
    tokio::spawn(async move {
        loop {
            if let Err(e) = gateway_session(&config, &agent_tx).await {
                eprintln!("discord gateway error: {e}");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn gateway_session(
    config: &Config,
    agent_tx: &mpsc::UnboundedSender<AgentCommand>,
) -> anyhow::Result<()> {
    let (mut ws, _) = connect_async(GATEWAY_URL).await?;
    let hello: Value = read_json(&mut ws).await?;
    let interval_ms = hello["d"]["heartbeat_interval"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("no heartbeat_interval in HELLO"))?;
    let seq = Arc::new(AtomicU64::new(0));

    let identify = json!({
        "op": 2,
        "d": {
            "token": config.discord.bot_token,
            "intents": INTENTS,
            "properties": { "os": "linux", "browser": "rho", "device": "rho" },
        }
    });
    ws.send(Message::Text(identify.to_string().into())).await?;

    let mut heartbeat = tokio::time::interval(Duration::from_millis(interval_ms));
    let seq_task = seq.clone();

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let hb = json!({ "op": 1, "d": seq_task.load(Ordering::SeqCst) });
                let _ = ws.send(Message::Text(hb.to_string().into())).await;
            }
            frame = ws.next() => {
                let frame = frame.ok_or_else(|| anyhow::anyhow!("gateway closed"))??;
                let Message::Text(text) = frame else { continue };
                let text = text.as_str();
                let Ok(value) = serde_json::from_str::<Value>(text) else { continue };
                if let Some(s) = value["s"].as_u64() {
                    seq.store(s, Ordering::SeqCst);
                }
                match value["op"].as_u64() {
                    Some(0) if value["t"].as_str() == Some("MESSAGE_CREATE") => {}
                    Some(0) => continue,
                    _ => continue,
                }
                let d = &value["d"];
                let channel_id = d["channel_id"].as_str().unwrap_or("").to_string();
                let is_relevant = config.discord.channel_id.is_empty()
                    || config.discord.channel_id == channel_id
                    || d["guild_id"].is_null();
                if !is_relevant {
                    continue;
                }
                if !should_respond(&d["author"], &channel_id, &config.discord.allowed_user_id) {
                    continue;
                }
                let content = d["content"].as_str().unwrap_or("").trim().to_string();
                if !content.is_empty() {
                    let _ = agent_tx.send(AgentCommand::Prompt { text: content });
                }
            }
        }
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> anyhow::Result<T> {
    while let Some(frame) = ws.next().await {
        match frame? {
            Message::Text(text) => {
                if let Ok(value) = serde_json::from_str::<T>(text.as_str()) {
                    return Ok(value);
                }
            }
            Message::Close(c) => anyhow::bail!("gateway closed: {c:?}"),
            _ => {}
        }
    }
    anyhow::bail!("gateway stream ended")
}

async fn reply_relay(
    config: crate::config::DiscordConfig,
    mut events: broadcast::Receiver<AgentEvent>,
) {
    let http = reqwest::Client::new();
    loop {
        match events.recv().await {
            Ok(AgentEvent::AssistantDone { text }) | Ok(AgentEvent::Error(text)) => {
                for chunk in chunk_message(&text) {
                    if let Err(e) = post_message(&http, &config, &chunk).await {
                        eprintln!("discord send failed: {e}");
                    }
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn post_message(
    http: &reqwest::Client,
    config: &crate::config::DiscordConfig,
    text: &str,
) -> anyhow::Result<()> {
    if config.channel_id.is_empty() {
        return Ok(());
    }
    let url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        config.channel_id
    );
    let resp = http
        .post(url)
        .bearer_auth(&config.bot_token)
        .json(&json!({ "content": text }))
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "discord REST {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_bots_and_wrong_users() {
        let bot = json!({"bot": true, "id": "1"});
        let user = json!({"bot": false, "id": "42"});
        assert!(!should_respond(&bot, "c", ""));
        assert!(should_respond(&user, "c", ""));
        assert!(should_respond(&user, "c", "42"));
        assert!(!should_respond(&user, "c", "99"));
    }

    #[test]
    fn chunks_long_messages() {
        let text = "x".repeat(4500);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 2000);
        assert_eq!(chunks[2].len(), 500);
        assert!(chunk_message("  ").is_empty());
        assert_eq!(chunk_message("hi"), vec!["hi".to_string()]);
    }

    #[test]
    fn chunks_respect_char_boundaries() {
        let text = "é".repeat(1500);
        let chunks = chunk_message(&text);
        assert_eq!(chunks.concat(), text);
        assert_eq!(
            chunks.iter().map(|c| c.chars().count()).sum::<usize>(),
            1500
        );
    }
}
