use anyhow::Result;
use chrono::Utc;
use libsql::{Rows, params};
use uuid::Uuid;

use crate::thread::Thread;
use crate::turn::{Turn, TurnStatus};

use super::Db;
use super::internal::*;

impl Db {
    pub async fn create_thread(
        &self,
        agent_id: &str,
        channel: &str,
        external_thread_id: &str,
        title: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Result<Thread> {
        let (_write_guard, conn) = self.write_connection().await?;
        let id = external_thread_id.to_string();
        let now = Utc::now();
        // INSERT OR IGNORE handles concurrent resolve_thread() calls racing
        // to create the same thread (same id = external_thread_id).
        conn.execute(
            "INSERT OR IGNORE INTO threads (id, agent_id, channel, external_thread_id, title, metadata_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                agent_id.to_string(),
                channel.to_string(),
                external_thread_id.to_string(),
                title.to_string(),
                metadata.map(|value| value.to_string()),
                now.to_rfc3339(),
                now.to_rfc3339()
            ],
        )
        .await?;
        // Always return the row — either ours or the one that beat us.
        if let Some(thread) = self
            .find_thread(agent_id, channel, external_thread_id)
            .await?
        {
            Ok(thread)
        } else {
            // Should never happen after INSERT OR IGNORE, but guard anyway.
            anyhow::bail!(
                "create_thread: row missing after INSERT OR IGNORE for {}",
                external_thread_id
            )
        }
    }

    pub async fn find_thread(
        &self,
        agent_id: &str,
        channel: &str,
        external_thread_id: &str,
    ) -> Result<Option<Thread>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, metadata_json, created_at, updated_at FROM threads WHERE agent_id = ? AND channel = ? AND external_thread_id = ?",
                params![agent_id.to_string(), channel.to_string(), external_thread_id.to_string()],
            )
            .await?;
        self.read_thread(&mut rows).await
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Option<Thread>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, metadata_json, created_at, updated_at FROM threads WHERE id = ?",
                params![thread_id.to_string()],
            )
            .await?;
        self.read_thread(&mut rows).await
    }

    pub async fn update_thread_metadata(
        &self,
        thread_id: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Result<()> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE threads SET metadata_json = ?, updated_at = ? WHERE id = ?",
            params![
                metadata.map(|value| value.to_string()),
                now,
                thread_id.to_string()
            ],
        )
        .await?;
        Ok(())
    }

    async fn read_thread(&self, rows: &mut Rows) -> Result<Option<Thread>> {
        Ok(if let Some(row) = rows.next().await? {
            Some(Thread {
                id: row.get::<String>(0)?,
                agent_id: row.get::<String>(1)?,
                channel: row.get::<String>(2)?,
                external_thread_id: row.get::<String>(3)?,
                title: row.get::<String>(4)?,
                metadata: row
                    .get::<Option<String>>(5)?
                    .and_then(|value| serde_json::from_str(&value).ok()),
                created_at: parse_datetime(&row.get::<String>(6)?)?,
                updated_at: parse_datetime(&row.get::<String>(7)?)?,
            })
        } else {
            None
        })
    }
    pub async fn get_turn(&self, turn_id: &str) -> Result<Option<Turn>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, attachments_json, assistant_message, error, created_at, updated_at FROM turns WHERE id = ?",
                params![turn_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(Turn {
                id: row.get::<String>(0)?,
                thread_id: row.get::<String>(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get::<String>(3)?,
                attachments_json: row.get::<Option<String>>(4)?,
                assistant_message: row.get::<Option<String>>(5)?,
                error: row.get::<Option<String>>(6)?,
                created_at: parse_datetime(&row.get::<String>(7)?)?,
                updated_at: parse_datetime(&row.get::<String>(8)?)?,
            })
        } else {
            None
        })
    }

    pub async fn list_threads(&self) -> Result<Vec<Thread>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, agent_id, channel, external_thread_id, title, metadata_json, created_at, updated_at FROM threads ORDER BY updated_at DESC",
                params![],
            )
            .await?;
        let mut threads = Vec::new();
        while let Some(row) = rows.next().await? {
            threads.push(Thread {
                id: row.get::<String>(0)?,
                agent_id: row.get::<String>(1)?,
                channel: row.get::<String>(2)?,
                external_thread_id: row.get::<String>(3)?,
                title: row.get::<String>(4)?,
                metadata: row
                    .get::<Option<String>>(5)?
                    .and_then(|value| serde_json::from_str(&value).ok()),
                created_at: parse_datetime(&row.get::<String>(6)?)?,
                updated_at: parse_datetime(&row.get::<String>(7)?)?,
            });
        }
        Ok(threads)
    }
    pub async fn create_turn(
        &self,
        thread_id: &str,
        user_message: &str,
        attachments_json: Option<&str>,
    ) -> Result<Turn> {
        let (_write_guard, conn) = self.write_connection().await?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO turns (id, thread_id, status, user_message, attachments_json, assistant_message, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, NULL, NULL, ?, ?)",
            params![
                id.clone(),
                thread_id.to_string(),
                "running".to_string(),
                user_message.to_string(),
                attachments_json.map(ToString::to_string),
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
            attachments_json: attachments_json.map(|s| s.to_string()),
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
        let (_write_guard, conn) = self.write_connection().await?;
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
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, attachments_json, assistant_message, error, created_at, updated_at FROM turns WHERE thread_id = ? ORDER BY created_at ASC",
                params![thread_id.to_string()],
            )
            .await?;
        let mut turns = Vec::new();
        while let Some(row) = rows.next().await? {
            turns.push(Turn {
                id: row.get::<String>(0)?,
                thread_id: row.get::<String>(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get::<String>(3)?,
                attachments_json: row.get::<Option<String>>(4)?,
                assistant_message: row.get::<Option<String>>(5)?,
                error: row.get::<Option<String>>(6)?,
                created_at: parse_datetime(&row.get::<String>(7)?)?,
                updated_at: parse_datetime(&row.get::<String>(8)?)?,
            });
        }
        Ok(turns)
    }
    pub async fn list_running_turns(&self) -> Result<Vec<Turn>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, thread_id, status, user_message, attachments_json, assistant_message, error, created_at, updated_at FROM turns WHERE status = ? ORDER BY created_at ASC",
                params!["running".to_string()],
            )
            .await?;
        let mut turns = Vec::new();
        while let Some(row) = rows.next().await? {
            turns.push(Turn {
                id: row.get::<String>(0)?,
                thread_id: row.get::<String>(1)?,
                status: turn_status_from_string(&row.get::<String>(2)?),
                user_message: row.get::<String>(3)?,
                attachments_json: row.get::<Option<String>>(4)?,
                assistant_message: row.get::<Option<String>>(5)?,
                error: row.get::<Option<String>>(6)?,
                created_at: parse_datetime(&row.get::<String>(7)?)?,
                updated_at: parse_datetime(&row.get::<String>(8)?)?,
            });
        }
        Ok(turns)
    }
}
