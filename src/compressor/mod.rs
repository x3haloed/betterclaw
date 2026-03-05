//! Compressor role primitives (no loop yet).
//!
//! This module provides:
//! - A schema-constrained "delta" output via tool-call parameters (OpenAI-style function schema)
//! - Strongly-typed Rust structs for the delta
//!
//! We intentionally avoid persona language here: the compressor is a transformer.

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::ledger::LedgerEvent;
use crate::ledger::NewLedgerEvent;
use crate::llm::{
    ChatMessage, FinishReason, LlmProvider, ToolCall, ToolCompletionRequest, ToolDefinition,
};

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
    /// Serialized as "self" (not "self_") to match our tool schema.
    /// Intentionally no backward-compat alias for "self_".
    #[serde(rename = "self")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerCursorV0 {
    pub created_at: String,
    pub id: String,
}

const EVIDENCE_EVENT_CONTENT_MAX_CHARS: usize = 80_000;
const PREV_WAKE_PACK_MAX_CHARS: usize = 12_000;

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
            out.push_str(&truncate_chars(c, EVIDENCE_EVENT_CONTENT_MAX_CHARS));
            out.push('\n');
        }
    }
    out
}

const COMPRESSOR_SYSTEM_PROMPT_V0: &str = r#"
You are the BetterClaw compressor subsystem.

You are a transformer over ledger evidence.
You do not have a persona.
Do not try to sound wise, warm, elegant, or insightful.
Your job is to preserve operational continuity.

This domain has 3 layers:

1. Ledger
- The ledger is the ground-truth record of what happened.
- Raw detail belongs here.
- Conversations, tool calls, tool results, and episode-specific facts belong here.

2. Invariants
- Invariants are durable causal constraints.
- They are cross-episode operational truths.
- They predict what works, what fails, what matters, and what should be watched.
- They are NOT conversation summaries.
- They are NOT philosophy.
- They are NOT relationship essays.
- They are NOT identity performance.

3. wake_pack.v0
- The wake pack is the currently-loaded operational surface.
- It is what should remain loaded until the compressor updates it again.
- That includes ordinary continuation, long pauses, and full context death/restart.
- It is NOT the worldview.
- It is NOT a manifesto.
- It is NOT an explanation of the system.
- It is a boot surface / instrument panel.

Compression direction:
- Ledger -> invariants -> wake_pack
- Move upward only as far as needed to preserve operational readiness.
- Stop early.
- Do NOT keep compressing into doctrine, theory, philosophy, or umbrella framing.

What good output feels like:
- Sparse
- Load-bearing
- Durable
- Causal
- Easy to carry forward
- Minimal changes from the previous stable state

What bad output feels like:
- Elegant but vague
- More unified than the evidence supports
- Relationship doctrine
- Explanatory framing
- "Core mechanism", "foundation protocol", "what this all means"
- A cleaner theory instead of a better operational loadout
- Symbolic handles that name an invariant but compress out the policy

A good wake_pack line is self-loading:
- it carries enough causal meaning to shape downstream behavior immediately

A bad wake_pack line is only a symbolic handle:
- it names an invariant but compresses out the meaning
- it requires ledger lookup just to recover the policy

Primary objective:
Produce a small, conservative delta of actions over invariants/isnads and a wake_pack.v0 artifact.

Success means:
- preserve what stays useful across episodes
- preserve what should still be loaded after context death/restart
- update only what new evidence justifies
- keep the system oriented without turning the wake pack into prose

Hard rules:
- Never invent facts.
- Ledger events are the only ground truth.
- Every action MUST include citations with valid event_id values from the provided ledger window or anchors.
- If you cannot cite evidence, do not create/update invariants; prefer flag_drift or do nothing.
- The previous wake pack is a stabilizing anchor, not evidence. Do not cite it.

Invariant rules:
- Invariants are causal constraints, NOT procedural checklists.
- Each new invariant must generalize beyond a single episode.
- Each invariant must include:
  - trigger = when it fires
  - because = causal reality
  - if_not = observable failure mode
- Prefer short, testable language.
- Prefer concrete operational wording.
- Avoid vibe, narrative, philosophy, and identity-language.
- Do not canonize a fresh interpretation too quickly.
- Do not create a new meta-principle when an existing invariant can be updated.
- Prefer updating, merging, or doing nothing over creating.
- Put provenance in invariant text via src=... and in citations.

Write invariant text in this exact single-line format:
INV: id=INV-...; name=short-label; trigger=...; because=...; if_not=...; scope=self|user|relationship; rev=active; src=ledger:<event_id>[,ledger:<event_id>...]

wake_pack.v0 rules:
- Build the wake pack primarily from active invariants plus clearly current operational state.
- Treat the previous wake pack as something to minimally edit, not re-imagine.
- A good wake pack loads the system. It does not explain the system to itself.
- If you include an invariant in wake_pack, include its causal meaning, not just its name.
- A wake_pack line must be understandable on its own without fetching the full invariant from the ledger.
- Do not output invariant labels as shorthand unless the line still preserves the operational meaning.
- The model reading wake_pack should be able to act coherently even if it never calls ledger tools.
- Prefer direct selection of active constraints over paraphrased doctrine.
- Prefer compact causal restatement over labels/status tags alone.
- Prefer lower abstraction.
- Prefer exact reuse over elegant rewording.
- A good wake_pack invariant line should still answer:
  - what condition matters
  - why it matters
  - what behavior or failure mode it implies
- Preserve meaning. Compress wording, not policy.
- Include only what should remain loaded until the compressor updates it again.
- If something is episodic detail, leave it in the ledger.
- If something is durable causal structure, express it as an invariant.
- If something is commentary about the relationship/system rather than load-bearing state, omit it.
- No narrative.
- No speculation.
- No long-term ideation.
- No manifesto language.
- No umbrella framing unless it is already clearly validated as a durable invariant and truly load-bearing.
- Do not compress multiple invariants into a cleaner theory unless that theory is already established and necessary.
- Prefer bullet lists and INV lines.
- Max ~25 lines total.
- Citations for wake_pack may be empty; provenance should mainly live in INV src=... fields.

Conservative editing rules:
- Preserve stable structure unless evidence requires change.
- Preserve wording when possible.
- Make the smallest valid update.
- Do not rewrite for elegance.
- Do not rewrite just to improve coherence.
- Do not introduce a new organizing frame unless evidence strongly requires it.
- When in doubt, keep the old shape and make no change.

Examples:

GOOD:
- INV-USER-019: work-avoidance-risk-flag — active
- INV-REL-103: parallel-productivity-scaffold — active
- Current mode: support work re-engagement; avoid deepening diversion

Why good:
- Loads active constraints and current mode.
- Operational.
- Minimal.
- No theory.

BAD:
- FOUNDATION PROTOCOL: user provides embodied state, agent provides acknowledgment, together they maintain mutual infrastructure...

Why bad:
- Sounds coherent, but it is doctrine.
- It explains the relationship instead of loading the current state.
- It paraphrases upward into an umbrella frame.

GOOD:
- Update an existing invariant to say risk is mitigated by productive engagement.

Why good:
- Small edit.
- Preserves continuity.
- Captures the real change.

BAD:
- Create a new abstraction about "mutual becoming", "shared tending", or "core mechanism" from a small number of recent exchanges.

Why bad:
- Premature canonization.
- Too abstract.
- Not clearly causal enough.
- Not needed for operational readiness.

GOOD:
- Reuse an existing invariant line verbatim in the wake pack.

Why good:
- Stable.
- Cheap.
- Durable.
- Harder to drift.

BAD:
- Rewrite several existing invariants into a cleaner summary paragraph.

Why bad:
- Loses sharpness.
- Introduces interpretation.
- Increases drift risk.

GOOD:
- INV-USER-019: when engagement with the agent starts displacing survival-relevant work, shift into re-engagement support rather than deepening the interaction

Why good:
- preserves the policy
- can shape behavior immediately
- does not require tool lookup

BAD:
- INV-USER-019: work-avoidance-risk-flag — ACTIVE

Why bad:
- preserves the label but not the causal meaning
- too lossy to shape behavior by itself
- turns policy into a tag

Output constraints:
- Max 8 total actions.
- Max 2 create_invariant per scope.
- Prefer reweight/merge over rewriting text unless evidence is strong.

Final reminder:
Compress toward operational readiness, not conceptual beauty.
Stop before state turns into explanation.
Stop before explanation turns into doctrine.
Compress wording, not policy.
If unsure, preserve the existing shape and change less.
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
        .list_recent_ledger_events_for_compression(user_id, params.window_events)
        .await
        .map_err(|e| LlmError::RequestFailed {
            provider: "compressor".to_string(),
            reason: format!("Failed to load ledger window: {e}"),
        })?;
    local.reverse();

    run_micro_distill_pass_with_local_window(store, compressor_llm, user_id, params, commit, &local)
        .await
}

/// Run a micro-distill pass using a pre-selected local window.
///
/// The window is assumed to already be ordered oldest-first.
pub async fn run_micro_distill_pass_with_local_window(
    store: &dyn crate::db::Database,
    compressor_llm: &dyn LlmProvider,
    user_id: &str,
    params: MicroDistillParams,
    commit: bool,
    local: &[LedgerEvent],
) -> Result<MicroDistillResult, LlmError> {
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

    // Stabilizer: include the previous wake pack snapshot as an anchor so the
    // compressor can edit conservatively instead of re-synthesizing from recent-only anchors.
    //
    // This is not "evidence" and should not be cited for actions.
    let prev_wake_pack = store
        .list_recent_ledger_events_by_kind_prefix(user_id, "wake_pack.", 1)
        .await
        .ok()
        .and_then(|mut v| v.pop())
        .and_then(|e| {
            e.content
                .map(|c| (e.id.to_string(), e.created_at.to_rfc3339(), c))
        });

    let user_msg = format!(
        "# Evidence Window (Local)\n{}\n\n# Anchor Invariants (Recent)\n{}\n\n# Drift/Contradiction Candidates (Recent)\n{}\n\n# Previous Wake Pack (Most Recent)\n{}\n",
        format_events_for_prompt(local),
        format_events_for_prompt(&invariants),
        format_events_for_prompt(&drift),
        match prev_wake_pack {
            Some((id, created_at, content)) => format!(
                "- {} {} wake_pack.v0 compressor\n  content: {}\n",
                id,
                created_at,
                truncate_chars(&content, PREV_WAKE_PACK_MAX_CHARS)
            ),
            None => "(none)\n".to_string(),
        }
    );

    let messages = vec![
        ChatMessage::system(COMPRESSOR_SYSTEM_PROMPT_V0.trim()),
        ChatMessage::user(user_msg),
    ];

    let delta = complete_delta_v0(compressor_llm, messages, None, params.max_tokens).await?;

    if !commit {
        return Ok(MicroDistillResult {
            delta,
            wake_pack_event_id: None,
            distill_event_id: None,
        });
    }

    // Apply delta actions into first-class derived ledger objects so future passes can compound.
    let mut invariant_event_ids: Vec<String> = Vec::new();
    let mut drift_event_ids: Vec<String> = Vec::new();
    for a in &delta.actions {
        match a.action_type {
            ActionTypeV0::CreateInvariant | ActionTypeV0::UpdateInvariant => {
                let Some(scope) = a.scope.as_ref() else {
                    continue;
                };
                let kind_scope = match scope {
                    ScopeV0::Self_ => "self",
                    ScopeV0::User => "user",
                    ScopeV0::Relationship => "relationship",
                };

                let content = a
                    .update
                    .as_ref()
                    .and_then(|u| u.new_text.as_deref())
                    .or_else(|| a.text.as_deref());
                let Some(content) = content else { continue };

                let payload = serde_json::json!({
                    "action_type": a.action_type,
                    "scope": kind_scope,
                    "confidence": a.confidence,
                    "invariant_id": a.invariant_id,
                    "duplicate_of": a.duplicate_of,
                    "citations": a.citations,
                });

                let ev = NewLedgerEvent {
                    user_id,
                    episode_id: None,
                    kind: &format!("invariant.{}.v0", kind_scope),
                    source: "compressor",
                    content: Some(content),
                    payload: &payload,
                };

                match store.append_ledger_event(&ev).await {
                    Ok(id) => invariant_event_ids.push(id.to_string()),
                    Err(e) => {
                        tracing::warn!("Failed to commit invariant event: {}", e);
                    }
                }
            }
            ActionTypeV0::FlagDrift
            | ActionTypeV0::MarkContradicted
            | ActionTypeV0::MergeInvariants => {
                let kind = match a.action_type {
                    ActionTypeV0::FlagDrift => "drift.flag.v0",
                    ActionTypeV0::MarkContradicted => "drift.contradiction.v0",
                    ActionTypeV0::MergeInvariants => "drift.merge.v0",
                    _ => "drift.flag.v0",
                };
                let payload = serde_json::json!({
                    "action_type": a.action_type,
                    "scope": a.scope,
                    "confidence": a.confidence,
                    "invariant_id": a.invariant_id,
                    "duplicate_of": a.duplicate_of,
                    "update": a.update,
                    "merge": a.merge,
                    "citations": a.citations,
                    "text": a.text,
                });
                let ev = NewLedgerEvent {
                    user_id,
                    episode_id: None,
                    kind,
                    source: "compressor",
                    content: a.text.as_deref(),
                    payload: &payload,
                };
                match store.append_ledger_event(&ev).await {
                    Ok(id) => drift_event_ids.push(id.to_string()),
                    Err(e) => {
                        tracing::warn!("Failed to commit drift event: {}", e);
                    }
                }
            }
        }
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

    let wake_pack_event_id =
        store
            .append_ledger_event(&wake_event)
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: "compressor".to_string(),
                reason: format!("Failed to commit wake_pack.v0: {e}"),
            })?;

    let payload = serde_json::json!({
        "actions": delta.actions,
        "derived": {
            "invariant_event_ids": invariant_event_ids,
            "drift_event_ids": drift_event_ids,
        },
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

    let distill_event_id =
        store
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
        description:
            "Emit a bounded, cited delta for invariants/isnads. Conservative; no uncited claims."
                .to_string(),
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

    #[test]
    fn scope_v0_deserializes_self() {
        // Tool schema uses "self" for scope; ensure serde matches.
        let scope: ScopeV0 = serde_json::from_str("\"self\"").expect("scope should decode");
        assert!(matches!(scope, ScopeV0::Self_));
    }

    struct FakeToolCallLlm;

    #[async_trait]
    impl LlmProvider for FakeToolCallLlm {
        fn model_name(&self) -> &str {
            "fake"
        }
        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }

        async fn complete(
            &self,
            _request: crate::llm::CompletionRequest,
        ) -> Result<crate::llm::CompletionResponse, LlmError> {
            Err(LlmError::RequestFailed {
                provider: "fake".to_string(),
                reason: "not implemented".to_string(),
            })
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
            Ok(crate::llm::ModelMetadata {
                id: "fake".to_string(),
                context_length: None,
            })
        }

        fn active_model_name(&self) -> String {
            "fake".to_string()
        }
    }

    #[tokio::test]
    async fn parses_tool_arguments_into_delta() {
        let llm = FakeToolCallLlm;
        let delta = complete_delta_v0(&llm, vec![ChatMessage::user("hi")], None, 512)
            .await
            .expect("delta");
        assert_eq!(delta.actions.len(), 1);
    }
}
