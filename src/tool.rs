use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::error::RuntimeError;
use crate::workspace::Workspace;

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

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn validate(&self, params: &Value) -> Result<(), RuntimeError>;
    async fn call(&self, params: Value, workspace: &Workspace) -> Result<Value, RuntimeError>;
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
        registry.register(ListDirTool);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, Arc::new(tool));
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|tool| tool.definition()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        params: Value,
        workspace: &Workspace,
    ) -> Result<Value, RuntimeError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| RuntimeError::ToolNotFound(name.to_string()))?;
        tool.validate(&params)?;
        tool.call(params, workspace).await
    }
}

fn require_string(params: &Value, field: &str) -> Result<String, RuntimeError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| RuntimeError::InvalidToolParameters {
            tool: "unknown".to_string(),
            reason: format!("missing or invalid string field '{field}'"),
        })
}

fn resolve_path(workspace: &Workspace, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.root.join(path)
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "echo".to_string(),
            description: "Return the provided message.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "message").map(|_| ())
    }

    async fn call(&self, params: Value, _workspace: &Workspace) -> Result<Value, RuntimeError> {
        Ok(json!({ "message": require_string(&params, "message")? }))
    }
}

struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".to_string(),
            description: "Run a shell command from the workspace root.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "command").map(|_| ())
    }

    async fn call(&self, params: Value, workspace: &Workspace) -> Result<Value, RuntimeError> {
        let command = require_string(&params, "command")?;
        let output = Command::new("sh")
            .arg("-lc")
            .arg(&command)
            .current_dir(&workspace.root)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "shell".to_string(),
                reason: error.to_string(),
            })?;
        Ok(json!({
            "command": command,
            "status": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }))
    }
}

struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read a file relative to the workspace root.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "path").map(|_| ())
    }

    async fn call(&self, params: Value, workspace: &Workspace) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "path")?;
        let path = resolve_path(workspace, &raw_path);
        let content = fs::read_to_string(&path).map_err(|error| RuntimeError::ToolExecution {
            tool: "read_file".to_string(),
            reason: error.to_string(),
        })?;
        Ok(json!({
            "path": path.display().to_string(),
            "content": content
        }))
    }
}

struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_file".to_string(),
            description: "Write a file relative to the workspace root.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "path")?;
        require_string(params, "content")?;
        Ok(())
    }

    async fn call(&self, params: Value, workspace: &Workspace) -> Result<Value, RuntimeError> {
        let raw_path = require_string(&params, "path")?;
        let content = require_string(&params, "content")?;
        let path = resolve_path(workspace, &raw_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| RuntimeError::ToolExecution {
                tool: "write_file".to_string(),
                reason: error.to_string(),
            })?;
        }
        fs::write(&path, content).map_err(|error| RuntimeError::ToolExecution {
            tool: "write_file".to_string(),
            reason: error.to_string(),
        })?;
        Ok(json!({ "path": path.display().to_string(), "written": true }))
    }
}

struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_dir".to_string(),
            description: "List a directory relative to the workspace root.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        if let Some(path) = params.get("path") {
            path.as_str()
                .ok_or_else(|| RuntimeError::InvalidToolParameters {
                    tool: "list_dir".to_string(),
                    reason: "field 'path' must be a string".to_string(),
                })?;
        }
        Ok(())
    }

    async fn call(&self, params: Value, workspace: &Workspace) -> Result<Value, RuntimeError> {
        let raw_path = params.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = resolve_path(workspace, raw_path);
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path).map_err(|error| RuntimeError::ToolExecution {
            tool: "list_dir".to_string(),
            reason: error.to_string(),
        })? {
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
            entries.push(json!({
                "name": entry.file_name().to_string_lossy().to_string(),
                "is_dir": metadata.is_dir(),
                "is_file": metadata.is_file(),
            }));
        }
        Ok(json!({
            "path": path.display().to_string(),
            "entries": entries
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{EchoTool, Tool, resolve_path};
    use crate::workspace::Workspace;

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
}
