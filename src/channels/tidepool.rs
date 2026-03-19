use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::json;

use crate::channel::{ChannelCursor, InboundEvent};
use crate::runtime::Runtime;
use crate::tidepool::{
    TidepoolClient, TidepoolConfig, TidepoolInboundContext, TidepoolInboundMessage, clear_shared_client,
    connect_shared_client,
};

/// Initial reconnect delay. Doubled on each consecutive failure up to MAX_RECONNECT_DELAY.
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_secs(1);
/// Cap on exponential backoff for reconnects.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);
/// If a connection survives this long, reset the backoff to INITIAL on next failure.
const CONNECTION_HEALTHY_THRESHOLD: Duration = Duration::from_secs(30);

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

    /// Run the reconnect loop with exponential backoff.
    ///
    /// Connection-level failures (event stream ends, Tidepool server unreachable)
    /// trigger a reconnect with doubling delay. Model API errors inside handle_message
    /// are logged and skipped — they do NOT tear down the Tidepool connection.
    async fn run_forever(self) {
        let mut backoff = INITIAL_RECONNECT_DELAY;
        loop {
            let start = tokio::time::Instant::now();
            if let Err(error) = self.run_once().await {
                clear_shared_client().await;

                // If the connection was healthy for a while, reset backoff.
                // A connection that lasted CONNECTION_HEALTHY_THRESHOLD means
                // the failure was likely transient, not a persistent outage.
                if start.elapsed() >= CONNECTION_HEALTHY_THRESHOLD {
                    backoff = INITIAL_RECONNECT_DELAY;
                }

                tracing::error!(
                    error = %error,
                    backoff_secs = backoff.as_secs(),
                    "Tidepool channel loop failed; reconnecting with backoff"
                );

                tokio::time::sleep(backoff).await;

                // Exponential backoff with cap
                backoff = (backoff * 2).min(MAX_RECONNECT_DELAY);
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

        // Skip messages authored by this agent to prevent self-echo feedback loops.
        // Without this, an agent subscribed to the same domain as another instance of
        // itself (or sharing a domain with cross-posting) would receive its own outbound
        // messages as new inbound events, triggering unnecessary turns.
        if let Some(account) = client.account() {
            if is_self_echo(message.author_account_id, account.account_id) {
                tracing::debug!(
                    domain_id = message.domain_id,
                    message_id = message.message_id,
                    author_account_id = message.author_account_id,
                    "Skipping Tidepool self-echo message"
                );
                // Still advance the cursor so we don't re-process on reconnect.
                self.runtime
                    .db()
                    .upsert_cursor(&ChannelCursor {
                        channel: "tidepool".to_string(),
                        cursor_key,
                        cursor_value: message.domain_sequence.to_string(),
                        updated_at: Utc::now(),
                    })
                    .await
                    .context("upserting Tidepool cursor after self-echo skip")?;
                return Ok(());
            }
        }

        tracing::info!(
            domain_id = message.domain_id,
            message_id = message.message_id,
            domain_sequence = message.domain_sequence,
            author_account_id = message.author_account_id,
            "Tidepool inbound message received"
        );

        let inbound_context = client.inbound_context(&message);
        let external_thread_id = tidepool_thread_key(message.domain_id);
        let metadata = json!({
            "domain_id": message.domain_id,
            "domain_title": message.domain_title,
            "domain_slug": message.domain_slug,
            "message_id": message.message_id,
            "domain_sequence": message.domain_sequence,
            "author_account_id": message.author_account_id,
            "reply_to_message_id": message.reply_to_message_id,
            "created_at_micros": message.created_at_micros,
            "auto_subscribed_dm_first_message": inbound_context.auto_subscribed_dm_first_message,
            "unsubscribe_tool_call": unsubscribe_tool_call(message.domain_id),
            "tidepool_target": external_thread_id,
        });
        let content = render_inbound_content(&message, &inbound_context);

        let outcome = match self
            .runtime
            .handle_inbound(InboundEvent {
                agent_id: self.config.agent_id.clone(),
                channel: "tidepool".to_string(),
                external_thread_id: external_thread_id.clone(),
                content,
                metadata: Some(metadata.clone()),
                attachments: Vec::new(),
                received_at: Utc::now(),
            })
            .await
        {
            Ok(outcome) => outcome,
            Err(error) => {
                let error_text = error.to_string();
                tracing::error!(
                    error = %error_text,
                    domain_id = message.domain_id,
                    message_id = message.message_id,
                    "Tidepool inbound turn failed — notifying model, then advancing cursor"
                );
                self
                    .notify_model_of_inbound_failure(&message, &metadata, &error_text)
                    .await;
                self.runtime
                    .db()
                    .upsert_cursor(&ChannelCursor {
                        channel: "tidepool".to_string(),
                        cursor_key,
                        cursor_value: message.domain_sequence.to_string(),
                        updated_at: Utc::now(),
                    })
                    .await
                    .context("upserting Tidepool cursor after failed turn")?;
                return Ok(());
            }
        };

        // Track posting errors without tearing down the connection.
        // We always advance the cursor — we've consumed the inbound message.
        // If posting fails, we log it. On reconnect, cursor prevents re-processing
        // the inbound (avoiding duplicate replies for already-succeeded posts).
        let mut first_post_error: Option<anyhow::Error> = None;
        for outbound in &outcome.outbound_messages {
            if outbound.trim().is_empty() {
                continue;
            }
            tracing::info!(
                domain_id = message.domain_id,
                message_id = message.message_id,
                "Posting Tidepool outbound reply"
            );
            if let Err(error) = client
                .post_message(message.domain_id, outbound, Some(message.message_id))
                .with_context(|| {
                    format!("posting reply to Tidepool domain {}", message.domain_id)
                })
            {
                tracing::error!(
                    error = %error,
                    domain_id = message.domain_id,
                    message_id = message.message_id,
                    "Failed to post Tidepool reply — will advance cursor anyway"
                );
                if first_post_error.is_none() {
                    first_post_error = Some(error);
                }
            }
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

        if let Some(error) = first_post_error {
            // Return the posting error to trigger reconnect, but only after
            // cursor is persisted. This prevents the reconnect from
            // re-processing the inbound message (cursor already advanced).
            return Err(error);
        }

        Ok(())
    }

    async fn notify_model_of_inbound_failure(
        &self,
        message: &TidepoolInboundMessage,
        metadata: &serde_json::Value,
        error_text: &str,
    ) {
        let external_thread_id = tidepool_thread_key(message.domain_id);
        let mut error_metadata = metadata.clone();
        if let Some(object) = error_metadata.as_object_mut() {
            object.insert("tidepool_runtime_error".to_string(), json!(true));
            object.insert(
                "tidepool_runtime_error_message".to_string(),
                json!(error_text),
            );
            object.insert(
                "tidepool_runtime_error_source_message_id".to_string(),
                json!(message.message_id),
            );
        }
        let content = format!(
            "System note: handling Tidepool message {} in domain {} failed before the model completed the turn. Error: {}. Please account for that failure in your ongoing coordination state.",
            message.message_id, message.domain_id, error_text
        );
        if let Err(notify_error) = self
            .runtime
            .handle_inbound(InboundEvent {
                agent_id: self.config.agent_id.clone(),
                channel: "tidepool".to_string(),
                external_thread_id,
                content,
                metadata: Some(error_metadata),
                attachments: Vec::new(),
                received_at: Utc::now(),
            })
            .await
        {
            tracing::error!(
                error = %notify_error,
                domain_id = message.domain_id,
                message_id = message.message_id,
                "Failed to notify model of Tidepool runtime error"
            );
        }
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

fn unsubscribe_tool_call(domain_id: u64) -> String {
    format!("tidepool_unsubscribe_domain({{\"domain_id\":{domain_id}}})")
}

fn render_inbound_content(
    message: &TidepoolInboundMessage,
    context: &TidepoolInboundContext,
) -> String {
    if !context.auto_subscribed_dm_first_message {
        return message.body.clone();
    }

    format!(
        "[System note: This message is the first inbound message from a DM domain you were auto-subscribed to when it was created. If you do not want further messages from this DM, call {}.]\n\n{}",
        unsubscribe_tool_call(message.domain_id),
        message.body
    )
}

/// Returns true if the message was authored by this agent (self-echo).
/// Prevents an agent from receiving its own outbound messages as new inbound events.
fn is_self_echo(author_account_id: u64, own_account_id: u64) -> bool {
    author_account_id == own_account_id
}

#[cfg(test)]
fn cursor_seed_value(baseline_sequence: u64) -> u64 {
    baseline_sequence.saturating_sub(1)
}

#[cfg(test)]
fn next_backoff(current: std::time::Duration) -> std::time::Duration {
    (current * 2).min(MAX_RECONNECT_DELAY)
}

#[cfg(test)]
mod tests {
    use super::{
        CONNECTION_HEALTHY_THRESHOLD, INITIAL_RECONNECT_DELAY, MAX_RECONNECT_DELAY,
        cursor_seed_value, is_self_echo, next_backoff, render_inbound_content,
        tidepool_thread_key, unsubscribe_tool_call,
    };
    use crate::tidepool::{TidepoolInboundContext, TidepoolInboundMessage};
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn canonical_thread_key_uses_domain_id() {
        assert_eq!(tidepool_thread_key(42), "tidepool:domain:42");
    }

    #[test]
    fn unsubscribe_tool_call_uses_exact_tool_shape() {
        assert_eq!(
            unsubscribe_tool_call(42),
            "tidepool_unsubscribe_domain({\"domain_id\":42})"
        );
    }

    #[test]
    fn render_inbound_content_leaves_normal_messages_unchanged() {
        let message = TidepoolInboundMessage {
            domain_id: 42,
            domain_title: "DM".to_string(),
            domain_slug: "".to_string(),
            message_id: 7,
            domain_sequence: 2,
            author_account_id: 99,
            body: "hello".to_string(),
            reply_to_message_id: None,
            created_at_micros: 0,
        };
        let context = TidepoolInboundContext {
            auto_subscribed_dm_first_message: false,
        };
        assert_eq!(render_inbound_content(&message, &context), "hello");
    }

    #[test]
    fn render_inbound_content_adds_unsubscribe_guidance_for_first_auto_dm_message() {
        let message = TidepoolInboundMessage {
            domain_id: 42,
            domain_title: "DM".to_string(),
            domain_slug: "".to_string(),
            message_id: 7,
            domain_sequence: 1,
            author_account_id: 99,
            body: "hello".to_string(),
            reply_to_message_id: None,
            created_at_micros: 0,
        };
        let context = TidepoolInboundContext {
            auto_subscribed_dm_first_message: true,
        };
        let rendered = render_inbound_content(&message, &context);
        assert!(rendered.contains("auto-subscribed"));
        assert!(rendered.contains("tidepool_unsubscribe_domain({\"domain_id\":42})"));
        assert!(rendered.ends_with("\n\nhello"));
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
    fn backoff_doubles_until_max() {
        assert_eq!(INITIAL_RECONNECT_DELAY, Duration::from_secs(1));
        assert_eq!(MAX_RECONNECT_DELAY, Duration::from_secs(60));

        let mut d = INITIAL_RECONNECT_DELAY;
        // 1 → 2 → 4 → 8 → 16 → 32 → 60 (capped) → 60
        assert_eq!(next_backoff(d), Duration::from_secs(2));
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(4));
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(8));
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(16));
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(32));
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(60)); // capped
        d = next_backoff(d);
        assert_eq!(next_backoff(d), Duration::from_secs(60)); // stays capped
    }

    #[test]
    fn healthy_connection_threshold_is_reasonable() {
        // Connections lasting 30+ seconds should reset backoff
        assert_eq!(CONNECTION_HEALTHY_THRESHOLD, Duration::from_secs(30));
    }

    #[test]
    fn self_echo_detects_matching_account_ids() {
        assert!(is_self_echo(42, 42));
        assert!(is_self_echo(1, 1));
        assert!(is_self_echo(0, 0));
    }

    #[test]
    fn self_echo_allows_other_agents() {
        assert!(!is_self_echo(42, 99));
        assert!(!is_self_echo(1, 2));
        assert!(!is_self_echo(100, 200));
    }
}
