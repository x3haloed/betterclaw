use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use globset::{GlobBuilder, GlobSetBuilder};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use walkdir::WalkDir;

use crate::db::Db;
use crate::error::RuntimeError;
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
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let mut registry = Self::default();
        registry.register(EchoTool);
        registry.register(ShellTool);
        registry.register(ReadFileTool);
        registry.register(WriteFileTool);
        registry.register(CreateFileTool);
        registry.register(EditFileTool);
        registry.register(ListDirTool);
        registry.register(GrepTool);
        registry.register(FindTool);
        registry.register(AskUserTool);
        registry.register(WebSearchTool);
        registry.register(WebFetchTool);
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

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            description: "Return the provided message. Useful for testing the tool loop."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "echo", "message").map(|_| ())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        Ok(json!({ "message": require_string(&params, "echo", "message")? }))
    }
}

struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command from the workspace root. Use this as an escape hatch when a dedicated tool does not fit.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer", "minimum": 1 }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "shell", "command")?;
        optional_usize(params, "shell", "timeout_secs")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let command = require_string(&params, "shell", "command")?;
        let timeout_secs = optional_usize(&params, "shell", "timeout_secs")?;
        let mut child = Command::new("sh");
        child
            .arg("-lc")
            .arg(&command)
            .current_dir(&context.workspace.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = child.spawn().map_err(|error| RuntimeError::ToolExecution {
            tool: "shell".to_string(),
            reason: error.to_string(),
        })?;
        let output = if let Some(timeout_secs) = timeout_secs {
            match tokio::time::timeout(
                Duration::from_secs(timeout_secs as u64),
                child.wait_with_output(),
            )
            .await
            {
                Ok(result) => (
                    result.map_err(|error| RuntimeError::ToolExecution {
                        tool: "shell".to_string(),
                        reason: error.to_string(),
                    })?,
                    false,
                ),
                Err(_) => {
                    return Ok(json!({
                        "command": command,
                        "status": null,
                        "stdout": "",
                        "stderr": "",
                        "timed_out": true
                    }));
                }
            }
        } else {
            (
                child
                    .wait_with_output()
                    .await
                    .map_err(|error| RuntimeError::ToolExecution {
                        tool: "shell".to_string(),
                        reason: error.to_string(),
                    })?,
                false,
            )
        };

        Ok(json!({
            "command": command,
            "status": output.0.status.code(),
            "stdout": String::from_utf8_lossy(&output.0.stdout),
            "stderr": String::from_utf8_lossy(&output.0.stderr),
            "timed_out": output.1,
        }))
    }
}

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file relative to the workspace root with optional paging. Use offset to continue through large files.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "read_file", "path")?;
        optional_usize(params, "read_file", "offset")?;
        optional_usize(params, "read_file", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "read_file", "path")?;
        let offset = optional_usize(&params, "read_file", "offset")?.unwrap_or(1);
        let limit = optional_usize(&params, "read_file", "limit")?.unwrap_or(DEFAULT_READ_LIMIT);
        let path = resolve_path(&context.workspace, &raw_path);
        let bytes = fs::read(&path).map_err(|error| RuntimeError::ToolExecution {
            tool: "read_file".to_string(),
            reason: error.to_string(),
        })?;
        let content = String::from_utf8_lossy(&bytes).to_string();
        let lines = content.lines().map(ToString::to_string).collect::<Vec<_>>();
        let total_lines = lines.len();
        let start_index = offset.saturating_sub(1).min(total_lines);
        let end_index = (start_index + limit).min(total_lines);
        let selected = lines[start_index..end_index].join("\n");
        let (truncated_content, byte_truncated) =
            truncate_text_by_bytes(&selected, DEFAULT_MAX_BYTES);
        let line_truncated = end_index < total_lines;
        let truncated = byte_truncated || line_truncated;
        let next_offset = if truncated { Some(end_index + 1) } else { None };

        Ok(json!({
            "path": path.display().to_string(),
            "content": truncated_content,
            "offset": offset,
            "lines_shown": end_index.saturating_sub(start_index),
            "total_lines": total_lines,
            "truncated": truncated,
            "next_offset": next_offset,
            "truncation": {
                "line_limit_hit": line_truncated,
                "byte_limit_hit": byte_truncated
            }
        }))
    }
}

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Replace the full contents of a file relative to the workspace root. Creates parent directories when needed.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "write_file", "path")?;
        require_string(params, "write_file", "content")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "write_file", "path")?;
        let content = require_string(&params, "write_file", "content")?;
        let path = resolve_path(&context.workspace, &raw_path);
        let existed = path.exists();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| RuntimeError::ToolExecution {
                tool: "write_file".to_string(),
                reason: error.to_string(),
            })?;
        }
        fs::write(&path, &content).map_err(|error| RuntimeError::ToolExecution {
            tool: "write_file".to_string(),
            reason: error.to_string(),
        })?;
        Ok(json!({
            "path": path.display().to_string(),
            "bytes_written": content.len(),
            "created": !existed,
            "overwrote": existed
        }))
    }
}

struct CreateFileTool;

#[async_trait]
impl Tool for CreateFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_file".to_string(),
            description: "Create a new file relative to the workspace root. Fails if the file exists unless overwrite=true.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "overwrite": { "type": "boolean" }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "create_file", "path")?;
        require_string(params, "create_file", "content")?;
        optional_bool(params, "create_file", "overwrite")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "create_file", "path")?;
        let content = require_string(&params, "create_file", "content")?;
        let overwrite = optional_bool(&params, "create_file", "overwrite")?.unwrap_or(false);
        let path = resolve_path(&context.workspace, &raw_path);
        if path.exists() && !overwrite {
            return Err(RuntimeError::ToolExecution {
                tool: "create_file".to_string(),
                reason: format!("file already exists: {}", path.display()),
            });
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| RuntimeError::ToolExecution {
                tool: "create_file".to_string(),
                reason: error.to_string(),
            })?;
        }
        fs::write(&path, &content).map_err(|error| RuntimeError::ToolExecution {
            tool: "create_file".to_string(),
            reason: error.to_string(),
        })?;
        Ok(json!({
            "path": path.display().to_string(),
            "bytes_written": content.len(),
            "created": true
        }))
    }
}

struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit_file".to_string(),
            description:
                "Edit a file by replacing exact text. Use this for precise, surgical changes."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_text": { "type": "string" },
                    "new_text": { "type": "string" }
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "edit_file", "path")?;
        require_string(params, "edit_file", "old_text")?;
        require_string(params, "edit_file", "new_text")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "edit_file", "path")?;
        let old_text = require_string(&params, "edit_file", "old_text")?;
        let new_text = require_string(&params, "edit_file", "new_text")?;
        let path = resolve_path(&context.workspace, &raw_path);
        let original = fs::read_to_string(&path).map_err(|error| RuntimeError::ToolExecution {
            tool: "edit_file".to_string(),
            reason: error.to_string(),
        })?;
        let occurrences_found = original.matches(&old_text).count();
        if occurrences_found == 0 {
            return Err(RuntimeError::ToolExecution {
                tool: "edit_file".to_string(),
                reason: format!("could not find the requested text in {}", path.display()),
            });
        }
        if occurrences_found > 1 {
            return Err(RuntimeError::ToolExecution {
                tool: "edit_file".to_string(),
                reason: format!(
                    "found {occurrences_found} occurrences in {}. Provide a more specific old_text.",
                    path.display()
                ),
            });
        }
        let updated = original.replacen(&old_text, &new_text, 1);
        let (diff, first_changed_line) = diff_lines(&original, &updated);
        fs::write(&path, &updated).map_err(|error| RuntimeError::ToolExecution {
            tool: "edit_file".to_string(),
            reason: error.to_string(),
        })?;
        Ok(json!({
            "path": path.display().to_string(),
            "replaced": true,
            "occurrences_found": occurrences_found,
            "diff": diff,
            "first_changed_line": first_changed_line
        }))
    }
}

struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".to_string(),
            description:
                "List directory contents relative to the workspace root with structured entries."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_string(params, "list_dir", "path")?;
        optional_usize(params, "list_dir", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let raw_path =
            optional_string(&params, "list_dir", "path")?.unwrap_or_else(|| ".".to_string());
        let limit = optional_usize(&params, "list_dir", "limit")?.unwrap_or(DEFAULT_LIST_LIMIT);
        let path = resolve_path(&context.workspace, &raw_path);
        let read_dir = fs::read_dir(&path).map_err(|error| RuntimeError::ToolExecution {
            tool: "list_dir".to_string(),
            reason: error.to_string(),
        })?;

        let mut entries = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|error| RuntimeError::ToolExecution {
                tool: "list_dir".to_string(),
                reason: error.to_string(),
            })?;
            let metadata = entry
                .metadata()
                .map_err(|error| RuntimeError::ToolExecution {
                    tool: "list_dir".to_string(),
                    reason: error.to_string(),
                })?;
            entries.push((
                entry.file_name().to_string_lossy().to_string(),
                json!({
                    "name": entry.file_name().to_string_lossy().to_string(),
                    "path": relativize_path(&context.workspace.root, &entry.path()),
                    "is_dir": metadata.is_dir(),
                }),
            ));
        }
        entries.sort_by(|left, right| left.0.to_lowercase().cmp(&right.0.to_lowercase()));
        let entry_count = entries.len();
        let limit_reached = summarize_limit_reached(entry_count, limit);
        let entries = entries
            .into_iter()
            .take(limit)
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();

        Ok(json!({
            "path": path.display().to_string(),
            "entries": entries,
            "entry_count": entry_count,
            "truncated": limit_reached,
            "limit_reached": limit_reached.then_some(limit)
        }))
    }
}

struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "grep".to_string(),
            description:
                "Search file contents for a pattern with line-numbered structured matches."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "ignore_case": { "type": "boolean" },
                    "literal": { "type": "boolean" },
                    "context": { "type": "integer", "minimum": 0 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "grep", "pattern")?;
        optional_string(params, "grep", "path")?;
        optional_bool(params, "grep", "ignore_case")?;
        optional_bool(params, "grep", "literal")?;
        optional_usize(params, "grep", "context")?;
        optional_usize(params, "grep", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let pattern = require_string(&params, "grep", "pattern")?;
        let raw_path = optional_string(&params, "grep", "path")?.unwrap_or_else(|| ".".to_string());
        let ignore_case = optional_bool(&params, "grep", "ignore_case")?.unwrap_or(false);
        let literal = optional_bool(&params, "grep", "literal")?.unwrap_or(false);
        let context_lines = optional_usize(&params, "grep", "context")?.unwrap_or(0);
        let limit = optional_usize(&params, "grep", "limit")?.unwrap_or(DEFAULT_GREP_LIMIT);
        let start = resolve_path(&context.workspace, &raw_path);

        let regex = if literal {
            None
        } else {
            Some(
                RegexBuilder::new(&pattern)
                    .case_insensitive(ignore_case)
                    .build()
                    .map_err(|error| invalid_tool_parameters("grep", error.to_string()))?,
            )
        };
        let needle = if ignore_case {
            pattern.to_lowercase()
        } else {
            pattern.clone()
        };

        let mut matches = Vec::new();
        for file in collect_files(&start) {
            if matches.len() >= limit {
                break;
            }
            let bytes = match fs::read(&file) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if is_probably_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            let lines = text.lines().collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                let matched = if let Some(regex) = &regex {
                    regex.is_match(line)
                } else if ignore_case {
                    line.to_lowercase().contains(&needle)
                } else {
                    line.contains(&needle)
                };
                if !matched {
                    continue;
                }
                let before = if context_lines > 0 {
                    lines[index.saturating_sub(context_lines)..index]
                        .iter()
                        .map(|line| (*line).to_string())
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let after = if context_lines > 0 {
                    let end = (index + 1 + context_lines).min(lines.len());
                    lines[index + 1..end]
                        .iter()
                        .map(|line| (*line).to_string())
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                matches.push(json!({
                    "path": relativize_path(&context.workspace.root, &file),
                    "line": index + 1,
                    "text": line,
                    "context_before": before,
                    "context_after": after
                }));
                if matches.len() >= limit {
                    break;
                }
            }
        }

        let limit_reached = summarize_limit_reached(matches.len(), limit);
        Ok(json!({
            "matches": matches,
            "match_count": matches.len(),
            "truncated": limit_reached,
            "limit_reached": limit_reached.then_some(limit)
        }))
    }
}

struct FindTool;

#[async_trait]
impl Tool for FindTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "find".to_string(),
            description: "Find files by glob pattern relative to the workspace root.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string" },
                    "path": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "find", "pattern")?;
        optional_string(params, "find", "path")?;
        optional_usize(params, "find", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let pattern = require_string(&params, "find", "pattern")?;
        let raw_path = optional_string(&params, "find", "path")?.unwrap_or_else(|| ".".to_string());
        let limit = optional_usize(&params, "find", "limit")?.unwrap_or(DEFAULT_FIND_LIMIT);
        let start = resolve_path(&context.workspace, &raw_path);

        let mut builder = GlobSetBuilder::new();
        builder.add(
            GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
                .map_err(|error| invalid_tool_parameters("find", error.to_string()))?,
        );
        let matcher = builder
            .build()
            .map_err(|error| invalid_tool_parameters("find", error.to_string()))?;

        let mut paths = Vec::new();
        for file in collect_files(&start) {
            let relative = relativize_path(&start, &file);
            if matcher.is_match(&relative) {
                paths.push(relative);
                if paths.len() >= limit {
                    break;
                }
            }
        }
        let limit_reached = summarize_limit_reached(paths.len(), limit);
        Ok(json!({
            "paths": paths,
            "match_count": paths.len(),
            "truncated": limit_reached,
            "limit_reached": limit_reached.then_some(limit)
        }))
    }
}

struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".to_string(),
            description: "Ask the operator a clarifying question and pause the current turn until they reply.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" },
                    "context": { "type": "string" }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "ask_user", "question")?;
        optional_string(params, "ask_user", "context")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let question = require_string(&params, "ask_user", "question")?;
        let context = optional_string(&params, "ask_user", "context")?;
        Ok(json!({
            "question": question,
            "context": context,
            "status": "awaiting_user",
            "control": control_payload("ask_user", json!({
                "question": question,
                "context": context
            }))
        }))
    }
}

struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web for lightweight results with titles, urls, and snippets."
                .to_string(),
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
        require_string(params, "web_search", "query")?;
        optional_usize(params, "web_search", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let query = require_string(&params, "web_search", "query")?;
        let limit =
            optional_usize(&params, "web_search", "limit")?.unwrap_or(DEFAULT_WEB_SEARCH_LIMIT);
        let client = reqwest::Client::new();
        let response = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query.as_str())])
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: error.to_string(),
            })?;
        let body = response
            .text()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: error.to_string(),
            })?;

        let link_regex = regex::Regex::new(
            r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="(?P<url>[^"]+)"[^>]*>(?P<title>.*?)</a>"#,
        )
        .unwrap();
        let snippet_regex = regex::Regex::new(
            r#"<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(?P<snippet>.*?)</a>|<div[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(?P<snippet_div>.*?)</div>"#,
        )
        .unwrap();

        let snippets = snippet_regex
            .captures_iter(&body)
            .filter_map(|captures| {
                captures
                    .name("snippet")
                    .or_else(|| captures.name("snippet_div"))
            })
            .map(|capture| {
                normalize_whitespace(&decode_html_entities(&strip_tags(capture.as_str())))
            })
            .collect::<Vec<_>>();

        let mut results = Vec::new();
        for (index, captures) in link_regex.captures_iter(&body).enumerate() {
            if results.len() >= limit {
                break;
            }
            let Some(url) = captures.name("url") else {
                continue;
            };
            let Some(title) = captures.name("title") else {
                continue;
            };
            results.push(json!({
                "title": normalize_whitespace(&decode_html_entities(&strip_tags(title.as_str()))),
                "url": decode_html_entities(url.as_str()),
                "snippet": snippets.get(index).cloned().unwrap_or_default(),
            }));
        }

        Ok(json!({
            "query": query,
            "results": results,
            "result_count": results.len()
        }))
    }
}

struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL and return normalized text content plus metadata."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "web_fetch", "url")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let url = require_string(&params, "web_fetch", "url")?;
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_fetch".to_string(),
                reason: error.to_string(),
            })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_fetch".to_string(),
                reason: error.to_string(),
            })?;
        let title = regex::Regex::new(r"(?is)<title>(?P<title>.*?)</title>")
            .unwrap()
            .captures(&body)
            .and_then(|captures| captures.name("title"))
            .map(|title| normalize_whitespace(&decode_html_entities(&strip_tags(title.as_str()))));
        let normalized = normalize_whitespace(&decode_html_entities(&strip_tags(&body)));
        let (content, truncated) = truncate_text_by_bytes(&normalized, DEFAULT_MAX_BYTES);

        Ok(json!({
            "url": url,
            "status": status,
            "content_type": content_type,
            "title": title,
            "content": content,
            "truncated": truncated
        }))
    }
}

#[allow(dead_code)]
struct MessageTool;

#[async_trait]
impl Tool for MessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "message".to_string(),
            description: "Send a message to the active channel/thread routed through the current conversation metadata.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" }
                },
                "required": ["content"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "message", "content")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        // This tool is intentionally left disabled in the default registry for now.
        // Before it is useful as a first-class primitive, BetterClaw needs:
        // 1. a stable runtime target model (thread/channel/identity targets),
        // 2. a routing resolver from model-visible targets to concrete channel endpoints,
        // 3. outbound policy/permissions for cross-thread and cross-channel delivery,
        // 4. channel adapters that can send to arbitrary resolved targets, not just the active thread.
        let content = require_string(&params, "message", "content")?;
        Ok(json!({
            "content": content,
            "status": "queued",
            "control": control_payload("message", json!({ "content": content }))
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::tempdir;

    use super::{
        CreateFileTool, EchoTool, EditFileTool, FindTool, GrepTool, ListDirTool, ReadFileTool,
        ShellTool, Tool, ToolContext, resolve_path,
    };
    use crate::db::Db;
    use crate::workspace::Workspace;

    async fn test_context(root: &Path) -> ToolContext {
        let db = Arc::new(Db::open(&root.join("test.db")).await.unwrap());
        ToolContext::new(
            Workspace::new("default", root),
            "thread-1",
            "thread-1",
            "web",
            db,
        )
    }

    #[test]
    fn relative_paths_resolve_from_workspace_root() {
        let workspace = Workspace::new("default", "/tmp/workspace");
        let resolved = resolve_path(&workspace, "documents/test.txt");
        assert_eq!(resolved, PathBuf::from("/tmp/workspace/documents/test.txt"));
    }

    #[test]
    fn absolute_paths_are_preserved() {
        let workspace = Workspace::new("default", "/tmp/workspace");
        let resolved = resolve_path(&workspace, "/var/tmp/test.txt");
        assert_eq!(resolved, PathBuf::from("/var/tmp/test.txt"));
    }

    #[tokio::test]
    async fn echo_validation_rejects_missing_message() {
        let tool = EchoTool;
        let result = tool.validate(&json!({}));
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_file_supports_paging_and_next_offset() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "a\nb\nc\nd\n").unwrap();
        let tool = ReadFileTool;
        let context = test_context(root).await;

        let result = tool
            .call(json!({"path":"sample.txt","offset":2,"limit":2}), &context)
            .await
            .unwrap();

        assert_eq!(result["content"], "b\nc");
        assert_eq!(result["offset"], 2);
        assert_eq!(result["lines_shown"], 2);
        assert_eq!(result["total_lines"], 4);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["next_offset"], 4);
    }

    #[tokio::test]
    async fn read_file_offset_past_eof_returns_empty_content() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "a\nb\n").unwrap();
        let tool = ReadFileTool;
        let context = test_context(root).await;

        let result = tool
            .call(
                json!({"path":"sample.txt","offset":99,"limit":10}),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(result["content"], "");
        assert_eq!(result["lines_shown"], 0);
        assert_eq!(result["truncated"], false);
    }

    #[tokio::test]
    async fn edit_file_replaces_exact_text_and_returns_diff() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "hello\nworld\n").unwrap();
        let tool = EditFileTool;
        let context = test_context(root).await;

        let result = tool
            .call(
                json!({"path":"sample.txt","old_text":"world","new_text":"there"}),
                &context,
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("sample.txt")).unwrap(),
            "hello\nthere\n"
        );
        assert_eq!(result["replaced"], true);
        assert_eq!(result["occurrences_found"], 1);
        assert!(result["diff"].as_str().unwrap().contains("-world"));
        assert!(result["diff"].as_str().unwrap().contains("+there"));
    }

    #[tokio::test]
    async fn edit_file_fails_when_text_is_ambiguous() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "x\ny\nx\n").unwrap();
        let tool = EditFileTool;
        let context = test_context(root).await;

        let error = tool
            .call(
                json!({"path":"sample.txt","old_text":"x","new_text":"z"}),
                &context,
            )
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Provide a more specific old_text")
        );
    }

    #[tokio::test]
    async fn create_file_fails_when_existing_without_overwrite() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "old").unwrap();
        let tool = CreateFileTool;
        let context = test_context(root).await;

        let error = tool
            .call(json!({"path":"sample.txt","content":"new"}), &context)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("file already exists"));
    }

    #[tokio::test]
    async fn find_returns_relative_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn main() {}\n").unwrap();
        let tool = FindTool;
        let context = test_context(root).await;

        let result = tool
            .call(json!({"pattern":"**/*.rs","path":"src"}), &context)
            .await
            .unwrap();

        assert_eq!(result["paths"][0], "lib.rs");
    }

    #[tokio::test]
    async fn grep_returns_structured_matches() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("sample.txt"), "alpha\nbeta alpha\ngamma\n").unwrap();
        let tool = GrepTool;
        let context = test_context(root).await;

        let result = tool
            .call(json!({"pattern":"alpha","literal":true}), &context)
            .await
            .unwrap();

        assert_eq!(result["match_count"], 2);
        assert_eq!(result["matches"][0]["path"], "sample.txt");
        assert_eq!(result["matches"][0]["line"], 1);
    }

    #[tokio::test]
    async fn list_dir_returns_structured_entries() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("file.txt"), "hi").unwrap();
        let tool = ListDirTool;
        let context = test_context(root).await;

        let result = tool.call(json!({}), &context).await.unwrap();
        let entries = result["entries"].as_array().unwrap();
        assert!(result["entry_count"].as_u64().unwrap() >= 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry["name"] == "nested" && entry["is_dir"] == true)
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry["name"] == "file.txt" && entry["is_dir"] == false)
        );
    }

    #[tokio::test]
    async fn shell_timeout_reports_timed_out() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let tool = ShellTool;
        let context = test_context(root).await;

        let result = tool
            .call(json!({"command":"sleep 2","timeout_secs":1}), &context)
            .await
            .unwrap();

        assert_eq!(result["timed_out"], true);
    }
}
