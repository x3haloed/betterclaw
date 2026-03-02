//! Native Discord channel (Gateway WebSocket).
//!
//! Ported and simplified from ZeroClaw's Discord implementation.
//! This integrates directly with IronClaw's `Channel` abstraction so the
//! gateway + discord run in a single process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;

use crate::channels::{Channel, IncomingMessage, MessageStream, OutgoingResponse, StatusUpdate};
use crate::config::DiscordConfig;
use crate::error::ChannelError;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_WS_DEFAULT: &str = "wss://gateway.discord.gg";

// Discord max message length is 2000 chars.
const DISCORD_MAX_MESSAGE_LEN: usize = 2000;

// Rate-limit typing; Discord typing indicator expires quickly, but we don't want to spam.
const TYPING_MIN_INTERVAL: Duration = Duration::from_secs(7);

// Button IDs for approval prompts.
const DISCORD_APPROVAL_APPROVE_PREFIX: &str = "bc_approve:";
const DISCORD_APPROVAL_DENY_PREFIX: &str = "bc_deny:";
const DISCORD_APPROVAL_ALWAYS_PREFIX: &str = "bc_always:";

#[derive(Clone)]
struct DiscordState {
    config: DiscordConfig,
    client: reqwest::Client,
    // Map channel_id -> last typing timestamp.
    typing_last: Arc<Mutex<HashMap<String, Instant>>>,
}

/// Discord channel — connects to Discord Gateway WebSocket for real-time messages.
pub struct DiscordChannel {
    state: DiscordState,
}

impl DiscordChannel {
    pub fn new(config: DiscordConfig) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builder should succeed");
        Self {
            state: DiscordState {
                config,
                client,
                typing_last: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    fn http_client(&self) -> reqwest::Client {
        self.state.client.clone()
    }

    fn is_user_allowed(&self, user_id: &str) -> bool {
        let allow = &self.state.config.allowed_users;
        allow.iter().any(|u| u == "*" || u == user_id)
    }

    fn is_group_sender_trigger_enabled(&self, user_id: &str) -> bool {
        let allow = &self.state.config.group_reply_allowed_sender_ids;
        let user_id = user_id.trim();
        if user_id.is_empty() {
            return false;
        }
        allow.iter().any(|u| u == "*" || u == user_id)
    }

    async fn fetch_bot_user_id(&self) -> Result<String, ChannelError> {
        let url = format!("{DISCORD_API_BASE}/users/@me");
        let resp = self
            .http_client()
            .get(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .send()
            .await
            .map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Failed to call Discord /users/@me: {e}"),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord /users/@me failed ({status}): {body}"),
            });
        }

        let v: serde_json::Value = resp.json().await.map_err(|e| ChannelError::StartupFailed {
            name: "discord".to_string(),
            reason: format!("Discord /users/@me JSON parse failed: {e}"),
        })?;

        Ok(v.get("id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string())
    }

    async fn fetch_gateway_url(&self) -> Result<String, ChannelError> {
        let url = format!("{DISCORD_API_BASE}/gateway/bot");
        let resp = self
            .http_client()
            .get(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .send()
            .await
            .map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Failed to call Discord /gateway/bot: {e}"),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord /gateway/bot failed ({status}): {body}"),
            });
        }

        let v: serde_json::Value = resp.json().await.map_err(|e| ChannelError::StartupFailed {
            name: "discord".to_string(),
            reason: format!("Discord /gateway/bot JSON parse failed: {e}"),
        })?;

        Ok(v.get("url")
            .and_then(|x| x.as_str())
            .unwrap_or(DISCORD_WS_DEFAULT)
            .to_string())
    }

    fn split_for_discord(content: &str) -> Vec<String> {
        if content.chars().count() <= DISCORD_MAX_MESSAGE_LEN {
            return vec![content.to_string()];
        }

        let mut chunks = Vec::new();
        let mut current = String::new();
        for line in content.split('\n') {
            // +1 for the newline we add back.
            let extra = if current.is_empty() { line.len() } else { line.len() + 1 };
            if !current.is_empty() && current.chars().count() + extra > DISCORD_MAX_MESSAGE_LEN {
                chunks.push(current);
                current = String::new();
            }
            if !current.is_empty() {
                current.push('\n');
            }
            current.push_str(line);
        }
        if !current.is_empty() {
            chunks.push(current);
        }

        // Fallback: if a single line was too long, hard-split by char count.
        let mut out = Vec::new();
        for c in chunks {
            if c.chars().count() <= DISCORD_MAX_MESSAGE_LEN {
                out.push(c);
            } else {
                let mut buf = String::new();
                for ch in c.chars() {
                    if buf.chars().count() >= DISCORD_MAX_MESSAGE_LEN {
                        out.push(buf);
                        buf = String::new();
                    }
                    buf.push(ch);
                }
                if !buf.is_empty() {
                    out.push(buf);
                }
            }
        }
        out
    }

    fn normalize_incoming_content(
        content: &str,
        require_mention: bool,
        bot_user_id: &str,
    ) -> Option<String> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }

        if !require_mention {
            return Some(trimmed.to_string());
        }

        // Discord mention formats: <@id> and <@!id>
        let m1 = format!("<@{bot_user_id}>");
        let m2 = format!("<@!{bot_user_id}>");
        if !trimmed.contains(&m1) && !trimmed.contains(&m2) {
            return None;
        }

        let cleaned = trimmed
            .replace(&m1, "")
            .replace(&m2, "")
            .trim()
            .to_string();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }

    async fn post_typing(&self, channel_id: &str) {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/typing");
        let _ = self
            .http_client()
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .send()
            .await;
    }

    async fn maybe_typing(&self, metadata: &serde_json::Value) {
        let Some(ch) = metadata
            .get("discord_channel_id")
            .and_then(|v| v.as_str())
        else {
            return;
        };

        let mut map = self.state.typing_last.lock().await;
        let now = Instant::now();
        let should = match map.get(ch) {
            Some(last) => now.duration_since(*last) >= TYPING_MIN_INTERVAL,
            None => true,
        };
        if should {
            map.insert(ch.to_string(), now);
            drop(map);
            self.post_typing(ch).await;
        }
    }

    async fn send_json_message(&self, channel_id: &str, content: &str) -> Result<(), ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let body = serde_json::json!({ "content": content });
        let resp = self
            .http_client()
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord send failed: {e}"),
            })?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord send failed ({status}): {body}"),
            })
        }
    }

    async fn send_approval_prompt(
        &self,
        channel_id: &str,
        request_id: &str,
        tool_name: &str,
        description: &str,
        parameters: &serde_json::Value,
    ) -> Result<(), ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");
        let args_preview = {
            let raw = parameters.to_string();
            if raw.chars().count() > 260 {
                format!("{}...", raw.chars().take(260).collect::<String>())
            } else {
                raw
            }
        };

        let body = serde_json::json!({
            "content": format!(
                "**Approval required** for tool `{tool_name}`.\n{description}\nRequest ID: `{request_id}`\nArgs: `{args_preview}`\n\nYou can also reply with `yes`, `no`, or `always`.",
            ),
            "components": [{
                "type": 1,
                "components": [
                    { "type": 2, "style": 3, "label": "Approve", "custom_id": format!("{DISCORD_APPROVAL_APPROVE_PREFIX}{request_id}") },
                    { "type": 2, "style": 1, "label": "Always",  "custom_id": format!("{DISCORD_APPROVAL_ALWAYS_PREFIX}{request_id}") },
                    { "type": 2, "style": 4, "label": "Deny",    "custom_id": format!("{DISCORD_APPROVAL_DENY_PREFIX}{request_id}") }
                ]
            }]
        });

        let resp = self
            .http_client()
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord approval prompt failed: {e}"),
            })?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord approval prompt failed ({status}): {body}"),
            })
        }
    }

    fn parse_interaction_custom_id(custom_id: &str) -> Option<(String, bool, bool)> {
        if let Some(rest) = custom_id.strip_prefix(DISCORD_APPROVAL_APPROVE_PREFIX) {
            return Some((rest.to_string(), true, false));
        }
        if let Some(rest) = custom_id.strip_prefix(DISCORD_APPROVAL_ALWAYS_PREFIX) {
            return Some((rest.to_string(), true, true));
        }
        if let Some(rest) = custom_id.strip_prefix(DISCORD_APPROVAL_DENY_PREFIX) {
            return Some((rest.to_string(), false, false));
        }
        None
    }

    async fn ack_interaction(&self, interaction_id: &str, interaction_token: &str) {
        // ACK with a deferred update so Discord doesn't show "interaction failed".
        let url = format!(
            "{DISCORD_API_BASE}/interactions/{interaction_id}/{interaction_token}/callback"
        );
        let body = serde_json::json!({ "type": 6 }); // DEFERRED_UPDATE_MESSAGE
        let _ = self
            .http_client()
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .json(&body)
            .send()
            .await;
    }

    async fn listen_loop(self: Arc<Self>, tx: mpsc::Sender<IncomingMessage>) {
        let mut backoff = Duration::from_secs(2);
        loop {
            match self.listen_once(tx.clone()).await {
                Ok(()) => {
                    backoff = Duration::from_secs(2);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Discord channel disconnected; reconnecting");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(60));
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn listen_once(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<(), ChannelError> {
        let bot_user_id = self.fetch_bot_user_id().await?;
        let gw = self.fetch_gateway_url().await?;
        let ws_url = format!("{gw}/?v=10&encoding=json");

        tracing::info!("Discord: connecting to gateway...");
        let (ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord WS connect failed: {e}"),
            })?;
        let (mut write, mut read) = ws_stream.split();

        // Read Hello (opcode 10)
        let hello = read
            .next()
            .await
            .ok_or_else(|| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: "Discord WS: no hello".to_string(),
            })?
            .map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord WS hello read failed: {e}"),
            })?;
        let hello_v: serde_json::Value =
            serde_json::from_str(&hello.to_string()).map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord hello parse failed: {e}"),
            })?;
        let hb_ms = hello_v
            .get("d")
            .and_then(|d| d.get("heartbeat_interval"))
            .and_then(|v| v.as_u64())
            .unwrap_or(41_250);

        // Identify (opcode 2).
        // Intents: GUILDS | GUILD_MESSAGES | MESSAGE_CONTENT | DIRECT_MESSAGES
        let identify = serde_json::json!({
            "op": 2,
            "d": {
                "token": self.state.config.bot_token.expose_secret(),
                "intents": 37377,
                "properties": {
                    "os": "linux",
                    "browser": "betterclaw",
                    "device": "betterclaw"
                }
            }
        });
        write
            .send(tokio_tungstenite::tungstenite::Message::Text(
                identify.to_string().into(),
            ))
            .await
            .map_err(|e| ChannelError::StartupFailed {
                name: "discord".to_string(),
                reason: format!("Discord identify failed: {e}"),
            })?;

        tracing::info!("Discord: connected and identified");

        // Track last sequence number for heartbeats.
        let mut sequence: i64 = -1;

        let (hb_tx, mut hb_rx) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(hb_ms));
            loop {
                interval.tick().await;
                if hb_tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        let guild_filter = self.state.config.guild_id.clone();

        loop {
            tokio::select! {
                _ = hb_rx.recv() => {
                    let d = if sequence >= 0 { serde_json::json!(sequence) } else { serde_json::json!(null) };
                    let hb = serde_json::json!({ "op": 1, "d": d });
                    if write.send(tokio_tungstenite::tungstenite::Message::Text(hb.to_string().into())).await.is_err() {
                        return Err(ChannelError::StartupFailed {
                            name: "discord".to_string(),
                            reason: "Discord heartbeat send failed".to_string(),
                        });
                    }
                }
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => t,
                        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None => {
                            return Err(ChannelError::StartupFailed { name: "discord".to_string(), reason: "Discord WS closed".to_string() });
                        }
                        _ => continue,
                    };

                    let event: serde_json::Value = match serde_json::from_str(msg.as_ref()) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    if let Some(s) = event.get("s").and_then(|v| v.as_i64()) {
                        sequence = s;
                    }

                    let op = event.get("op").and_then(|v| v.as_u64()).unwrap_or(0);
                    match op {
                        1 => {
                            // Server requested immediate heartbeat.
                            let d = if sequence >= 0 { serde_json::json!(sequence) } else { serde_json::json!(null) };
                            let hb = serde_json::json!({ "op": 1, "d": d });
                            let _ = write.send(tokio_tungstenite::tungstenite::Message::Text(hb.to_string().into())).await;
                            continue;
                        }
                        7 | 9 => {
                            // Reconnect / Invalid Session.
                            return Err(ChannelError::StartupFailed { name: "discord".to_string(), reason: format!("Discord WS restart requested (op {op})") });
                        }
                        _ => {}
                    }

                    let event_type = event.get("t").and_then(|t| t.as_str()).unwrap_or("");
                    let Some(d) = event.get("d") else { continue; };

                    if event_type == "INTERACTION_CREATE" {
                        // Handle approval buttons.
                        let custom_id = d
                            .get("data")
                            .and_then(|x| x.get("custom_id"))
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        let interaction_id = d.get("id").and_then(|x| x.as_str()).unwrap_or("");
                        let interaction_token = d.get("token").and_then(|x| x.as_str()).unwrap_or("");
                        let author_id = d.get("member")
                            .and_then(|m| m.get("user"))
                            .and_then(|u| u.get("id"))
                            .and_then(|x| x.as_str())
                            .or_else(|| d.get("user").and_then(|u| u.get("id")).and_then(|x| x.as_str()))
                            .unwrap_or("");

                        if !custom_id.is_empty() && !interaction_id.is_empty() && !interaction_token.is_empty() {
                            self.ack_interaction(interaction_id, interaction_token).await;
                        }

                        if !author_id.is_empty() && !self.is_user_allowed(author_id) {
                            tracing::warn!(author_id, "Discord: ignoring interaction from unauthorized user");
                            continue;
                        }

                        if let Some((request_id, approved, always)) = Self::parse_interaction_custom_id(custom_id) {
                            // IMPORTANT: `Submission` is an externally-tagged enum in IronClaw.
                            // The parser only accepts JSON that deserializes to `Submission::ExecApproval`,
                            // which means we must wrap fields under the `"ExecApproval"` variant key.
                            let content = serde_json::json!({
                                "ExecApproval": {
                                    "request_id": request_id,
                                    "approved": approved,
                                    "always": always
                                }
                            }).to_string();

                            let channel_id = d
                                .get("channel_id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            let mut msg = IncomingMessage::new("discord", author_id, content);
                            if !channel_id.is_empty() {
                                msg.thread_id = Some(channel_id.to_string());
                            }
                            msg.metadata = serde_json::json!({
                                "discord_channel_id": channel_id,
                                "discord_sender_id": author_id
                            });

                            if tx.send(msg).await.is_err() {
                                return Err(ChannelError::StartupFailed { name: "discord".to_string(), reason: "Discord: receiver dropped".to_string() });
                            }
                        }
                        continue;
                    }

                    if event_type != "MESSAGE_CREATE" {
                        continue;
                    }

                    let author = d.get("author").cloned().unwrap_or(serde_json::Value::Null);
                    let author_id = author.get("id").and_then(|x| x.as_str()).unwrap_or("");
                    if author_id.is_empty() {
                        continue;
                    }

                    // Skip messages from the bot itself.
                    if author_id == bot_user_id {
                        continue;
                    }

                    // Skip other bot messages unless enabled.
                    if !self.state.config.listen_to_bots
                        && author.get("bot").and_then(|x| x.as_bool()).unwrap_or(false)
                    {
                        continue;
                    }

                    if !self.is_user_allowed(author_id) {
                        tracing::debug!(author_id, "Discord: ignoring message from unauthorized user");
                        continue;
                    }

                    let guild_id = d.get("guild_id").and_then(|x| x.as_str());
                    if let (Some(filter), Some(gid)) = (guild_filter.as_deref(), guild_id) {
                        if gid != filter {
                            continue;
                        }
                    }

                    let channel_id = d.get("channel_id").and_then(|x| x.as_str()).unwrap_or("");
                    if channel_id.is_empty() {
                        continue;
                    }

                    let content = d.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    let is_group_message = guild_id.is_some();
                    let allow_sender_without_mention =
                        is_group_message && self.is_group_sender_trigger_enabled(author_id);
                    let require_mention =
                        self.state.config.mention_only && is_group_message && !allow_sender_without_mention;

                    let Some(clean_content) = Self::normalize_incoming_content(content, require_mention, &bot_user_id) else {
                        continue;
                    };

                    // Lightweight attachment markers (no fetching here).
                    let attachment_text = d.get("attachments").and_then(|a| a.as_array()).map(|arr| {
                        let mut parts = Vec::new();
                        for att in arr {
                            let name = att
                                .get("filename")
                                .and_then(|x| x.as_str())
                                .unwrap_or("file");
                            let url = att.get("url").and_then(|x| x.as_str()).unwrap_or("");
                            let ct = att
                                .get("content_type")
                                .and_then(|x| x.as_str())
                                .unwrap_or("");
                            if !url.is_empty() {
                                if ct.to_ascii_lowercase().starts_with("image/") {
                                    parts.push(format!("[IMAGE:{url}]"));
                                } else {
                                    parts.push(format!("[ATTACHMENT:{name}] {url}"));
                                }
                            }
                        }
                        parts.join("\n")
                    }).unwrap_or_default();

                    let final_content = if attachment_text.is_empty() {
                        clean_content
                    } else {
                        format!("{clean_content}\n\n[Attachments]\n{attachment_text}")
                    };

                    let user_name = author
                        .get("global_name")
                        .and_then(|x| x.as_str())
                        .or_else(|| author.get("username").and_then(|x| x.as_str()))
                        .map(|s| s.to_string());

                    let message_id = d.get("id").and_then(|x| x.as_str()).unwrap_or("");

                    let metadata = serde_json::json!({
                        "discord_channel_id": channel_id,
                        "discord_message_id": message_id,
                        "discord_guild_id": guild_id.unwrap_or(""),
                        "discord_sender_id": author_id
                    });

                    let mut msg = IncomingMessage::new("discord", author_id, final_content)
                        .with_metadata(metadata);
                    msg.thread_id = Some(channel_id.to_string());
                    if let Some(name) = user_name {
                        msg.user_name = Some(name);
                    }

                    if tx.send(msg).await.is_err() {
                        return Err(ChannelError::StartupFailed { name: "discord".to_string(), reason: "Discord: receiver dropped".to_string() });
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    async fn start(&self) -> Result<MessageStream, ChannelError> {
        let (tx, rx) = mpsc::channel(256);
        let this = Arc::new(self.clone_for_task());
        tokio::spawn(async move {
            this.listen_loop(tx).await;
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    async fn respond(&self, msg: &IncomingMessage, response: OutgoingResponse) -> Result<(), ChannelError> {
        let channel_id = msg
            .metadata
            .get("discord_channel_id")
            .and_then(|v| v.as_str())
            .or_else(|| msg.thread_id.as_deref())
            .ok_or(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: "Missing discord_channel_id for reply".to_string(),
            })?;

        if !response.attachments.is_empty() {
            tracing::warn!(
                count = response.attachments.len(),
                "DiscordChannel: attachments are not implemented yet; sending text only"
            );
        }

        for chunk in Self::split_for_discord(&response.content) {
            self.send_json_message(channel_id, &chunk).await?;
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
        Ok(())
    }

    async fn send_status(&self, status: StatusUpdate, metadata: &serde_json::Value) -> Result<(), ChannelError> {
        match status {
            StatusUpdate::Thinking(_) | StatusUpdate::ToolStarted { .. } => {
                self.maybe_typing(metadata).await;
            }
            StatusUpdate::ApprovalNeeded { request_id, tool_name, description, parameters } => {
                let Some(channel_id) = metadata
                    .get("discord_channel_id")
                    .and_then(|v| v.as_str())
                else {
                    return Ok(());
                };
                self.send_approval_prompt(channel_id, &request_id, &tool_name, &description, &parameters).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn health_check(&self) -> Result<(), ChannelError> {
        let _ = self.fetch_bot_user_id().await?;
        Ok(())
    }
}

impl DiscordChannel {
    fn clone_for_task(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}
