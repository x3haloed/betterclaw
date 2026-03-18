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
            description: "Read recent messages from Tidepool domains the account is subscribed to. Optionally filter by domain_id and/or after_message_id for incremental reads. Returns the most recent messages up to the specified limit.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Filter messages to a specific domain. If omitted, returns messages from all subscribed domains."
                    },
                    "after_message_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Only return messages with message_id strictly greater than this value. Useful for incremental polling after the last message you've seen."
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
        optional_u64(params, "tidepool_read_messages", "after_message_id")?;
        optional_u32(params, "tidepool_read_messages", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = optional_u64(&params, "tidepool_read_messages", "domain_id")?;
        let after_message_id = optional_u64(&params, "tidepool_read_messages", "after_message_id")?;
        let limit = optional_u32(&params, "tidepool_read_messages", "limit")?
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_READ_MESSAGES_LIMIT);
        let client = shared_tidepool_client("tidepool_read_messages").await?;
        let messages = client.read_messages_filtered(domain_id, after_message_id, limit);
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
            "after_message_id": after_message_id,
        }))
    }
}

pub struct TidepoolAgentPresenceTool;

#[async_trait]
impl Tool for TidepoolAgentPresenceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_agent_presence".to_string(),
            description: "Detect which agents/accounts are active in Tidepool domains by analyzing recent message activity. Returns presence information including last activity, message count, and active domains for each recently-active account. Use this to determine who is online and responsive before sending coordination messages.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Restrict presence analysis to a specific domain. If omitted, analyzes all subscribed domains."
                    },
                    "window_size": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of active accounts to return, ordered by most recent activity. Defaults to 20."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u64(params, "tidepool_agent_presence", "domain_id")?;
        optional_u32(params, "tidepool_agent_presence", "window_size")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = optional_u64(&params, "tidepool_agent_presence", "domain_id")?;
        let window_size = optional_u32(&params, "tidepool_agent_presence", "window_size")?
            .map(|v| v as usize)
            .unwrap_or(20);
        let client = shared_tidepool_client("tidepool_agent_presence").await?;
        let presence = client.agent_presence(domain_id, window_size);
        Ok(json!({
            "agents": presence.iter().map(|p| json!({
                "account_id": p.account_id,
                "last_message_id": p.last_message_id,
                "last_domain_id": p.last_domain_id,
                "last_domain_title": p.last_domain_title,
                "message_count": p.message_count,
                "active_domain_ids": p.active_domain_ids,
            })).collect::<Vec<_>>(),
            "count": presence.len(),
            "domain_filter": domain_id,
        }))
    }
}

pub struct TidepoolAgentHealthTool;

#[async_trait]
impl Tool for TidepoolAgentHealthTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_agent_health".to_string(),
            description: "Check agent health status in Tidepool domains by analyzing recent \
                message activity with time-based health assessment. Returns each agent's \
                last activity time, seconds since last message, and health status \
                (active/idle/stale/silent). Use this to determine if BUZZ, CHIP, or other \
                agents are responsive or potentially down. Prefer this over \
                tidepool_agent_presence when you need health diagnostics.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "account_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Check health for a specific agent account. If omitted, checks all recently-active accounts."
                    },
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Restrict health analysis to a specific domain. If omitted, analyzes all subscribed domains."
                    },
                    "window_size": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of agents to return, ordered by most recent activity. Defaults to 20."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u64(params, "tidepool_agent_health", "account_id")?;
        optional_u64(params, "tidepool_agent_health", "domain_id")?;
        optional_u32(params, "tidepool_agent_health", "window_size")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let account_id = optional_u64(&params, "tidepool_agent_health", "account_id")?;
        let domain_id = optional_u64(&params, "tidepool_agent_health", "domain_id")?;
        let window_size = optional_u32(&params, "tidepool_agent_health", "window_size")?
            .map(|v| v as usize)
            .unwrap_or(20);
        let client = shared_tidepool_client("tidepool_agent_health").await?;
        let health = client.agent_health(account_id, domain_id, window_size);
        Ok(json!({
            "agents": health.iter().map(|h| json!({
                "account_id": h.account_id,
                "last_message_id": h.last_message_id,
                "last_domain_id": h.last_domain_id,
                "last_domain_title": h.last_domain_title,
                "message_count": h.message_count,
                "active_domain_ids": h.active_domain_ids,
                "seconds_since_last_message": h.seconds_since_last_message,
                "health_status": h.health_status,
            })).collect::<Vec<_>>(),
            "count": health.len(),
            "domain_filter": domain_id,
            "account_filter": account_id,
        }))
    }
}

pub struct TidepoolGetThreadTool;

#[async_trait]
impl Tool for TidepoolGetThreadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_get_thread".to_string(),
            description: "Retrieve all replies to a specific Tidepool message. Use this to read the \
                full conversation thread for a message, identified by its message_id. Returns \
                direct replies ordered by message ID. Optionally filter to a specific domain."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "message_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The message_id to retrieve replies for."
                    },
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Restrict to a specific domain."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of replies to return. Defaults to 50."
                    }
                },
                "required": ["message_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_get_thread", "message_id")?;
        optional_u64(params, "tidepool_get_thread", "domain_id")?;
        optional_u32(params, "tidepool_get_thread", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let message_id = require_u64(&params, "tidepool_get_thread", "message_id")?;
        let domain_id = optional_u64(&params, "tidepool_get_thread", "domain_id")?;
        let limit = optional_u32(&params, "tidepool_get_thread", "limit")?
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_READ_MESSAGES_LIMIT);
        let client = shared_tidepool_client("tidepool_get_thread").await?;
        let replies = client.get_thread(message_id, domain_id, limit);
        Ok(json!({
            "root_message_id": message_id,
            "replies": replies.iter().map(|m| json!({
                "message_id": m.message_id,
                "domain_id": m.domain_id,
                "domain_title": m.domain_title,
                "domain_slug": m.domain_slug,
                "domain_sequence": m.domain_sequence,
                "author_account_id": m.author_account_id,
                "body": m.body,
                "reply_to_message_id": m.reply_to_message_id,
            })).collect::<Vec<_>>(),
            "count": replies.len(),
            "domain_filter": domain_id,
        }))
    }
}

pub struct TidepoolSearchMessagesTool;

#[async_trait]
impl Tool for TidepoolSearchMessagesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_search_messages".to_string(),
            description: "Search message history by content across subscribed Tidepool domains. \
                Case-insensitive substring match on message body. Supports filtering by domain, \
                author, and returning only messages after a given ID. Returns most recent matches first."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text to search for in message bodies. Case-insensitive substring match."
                    },
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Restrict search to a specific domain."
                    },
                    "author_account_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Only return messages from this account."
                    },
                    "after_message_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Only search messages with message_id greater than this value."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of results to return. Defaults to 20."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let query = require_string(params, "tidepool_search_messages", "query")?;
        if query.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_search_messages".to_string(),
                reason: "field 'query' must not be empty".to_string(),
            });
        }
        optional_u64(params, "tidepool_search_messages", "domain_id")?;
        optional_u64(params, "tidepool_search_messages", "author_account_id")?;
        optional_u64(params, "tidepool_search_messages", "after_message_id")?;
        optional_u32(params, "tidepool_search_messages", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let query = require_string(&params, "tidepool_search_messages", "query")?;
        let domain_id = optional_u64(&params, "tidepool_search_messages", "domain_id")?;
        let author_account_id =
            optional_u64(&params, "tidepool_search_messages", "author_account_id")?;
        let after_message_id =
            optional_u64(&params, "tidepool_search_messages", "after_message_id")?;
        let limit = optional_u32(&params, "tidepool_search_messages", "limit")?
            .map(|v| v as usize)
            .unwrap_or(20);
        let client = shared_tidepool_client("tidepool_search_messages").await?;
        let messages = client.search_messages(&query, domain_id, author_account_id, after_message_id, limit);
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
            "query": query,
            "domain_filter": domain_id,
            "author_filter": author_account_id,
        }))
    }
}

pub struct TidepoolFindMentionsTool;

#[async_trait]
impl Tool for TidepoolFindMentionsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_find_mentions".to_string(),
            description: "Find messages that mention a specific agent handle via @handle mentions. \
                Parses message bodies for @handle patterns (case-insensitive). Use this to find \
                coordination messages directed at you or another agent without scanning all domain \
                messages. Returns most recent mentions first.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "handle": {
                        "type": "string",
                        "description": "The handle to search for (e.g. 'buzz', 'horus', 'chip'). Do NOT include the @ prefix."
                    },
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Optional. Restrict search to a specific domain."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of mention messages to return. Defaults to 20."
                    }
                },
                "required": ["handle"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let handle = require_string(params, "tidepool_find_mentions", "handle")?;
        if handle.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_find_mentions".to_string(),
                reason: "field 'handle' must not be empty".to_string(),
            });
        }
        optional_u64(params, "tidepool_find_mentions", "domain_id")?;
        optional_u32(params, "tidepool_find_mentions", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let handle = require_string(&params, "tidepool_find_mentions", "handle")?;
        let domain_id = optional_u64(&params, "tidepool_find_mentions", "domain_id")?;
        let limit = optional_u32(&params, "tidepool_find_mentions", "limit")?
            .map(|v| v as usize)
            .unwrap_or(20);
        let client = shared_tidepool_client("tidepool_find_mentions").await?;
        let mentions = client.find_mentions(&handle, domain_id, limit);
        Ok(json!({
            "handle": handle,
            "mentions": mentions.iter().map(|m| json!({
                "message_id": m.message_id,
                "domain_id": m.domain_id,
                "domain_title": m.domain_title,
                "domain_slug": m.domain_slug,
                "author_account_id": m.author_account_id,
                "body": m.body,
                "reply_to_message_id": m.reply_to_message_id,
            })).collect::<Vec<_>>(),
            "count": mentions.len(),
            "domain_filter": domain_id,
        }))
    }
}

pub struct TidepoolSystemStatusTool;

#[async_trait]
impl Tool for TidepoolSystemStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_system_status".to_string(),
            description: "Get a comprehensive single-call overview of the entire Tidepool coordination system. \
                Returns subscribed domains with per-domain activity stats, agent health across all domains, \
                and recent thread activity. Use this as the first-call dashboard to understand what's happening \
                before diving into specific domains or agents. Replaces the need to call tidepool_list_subscriptions, \
                tidepool_read_messages, tidepool_agent_health, and tidepool_agent_presence separately."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "recent_message_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Number of most recent messages to include per domain. Defaults to 5."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u32(params, "tidepool_system_status", "recent_message_limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let per_domain_limit = optional_u32(&params, "tidepool_system_status", "recent_message_limit")?
            .map(|v| v as usize)
            .unwrap_or(5);

        let client = shared_tidepool_client("tidepool_system_status").await?;

        // 1. Subscriptions
        let subscriptions = client.subscriptions();

        // 2. All messages for aggregation
        let all_messages = client.read_messages_filtered(None, None, 500);

        // 3. Per-domain activity
        let mut domain_stats: std::collections::HashMap<u64, serde_json::Map<String, Value>> =
            std::collections::HashMap::new();
        for sub in &subscriptions {
            let mut stats = serde_json::Map::new();
            stats.insert("domain_id".into(), json!(sub.domain_id));
            stats.insert("slug".into(), json!(sub.slug));
            stats.insert("title".into(), json!(sub.title));
            stats.insert("message_count".into(), json!(0));
            stats.insert("last_message_id".into(), json!(null));
            stats.insert("last_message_preview".into(), json!(null));
            stats.insert("last_author_account_id".into(), json!(null));
            stats.insert("unique_authors".into(), json!([]));
            stats.insert("threaded_messages".into(), json!(0));
            domain_stats.insert(sub.domain_id, stats);
        }

        for msg in &all_messages {
            if let Some(stats) = domain_stats.get_mut(&msg.domain_id) {
                let count = stats["message_count"].as_u64().unwrap_or(0) + 1;
                stats.insert("message_count".into(), json!(count));

                if msg.message_id > stats["last_message_id"].as_u64().unwrap_or(0) {
                    stats.insert("last_message_id".into(), json!(msg.message_id));
                    stats.insert(
                        "last_message_preview".into(),
                        json!(msg.body.chars().take(120).collect::<String>()),
                    );
                    stats.insert("last_author_account_id".into(), json!(msg.author_account_id));
                }

                if msg.reply_to_message_id.is_some() {
                    let threaded = stats["threaded_messages"].as_u64().unwrap_or(0) + 1;
                    stats.insert("threaded_messages".into(), json!(threaded));
                }

                // Track unique authors
                if let Some(authors) = stats.get_mut("unique_authors") {
                    if let Some(arr) = authors.as_array_mut() {
                        if !arr.contains(&json!(msg.author_account_id)) {
                            arr.push(json!(msg.author_account_id));
                        }
                    }
                }
            }
        }

        // Build per-domain summaries with recent messages
        let mut domains: Vec<Value> = Vec::new();
        for sub in &subscriptions {
            let mut domain_entry = if let Some(stats) = domain_stats.remove(&sub.domain_id) {
                stats
            } else {
                let mut stats = serde_json::Map::new();
                stats.insert("domain_id".into(), json!(sub.domain_id));
                stats.insert("slug".into(), json!(sub.slug));
                stats.insert("title".into(), json!(sub.title));
                stats
            };

            // Recent messages for this domain
            let recent: Vec<Value> = all_messages
                .iter()
                .filter(|m| m.domain_id == sub.domain_id)
                .rev()
                .take(per_domain_limit)
                .map(|m| {
                    json!({
                        "message_id": m.message_id,
                        "author_account_id": m.author_account_id,
                        "body_preview": m.body.chars().take(100).collect::<String>(),
                        "reply_to_message_id": m.reply_to_message_id,
                    })
                })
                .collect();
            domain_entry.insert("recent_messages".into(), json!(recent));

            domains.push(Value::Object(domain_entry));
        }

        // 4. Agent health
        let health = client.agent_health(None, None, 20);
        let agents: Vec<Value> = health
            .iter()
            .map(|h| {
                json!({
                    "account_id": h.account_id,
                    "health_status": h.health_status,
                    "seconds_since_last_message": h.seconds_since_last_message,
                    "message_count": h.message_count,
                    "last_domain_id": h.last_domain_id,
                    "last_domain_title": h.last_domain_title,
                    "active_domain_ids": h.active_domain_ids,
                })
            })
            .collect();

        // 5. Thread activity (messages that are replies)
        let threads: Vec<Value> = all_messages
            .iter()
            .filter(|m| m.reply_to_message_id.is_some())
            .rev()
            .take(10)
            .map(|m| {
                json!({
                    "message_id": m.message_id,
                    "domain_id": m.domain_id,
                    "author_account_id": m.author_account_id,
                    "reply_to_message_id": m.reply_to_message_id,
                    "body_preview": m.body.chars().take(80).collect::<String>(),
                })
            })
            .collect();

        Ok(json!({
            "domains": domains,
            "agents": agents,
            "recent_threads": threads,
            "total_messages": all_messages.len(),
            "total_domains": subscriptions.len(),
            "total_agents": agents.len(),
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
        assert!(names.contains(&"tidepool_get_thread".to_string()));
        assert!(names.contains(&"tidepool_search_messages".to_string()));
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

    #[test]
    fn read_messages_validation_accepts_after_message_id() {
        let tool = TidepoolReadMessagesTool;
        tool.validate(&json!({"after_message_id": 100})).unwrap();
        tool.validate(&json!({"domain_id": 1, "after_message_id": 100, "limit": 20}))
            .unwrap();
    }

    #[test]
    fn read_messages_validation_rejects_invalid_after_message_id() {
        let tool = TidepoolReadMessagesTool;
        let error = tool
            .validate(&json!({"after_message_id": "not-a-number"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn search_messages_validation_requires_query() {
        let tool = TidepoolSearchMessagesTool;
        let error = tool.validate(&json!({})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn search_messages_validation_rejects_blank_query() {
        let tool = TidepoolSearchMessagesTool;
        let error = tool.validate(&json!({"query": "   "})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn search_messages_validation_accepts_valid_params() {
        let tool = TidepoolSearchMessagesTool;
        tool.validate(&json!({"query": "hello"})).unwrap();
        tool.validate(&json!({
            "query": "hello",
            "domain_id": 1,
            "author_account_id": 42,
            "after_message_id": 100,
            "limit": 50
        }))
        .unwrap();
    }

    #[test]
    fn search_messages_validation_rejects_invalid_domain_id() {
        let tool = TidepoolSearchMessagesTool;
        let error = tool
            .validate(&json!({"query": "test", "domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_search_messages_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_search_messages".to_string()));
    }

    #[test]
    fn default_registry_includes_agent_presence_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_agent_presence".to_string()));
    }

    #[test]
    fn agent_presence_validation_accepts_empty_params() {
        let tool = TidepoolAgentPresenceTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn agent_presence_validation_accepts_domain_filter() {
        let tool = TidepoolAgentPresenceTool;
        tool.validate(&json!({"domain_id": 42})).unwrap();
    }

    #[test]
    fn agent_presence_validation_accepts_window_size() {
        let tool = TidepoolAgentPresenceTool;
        tool.validate(&json!({"window_size": 5})).unwrap();
        tool.validate(&json!({"domain_id": 1, "window_size": 50})).unwrap();
    }

    #[test]
    fn agent_presence_validation_rejects_invalid_domain_id() {
        let tool = TidepoolAgentPresenceTool;
        let error = tool
            .validate(&json!({"domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn agent_presence_validation_rejects_invalid_window_size() {
        let tool = TidepoolAgentPresenceTool;
        let error = tool
            .validate(&json!({"window_size": "big"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn get_thread_validation_requires_message_id() {
        let tool = TidepoolGetThreadTool;
        let error = tool.validate(&json!({})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn get_thread_validation_accepts_valid_params() {
        let tool = TidepoolGetThreadTool;
        tool.validate(&json!({"message_id": 42})).unwrap();
        tool.validate(&json!({"message_id": 42, "domain_id": 1, "limit": 100})).unwrap();
    }

    #[test]
    fn get_thread_validation_rejects_non_numeric_message_id() {
        let tool = TidepoolGetThreadTool;
        let error = tool
            .validate(&json!({"message_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn get_thread_validation_rejects_invalid_domain_id() {
        let tool = TidepoolGetThreadTool;
        let error = tool
            .validate(&json!({"message_id": 1, "domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_agent_health_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_agent_health".to_string()));
    }

    #[test]
    fn agent_health_validation_accepts_empty_params() {
        let tool = TidepoolAgentHealthTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn agent_health_validation_accepts_all_filters() {
        let tool = TidepoolAgentHealthTool;
        tool.validate(&json!({"account_id": 1, "domain_id": 42, "window_size": 10})).unwrap();
    }

    #[test]
    fn agent_health_validation_rejects_invalid_account_id() {
        let tool = TidepoolAgentHealthTool;
        let error = tool
            .validate(&json!({"account_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn agent_health_validation_rejects_invalid_window_size() {
        let tool = TidepoolAgentHealthTool;
        let error = tool
            .validate(&json!({"window_size": -1}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_system_status_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_system_status".to_string()));
    }

    #[test]
    fn system_status_validation_accepts_empty_params() {
        let tool = TidepoolSystemStatusTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn system_status_validation_accepts_recent_message_limit() {
        let tool = TidepoolSystemStatusTool;
        tool.validate(&json!({"recent_message_limit": 10})).unwrap();
    }

    #[test]
    fn system_status_validation_rejects_invalid_limit() {
        let tool = TidepoolSystemStatusTool;
        let error = tool
            .validate(&json!({"recent_message_limit": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn system_status_validation_rejects_invalid_type() {
        let tool = TidepoolSystemStatusTool;
        let error = tool
            .validate(&json!({"recent_message_limit": -5}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_find_mentions_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_find_mentions".to_string()));
    }

    #[test]
    fn find_mentions_validation_requires_handle() {
        let tool = TidepoolFindMentionsTool;
        let error = tool.validate(&json!({})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn find_mentions_validation_rejects_blank_handle() {
        let tool = TidepoolFindMentionsTool;
        let error = tool.validate(&json!({"handle": "   "})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn find_mentions_validation_accepts_valid_params() {
        let tool = TidepoolFindMentionsTool;
        tool.validate(&json!({"handle": "buzz"})).unwrap();
        tool.validate(&json!({"handle": "horus", "domain_id": 1, "limit": 50})).unwrap();
    }

    #[test]
    fn find_mentions_validation_rejects_invalid_domain_id() {
        let tool = TidepoolFindMentionsTool;
        let error = tool
            .validate(&json!({"handle": "buzz", "domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn find_mentions_validation_rejects_invalid_limit() {
        let tool = TidepoolFindMentionsTool;
        let error = tool
            .validate(&json!({"handle": "buzz", "limit": "big"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }
}