use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use spacetimedb_sdk::{DbContext as _, Table as _};
use tokio::sync::mpsc;

use crate::generated::tidepool::{
    DbConnection, MyAccountTableAccess, MySubscribedMessagesTableAccess, MySubscriptionsTableAccess,
    post_message, subscribe_domain,
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

pub struct TidepoolClient {
    connection: Arc<DbConnection>,
    receiver: mpsc::UnboundedReceiver<TidepoolClientEvent>,
    _run_loop: tokio::task::JoinHandle<spacetimedb_sdk::Result<()>>,
    bootstrap: TidepoolBootstrapOutcome,
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

        let _subscription = connection
            .subscription_builder()
            .subscribe([
                "SELECT * FROM my_account",
                "SELECT * FROM my_subscriptions",
                "SELECT * FROM my_subscribed_messages",
            ]);

        let run_connection = Arc::clone(&connection);
        let run_loop = tokio::spawn(async move { run_connection.run_async().await });

        let bootstrap = bootstrap_account(&connection, &config).await?;

        Ok(Self {
            connection,
            receiver: event_rx,
            _run_loop: run_loop,
            bootstrap,
        })
    }

    pub fn bootstrap_outcome(&self) -> &TidepoolBootstrapOutcome {
        &self.bootstrap
    }

    pub async fn recv(&mut self) -> Option<Result<TidepoolInboundMessage>> {
        match self.receiver.recv().await {
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
        self.connection
            .reducers
            .post_message(domain_id, body.into(), reply_to_message_id)
            .context("posting Tidepool message")
    }
}

fn register_callbacks(
    connection: &Arc<DbConnection>,
    event_tx: &mpsc::UnboundedSender<TidepoolClientEvent>,
    emit_self_messages: bool,
) {
    let message_tx = event_tx.clone();
    connection.db.my_subscribed_messages().on_insert(move |ctx, row| {
        let account = ctx.db.my_account().iter().next();
        if !emit_self_messages && account.as_ref().map(|item| item.account_id) == Some(row.author_account_id)
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
        tracing::info!(domain_id = *domain_id, "Subscribing Tidepool agent to seed domain");
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
