use super::*;
use serde_json::json;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::turn::{Turn};
use crate::thread::Thread;
use crate::memory::*;
use super::internal::{build_fts_query, truncate_for_wake_pack};
use super::engine::env_role;

impl Runtime {
    pub(crate) async fn search_recall(
        &self,
        namespace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, RuntimeError> {
        let mut scores = std::collections::HashMap::<String, RecallHit>::new();
        let lexical_query = build_fts_query(query);
        if !lexical_query.trim().is_empty() {
            for hit in self
                .db
                .search_recall_chunks_keyword(namespace_id, &lexical_query, limit as i64 * 2)
                .await
                .map_err(RuntimeError::from)?
            {
                scores
                    .entry(hit.entry_id.clone())
                    .and_modify(|current| current.score += hit.score.max(0.1))
                    .or_insert(hit);
            }
        }
        if let Some(client) = self.embedding_client_for_namespace(namespace_id).await? {
            let query_embedding = client.embed(query).await?;
            for chunk in self
                .db
                .list_recall_chunks_with_embeddings(namespace_id, 256)
                .await
                .map_err(RuntimeError::from)?
            {
                let Some(embedding_json) = &chunk.embedding_json else {
                    continue;
                };
                let Ok(values) = serde_json::from_str::<Vec<f32>>(embedding_json) else {
                    continue;
                };
                let Some(score) = cosine_similarity(&query_embedding, &values) else {
                    continue;
                };
                let hit = RecallHit {
                    entry_id: chunk.entry_id.clone(),
                    source_id: chunk.source_id.clone(),
                    source_type: chunk.source_type.clone(),
                    content: chunk.content.clone(),
                    score,
                    citation: Some(chunk.entry_id.clone()),
                };
                scores
                    .entry(hit.entry_id.clone())
                    .and_modify(|current| {
                        if score > current.score {
                            current.score = score;
                            current.content = hit.content.clone();
                            current.citation = hit.citation.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }
        let mut hits = scores.into_values().collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    pub(crate) async fn embedding_client_for_namespace(
        &self,
        _namespace_id: &str,
    ) -> Result<Option<EmbeddingClient>, RuntimeError> {
        let settings = self.get_runtime_settings("default").await?;
        let role = settings
            .model_roles
            .iter()
            .find(|role| role.role == ModelRole::Embeddings && role.enabled)
            .cloned()
            .or_else(|| env_role(ModelRole::Embeddings));
        role.map(|role| EmbeddingClient::new(&role).map_err(RuntimeError::from))
            .transpose()
    }

    pub(crate) async fn sync_memory_for_turn(
        &self,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<(), RuntimeError> {
        let namespace = default_memory_namespace();
        let entries = self.normalized_entries_for_turn(thread, turn).await?;
        let embedder = self.embedding_client_for_namespace(&namespace).await?;
        for entry in entries {
            let content = entry
                .content
                .clone()
                .unwrap_or_else(|| entry.payload.to_string());
            let chunks = chunk_text(&content, 1200);
            let mut stored_chunks = Vec::new();
            for chunk in chunks {
                let embedding_json = match &embedder {
                    Some(client) => Some(
                        serde_json::to_string(&client.embed(&chunk).await?)
                            .map_err(|error| RuntimeError::ModelParse(error.to_string()))?,
                    ),
                    None => None,
                };
                stored_chunks.push((chunk, embedding_json));
            }
            self.db
                .replace_recall_chunks_for_source(
                    &namespace,
                    "ledger_entry",
                    &entry.entry_id,
                    &entry.entry_id,
                    &stored_chunks,
                )
                .await
                .map_err(RuntimeError::from)?;
        }
        if settings.enable_auto_distill {
            self.auto_distill_namespace(&namespace).await?;
        }
        Ok(())
    }

    pub(crate) async fn auto_distill_namespace(&self, namespace_id: &str) -> Result<(), RuntimeError> {
        let entries = self.normalized_entries_for_namespace(namespace_id).await?;
        if entries.is_empty() {
            return Ok(());
        }
        let recent = entries.into_iter().rev().take(8).collect::<Vec<_>>();
        let mut lines = Vec::new();
        let mut citations = Vec::new();
        for entry in recent.iter().rev() {
            citations.push(entry.entry_id.clone());
            match entry.kind {
                LedgerEntryKind::UserTurn => {
                    if let Some(content) = &entry.content {
                        lines.push(format!("- User asked: {}", truncate_for_wake_pack(content)));
                    }
                }
                LedgerEntryKind::AgentTurn => {
                    if let Some(content) = &entry.content {
                        lines.push(format!(
                            "- Agent answered: {}",
                            truncate_for_wake_pack(content)
                        ));
                    }
                }
                LedgerEntryKind::ToolResult => {
                    lines.push(format!(
                        "- Tool result observed: {}",
                        truncate_for_wake_pack(&entry.payload.to_string())
                    ));
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            return Ok(());
        }
        let content = format!("# Wake Pack (v0)\n\n{}\n", lines.join("\n"));
        let prior = self
            .db
            .latest_memory_artifact(namespace_id, MemoryArtifactKind::WakePackV0)
            .await
            .map_err(RuntimeError::from)?;
        let wake_pack = self
            .db
            .upsert_memory_artifact(&NewMemoryArtifact {
                namespace_id: namespace_id.to_string(),
                kind: MemoryArtifactKind::WakePackV0,
                source: "compressor".to_string(),
                content: content.clone(),
                payload: json!({
                    "line_count": lines.len(),
                    "strategy": "deterministic_recent_runtime_summary",
                }),
                citations: citations.clone(),
                supersedes_id: prior.as_ref().map(|artifact| artifact.id.clone()),
            })
            .await
            .map_err(RuntimeError::from)?;
        self.db
            .upsert_memory_artifact(&NewMemoryArtifact {
                namespace_id: namespace_id.to_string(),
                kind: MemoryArtifactKind::DistillMicro,
                source: "compressor".to_string(),
                content: String::new(),
                payload: json!({
                    "wake_pack_id": wake_pack.id,
                    "citations": citations,
                }),
                citations: prior.map(|artifact| vec![artifact.id]).unwrap_or_default(),
                supersedes_id: None,
            })
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    pub(crate) async fn normalized_entries_for_namespace(
        &self,
        namespace_id: &str,
    ) -> Result<Vec<LedgerEntry>, RuntimeError> {
        let mut entries = Vec::new();
        for thread in self.db.list_threads().await.map_err(RuntimeError::from)? {
            for turn in self
                .db
                .list_thread_turns(&thread.id)
                .await
                .map_err(RuntimeError::from)?
            {
                entries.extend(self.normalized_entries_for_turn(&thread, &turn).await?);
            }
        }
        entries.sort_by_key(|entry| entry.created_at);
        for entry in &mut entries {
            entry.namespace_id = namespace_id.to_string();
        }
        Ok(entries)
    }

    pub(crate) async fn normalized_entries_for_turn(
        &self,
        thread: &Thread,
        turn: &Turn,
    ) -> Result<Vec<LedgerEntry>, RuntimeError> {
        let mut entries = vec![LedgerEntry {
            entry_id: format!("turn:{}:user", turn.id),
            namespace_id: default_memory_namespace(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            kind: LedgerEntryKind::UserTurn,
            source: thread.channel.clone(),
            content: Some(turn.user_message.clone()),
            payload: json!({ "thread_id": thread.id, "turn_id": turn.id }),
            citation: format!("turn:{}", turn.id),
            created_at: turn.created_at,
        }];
        if let Some(assistant_message) = &turn.assistant_message {
            entries.push(LedgerEntry {
                entry_id: format!("turn:{}:assistant", turn.id),
                namespace_id: default_memory_namespace(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                kind: LedgerEntryKind::AgentTurn,
                source: thread.channel.clone(),
                content: Some(assistant_message.clone()),
                payload: json!({ "thread_id": thread.id, "turn_id": turn.id }),
                citation: format!("turn:{}", turn.id),
                created_at: turn.updated_at,
            });
        }
        let events = self
            .db
            .list_thread_events(&thread.id)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .filter(|event| event.turn_id == turn.id)
            .collect::<Vec<_>>();
        for event in events {
            let kind = match event.kind {
                EventKind::ToolCall => LedgerEntryKind::ToolCall,
                EventKind::ToolResult => LedgerEntryKind::ToolResult,
                EventKind::Error => LedgerEntryKind::Error,
                _ => continue,
            };
            entries.push(LedgerEntry {
                entry_id: format!("event:{}", event.id),
                namespace_id: default_memory_namespace(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                kind,
                source: "runtime_event".to_string(),
                content: Some(event.payload.to_string()),
                payload: event.payload.clone(),
                citation: format!("event:{}", event.id),
                created_at: event.created_at,
            });
        }
        Ok(entries)
    }
}

pub(crate) fn default_memory_namespace() -> String {
    "default".to_string()
}
