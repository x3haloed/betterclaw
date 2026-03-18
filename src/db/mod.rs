pub mod events;
pub(crate) mod internal;
pub mod memory;
pub mod routines;
pub mod settings;
#[cfg(test)]
mod tests;
pub mod threads;
pub mod traces;

pub use traces::TraceBlobPruneReport;

use crate::agent::Agent;
use crate::workspace::Workspace;
use anyhow::Result;
use chrono::Utc;
use libsql::{Builder, Connection, Database, params};
use std::path::Path;

pub struct Db {
    database: Database,
}

impl Db {
    pub async fn open(path: &Path) -> Result<Self> {
        let database = Builder::new_local(path).build().await?;
        let db = Self { database };
        db.migrate().await?;
        Ok(db)
    }

    pub(crate) fn connect(&self) -> Result<Connection> {
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
                system_prompt TEXT NOT NULL,
                max_tokens INTEGER NOT NULL,
                stream INTEGER NOT NULL,
                allow_tools INTEGER NOT NULL,
                max_history_turns INTEGER NOT NULL,
                inject_wake_pack INTEGER NOT NULL DEFAULT 1,
                inject_ledger_recall INTEGER NOT NULL DEFAULT 1,
                enable_auto_distill INTEGER NOT NULL DEFAULT 1,
                model_roles_json TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS retention_settings (
                agent_id TEXT PRIMARY KEY,
                trace_blob_retention_days INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_artifacts (
                id TEXT PRIMARY KEY,
                namespace_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                source TEXT NOT NULL,
                content TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                citations_json TEXT NOT NULL,
                supersedes_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS memory_state (
                namespace_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(namespace_id, key)
            );
            CREATE TABLE IF NOT EXISTS memory_recall_chunks (
                chunk_id TEXT PRIMARY KEY,
                namespace_id TEXT NOT NULL,
                source_type TEXT NOT NULL,
                source_id TEXT NOT NULL,
                entry_id TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                embedding_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(source_type, source_id, chunk_index)
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS memory_recall_chunks_fts USING fts5(
                chunk_id UNINDEXED,
                namespace_id UNINDEXED,
                source_type UNINDEXED,
                source_id UNINDEXED,
                entry_id UNINDEXED,
                content
            );
            CREATE TABLE IF NOT EXISTS observations (
                id TEXT PRIMARY KEY,
                namespace_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                severity TEXT NOT NULL,
                summary TEXT NOT NULL,
                detail TEXT,
                citations_json TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                resolved INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_observations_namespace_kind
                ON observations(namespace_id, kind, resolved);
            CREATE INDEX IF NOT EXISTS idx_observations_created
                ON observations(namespace_id, created_at DESC);

            "#,
        )
        .await?;
        self.add_column_if_missing(&conn, "outbound_messages", "metadata_json", "TEXT")
            .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "inject_wake_pack",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "inject_ledger_recall",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "enable_auto_distill",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "model_roles_json",
            "TEXT NOT NULL DEFAULT '[]'",
        )
        .await?;
        self.drop_column_if_exists(&conn, "runtime_settings", "model")
            .await?;
        self.drop_column_if_exists(&conn, "runtime_settings", "temperature")
            .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "enable_observations",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "inject_observations",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(
            &conn,
            "runtime_settings",
            "inject_skills",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await?;
        self.add_column_if_missing(&conn, "turns", "attachments_json", "TEXT")
            .await?;
        Ok(())
    }

    async fn drop_column_if_exists(
        &self,
        conn: &Connection,
        table: &str,
        column: &str,
    ) -> Result<()> {
        let pragma = format!("PRAGMA table_info({table})");
        let mut rows = conn.query(&pragma, params![]).await?;
        while let Some(row) = rows.next().await? {
            let existing: String = row.get(1)?;
            if existing == column {
                let alter = format!("ALTER TABLE {table} DROP COLUMN {column}");
                conn.execute(&alter, params![]).await?;
                return Ok(());
            }
        }
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
                id: row.get::<String>(0)?,
                display_name: row.get::<String>(1)?,
                workspace_id: row.get::<String>(2)?,
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
}
