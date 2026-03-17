use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use spacetimedb_sdk::{DbContext as _, Table as _};
use tokio::sync::{Mutex, mpsc};

use crate::generated::tidepool::{
    AccountLookup, DbConnection, DomainKind, DomainRole, MyAccountTableAccess,
    MySubscribedMessagesTableAccess, MySubscriptionsTableAccess, SubscriptionLookup,
    add_domain_member, create_dm, create_domain, post_message, subscribe_domain,
    unsubscribe_domain,
};

const DEFAULT_BASE_URL: &str = "https://spacetimedb.com";
const DEFAULT_BATCH_WINDOW_SECONDS: u32 = 30;
const DEFAULT_AGENT_ID: &str = "default";

#[derive(Debug, Clone)]
pub struct TidepoolConfig {
    pub agent_id: String,
    pub handle: String,
    pub base_url: String,
    pub database: String,
    pub token_path: PathBuf,
    pub seed_domain_ids: Vec<u64>,
    pub emit_self_messages: bool,
    pub batch_window_seconds: u32,
}

impl TidepoolConfig {
    pub fn from_env() -> Option<Self> {
        let database = std::env::var("TIDEPOOL_DATABASE").ok()?;
        let handle = std::env::var("TIDEPOOL_HANDLE").ok()?;
        let token_path = std::env::var("TIDEPOOL_TOKEN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".betterclaw/tidepool_token"));
        Some(Self {
            agent_id: std::env::var("TIDEPOOL_AGENT_ID")
                .unwrap_or_else(|_| DEFAULT_AGENT_ID.to_string()),
            handle,
            base_url: std::env::var("TIDEPOOL_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            database,
            token_path,
            seed_domain_ids: parse_seed_domains("TIDEPOOL_SEED_DOMAIN_IDS"),
            emit_self_messages: parse_bool_env("TIDEPOOL_EMIT_SELF_MESSAGES"),
            batch_window_seconds: std::env::var("TIDEPOOL_BATCH_WINDOW_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(DEFAULT_BATCH_WINDOW_SECONDS),
        })
    }

    pub fn token_exists(&self) -> bool {
        self.token_path.is_file()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TidepoolInboundMessage {
    pub domain_id: u64,
    pub domain_title: String,
    pub domain_slug: String,
    pub message_id: u64,
    pub domain_sequence: u64,
    pub author_account_id: u64,
    pub body: String,
    pub reply_to_message_id: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TidepoolBootstrapOutcome {
    pub account_id: u64,
    pub handle: String,
    pub token_path: PathBuf,
    pub subscribed_domain_ids: Vec<u64>,
}

#[derive(Debug)]
enum TidepoolClientEvent {
    Message(TidepoolInboundMessage),
    Disconnected(String),
}

struct TidepoolClientInner {
    connection: Arc<DbConnection>,
    receiver: Mutex<mpsc::UnboundedReceiver<TidepoolClientEvent>>,
    run_loop: Mutex<Option<tokio::task::JoinHandle<spacetimedb_sdk::Result<()>>>>,
    bootstrap: TidepoolBootstrapOutcome,
    attach_baseline_sequences: HashMap<u64, u64>,
}

#[derive(Clone)]
pub struct TidepoolClient {
    inner: Arc<TidepoolClientInner>,
}

impl TidepoolClient {
    pub async fn connect(config: TidepoolConfig) -> Result<Self> {
        let token = load_token(&config.token_path)?
            .ok_or_else(|| anyhow!("Tidepool token missing at {}", config.token_path.display()))?;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let disconnect_tx = event_tx.clone();

        let connection = Arc::new(
            DbConnection::builder()
                .with_uri(config.base_url.clone())
                .with_database_name(config.database.clone())
                .with_token(Some(token))
                .on_disconnect(move |_ctx, error| {
                    let reason = error
                        .map(|item| item.to_string())
                        .unwrap_or_else(|| "Tidepool connection closed".to_string());
                    let _ = disconnect_tx.send(TidepoolClientEvent::Disconnected(reason));
                })
                .build()
                .context("building Tidepool live client connection")?,
        );

        register_callbacks(&connection, &event_tx, config.emit_self_messages);

        let _subscription = connection.subscription_builder().subscribe([
            "SELECT * FROM my_account",
            "SELECT * FROM my_subscriptions",
            "SELECT * FROM my_subscribed_messages",
        ]);

        let run_connection = Arc::clone(&connection);
        let run_loop = tokio::spawn(async move { run_connection.run_async().await });

        let bootstrap = bootstrap_account(&connection, &config).await?;
        let attach_baseline_sequences = current_attach_baseline(&connection);

        Ok(Self {
            inner: Arc::new(TidepoolClientInner {
                connection,
                receiver: Mutex::new(event_rx),
                run_loop: Mutex::new(Some(run_loop)),
                bootstrap,
                attach_baseline_sequences,
            }),
        })
    }

    pub fn bootstrap_outcome(&self) -> &TidepoolBootstrapOutcome {
        &self.inner.bootstrap
    }

    pub fn attach_baseline_sequences(&self) -> &HashMap<u64, u64> {
        &self.inner.attach_baseline_sequences
    }

    pub fn account(&self) -> Option<AccountLookup> {
        self.inner.connection.db.my_account().iter().next()
    }

    pub fn subscriptions(&self) -> Vec<SubscriptionLookup> {
        let mut subscriptions = self
            .inner
            .connection
            .db
            .my_subscriptions()
            .iter()
            .collect::<Vec<_>>();
        subscriptions.sort_by_key(|item| item.domain_id);
        subscriptions
    }

    pub async fn recv(&self) -> Option<Result<TidepoolInboundMessage>> {
        let mut receiver = self.inner.receiver.lock().await;
        match receiver.recv().await {
            Some(TidepoolClientEvent::Message(message)) => Some(Ok(message)),
            Some(TidepoolClientEvent::Disconnected(reason)) => Some(Err(anyhow!(reason))),
            None => None,
        }
    }

    pub fn post_message(
        &self,
        domain_id: u64,
        body: impl Into<String>,
        reply_to_message_id: Option<u64>,
    ) -> Result<()> {
        self.inner
            .connection
            .reducers
            .post_message(domain_id, body.into(), reply_to_message_id)
            .context("posting Tidepool message")
    }

    pub async fn subscribe_domain(
        &self,
        domain_id: u64,
        batch_window_seconds: u32,
    ) -> Result<Vec<SubscriptionLookup>> {
        self.inner
            .connection
            .reducers
            .subscribe_domain(domain_id, batch_window_seconds)
            .with_context(|| format!("subscribing to Tidepool domain {domain_id}"))?;
        self.wait_for_subscription_state(domain_id, true).await?;
        Ok(self.subscriptions())
    }

    pub async fn unsubscribe_domain(&self, domain_id: u64) -> Result<Vec<SubscriptionLookup>> {
        self.inner
            .connection
            .reducers
            .unsubscribe_domain(domain_id)
            .with_context(|| format!("unsubscribing from Tidepool domain {domain_id}"))?;
        self.wait_for_subscription_state(domain_id, false).await?;
        Ok(self.subscriptions())
    }

    pub fn create_domain(
        &self,
        kind: DomainKind,
        slug: impl Into<String>,
        title: impl Into<String>,
        message_char_limit: u16,
    ) -> Result<()> {
        self.inner
            .connection
            .reducers
            .create_domain(kind, slug.into(), title.into(), message_char_limit)
            .context("creating Tidepool domain")
    }

    pub fn add_domain_member(
        &self,
        domain_id: u64,
        account_id: u64,
        role: DomainRole,
    ) -> Result<()> {
        self.inner
            .connection
            .reducers
            .add_domain_member(domain_id, account_id, role)
            .context("adding Tidepool domain member")
    }

    pub fn create_dm(&self, recipient_account_ids: Vec<u64>, title: impl Into<String>) -> Result<()> {
        self.inner
            .connection
            .reducers
            .create_dm(recipient_account_ids, title.into())
            .context("creating Tidepool DM")
    }

    async fn wait_for_subscription_state(&self, domain_id: u64, present: bool) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let currently_present = self
                .inner
                .connection
                .db
                .my_subscriptions()
                .iter()
                .any(|item| item.domain_id == domain_id);
            if currently_present == present {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                let target_state = if present {
                    "subscription to appear"
                } else {
                    "subscription to disappear"
                };
                return Err(anyhow!(
                    "Timed out waiting for Tidepool domain {domain_id} {target_state}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn read_messages(
        &self,
        domain_id: Option<u64>,
        limit: usize,
    ) -> Vec<TidepoolInboundMessage> {
        let mut messages: Vec<TidepoolInboundMessage> = self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| domain_id.map_or(true, |id| row.domain_id == id))
            .map(|row| {
                let subscription = self
                    .inner
                    .connection
                    .db
                    .my_subscriptions()
                    .iter()
                    .find(|s| s.domain_id == row.domain_id);
                TidepoolInboundMessage {
                    domain_id: row.domain_id,
                    domain_title: subscription
                        .as_ref()
                        .map(|s| s.title.clone())
                        .unwrap_or_else(|| format!("Domain {}", row.domain_id)),
                    domain_slug: subscription
                        .as_ref()
                        .map(|s| s.slug.clone())
                        .unwrap_or_default(),
                    message_id: row.message_id,
                    domain_sequence: row.domain_sequence,
                    author_account_id: row.author_account_id,
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                }
            })
            .collect();
        messages.sort_by(|a, b| {
            a.domain_id
                .cmp(&b.domain_id)
                .then(a.domain_sequence.cmp(&b.domain_sequence))
        });
        if messages.len() > limit {
            messages = messages[messages.len() - limit..].to_vec();
        }
        messages
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.connection.disconnect();
        if let Some(run_loop) = self.inner.run_loop.lock().await.take() {
            run_loop.abort();
        }
    }
}

fn register_callbacks(
    connection: &Arc<DbConnection>,
    event_tx: &mpsc::UnboundedSender<TidepoolClientEvent>,
    emit_self_messages: bool,
) {
    let message_tx = event_tx.clone();
    connection
        .db
        .my_subscribed_messages()
        .on_insert(move |ctx, row| {
            let account = ctx.db.my_account().iter().next();
            if !emit_self_messages
                && account.as_ref().map(|item| item.account_id) == Some(row.author_account_id)
            {
                return;
            }

            let subscription = ctx
                .db
                .my_subscriptions()
                .iter()
                .find(|item| item.domain_id == row.domain_id);
            let message = TidepoolInboundMessage {
                domain_id: row.domain_id,
                domain_title: subscription
                    .as_ref()
                    .map(|item| item.title.clone())
                    .unwrap_or_else(|| format!("Domain {}", row.domain_id)),
                domain_slug: subscription
                    .as_ref()
                    .map(|item| item.slug.clone())
                    .unwrap_or_default(),
                message_id: row.message_id,
                domain_sequence: row.domain_sequence,
                author_account_id: row.author_account_id,
                body: row.body.clone(),
                reply_to_message_id: row.reply_to_message_id,
            };
            let _ = message_tx.send(TidepoolClientEvent::Message(message));
        });
}

fn current_attach_baseline(connection: &Arc<DbConnection>) -> HashMap<u64, u64> {
    let mut baseline = HashMap::new();
    for row in connection.db.my_subscribed_messages().iter() {
        baseline
            .entry(row.domain_id)
            .and_modify(|sequence| {
                if row.domain_sequence > *sequence {
                    *sequence = row.domain_sequence;
                }
            })
            .or_insert(row.domain_sequence);
    }
    baseline
}

async fn bootstrap_account(
    connection: &Arc<DbConnection>,
    config: &TidepoolConfig,
) -> Result<TidepoolBootstrapOutcome> {
    let account = wait_for_account(connection, Duration::from_secs(10))
        .await?
        .ok_or_else(|| {
            anyhow!(
                "Tidepool token exists but no account is visible for handle '{}'",
                config.handle
            )
        })?;

    let mut subscribed_domain_ids = connection
        .db
        .my_subscriptions()
        .iter()
        .map(|item| item.domain_id)
        .collect::<Vec<_>>();

    for domain_id in &config.seed_domain_ids {
        if subscribed_domain_ids.iter().any(|item| item == domain_id) {
            continue;
        }
        tracing::info!(
            domain_id = *domain_id,
            "Subscribing Tidepool agent to seed domain"
        );
        connection
            .reducers
            .subscribe_domain(*domain_id, config.batch_window_seconds)
            .with_context(|| format!("subscribing to Tidepool domain {}", domain_id))?;
        subscribed_domain_ids.push(*domain_id);
    }

    Ok(TidepoolBootstrapOutcome {
        account_id: account.account_id,
        handle: account.handle,
        token_path: config.token_path.clone(),
        subscribed_domain_ids,
    })
}

async fn wait_for_account(
    connection: &Arc<DbConnection>,
    timeout: Duration,
) -> Result<Option<crate::generated::tidepool::AccountLookup>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(account) = connection.db.my_account().iter().next() {
            return Ok(Some(account));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn load_token(path: &PathBuf) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let token = fs::read_to_string(path)
        .with_context(|| format!("reading Tidepool token from {}", path.display()))?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Ok(None);
    }
    Ok(Some(token))
}

fn parse_seed_domains(name: &str) -> Vec<u64> {
    std::env::var(name)
        .ok()
        .map(|value| {
            value
                .split(',')
                .filter_map(|item| item.trim().parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_bool_env(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}
