use super::*;

use crate::error::RuntimeError;
use async_trait::async_trait;
use globset::{GlobBuilder, GlobSetBuilder};
use regex::RegexBuilder;
use serde_json::{Value, json};
use std::fs;

pub struct ReadFileTool;

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

pub struct WriteFileTool;

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

pub struct CreateFileTool;

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

pub struct EditFileTool;

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

pub struct ListDirTool;

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

pub struct GrepTool;

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

pub struct FindTool;

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
