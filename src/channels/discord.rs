//! Native Discord channel (Gateway WebSocket).
//!
//! Ported and simplified from ZeroClaw's Discord implementation.
//! This integrates directly with BetterClaw's `Channel` abstraction so the
//! gateway + discord run in a single process.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use rand::seq::SliceRandom;
use secrecy::ExposeSecret;
use tokio::sync::{Mutex, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::channels::{Channel, IncomingMessage, MessageStream, OutgoingResponse, StatusUpdate};
use crate::config::DiscordConfig;
use crate::error::ChannelError;

const DISCORD_API_BASE: &str = "https://discord.com/api/v10";
const DISCORD_WS_DEFAULT: &str = "wss://gateway.discord.gg";

// Discord max message length is 2000 chars.
const DISCORD_MAX_MESSAGE_LEN: usize = 2000;
// Common Discord upload limit for bots without boosts. This varies by server,
// but we enforce a conservative cap to avoid repeated 413s.
const DISCORD_MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
// Discord accepts max 10 files per message.
const DISCORD_MAX_FILES: usize = 10;
const DISCORD_MAX_TEXT_ATTACHMENT_BYTES: usize = 256 * 1024;
const DISCORD_ATTACHMENT_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

// Rate-limit typing; Discord typing indicator expires quickly, but we don't want to spam.
const TYPING_LOOP_INTERVAL: Duration = Duration::from_secs(7);

// Button IDs for approval prompts.
const DISCORD_APPROVAL_APPROVE_PREFIX: &str = "bc_approve:";
const DISCORD_APPROVAL_DENY_PREFIX: &str = "bc_deny:";
const DISCORD_APPROVAL_ALWAYS_PREFIX: &str = "bc_always:";

#[derive(Clone)]
struct DiscordState {
    config: DiscordConfig,
    client: reqwest::Client,
    /// Active typing tasks keyed by Discord channel_id.
    typing_tasks: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
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
                typing_tasks: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }

    fn http_client(&self) -> reqwest::Client {
        self.state.client.clone()
    }

    fn is_user_allowed(&self, user_id: &str, is_group_message: bool) -> bool {
        let allow = if is_group_message {
            &self.state.config.guild_allowed_users
        } else {
            &self.state.config.dm_allowed_users
        };
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

    fn format_guild_sender_prefix(
        primary_discord_user_id: Option<&str>,
        author_id: &str,
        display_name: &str,
    ) -> String {
        let is_primary = primary_discord_user_id
            .map(|pid| pid.trim())
            .filter(|pid| !pid.is_empty())
            .is_some_and(|pid| pid == author_id);

        if is_primary {
            format!("Discord(guild): YOU {display_name} ({author_id}): ")
        } else {
            format!("Discord(guild): {display_name} ({author_id}): ")
        }
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
        // Code-block-aware splitter: preserves fenced blocks across chunk boundaries
        // by inserting closing/opening fences as needed.
        if content.chars().count() <= DISCORD_MAX_MESSAGE_LEN {
            return vec![content.to_string()];
        }

        #[derive(Clone)]
        struct Fence {
            open_line: String, // e.g. ```rust
        }

        fn push_with_limit(
            out: &mut Vec<String>,
            current: &mut String,
            line: &str,
            active_fence: Option<&Fence>,
        ) {
            const CLOSER: &str = "\n```";
            let closer_len = CLOSER.chars().count();

            // When we're inside a fenced block, reserve space for the closing fence
            // so every emitted chunk is self-contained (balanced fences).
            let max_len = if active_fence.is_some() {
                DISCORD_MAX_MESSAGE_LEN.saturating_sub(closer_len)
            } else {
                DISCORD_MAX_MESSAGE_LEN
            };

            let sep = if current.is_empty() { "" } else { "\n" };
            let candidate_len =
                current.chars().count() + sep.chars().count() + line.chars().count();
            if candidate_len <= max_len {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(line);
                return;
            }

            if !current.is_empty() {
                // Close fence before flushing if we're inside one.
                if active_fence.is_some() {
                    current.push_str(CLOSER);
                }
                out.push(std::mem::take(current));
            }

            // Start a new chunk; reopen fence if needed.
            if let Some(f) = active_fence {
                current.push_str(&f.open_line);
                current.push('\n');
            }

            // If the line itself is too long, hard-split it.
            let line_len = line.chars().count();
            if line_len > DISCORD_MAX_MESSAGE_LEN {
                if let Some(f) = active_fence {
                    // For fenced blocks, split the long line into pieces that
                    // leave room for open+close fences.
                    let overhead = f.open_line.chars().count() + 1 + closer_len; // open + \n + closer
                    let cap = DISCORD_MAX_MESSAGE_LEN.saturating_sub(overhead).max(1);
                    let mut it = line.chars();
                    loop {
                        let piece: String = it.by_ref().take(cap).collect();
                        if piece.is_empty() {
                            break;
                        }
                        out.push(format!("{}\n{}{}", f.open_line, piece, CLOSER));
                    }
                } else {
                    // Plain text: split into <= 2000-char chunks.
                    let mut it = line.chars();
                    loop {
                        let piece: String = it.by_ref().take(DISCORD_MAX_MESSAGE_LEN).collect();
                        if piece.is_empty() {
                            break;
                        }
                        out.push(piece);
                    }
                }
                return;
            }

            current.push_str(line);
        }

        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut fence: Option<Fence> = None;

        for raw_line in content.split('\n') {
            let line = raw_line;
            let trimmed = line.trim_start();
            let is_fence_line = trimmed.starts_with("```");

            if is_fence_line {
                if fence.is_some() {
                    // Closing fence: include it in the current chunk.
                    push_with_limit(&mut out, &mut current, "```", None);
                    fence = None;
                    continue;
                } else {
                    // Opening fence: capture full opening line (may include language).
                    let open_line = trimmed.to_string();
                    push_with_limit(&mut out, &mut current, &open_line, None);
                    fence = Some(Fence { open_line });
                    continue;
                }
            }

            // Normal line: route through limiter with awareness of current fence.
            let active = fence.as_ref();
            push_with_limit(&mut out, &mut current, line, active);
        }

        if !current.is_empty() {
            if fence.is_some() && !current.ends_with("```") {
                current.push_str("\n```");
            }
            out.push(current);
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

        let cleaned = trimmed.replace(&m1, "").replace(&m2, "").trim().to_string();
        if cleaned.is_empty() {
            None
        } else {
            Some(cleaned)
        }
    }

    fn is_image_attachment(content_type: &str, filename: &str, url: &str) -> bool {
        let normalized_content_type = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();

        if !normalized_content_type.is_empty() {
            if normalized_content_type.starts_with("image/") {
                return true;
            }
            // Trust explicit non-image MIME to avoid false positives.
            if normalized_content_type != "application/octet-stream" {
                return false;
            }
        }

        Self::has_image_extension(filename) || Self::has_image_extension(url)
    }

    fn has_image_extension(value: &str) -> bool {
        let value = value.to_ascii_lowercase();
        [
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tiff", ".svg",
        ]
        .iter()
        .any(|ext| value.ends_with(ext))
    }

    async fn fetch_text_attachment_limited(&self, url: &str) -> Option<String> {
        let resp = self
            .http_client()
            .get(url)
            .timeout(DISCORD_ATTACHMENT_FETCH_TIMEOUT)
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        if let Some(len) = resp.content_length()
            && len as usize > DISCORD_MAX_TEXT_ATTACHMENT_BYTES
        {
            return None;
        }

        let mut bytes: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(item) = stream.next().await {
            let chunk = item.ok()?;
            let remaining = DISCORD_MAX_TEXT_ATTACHMENT_BYTES.saturating_sub(bytes.len());
            if remaining == 0 {
                break;
            }
            if chunk.len() <= remaining {
                bytes.extend_from_slice(&chunk);
            } else {
                bytes.extend_from_slice(&chunk[..remaining]);
                break;
            }
        }

        Some(String::from_utf8_lossy(&bytes).to_string())
    }

    async fn process_attachments(&self, attachments: &[serde_json::Value]) -> String {
        let mut parts: Vec<String> = Vec::new();

        for att in attachments {
            let ct = att
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = att
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("file");
            let Some(url) = att.get("url").and_then(|v| v.as_str()) else {
                continue;
            };

            if Self::is_image_attachment(ct, name, url) {
                parts.push(format!("[IMAGE:{url}]"));
                continue;
            }

            if ct.starts_with("text/") {
                match self.fetch_text_attachment_limited(url).await {
                    Some(text) => parts.push(format!("[{name}]\n{text}")),
                    None => parts.push(format!("[ATTACHMENT:{name}] {url}")),
                }
                continue;
            }

            parts.push(format!("[ATTACHMENT:{name}] {url}"));
        }

        parts.join("\n---\n")
    }

    fn ack_reaction_enabled_for_chat_type(&self, is_group_message: bool) -> bool {
        let types = &self.state.config.ack_reaction_chat_types;
        if types.is_empty() {
            return true;
        }
        if is_group_message {
            types.iter().any(|t| t == "group")
        } else {
            types.iter().any(|t| t == "dm")
        }
    }

    async fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), ChannelError> {
        let encoded = urlencoding::encode(emoji);
        let url = format!(
            "{DISCORD_API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{encoded}/@me"
        );

        let resp = self
            .http_client()
            .put(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord add reaction failed: {e}"),
            })?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord add reaction failed ({status}): {body}"),
            })
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

    async fn start_typing(&self, channel_id: &str) {
        let mut tasks = self.state.typing_tasks.lock().await;
        if tasks.contains_key(channel_id) {
            return;
        }

        let ch = channel_id.to_string();
        let this = self.clone_for_task();
        let handle = tokio::spawn(async move {
            loop {
                this.post_typing(&ch).await;
                tokio::time::sleep(TYPING_LOOP_INTERVAL).await;
            }
        });

        tasks.insert(channel_id.to_string(), handle);
    }

    async fn stop_typing(&self, channel_id: &str) {
        let mut tasks = self.state.typing_tasks.lock().await;
        if let Some(handle) = tasks.remove(channel_id) {
            handle.abort();
        }
    }

    async fn stop_typing_from_metadata(&self, metadata: &serde_json::Value) {
        if let Some(ch) = metadata.get("discord_channel_id").and_then(|v| v.as_str()) {
            self.stop_typing(ch).await;
        }
    }

    async fn start_typing_from_metadata(&self, metadata: &serde_json::Value) {
        if let Some(ch) = metadata.get("discord_channel_id").and_then(|v| v.as_str()) {
            self.start_typing(ch).await;
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

    async fn send_message_with_files(
        &self,
        channel_id: &str,
        content: &str,
        attachments: &[String],
    ) -> Result<(), ChannelError> {
        let url = format!("{DISCORD_API_BASE}/channels/{channel_id}/messages");

        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for path_str in attachments.iter().take(DISCORD_MAX_FILES) {
            let path = std::path::Path::new(path_str);
            let meta = tokio::fs::metadata(path)
                .await
                .map_err(|e| ChannelError::SendFailed {
                    name: "discord".to_string(),
                    reason: format!("Attachment not found: {path_str}: {e}"),
                })?;

            if !meta.is_file() {
                return Err(ChannelError::SendFailed {
                    name: "discord".to_string(),
                    reason: format!("Attachment is not a file: {path_str}"),
                });
            }

            if meta.len() > DISCORD_MAX_FILE_BYTES {
                return Err(ChannelError::SendFailed {
                    name: "discord".to_string(),
                    reason: format!(
                        "Attachment too large ({} bytes > {}): {path_str}",
                        meta.len(),
                        DISCORD_MAX_FILE_BYTES
                    ),
                });
            }

            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            let data = tokio::fs::read(path)
                .await
                .map_err(|e| ChannelError::SendFailed {
                    name: "discord".to_string(),
                    reason: format!("Failed to read attachment: {path_str}: {e}"),
                })?;
            files.push((name, data));
        }

        if files.is_empty() {
            return self.send_json_message(channel_id, content).await;
        }

        let payload = serde_json::json!({ "content": content }).to_string();
        let mut form = reqwest::multipart::Form::new().text("payload_json", payload);

        for (i, (name, data)) in files.into_iter().enumerate() {
            let mime = mime_guess::from_path(&name).first_or_octet_stream();
            let part = reqwest::multipart::Part::bytes(data)
                .file_name(name)
                .mime_str(mime.as_ref())
                .map_err(|e| ChannelError::SendFailed {
                    name: "discord".to_string(),
                    reason: format!("Failed to build multipart part: {e}"),
                })?;
            form = form.part(format!("files[{i}]"), part);
        }

        let resp = self
            .http_client()
            .post(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.state.config.bot_token.expose_secret()),
            )
            .multipart(form)
            .send()
            .await
            .map_err(|e| ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord send (multipart) failed: {e}"),
            })?;

        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: format!("Discord send (multipart) failed ({status}): {body}"),
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

                        let is_group_interaction = d.get("guild_id").and_then(|x| x.as_str()).is_some();
                        if !author_id.is_empty() && !self.is_user_allowed(author_id, is_group_interaction) {
                            tracing::warn!(author_id, "Discord: ignoring interaction from unauthorized user");
                            continue;
                        }

                        if let Some((request_id, approved, always)) = Self::parse_interaction_custom_id(custom_id) {
                            // IMPORTANT: `Submission` is an externally-tagged enum in BetterClaw.
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
                            // Route all Discord traffic into a single BetterClaw user_id by default.
                            // The Discord sender is tracked separately in metadata + allowlists.
                            let mut msg =
                                IncomingMessage::new("discord", &self.state.config.user_id, content);
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

                    if !self.is_user_allowed(author_id, is_group_message) {
                        tracing::debug!(
                            author_id,
                            is_group_message,
                            "Discord: ignoring message from unauthorized user"
                        );
                        continue;
                    }

                    let allow_sender_without_mention =
                        is_group_message && self.is_group_sender_trigger_enabled(author_id);
                    let require_mention =
                        self.state.config.mention_only && is_group_message && !allow_sender_without_mention;

                    let Some(clean_content) = Self::normalize_incoming_content(content, require_mention, &bot_user_id) else {
                        continue;
                    };

                    let user_name = author
                        .get("global_name")
                        .and_then(|x| x.as_str())
                        .or_else(|| author.get("username").and_then(|x| x.as_str()))
                        .map(|s| s.to_string());
                    let display_name = user_name.as_deref().unwrap_or(author_id).trim();

                    let attachment_text = if let Some(arr) = d.get("attachments").and_then(|a| a.as_array()) {
                        self.process_attachments(arr).await
                    } else {
                        String::new()
                    };
                    let image_urls: Vec<String> = d
                         .get("attachments")
                        .and_then(|a| a.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|att| {
                                    let ct = att.get("content_type").and_then(|v| v.as_str()).unwrap_or("");
                                    let name = att.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                                    let url = att.get("url").and_then(|v| v.as_str())?;
                                    if Self::is_image_attachment(ct, name, url) {
                                        Some(url.to_string())
                                    } else {
                                        None
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_else(|| Vec::new());
                    

                    let mut final_content = if attachment_text.is_empty() {
                        clean_content
                    } else {
                        format!("{clean_content}\n\n[Attachments]\n{attachment_text}")
                    };

                    // Shared workspace: in guild channels, prefix messages with explicit sender identity.
                    // DMs are left untouched.
                    if is_group_message {
                        let prefix = Self::format_guild_sender_prefix(
                            self.state.config.primary_discord_user_id.as_deref(),
                            author_id,
                            display_name,
                        );
                        final_content = format!("{prefix}{final_content}");
                    }

                    let message_id = d.get("id").and_then(|x| x.as_str()).unwrap_or("");

                    // Optional ACK reaction (best-effort, async).
                    if !message_id.is_empty()
                        && !channel_id.is_empty()
                        && !self.state.config.ack_reactions.is_empty()
                        && self.ack_reaction_enabled_for_chat_type(is_group_message)
                    {
                        let mut rng = rand::thread_rng();
                        if let Some(emoji) = self.state.config.ack_reactions.choose(&mut rng).cloned() {
                            let ch = channel_id.to_string();
                            let mid = message_id.to_string();
                            let this = self.clone_for_task();
                            tokio::spawn(async move {
                                let _ = this.add_reaction(&ch, &mid, &emoji).await;
                            });
                        }
                    }

                    let metadata = serde_json::json!({
                        "discord_channel_id": channel_id,
                        "discord_message_id": message_id,
                        "discord_guild_id": guild_id.unwrap_or(""),
                        "discord_sender_id": author_id
                    });

                    // Route all Discord traffic into a single BetterClaw user_id by default.
                    // The Discord sender is tracked separately in metadata + allowlists.
                    let mut msg = IncomingMessage::new("discord", &self.state.config.user_id, final_content)
                        .with_metadata(metadata);
                    if !image_urls.is_empty() {
                        msg.images = image_urls;
                    }
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

    async fn respond(
        &self,
        msg: &IncomingMessage,
        response: OutgoingResponse,
    ) -> Result<(), ChannelError> {
        let channel_id = msg
            .metadata
            .get("discord_channel_id")
            .and_then(|v| v.as_str())
            .or_else(|| msg.thread_id.as_deref())
            .ok_or(ChannelError::SendFailed {
                name: "discord".to_string(),
                reason: "Missing discord_channel_id for reply".to_string(),
            })?;

        // If we're about to respond, we're no longer "typing".
        self.stop_typing(channel_id).await;

        let mut chunks = Self::split_for_discord(&response.content);
        if chunks.is_empty() {
            chunks.push(String::new());
        }

        // If there are attachments, send them with the first chunk only.
        let mut first = true;
        for chunk in chunks {
            if first && !response.attachments.is_empty() {
                if response.attachments.len() > DISCORD_MAX_FILES {
                    tracing::warn!(
                        count = response.attachments.len(),
                        max = DISCORD_MAX_FILES,
                        "DiscordChannel: truncating attachments to Discord limit"
                    );
                }
                self.send_message_with_files(channel_id, &chunk, &response.attachments)
                    .await?;
            } else {
                self.send_json_message(channel_id, &chunk).await?;
            }
            first = false;
            tokio::time::sleep(Duration::from_millis(350)).await;
        }
        Ok(())
    }

    async fn send_status(
        &self,
        status: StatusUpdate,
        metadata: &serde_json::Value,
    ) -> Result<(), ChannelError> {
        match status {
            StatusUpdate::Thinking(_)
            | StatusUpdate::ToolStarted { .. }
            | StatusUpdate::StreamChunk(_) => {
                self.start_typing_from_metadata(metadata).await;
            }
            StatusUpdate::Status(msg) => {
                let lower = msg.to_ascii_lowercase();
                // Best-effort lifecycle mapping from agent status strings.
                if lower.contains("done")
                    || lower.contains("rejected")
                    || lower.contains("awaiting approval")
                    || lower.contains("awaiting")
                {
                    self.stop_typing_from_metadata(metadata).await;
                }
            }
            StatusUpdate::ApprovalNeeded {
                request_id,
                tool_name,
                description,
                parameters,
            } => {
                let Some(channel_id) = metadata.get("discord_channel_id").and_then(|v| v.as_str())
                else {
                    return Ok(());
                };
                self.stop_typing(channel_id).await;
                self.send_approval_prompt(
                    channel_id,
                    &request_id,
                    &tool_name,
                    &description,
                    &parameters,
                )
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn health_check(&self) -> Result<(), ChannelError> {
        let _ = self.fetch_bot_user_id().await?;
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), ChannelError> {
        let mut tasks = self.state.typing_tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
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

#[cfg(test)]
mod tests {
    use super::DiscordChannel;

    #[test]
    fn split_for_discord_keeps_chunks_under_limit() {
        let input = "a".repeat(5000);
        let chunks = DiscordChannel::split_for_discord(&input);
        assert!(chunks.len() >= 3);
        for c in chunks {
            assert!(c.chars().count() <= super::DISCORD_MAX_MESSAGE_LEN);
        }
    }

    #[test]
    fn split_for_discord_preserves_fenced_blocks() {
        let mut input = String::new();
        input.push_str("before\n");
        input.push_str("```rust\n");
        input.push_str(&"let x = 1;\n".repeat(400));
        input.push_str("```\n");
        input.push_str("after\n");

        let chunks = DiscordChannel::split_for_discord(&input);
        assert!(chunks.len() >= 2);
        for c in &chunks {
            assert!(c.chars().count() <= super::DISCORD_MAX_MESSAGE_LEN);
        }

        // If any chunk opens a fence, it should also close it (we wrap/reopen across chunks).
        for c in &chunks {
            let opens = c.matches("```").count();
            assert!(opens % 2 == 0, "chunk has unbalanced fences:\n{c}");
        }
    }

    #[test]
    fn guild_sender_prefix_marks_primary_as_you() {
        let p = DiscordChannel::format_guild_sender_prefix(Some("123"), "123", "Chad");
        assert!(p.contains("Discord(guild):"));
        assert!(p.contains("YOU"));
        assert!(p.contains("Chad"));
        assert!(p.contains("(123)"));

        let p2 = DiscordChannel::format_guild_sender_prefix(Some("123"), "999", "Alice");
        assert!(p2.contains("Discord(guild):"));
        assert!(!p2.contains("YOU"));
        assert!(p2.contains("Alice"));
        assert!(p2.contains("(999)"));
    }
}
