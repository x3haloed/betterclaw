use anyhow::Result;
use chrono::Utc;
use libsql::params;
use serde_json::Value;
use uuid::Uuid;

use crate::channel::{ChannelCursor, OutboundMessage};
use crate::memory::{
    DriftRecord, MemoryArtifact, MemoryArtifactKind, MemoryInvariantRecord, NewMemoryArtifact,
    RecallChunk, RecallHit, WakePackRecord,
};

use super::Db;
use super::internal::*;

impl Db {
    pub async fn clear_rebuilt_memory(&self, namespace_id: &str) -> Result<()> {
        let (_write_guard, conn) = self.write_connection().await?;
        conn.execute(
            "DELETE FROM memory_artifacts WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_state WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_recall_chunks WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_recall_chunks_fts WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM observations WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_fact_evidence WHERE fact_id IN (SELECT id FROM memory_facts WHERE namespace_id = ?)",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_candidate_facts WHERE candidate_id IN (SELECT id FROM memory_invariant_candidates WHERE namespace_id = ?)",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_invariant_facts WHERE invariant_id IN (SELECT id FROM memory_invariants WHERE namespace_id = ?)",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_invariant_supersedes WHERE invariant_id IN (SELECT id FROM memory_invariants WHERE namespace_id = ?) OR superseded_invariant_id IN (SELECT id FROM memory_invariants WHERE namespace_id = ?)",
            params![namespace_id.to_string(), namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM wake_pack_invariants WHERE wake_pack_id IN (SELECT id FROM wake_packs WHERE namespace_id = ?)",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_drift_item_facts WHERE drift_item_id IN (SELECT id FROM memory_drift_items WHERE namespace_id = ?)",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_facts WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_invariant_candidates WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_invariants WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_policies WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_preferences WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_hypotheses WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM memory_drift_items WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        conn.execute(
            "DELETE FROM wake_packs WHERE namespace_id = ?",
            params![namespace_id.to_string()],
        )
        .await?;
        Ok(())
    }

    pub async fn record_outbound_message(&self, outbound: &OutboundMessage) -> Result<()> {
        let (_write_guard, conn) = self.write_connection().await?;
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
        let (_write_guard, conn) = self.write_connection().await?;
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

    pub async fn insert_memory_fact(
        &self,
        namespace_id: &str,
        fact_key: &str,
        claim: &str,
        support_excerpt: &str,
        falsifier: &str,
        confidence: Option<f64>,
        evidence_ids: &[String],
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_facts (id, namespace_id, fact_key, claim, support_excerpt, falsifier, confidence, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                fact_key.to_string(),
                claim.to_string(),
                support_excerpt.to_string(),
                falsifier.to_string(),
                confidence,
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        for entry_id in evidence_ids {
            conn.execute(
                "INSERT OR IGNORE INTO memory_fact_evidence (fact_id, entry_id) VALUES (?, ?)",
                params![id.clone(), entry_id.clone()],
            )
            .await?;
        }
        Ok(id)
    }

    pub async fn insert_invariant_candidate(
        &self,
        namespace_id: &str,
        source_kind: &str,
        classifier: &str,
        claim: &str,
        support_excerpt: Option<&str>,
        falsifier: Option<&str>,
        fact_ids: &[String],
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_invariant_candidates (id, namespace_id, source_kind, classifier, claim, support_excerpt, falsifier, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                source_kind.to_string(),
                classifier.to_string(),
                claim.to_string(),
                support_excerpt.map(ToString::to_string),
                falsifier.map(ToString::to_string),
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        for fact_id in fact_ids {
            conn.execute(
                "INSERT OR IGNORE INTO memory_candidate_facts (candidate_id, fact_id) VALUES (?, ?)",
                params![id.clone(), fact_id.clone()],
            )
            .await?;
        }
        Ok(id)
    }

    pub async fn insert_memory_invariant(
        &self,
        namespace_id: &str,
        scope: &str,
        claim: &str,
        support_excerpt: &str,
        falsifier: &str,
        fact_ids: &[String],
        supersedes_ids: &[String],
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_invariants (id, namespace_id, scope, claim, support_excerpt, falsifier, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                scope.to_string(),
                claim.to_string(),
                support_excerpt.to_string(),
                falsifier.to_string(),
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        for fact_id in fact_ids {
            conn.execute(
                "INSERT OR IGNORE INTO memory_invariant_facts (invariant_id, fact_id) VALUES (?, ?)",
                params![id.clone(), fact_id.clone()],
            )
            .await?;
        }
        for superseded_invariant_id in supersedes_ids {
            conn.execute(
                "INSERT OR IGNORE INTO memory_invariant_supersedes (invariant_id, superseded_invariant_id) VALUES (?, ?)",
                params![id.clone(), superseded_invariant_id.clone()],
            )
            .await?;
            conn.execute(
                "UPDATE memory_invariants SET status = 'superseded', updated_at = ? WHERE id = ?",
                params![now.clone(), superseded_invariant_id.clone()],
            )
            .await?;
        }
        Ok(id)
    }

    pub async fn insert_memory_policy(
        &self,
        namespace_id: &str,
        claim: &str,
        evidence_note: Option<&str>,
        candidate_id: Option<&str>,
    ) -> Result<String> {
        self.insert_memory_note(
            "memory_policies",
            namespace_id,
            claim,
            evidence_note,
            candidate_id,
        )
        .await
    }

    pub async fn insert_memory_preference(
        &self,
        namespace_id: &str,
        claim: &str,
        evidence_note: Option<&str>,
        candidate_id: Option<&str>,
    ) -> Result<String> {
        self.insert_memory_note(
            "memory_preferences",
            namespace_id,
            claim,
            evidence_note,
            candidate_id,
        )
        .await
    }

    pub async fn insert_memory_hypothesis(
        &self,
        namespace_id: &str,
        claim: &str,
        support_excerpt: Option<&str>,
        falsifier: Option<&str>,
        candidate_id: Option<&str>,
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_hypotheses (id, namespace_id, claim, support_excerpt, falsifier, candidate_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                claim.to_string(),
                support_excerpt.map(ToString::to_string),
                falsifier.map(ToString::to_string),
                candidate_id.map(ToString::to_string),
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        Ok(id)
    }

    async fn insert_memory_note(
        &self,
        table: &str,
        namespace_id: &str,
        claim: &str,
        evidence_note: Option<&str>,
        candidate_id: Option<&str>,
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        let sql = format!(
            "INSERT INTO {table} (id, namespace_id, claim, evidence_note, candidate_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)"
        );
        conn.execute(
            &sql,
            params![
                id.clone(),
                namespace_id.to_string(),
                claim.to_string(),
                evidence_note.map(ToString::to_string),
                candidate_id.map(ToString::to_string),
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        Ok(id)
    }

    pub async fn insert_wake_pack_v2(
        &self,
        namespace_id: &str,
        content: &str,
        summary: Option<&str>,
        invariant_ids: &[String],
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO wake_packs (id, namespace_id, content, summary, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                content.to_string(),
                summary.map(ToString::to_string),
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        for invariant_id in invariant_ids {
            conn.execute(
                "INSERT OR IGNORE INTO wake_pack_invariants (wake_pack_id, invariant_id) VALUES (?, ?)",
                params![id.clone(), invariant_id.clone()],
            )
            .await?;
        }
        Ok(id)
    }

    pub async fn insert_memory_drift_item(
        &self,
        namespace_id: &str,
        kind: &str,
        claim: &str,
        support_excerpt: Option<&str>,
        falsifier: Option<&str>,
        citations: &[String],
        fact_ids: &[String],
    ) -> Result<String> {
        let (_write_guard, conn) = self.write_connection().await?;
        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO memory_drift_items (id, namespace_id, kind, claim, support_excerpt, falsifier, citations_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.clone(),
                namespace_id.to_string(),
                kind.to_string(),
                claim.to_string(),
                support_excerpt.map(ToString::to_string),
                falsifier.map(ToString::to_string),
                serde_json::to_string(citations)?,
                now.clone(),
                now.clone(),
            ],
        )
        .await?;
        for fact_id in fact_ids {
            conn.execute(
                "INSERT OR IGNORE INTO memory_drift_item_facts (drift_item_id, fact_id) VALUES (?, ?)",
                params![id.clone(), fact_id.clone()],
            )
            .await?;
        }
        Ok(id)
    }

    pub async fn latest_wake_pack(&self, namespace_id: &str) -> Result<Option<WakePackRecord>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, namespace_id, content, summary, created_at, updated_at FROM wake_packs WHERE namespace_id = ? ORDER BY created_at DESC LIMIT 1",
                params![namespace_id.to_string()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let id: String = row.get(0)?;
        let invariant_ids = self.list_wake_pack_invariant_ids(&id).await?;
        Ok(Some(WakePackRecord {
            id,
            namespace_id: row.get(1)?,
            content: row.get(2)?,
            summary: row.get(3)?,
            invariant_ids,
            created_at: row.get::<String>(4)?.parse()?,
            updated_at: row.get::<String>(5)?.parse()?,
        }))
    }

    pub async fn list_active_memory_invariants(
        &self,
        namespace_id: &str,
        limit: usize,
    ) -> Result<Vec<MemoryInvariantRecord>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT id, namespace_id, scope, claim, support_excerpt, falsifier, status, created_at, updated_at FROM memory_invariants WHERE namespace_id = ? AND status = 'active' ORDER BY created_at DESC LIMIT ?",
                params![namespace_id.to_string(), limit as i64],
            )
            .await?;
        let mut invariants = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let fact_ids = self.list_invariant_fact_ids(&id).await?;
            let supersedes_ids = self.list_supersedes_ids(&id).await?;
            invariants.push(MemoryInvariantRecord {
                id,
                namespace_id: row.get(1)?,
                scope: row.get(2)?,
                claim: row.get(3)?,
                support_excerpt: row.get(4)?,
                falsifier: row.get(5)?,
                status: row.get(6)?,
                fact_ids,
                supersedes_ids,
                created_at: row.get::<String>(7)?.parse()?,
                updated_at: row.get::<String>(8)?.parse()?,
            });
        }
        Ok(invariants)
    }

    pub async fn list_memory_drift_items(
        &self,
        namespace_id: &str,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DriftRecord>> {
        let conn = self.connect().await?;
        let kind_value = kind.unwrap_or_default().to_string();
        let mut rows = conn
            .query(
                "SELECT id, namespace_id, kind, claim, support_excerpt, falsifier, citations_json, created_at, updated_at FROM memory_drift_items WHERE namespace_id = ? AND (? = '' OR kind = ?) ORDER BY created_at DESC LIMIT ?",
                params![
                    namespace_id.to_string(),
                    kind_value.clone(),
                    kind_value,
                    limit as i64,
                ],
            )
            .await?;
        let mut items = Vec::new();
        while let Some(row) = rows.next().await? {
            let id: String = row.get(0)?;
            let fact_ids = self.list_drift_item_fact_ids(&id).await?;
            items.push(DriftRecord {
                id,
                namespace_id: row.get(1)?,
                kind: row.get(2)?,
                claim: row.get(3)?,
                support_excerpt: row.get(4)?,
                falsifier: row.get(5)?,
                citations: serde_json::from_str(&row.get::<String>(6)?)?,
                fact_ids,
                created_at: parse_datetime(&row.get::<String>(7)?)?,
                updated_at: parse_datetime(&row.get::<String>(8)?)?,
            });
        }
        Ok(items)
    }

    async fn list_wake_pack_invariant_ids(&self, wake_pack_id: &str) -> Result<Vec<String>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT invariant_id FROM wake_pack_invariants WHERE wake_pack_id = ? ORDER BY invariant_id",
                params![wake_pack_id.to_string()],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<String>(0)?);
        }
        Ok(ids)
    }

    async fn list_invariant_fact_ids(&self, invariant_id: &str) -> Result<Vec<String>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT fact_id FROM memory_invariant_facts WHERE invariant_id = ? ORDER BY fact_id",
                params![invariant_id.to_string()],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<String>(0)?);
        }
        Ok(ids)
    }

    async fn list_supersedes_ids(&self, invariant_id: &str) -> Result<Vec<String>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT superseded_invariant_id FROM memory_invariant_supersedes WHERE invariant_id = ? ORDER BY superseded_invariant_id",
                params![invariant_id.to_string()],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<String>(0)?);
        }
        Ok(ids)
    }

    async fn list_drift_item_fact_ids(&self, drift_item_id: &str) -> Result<Vec<String>> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT fact_id FROM memory_drift_item_facts WHERE drift_item_id = ? ORDER BY fact_id",
                params![drift_item_id.to_string()],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<String>(0)?);
        }
        Ok(ids)
    }

    pub async fn list_memory_artifacts(
        &self,
        namespace_id: &str,
        kind: Option<MemoryArtifactKind>,
        limit: i64,
    ) -> Result<Vec<MemoryArtifact>> {
        let conn = self.connect().await?;
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
        let (_write_guard, conn) = self.write_connection().await?;
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
        let conn = self.connect().await?;
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
        let (_write_guard, conn) = self.write_connection().await?;
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
        let conn = self.connect().await?;
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
        let conn = self.connect().await?;
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
        let conn = self.connect().await?;
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
        let (_write_guard, conn) = self.write_connection().await?;
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
