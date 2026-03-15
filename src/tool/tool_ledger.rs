use super::*;

use crate::error::RuntimeError;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct LedgerListTool;

#[async_trait]
impl Tool for LedgerListTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ledger_list".to_string(),
            description:
                "List cited ledger entries from runtime history with provenance-aware metadata."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "kind": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_string(params, "ledger_list", "thread_id")?;
        optional_string(params, "ledger_list", "kind")?;
        optional_usize(params, "ledger_list", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let thread_id = optional_string(&params, "ledger_list", "thread_id")?;
        let kind = optional_string(&params, "ledger_list", "kind")?;
        let limit = optional_usize(&params, "ledger_list", "limit")?.unwrap_or(20);
        let mut entries = tool_collect_ledger_entries(&context.db, thread_id.as_deref()).await?;
        if let Some(kind) = kind {
            entries.retain(|entry| entry.kind == kind);
        }
        let total = entries.len();
        let truncated = total > limit;
        let items = entries
            .into_iter()
            .rev()
            .take(limit)
            .map(|entry| {
                json!({
                    "entry_id": entry.entry_id,
                    "thread_id": entry.thread_id,
                    "turn_id": entry.turn_id,
                    "kind": entry.kind,
                    "created_at": entry.created_at,
                    "citation": entry.citation,
                    "summary": entry.content.as_deref().map(summarize_entry_content),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "entries": items,
            "entry_count": total,
            "truncated": truncated,
            "limit_reached": truncated.then_some(limit),
        }))
    }
}

pub struct LedgerGetTool;

#[async_trait]
impl Tool for LedgerGetTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ledger_get".to_string(),
            description:
                "Fetch one ledger entry with full content, provenance, and citation metadata."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "entry_id": { "type": "string" }
                },
                "required": ["entry_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "ledger_get", "entry_id").map(|_| ())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let entry_id = require_string(&params, "ledger_get", "entry_id")?;
        let entries = tool_collect_ledger_entries(&context.db, None).await?;
        let entry = entries
            .into_iter()
            .find(|entry| entry.entry_id == entry_id)
            .ok_or_else(|| RuntimeError::ToolExecution {
                tool: "ledger_get".to_string(),
                reason: format!("ledger entry not found: {entry_id}"),
            })?;
        Ok(json!({
            "entry_id": entry.entry_id,
            "thread_id": entry.thread_id,
            "turn_id": entry.turn_id,
            "kind": entry.kind,
            "created_at": entry.created_at,
            "citation": entry.citation,
            "content": entry.content,
            "payload": entry.payload,
        }))
    }
}

pub struct ConversationSearchTool;

#[async_trait]
impl Tool for ConversationSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "conversation_search".to_string(),
            description: "Search past conversation and runtime entries by semantic/keyword recall without ledger causal framing.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "conversation_search", "query")?;
        optional_usize(params, "conversation_search", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let query = require_string(&params, "conversation_search", "query")?;
        let limit = optional_usize(&params, "conversation_search", "limit")?.unwrap_or(8);
        let lexical_query = build_fts_query(&query);
        if lexical_query.is_empty() {
            return Ok(json!({
                "query": query,
                "results": [],
                "result_count": 0,
            }));
        }
        let hits = context
            .db
            .search_recall_chunks_keyword("default", &lexical_query, limit as i64)
            .await
            .map_err(RuntimeError::from)?;
        let results = hits
            .into_iter()
            .map(|hit| {
                json!({
                    "entry_id": hit.entry_id,
                    "source_type": hit.source_type,
                    "source_id": hit.source_id,
                    "content": hit.content,
                    "score": hit.score,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "query": query,
            "results": results,
            "result_count": results.len(),
        }))
    }
}
