//! Tool for reading structured context about the current inbound message.

use async_trait::async_trait;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

/// Returns structured metadata for the current inbound message.
pub struct CurrentMessageTool;

#[async_trait]
impl Tool for CurrentMessageTool {
    fn name(&self) -> &str {
        "current_message"
    }

    fn description(&self) -> &str {
        "Return structured data about the current inbound message, including channel, thread, sender, and channel-specific metadata such as Tidepool reply targets."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success(
            ctx.metadata.clone(),
            std::time::Duration::from_millis(0),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn current_message_returns_context_metadata() {
        let tool = CurrentMessageTool;
        let mut ctx = JobContext::new("test", "test");
        ctx.metadata = serde_json::json!({
            "incoming_message": {
                "channel": "tidepool-channel",
                "metadata": {
                    "domain_id": 1,
                    "reply_to_message_id": 42
                }
            }
        });

        let output = tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .expect("tool should succeed");

        assert_eq!(
            output.result["incoming_message"]["channel"],
            "tidepool-channel"
        );
        assert_eq!(
            output.result["incoming_message"]["metadata"]["reply_to_message_id"],
            42
        );
    }
}
