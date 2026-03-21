use super::engine::{env_role, resolve_role_engine};
use super::internal::{build_fts_query, truncate_for_wake_pack};
use super::*;
use crate::error::RuntimeError;
use crate::memory::*;
use crate::model::{MessageContent, ModelExchangeRequest, ModelMessage, ModelExchangeResult};
use crate::thread::Thread;
use crate::turn::Turn;
use serde_json::json;

#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CompressorArtifactClassifier {
    EmpiricalInvariant,
    Policy,
    Preference,
    Hypothesis,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CompressorFactSpec {
    fact_id: String,
    text: String,
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    support_excerpt: Option<String>,
    #[serde(default)]
    falsifier: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CompressorArtifactSpec {
    text: String,
    #[serde(default)]
    citations: Vec<String>,
    #[serde(default)]
    support_excerpt: Option<String>,
    #[serde(default)]
    falsifier: Option<String>,
    #[serde(default)]
    why_it_holds: Option<String>,
    #[serde(default)]
    classifier: Option<CompressorArtifactClassifier>,
    #[serde(default)]
    supersedes_ids: Vec<String>,
    #[serde(default)]
    derived_from_fact_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct CompressorOutput {
    wake_pack: String,
    #[serde(default)]
    facts: Vec<CompressorFactSpec>,
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

#[derive(Debug, Clone)]
struct DistillResult {
    output: CompressorOutput,
    valid_evidence_ids: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct PreparedCompressorArtifact {
    target_kind: MemoryArtifactKind,
    classifier: CompressorArtifactClassifier,
    promoted_payload: Option<serde_json::Value>,
    content: String,
    citations: Vec<String>,
    supersedes_ids: Vec<String>,
    derived_from_fact_ids: Vec<String>,
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
        self.sync_recall_for_turn(&namespace, thread, turn).await?;
        if turn.assistant_message.is_none() {
            return Ok(());
        }
        let should_force_catchup = self
            .frontier_needs_model_driven_catchup(&namespace, turn)
            .await?;
        if settings.enable_auto_distill {
            self.auto_distill_namespace(&namespace, thread, turn, settings)
                .await?;
        } else if should_force_catchup {
            self.force_model_driven_distill_namespace(&namespace, thread, turn, settings)
                .await?;
        }
        Ok(())
    }

    async fn sync_recall_for_turn(
        &self,
        namespace: &str,
        thread: &Thread,
        turn: &Turn,
    ) -> Result<(), RuntimeError> {
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
        Ok(())
    }

    pub(crate) async fn auto_distill_namespace(
        &self,
        namespace_id: &str,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<(), RuntimeError> {
        if let Some(result) = self
            .run_model_driven_distill(namespace_id, thread, turn, settings)
            .await?
        {
            self.persist_compressor_output(
                namespace_id,
                &result.output,
                &result.valid_evidence_ids,
            )
            .await?;
            return Ok(());
        }
        self.persist_deterministic_distill(namespace_id).await
    }

    async fn force_model_driven_distill_namespace(
        &self,
        namespace_id: &str,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<(), RuntimeError> {
        if let Some(result) = self
            .run_model_driven_distill(namespace_id, thread, turn, settings)
            .await?
        {
            self.persist_compressor_output(
                namespace_id,
                &result.output,
                &result.valid_evidence_ids,
            )
            .await?;
        }
        Ok(())
    }

    async fn frontier_needs_model_driven_catchup(
        &self,
        namespace_id: &str,
        turn: &Turn,
    ) -> Result<bool, RuntimeError> {
        let latest_wake_pack = self
            .db
            .latest_wake_pack(namespace_id)
            .await
            .map_err(RuntimeError::from)?;
        Ok(match latest_wake_pack {
            Some(wake_pack) => wake_pack.created_at < turn.updated_at,
            None => true,
        })
    }

    async fn run_model_driven_distill(
        &self,
        namespace_id: &str,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<Option<DistillResult>, RuntimeError> {
        let thread_history = self
            .thread_history_entries_up_to_frontier(thread, turn)
            .await?;
        if thread_history.is_empty() {
            return Ok(None);
        }
        let valid_evidence_ids = thread_history
            .iter()
            .map(|entry| entry.entry_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let evidence = thread_history
            .iter()
            .cloned()
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
            .latest_wake_pack(namespace_id)
            .await
            .map_err(RuntimeError::from)?;
        let active_invariants = self
            .db
            .list_active_memory_invariants(namespace_id, 24)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .take(12)
            .map(|invariant| {
                json!({
                    "id": invariant.id,
                    "scope": invariant.scope,
                    "claim": invariant.claim,
                    "support_excerpt": invariant.support_excerpt,
                    "falsifier": invariant.falsifier,
                    "fact_ids": invariant.fact_ids,
                    "supersedes_ids": invariant.supersedes_ids,
                    "created_at": invariant.created_at,
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
                    self.model_name.clone(),
                    true,
                )
            };
        if matches!(&engine, ModelEngine::Stub(_)) || provider_name == "stub" {
            return Ok(Some(DistillResult {
                output: stub_compressor_output(),
                valid_evidence_ids,
            }));
        }

        let request = ModelExchangeRequest {
            model: model_name.clone(),
            messages: vec![
                ModelMessage {
                    role: "system".to_string(),
                    content: Some(MessageContent::Text(
                        "You are BetterClaw's memory compressor. Current invariants are the current best map of local reality. The new thread frontier is new evidence about that reality. Evaluate whether the current invariants still hold, and update them only where the new evidence shows reality more clearly or shows that reality has changed. Distill the local physics from the evidence into a compact wake pack plus cited invariant and drift artifacts. Be conservative, do not invent facts, and only cite entry ids present in the evidence. Emit the full current invariant set each time, but it is valid for that set to remain unchanged if the new evidence does not change local reality. Do not describe invariants as objects; use them as claims about local reality. Do not emit meta-invariants about invariants, wake packs, compression, memory, or the prompt itself. It is acceptable to replace several older invariants with one cleaner invariant when the new invariant describes the same local reality more accurately and remains grounded in the underlying evidence."
                            .to_string(),
                    )),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ModelMessage {
                    role: "user".to_string(),
                    content: Some(MessageContent::Text(
                        json!({
                            "task": "distill_thread_frontier",
                            "namespace_id": namespace_id,
                            "thread_id": thread.id,
                            "frontier_turn_id": turn.id,
                            "previous_wake_pack": prior_wake_pack.as_ref().map(|wake_pack| wake_pack.content.clone()),
                            "active_invariants": active_invariants,
                            "thread_history_up_to_frontier": evidence,
                            "output_contract": {
                                "wake_pack": "string",
                                "facts": [{
                                    "fact_id":"string",
                                    "text":"string",
                                    "citations":["entry_id"],
                                    "support_excerpt":"string",
                                    "falsifier":"string"
                                }],
                                "invariant_self": [{
                                    "text":"string",
                                    "citations":["entry_id"],
                                    "support_excerpt":"string",
                                    "falsifier":"string",
                                    "why_it_holds":"string",
                                    "classifier":"empirical_invariant|policy|preference|hypothesis",
                                    "supersedes_ids":["artifact_id"],
                                    "derived_from_fact_ids":["fact_id"]
                                }],
                                "invariant_user": [{
                                    "text":"string",
                                    "citations":["entry_id"],
                                    "support_excerpt":"string",
                                    "falsifier":"string",
                                    "why_it_holds":"string",
                                    "classifier":"empirical_invariant|policy|preference|hypothesis",
                                    "supersedes_ids":["artifact_id"],
                                    "derived_from_fact_ids":["fact_id"]
                                }],
                                "invariant_relationship": [{
                                    "text":"string",
                                    "citations":["entry_id"],
                                    "support_excerpt":"string",
                                    "falsifier":"string",
                                    "why_it_holds":"string",
                                    "classifier":"empirical_invariant|policy|preference|hypothesis",
                                    "supersedes_ids":["artifact_id"],
                                    "derived_from_fact_ids":["fact_id"]
                                }],
                                "drift_flags": [{"text":"string","citations":["entry_id"]}],
                                "drift_contradictions": [{"text":"string","citations":["entry_id"]}],
                                "drift_merges": [{"text":"string","citations":["entry_id"]}],
                                "summary": "optional string"
                            }
                        })
                        .to_string(),
                    )),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: Vec::new(),
            max_tokens: Some(1400),
            stream: false,
            	response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "betterclaw_memory_distill",
                    "strict": true,
                    "schema": {
                        "type": "object",
                        "properties": {
                            "wake_pack": { "type": "string" },
                            "facts": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "fact_id": { "type": "string" },
                                        "text": { "type": "string" },
                                        "citations": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        },
                                        "support_excerpt": { "type": ["string", "null"] },
                                        "falsifier": { "type": ["string", "null"] }
                                    },
                                    "required": ["fact_id", "text", "citations"],
                                    "additionalProperties": false
                                }
                            },
                            "invariant_self": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/item" }
                            },
                            "invariant_user": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/item" }
                            },
                            "invariant_relationship": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/item" }
                            },
                            "drift_flags": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/drift_item" }
                            },
                            "drift_contradictions": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/drift_item" }
                            },
                            "drift_merges": {
                                "type": "array",
                                "items": { "$ref": "#/$defs/drift_item" }
                            },
                            "summary": { "type": ["string", "null"] }
                        },
                        "required": ["wake_pack", "facts", "invariant_self", "invariant_user", "invariant_relationship", "drift_flags", "drift_contradictions", "drift_merges"],
                        "$defs": {
                            "item": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "citations": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "support_excerpt": { "type": ["string", "null"] },
                                    "falsifier": { "type": ["string", "null"] },
                                    "why_it_holds": { "type": ["string", "null"] },
                                    "classifier": {
                                        "type": ["string", "null"],
                                        "enum": ["empirical_invariant", "policy", "preference", "hypothesis", null]
                                    },
                                    "supersedes_ids": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "derived_from_fact_ids": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    }
                                },
                                "required": ["text", "citations"],
                                "additionalProperties": false
                            },
                            "drift_item": {
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
                            }
                        },
                        "additionalProperties": false
                    }
                }
            })),
            extra: json!({}),
        };
        let attempt = self
            .run_compressor_exchange(
                turn,
                thread,
                &settings.agent_id,
                &thread.channel,
                &engine,
                used_fallback_engine,
                request.clone(),
            )
            .await?;
        let Some(exchange) = attempt else {
            return Ok(None);
        };
        if let Some(parsed) = self
            .parse_or_repair_compressor_output(
                turn,
                thread,
                &settings.agent_id,
                &thread.channel,
                &engine,
                used_fallback_engine,
                &request,
                &exchange,
            )
            .await?
        {
            let _ = provider_name;
            return Ok(Some(DistillResult {
                output: parsed,
                valid_evidence_ids,
            }));
        }
        Ok(None)
    }

    async fn run_compressor_exchange(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        engine: &ModelEngine,
        used_fallback_engine: bool,
        request: ModelExchangeRequest,
    ) -> Result<Option<ModelExchangeResult>, RuntimeError> {
        let exchange = if used_fallback_engine {
            self.run_and_record_exchange(turn, thread, agent_id, channel, request)
                .await?
        } else {
            match engine.run(request).await {
                Ok(exchange) => exchange,
                Err(error) => {
                    self.record_trace(turn, thread, agent_id, channel, error.exchange())
                        .await?;
                    return Ok(None);
                }
            }
        };
        self.record_trace(turn, thread, agent_id, channel, &exchange)
            .await?;
        Ok(Some(exchange))
    }

    async fn parse_or_repair_compressor_output(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        engine: &ModelEngine,
        used_fallback_engine: bool,
        request: &ModelExchangeRequest,
        exchange: &ModelExchangeResult,
    ) -> Result<Option<CompressorOutput>, RuntimeError> {
        let primary_content = exchange.content.as_deref();
        tracing::info!(
            target: "betterclaw::memory",
            kind = "compressor_primary_attempt",
            thread_id = %thread.id,
            turn_id = %turn.id,
            engine = %engine.kind_name(),
            "starting primary compressor parse/validation"
        );
        let parse_error = match primary_content {
            Some(content) => match parse_compressor_output(content) {
                Ok(parsed) if !parsed.wake_pack.trim().is_empty() => return Ok(Some(parsed)),
                Ok(_) => RuntimeError::ModelParse("compressor returned empty wake_pack".to_string()),
                Err(error) => error,
            },
            None => RuntimeError::ModelParse("compressor returned no content".to_string()),
        };

        tracing::info!(
            target: "betterclaw::memory",
            kind = "compressor_primary_parse_failed",
            thread_id = %thread.id,
            turn_id = %turn.id,
            engine = %engine.kind_name(),
            parse_error = %format!("{}", parse_error),
            "primary parse failed; attempting one-shot repair"
        );

        let repair_request = build_compressor_repair_request(
            request,
            primary_content,
            &format!("{}", parse_error),
        );
        let Some(repair_exchange) = self
            .run_compressor_exchange(
                turn,
                thread,
                agent_id,
                channel,
                engine,
                used_fallback_engine,
                repair_request,
            )
            .await?
        else {
            tracing::warn!(
                target: "betterclaw::memory",
                kind = "compressor_repair_skipped",
                thread_id = %thread.id,
                turn_id = %turn.id,
                engine = %engine.kind_name(),
                "repair exchange could not be run or recorded"
            );
            return Ok(None);
        };
        tracing::info!(
            target: "betterclaw::memory",
            kind = "compressor_repair_attempt",
            thread_id = %thread.id,
            turn_id = %turn.id,
            engine = %engine.kind_name(),
            "repair exchange completed; parsing repair content"
        );

        let Some(repair_content) = repair_exchange.content.as_deref() else {
            tracing::warn!(
                target: "betterclaw::memory",
                kind = "compressor_repair_no_content",
                thread_id = %thread.id,
                turn_id = %turn.id,
                engine = %engine.kind_name(),
                "repair exchange returned no content"
            );
            return Ok(None);
        };
        let parsed = match parse_compressor_output(repair_content) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!(
                    target: "betterclaw::memory",
                    kind = "compressor_repair_parse_failed",
                    thread_id = %thread.id,
                    turn_id = %turn.id,
                    engine = %engine.kind_name(),
                    parse_error = %format!("{}", err),
                    "repair parse failed; skipping this distill pass"
                );
                return Ok(None);
            }
        };
        if parsed.wake_pack.trim().is_empty() {
            tracing::info!(
                target: "betterclaw::memory",
                kind = "compressor_repair_empty_wakepack",
                thread_id = %thread.id,
                turn_id = %turn.id,
                engine = %engine.kind_name(),
                "repair produced empty wake_pack; skipping"
            );
            return Ok(None);
        }
        Ok(Some(parsed))
    }

    async fn persist_compressor_output(
        &self,
        namespace_id: &str,
        output: &CompressorOutput,
        valid_evidence_ids: &std::collections::BTreeSet<String>,
    ) -> Result<(), RuntimeError> {
        let facts = self
            .persist_compressor_facts(namespace_id, &output.facts, valid_evidence_ids)
            .await?;
        let mut promoted_invariants = Vec::new();
        promoted_invariants.extend(
            self.persist_compressor_artifact_list(
                namespace_id,
                MemoryArtifactKind::InvariantSelfV0,
                &output.invariant_self,
                &facts,
                valid_evidence_ids,
            )
            .await?,
        );
        promoted_invariants.extend(
            self.persist_compressor_artifact_list(
                namespace_id,
                MemoryArtifactKind::InvariantUserV0,
                &output.invariant_user,
                &facts,
                valid_evidence_ids,
            )
            .await?,
        );
        promoted_invariants.extend(
            self.persist_compressor_artifact_list(
                namespace_id,
                MemoryArtifactKind::InvariantRelationshipV0,
                &output.invariant_relationship,
                &facts,
                valid_evidence_ids,
            )
            .await?,
        );
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftFlagV0,
            &output.drift_flags,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftContradictionV0,
            &output.drift_contradictions,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.persist_compressor_artifact_list(
            namespace_id,
            MemoryArtifactKind::DriftMergeV0,
            &output.drift_merges,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.persist_compressor_drift_items(
            namespace_id,
            "flag",
            &output.drift_flags,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.persist_compressor_drift_items(
            namespace_id,
            "contradiction",
            &output.drift_contradictions,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.persist_compressor_drift_items(
            namespace_id,
            "merge",
            &output.drift_merges,
            &facts,
            valid_evidence_ids,
        )
        .await?;
        self.db
            .insert_wake_pack_v2(
                namespace_id,
                &output.wake_pack,
                output.summary.as_deref(),
                &promoted_invariants,
            )
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    async fn persist_compressor_artifact_list(
        &self,
        namespace_id: &str,
        kind: MemoryArtifactKind,
        items: &[CompressorArtifactSpec],
        fact_ids: &std::collections::BTreeMap<String, String>,
        valid_evidence_ids: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<String>, RuntimeError> {
        let active_invariant_ids = self
            .db
            .list_active_memory_invariants(namespace_id, 256)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .map(|invariant| invariant.id)
            .collect::<std::collections::BTreeSet<_>>();
        let mut promoted = Vec::new();
        for item in items {
            let Some(prepared) = prepare_compressor_artifact(
                kind.clone(),
                item,
                valid_evidence_ids,
                &active_invariant_ids,
                fact_ids,
            ) else {
                continue;
            };
            let candidate_v2_id = self
                .db
                .insert_invariant_candidate(
                    namespace_id,
                    kind.as_str(),
                    kind_classifier_label(prepared.classifier),
                    &prepared.content,
                    item.support_excerpt.as_deref(),
                    item.falsifier.as_deref(),
                    &prepared.derived_from_fact_ids,
                )
                .await
                .map_err(RuntimeError::from)?;
            if let Some(promoted_payload) = prepared.promoted_payload {
                let invariant_id = self
                    .db
                    .insert_memory_invariant(
                        namespace_id,
                        invariant_scope(kind.clone()),
                        &prepared.content,
                        promoted_payload
                            .get("support_excerpt")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default(),
                        promoted_payload
                            .get("falsifier")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default(),
                        &prepared.derived_from_fact_ids,
                        &prepared.supersedes_ids,
                    )
                    .await
                    .map_err(RuntimeError::from)?;
                promoted.push(invariant_id);
            } else if !matches!(
                prepared.target_kind,
                MemoryArtifactKind::InvariantCandidateV0
            ) {
                match prepared.target_kind {
                    MemoryArtifactKind::PolicyV0 => {
                        self.db
                            .insert_memory_policy(
                                namespace_id,
                                &prepared.content,
                                item.support_excerpt.as_deref(),
                                Some(&candidate_v2_id),
                            )
                            .await
                            .map_err(RuntimeError::from)?;
                    }
                    MemoryArtifactKind::PreferenceV0 => {
                        self.db
                            .insert_memory_preference(
                                namespace_id,
                                &prepared.content,
                                item.support_excerpt.as_deref(),
                                Some(&candidate_v2_id),
                            )
                            .await
                            .map_err(RuntimeError::from)?;
                    }
                    MemoryArtifactKind::HypothesisV0 => {
                        self.db
                            .insert_memory_hypothesis(
                                namespace_id,
                                &prepared.content,
                                item.support_excerpt.as_deref(),
                                item.falsifier.as_deref(),
                                Some(&candidate_v2_id),
                            )
                            .await
                            .map_err(RuntimeError::from)?;
                    }
                    _ => {}
                }
                let _ = candidate_v2_id;
            }
        }
        Ok(promoted)
    }

    async fn persist_compressor_drift_items(
        &self,
        namespace_id: &str,
        drift_kind: &str,
        items: &[CompressorArtifactSpec],
        fact_ids: &std::collections::BTreeMap<String, String>,
        valid_evidence_ids: &std::collections::BTreeSet<String>,
    ) -> Result<(), RuntimeError> {
        let active_invariant_ids = self
            .db
            .list_active_memory_invariants(namespace_id, 256)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .map(|invariant| invariant.id)
            .collect::<std::collections::BTreeSet<_>>();
        for item in items {
            let Some(prepared) = prepare_compressor_artifact(
                MemoryArtifactKind::InvariantCandidateV0,
                item,
                valid_evidence_ids,
                &active_invariant_ids,
                fact_ids,
            ) else {
                continue;
            };
            self.db
                .insert_memory_drift_item(
                    namespace_id,
                    drift_kind,
                    &prepared.content,
                    item.support_excerpt.as_deref(),
                    item.falsifier.as_deref(),
                    &prepared.citations,
                    &prepared.derived_from_fact_ids,
                )
                .await
                .map_err(RuntimeError::from)?;
        }
        Ok(())
    }

    async fn persist_compressor_facts(
        &self,
        namespace_id: &str,
        facts: &[CompressorFactSpec],
        valid_evidence_ids: &std::collections::BTreeSet<String>,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        let mut persisted = std::collections::BTreeMap::new();
        for fact in facts {
            if fact.fact_id.trim().is_empty() || fact.text.trim().is_empty() {
                continue;
            }
            let citations = fact
                .citations
                .iter()
                .filter(|citation| valid_evidence_ids.contains(*citation))
                .cloned()
                .collect::<Vec<_>>();
            let support_excerpt = fact
                .support_excerpt
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let falsifier = fact
                .falsifier
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if citations.is_empty() || support_excerpt.is_none() || falsifier.is_none() {
                continue;
            }
            let persisted_fact_id = self
                .db
                .insert_memory_fact(
                    namespace_id,
                    &fact.fact_id,
                    &fact.text,
                    support_excerpt.as_deref().unwrap_or_default(),
                    falsifier.as_deref().unwrap_or_default(),
                    None,
                    &citations,
                )
                .await
                .map_err(RuntimeError::from)?;
            persisted.insert(fact.fact_id.clone(), persisted_fact_id);
        }
        Ok(persisted)
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
        self.db
            .insert_wake_pack_v2(namespace_id, &content, None, &Vec::new())
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    pub async fn rebuild_memory_namespace(
        &self,
        namespace_id: &str,
    ) -> Result<MemoryRebuildReport, RuntimeError> {
        self.db
            .clear_rebuilt_memory(namespace_id)
            .await
            .map_err(RuntimeError::from)?;
        let settings = self.get_runtime_settings("default").await?;
        let threads = self.db.list_threads().await.map_err(RuntimeError::from)?;
        let mut replay_work = Vec::new();
        let mut frontier_work = Vec::new();
        for thread in threads.iter() {
            let turns = self
                .db
                .list_thread_turns(&thread.id)
                .await
                .map_err(RuntimeError::from)?;
            for turn in &turns {
                replay_work.push((thread.clone(), turn.clone()));
            }
            let frontier_turns = rebuild_frontier_turns(&turns);
            let frontier_total = frontier_turns.len();
            for (frontier_index, turn) in frontier_turns.into_iter().enumerate() {
                frontier_work.push((
                    turn.created_at,
                    thread.clone(),
                    turn,
                    frontier_index + 1,
                    frontier_total,
                ));
            }
        }
        replay_work.sort_by_key(|(_, turn)| turn.created_at);
        frontier_work.sort_by_key(|(created_at, _, _, _, _)| *created_at);
        let mut replay_settings = settings.clone();
        replay_settings.enable_auto_distill = true;
        let frontier_total = frontier_work.len();
        let mut next_frontier_index = 0usize;
        for (thread, turn) in replay_work.iter() {
            self.sync_recall_for_turn(namespace_id, thread, turn)
                .await?;
            while let Some((
                _,
                frontier_thread,
                frontier_turn,
                thread_frontier_index,
                thread_frontier_total,
            )) = frontier_work.get(next_frontier_index)
            {
                if frontier_turn.id != turn.id {
                    break;
                }
                eprintln!(
                    "memory-rebuild progress: frontier {}/{} thread_frontier {}/{} thread_id={} turn_id={}",
                    next_frontier_index + 1,
                    frontier_total,
                    thread_frontier_index,
                    thread_frontier_total,
                    frontier_thread.id,
                    frontier_turn.id
                );
                self.force_model_driven_distill_namespace(
                    namespace_id,
                    frontier_thread,
                    frontier_turn,
                    &replay_settings,
                )
                .await?;
                next_frontier_index += 1;
            }
        }
        Ok(MemoryRebuildReport {
            namespace_id: namespace_id.to_string(),
            turns_processed: replay_work.len(),
        })
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
            .list_turn_events(&turn.id)
            .await
            .map_err(RuntimeError::from)?;
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

    async fn thread_history_entries_up_to_frontier(
        &self,
        thread: &Thread,
        frontier_turn: &Turn,
    ) -> Result<Vec<LedgerEntry>, RuntimeError> {
        let mut entries = Vec::new();
        for turn in self
            .db
            .list_thread_turns(&thread.id)
            .await
            .map_err(RuntimeError::from)?
        {
            entries.extend(self.normalized_entries_for_turn(thread, &turn).await?);
            if turn.id == frontier_turn.id {
                break;
            }
        }
        entries.sort_by_key(|entry| entry.created_at);
        Ok(entries)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryRebuildReport {
    pub namespace_id: String,
    pub turns_processed: usize,
}

fn rebuild_frontier_turns(turns: &[Turn]) -> Vec<Turn> {
    let mut frontiers = Vec::new();
    let mut pending_frontier: Option<Turn> = None;
    for turn in turns {
        if !turn.user_message.trim().is_empty() {
            if let Some(frontier) = pending_frontier.take() {
                frontiers.push(frontier);
            }
        }
        if turn.assistant_message.is_some() {
            pending_frontier = Some(turn.clone());
        }
    }
    if let Some(frontier) = pending_frontier {
        frontiers.push(frontier);
    }
    frontiers
}

#[cfg(test)]
mod rebuild_frontier_tests {
    use super::rebuild_frontier_turns;
    use crate::turn::Turn;
    use crate::turn::TurnStatus;
    use chrono::Utc;

    fn turn(id: &str, user: &str, assistant: Option<&str>) -> Turn {
        Turn {
            id: id.to_string(),
            thread_id: "thread".to_string(),
            status: TurnStatus::Succeeded,
            user_message: user.to_string(),
            attachments_json: None,
            assistant_message: assistant.map(str::to_string),
            error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn rebuild_frontiers_use_next_non_empty_user_as_boundary() {
        let turns = vec![
            turn("u1", "U1", None),
            turn("a1", "", Some("A1")),
            turn("a2", "", Some("A2")),
            turn("u2", "U2", None),
            turn("a3", "", Some("A3")),
            turn("a4", "", Some("A4")),
            turn("u3", "U3", None),
        ];
        let frontiers = rebuild_frontier_turns(&turns)
            .into_iter()
            .map(|turn| turn.id)
            .collect::<Vec<_>>();
        assert_eq!(frontiers, vec!["a2".to_string(), "a4".to_string()]);
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

fn build_compressor_repair_request(
    request: &ModelExchangeRequest,
    failed_content: Option<&str>,
    failure_reason: &str,
) -> ModelExchangeRequest {
    let mut repair_request = request.clone();
    let mut messages = repair_request.messages;
    if let Some(content) = failed_content {
        messages.push(ModelMessage {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text(content.to_string())),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    let prompt = if let Some(content) = failed_content {
        format!(
            "The previous assistant response failed validation.\nError: {}\nReturn only corrected JSON matching the schema. Preserve valid facts and invariants if possible.\nPrevious response:\n```json\n{}\n```",
            failure_reason, content
        )
    } else {
        format!(
            "The previous assistant response failed validation.\nError: {}\nReturn only corrected JSON matching the schema. Preserve valid facts and invariants if possible.\nThe previous response returned no content.",
            failure_reason
        )
    };
    messages.push(ModelMessage {
        role: "user".to_string(),
        content: Some(MessageContent::Text(prompt)),
        tool_calls: None,
        tool_call_id: None,
    });
    repair_request.messages = messages;
    repair_request
}

fn stub_compressor_output() -> CompressorOutput {
    CompressorOutput {
        wake_pack: "Stub compressor wake pack: preserve recent user intent and tool outcomes."
            .to_string(),
        facts: vec![
            CompressorFactSpec {
                fact_id: "fact-self-1".to_string(),
                text: "Tool-backed answers have been useful in the current runtime iteration."
                    .to_string(),
                citations: vec!["turn:stub:user".to_string()],
                support_excerpt: Some("Stub user turn asks about runtime behavior.".to_string()),
                falsifier: Some(
                    "Future runtime work shows tool-backed answers are not useful.".to_string(),
                ),
            },
            CompressorFactSpec {
                fact_id: "fact-user-1".to_string(),
                text: "The user is actively iterating on BetterClaw runtime behavior.".to_string(),
                citations: vec!["turn:stub:user".to_string()],
                support_excerpt: Some("Stub user turn asks about runtime behavior.".to_string()),
                falsifier: Some("Future turns stop focusing on runtime behavior.".to_string()),
            },
        ],
        invariant_self: vec![CompressorArtifactSpec {
            text: "BetterClaw should prefer tool-backed answers when tools materially help."
                .to_string(),
            citations: vec!["turn:stub:user".to_string()],
            support_excerpt: Some("User is iterating directly on runtime behavior.".to_string()),
            falsifier: Some("Tools stop materially improving answer quality.".to_string()),
            why_it_holds: Some(
                "The current runtime iteration repeatedly centers tool-backed behavior."
                    .to_string(),
            ),
            classifier: Some(CompressorArtifactClassifier::EmpiricalInvariant),
            supersedes_ids: Vec::new(),
            derived_from_fact_ids: vec!["fact-self-1".to_string()],
        }],
        invariant_user: vec![CompressorArtifactSpec {
            text: "The user is actively iterating on BetterClaw runtime behavior.".to_string(),
            citations: vec!["turn:stub:user".to_string()],
            support_excerpt: Some("Stub user turn asks about runtime behavior.".to_string()),
            falsifier: Some("Future turns stop focusing on runtime behavior.".to_string()),
            why_it_holds: Some(
                "The visible thread evidence is centered on runtime behavior changes.".to_string(),
            ),
            classifier: Some(CompressorArtifactClassifier::EmpiricalInvariant),
            supersedes_ids: Vec::new(),
            derived_from_fact_ids: vec!["fact-user-1".to_string()],
        }],
        invariant_relationship: Vec::new(),
        drift_flags: Vec::new(),
        drift_contradictions: Vec::new(),
        drift_merges: Vec::new(),
        summary: Some("Stub compressor distill complete.".to_string()),
    }
}

#[cfg(test)]
mod compressor_repair_tests {
    use super::build_compressor_repair_request;
    use crate::model::{MessageContent, ModelExchangeRequest, ModelMessage};
    use serde_json::json;

    #[test]
    fn repair_request_appends_fixup_turn_and_preserves_schema() {
        let request = ModelExchangeRequest {
            model: "gpt-5-mini".to_string(),
            messages: vec![
                ModelMessage {
                    role: "system".to_string(),
                    content: Some(MessageContent::Text("system".to_string())),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ModelMessage {
                    role: "user".to_string(),
                    content: Some(MessageContent::Text("user".to_string())),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: vec![],
            max_tokens: Some(123),
            stream: false,
            response_format: Some(json!({"type": "json_schema", "json_schema": {"name": "x", "strict": true, "schema": {"type": "object", "properties": {"wake_pack": {"type": "string"}}, "required": ["wake_pack"], "additionalProperties": false}}})),
            extra: json!({"foo": "bar"}),
        };

        let repaired = build_compressor_repair_request(
            &request,
            Some("{\"wake_pack\":\"oops\"}"),
            "compressor returned invalid JSON",
        );

        assert_eq!(repaired.model, request.model);
        assert_eq!(repaired.response_format, request.response_format);
        assert_eq!(repaired.extra, request.extra);
        assert_eq!(repaired.messages.len(), 4);
        assert_eq!(repaired.messages[2].role, "assistant");
        assert_eq!(
            repaired.messages[3].role,
            "user"
        );
        let repair_prompt = match repaired.messages[3].content.as_ref().expect("repair prompt") {
            MessageContent::Text(text) => text,
            _ => panic!("expected text prompt"),
        };
        assert!(repair_prompt.contains("compressor returned invalid JSON"));
        assert!(repair_prompt.contains("Return only corrected JSON matching the schema"));
    }
}

pub(crate) fn default_memory_namespace() -> String {
    "default".to_string()
}

fn prepare_compressor_artifact(
    kind: MemoryArtifactKind,
    item: &CompressorArtifactSpec,
    valid_evidence_ids: &std::collections::BTreeSet<String>,
    active_invariant_ids: &std::collections::BTreeSet<String>,
    fact_ids: &std::collections::BTreeMap<String, String>,
) -> Option<PreparedCompressorArtifact> {
    if item.text.trim().is_empty() {
        return None;
    }
    let classifier = item
        .classifier
        .unwrap_or(CompressorArtifactClassifier::EmpiricalInvariant);
    let citations = item
        .citations
        .iter()
        .filter(|citation| valid_evidence_ids.contains(*citation))
        .cloned()
        .collect::<Vec<_>>();
    let supersedes_ids = item
        .supersedes_ids
        .iter()
        .filter(|artifact_id| active_invariant_ids.contains(*artifact_id))
        .cloned()
        .collect::<Vec<_>>();
    let derived_from_fact_ids = item
        .derived_from_fact_ids
        .iter()
        .filter_map(|fact_id| fact_ids.get(fact_id).cloned())
        .collect::<Vec<_>>();
    let support_excerpt = item
        .support_excerpt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let falsifier = item
        .falsifier
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let candidate_payload = json!({
        "classifier": kind_classifier_label(classifier),
        "target_kind": kind.as_str(),
        "support_excerpt": support_excerpt,
        "falsifier": falsifier,
        "supersedes_ids": supersedes_ids,
        "derived_from_fact_ids": derived_from_fact_ids,
        "validation": {
            "citations_present": !citations.is_empty(),
            "support_excerpt_present": support_excerpt.is_some(),
            "falsifier_present": falsifier.is_some(),
            "derived_from_fact_ids_present": !derived_from_fact_ids.is_empty(),
        }
    });
    if !matches!(classifier, CompressorArtifactClassifier::EmpiricalInvariant) {
        return Some(PreparedCompressorArtifact {
            target_kind: classifier_artifact_kind(classifier),
            classifier,
            promoted_payload: None,
            content: item.text.clone(),
            citations,
            supersedes_ids,
            derived_from_fact_ids,
        });
    }
    let is_pass_through = !supersedes_ids.is_empty();
    if (!is_pass_through && citations.is_empty())
        || support_excerpt.is_none()
        || falsifier.is_none()
        || (!is_pass_through && derived_from_fact_ids.is_empty())
    {
        return Some(PreparedCompressorArtifact {
            target_kind: MemoryArtifactKind::HypothesisV0,
            classifier,
            promoted_payload: None,
            content: item.text.clone(),
            citations,
            supersedes_ids,
            derived_from_fact_ids,
        });
    }
    Some(PreparedCompressorArtifact {
        target_kind: kind,
        classifier,
        promoted_payload: Some(json!({
            "classifier": kind_classifier_label(classifier),
            "support_excerpt": support_excerpt,
            "falsifier": falsifier,
            "supersedes_ids": supersedes_ids,
            "derived_from_fact_ids": derived_from_fact_ids,
            "candidate_payload": candidate_payload,
        })),
        content: item.text.clone(),
        citations,
        supersedes_ids,
        derived_from_fact_ids,
    })
}

fn kind_classifier_label(classifier: CompressorArtifactClassifier) -> &'static str {
    match classifier {
        CompressorArtifactClassifier::EmpiricalInvariant => "empirical_invariant",
        CompressorArtifactClassifier::Policy => "policy",
        CompressorArtifactClassifier::Preference => "preference",
        CompressorArtifactClassifier::Hypothesis => "hypothesis",
    }
}

fn classifier_artifact_kind(classifier: CompressorArtifactClassifier) -> MemoryArtifactKind {
    match classifier {
        CompressorArtifactClassifier::EmpiricalInvariant => {
            MemoryArtifactKind::InvariantCandidateV0
        }
        CompressorArtifactClassifier::Policy => MemoryArtifactKind::PolicyV0,
        CompressorArtifactClassifier::Preference => MemoryArtifactKind::PreferenceV0,
        CompressorArtifactClassifier::Hypothesis => MemoryArtifactKind::HypothesisV0,
    }
}

fn invariant_scope(kind: MemoryArtifactKind) -> &'static str {
    match kind {
        MemoryArtifactKind::InvariantSelfV0 => "self",
        MemoryArtifactKind::InvariantUserV0 => "user",
        MemoryArtifactKind::InvariantRelationshipV0 => "relationship",
        MemoryArtifactKind::DriftFlagV0 => "drift_flag",
        MemoryArtifactKind::DriftContradictionV0 => "drift_contradiction",
        MemoryArtifactKind::DriftMergeV0 => "drift_merge",
        _ => "unknown",
    }
}

#[cfg(test)]
mod compressor_tests {
    use super::*;

    fn set(values: &[&str]) -> std::collections::BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn empirical_invariant_requires_evidence_support_and_falsifier_for_promotion() {
        let item = CompressorArtifactSpec {
            text: "A real invariant".to_string(),
            citations: vec!["turn:1:user".to_string()],
            support_excerpt: Some(
                "The user explicitly reported the recurring failure.".to_string(),
            ),
            falsifier: Some("The failure stops recurring across future turns.".to_string()),
            why_it_holds: Some(
                "The evidence describes a recurring failure mode rather than a one-off event."
                    .to_string(),
            ),
            classifier: Some(CompressorArtifactClassifier::EmpiricalInvariant),
            supersedes_ids: vec!["artifact-1".to_string()],
            derived_from_fact_ids: vec!["fact-1".to_string()],
        };

        let prepared = prepare_compressor_artifact(
            MemoryArtifactKind::InvariantSelfV0,
            &item,
            &set(&["turn:1:user"]),
            &set(&["artifact-1"]),
            &std::collections::BTreeMap::from([(
                "fact-1".to_string(),
                "fact-artifact-1".to_string(),
            )]),
        )
        .expect("item should produce artifact");

        assert_eq!(prepared.target_kind, MemoryArtifactKind::InvariantSelfV0);
        assert!(prepared.promoted_payload.is_some());
        assert_eq!(prepared.supersedes_ids, vec!["artifact-1".to_string()]);
        assert_eq!(
            prepared.derived_from_fact_ids,
            vec!["fact-artifact-1".to_string()]
        );
    }

    #[test]
    fn non_empirical_invariant_is_diverted_out_of_invariant_store() {
        let item = CompressorArtifactSpec {
            text: "POLICY-tier architecture".to_string(),
            citations: vec!["turn:1:user".to_string()],
            support_excerpt: Some("This was proposed as a design preference.".to_string()),
            falsifier: Some("A different policy would work better.".to_string()),
            why_it_holds: Some(
                "This is a preference statement, not an empirical invariant.".to_string(),
            ),
            classifier: Some(CompressorArtifactClassifier::Policy),
            supersedes_ids: Vec::new(),
            derived_from_fact_ids: Vec::new(),
        };

        let prepared = prepare_compressor_artifact(
            MemoryArtifactKind::InvariantSelfV0,
            &item,
            &set(&["turn:1:user"]),
            &set(&[]),
            &std::collections::BTreeMap::new(),
        )
        .expect("item should produce artifact");

        assert_eq!(prepared.target_kind, MemoryArtifactKind::PolicyV0);
        assert!(prepared.promoted_payload.is_none());
    }

    #[test]
    fn weak_empirical_invariant_falls_back_to_hypothesis() {
        let item = CompressorArtifactSpec {
            text: "A maybe-invariant".to_string(),
            citations: vec!["turn:missing:user".to_string()],
            support_excerpt: None,
            falsifier: None,
            why_it_holds: None,
            classifier: Some(CompressorArtifactClassifier::EmpiricalInvariant),
            supersedes_ids: Vec::new(),
            derived_from_fact_ids: Vec::new(),
        };

        let prepared = prepare_compressor_artifact(
            MemoryArtifactKind::InvariantRelationshipV0,
            &item,
            &set(&["turn:1:user"]),
            &set(&[]),
            &std::collections::BTreeMap::new(),
        )
        .expect("item should produce artifact");

        assert_eq!(prepared.target_kind, MemoryArtifactKind::HypothesisV0);
        assert!(prepared.promoted_payload.is_none());
        assert!(prepared.citations.is_empty());
    }

    #[test]
    fn empirical_invariant_without_fact_lineage_falls_back_to_hypothesis() {
        let item = CompressorArtifactSpec {
            text: "A not-yet-grounded invariant".to_string(),
            citations: vec!["turn:1:user".to_string()],
            support_excerpt: Some("User described the failure.".to_string()),
            falsifier: Some("Future evidence disproves the pattern.".to_string()),
            why_it_holds: Some(
                "There is not yet enough fact lineage to show this is stable.".to_string(),
            ),
            classifier: Some(CompressorArtifactClassifier::EmpiricalInvariant),
            supersedes_ids: Vec::new(),
            derived_from_fact_ids: vec!["missing-fact".to_string()],
        };

        let prepared = prepare_compressor_artifact(
            MemoryArtifactKind::InvariantUserV0,
            &item,
            &set(&["turn:1:user"]),
            &set(&[]),
            &std::collections::BTreeMap::new(),
        )
        .expect("item should produce artifact");

        assert_eq!(prepared.target_kind, MemoryArtifactKind::HypothesisV0);
        assert!(prepared.promoted_payload.is_none());
    }
}
