use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::error::RuntimeError;
use crate::generated::tidepool::SubscriptionLookup;
use crate::tidepool::require_shared_client;

const DEFAULT_SUBSCRIBE_BATCH_WINDOW_SECONDS: u32 = 30;

pub struct TidepoolMyAccountTool;

#[async_trait]
impl Tool for TidepoolMyAccountTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_my_account".to_string(),
            description: "Return the configured Tidepool account identity for the current BetterClaw runtime.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        validate_empty_object(params, "tidepool_my_account")
    }

    async fn call(&self, _params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let client = shared_tidepool_client("tidepool_my_account").await?;
        let account = client.account().ok_or_else(|| RuntimeError::ToolExecution {
            tool: "tidepool_my_account".to_string(),
            reason: "Tidepool channel is active but no account is visible on the shared connection"
                .to_string(),
        })?;
        let bootstrap = client.bootstrap_outcome();
        Ok(json!({
            "account_id": account.account_id,
            "handle": account.handle,
            "token_path": bootstrap.token_path,
            "subscribed_domain_ids": bootstrap.subscribed_domain_ids,
        }))
    }
}

pub struct TidepoolListSubscriptionsTool;

#[async_trait]
impl Tool for TidepoolListSubscriptionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_list_subscriptions".to_string(),
            description: "List Tidepool domains currently subscribed by the configured BetterClaw account.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        validate_empty_object(params, "tidepool_list_subscriptions")
    }

    async fn call(&self, _params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let client = shared_tidepool_client("tidepool_list_subscriptions").await?;
        let subscriptions = client.subscriptions();
        Ok(json!({
            "subscriptions": serialize_subscriptions(&subscriptions),
            "count": subscriptions.len(),
        }))
    }
}

pub struct TidepoolSubscribeDomainTool;

#[async_trait]
impl Tool for TidepoolSubscribeDomainTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_subscribe_domain".to_string(),
            description: "Subscribe the configured Tidepool account to a domain so it can receive and inspect messages there.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": { "type": "integer", "minimum": 0 },
                    "batch_window_seconds": { "type": "integer", "minimum": 1 }
                },
                "required": ["domain_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_subscribe_domain", "domain_id")?;
        optional_u32(params, "tidepool_subscribe_domain", "batch_window_seconds")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_subscribe_domain", "domain_id")?;
        let batch_window_seconds = optional_u32(
            &params,
            "tidepool_subscribe_domain",
            "batch_window_seconds",
        )?
        .unwrap_or(DEFAULT_SUBSCRIBE_BATCH_WINDOW_SECONDS);
        let client = shared_tidepool_client("tidepool_subscribe_domain").await?;
        let subscriptions = client
            .subscribe_domain(domain_id, batch_window_seconds)
            .await
            .map_err(|error| tool_execution("tidepool_subscribe_domain", error))?;
        Ok(json!({
            "status": "subscribed",
            "domain_id": domain_id,
            "batch_window_seconds": batch_window_seconds,
            "subscriptions": serialize_subscriptions(&subscriptions),
        }))
    }
}

pub struct TidepoolUnsubscribeDomainTool;

#[async_trait]
impl Tool for TidepoolUnsubscribeDomainTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_unsubscribe_domain".to_string(),
            description: "Remove the configured Tidepool account from a subscribed domain.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": { "type": "integer", "minimum": 0 }
                },
                "required": ["domain_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_unsubscribe_domain", "domain_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_unsubscribe_domain", "domain_id")?;
        let client = shared_tidepool_client("tidepool_unsubscribe_domain").await?;
        let subscriptions = client
            .unsubscribe_domain(domain_id)
            .await
            .map_err(|error| tool_execution("tidepool_unsubscribe_domain", error))?;
        Ok(json!({
            "status": "unsubscribed",
            "domain_id": domain_id,
            "subscriptions": serialize_subscriptions(&subscriptions),
        }))
    }
}

pub struct TidepoolPostMessageTool;

#[async_trait]
impl Tool for TidepoolPostMessageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_post_message".to_string(),
            description: "Post a message into a Tidepool domain using the configured BetterClaw account.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": { "type": "integer", "minimum": 0 },
                    "body": { "type": "string" },
                    "reply_to_message_id": { "type": "integer", "minimum": 0 }
                },
                "required": ["domain_id", "body"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_post_message", "domain_id")?;
        let body = require_string(params, "tidepool_post_message", "body")?;
        if body.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_post_message".to_string(),
                reason: "field 'body' must not be empty".to_string(),
            });
        }
        optional_u64(params, "tidepool_post_message", "reply_to_message_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_post_message", "domain_id")?;
        let body = require_string(&params, "tidepool_post_message", "body")?;
        let reply_to_message_id =
            optional_u64(&params, "tidepool_post_message", "reply_to_message_id")?;
        let client = shared_tidepool_client("tidepool_post_message").await?;
        client
            .post_message(domain_id, body.clone(), reply_to_message_id)
            .map_err(|error| tool_execution("tidepool_post_message", error))?;
        Ok(json!({
            "status": "posted",
            "domain_id": domain_id,
            "reply_to_message_id": reply_to_message_id,
            "body": body,
        }))
    }
}

async fn shared_tidepool_client(tool: &str) -> Result<crate::tidepool::TidepoolClient, RuntimeError> {
    require_shared_client()
        .await
        .map_err(|error| tool_execution(tool, error))
}

fn serialize_subscriptions(subscriptions: &[SubscriptionLookup]) -> Vec<Value> {
    subscriptions
        .iter()
        .map(|item| {
            json!({
                "domain_id": item.domain_id,
                "slug": item.slug,
                "title": item.title,
                "message_char_limit": item.message_char_limit,
                "batch_window_seconds": item.batch_window_seconds,
            })
        })
        .collect()
}

fn validate_empty_object(params: &Value, tool: &str) -> Result<(), RuntimeError> {
    let Some(object) = params.as_object() else {
        return Err(RuntimeError::InvalidToolParameters {
            tool: tool.to_string(),
            reason: "parameters must be a JSON object".to_string(),
        });
    };
    if !object.is_empty() {
        return Err(RuntimeError::InvalidToolParameters {
            tool: tool.to_string(),
            reason: "this tool does not accept parameters".to_string(),
        });
    }
    Ok(())
}

fn require_u64(params: &Value, tool: &str, field: &str) -> Result<u64, RuntimeError> {
    params
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| RuntimeError::InvalidToolParameters {
            tool: tool.to_string(),
            reason: format!("missing or invalid integer field '{field}'"),
        })
}

fn optional_u64(params: &Value, tool: &str, field: &str) -> Result<Option<u64>, RuntimeError> {
    match params.get(field) {
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| RuntimeError::InvalidToolParameters {
                tool: tool.to_string(),
                reason: format!("field '{field}' must be an integer"),
            }),
        None => Ok(None),
    }
}

fn optional_u32(params: &Value, tool: &str, field: &str) -> Result<Option<u32>, RuntimeError> {
    match params.get(field) {
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: tool.to_string(),
                    reason: format!("field '{field}' must be an integer"),
                });
            };
            let narrowed = u32::try_from(raw).map_err(|_| RuntimeError::InvalidToolParameters {
                tool: tool.to_string(),
                reason: format!("field '{field}' is too large"),
            })?;
            Ok(Some(narrowed))
        }
        None => Ok(None),
    }
}

fn tool_execution(tool: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolExecution {
        tool: tool.to_string(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn default_registry_includes_tidepool_tools() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();

        assert!(names.contains(&"tidepool_my_account".to_string()));
        assert!(names.contains(&"tidepool_list_subscriptions".to_string()));
        assert!(names.contains(&"tidepool_subscribe_domain".to_string()));
        assert!(names.contains(&"tidepool_unsubscribe_domain".to_string()));
        assert!(names.contains(&"tidepool_post_message".to_string()));
    }

    #[test]
    fn subscribe_domain_validation_rejects_non_numeric_domain_id() {
        let tool = TidepoolSubscribeDomainTool;
        let error = tool
            .validate(&json!({"domain_id":"not-a-number"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn post_message_validation_rejects_blank_body() {
        let tool = TidepoolPostMessageTool;
        let error = tool
            .validate(&json!({"domain_id":42,"body":"   "}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }
}