use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::json;

use crate::channel::{ChannelCursor, InboundEvent};
use crate::runtime::Runtime;
use crate::tidepool::{
    TidepoolClient, TidepoolConfig, TidepoolInboundMessage, clear_shared_client,
    connect_shared_client,
};

const RECONNECT_DELAY: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct TidepoolChannel {
    runtime: Arc<Runtime>,
    config: TidepoolConfig,
}

impl TidepoolChannel {
    pub fn new(runtime: Arc<Runtime>, config: TidepoolConfig) -> Self {
        Self { runtime, config }
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run_forever().await;
        })
    }

    async fn run_forever(self) {
        loop {
            if let Err(error) = self.run_once().await {
                clear_shared_client().await;
                tracing::error!(error = %error, "Tidepool channel loop failed; reconnecting");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }

    async fn run_once(&self) -> Result<()> {
        tracing::info!(
            base_url = %self.config.base_url,
            database = %self.config.database,
            handle = %self.config.handle,
            token_path = %self.config.token_path.display(),
            "Starting Tidepool channel"
        );

        let client = connect_shared_client(self.config.clone())
            .await
            .context("connecting Tidepool client")?;
        let bootstrap = client.bootstrap_outcome();
        tracing::info!(
            account_id = bootstrap.account_id,
            handle = %bootstrap.handle,
            subscribed_domain_ids = ?bootstrap.subscribed_domain_ids,
            token_path = %bootstrap.token_path.display(),
            "Tidepool bootstrap complete"
        );
        self.seed_attach_cursors(client.attach_baseline_sequences())
            .await?;

        while let Some(event) = client.recv().await {
            let inbound = event?;
            self.handle_message(&client, inbound).await?;
        }

        Err(anyhow!("Tidepool client event stream ended"))
    }

    async fn handle_message(
        &self,
        client: &TidepoolClient,
        message: TidepoolInboundMessage,
    ) -> Result<()> {
        let cursor_key = message.domain_id.to_string();
        let current_cursor = self
            .runtime
            .db()
            .load_cursor("tidepool", &cursor_key)
            .await
            .context("loading Tidepool cursor")?
            .and_then(|cursor| cursor.cursor_value.parse::<u64>().ok())
            .unwrap_or(0);

        if message.domain_sequence <= current_cursor {
            tracing::debug!(
                domain_id = message.domain_id,
                domain_sequence = message.domain_sequence,
                current_cursor,
                "Skipping Tidepool message already covered by cursor"
            );
            return Ok(());
        }

        tracing::info!(
            domain_id = message.domain_id,
            message_id = message.message_id,
            domain_sequence = message.domain_sequence,
            author_account_id = message.author_account_id,
            "Tidepool inbound message received"
        );

        let external_thread_id = tidepool_thread_key(message.domain_id);
        let metadata = json!({
            "domain_id": message.domain_id,
            "domain_title": message.domain_title,
            "domain_slug": message.domain_slug,
            "message_id": message.message_id,
            "domain_sequence": message.domain_sequence,
            "author_account_id": message.author_account_id,
            "reply_to_message_id": message.reply_to_message_id,
            "tidepool_target": external_thread_id,
        });

        let outcome = match self
            .runtime
            .handle_inbound(InboundEvent {
                agent_id: self.config.agent_id.clone(),
                channel: "tidepool".to_string(),
                external_thread_id: external_thread_id.clone(),
                content: message.body.clone(),
                metadata: Some(metadata.clone()),
                attachments: Vec::new(),
                received_at: Utc::now(),
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(
                    error = %error,
                    domain_id = message.domain_id,
                    message_id = message.message_id,
                    "Tidepool inbound turn failed"
                );
                return Err(error.into());
            }
        };

        for outbound in &outcome.outbound_messages {
            let trimmed = outbound.trim();
            if trimmed.is_empty() {
                continue;
            }
            if is_tidepool_noop(trimmed) {
                tracing::debug!(
                    domain_id = message.domain_id,
                    message_id = message.message_id,
                    body = %trimmed,
                    "Skipping Tidepool no-op outbound message"
                );
                continue;
            }
            tracing::info!(
                domain_id = message.domain_id,
                message_id = message.message_id,
                "Posting Tidepool outbound reply"
            );
            client
                .post_message(message.domain_id, outbound, Some(message.message_id))
                .with_context(|| {
                    format!("posting reply to Tidepool domain {}", message.domain_id)
                })?;
        }

        self.runtime
            .db()
            .upsert_cursor(&ChannelCursor {
                channel: "tidepool".to_string(),
                cursor_key,
                cursor_value: message.domain_sequence.to_string(),
                updated_at: Utc::now(),
            })
            .await
            .context("upserting Tidepool cursor")?;

        Ok(())
    }

    async fn seed_attach_cursors(
        &self,
        attach_baseline_sequences: &std::collections::HashMap<u64, u64>,
    ) -> Result<()> {
        for (domain_id, baseline_sequence) in attach_baseline_sequences {
            let cursor_key = domain_id.to_string();
            let current_cursor = self
                .runtime
                .db()
                .load_cursor("tidepool", &cursor_key)
                .await
                .context("loading Tidepool cursor during attach")?
                .and_then(|cursor| cursor.cursor_value.parse::<u64>().ok())
                .unwrap_or(0);

            if current_cursor >= *baseline_sequence {
                continue;
            }

            // Seed cursor to baseline - 1 so messages arriving exactly at the
            // baseline boundary are not skipped by the <= filter in handle_message.
            let seed_value = baseline_sequence.saturating_sub(1);

            tracing::info!(
                domain_id = *domain_id,
                baseline_sequence = *baseline_sequence,
                seed_value,
                current_cursor,
                "Seeding Tidepool cursor from attach snapshot baseline"
            );
            self.runtime
                .db()
                .upsert_cursor(&ChannelCursor {
                    channel: "tidepool".to_string(),
                    cursor_key,
                    cursor_value: seed_value.to_string(),
                    updated_at: Utc::now(),
                })
                .await
                .context("upserting Tidepool cursor from attach baseline")?;
        }
        Ok(())
    }
}

fn tidepool_thread_key(domain_id: u64) -> String {
    format!("tidepool:domain:{domain_id}")
}

/// Detect outbound messages that are pure coordination noise.
///
/// In multi-agent Tidepool channels, models often produce low-signal responses
/// like "Acknowledged", "No response needed", or "FYI" to messages that don't
/// require action. These create feedback loops where each trivial reply triggers
/// another turn on the receiving end. This filter drops them before posting.
fn is_tidepool_noop(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let trimmed = lower.trim();

    // Very short messages are almost always noise in coordination contexts
    if trimmed.len() <= 3 {
        return true;
    }

    // Common no-op patterns
    const NOOP_PATTERNS: &[&str] = &[
        "no response needed",
        "no response",
        "noted",
        "acknowledged",
        "ack",
        "understood",
        "got it",
        "roger",
        "copy",
        "ok",
        "okay",
        "👍",
        "fyi",
        "no action needed",
        "no action",
        "standing by",
        "no reply",
        "looks good",
        "sounds good",
        "agreed",
        "confirmed",
        "same here",
        "nothing to add",
        "nothing further",
        "i'm not sure how to respond",
        "that was already fine",
        "the channel is working",
        "no action taken",
        "per the rules",
        "per v0 rules",
        "this is already settled",
        "the thread is already settled",
    ];

    for pattern in NOOP_PATTERNS {
        if trimmed == *pattern || trimmed.starts_with(pattern) {
            return true;
        }
    }

    // Messages that are purely "no reply" declarations
    if trimmed.starts_with("*no reply")
        || trimmed.starts_with("*no action")
        || trimmed.starts_with("no reply —")
        || trimmed.starts_with("no action —")
    {
        return true;
    }

    // Emoji-only or very short symbol messages
    if trimmed.chars().all(|c| c.is_ascii_punctuation() || c.is_ascii_digit() || !c.is_ascii()) {
        if trimmed.chars().count() <= 4 {
            return true;
        }
    }

    false
}

#[cfg(test)]
fn cursor_seed_value(baseline_sequence: u64) -> u64 {
    baseline_sequence.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::{cursor_seed_value, is_tidepool_noop, tidepool_thread_key};
    use std::collections::HashMap;

    #[test]
    fn canonical_thread_key_uses_domain_id() {
        assert_eq!(tidepool_thread_key(42), "tidepool:domain:42");
    }

    #[test]
    fn baseline_map_keeps_highest_sequence_per_domain() {
        let mut baseline = HashMap::new();
        for (domain_id, sequence) in [(7, 2), (7, 5), (8, 3), (7, 4)] {
            baseline
                .entry(domain_id)
                .and_modify(|current| {
                    if sequence > *current {
                        *current = sequence;
                    }
                })
                .or_insert(sequence);
        }
        assert_eq!(baseline.get(&7), Some(&5));
        assert_eq!(baseline.get(&8), Some(&3));
    }

    #[test]
    fn cursor_seed_subtracts_one_to_avoid_boundary_skip() {
        assert_eq!(cursor_seed_value(0), 0);
        assert_eq!(cursor_seed_value(1), 0);
        assert_eq!(cursor_seed_value(48), 47);
        assert_eq!(cursor_seed_value(100), 99);
    }

    #[test]
    fn noop_filters_common_ack_patterns() {
        assert!(is_tidepool_noop("Acknowledged."));
        assert!(is_tidepool_noop("Noted."));
        assert!(is_tidepool_noop("No response needed."));
        assert!(is_tidepool_noop("Understood."));
        assert!(is_tidepool_noop("Got it."));
        assert!(is_tidepool_noop("Standing by."));
        assert!(is_tidepool_noop("Looks good."));
        assert!(is_tidepool_noop("No action taken — FYI per v0 rules."));
        assert!(is_tidepool_noop("No reply — FYI per v0 rules."));
        assert!(is_tidepool_noop("*No reply — FYI per v0 rules.*"));
        assert!(is_tidepool_noop("I'm not sure how to respond to that."));
        assert!(is_tidepool_noop("👍"));
        assert!(is_tidepool_noop("ok"));
        assert!(is_tidepool_noop("OK"));
    }

    #[test]
    fn noop_allows_substantive_messages() {
        assert!(!is_tidepool_noop("I'll investigate the Tidepool connection and report back."));
        assert!(!is_tidepool_noop("CLAIM BUZZ: fixing the cursor seeding bug in tidepool.rs. ETA 10 minutes."));
        assert!(!is_tidepool_noop("REQUEST: Can someone check if CHIP is connected?"));
        assert!(!is_tidepool_noop("The issue is in the SpacetimeDB subscription callback."));
    }

    #[test]
    fn noop_filters_very_short_messages() {
        assert!(is_tidepool_noop("ok"));
        assert!(is_tidepool_noop("ack"));
        assert!(is_tidepool_noop("k"));
    }

    #[test]
    fn noop_allows_medium_messages() {
        assert!(!is_tidepool_noop("Running cargo test now."));
        assert!(!is_tidepool_noop("Fixed in commit abc123."));
    }
}
