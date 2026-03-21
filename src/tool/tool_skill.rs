use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::RuntimeError;
use crate::skill::read_skill_by_name;
use crate::tool::{Tool, ToolContext, ToolDefinition, require_string};

pub struct ReadSkillTool;

#[async_trait]
impl Tool for ReadSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_skill".to_string(),
            description:
                "Read the full instructions for a workspace skill by name. Returns the SKILL.md content."
                    .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The skill name (directory name under skills/)"
                    }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "read_skill", "name")?;
        Ok(())
    }

    async fn call(&self, params: Value, context: &ToolContext) -> Result<Value, RuntimeError> {
        let name = require_string(&params, "read_skill", "name")?;
        let workspace_root = &context.workspace.root;

        match read_skill_by_name(workspace_root, &name).await {
            Some(skill) => Ok(json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.path.to_string_lossy(),
                "instructions": skill.instructions,
                "has_scripts": skill.scripts_dir.is_some(),
            })),
            None => Ok(json!({
                "error": format!("Skill '{}' not found in workspace skills/ directory", name)
            })),
        }
    }
}
