use super::engine::{env_role, resolve_role_engine};
use super::internal::{build_fts_query, truncate_for_wake_pack};
use super::*;
use crate::error::RuntimeError;
use crate::memory::*;
use crate::model::ModelExchangeRequest;
use crate::thread::Thread;
use crate::turn::Turn;
use serde_json::json;

#[derive(Debug, Clone, serde::Deserialize)]
struct CompressorArtifactSpec {
    text: String,
    #[serde(default)]
    citations: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CompressorOutput {
    wake_pack: String,
    #[serde(default)]
    invariant_self: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    invariant_user: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    invariant_relationship: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    drift_flags: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    drift_contradictions: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    drift_merges: Vec<CompressorArtifactSpec>,
    #[serde(default)]
    summary: Option<String>,
}

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
            self.auto_distill_namespace(&namespace, thread, turn, settings)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn auto_distill_namespace(
        &self,
        namespace_id: &str,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<(), RuntimeError> {
        if let Some(output) = self
            .run_model_driven_distill(namespace_id, thread, turn, settings)
            .await?
        {
            self.persist_compressor_output(namespace_id, &output)
                .await?;
            return Ok(());
        }
        self.persist_deterministic_distill(namespace_id).await
    }

    async fn run_model_driven_distill(
        &self,
        namespace_id: &str,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<Option<CompressorOutput>, RuntimeError> {
        let recent_entries = self.normalized_entries_for_namespace(namespace_id).await?;
        if recent_entries.is_empty() {
            return Ok(None);
        }
        let evidence = recent_entries
            .iter()
            .rev()
            .take(24)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|entry| {
                json!({
                    "entry_id": entry.entry_id,
                    "kind": format!("{:?}", entry.kind).to_lowercase(),
                    "citation": entry.citation,
                    "content": entry.content,
                    "payload": entry.payload,
                    "created_at": entry.created_at,
                })
            })
            .collect::<Vec<_>>();
        let prior_wake_pack = self
            .db
            .latest_memory_artifact(namespace_id, MemoryArtifactKind::WakePackV0)
            .await
            .map_err(RuntimeError::from)?;
        let active_invariants = self
            .db
            .list_memory_artifacts(namespace_id, None, 24)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .filter(|artifact| {
                matches!(
                    artifact.kind,
                    MemoryArtifactKind::InvariantSelfV0
                        | MemoryArtifactKind::InvariantUserV0
                        | MemoryArtifactKind::InvariantRelationshipV0
                )
            })
            .take(12)
            .map(|artifact| {
                json!({
                    "id": artifact.id,
                    "kind": artifact.kind.as_str(),
                    "content": artifact.content,
                    "citations": artifact.citations,
                })
            })
            .collect::<Vec<_>>();

        let compressor_role = settings
            .model_roles
            .iter()
            .find(|role| role.role == ModelRole::Compressor && role.enabled)
            .cloned()
            .or_else(|| env_role(ModelRole::Compressor));
        let (engine, provider_name, model_name, used_fallback_engine) =
            if let Some(role) = compressor_role {
                let resolved = resolve_role_engine(&role).map_err(RuntimeError::from)?;
                (
                    resolved.engine,
                    resolved.provider_name,
                    resolved.model_name,
                    false,
                )
            } else {
                (
                    (*self.model_engine).clone(),
                    self.provider_name.clone(),
                    settings.model.clone(),
                    true,
                )
            };
        if matches!(&engine, ModelEngine::Stub(_)) || provider_name == "stub" {
            return Ok(Some(stub_compressor_output()));
        }

        let request = ModelExchangeRequest {
            model: model_name.clone(),
            messages: vec![
                ModelMessage {
                    role: "system".to_string(),
                    content: Some(
                        "You are BetterClaw's memory compressor. Produce strict JSON only. Synthesize a compact wake pack plus cited invariant and drift artifacts from the supplied runtime evidence. Be conservative, do not invent facts, and only cite entry ids present in the evidence."
                            .to_string(),
                    ),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ModelMessage {
                    role: "user".to_string(),
                    content: Some(
                        json!({
                            "task": "distill_runtime_memory",
                            "namespace_id": namespace_id,
                            "previous_wake_pack": prior_wake_pack.as_ref().map(|artifact| artifact.content.clone()),
                            "active_invariants": active_invariants,
                            "recent_ledger_entries": evidence,
                            "output_contract": {
                                "wake_pack": "string",
                                "invariant_self": [{"text":"string","citations":["entry_id"]}],
                                "invariant_user": [{"text":"string","citations":["entry_id"]}],
                                "invariant_relationship": [{"text":"string","citations":["entry_id"]}],
                                "drift_flags": [{"text":"string","citations":["entry_id"]}],
                                "drift_contradictions": [{"text":"string","citations":["entry_id"]}],
                                "drift_merges": [{"text":"string","citations":["entry_id"]}],
                                "summary": "optional string"
                            }
                        })
                        .to_string(),
                    ),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: Vec::new(),
            temperature: Some(0.1),
            max_tokens: Some(1400),
            stream: false,
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "betterclaw_memory_distill",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "wake_pack": { "type": "string" },
                            "invariant_self": { "$ref": "#/$defs/items" },
                            "invariant_user": { "$ref": "#/$defs/items" },
                            "invariant_relationship": { "$ref": "#/$defs/items" },
                            "drift_flags": { "$ref": "#/$defs/items" },
                            "drift_contradictions": { "$ref": "#/$defs/items" },
                            "drift_merges": { "$ref": "#/$defs/items" },
                            "summary": { "type": ["string", "null"] }
                        },
                        "required": ["wake_pack", "invariant_self", "invariant_user", "invariant_relationship", "drift_flags", "drift_contradictions", "drift_merges"],
                        "$defs": {
                            "item": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "citations": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    }
                                },
                                "required": ["text", "citations"],
                                "additionalProperties": false
                            },
                            "items": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/item" }
                            }
                        },
                        "additionalProperties": false
                    }
                }
            })),
            extra: json!({
                "betterclaw_role": "compressor",
                "namespace_id": namespace_id,
            }),
        };

        let exchange = if used_fallback_engine {
            self.run_and_record_exchange(turn, thread, &settings.agent_id, &thread.channel, request)
                .await?
        } else {
            match engine.run(request).await {
                Ok(exchange) => exchange,
                Err(error) => {
                    self.record_trace(
                        turn,
                        thread,
                        &settings.agent_id,
                        &thread.channel,
                        error.exchange(),
                    )
                    .await?;
                    return Ok(None);
                }
            }
        };
        self.record_trace(turn, thread, &settings.agent_id, &thread.channel, &exchange)
            .await?;
        let Some(content) = exchange.content.as_deref() else {
            return Ok(None);
        };
        let parsed = match parse_compressor_output(content) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None),
        };
        if parsed.wake_pack.trim().is_empty() {
            return Ok(None);
        }
        let _ = provider_name;
        Ok(Some(parsed))
    }

    async fn persist_compressor_output(
        &self,
        namespace_id: &str,
        output: &CompressorOutput,
    ) -> Result<(), RuntimeError> {
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
                content: output.wake_pack.clone(),
                payload: json!({
                    "strategy": "model_driven_distill",
                    "summary": output.summary,
                }),
                citations: collect_output_citations(output),
                supersedes_id: prior.as_ref().map(|artifact| artifact.id.clone()),
            })
            .await
            .map_err(RuntimeError::from)?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::InvariantSelfV0,
            &output.invariant_self,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::InvariantUserV0,
            &output.invariant_user,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::InvariantRelationshipV0,
            &output.invariant_relationship,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftFlagV0,
            &output.drift_flags,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftContradictionV0,
            &output.drift_contradictions,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftMergeV0,
            &output.drift_merges,
        )
        .await?;
        self.db
            .upsert_memory_artifact(&NewMemoryArtifact {
                namespace_id: namespace_id.to_string(),
                kind: MemoryArtifactKind::DistillMicro,
                source: "compressor".to_string(),
                content: output.summary.clone().unwrap_or_default(),
                payload: json!({
                    "wake_pack_id": wake_pack.id,
                    "artifact_counts": {
                        "invariant_self": output.invariant_self.len(),
                        "invariant_user": output.invariant_user.len(),
                        "invariant_relationship": output.invariant_relationship.len(),
                        "drift_flags": output.drift_flags.len(),
                        "drift_contradictions": output.drift_contradictions.len(),
                        "drift_merges": output.drift_merges.len(),
                    }
                }),
                citations: collect_output_citations(output),
                supersedes_id: None,
            })
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    async fn persist_compressor_artifact_list(
        &self,
        namespace_id: &str,
        kind: MemoryArtifactKind,
        items: &[CompressorArtifactSpec],
    ) -> Result<(), RuntimeError> {
        for item in items {
            if item.text.trim().is_empty() {
                continue;
            }
            self.db
                .upsert_memory_artifact(&NewMemoryArtifact {
                    namespace_id: namespace_id.to_string(),
                    kind: kind.clone(),
                    source: "compressor".to_string(),
                    content: item.text.clone(),
                    payload: json!({}),
                    citations: item.citations.clone(),
                    supersedes_id: None,
                })
                .await
                .map_err(RuntimeError::from)?;
        }
        Ok(())
    }

    async fn persist_deterministic_distill(&self, namespace_id: &str) -> Result<(), RuntimeError> {
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

fn parse_compressor_output(content: &str) -> Result<CompressorOutput, RuntimeError> {
    let trimmed = content.trim();
    let json_text = trimmed
        .strip_prefix("```json")
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .or_else(|| {
            trimmed
                .strip_prefix("```")
                .and_then(|value| value.strip_suffix("```"))
                .map(str::trim)
        })
        .unwrap_or(trimmed);
    serde_json::from_str(json_text).map_err(|error| {
        RuntimeError::ModelParse(format!("compressor returned invalid JSON: {error}"))
    })
}

fn collect_output_citations(output: &CompressorOutput) -> Vec<String> {
    let mut citations = std::collections::BTreeSet::new();
    for item in output
        .invariant_self
        .iter()
        .chain(output.invariant_user.iter())
        .chain(output.invariant_relationship.iter())
        .chain(output.drift_flags.iter())
        .chain(output.drift_contradictions.iter())
        .chain(output.drift_merges.iter())
    {
        for citation in &item.citations {
            citations.insert(citation.clone());
        }
    }
    citations.into_iter().collect()
}

fn stub_compressor_output() -> CompressorOutput {
    CompressorOutput {
        wake_pack: "Stub compressor wake pack: preserve recent user intent and tool outcomes."
            .to_string(),
        invariant_self: vec![CompressorArtifactSpec {
            text: "BetterClaw should prefer tool-backed answers when tools materially help."
                .to_string(),
            citations: vec!["turn:stub:user".to_string()],
        }],
        invariant_user: vec![CompressorArtifactSpec {
            text: "The user is actively iterating on BetterClaw runtime behavior.".to_string(),
            citations: vec!["turn:stub:user".to_string()],
        }],
        invariant_relationship: Vec::new(),
        drift_flags: Vec::new(),
        drift_contradictions: Vec::new(),
        drift_merges: Vec::new(),
        summary: Some("Stub compressor distill complete.".to_string()),
    }
}

pub(crate) fn default_memory_namespace() -> String {
    "default".to_string()
}
