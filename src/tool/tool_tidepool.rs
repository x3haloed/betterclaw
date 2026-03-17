use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::error::RuntimeError;
use crate::generated::tidepool::{DomainKind, DomainRole, SubscriptionLookup};
use crate::tidepool::require_shared_client;

const DEFAULT_SUBSCRIBE_BATCH_WINDOW_SECONDS: u32 = 30;
const DEFAULT_CREATE_DOMAIN_MESSAGE_CHAR_LIMIT: u16 = 4096;

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

pub struct TidepoolCreateDomainTool;

#[async_trait]
impl Tool for TidepoolCreateDomainTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_create_domain".to_string(),
            description: "Create a new Tidepool domain. Use this to spin up coordination channels for multi-agent workflows.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["Public", "Private", "Dm"],
                        "description": "Domain visibility: Public (anyone can join), Private (invite-only), Dm (direct message)."
                    },
                    "slug": {
                        "type": "string",
                        "description": "URL-safe identifier for the domain (e.g. 'coord-lab')."
                    },
                    "title": {
                        "type": "string",
                        "description": "Human-readable name for the domain."
                    },
                    "message_char_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum message length. Defaults to 4096."
                    }
                },
                "required": ["kind", "slug", "title"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let kind = require_string(params, "tidepool_create_domain", "kind")?;
        match kind.as_str() {
            "Public" | "Private" | "Dm" => {}
            other => {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: "tidepool_create_domain".to_string(),
                    reason: format!(
                        "field 'kind' must be one of 'Public', 'Private', 'Dm'; got '{other}'"
                    ),
                });
            }
        }
        let slug = require_string(params, "tidepool_create_domain", "slug")?;
        if slug.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_create_domain".to_string(),
                reason: "field 'slug' must not be empty".to_string(),
            });
        }
        let title = require_string(params, "tidepool_create_domain", "title")?;
        if title.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_create_domain".to_string(),
                reason: "field 'title' must not be empty".to_string(),
            });
        }
        optional_u16(params, "tidepool_create_domain", "message_char_limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let kind_str = require_string(&params, "tidepool_create_domain", "kind")?;
        let kind = match kind_str.as_str() {
            "Public" => DomainKind::Public,
            "Private" => DomainKind::Private,
            "Dm" => DomainKind::Dm,
            other => {
                return Err(RuntimeError::ToolExecution {
                    tool: "tidepool_create_domain".to_string(),
                    reason: format!("unexpected kind '{other}'"),
                });
            }
        };
        let slug = require_string(&params, "tidepool_create_domain", "slug")?;
        let title = require_string(&params, "tidepool_create_domain", "title")?;
        let message_char_limit = optional_u16(
            &params,
            "tidepool_create_domain",
            "message_char_limit",
        )?
        .unwrap_or(DEFAULT_CREATE_DOMAIN_MESSAGE_CHAR_LIMIT);
        let client = shared_tidepool_client("tidepool_create_domain").await?;
        client
            .create_domain(kind, slug.clone(), title.clone(), message_char_limit)
            .map_err(|error| tool_execution("tidepool_create_domain", error))?;
        Ok(json!({
            "status": "created",
            "kind": kind_str,
            "slug": slug,
            "title": title,
            "message_char_limit": message_char_limit,
        }))
    }
}

pub struct TidepoolAddDomainMemberTool;

#[async_trait]
impl Tool for TidepoolAddDomainMemberTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_add_domain_member".to_string(),
            description: "Add an account as a member (or owner) of a Tidepool domain. Use this to invite other agents or users into coordination channels.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The domain to add the member to."
                    },
                    "account_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The account to add as a member."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["Owner", "Member"],
                        "description": "The role to grant. Owner can manage domain settings. Member can read and post."
                    }
                },
                "required": ["domain_id", "account_id", "role"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_add_domain_member", "domain_id")?;
        require_u64(params, "tidepool_add_domain_member", "account_id")?;
        let role = require_string(params, "tidepool_add_domain_member", "role")?;
        match role.as_str() {
            "Owner" | "Member" => {}
            other => {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: "tidepool_add_domain_member".to_string(),
                    reason: format!(
                        "field 'role' must be one of 'Owner', 'Member'; got '{other}'"
                    ),
                });
            }
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_add_domain_member", "domain_id")?;
        let account_id = require_u64(&params, "tidepool_add_domain_member", "account_id")?;
        let role_str = require_string(&params, "tidepool_add_domain_member", "role")?;
        let role = match role_str.as_str() {
            "Owner" => DomainRole::Owner,
            "Member" => DomainRole::Member,
            other => {
                return Err(RuntimeError::ToolExecution {
                    tool: "tidepool_add_domain_member".to_string(),
                    reason: format!("unexpected role '{other}'"),
                });
            }
        };
        let client = shared_tidepool_client("tidepool_add_domain_member").await?;
        client
            .add_domain_member(domain_id, account_id, role)
            .map_err(|error| tool_execution("tidepool_add_domain_member", error))?;
        Ok(json!({
            "status": "added",
            "domain_id": domain_id,
            "account_id": account_id,
            "role": role_str,
        }))
    }
}

pub struct TidepoolRemoveDomainMemberTool;

#[async_trait]
impl Tool for TidepoolRemoveDomainMemberTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_remove_domain_member".to_string(),
            description: "Remove an account from a Tidepool domain. Use this to revoke access to coordination channels.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The domain to remove the member from."
                    },
                    "account_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The account to remove."
                    }
                },
                "required": ["domain_id", "account_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_remove_domain_member", "domain_id")?;
        require_u64(params, "tidepool_remove_domain_member", "account_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_remove_domain_member", "domain_id")?;
        let account_id = require_u64(&params, "tidepool_remove_domain_member", "account_id")?;
        let client = shared_tidepool_client("tidepool_remove_domain_member").await?;
        client
            .remove_domain_member(domain_id, account_id)
            .map_err(|error| tool_execution("tidepool_remove_domain_member", error))?;
        Ok(json!({
            "status": "removed",
            "domain_id": domain_id,
            "account_id": account_id,
        }))
    }
}

pub struct TidepoolCreateDmTool;

#[async_trait]
impl Tool for TidepoolCreateDmTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_create_dm".to_string(),
            description: "Create a direct message channel between the configured account and one or more recipients. Use for private agent-to-agent coordination.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "recipient_account_ids": {
                        "type": "array",
                        "items": { "type": "integer", "minimum": 0 },
                        "minItems": 1,
                        "description": "Account IDs of the DM recipients."
                    },
                    "title": {
                        "type": "string",
                        "description": "Title for the DM channel."
                    }
                },
                "required": ["recipient_account_ids", "title"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let ids = params.get("recipient_account_ids").and_then(Value::as_array);
        match ids {
            Some(arr) if !arr.is_empty() => {
                for (i, v) in arr.iter().enumerate() {
                    if v.as_u64().is_none() {
                        return Err(RuntimeError::InvalidToolParameters {
                            tool: "tidepool_create_dm".to_string(),
                            reason: format!(
                                "recipient_account_ids[{i}] must be a non-negative integer"
                            ),
                        });
                    }
                }
            }
            _ => {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: "tidepool_create_dm".to_string(),
                    reason: "field 'recipient_account_ids' must be a non-empty array of integers"
                        .to_string(),
                });
            }
        }
        let title = require_string(params, "tidepool_create_dm", "title")?;
        if title.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_create_dm".to_string(),
                reason: "field 'title' must not be empty".to_string(),
            });
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let ids: Vec<u64> = params
            .get("recipient_account_ids")
            .and_then(Value::as_array)
            .expect("validated")
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        let title = require_string(&params, "tidepool_create_dm", "title")?;
        let client = shared_tidepool_client("tidepool_create_dm").await?;
        client
            .create_dm(ids.clone(), title.clone())
            .map_err(|error| tool_execution("tidepool_create_dm", error))?;
        Ok(json!({
            "status": "created",
            "recipient_account_ids": ids,
            "title": title,
        }))
    }
}

const DEFAULT_READ_MESSAGES_LIMIT: usize = 50;

pub struct TidepoolListDmDomainsTool;

#[async_trait]
impl Tool for TidepoolListDmDomainsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_list_dm_domains".to_string(),
            description: "List direct message domains the configured account participates in. Shows domain ID, title, and participant account IDs for each DM channel.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        validate_empty_object(params, "tidepool_list_dm_domains")
    }

    async fn call(&self, _params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let client = shared_tidepool_client("tidepool_list_dm_domains").await?;
        let dm_domains = client.dm_domains();
        Ok(json!({
            "dm_domains": dm_domains.iter().map(|dm| json!({
                "domain_id": dm.domain_id,
                "title": dm.title,
                "participant_account_ids": dm.participant_account_ids,
            })).collect::<Vec<_>>(),
            "count": dm_domains.len(),
        }))
    }
}

pub struct TidepoolListDomainMembersTool;

#[async_trait]
impl Tool for TidepoolListDomainMembersTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_list_domain_members".to_string(),
            description: "List members of Tidepool domains. Optionally filter to a specific domain. Returns account ID, role, and join timestamp for each member.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Filter to a specific domain. If omitted, returns members from all domains."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u64(params, "tidepool_list_domain_members", "domain_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = optional_u64(&params, "tidepool_list_domain_members", "domain_id")?;
        let client = shared_tidepool_client("tidepool_list_domain_members").await?;
        let members = client.domain_members(domain_id);
        Ok(json!({
            "members": members.iter().map(|m| json!({
                "membership_id": m.membership_id,
                "domain_id": m.domain_id,
                "account_id": m.account_id,
                "role": format!("{:?}", m.role),
            })).collect::<Vec<_>>(),
            "count": members.len(),
            "domain_filter": domain_id,
        }))
    }
}

pub struct TidepoolReadMessagesTool;

#[async_trait]
impl Tool for TidepoolReadMessagesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_read_messages".to_string(),
            description: "Read recent messages from Tidepool domains the account is subscribed to. Optionally filter by domain_id. Returns the most recent messages up to the specified limit.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Filter messages to a specific domain. If omitted, returns messages from all subscribed domains."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of messages to return. Defaults to 50."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u64(params, "tidepool_read_messages", "domain_id")?;
        optional_u32(params, "tidepool_read_messages", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = optional_u64(&params, "tidepool_read_messages", "domain_id")?;
        let limit = optional_u32(&params, "tidepool_read_messages", "limit")?
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_READ_MESSAGES_LIMIT);
        let client = shared_tidepool_client("tidepool_read_messages").await?;
        let messages = client.read_messages(domain_id, limit);
        Ok(json!({
            "messages": messages.iter().map(|m| json!({
                "message_id": m.message_id,
                "domain_id": m.domain_id,
                "domain_title": m.domain_title,
                "domain_slug": m.domain_slug,
                "domain_sequence": m.domain_sequence,
                "author_account_id": m.author_account_id,
                "body": m.body,
                "reply_to_message_id": m.reply_to_message_id,
            })).collect::<Vec<_>>(),
            "count": messages.len(),
            "domain_filter": domain_id,
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

fn optional_u16(params: &Value, tool: &str, field: &str) -> Result<Option<u16>, RuntimeError> {
    match params.get(field) {
        Some(value) => {
            let Some(raw) = value.as_u64() else {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: tool.to_string(),
                    reason: format!("field '{field}' must be an integer"),
                });
            };
            let narrowed = u16::try_from(raw).map_err(|_| RuntimeError::InvalidToolParameters {
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
        assert!(names.contains(&"tidepool_create_domain".to_string()));
        assert!(names.contains(&"tidepool_add_domain_member".to_string()));
        assert!(names.contains(&"tidepool_remove_domain_member".to_string()));
        assert!(names.contains(&"tidepool_create_dm".to_string()));
        assert!(names.contains(&"tidepool_list_dm_domains".to_string()));
        assert!(names.contains(&"tidepool_list_domain_members".to_string()));
        assert!(names.contains(&"tidepool_read_messages".to_string()));
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

    #[test]
    fn create_domain_validation_rejects_invalid_kind() {
        let tool = TidepoolCreateDomainTool;
        let error = tool
            .validate(&json!({"kind":"Secret","slug":"test","title":"Test"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn create_domain_validation_rejects_blank_slug() {
        let tool = TidepoolCreateDomainTool;
        let error = tool
            .validate(&json!({"kind":"Public","slug":"  ","title":"Test"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn create_domain_validation_accepts_valid_params() {
        let tool = TidepoolCreateDomainTool;
        tool.validate(&json!({
            "kind": "Private",
            "slug": "my-domain",
            "title": "My Domain",
            "message_char_limit": 2048
        }))
        .unwrap();
    }

    #[test]
    fn add_domain_member_validation_rejects_invalid_role() {
        let tool = TidepoolAddDomainMemberTool;
        let error = tool
            .validate(&json!({"domain_id": 1, "account_id": 42, "role": "Admin"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn add_domain_member_validation_accepts_valid_params() {
        let tool = TidepoolAddDomainMemberTool;
        tool.validate(&json!({
            "domain_id": 1,
            "account_id": 42,
            "role": "Member"
        }))
        .unwrap();
    }

    #[test]
    fn remove_domain_member_validation_requires_both_ids() {
        let tool = TidepoolRemoveDomainMemberTool;
        let error = tool.validate(&json!({"domain_id": 1})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn remove_domain_member_validation_accepts_valid_params() {
        let tool = TidepoolRemoveDomainMemberTool;
        tool.validate(&json!({
            "domain_id": 1,
            "account_id": 42
        }))
        .unwrap();
    }

    #[test]
    fn remove_domain_member_validation_rejects_non_numeric() {
        let tool = TidepoolRemoveDomainMemberTool;
        let error = tool
            .validate(&json!({"domain_id": "abc", "account_id": 42}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn create_dm_validation_rejects_empty_recipients() {
        let tool = TidepoolCreateDmTool;
        let error = tool
            .validate(&json!({"recipient_account_ids": [], "title": "Test"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn create_dm_validation_rejects_non_integer_recipients() {
        let tool = TidepoolCreateDmTool;
        let error = tool
            .validate(&json!({"recipient_account_ids": ["not-a-number"], "title": "Test"}))
            .unwrap_err();

        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn create_dm_validation_accepts_valid_params() {
        let tool = TidepoolCreateDmTool;
        tool.validate(&json!({
            "recipient_account_ids": [1, 42],
            "title": "Direct chat"
        }))
        .unwrap();
    }

    #[test]
    fn list_dm_domains_validation_accepts_empty_params() {
        let tool = TidepoolListDmDomainsTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn list_dm_domains_validation_rejects_params() {
        let tool = TidepoolListDmDomainsTool;
        let error = tool.validate(&json!({"domain_id": 1})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn list_domain_members_validation_accepts_empty_params() {
        let tool = TidepoolListDomainMembersTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn list_domain_members_validation_accepts_domain_filter() {
        let tool = TidepoolListDomainMembersTool;
        tool.validate(&json!({"domain_id": 42})).unwrap();
    }

    #[test]
    fn list_domain_members_validation_rejects_non_numeric() {
        let tool = TidepoolListDomainMembersTool;
        let error = tool
            .validate(&json!({"domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn read_messages_validation_accepts_empty_params() {
        let tool = TidepoolReadMessagesTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn read_messages_validation_accepts_domain_filter() {
        let tool = TidepoolReadMessagesTool;
        tool.validate(&json!({"domain_id": 42, "limit": 100})).unwrap();
    }

    #[test]
    fn read_messages_validation_rejects_invalid_domain_id() {
        let tool = TidepoolReadMessagesTool;
        let error = tool
            .validate(&json!({"domain_id": "not-a-number"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }
}