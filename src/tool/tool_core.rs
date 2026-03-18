use super::*;

use crate::error::RuntimeError;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;

pub struct EchoTool;

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

pub struct ShellTool;

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

pub struct AskUserTool;

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

pub struct MessageTool;

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

pub struct NoOpTool;

#[async_trait]
impl Tool for NoOpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "no_op".to_string(),
            description: "Record that no external action or user-facing reply is needed for this step. Use this when a tool call is required but the correct action is to do nothing and continue or conclude.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string" }
                },
                "required": ["reason"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "no_op", "reason")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let reason = require_string(&params, "no_op", "reason")?;
        Ok(json!({
            "status": "no_op",
            "reason": reason,
        }))
    }
}

pub struct FinalMessageTool;

#[async_trait]
impl Tool for FinalMessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "final_message".to_string(),
            description: "Deliver the final user-facing response for the current turn. Use this instead of plain assistant text when no more tool work is needed.".to_string(),
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
        require_string(params, "final_message", "content")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let content = require_string(&params, "final_message", "content")?;
        Ok(json!({
            "content": content,
            "status": "final",
            "control": control_payload("final_message", json!({ "content": content }))
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

    use super::super::{Tool, ToolContext, resolve_path};
    use crate::db::Db;
    use crate::tool::tool_core::{EchoTool, NoOpTool, ShellTool};
    use crate::tool::tool_fs::{
        CreateFileTool, EditFileTool, FindTool, GrepTool, ListDirTool, ReadFileTool,
    };
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

    #[tokio::test]
    async fn noop_tool_returns_reason() {
        let dir = tempdir().unwrap();
        let context = test_context(dir.path()).await;
        let tool = NoOpTool;
        let output = tool
            .call(json!({"reason":"No reply needed for this coordination ping."}), &context)
            .await
            .unwrap();
        assert_eq!(output["status"], json!("no_op"));
        assert_eq!(
            output["reason"],
            json!("No reply needed for this coordination ping.")
        );
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
