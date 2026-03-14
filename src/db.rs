use std::io::{Read, Write};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use libsql::{Builder, Connection, Database, Rows, params};
use serde_json::Value;
use uuid::Uuid;

use crate::agent::Agent;
use crate::channel::{ChannelCursor, OutboundMessage};
use crate::event::{Event, EventKind};
use crate::model::{ModelTrace, TraceBlob, TraceDetail};
use crate::settings::{RetentionSettings, RuntimeSettings};
use crate::thread::Thread;
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

pub struct Db {
    database: Database,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceBlobPruneReport {
    pub pruned_blob_count: usize,
    pub reclaimed_bytes: i64,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let database = Builder::new_local(path).build().await?;
        let db = Self { database };
        db.migrate().await?;
        Ok(db)
    }

    fn connect(&self) -> Result<Connection> {
        Ok(self.database.connect()?)
    }

    async fn migrate(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS agents (
                id TEXT PRIMARY KEY,
                display_name TEXT NOT NULL,
                workspace_id TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                root TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS threads (
                id TEXT PRIMARY KEY,
                agent_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                external_thread_id TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(agent_id, channel, external_thread_id)
            );
            CREATE TABLE IF NOT EXISTS turns (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                status TEXT NOT NULL,
                user_message TEXT NOT NULL,
                assistant_message TEXT,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                turn_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS trace_blobs (
                id TEXT PRIMARY KEY,
                encoding TEXT NOT NULL,
                content_type TEXT NOT NULL,
                body BLOB NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS model_traces (
                id TEXT PRIMARY KEY,
                turn_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                model TEXT NOT NULL,
                request_started_at TEXT NOT NULL,
                request_completed_at TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_input_tokens INTEGER NOT NULL,
                cache_creation_input_tokens INTEGER NOT NULL,
                provider_request_id TEXT,
                tool_count INTEGER NOT NULL,
                tool_names TEXT NOT NULL,
                request_blob_id TEXT NOT NULL,
                response_blob_id TEXT NOT NULL,
                stream_blob_id TEXT,
                error_summary TEXT
            );
            CREATE TABLE IF NOT EXISTS channel_cursors (
                channel TEXT NOT NULL,
                cursor_key TEXT NOT NULL,
                cursor_value TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(channel, cursor_key)
            );
            CREATE TABLE IF NOT EXISTS outbound_messages (
                id TEXT PRIMARY KEY,
                turn_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                external_thread_id TEXT NOT NULL,
                content TEXT NOT NULL,
                metadata_json TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runtime_settings (
                agent_id TEXT PRIMARY KEY,
                model TEXT NOT NULL,
                system_prompt TEXT NOT NULL,
                temperature REAL NOT NULL,
                max_tokens INTEGER NOT NULL,
                stream INTEGER NOT NULL,
                allow_tools INTEGER NOT NULL,
                max_history_turns INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS retention_settings (
                agent_id TEXT PRIMARY KEY,
                trace_blob_retention_days INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
        "#,
        )
        .await?;
        self.add_column_if_missing(&conn, "outbound_messages", "metadata_json", "TEXT")
            .await?;
        Ok(())
    }

    async fn add_column_if_missing(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<()> {
        let pragma = format!("PRAGMA table_info({table})");
        let mut rows = conn.query(&pragma, params![]).await?;
        while let Some(row) = rows.next().await? {
            let existing: String = row.get(1)?;
            if existing == column {
                return Ok(());
            }
        }

        let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {definition}");
        conn.execute(&alter, params![]).await?;
        Ok(())
    }

    pub async fn seed_default_agent(&self, agent: &Agent, workspace: &Workspace) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO workspaces (id, root, created_at) VALUES (?, ?, ?)",
            params![
                workspace.id.clone(),
                workspace.root.display().to_string(),
                now.clone()
            ],
        )
        .await?;
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, display_name, workspace_id, created_at) VALUES (?, ?, ?, ?)",
            params![agent.id.clone(), agent.display_name.clone(), workspace.id.clone(), now],
        )
        .await?;
        Ok(())
    }

    pub async fn seed_runtime_settings(&self, settings: &RuntimeSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO runtime_settings (agent_id, model, system_prompt, temperature, max_tokens, stream, allow_tools, max_history_turns, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                settings.agent_id.clone(),
                settings.model.clone(),
                settings.system_prompt.clone(),
                settings.temperature as f64,
                settings.max_tokens as i64,
                if settings.stream { 1 } else { 0 },
                if settings.allow_tools { 1 } else { 0 },
                settings.max_history_turns as i64,
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn seed_retention_settings(&self, settings: &RetentionSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO retention_settings (agent_id, trace_blob_retention_days, created_at, updated_at) VALUES (?, ?, ?, ?)",
            params![
                settings.agent_id.clone(),
                settings.trace_blob_retention_days as i64,
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn load_runtime_settings(&self, agent_id: &str) -> Result<Option<RuntimeSettings>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT agent_id, model, system_prompt, temperature, max_tokens, stream, allow_tools, max_history_turns, created_at, updated_at FROM runtime_settings WHERE agent_id = ?",
                params![agent_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(RuntimeSettings {
                agent_id: row.get(0)?,
                model: row.get(1)?,
                system_prompt: row.get(2)?,
                temperature: row.get::<f64>(3)? as f32,
                max_tokens: row.get::<i64>(4)? as u32,
                stream: row.get::<i64>(5)? != 0,
                allow_tools: row.get::<i64>(6)? != 0,
                max_history_turns: row.get::<i64>(7)? as u32,
                created_at: parse_datetime(&row.get::<String>(8)?)?,
                updated_at: parse_datetime(&row.get::<String>(9)?)?,
            })
        } else {
            None
        })
    }

    pub async fn load_retention_settings(
        &self,
        agent_id: &str,
    ) -> Result<Option<RetentionSettings>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT agent_id, trace_blob_retention_days, created_at, updated_at FROM retention_settings WHERE agent_id = ?",
                params![agent_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(RetentionSettings {
                agent_id: row.get(0)?,
                trace_blob_retention_days: row.get::<i64>(1)? as u32,
                created_at: parse_datetime(&row.get::<String>(2)?)?,
                updated_at: parse_datetime(&row.get::<String>(3)?)?,
            })
        } else {
            None
        })
    }

    pub async fn update_runtime_settings(&self, settings: &RuntimeSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO runtime_settings (agent_id, model, system_prompt, temperature, max_tokens, stream, allow_tools, max_history_turns, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, COALESCE((SELECT created_at FROM runtime_settings WHERE agent_id = ?), ?), ?)",
            params![
                settings.agent_id.clone(),
                settings.model.clone(),
                settings.system_prompt.clone(),
                settings.temperature as f64,
                settings.max_tokens as i64,
                if settings.stream { 1 } else { 0 },
                if settings.allow_tools { 1 } else { 0 },
                settings.max_history_turns as i64,
                settings.agent_id.clone(),
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn update_retention_settings(&self, settings: &RetentionSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO retention_settings (agent_id, trace_blob_retention_days, created_at, updated_at) VALUES (?, ?, COALESCE((SELECT created_at FROM retention_settings WHERE agent_id = ?), ?), ?)",
            params![
                settings.agent_id.clone(),
                settings.trace_blob_retention_days as i64,
                settings.agent_id.clone(),
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn load_agent(&self, agent_id: &str) -> Result<Option<Agent>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, display_name, workspace_id FROM agents WHERE id = ?",
                params![agent_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(Agent {
                id: row.get(0)?,
                display_name: row.get(1)?,
                workspace_id: row.get(2)?,
            })
        } else {
            None
        })
    }

    pub async fn load_workspace(&self, workspace_id: &str) -> Result<Option<Workspace>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, root FROM workspaces WHERE id = ?",
                params![workspace_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(Workspace::new(row.get::<String>(0)?, row.get::<String>(1)?))
        } else {
            None
        })
    }

    pub async fn create_thread(
        &self,
        agent_id: &str,
        channel: &str,
        external_thread_id: &str,
        title: &str,
    ) -> Result<Thread> {
        let conn = self.connect()?;
        let id = external_thread_id.to_string();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO threads (id, agent_id, channel, external_thread_id, title, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                agent_id.to_string(),
                channel.to_string(),
                external_thread_id.to_string(),
                title.to_string(),
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )
        .await?;
        Ok(Thread {
            id,
            agent_id: agent_id.to_string(),
            channel: channel.to_string(),
            external_thread_id: external_thread_id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn find_thread(
        &self,
        agent_id: &str,
        channel: &str,
        external_thread_id: &str,
    ) -> Result<Option<Thread>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, created_at, updated_at FROM threads WHERE agent_id = ? AND channel = ? AND external_thread_id = ?",
                params![agent_id.to_string(), channel.to_string(), external_thread_id.to_string()],
            )
            .await?;
        Ok(self.read_thread(&mut rows).await?)
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Option<Thread>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, created_at, updated_at FROM threads WHERE id = ?",
                params![thread_id.to_string()],
            )
            .await?;
        Ok(self.read_thread(&mut rows).await?)
    }

    async fn read_thread(&self, rows: &mut Rows) -> Result<Option<Thread>> {
        Ok(if let Some(row) = rows.next().await? {
            Some(Thread {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                channel: row.get(2)?,
                external_thread_id: row.get(3)?,
                title: row.get(4)?,
                created_at: parse_datetime(&row.get::<String>(5)?)?,
                updated_at: parse_datetime(&row.get::<String>(6)?)?,
            })
        } else {
            None
        })
    }

    pub async fn get_turn(&self, turn_id: &str) -> Result<Option<Turn>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, assistant_message, error, created_at, updated_at FROM turns WHERE id = ?",
                params![turn_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(Turn {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get(3)?,
                assistant_message: row.get(4)?,
                error: row.get(5)?,
                created_at: parse_datetime(&row.get::<String>(6)?)?,
                updated_at: parse_datetime(&row.get::<String>(7)?)?,
            })
        } else {
            None
        })
    }

    pub async fn list_threads(&self) -> Result<Vec<Thread>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, created_at, updated_at FROM threads ORDER BY updated_at DESC",
                params![],
            )
            .await?;
        let mut threads = Vec::new();
        while let Some(row) = rows.next().await? {
            threads.push(Thread {
                id: row.get(0)?,
                agent_id: row.get(1)?,
                channel: row.get(2)?,
                external_thread_id: row.get(3)?,
                title: row.get(4)?,
                created_at: parse_datetime(&row.get::<String>(5)?)?,
                updated_at: parse_datetime(&row.get::<String>(6)?)?,
            });
        }
        Ok(threads)
    }

    pub async fn create_turn(&self, thread_id: &str, user_message: &str) -> Result<Turn> {
        let conn = self.connect()?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO turns (id, thread_id, status, user_message, assistant_message, error, created_at, updated_at) VALUES (?, ?, ?, ?, NULL, NULL, ?, ?)",
            params![
                id.clone(),
                thread_id.to_string(),
                "running".to_string(),
                user_message.to_string(),
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )
        .await?;
        Ok(Turn {
            id,
            thread_id: thread_id.to_string(),
            status: TurnStatus::Running,
            user_message: user_message.to_string(),
            assistant_message: None,
            error: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn update_turn(
        &self,
        turn_id: &str,
        status: TurnStatus,
        assistant_message: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let conn = self.connect()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE turns SET status = ?, assistant_message = ?, error = ?, updated_at = ? WHERE id = ?",
            params![
                turn_status_string(&status),
                assistant_message.map(ToString::to_string),
                error.map(ToString::to_string),
                now,
                turn_id.to_string()
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn list_thread_turns(&self, thread_id: &str) -> Result<Vec<Turn>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, assistant_message, error, created_at, updated_at FROM turns WHERE thread_id = ? ORDER BY created_at ASC",
                params![thread_id.to_string()],
            )
            .await?;
        let mut turns = Vec::new();
        while let Some(row) = rows.next().await? {
            turns.push(Turn {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get(3)?,
                assistant_message: row.get(4)?,
                error: row.get(5)?,
                created_at: parse_datetime(&row.get::<String>(6)?)?,
                updated_at: parse_datetime(&row.get::<String>(7)?)?,
            });
        }
        Ok(turns)
    }

    pub async fn list_running_turns(&self) -> Result<Vec<Turn>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, assistant_message, error, created_at, updated_at FROM turns WHERE status = ? ORDER BY created_at ASC",
                params!["running".to_string()],
            )
            .await?;
        let mut turns = Vec::new();
        while let Some(row) = rows.next().await? {
            turns.push(Turn {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get(3)?,
                assistant_message: row.get(4)?,
                error: row.get(5)?,
                created_at: parse_datetime(&row.get::<String>(6)?)?,
                updated_at: parse_datetime(&row.get::<String>(7)?)?,
            });
        }
        Ok(turns)
    }

    pub async fn append_event(
        &self,
        turn_id: &str,
        thread_id: &str,
        kind: EventKind,
        payload: &Value,
    ) -> Result<Event> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE turn_id = ?",
                params![turn_id.to_string()],
            )
            .await?;
        let sequence = if let Some(row) = rows.next().await? {
            row.get::<i64>(0)?
        } else {
            1
        };
        let event = Event {
            id: Uuid::new_v4().to_string(),
            turn_id: turn_id.to_string(),
            thread_id: thread_id.to_string(),
            sequence,
            kind,
            payload: payload.clone(),
            created_at: Utc::now(),
        };
        conn.execute(
            "INSERT INTO events (id, turn_id, thread_id, sequence, kind, payload, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                event.id.clone(),
                event.turn_id.clone(),
                event.thread_id.clone(),
                event.sequence,
                serde_json::to_string(&event.kind)?,
                serde_json::to_string(&event.payload)?,
                event.created_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(event)
    }

    pub async fn list_thread_events(&self, thread_id: &str) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, sequence, kind, payload, created_at FROM events WHERE thread_id = ? ORDER BY created_at ASC, sequence ASC",
                params![thread_id.to_string()],
            )
            .await?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await? {
            events.push(Event {
                id: row.get(0)?,
                turn_id: row.get(1)?,
                thread_id: row.get(2)?,
                sequence: row.get(3)?,
                kind: serde_json::from_str(&row.get::<String>(4)?)?,
                payload: serde_json::from_str(&row.get::<String>(5)?)?,
                created_at: parse_datetime(&row.get::<String>(6)?)?,
            });
        }
        Ok(events)
    }

    pub async fn store_trace_blob(&self, body: &Value) -> Result<TraceBlob> {
        self.store_trace_blob_json(body).await
    }

    pub async fn store_trace_blob_json<T: serde::Serialize>(&self, body: &T) -> Result<TraceBlob> {
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now();
        let value = redact_json(&serde_json::to_value(body)?);
        let body = compress_bytes(value.to_string().as_bytes())?;
        let blob = TraceBlob {
            id: id.clone(),
            encoding: "gzip".to_string(),
            content_type: "application/json".to_string(),
            body,
            created_at,
        };
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO trace_blobs (id, encoding, content_type, body, created_at) VALUES (?, ?, ?, ?, ?)",
            params![
                blob.id.clone(),
                blob.encoding.clone(),
                blob.content_type.clone(),
                blob.body.clone(),
                blob.created_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(blob)
    }

    pub async fn fetch_trace_blob_json(&self, blob_id: &str) -> Result<Value> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT body FROM trace_blobs WHERE id = ?",
                params![blob_id.to_string()],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("trace blob not found: {blob_id}"))?;
        let body: Vec<u8> = row.get(0)?;
        Ok(serde_json::from_slice(&decompress_bytes(&body)?)?)
    }

    pub async fn prune_trace_blobs_older_than(
        &self,
        cutoff: DateTime<Utc>,
        pruned_at: DateTime<Utc>,
    ) -> Result<TraceBlobPruneReport> {
        const PRUNED_CONTENT_TYPE: &str = "application/vnd.betterclaw.pruned+json";

        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, encoding, content_type, body, created_at FROM trace_blobs WHERE created_at < ? AND content_type != ?",
                params![cutoff.to_rfc3339(), PRUNED_CONTENT_TYPE.to_string()],
            )
            .await?;

        let mut blobs = Vec::new();
        while let Some(row) = rows.next().await? {
            blobs.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<Vec<u8>>(3)?,
                row.get::<String>(4)?,
            ));
        }

        let mut report = TraceBlobPruneReport {
            pruned_blob_count: 0,
            reclaimed_bytes: 0,
        };

        for (blob_id, encoding, content_type, body, created_at) in blobs {
            let original_bytes = body.len() as i64;
            let placeholder = serde_json::json!({
                "pruned": true,
                "reason": "retention_policy",
                "pruned_at": pruned_at,
                "original_encoding": encoding,
                "original_content_type": content_type,
                "original_created_at": created_at,
                "original_size_bytes": original_bytes,
            });
            let replacement = compress_bytes(placeholder.to_string().as_bytes())?;
            let replacement_bytes = replacement.len() as i64;
            conn.execute(
                "UPDATE trace_blobs SET encoding = ?, content_type = ?, body = ? WHERE id = ?",
                params![
                    "gzip".to_string(),
                    PRUNED_CONTENT_TYPE.to_string(),
                    replacement,
                    blob_id,
                ],
            )
            .await?;
            report.pruned_blob_count += 1;
            report.reclaimed_bytes += (original_bytes - replacement_bytes).max(0);
        }

        Ok(report)
    }

    pub async fn record_model_trace(&self, trace: &ModelTrace) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO model_traces (id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                trace.id.clone(),
                trace.turn_id.clone(),
                trace.thread_id.clone(),
                trace.agent_id.clone(),
                trace.channel.clone(),
                trace.model.clone(),
                trace.request_started_at.to_rfc3339(),
                trace.request_completed_at.to_rfc3339(),
                trace.duration_ms,
                serde_json::to_string(&trace.outcome)?,
                trace.input_tokens,
                trace.output_tokens,
                trace.cache_read_input_tokens,
                trace.cache_creation_input_tokens,
                trace.provider_request_id.clone(),
                trace.tool_count,
                serde_json::to_string(&trace.tool_names)?,
                trace.request_blob_id.clone(),
                trace.response_blob_id.clone(),
                trace.stream_blob_id.clone(),
                trace.error_summary.clone()
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn list_turn_traces(&self, turn_id: &str) -> Result<Vec<ModelTrace>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary FROM model_traces WHERE turn_id = ? ORDER BY request_started_at ASC",
                params![turn_id.to_string()],
            )
            .await?;
        let mut traces = Vec::new();
        while let Some(row) = rows.next().await? {
            traces.push(ModelTrace {
                id: row.get(0)?,
                turn_id: row.get(1)?,
                thread_id: row.get(2)?,
                agent_id: row.get(3)?,
                channel: row.get(4)?,
                model: row.get(5)?,
                request_started_at: parse_datetime(&row.get::<String>(6)?)?,
                request_completed_at: parse_datetime(&row.get::<String>(7)?)?,
                duration_ms: row.get(8)?,
                outcome: serde_json::from_str(&row.get::<String>(9)?)?,
                input_tokens: row.get(10)?,
                output_tokens: row.get(11)?,
                cache_read_input_tokens: row.get(12)?,
                cache_creation_input_tokens: row.get(13)?,
                provider_request_id: row.get(14)?,
                tool_count: row.get(15)?,
                tool_names: serde_json::from_str(&row.get::<String>(16)?)?,
                request_blob_id: row.get(17)?,
                response_blob_id: row.get(18)?,
                stream_blob_id: row.get(19)?,
                error_summary: row.get(20)?,
            });
        }
        Ok(traces)
    }

    pub async fn get_trace_detail(&self, trace_id: &str) -> Result<Option<TraceDetail>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary FROM model_traces WHERE id = ?",
                params![trace_id.to_string()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let trace = ModelTrace {
            id: row.get(0)?,
            turn_id: row.get(1)?,
            thread_id: row.get(2)?,
            agent_id: row.get(3)?,
            channel: row.get(4)?,
            model: row.get(5)?,
            request_started_at: parse_datetime(&row.get::<String>(6)?)?,
            request_completed_at: parse_datetime(&row.get::<String>(7)?)?,
            duration_ms: row.get(8)?,
            outcome: serde_json::from_str(&row.get::<String>(9)?)?,
            input_tokens: row.get(10)?,
            output_tokens: row.get(11)?,
            cache_read_input_tokens: row.get(12)?,
            cache_creation_input_tokens: row.get(13)?,
            provider_request_id: row.get(14)?,
            tool_count: row.get(15)?,
            tool_names: serde_json::from_str(&row.get::<String>(16)?)?,
            request_blob_id: row.get(17)?,
            response_blob_id: row.get(18)?,
            stream_blob_id: row.get(19)?,
            error_summary: row.get(20)?,
        };
        let response_blob = self.fetch_trace_blob_json(&trace.response_blob_id).await?;
        let (response_body, reduced_result) = match response_blob {
            Value::Object(map) => (
                map.get("raw_response").cloned().unwrap_or(Value::Null),
                map.get("reduced_result").cloned(),
            ),
            other => (other, None),
        };
        Ok(Some(TraceDetail {
            request_body: self.fetch_trace_blob_json(&trace.request_blob_id).await?,
            response_body,
            stream_body: match &trace.stream_blob_id {
                Some(blob_id) => Some(self.fetch_trace_blob_json(blob_id).await?),
                None => None,
            },
            reduced_result,
            trace,
        }))
    }

    pub async fn record_outbound_message(&self, outbound: &OutboundMessage) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO outbound_messages (id, turn_id, thread_id, channel, external_thread_id, content, metadata_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                outbound.id.clone(),
                outbound.turn_id.clone(),
                outbound.thread_id.clone(),
                outbound.channel.clone(),
                outbound.external_thread_id.clone(),
                outbound.content.clone(),
                outbound.metadata.as_ref().map(Value::to_string),
                outbound.created_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn load_cursor(
        &self,
        channel: &str,
        cursor_key: &str,
    ) -> Result<Option<ChannelCursor>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT channel, cursor_key, cursor_value, updated_at FROM channel_cursors WHERE channel = ? AND cursor_key = ?",
                params![channel.to_string(), cursor_key.to_string()],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            return Ok(Some(ChannelCursor {
                channel: row.get::<String>(0)?,
                cursor_key: row.get::<String>(1)?,
                cursor_value: row.get::<String>(2)?,
                updated_at: row.get::<String>(3)?.parse()?,
            }));
        }
        Ok(None)
    }

    pub async fn upsert_cursor(&self, cursor: &ChannelCursor) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO channel_cursors (channel, cursor_key, cursor_value, updated_at) VALUES (?, ?, ?, ?)",
            params![
                cursor.channel.clone(),
                cursor.cursor_key.clone(),
                cursor.cursor_value.clone(),
                cursor.updated_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn backdate_all_trace_blobs(&self, created_at: DateTime<Utc>) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE trace_blobs SET created_at = ?",
            params![created_at.to_rfc3339()],
        )
        .await?;
        Ok(())
    }
}

fn turn_status_string(status: &TurnStatus) -> String {
    match status {
        TurnStatus::Pending => "pending",
        TurnStatus::Running => "running",
        TurnStatus::AwaitingUser => "awaiting_user",
        TurnStatus::Succeeded => "succeeded",
        TurnStatus::Failed => "failed",
    }
    .to_string()
}

fn turn_status_from_string(value: &str) -> TurnStatus {
    match value {
        "pending" => TurnStatus::Pending,
        "running" => TurnStatus::Running,
        "awaiting_user" => TurnStatus::AwaitingUser,
        "succeeded" => TurnStatus::Succeeded,
        "failed" => TurnStatus::Failed,
        _ => TurnStatus::Failed,
    }
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)?.with_timezone(&Utc))
}

fn compress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    if [
                        "authorization",
                        "api_key",
                        "x-api-key",
                        "cookie",
                        "token",
                        "access_token",
                        "refresh_token",
                        "session_token",
                        "bearer_token",
                    ]
                    .iter()
                    .any(|needle| lower == *needle)
                        || lower.ends_with("_api_key")
                        || lower.ends_with("_secret")
                    {
                        (key.clone(), Value::String("[REDACTED]".to_string()))
                    } else {
                        (key.clone(), redact_json(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Db, redact_json};
    use chrono::{Duration, Utc};
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn redacts_secret_keys() {
        let value = json!({
            "authorization": "Bearer abc",
            "nested": {
                "api_key": "secret"
            }
        });
        let redacted = redact_json(&value);
        assert_eq!(redacted["authorization"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
    }

    #[test]
    fn preserves_non_secret_token_counters() {
        let value = json!({
            "max_tokens": 512,
            "prompt_tokens": 128
        });
        let redacted = redact_json(&value);
        assert_eq!(redacted["max_tokens"], 512);
        assert_eq!(redacted["prompt_tokens"], 128);
    }

    #[tokio::test]
    async fn prune_trace_blobs_replaces_body_with_placeholder() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("prune.db")).await.unwrap();
        let blob = db
            .store_trace_blob_json(&json!({"hello":"world"}))
            .await
            .unwrap();
        let report = db
            .prune_trace_blobs_older_than(Utc::now() + Duration::days(1), Utc::now())
            .await
            .unwrap();
        assert_eq!(report.pruned_blob_count, 1);
        let payload = db.fetch_trace_blob_json(&blob.id).await.unwrap();
        assert_eq!(payload["pruned"], true);
    }
}
