use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde_json::json;

use crate::channel::{ChannelCursor, InboundEvent};
use crate::runtime::Runtime;
use crate::tidepool::{TidepoolClient, TidepoolConfig, TidepoolInboundMessage};

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

        let mut client = TidepoolClient::connect(self.config.clone())
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
            if outbound.trim().is_empty() {
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

            tracing::info!(
                domain_id = *domain_id,
                baseline_sequence = *baseline_sequence,
                current_cursor,
                "Seeding Tidepool cursor from attach snapshot baseline"
            );
            self.runtime
                .db()
                .upsert_cursor(&ChannelCursor {
                    channel: "tidepool".to_string(),
                    cursor_key,
                    cursor_value: baseline_sequence.to_string(),
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

#[cfg(test)]
mod tests {
    use super::tidepool_thread_key;
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
}
