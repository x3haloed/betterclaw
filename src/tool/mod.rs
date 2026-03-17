use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::db::Db;
use crate::error::RuntimeError;
use crate::event::{Event, EventKind};
use crate::model::normalize_schema_strict;
use crate::turn::Turn;
use crate::workspace::Workspace;

const DEFAULT_READ_LIMIT: usize = 200;
const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_GREP_LIMIT: usize = 100;
const DEFAULT_FIND_LIMIT: usize = 1000;
const DEFAULT_WEB_SEARCH_LIMIT: usize = 5;
const DEFAULT_MAX_BYTES: usize = 30 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub id: String,
    pub turn_id: String,
    pub thread_id: String,
    pub tool_name: String,
    pub parameters: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub invocation_id: String,
    pub tool_name: String,
    pub output: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct ToolContext {
    pub workspace: Workspace,
    pub thread_id: String,
    pub external_thread_id: String,
    pub channel: String,
    pub db: Arc<Db>,
}

impl ToolContext {
    pub fn new(
        workspace: Workspace,
        thread_id: impl Into<String>,
        external_thread_id: impl Into<String>,
        channel: impl Into<String>,
        db: Arc<Db>,
    ) -> Self {
        Self {
            workspace,
            thread_id: thread_id.into(),
            external_thread_id: external_thread_id.into(),
            channel: channel.into(),
            db,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn validate(&self, params: &Value) -> Result<(), RuntimeError>;
    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError>;
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register(tool_core::EchoTool);
        registry.register(tool_core::ShellTool);
        registry.register(tool_fs::ReadFileTool);
        registry.register(tool_fs::WriteFileTool);
        registry.register(tool_fs::CreateFileTool);
        registry.register(tool_fs::EditFileTool);
        registry.register(tool_fs::ListDirTool);
        registry.register(tool_fs::GrepTool);
        registry.register(tool_fs::FindTool);
        registry.register(tool_tidepool::TidepoolMyAccountTool);
        registry.register(tool_tidepool::TidepoolListSubscriptionsTool);
        registry.register(tool_tidepool::TidepoolSubscribeDomainTool);
        registry.register(tool_tidepool::TidepoolUnsubscribeDomainTool);
        registry.register(tool_tidepool::TidepoolPostMessageTool);
        registry.register(tool_tidepool::TidepoolCreateDomainTool);
        registry.register(tool_tidepool::TidepoolAddDomainMemberTool);
        registry.register(tool_tidepool::TidepoolRemoveDomainMemberTool);
        registry.register(tool_tidepool::TidepoolCreateDmTool);
        registry.register(tool_tidepool::TidepoolReadMessagesTool);
        registry.register(tool_core::FinalMessageTool);
        registry.register(tool_core::AskUserTool);
        registry.register(tool_web::WebSearchTool);
        registry.register(tool_web::WebFetchTool);
        registry.register(tool_ledger::LedgerListTool);
        registry.register(tool_ledger::LedgerGetTool);
        registry.register(tool_ledger::ConversationSearchTool);
        registry.register(tool_skill::ReadSkillTool);
        registry.register(tool_core::MessageTool);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    pub async fn execute(
        &self,
        name: &str,
        params: Value,
        context: &ToolContext,
    ) -> Result<Value, RuntimeError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| RuntimeError::ToolNotFound(name.to_string()))?;
        tool.validate(&params)?;
        tool.call(params, context).await
    }
}

fn invalid_tool_parameters(tool: &str, reason: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidToolParameters {
        tool: tool.to_string(),
        reason: reason.into(),
    }
}

fn require_string(params: &Value, tool: &str, field: &str) -> Result<String, RuntimeError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            invalid_tool_parameters(tool, format!("missing or invalid string field '{field}'"))
        })
}

fn optional_string(
    params: &Value,
    tool: &str,
    field: &str,
) -> Result<Option<String>, RuntimeError> {
    match params.get(field) {
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| {
                invalid_tool_parameters(tool, format!("field '{field}' must be a string"))
            }),
        None => Ok(None),
    }
}

fn optional_bool(params: &Value, tool: &str, field: &str) -> Result<Option<bool>, RuntimeError> {
    match params.get(field) {
        Some(value) => value.as_bool().map(Some).ok_or_else(|| {
            invalid_tool_parameters(tool, format!("field '{field}' must be a boolean"))
        }),
        None => Ok(None),
    }
}

fn optional_usize(params: &Value, tool: &str, field: &str) -> Result<Option<usize>, RuntimeError> {
    match params.get(field) {
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return Err(invalid_tool_parameters(
                    tool,
                    format!("field '{field}' must be a non-negative integer"),
                ));
            };
            usize::try_from(raw)
                .map(Some)
                .map_err(|_| invalid_tool_parameters(tool, format!("field '{field}' is too large")))
        }
        None => Ok(None),
    }
}

fn resolve_path(workspace: &Workspace, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.root.join(path)
    }
}

fn relativize_path(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn summarize_limit_reached(returned: usize, limit: usize) -> bool {
    returned >= limit
}

fn truncate_text_by_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn diff_lines(old_text: &str, new_text: &str) -> (String, Option<usize>) {
    let old_lines = old_text.lines().collect::<Vec<_>>();
    let new_lines = new_text.lines().collect::<Vec<_>>();
    let max_len = old_lines.len().max(new_lines.len());
    let mut first_changed_line = None;
    let mut diff = Vec::new();

    for index in 0..max_len {
        let old_line = old_lines.get(index).copied();
        let new_line = new_lines.get(index).copied();
        if old_line == new_line {
            continue;
        }
        if first_changed_line.is_none() {
            first_changed_line = Some(index + 1);
        }
        if let Some(old_line) = old_line {
            diff.push(format!("-{old_line}"));
        }
        if let Some(new_line) = new_line {
            diff.push(format!("+{new_line}"));
        }
        if diff.len() >= 200 {
            diff.push("...".to_string());
            break;
        }
    }

    (diff.join("\n"), first_changed_line)
}

fn control_payload(kind: &str, payload: Value) -> Value {
    json!({
        "__betterclaw_control": {
            "kind": kind,
            "payload": payload,
        }
    })
}

fn strip_tags(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn normalize_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_files(start: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    WalkDir::new(start)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(1024).any(|byte| *byte == 0)
}

pub fn normalize_tool_parameters_schema(schema: &Value) -> Value {
    normalize_schema_strict(schema)
}

#[derive(Debug, Clone)]
struct ToolLedgerEntry {
    entry_id: String,
    thread_id: String,
    turn_id: Option<String>,
    kind: String,
    created_at: DateTime<Utc>,
    content: Option<String>,
    payload: Value,
    citation: String,
}

fn summarize_entry_content(content: &str) -> String {
    let trimmed = normalize_whitespace(content);
    let (summary, truncated) = truncate_text_by_bytes(&trimmed, 240);
    if truncated {
        format!("{summary}...")
    } else {
        summary
    }
}

fn build_fts_query(query: &str) -> String {
    query
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .filter(|token| !token.trim().is_empty())
        .take(8)
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn tool_ledger_entry_from_turn(
    thread_id: &str,
    turn: &Turn,
    assistant: bool,
) -> Option<ToolLedgerEntry> {
    let (entry_id, kind, content) = if assistant {
        (
            format!("turn:{}:assistant", turn.id),
            "agent_turn".to_string(),
            turn.assistant_message.clone(),
        )
    } else {
        (
            format!("turn:{}:user", turn.id),
            "user_turn".to_string(),
            Some(turn.user_message.clone()),
        )
    };
    let content = content.filter(|value| !value.trim().is_empty())?;
    Some(ToolLedgerEntry {
        entry_id: entry_id.clone(),
        thread_id: thread_id.to_string(),
        turn_id: Some(turn.id.clone()),
        kind,
        created_at: turn.created_at,
        citation: entry_id,
        payload: json!({
            "status": turn.status,
            "error": turn.error,
        }),
        content: Some(content),
    })
}

fn tool_ledger_entry_from_event(event: &Event) -> Option<ToolLedgerEntry> {
    let kind = match event.kind {
        EventKind::ToolCall => "tool_call",
        EventKind::ToolResult => "tool_result",
        EventKind::Error => "error",
        _ => return None,
    };
    Some(ToolLedgerEntry {
        entry_id: format!("event:{}", event.id),
        thread_id: event.thread_id.clone(),
        turn_id: Some(event.turn_id.clone()),
        kind: kind.to_string(),
        created_at: event.created_at,
        citation: format!("event:{}", event.id),
        content: event
            .payload
            .get("content")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                event
                    .payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .or_else(|| {
                if kind == "error" {
                    Some(event.payload.to_string())
                } else {
                    None
                }
            }),
        payload: event.payload.clone(),
    })
}

#[cfg(test)]
mod schema_tests {
    use serde_json::json;

    use super::normalize_tool_parameters_schema;

    fn required_names(value: &serde_json::Value) -> Vec<String> {
        let mut names = value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn normalizes_optional_fields_to_required_nullable() {
        let normalized = normalize_tool_parameters_schema(&json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "context": { "type": "string" }
            },
            "required": ["question"]
        }));

        assert_eq!(
            required_names(&normalized["required"]),
            vec!["context".to_string(), "question".to_string()]
        );
        assert_eq!(
            normalized["properties"]["context"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(normalized["additionalProperties"], json!(false));
    }

    #[test]
    fn normalizes_nested_object_fields() {
        let normalized = normalize_tool_parameters_schema(&json!({
            "type": "object",
            "properties": {
                "payload": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "label": { "type": "string" }
                    },
                    "required": ["enabled"]
                }
            },
            "required": []
        }));

        assert_eq!(
            normalized["properties"]["payload"]["type"],
            json!(["object", "null"])
        );
        assert_eq!(
            normalized["properties"]["payload"]["properties"]["label"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            required_names(&normalized["properties"]["payload"]["required"]),
            vec!["enabled".to_string(), "label".to_string()]
        );
    }
}

async fn tool_collect_ledger_entries(
    db: &Db,
    thread_id: Option<&str>,
) -> Result<Vec<ToolLedgerEntry>, RuntimeError> {
    let threads = if let Some(thread_id) = thread_id {
        match db.get_thread(thread_id).await.map_err(RuntimeError::from)? {
            Some(thread) => vec![thread],
            None => return Ok(Vec::new()),
        }
    } else {
        db.list_threads().await.map_err(RuntimeError::from)?
    };

    let mut entries = Vec::new();
    for thread in threads {
        let turns = db
            .list_thread_turns(&thread.id)
            .await
            .map_err(RuntimeError::from)?;
        for turn in &turns {
            if let Some(entry) = tool_ledger_entry_from_turn(&thread.id, turn, false) {
                entries.push(entry);
            }
            if let Some(entry) = tool_ledger_entry_from_turn(&thread.id, turn, true) {
                entries.push(entry);
            }
        }
        let events = db
            .list_thread_events(&thread.id)
            .await
            .map_err(RuntimeError::from)?;
        for event in &events {
            if let Some(entry) = tool_ledger_entry_from_event(event) {
                entries.push(entry);
            }
        }
    }
    entries.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
    Ok(entries)
}

mod tool_ledger;

mod tool_skill;

mod tool_core;

mod tool_fs;

mod tool_web;

mod tool_tidepool;

// Register function
