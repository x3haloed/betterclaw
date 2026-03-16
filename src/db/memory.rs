use anyhow::Result;
use chrono::Utc;
use libsql::params;
use serde_json::Value;
use uuid::Uuid;

use crate::channel::{ChannelCursor, OutboundMessage};
use crate::memory::{
    MemoryArtifact, MemoryArtifactKind, NewMemoryArtifact, RecallChunk, RecallHit,
};

use super::Db;
use super::internal::*;

impl Db {
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
    pub async fn upsert_memory_artifact(
        &self,
        artifact: &NewMemoryArtifact,
    ) -> Result<MemoryArtifact> {
        let conn = self.connect()?;
        let now = Utc::now();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_artifacts (id, namespace_id, kind, source, content, payload_json, citations_json, supersedes_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                artifact.namespace_id.clone(),
                artifact.kind.as_str().to_string(),
                artifact.source.clone(),
                artifact.content.clone(),
                artifact.payload.to_string(),
                serde_json::to_string(&artifact.citations)?,
                artifact.supersedes_id.clone(),
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )
        .await?;
        Ok(MemoryArtifact {
            id,
            namespace_id: artifact.namespace_id.clone(),
            kind: artifact.kind.clone(),
            source: artifact.source.clone(),
            content: artifact.content.clone(),
            payload: artifact.payload.clone(),
            citations: artifact.citations.clone(),
            supersedes_id: artifact.supersedes_id.clone(),
            created_at: now,
            updated_at: now,
        })
    }
    pub async fn list_memory_artifacts(
        &self,
        namespace_id: &str,
        kind: Option<MemoryArtifactKind>,
        limit: i64,
    ) -> Result<Vec<MemoryArtifact>> {
        let conn = self.connect()?;
        let kind_value = kind
            .map(|item| item.as_str().to_string())
            .unwrap_or_default();
        let mut rows = conn
            .query(
                "SELECT id, namespace_id, kind, source, content, payload_json, citations_json, supersedes_id, created_at, updated_at FROM memory_artifacts WHERE namespace_id = ? AND (? = '' OR kind = ?) ORDER BY created_at DESC LIMIT ?",
                params![namespace_id.to_string(), kind_value.clone(), kind_value, limit],
            )
            .await?;
        let mut artifacts = Vec::new();
        while let Some(row) = rows.next().await? {
            artifacts.push(MemoryArtifact {
                id: row.get::<String>(0)?,
                namespace_id: row.get::<String>(1)?,
                kind: memory_artifact_kind_from_str(&row.get::<String>(2)?),
                source: row.get::<String>(3)?,
                content: row.get::<String>(4)?,
                payload: serde_json::from_str(&row.get::<String>(5)?)?,
                citations: serde_json::from_str(&row.get::<String>(6)?)?,
                supersedes_id: row.get::<Option<String>>(7)?,
                created_at: parse_datetime(&row.get::<String>(8)?)?,
                updated_at: parse_datetime(&row.get::<String>(9)?)?,
            });
        }
        Ok(artifacts)
    }
    pub async fn latest_memory_artifact(
        &self,
        namespace_id: &str,
        kind: MemoryArtifactKind,
    ) -> Result<Option<MemoryArtifact>> {
        Ok(self
            .list_memory_artifacts(namespace_id, Some(kind), 1)
            .await?
            .into_iter()
            .next())
    }
    pub async fn set_memory_state(
        &self,
        namespace_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO memory_state (namespace_id, key, value_json, updated_at) VALUES (?, ?, ?, ?)",
            params![
                namespace_id.to_string(),
                key.to_string(),
                value.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn get_memory_state(&self, namespace_id: &str, key: &str) -> Result<Option<Value>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT value_json FROM memory_state WHERE namespace_id = ? AND key = ?",
                params![namespace_id.to_string(), key.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(serde_json::from_str(&row.get::<String>(0)?)?)
        } else {
            None
        })
    }
    pub async fn replace_recall_chunks_for_source(
        &self,
        namespace_id: &str,
        source_type: &str,
        source_id: &str,
        entry_id: &str,
        chunks: &[(String, Option<String>)],
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM memory_recall_chunks WHERE namespace_id = ? AND source_type = ? AND source_id = ?",
            params![namespace_id.to_string(), source_type.to_string(), source_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_recall_chunks_fts WHERE namespace_id = ? AND source_type = ? AND source_id = ?",
            params![namespace_id.to_string(), source_type.to_string(), source_id.to_string()],
        )
        .await?;
        let now = Utc::now().to_rfc3339();
        for (index, (content, embedding_json)) in chunks.iter().enumerate() {
            let chunk_id = format!("{source_type}:{source_id}:{index}");
            conn.execute(
                "INSERT INTO memory_recall_chunks (chunk_id, namespace_id, source_type, source_id, entry_id, chunk_index, content, embedding_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id.clone(),
                    namespace_id.to_string(),
                    source_type.to_string(),
                    source_id.to_string(),
                    entry_id.to_string(),
                    index as i64,
                    content.clone(),
                    embedding_json.clone(),
                    now.clone(),
                    now.clone(),
                ],
            )
            .await?;
            conn.execute(
                "INSERT INTO memory_recall_chunks_fts (chunk_id, namespace_id, source_type, source_id, entry_id, content) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id,
                    namespace_id.to_string(),
                    source_type.to_string(),
                    source_id.to_string(),
                    entry_id.to_string(),
                    content.clone(),
                ],
            )
            .await?;
        }
        Ok(())
    }
    pub async fn search_recall_chunks_keyword(
        &self,
        namespace_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<RecallHit>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT chunk_id, source_id, source_type, entry_id, content, bm25(memory_recall_chunks_fts) AS rank FROM memory_recall_chunks_fts WHERE namespace_id = ? AND memory_recall_chunks_fts MATCH ? ORDER BY rank ASC LIMIT ?",
                params![namespace_id.to_string(), query.to_string(), limit],
            )
            .await?;
        let mut hits = Vec::new();
        while let Some(row) = rows.next().await? {
            hits.push(RecallHit {
                entry_id: row.get::<String>(3)?,
                source_id: row.get::<String>(1)?,
                source_type: row.get::<String>(2)?,
                content: row.get::<String>(4)?,
                score: -row.get::<f64>(5).unwrap_or(0.0),
                citation: None,
            });
        }
        Ok(hits)
    }
    pub async fn list_recall_chunks_with_embeddings(
        &self,
        namespace_id: &str,
        limit: i64,
    ) -> Result<Vec<RecallChunk>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT chunk_id, namespace_id, source_type, source_id, entry_id, chunk_index, content, embedding_json, created_at, updated_at FROM memory_recall_chunks WHERE namespace_id = ? AND embedding_json IS NOT NULL ORDER BY updated_at DESC LIMIT ?",
                params![namespace_id.to_string(), limit],
            )
            .await?;
        let mut chunks = Vec::new();
        while let Some(row) = rows.next().await? {
            chunks.push(RecallChunk {
                chunk_id: row.get::<String>(0)?,
                namespace_id: row.get::<String>(1)?,
                source_type: row.get::<String>(2)?,
                source_id: row.get::<String>(3)?,
                entry_id: row.get::<String>(4)?,
                chunk_index: row.get::<i64>(5)?,
                content: row.get::<String>(6)?,
                embedding_json: row.get::<Option<String>>(7)?,
                created_at: parse_datetime(&row.get::<String>(8)?)?,
                updated_at: parse_datetime(&row.get::<String>(9)?)?,
            });
        }
        Ok(chunks)
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
}
