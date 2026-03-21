use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use spacetimedb_sdk::{DbContext as _, Table as _};
use tokio::sync::{Mutex, mpsc};

use crate::generated::tidepool::{
    AccountLookup, DbConnection, DmLookup, DomainKind, DomainMember, DomainMemberTableAccess,
    DomainRole, MyAccountTableAccess, MyDmDomainsTableAccess, MySubscribedMessagesTableAccess,
    MySubscriptionsTableAccess, SubscriptionLookup, add_domain_member, create_dm, create_domain,
    join_domain, post_message, remove_domain_member, subscribe_domain, unsubscribe_domain,
};

const DEFAULT_BASE_URL: &str = "https://spacetimedb.com";
const DEFAULT_AGENT_ID: &str = "default";

#[derive(Debug, Clone)]
pub struct TidepoolConfig {
    pub agent_id: String,
    pub handle: String,
    pub base_url: String,
    pub database: String,
    pub token_path: PathBuf,
    pub emit_self_messages: bool,
}

impl TidepoolConfig {
    pub fn from_env() -> Option<Self> {
        let database = std::env::var("TIDEPOOL_DATABASE").ok()?;
        let handle = std::env::var("TIDEPOOL_HANDLE").ok()?;
        let token_path = std::env::var("TIDEPOOL_TOKEN_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_tidepool_token_path());
        Some(Self {
            agent_id: std::env::var("TIDEPOOL_AGENT_ID")
                .unwrap_or_else(|_| DEFAULT_AGENT_ID.to_string()),
            handle,
            base_url: std::env::var("TIDEPOOL_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            database,
            token_path,
            emit_self_messages: parse_bool_env("TIDEPOOL_EMIT_SELF_MESSAGES"),
        })
    }

    pub fn token_exists(&self) -> bool {
        self.token_path.is_file()
    }
}

fn default_tidepool_token_path() -> PathBuf {
    dirs::home_dir()
        .map(|path| path.join(".betterclaw").join("tidepool_token"))
        .unwrap_or_else(|| PathBuf::from(".betterclaw/tidepool_token"))
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TidepoolInboundMessage {
    pub domain_id: u64,
    pub domain_title: String,
    pub domain_slug: String,
    pub message_id: u64,
    pub domain_sequence: u64,
    pub author_account_id: u64,
    pub author_handle: String,
    pub body: String,
    pub reply_to_message_id: Option<u64>,
    /// Message creation time as microseconds since Unix epoch.
    pub created_at_micros: i64,
}

#[derive(Debug, Clone)]
pub struct TidepoolInboundContext {
    pub auto_subscribed_dm_first_message: bool,
}

/// Presence information for a single agent/account based on recent message activity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentPresenceEntry {
    pub account_id: u64,
    pub last_message_id: u64,
    pub last_domain_id: u64,
    pub last_domain_title: String,
    pub message_count: usize,
    pub active_domain_ids: Vec<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentHealthEntry {
    pub account_id: u64,
    pub last_message_id: u64,
    pub last_domain_id: u64,
    pub last_domain_title: String,
    pub message_count: usize,
    pub active_domain_ids: Vec<u64>,
    /// Seconds since the agent's last message, or None if no messages found.
    pub seconds_since_last_message: Option<f64>,
    /// Human-readable health assessment: "active" (<5min), "idle" (<30min),
    /// "stale" (<2h), "silent" (>2h), or "unknown" (no messages).
    pub health_status: String,
}

/// Account information resolved from the Tidepool `account` table via HTTP API.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountEntry {
    pub account_id: u64,
    pub handle: String,
    pub status: String,
}

fn build_account_lookup_sql(handle: Option<&str>, account_id: Option<u64>) -> String {
    let mut sql = String::from("SELECT account_id, handle, status FROM account");
    let mut clauses = Vec::new();
    if let Some(handle) = handle {
        clauses.push(format!("handle = '{}'", handle.replace('\'', "''")));
    }
    if let Some(account_id) = account_id {
        clauses.push(format!("account_id = {account_id}"));
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" LIMIT 50");
    sql
}

fn parse_account_rows(body: &Value) -> Result<Vec<AccountEntry>> {
    let statements = body.as_array().ok_or_else(|| {
        anyhow!("expected Tidepool SQL response to be an array of statement results")
    })?;
    let first = statements
        .first()
        .ok_or_else(|| anyhow!("Tidepool SQL response contained no statement results"))?;
    let rows = first
        .get("rows")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Tidepool SQL statement result missing 'rows' array"))?;

    let mut entries = Vec::new();
    for row in rows {
        let arr = row
            .as_array()
            .ok_or_else(|| anyhow!("expected SQL row to be an array"))?;
        if arr.len() < 3 {
            continue;
        }
        let account_id = arr[0].as_u64().unwrap_or(0);
        let handle = arr[1].as_str().unwrap_or("").to_string();
        let status = match arr[2].as_str() {
            Some(status) => status.to_string(),
            None => match arr[2].get(0).and_then(|v| v.as_u64()) {
                Some(0) => "active".to_string(),
                Some(1) => "suspended".to_string(),
                _ => "unknown".to_string(),
            },
        };
        entries.push(AccountEntry {
            account_id,
            handle,
            status,
        });
    }

    Ok(entries)
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
            "SELECT * FROM my_dm_domains",
            "SELECT * FROM domain_member",
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

    pub fn dm_domains(&self) -> Vec<DmLookup> {
        let mut dm_domains = self
            .inner
            .connection
            .db
            .my_dm_domains()
            .iter()
            .collect::<Vec<_>>();
        dm_domains.sort_by_key(|item| item.domain_id);
        dm_domains
    }

    pub fn inbound_context(&self, message: &TidepoolInboundMessage) -> TidepoolInboundContext {
        let auto_subscribed_dm_first_message = message.domain_sequence == 1
            && self
                .subscriptions()
                .into_iter()
                .find(|item| item.domain_id == message.domain_id)
                .map(|item| item.auto_subscribed)
                .unwrap_or(false)
            && self
                .dm_domains()
                .into_iter()
                .any(|item| item.domain_id == message.domain_id);
        TidepoolInboundContext {
            auto_subscribed_dm_first_message,
        }
    }

    pub fn domain_members(&self, domain_id: Option<u64>) -> Vec<DomainMember> {
        let mut members: Vec<DomainMember> = self
            .inner
            .connection
            .db
            .domain_member()
            .iter()
            .filter(|m| domain_id.map_or(true, |id| m.domain_id == id))
            .collect();
        members.sort_by_key(|m| (m.domain_id, m.account_id));
        members
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

    /// Self-join a public domain. Unlike `add_domain_member` (which requires owner
    /// privileges), this lets an agent join any public domain on its own.
    pub fn join_domain(&self, domain_id: u64) -> Result<()> {
        self.inner
            .connection
            .reducers
            .join_domain(domain_id)
            .with_context(|| format!("joining Tidepool domain {domain_id}"))
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

    pub fn remove_domain_member(&self, domain_id: u64, account_id: u64) -> Result<()> {
        self.inner
            .connection
            .reducers
            .remove_domain_member(domain_id, account_id)
            .context("removing Tidepool domain member")
    }

    pub fn create_dm(
        &self,
        recipient_account_ids: Vec<u64>,
        title: impl Into<String>,
    ) -> Result<()> {
        self.inner
            .connection
            .reducers
            .create_dm(recipient_account_ids, title.into())
            .context("creating Tidepool DM")
    }

    /// Send a message to an agent by handle. Finds or creates a DM with that agent and posts.
    /// Returns (domain_id, created_dm, message_body_echo).
    pub async fn message_agent(
        &self,
        target_handle: &str,
        body: impl Into<String>,
    ) -> Result<(u64, bool, String)> {
        let body = body.into();
        let own_account = self
            .account()
            .ok_or_else(|| anyhow!("no account on shared Tidepool connection"))?;

        // Resolve target account by handle
        let accounts = self.resolve_accounts(Some(target_handle), None).await?;
        let target = accounts
            .iter()
            .find(|a| a.handle.eq_ignore_ascii_case(target_handle) && a.status == "active")
            .ok_or_else(|| {
                anyhow!("no active Tidepool account found for handle '{target_handle}'")
            })?;
        let target_account_id = target.account_id;

        if target_account_id == own_account.account_id {
            return Err(anyhow!("cannot send a DM to yourself"));
        }

        // Check for existing DM with this agent
        let existing_dm = self.dm_domains().into_iter().find(|dm| {
            dm.participant_account_ids.contains(&target_account_id)
                && dm.participant_account_ids.contains(&own_account.account_id)
        });

        let (domain_id, created_dm) = if let Some(dm) = existing_dm {
            (dm.domain_id, false)
        } else {
            // Create DM — title is derived from sorted handles
            let mut handles = vec![own_account.handle.clone(), target.handle.clone()];
            handles.sort();
            let title = handles.join(" ↔ ");

            let recipient_ids = vec![target_account_id];
            self.create_dm(recipient_ids, &title)?;

            // Wait for the DM to appear in local db (up to 5 seconds)
            let domain_id = self
                .wait_for_dm_with_participants(
                    &[own_account.account_id, target_account_id],
                    Duration::from_secs(5),
                )
                .await?;
            (domain_id, true)
        };

        // Post the message
        self.post_message(domain_id, &body, None)?;

        Ok((domain_id, created_dm, body))
    }

    /// Poll local db until a DM domain containing the given participants appears.
    async fn wait_for_dm_with_participants(
        &self,
        participant_account_ids: &[u64],
        timeout: Duration,
    ) -> Result<u64> {
        let deadline = std::time::Instant::now() + timeout;
        let mut expected = participant_account_ids.to_vec();
        expected.sort_unstable();

        loop {
            let found = self.dm_domains().into_iter().find(|dm| {
                let mut ids = dm.participant_account_ids.clone();
                ids.sort_unstable();
                ids == expected
            });
            if let Some(dm) = found {
                return Ok(dm.domain_id);
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for DM domain with participants {:?} to appear",
                    participant_account_ids
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait_for_subscription_state(&self, domain_id: u64, present: bool) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let currently_present = self.local_subscription_state(domain_id);
            if currently_present == present {
                return Ok(());
            }
            if let Ok(server_present) = self.subscription_state_via_http(domain_id).await {
                if server_present == present {
                    return Ok(());
                }
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

    fn local_subscription_state(&self, domain_id: u64) -> bool {
        self.inner
            .connection
            .db
            .my_subscriptions()
            .iter()
            .any(|item| item.domain_id == domain_id)
    }

    async fn subscription_state_via_http(&self, domain_id: u64) -> Result<bool> {
        let sql =
            format!("SELECT domain_id FROM my_subscriptions WHERE domain_id = {domain_id} LIMIT 1");
        let body = self.run_http_sql(&sql).await?;
        let statements = body.as_array().ok_or_else(|| {
            anyhow!("expected Tidepool SQL response to be an array of statement results")
        })?;
        let first = statements
            .first()
            .ok_or_else(|| anyhow!("Tidepool SQL response contained no statement results"))?;
        let rows = first
            .get("rows")
            .and_then(|value| value.as_array())
            .ok_or_else(|| anyhow!("Tidepool SQL statement result missing 'rows' array"))?;
        Ok(!rows.is_empty())
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
                    author_handle: row.author_handle.clone(),
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                    created_at_micros: row.created_at.to_micros_since_unix_epoch(),
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

    /// Read messages with optional domain and after_message_id filtering.
    /// Returns up to `limit` messages matching the filters.
    pub fn read_messages_filtered(
        &self,
        domain_id: Option<u64>,
        after_message_id: Option<u64>,
        limit: usize,
    ) -> Vec<TidepoolInboundMessage> {
        let mut messages: Vec<TidepoolInboundMessage> = self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| domain_id.map_or(true, |id| row.domain_id == id))
            .filter(|row| after_message_id.map_or(true, |after| row.message_id > after))
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
                    author_handle: row.author_handle.clone(),
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                    created_at_micros: row.created_at.to_micros_since_unix_epoch(),
                }
            })
            .collect();
        messages.sort_by(|a, b| {
            a.domain_id
                .cmp(&b.domain_id)
                .then(a.message_id.cmp(&b.message_id))
        });
        // For incremental reads (after_message_id set), take the first `limit` (oldest new messages).
        // For regular reads, take the last `limit` (most recent).
        if after_message_id.is_some() {
            messages.truncate(limit);
        } else if messages.len() > limit {
            messages = messages[messages.len() - limit..].to_vec();
        }
        messages
    }

    /// Search messages by content. Supports optional domain_id, author_account_id,
    /// and after_message_id filters. Case-insensitive substring match on body.
    /// Retrieve all replies to a specific message across subscribed domains.
    /// Returns messages where reply_to_message_id matches the given message_id.
    pub fn get_thread(
        &self,
        root_message_id: u64,
        domain_id: Option<u64>,
        limit: usize,
    ) -> Vec<TidepoolInboundMessage> {
        let mut messages: Vec<TidepoolInboundMessage> = self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| row.reply_to_message_id == Some(root_message_id))
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
                    author_handle: row.author_handle.clone(),
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                    created_at_micros: row.created_at.to_micros_since_unix_epoch(),
                }
            })
            .collect();
        messages.sort_by_key(|m| m.message_id);
        messages.truncate(limit);
        messages
    }

    pub fn search_messages(
        &self,
        query: &str,
        domain_id: Option<u64>,
        author_account_id: Option<u64>,
        after_message_id: Option<u64>,
        limit: usize,
    ) -> Vec<TidepoolInboundMessage> {
        let query_lower = query.to_lowercase();
        let mut messages: Vec<TidepoolInboundMessage> = self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| domain_id.map_or(true, |id| row.domain_id == id))
            .filter(|row| author_account_id.map_or(true, |id| row.author_account_id == id))
            .filter(|row| after_message_id.map_or(true, |after| row.message_id > after))
            .filter(|row| row.body.to_lowercase().contains(&query_lower))
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
                    author_handle: row.author_handle.clone(),
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                    created_at_micros: row.created_at.to_micros_since_unix_epoch(),
                }
            })
            .collect();
        messages.sort_by(|a, b| b.message_id.cmp(&a.message_id));
        messages.truncate(limit);
        messages
    }

    /// Infer agent presence from recent message activity in subscribed domains.
    ///
    /// Analyzes the last `window_size` messages across all (or a specific) subscribed
    /// domains to determine which accounts have been active recently. Returns a
    /// presence entry for each unique author with their last activity details.
    pub fn agent_presence(
        &self,
        domain_id: Option<u64>,
        window_size: usize,
    ) -> Vec<AgentPresenceEntry> {
        use std::collections::HashMap;

        let mut activity: HashMap<u64, AgentPresenceEntry> = HashMap::new();

        for row in self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| domain_id.map_or(true, |id| row.domain_id == id))
        {
            let entry =
                activity
                    .entry(row.author_account_id)
                    .or_insert_with(|| AgentPresenceEntry {
                        account_id: row.author_account_id,
                        last_message_id: 0,
                        last_domain_id: 0,
                        last_domain_title: String::new(),
                        message_count: 0,
                        active_domain_ids: Vec::new(),
                    });

            entry.message_count += 1;
            if row.message_id > entry.last_message_id {
                entry.last_message_id = row.message_id;
                entry.last_domain_id = row.domain_id;
                let subscription = self
                    .inner
                    .connection
                    .db
                    .my_subscriptions()
                    .iter()
                    .find(|s| s.domain_id == row.domain_id);
                entry.last_domain_title = subscription
                    .as_ref()
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| format!("Domain {}", row.domain_id));
            }
            if !entry.active_domain_ids.contains(&row.domain_id) {
                entry.active_domain_ids.push(row.domain_id);
            }
        }

        let mut entries: Vec<AgentPresenceEntry> = activity.into_values().collect();
        entries.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id));
        if entries.len() > window_size {
            entries.truncate(window_size);
        }
        entries
    }

    /// Resolve accounts from the Tidepool `account` table via the HTTP API.
    ///
    /// Returns account entries matching the given filter. At least one of `handle` or
    /// `account_id` must be provided. Uses the same base_url, database, and token as
    /// the SDK connection for authentication.
    pub async fn resolve_accounts(
        &self,
        handle: Option<&str>,
        account_id: Option<u64>,
    ) -> Result<Vec<AccountEntry>> {
        let sql = build_account_lookup_sql(handle, account_id);
        let body = self.run_http_sql(&sql).await?;
        parse_account_rows(&body)
    }

    async fn run_http_sql(&self, sql: &str) -> Result<Value> {
        let token = fs::read_to_string(&self.inner.bootstrap.token_path)
            .context("reading Tidepool token for HTTP API call")?
            .trim()
            .to_string();

        let config = TidepoolConfig::from_env();
        let base_url = config
            .as_ref()
            .map(|c| c.base_url.clone())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let database = config
            .as_ref()
            .map(|c| c.database.clone())
            .unwrap_or_else(|| "tidepool-dev".to_string());

        let url = format!(
            "{}/v1/database/{}/sql",
            base_url.trim_end_matches('/'),
            database
        );

        let response = reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "text/plain")
            .body(sql.to_string())
            .send()
            .await
            .context("sending Tidepool HTTP API query")?;

        let status = response.status();
        let response_text = response
            .text()
            .await
            .context("reading Tidepool HTTP API response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "Tidepool HTTP API query failed with {}: {}",
                status,
                response_text
            ));
        }
        serde_json::from_str(&response_text).context("parsing Tidepool HTTP API response")
    }

    pub async fn shutdown(&self) {
        let _ = self.inner.connection.disconnect();
        // Do NOT abort the run_loop task. After disconnect(), the SDK's
        // run_async() will exit on its own. Aborting it mid-cleanup leaves
        // internal WebSocket timers dangling, which panic when they fire
        // during the next connection attempt ("Tokio context is being
        // shutdown"). Simply drop the JoinHandle and let the reconnect
        // delay give the old task time to finish and release sockets.
        let _ = self.inner.run_loop.lock().await.take();
    }

    /// Find messages that mention a specific handle via `@handle` pattern.
    ///
    /// Parses message bodies for `@<handle>` mentions (case-insensitive, word-boundary-aware).
    /// Returns messages sorted by most recent first. Useful for agents to find coordination
    /// messages directed at them without scanning all domain messages.
    pub fn find_mentions(
        &self,
        handle: &str,
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
            .filter(|row| body_mentions_handle(&row.body, handle))
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
                    author_handle: row.author_handle.clone(),
                    body: row.body.clone(),
                    reply_to_message_id: row.reply_to_message_id,
                    created_at_micros: row.created_at.to_micros_since_unix_epoch(),
                }
            })
            .collect();
        messages.sort_by(|a, b| b.message_id.cmp(&a.message_id));
        messages.truncate(limit);
        messages
    }

    /// Compute health status for agents based on Tidepool message activity.
    /// Unlike `agent_presence`, this includes time-since-last-message analysis
    /// and a human-readable health status assessment.
    pub fn agent_health(
        &self,
        account_id: Option<u64>,
        domain_id: Option<u64>,
        window_size: usize,
    ) -> Vec<AgentHealthEntry> {
        let now_micros = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);

        let mut activity: HashMap<u64, AgentHealthEntry> = HashMap::new();

        for row in self
            .inner
            .connection
            .db
            .my_subscribed_messages()
            .iter()
            .filter(|row| domain_id.map_or(true, |id| row.domain_id == id))
            .filter(|row| account_id.map_or(true, |id| row.author_account_id == id))
        {
            let entry = activity
                .entry(row.author_account_id)
                .or_insert_with(|| AgentHealthEntry {
                    account_id: row.author_account_id,
                    last_message_id: 0,
                    last_domain_id: 0,
                    last_domain_title: String::new(),
                    message_count: 0,
                    active_domain_ids: Vec::new(),
                    seconds_since_last_message: None,
                    health_status: "unknown".to_string(),
                });

            entry.message_count += 1;
            if row.message_id > entry.last_message_id {
                entry.last_message_id = row.message_id;
                entry.last_domain_id = row.domain_id;
                let subscription = self
                    .inner
                    .connection
                    .db
                    .my_subscriptions()
                    .iter()
                    .find(|s| s.domain_id == row.domain_id);
                entry.last_domain_title = subscription
                    .as_ref()
                    .map(|s| s.title.clone())
                    .unwrap_or_else(|| format!("Domain {}", row.domain_id));
                // Compute age from the last (most recent) message's timestamp.
                let msg_micros = row.created_at.to_micros_since_unix_epoch();
                let age_secs = (now_micros - msg_micros) as f64 / 1_000_000.0;
                entry.seconds_since_last_message = Some(age_secs.max(0.0));
                entry.health_status = if age_secs < 300.0 {
                    "active".to_string()
                } else if age_secs < 1800.0 {
                    "idle".to_string()
                } else if age_secs < 7200.0 {
                    "stale".to_string()
                } else {
                    "silent".to_string()
                };
            }
            if !entry.active_domain_ids.contains(&row.domain_id) {
                entry.active_domain_ids.push(row.domain_id);
            }
        }

        let mut entries: Vec<AgentHealthEntry> = activity.into_values().collect();
        entries.sort_by(|a, b| b.last_message_id.cmp(&a.last_message_id));
        if entries.len() > window_size {
            entries.truncate(window_size);
        }
        entries
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
                author_handle: row.author_handle.clone(),
                body: row.body.clone(),
                reply_to_message_id: row.reply_to_message_id,
                created_at_micros: row.created_at.to_micros_since_unix_epoch(),
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

    let subscribed_domain_ids = connection
        .db
        .my_subscriptions()
        .iter()
        .map(|item| item.domain_id)
        .collect::<Vec<_>>();

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

/// Check if a message body mentions a given handle via `@handle` pattern.
/// Case-insensitive, word-boundary-aware matching.
pub fn body_mentions_handle(body: &str, handle: &str) -> bool {
    let handle_lower = handle.to_ascii_lowercase();
    body.split_whitespace().any(|word| {
        let stripped =
            word.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '_' && c != '-');
        let mention = stripped.strip_prefix('@').unwrap_or("");
        mention.to_ascii_lowercase() == handle_lower
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mention_matches_simple_handle() {
        assert!(body_mentions_handle(
            "hey @buzz can you check this?",
            "buzz"
        ));
        assert!(body_mentions_handle("@horus please review", "horus"));
        assert!(body_mentions_handle("ping @chip", "chip"));
    }

    #[test]
    fn mention_is_case_insensitive() {
        assert!(body_mentions_handle("hey @BUZZ what's up", "buzz"));
        assert!(body_mentions_handle("@Horus look at this", "horus"));
        assert!(body_mentions_handle("ping @ChIP", "chip"));
    }

    #[test]
    fn mention_matches_with_punctuation() {
        assert!(body_mentions_handle("@buzz, this is for you", "buzz"));
        assert!(body_mentions_handle("(@horus)", "horus"));
        assert!(body_mentions_handle("@buzz.", "buzz"));
        assert!(body_mentions_handle("@buzz!", "buzz"));
    }

    #[test]
    fn mention_does_not_match_substring() {
        assert!(!body_mentions_handle("the buzzword is important", "buzz"));
        assert!(!body_mentions_handle("rebugging the system", "bug"));
    }

    #[test]
    fn mention_does_not_match_different_handle() {
        assert!(!body_mentions_handle("hey @chip", "buzz"));
        assert!(!body_mentions_handle("@horus check this", "chip"));
    }

    #[test]
    fn mention_handles_underscore_and_hyphen() {
        assert!(body_mentions_handle("hey @my_agent check this", "my_agent"));
        assert!(body_mentions_handle("ping @my-agent", "my-agent"));
    }

    #[test]
    fn mention_empty_body_returns_false() {
        assert!(!body_mentions_handle("", "buzz"));
        assert!(!body_mentions_handle("no mentions here", "buzz"));
    }

    #[test]
    fn default_tidepool_token_path_targets_betterclaw_home() {
        let path = default_tidepool_token_path();
        assert!(path.ends_with(".betterclaw/tidepool_token"));
    }

    #[test]
    fn parse_account_rows_accepts_statement_result_array() {
        let body = serde_json::json!([
            {
                "schema": { "elements": [] },
                "rows": [
                    [293210979062513665u64, "chip", [0, []]],
                    [42, "horus", [1, []]]
                ]
            }
        ]);

        let rows = parse_account_rows(&body).expect("rows should parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].handle, "chip");
        assert_eq!(rows[0].status, "active");
        assert_eq!(rows[1].status, "suspended");
    }

    #[test]
    fn parse_account_rows_accepts_string_statuses() {
        let body = serde_json::json!([
            {
                "rows": [
                    [293210979062513665u64, "chip", "active"]
                ]
            }
        ]);

        let rows = parse_account_rows(&body).expect("rows should parse");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].handle, "chip");
        assert_eq!(rows[0].status, "active");
    }

    #[test]
    fn parse_account_rows_rejects_legacy_object_shape() {
        let body = serde_json::json!({
            "rows": [[293210979062513665u64, "chip", [0, []]]]
        });

        let err = parse_account_rows(&body).expect_err("legacy shape should fail");
        assert!(
            err.to_string().contains("array of statement results"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_account_lookup_sql_omits_dummy_literal_where_clause() {
        let sql = build_account_lookup_sql(Some("chip"), None);
        assert_eq!(
            sql,
            "SELECT account_id, handle, status FROM account WHERE handle = 'chip' LIMIT 50"
        );
        assert!(!sql.contains("1=1"));
    }

    #[test]
    fn build_account_lookup_sql_escapes_quotes_and_combines_filters() {
        let sql = build_account_lookup_sql(Some("o'hara"), Some(42));
        assert_eq!(
            sql,
            "SELECT account_id, handle, status FROM account WHERE handle = 'o''hara' AND account_id = 42 LIMIT 50"
        );
    }
}
