//! Tools for reading the append-only ledger (ledger_events).
//!
//! These are intentionally read-only and scoped to the requesting user_id.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::context::JobContext;
use crate::db::Database;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let byte_offset = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    format!("{}...", &s[..byte_offset])
}

// ==================== ledger_list ====================

pub struct LedgerListTool {
    store: Arc<dyn Database>,
}

impl LedgerListTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for LedgerListTool {
    fn name(&self) -> &str {
        // OpenAI-compatible tool names cannot include dots.
        "ledger_list"
    }

    fn description(&self) -> &str {
        "List ledger events (append-only history) for the current user. Use this to browse \
         isnad chains and other derived artifacts stored in the ledger. For full content, \
         call ledger_get with an event_id."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind_prefix": {
                    "type": ["string", "null"],
                    "description": "Optional kind prefix filter (e.g. 'isnad.' or 'wake_pack.'). Null/omitted means no filter.",
                    "default": null
                },
                "limit": {
                    "type": "integer",
                    "description": "Max events to return (default 50, max 200).",
                    "default": 50,
                    "minimum": 1,
                    "maximum": 200
                },
                "skip": {
                    "type": "integer",
                    "description": "How many matching events to skip (offset) for pagination (default 0, max 10000).",
                    "default": 0,
                    "minimum": 0,
                    "maximum": 10000
                },
                "include_payload": {
                    "type": "boolean",
                    "description": "Include parsed payload JSON for each event (default false).",
                    "default": false
                },
                "include_content_preview": {
                    "type": "boolean",
                    "description": "Include a short content preview for each event (default true).",
                    "default": true
                }
            },
            "required": []
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let kind_prefix = params
            .get("kind_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .min(200) as i64;

        let skip = params
            .get("skip")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(10_000) as i64;

        let include_payload = params
            .get("include_payload")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let include_content_preview = params
            .get("include_content_preview")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let events = self
            .store
            .list_ledger_events_by_kind_prefix_page(&ctx.user_id, &kind_prefix, limit, skip)
            .await
            .map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to list ledger events: {e}"))
            })?;
        tracing::debug!(
            user_id = %ctx.user_id,
            kind_prefix = %kind_prefix,
            limit = limit,
            skip = skip,
            returned = events.len(),
            "ledger_list executed"
        );

        let items: Vec<serde_json::Value> = events
            .iter()
            .map(|e| {
                let content_preview = if include_content_preview {
                    e.content.as_deref().map(|c| truncate_chars(c, 800))
                } else {
                    None
                };
                serde_json::json!({
                    "id": e.id.to_string(),
                    "created_at": e.created_at.to_rfc3339(),
                    "kind": e.kind,
                    "source": e.source,
                    "episode_id": e.episode_id.map(|u| u.to_string()),
                    "has_content": e.content.as_deref().is_some_and(|c| !c.is_empty()),
                    "content_preview": content_preview,
                    "payload": if include_payload { Some(e.payload.clone()) } else { None },
                })
            })
            .collect();

        let out = serde_json::json!({
            "kind_prefix": kind_prefix,
            "skip": skip,
            "limit": limit,
            "count": items.len(),
            "events": items,
        });

        Ok(ToolOutput::success(out, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

// ==================== ledger_get ====================

pub struct LedgerGetTool {
    store: Arc<dyn Database>,
}

impl LedgerGetTool {
    pub fn new(store: Arc<dyn Database>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for LedgerGetTool {
    fn name(&self) -> &str {
        "ledger_get"
    }

    fn description(&self) -> &str {
        "Fetch a single ledger event by event_id for the current user."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "event_id": {
                    "type": "string",
                    "description": "Ledger event UUID."
                }
            },
            "required": ["event_id"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = std::time::Instant::now();

        let event_id = params
            .get("event_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolError::InvalidParameters("missing 'event_id' parameter".to_string())
            })?;

        let id = Uuid::parse_str(event_id)
            .map_err(|_| ToolError::InvalidParameters("event_id must be a UUID".to_string()))?;

        let ev = self
            .store
            .get_ledger_event_for_user(&ctx.user_id, id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to get ledger event: {e}")))?;
        tracing::debug!(
            user_id = %ctx.user_id,
            event_id = %id,
            found = ev.is_some(),
            "ledger_get executed"
        );

        let Some(ev) = ev else {
            let out = serde_json::json!({
                "found": false,
                "event_id": event_id,
            });
            return Ok(ToolOutput::success(out, start.elapsed()));
        };

        let out = serde_json::json!({
            "found": true,
            "event": {
                "id": ev.id.to_string(),
                "created_at": ev.created_at.to_rfc3339(),
                "kind": ev.kind,
                "source": ev.source,
                "episode_id": ev.episode_id.map(|u| u.to_string()),
                "content": ev.content,
                "payload": ev.payload,
                "sha256": ev.sha256,
            }
        });

        Ok(ToolOutput::success(out, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}
