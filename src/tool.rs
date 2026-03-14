use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::RuntimeError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters_schema,
        }
    }
}

pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn validate(&self, _params: &Value) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn call(&self, params: Value) -> Result<Value, RuntimeError>;
}
