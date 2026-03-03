//! Compressor role primitives (no loop yet).
//!
//! This module provides:
//! - A schema-constrained "delta" output via tool-call parameters (OpenAI-style function schema)
//! - Strongly-typed Rust structs for the delta
//!
//! We intentionally avoid persona language here: the compressor is a transformer.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::ledger::NewLedgerEvent;
use crate::llm::{ChatMessage, FinishReason, LlmProvider, ToolCall, ToolCompletionRequest, ToolDefinition};

pub const COMPRESSOR_DELTA_TOOL_NAME: &str = "compressor_delta_v0";

pub fn truncate_chars(s: &str, max_chars: usize) -> String {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakePackV0 {
    /// The exact text to inject as the first system message prefix.
    pub content: String,
    /// Citations for the wake pack (event_id required).
    pub citations: Vec<CitationV0>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionTypeV0 {
    CreateInvariant,
    UpdateInvariant,
    MarkContradicted,
    FlagDrift,
    MergeInvariants,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeV0 {
    Self_,
    User,
    Relationship,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CitationV0 {
    pub event_id: String,
    #[serde(default)]
    pub quote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateV0 {
    #[serde(default)]
    pub new_text: Option<String>,
    #[serde(default)]
    pub weight_delta: Option<f64>,
    #[serde(default)]
    pub new_weight: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeV0 {
    #[serde(default)]
    pub from_invariant_ids: Option<Vec<String>>,
    #[serde(default)]
    pub into_invariant_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionV0 {
    pub action_type: ActionTypeV0,
    #[serde(default)]
    pub scope: Option<ScopeV0>,
    pub confidence: f64,

    #[serde(default)]
    pub invariant_id: Option<String>,
    #[serde(default)]
    pub text: Option<String>,

    /// Optional fast-path for dedupe: treat this action as a duplicate of an existing invariant.
    #[serde(default)]
    pub duplicate_of: Option<String>,

    #[serde(default)]
    pub update: Option<UpdateV0>,
    #[serde(default)]
    pub merge: Option<MergeV0>,

    pub citations: Vec<CitationV0>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressorDeltaV0 {
    pub wake_pack: WakePackV0,
    pub actions: Vec<ActionV0>,
}

#[derive(Debug, Clone)]
pub struct MicroDistillParams {
    pub window_events: i64,
    pub anchor_invariants: i64,
    pub drift_candidates: i64,
    pub max_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct MicroDistillResult {
    pub delta: CompressorDeltaV0,
    pub wake_pack_event_id: Option<uuid::Uuid>,
    pub distill_event_id: Option<uuid::Uuid>,
}

fn format_events_for_prompt(events: &[crate::ledger::LedgerEvent]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str("- ");
        out.push_str(&format!(
            "{} {} {} {}\n",
            e.id,
            e.created_at.to_rfc3339(),
            e.kind,
            e.source
        ));
        if let Some(ref c) = e.content {
            out.push_str("  content: ");
            // Keep prompt bounded; this is not a dump.
            out.push_str(&truncate_chars(c, 2_000));
            out.push('\n');
        }
    }
    out
}

const COMPRESSOR_SYSTEM_PROMPT_V0: &str = r#"
You are the BetterClaw compressor subsystem.
You are a transformer over evidence (ledger events). You do not have a persona.

Goal: produce a small, conservative delta of actions over invariants/isnads and a wake_pack.v0 artifact.

Hard rules:
- Never invent facts.
- Every action MUST include citations with valid event_id values from the provided ledger window or anchors.
- The wake_pack content MUST only summarize what is supported by citations; do not add uncited claims.
- If you cannot cite evidence, do not create/update invariants; prefer flag_drift or do nothing.

Output constraints:
- Max 8 total actions.
- Max 2 create_invariant per scope.
- Prefer reweight/merge over rewriting text unless evidence is strong.
"#;

/// Run a single bounded "micro distill" pass.
///
/// If `commit=true`, appends:
/// - `wake_pack.v0` (content in `LedgerEvent.content`, citations in `payload`)
/// - `distill.micro` (actions + window ids + wake_pack_event_id)
pub async fn run_micro_distill_pass(
    store: &dyn crate::db::Database,
    compressor_llm: &dyn LlmProvider,
    user_id: &str,
    params: MicroDistillParams,
    commit: bool,
) -> Result<MicroDistillResult, LlmError> {
    // Local window (newest-first from DB); present oldest-first to the model.
    let mut local = store
        .list_recent_ledger_events(user_id, params.window_events)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to load ledger window: {e}"),
        })?;
    local.reverse();

    let mut invariants = store
        .list_recent_ledger_events_by_kind_prefix(user_id, "invariant.", params.anchor_invariants)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to load invariant anchors: {e}"),
        })?;
    invariants.reverse();

    let mut drift = store
        .list_recent_ledger_events_by_kind_prefix(user_id, "drift.", params.drift_candidates)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to load drift candidates: {e}"),
        })?;
    drift.reverse();

    let user_msg = format!(
        "# Evidence Window (Local)\n{}\n\n# Anchor Invariants (Recent)\n{}\n\n# Drift/Contradiction Candidates (Recent)\n{}\n",
        format_events_for_prompt(&local),
        format_events_for_prompt(&invariants),
        format_events_for_prompt(&drift),
    );

    let messages = vec![
        ChatMessage::system(COMPRESSOR_SYSTEM_PROMPT_V0.trim()),
        ChatMessage::user(user_msg),
    ];

    let delta = complete_delta_v0(
        compressor_llm,
        messages,
        None,
        params.max_tokens,
    )
    .await?;

    if !commit {
        return Ok(MicroDistillResult {
            delta,
            wake_pack_event_id: None,
            distill_event_id: None,
        });
    }

    let wake_payload = serde_json::json!({
        "citations": delta.wake_pack.citations,
    });

    let wake_event = NewLedgerEvent {
        user_id,
        episode_id: None,
        kind: "wake_pack.v0",
        source: "compressor",
        content: Some(delta.wake_pack.content.as_str()),
        payload: &wake_payload,
    };

    let wake_pack_event_id = store
        .append_ledger_event(&wake_event)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to commit wake_pack.v0: {e}"),
        })?;

    let payload = serde_json::json!({
        "actions": delta.actions,
        "wake_pack_event_id": wake_pack_event_id.to_string(),
        "window": {
            "local_event_ids": local.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            "anchor_invariant_ids": invariants.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            "drift_candidate_ids": drift.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
        }
    });

    let ev = NewLedgerEvent {
        user_id,
        episode_id: None,
        kind: "distill.micro",
        source: "compressor",
        content: None,
        payload: &payload,
    };

    let distill_event_id = store
        .append_ledger_event(&ev)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to commit distill.micro: {e}"),
        })?;

    Ok(MicroDistillResult {
        delta,
        wake_pack_event_id: Some(wake_pack_event_id),
        distill_event_id: Some(distill_event_id),
    })
}

pub fn compressor_delta_tool_schema_v0() -> ToolDefinition {
    // Note: RigAdapter normalizes tool schemas to OpenAI strict-mode compliance
    // (required fields, additionalProperties=false, nullable optionals).
    ToolDefinition {
        name: COMPRESSOR_DELTA_TOOL_NAME.to_string(),
        description: "Emit a bounded, cited delta for invariants/isnads. Conservative; no uncited claims.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "wake_pack": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "citations": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "event_id": { "type": "string" },
                                    "quote": { "type": "string" }
                                },
                                "required": ["event_id"]
                            }
                        }
                    },
                    "required": ["content", "citations"]
                },
                "actions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "action_type": {
                                "type": "string",
                                "enum": [
                                    "create_invariant",
                                    "update_invariant",
                                    "mark_contradicted",
                                    "flag_drift",
                                    "merge_invariants"
                                ]
                            },
                            "scope": {
                                "type": "string",
                                "enum": ["self", "user", "relationship"]
                            },
                            "confidence": { "type": "number" },
                            "invariant_id": { "type": "string" },
                            "text": { "type": "string" },
                            "duplicate_of": { "type": "string" },
                            "update": {
                                "type": "object",
                                "properties": {
                                    "new_text": { "type": "string" },
                                    "weight_delta": { "type": "number" },
                                    "new_weight": { "type": "number" }
                                }
                            },
                            "merge": {
                                "type": "object",
                                "properties": {
                                    "from_invariant_ids": {
                                        "type": "array",
                                        "items": { "type": "string" }
                                    },
                                    "into_invariant_id": { "type": "string" }
                                }
                            },
                            "citations": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "event_id": { "type": "string" },
                                        "quote": { "type": "string" }
                                    },
                                    "required": ["event_id"]
                                }
                            }
                        },
                        "required": ["action_type", "confidence", "citations"]
                    }
                }
            },
            "required": ["wake_pack", "actions"]
        }),
    }
}

/// Execute a schema-constrained compressor call using tool-calling as the enforcement mechanism.
///
/// This avoids relying on provider-specific `response_format` passthrough and works well with
/// OpenAI-compatible chat-completions endpoints.
pub async fn complete_delta_v0(
    llm: &dyn LlmProvider,
    messages: Vec<ChatMessage>,
    model_override: Option<&str>,
    max_tokens: u32,
) -> Result<CompressorDeltaV0, LlmError> {
    // The tool schema is the contract; we don't need "output JSON" prompting.
    let tool = compressor_delta_tool_schema_v0();
    let mut req = ToolCompletionRequest::new(messages.clone(), vec![tool])
        .with_max_tokens(max_tokens)
        .with_temperature(0.2)
        .with_tool_choice("required");

    if let Some(m) = model_override {
        req = req.with_model(m);
    }

    let resp = llm.complete_with_tools(req).await?;

    if resp.finish_reason != FinishReason::ToolUse && resp.tool_calls.is_empty() {
        return Err(LlmError::InvalidResponse {
            provider: "compressor".to_string(),
            reason: "Expected a tool call for compressor_delta_v0".to_string(),
        });
    }

    let tc: &ToolCall = resp
        .tool_calls
        .iter()
        .find(|tc| tc.name == COMPRESSOR_DELTA_TOOL_NAME)
        .ok_or_else(|| LlmError::InvalidResponse {
            provider: "compressor".to_string(),
            reason: "Missing compressor_delta_v0 tool call".to_string(),
        })?;

    serde_json::from_value::<CompressorDeltaV0>(tc.arguments.clone()).map_err(|e| {
        LlmError::InvalidResponse {
            provider: "compressor".to_string(),
            reason: format!("Failed to decode compressor delta: {e}"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rust_decimal::Decimal;

    struct FakeToolCallLlm;

    #[async_trait]
    impl LlmProvider for FakeToolCallLlm {
        fn model_name(&self) -> &str { "fake" }
        fn cost_per_token(&self) -> (Decimal, Decimal) { (Decimal::ZERO, Decimal::ZERO) }

        async fn complete(&self, _request: crate::llm::CompletionRequest) -> Result<crate::llm::CompletionResponse, LlmError> {
            Err(LlmError::RequestFailed{ provider: "fake".to_string(), reason: "not implemented".to_string()})
        }

    async fn complete_with_tools(
            &self,
            _request: ToolCompletionRequest,
        ) -> Result<crate::llm::ToolCompletionResponse, LlmError> {
            Ok(crate::llm::ToolCompletionResponse {
                content: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: COMPRESSOR_DELTA_TOOL_NAME.to_string(),
                    arguments: serde_json::json!({
                        "wake_pack": {
                            "content": "# Wake Pack (v0)\n\n- Example\n",
                            "citations": [{"event_id":"00000000-0000-0000-0000-000000000000"}]
                        },
                        "actions": [{
                            "action_type": "flag_drift",
                            "confidence": 0.5,
                            "citations": [{"event_id":"00000000-0000-0000-0000-000000000000"}]
                        }]
                    }),
                }],
                input_tokens: 1,
                output_tokens: 1,
                finish_reason: FinishReason::ToolUse,
            })
        }

        async fn model_metadata(&self) -> Result<crate::llm::ModelMetadata, LlmError> {
            Ok(crate::llm::ModelMetadata { id: "fake".to_string(), context_length: None })
        }

        fn active_model_name(&self) -> String { "fake".to_string() }
    }

    #[tokio::test]
    async fn parses_tool_arguments_into_delta() {
        let llm = FakeToolCallLlm;
        let delta = complete_delta_v0(
            &llm,
            vec![ChatMessage::user("hi")],
            None,
            512,
        )
        .await
        .expect("delta");
        assert_eq!(delta.actions.len(), 1);
    }
}
