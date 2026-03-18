use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::channel::{InboundAttachment, InboundEvent};
use crate::runtime::Runtime;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_WS_BASE: &str = "wss://gateway.discord.gg";
const DISCORD_MAX_MESSAGE_LEN: usize = 2000;
const DISCORD_INTENTS: u64 = 1 | (1 << 9) | (1 << 12) | (1 << 15);
const DEFAULT_AGENT_ID: &str = "default";
const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub bot_token: String,
    pub api_base: String,
    pub gateway_base: String,
    pub allowed_guild_ids: Vec<String>,
    pub allowed_channel_ids: Vec<String>,
    pub allowed_dm_user_ids: Vec<String>,
    pub allowed_guild_user_ids: Vec<String>,
    pub mention_only: bool,
}

impl DiscordConfig {
    pub fn from_env() -> Option<Self> {
        let bot_token = std::env::var("DISCORD_BOT_TOKEN").ok()?;
        Some(Self {
            bot_token,
            api_base: std::env::var("DISCORD_API_BASE")
                .unwrap_or_else(|_| DISCORD_API_BASE.to_string()),
            gateway_base: std::env::var("DISCORD_GATEWAY_BASE")
                .unwrap_or_else(|_| DISCORD_WS_BASE.to_string()),
            allowed_guild_ids: parse_csv_env("DISCORD_ALLOWED_GUILD_IDS"),
            allowed_channel_ids: parse_csv_env_any(&[
                "DISCORD_ALLOWED_CHANNEL_IDS",
                "DISCORD_ALLOWED_GUILD_CHANNEL_IDS",
            ]),
            allowed_dm_user_ids: parse_csv_env("DISCORD_ALLOWED_DM_USER_IDS"),
            allowed_guild_user_ids: parse_csv_env("DISCORD_ALLOWED_GUILD_USER_IDS"),
            mention_only: parse_bool_env("DISCORD_MENTION_ONLY"),
        })
    }
}

#[derive(Clone)]
pub struct DiscordChannel {
    runtime: std::sync::Arc<Runtime>,
    config: DiscordConfig,
    client: Client,
}

impl DiscordChannel {
    pub fn new(runtime: std::sync::Arc<Runtime>, config: DiscordConfig) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("building Discord HTTP client")?;
        Ok(Self {
            runtime,
            config,
            client,
        })
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_forever().await;
        })
    }

    async fn run_forever(self) {
        loop {
            if let Err(error) = self.run_once().await {
                tracing::error!(error = %error, "Discord channel loop failed; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }

    async fn run_once(&self) -> Result<()> {
        let bot_user_id = self.fetch_bot_user_id().await?;
        let gateway_url = self.fetch_gateway_url().await?;
        let ws_url = format!("{gateway_url}/?v=10&encoding=json");
        let (socket, _) = connect_async(&ws_url)
            .await
            .with_context(|| format!("connecting to Discord gateway at {ws_url}"))?;
        let (mut write, mut read) = socket.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();
        let seq = std::sync::Arc::new(Mutex::new(None::<i64>));

        let writer = tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                if write.send(message).await.is_err() {
                    break;
                }
            }
        });

        let mut heartbeat_task: Option<tokio::task::JoinHandle<()>> = None;

        while let Some(frame) = read.next().await {
            let frame = frame.context("reading Discord gateway frame")?;
            let Message::Text(text) = frame else {
                continue;
            };
            let payload: Value =
                serde_json::from_str(&text).context("parsing Discord gateway payload")?;
            if let Some(value) = payload.get("s").and_then(Value::as_i64) {
                *seq.lock().await = Some(value);
            }

            match payload
                .get("op")
                .and_then(Value::as_u64)
                .unwrap_or_default()
            {
                10 => {
                    let interval_ms = payload
                        .get("d")
                        .and_then(|value| value.get("heartbeat_interval"))
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            anyhow!("Discord hello payload missing heartbeat interval")
                        })?;
                    let identify = json!({
                        "op": 2,
                        "d": {
                            "token": self.config.bot_token,
                            "intents": DISCORD_INTENTS,
                            "properties": {
                                "os": std::env::consts::OS,
                                "browser": "betterclaw",
                                "device": "betterclaw"
                            }
                        }
                    });
                    outbound_tx
                        .send(Message::Text(identify.to_string().into()))
                        .map_err(|_| anyhow!("Discord gateway writer dropped"))?;
                    let heartbeat_tx = outbound_tx.clone();
                    let heartbeat_seq = std::sync::Arc::clone(&seq);
                    heartbeat_task = Some(tokio::spawn(async move {
                        let mut interval =
                            tokio::time::interval(Duration::from_millis(interval_ms));
                        loop {
                            interval.tick().await;
                            let next_seq = *heartbeat_seq.lock().await;
                            let heartbeat = json!({
                                "op": 1,
                                "d": next_seq,
                            });
                            if heartbeat_tx
                                .send(Message::Text(heartbeat.to_string().into()))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }));
                }
                1 => {
                    let next_seq = *seq.lock().await;
                    let heartbeat = json!({
                        "op": 1,
                        "d": next_seq,
                    });
                    outbound_tx
                        .send(Message::Text(heartbeat.to_string().into()))
                        .map_err(|_| anyhow!("Discord gateway writer dropped"))?;
                }
                7 | 9 => {
                    return Err(anyhow!(
                        "Discord requested reconnect (op {})",
                        payload["op"]
                    ));
                }
                0 => {
                    let event_type = payload.get("t").and_then(Value::as_str).unwrap_or_default();
                    if event_type == "MESSAGE_CREATE"
                        && let Some(message) =
                            self.parse_inbound_message(payload.get("d"), &bot_user_id)
                    {
                        self.handle_message(message).await?;
                    }
                }
                _ => {}
            }
        }

        if let Some(task) = heartbeat_task {
            task.abort();
        }
        writer.abort();
        Err(anyhow!("Discord gateway stream ended"))
    }

    async fn handle_message(&self, message: DiscordInboundMessage) -> Result<()> {
        let outcome = match self.runtime.handle_inbound(message.event).await {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(error = %error, "Discord inbound turn failed");
                let error_text =
                    format!("I hit an internal error while handling that message.\n\n{error}");
                self.send_text_reply(&message.channel_id, &error_text)
                    .await?;
                return Ok(());
            }
        };

        if outcome
            .outbound_messages
            .iter()
            .all(|content| content.trim().is_empty())
        {
            return Ok(());
        }

        for outbound in &outcome.outbound_messages {
            if outbound.trim().is_empty() {
                continue;
            }
            self.send_text_reply(&message.channel_id, outbound).await?;
        }
        Ok(())
    }

    fn parse_inbound_message(
        &self,
        payload: Option<&Value>,
        bot_user_id: &str,
    ) -> Option<DiscordInboundMessage> {
        let payload = payload?;
        let author = payload.get("author")?;
        let author_id = author.get("id")?.as_str()?.to_string();
        if author_id == bot_user_id {
            return None;
        }
        if author.get("bot").and_then(Value::as_bool).unwrap_or(false) {
            return None;
        }

        let channel_id = payload.get("channel_id")?.as_str()?.to_string();
        let guild_id = payload
            .get("guild_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(guild_id) = guild_id.as_deref()
            && !self.config.allowed_guild_ids.is_empty()
            && !self
                .config
                .allowed_guild_ids
                .iter()
                .any(|entry| entry == guild_id)
        {
            return None;
        }
        if !guild_channel_is_allowed(guild_id.as_deref(), &channel_id, &self.config.allowed_channel_ids) {
            return None;
        }

        let is_dm = guild_id.is_none();
        let allow_list = if is_dm {
            &self.config.allowed_dm_user_ids
        } else {
            &self.config.allowed_guild_user_ids
        };
        if !allow_list.is_empty() && !allow_list.iter().any(|entry| entry == &author_id) {
            return None;
        }

        let raw_content = payload
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let normalized = normalize_message_content(
            raw_content,
            bot_user_id,
            !is_dm && self.config.mention_only,
        )?;

        let author_name = author
            .get("global_name")
            .and_then(Value::as_str)
            .or_else(|| author.get("username").and_then(Value::as_str))
            .unwrap_or(&author_id)
            .to_string();
        let message_id = payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let attachments = parse_discord_attachments(payload.get("attachments"));
        let content = append_attachment_lines(normalized, payload.get("attachments"));
        let content = if is_dm {
            content
        } else {
            format!(
                "Discord(guild): {} ({}): {}",
                author_name, author_id, content
            )
        };

        Some(DiscordInboundMessage {
            channel_id: channel_id.clone(),
            event: InboundEvent {
                agent_id: inbound_agent_id().to_string(),
                channel: "discord".to_string(),
                external_thread_id: discord_thread_key(&channel_id, is_dm),
                content,
                metadata: None,
                attachments,
                received_at: chrono::Utc::now(),
            },
            message_id,
        })
    }

    async fn send_text_reply(&self, channel_id: &str, content: &str) -> Result<()> {
        for chunk in split_for_discord(content) {
            let response = self
                .client
                .post(format!(
                    "{}/channels/{channel_id}/messages",
                    self.config.api_base
                ))
                .header("Authorization", format!("Bot {}", self.config.bot_token))
                .json(&json!({ "content": chunk }))
                .send()
                .await
                .with_context(|| format!("sending Discord message to channel {channel_id}"))?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "Discord send failed for channel {} ({}): {}",
                    channel_id,
                    status,
                    body
                ));
            }
        }
        Ok(())
    }

    async fn fetch_bot_user_id(&self) -> Result<String> {
        let response = self
            .client
            .get(format!("{}/users/@me", self.config.api_base))
            .header("Authorization", format!("Bot {}", self.config.bot_token))
            .send()
            .await
            .context("calling Discord /users/@me")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Discord /users/@me failed ({status}): {body}"));
        }
        let payload: Value = response
            .json()
            .await
            .context("parsing Discord /users/@me")?;
        payload
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Discord /users/@me payload missing id"))
    }

    async fn fetch_gateway_url(&self) -> Result<String> {
        let response = self
            .client
            .get(format!("{}/gateway/bot", self.config.api_base))
            .header("Authorization", format!("Bot {}", self.config.bot_token))
            .send()
            .await
            .context("calling Discord /gateway/bot")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Discord /gateway/bot failed ({status}): {body}"));
        }
        let payload: Value = response
            .json()
            .await
            .context("parsing Discord /gateway/bot")?;
        let base = payload
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(&self.config.gateway_base);
        Ok(base.to_string())
    }
}

#[derive(Debug, Clone)]
struct DiscordInboundMessage {
    channel_id: String,
    #[allow(dead_code)]
    message_id: String,
    event: InboundEvent,
}

fn parse_csv_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|value| parse_csv_value(&value))
        .unwrap_or_default()
}

fn parse_csv_env_any(names: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    for name in names {
        for entry in parse_csv_env(name) {
            if !values.contains(&entry) {
                values.push(entry);
            }
        }
    }
    values
}

fn parse_csv_value(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn guild_channel_is_allowed(
    guild_id: Option<&str>,
    channel_id: &str,
    allowed_channel_ids: &[String],
) -> bool {
    guild_id.is_none()
        || allowed_channel_ids.is_empty()
        || allowed_channel_ids.iter().any(|entry| entry == channel_id)
}

fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn discord_thread_key(channel_id: &str, is_dm: bool) -> String {
    if is_dm {
        format!("discord:dm:{channel_id}")
    } else {
        format!("discord:channel:{channel_id}")
    }
}

fn normalize_message_content(
    raw: &str,
    bot_user_id: &str,
    require_mention: bool,
) -> Option<String> {
    let mention_a = format!("<@{bot_user_id}>");
    let mention_b = format!("<@!{bot_user_id}>");
    let mentioned = raw.contains(&mention_a) || raw.contains(&mention_b);
    if require_mention && !mentioned {
        return None;
    }
    let content = raw.replace(&mention_a, "").replace(&mention_b, "");
    let trimmed = content.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn append_attachment_lines(content: String, attachments: Option<&Value>) -> String {
    let Some(items) = attachments.and_then(Value::as_array) else {
        return content;
    };
    if items.is_empty() {
        return content;
    }
    let mut lines = Vec::new();
    for attachment in items {
        let url = attachment
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let filename = attachment
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("attachment");
        lines.push(format!("- {}: {}", filename, url));
    }
    if lines.is_empty() {
        return content;
    }
    format!("{content}\n\n[Attachments]\n{}", lines.join("\n"))
}

fn parse_discord_attachments(attachments: Option<&Value>) -> Vec<InboundAttachment> {
    let Some(items) = attachments.and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let url = item.get("url")?.as_str()?.to_string();
            let filename = item
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("attachment")
                .to_string();
            let content_type = item
                .get("content_type")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            Some(InboundAttachment {
                url,
                filename,
                content_type,
            })
        })
        .collect()
}

fn split_for_discord(content: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    let chars = content.chars().collect::<Vec<_>>();
    chars
        .chunks(DISCORD_MAX_MESSAGE_LEN)
        .map(|chunk| chunk.iter().collect::<String>())
        .collect()
}

fn inbound_agent_id() -> &'static str {
    DEFAULT_AGENT_ID
}

#[cfg(test)]
mod tests {
    use super::{
        append_attachment_lines, discord_thread_key, inbound_agent_id, normalize_message_content,
        guild_channel_is_allowed, parse_csv_value, parse_discord_attachments, split_for_discord,
    };
    use serde_json::json;

    #[test]
    fn guild_messages_require_mention_when_enabled() {
        assert_eq!(normalize_message_content("hello there", "123", true), None);
        assert_eq!(
            normalize_message_content("<@123> hello there", "123", true).as_deref(),
            Some("hello there")
        );
    }

    #[test]
    fn thread_key_uses_dm_prefix_for_direct_messages() {
        assert_eq!(discord_thread_key("555", true), "discord:dm:555");
        assert_eq!(discord_thread_key("555", false), "discord:channel:555");
    }

    #[test]
    fn attachment_lines_append_urls() {
        let body = append_attachment_lines(
            "hello".to_string(),
            Some(&json!([
                { "filename": "image.png", "url": "https://example.com/image.png" }
            ])),
        );
        assert!(body.contains("[Attachments]"));
        assert!(body.contains("image.png"));
    }

    #[test]
    fn parse_discord_attachments_extracts_images() {
        let attachments = parse_discord_attachments(Some(&json!([
            { "filename": "photo.jpg", "url": "https://cdn.discord.com/photo.jpg", "content_type": "image/jpeg" },
            { "filename": "doc.pdf", "url": "https://cdn.discord.com/doc.pdf", "content_type": "application/pdf" }
        ])));
        assert_eq!(attachments.len(), 2);
        assert!(attachments[0].is_image());
        assert!(!attachments[1].is_image());
        assert_eq!(attachments[0].url, "https://cdn.discord.com/photo.jpg");
        assert_eq!(attachments[0].content_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn parse_discord_attachments_empty_when_no_attachments() {
        let attachments = parse_discord_attachments(None);
        assert!(attachments.is_empty());
        let attachments = parse_discord_attachments(Some(&json!([])));
        assert!(attachments.is_empty());
    }

    #[test]
    fn discord_split_respects_limit() {
        let input = "a".repeat(4500);
        let chunks = split_for_discord(&input);
        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 2000));
    }

    #[test]
    fn discord_inbound_messages_always_target_default_agent() {
        assert_eq!(inbound_agent_id(), "default");
    }

    #[test]
    fn parse_csv_value_trims_and_drops_empty_entries() {
        let values = parse_csv_value(" 1, 2 ,,3 ");
        assert_eq!(values, vec!["1".to_string(), "2".to_string(), "3".to_string()]);
    }

    #[test]
    fn guild_channel_whitelist_blocks_non_whitelisted_guild_channels_only() {
        let allowed = vec!["123".to_string()];
        assert!(guild_channel_is_allowed(Some("guild-1"), "123", &allowed));
        assert!(!guild_channel_is_allowed(Some("guild-1"), "999", &allowed));
        assert!(guild_channel_is_allowed(None, "999", &allowed));
        assert!(guild_channel_is_allowed(Some("guild-1"), "999", &[]));
    }
}
