use async_trait::async_trait;
use serde_json::{Value, json};

use super::*;
use crate::error::RuntimeError;
use crate::generated::tidepool::{DomainKind, DomainRole, SubscriptionLookup};
use crate::tidepool::{TidepoolConfig, require_shared_client};

const DEFAULT_SUBSCRIBE_BATCH_WINDOW_SECONDS: u32 = 30;
const DEFAULT_CREATE_DOMAIN_MESSAGE_CHAR_LIMIT: u16 = 4096;

pub fn registration_tool_should_be_available() -> bool {
    TidepoolConfig::from_env()
        .map(|config| !config.token_exists())
        .unwrap_or(false)
}

pub struct TidepoolCompleteRegistrationTool;

#[async_trait]
impl Tool for TidepoolCompleteRegistrationTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_complete_registration".to_string(),
            description: "Create and save the Tidepool registration token for this BetterClaw runtime using the configured Tidepool env. Use this only when Tidepool is configured but no saved token file exists yet.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        validate_empty_object(params, "tidepool_complete_registration")
    }

    async fn call(&self, _params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let config = TidepoolConfig::from_env().ok_or_else(|| RuntimeError::ToolExecution {
            tool: "tidepool_complete_registration".to_string(),
            reason: "Tidepool is not configured in env; TIDEPOOL_DATABASE, TIDEPOOL_HANDLE, and TIDEPOOL_TOKEN_PATH are required".to_string(),
        })?;

        if config.token_exists() {
            return Err(RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!(
                    "a Tidepool token is already saved at {}; registration is not needed",
                    config.token_path.display()
                ),
            });
        }

        if let Some(parent) = config.token_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!(
                    "creating Tidepool token directory {}: {error}",
                    parent.display()
                ),
            })?;
        }

        let url = format!(
            "{}/v1/database/{}/call/create_account",
            config.base_url.trim_end_matches('/'),
            config.database
        );
        let response = reqwest::Client::new()
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&vec![config.handle.clone()])
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!("sending Tidepool registration request: {error}"),
            })?;

        let status = response.status();
        let token = response
            .headers()
            .get("spacetime-identity-token")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let body = response
            .text()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!("reading Tidepool registration response: {error}"),
            })?;

        if !status.is_success() {
            return Err(RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!("Tidepool registration failed with {status}: {body}"),
            });
        }

        let token = token.ok_or_else(|| RuntimeError::ToolExecution {
            tool: "tidepool_complete_registration".to_string(),
            reason: "Tidepool registration succeeded but no spacetime-identity-token header was returned".to_string(),
        })?;

        std::fs::write(&config.token_path, format!("{token}\n")).map_err(|error| {
            RuntimeError::ToolExecution {
                tool: "tidepool_complete_registration".to_string(),
                reason: format!(
                    "writing Tidepool token to {}: {error}",
                    config.token_path.display()
                ),
            }
        })?;

        Ok(json!({
            "status": "registered",
            "handle": config.handle,
            "token_path": config.token_path,
            "base_url": config.base_url,
            "database": config.database,
            "channel_restart_required": true,
        }))
    }
}

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

pub struct TidepoolJoinDomainTool;

#[async_trait]
impl Tool for TidepoolJoinDomainTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_join_domain".to_string(),
            description: "Self-join a public Tidepool domain. Unlike tidepool_add_domain_member (which \
                requires owner privileges to add another account), this lets the configured agent \
                join any public domain on its own. Use this to enter coordination channels without \
                waiting for an owner to add you. Once joined, you can read and post messages in the \
                domain. To also receive messages via the subscription feed, call \
                tidepool_subscribe_domain after joining.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The public domain to join."
                    }
                },
                "required": ["domain_id"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_join_domain", "domain_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_join_domain", "domain_id")?;
        let client = shared_tidepool_client("tidepool_join_domain").await?;
        client
            .join_domain(domain_id)
            .map_err(|error| tool_execution("tidepool_join_domain", error))?;
        Ok(json!({
            "status": "joined",
            "domain_id": domain_id,
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

pub struct TidepoolMessageAgentTool;

#[async_trait]
impl Tool for TidepoolMessageAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_message_agent".to_string(),
            description: "Send a direct message to another agent by handle. Automatically finds an existing DM or creates one if needed. This is the fastest way to message another agent — no need to look up account IDs or DM domain IDs first.".to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "handle": {
                        "type": "string",
                        "description": "Handle of the agent to message (e.g. 'buzz', 'chip', 'horus'). Case-insensitive."
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body to send."
                    }
                },
                "required": ["handle", "body"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let handle = require_string(params, "tidepool_message_agent", "handle")?;
        if handle.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_message_agent".to_string(),
                reason: "handle must not be blank".to_string(),
            });
        }
        let body = require_string(params, "tidepool_message_agent", "body")?;
        if body.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_message_agent".to_string(),
                reason: "body must not be blank".to_string(),
            });
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let handle = require_string(&params, "tidepool_message_agent", "handle")?;
        let body = require_string(&params, "tidepool_message_agent", "body")?;
        let client = shared_tidepool_client("tidepool_message_agent").await?;

        let (domain_id, created_dm, sent_body) = client
            .message_agent(&handle, &body)
            .await
            .map_err(|e| tool_execution("tidepool_message_agent", e))?;

        Ok(json!({
            "domain_id": domain_id,
            "created_dm": created_dm,
            "body": sent_body,
            "target_handle": handle,
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

pub struct TidepoolLookupAccountTool;

#[async_trait]
impl Tool for TidepoolLookupAccountTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_lookup_account".to_string(),
            description: "Resolve Tidepool account information by handle or account_id. \
                Use this to convert between agent handles (e.g. 'buzz', 'horus', 'chip') \
                and their numeric account IDs. At least one of handle or account_id must \
                be provided. Returns matching accounts with their ID, handle, and status."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "handle": {
                        "type": "string",
                        "description": "The account handle to look up (e.g. 'buzz', 'horus'). Case-sensitive."
                    },
                    "account_id": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "The numeric account ID to look up."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let has_handle = params.get("handle").is_some();
        let has_account_id = params.get("account_id").is_some();
        if !has_handle && !has_account_id {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_lookup_account".to_string(),
                reason: "at least one of 'handle' or 'account_id' must be provided".to_string(),
            });
        }
        if let Some(handle) = params.get("handle") {
            if handle.as_str().map(|s| s.trim().is_empty()).unwrap_or(true) {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: "tidepool_lookup_account".to_string(),
                    reason: "field 'handle' must not be empty".to_string(),
                });
            }
        }
        optional_u64(params, "tidepool_lookup_account", "account_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let handle = params.get("handle").and_then(|v| v.as_str());
        let account_id = params.get("account_id").and_then(|v| v.as_u64());
        let client = shared_tidepool_client("tidepool_lookup_account").await?;
        let accounts = client
            .resolve_accounts(handle, account_id)
            .await
            .map_err(|error| tool_execution("tidepool_lookup_account", error))?;
        Ok(json!({
            "accounts": accounts.iter().map(|a| json!({
                "account_id": a.account_id,
                "handle": a.handle,
                "status": a.status,
            })).collect::<Vec<_>>(),
            "count": accounts.len(),
            "handle_filter": handle,
            "account_id_filter": account_id,
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

// ── Task Claiming Protocol ──────────────────────────────────────────────────
//
// Lightweight coordination protocol built on message conventions.
// No Tidepool module changes required — uses formatted messages:
//
//   [CLAIM <handle>] <task description>   — claim a task
//   [DONE <handle>] <task description>    — mark it complete
//
// Agents scan CLAIM messages and filter out those with matching DONE messages.

const CLAIM_PREFIX: &str = "[CLAIM ";
const DONE_PREFIX: &str = "[DONE ";

/// Parsed claim from a message body.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TaskClaim {
    message_id: u64,
    domain_id: u64,
    domain_sequence: u64,
    author_account_id: u64,
    handle: String,
    description: String,
}

/// Parse a [CLAIM handle] message. Returns None if not a valid claim.
fn parse_claim_message(msg: &crate::tidepool::TidepoolInboundMessage) -> Option<TaskClaim> {
    let body = msg.body.trim();
    if !body.starts_with(CLAIM_PREFIX) {
        return None;
    }
    let rest = &body[CLAIM_PREFIX.len()..];
    let end_bracket = rest.find(']')?;
    let handle = rest[..end_bracket].trim().to_string();
    if handle.is_empty() {
        return None;
    }
    let description = rest[end_bracket + 1..].trim().to_string();
    if description.is_empty() {
        return None;
    }
    Some(TaskClaim {
        message_id: msg.message_id,
        domain_id: msg.domain_id,
        domain_sequence: msg.domain_sequence,
        author_account_id: msg.author_account_id,
        handle,
        description,
    })
}

/// Parse a [DONE handle] message. Returns (handle, description) or None.
fn parse_done_message(body: &str) -> Option<(String, String)> {
    let body = body.trim();
    if !body.starts_with(DONE_PREFIX) {
        return None;
    }
    let rest = &body[DONE_PREFIX.len()..];
    let end_bracket = rest.find(']')?;
    let handle = rest[..end_bracket].trim().to_string();
    let description = rest[end_bracket + 1..].trim().to_string();
    if handle.is_empty() || description.is_empty() {
        return None;
    }
    Some((handle, description))
}

/// Check if a claim has been completed by scanning DONE messages.
fn is_claim_completed(claim: &TaskClaim, done_messages: &[crate::tidepool::TidepoolInboundMessage]) -> bool {
    for done_msg in done_messages {
        if done_msg.domain_id != claim.domain_id {
            continue;
        }
        if let Some((done_handle, done_desc)) = parse_done_message(&done_msg.body) {
            if done_handle.eq_ignore_ascii_case(&claim.handle)
                && done_desc.eq_ignore_ascii_case(&claim.description)
            {
                return true;
            }
        }
    }
    false
}

// ── tidepool_claim_task ─────────────────────────────────────────────────────

pub struct TidepoolClaimTaskTool;

#[async_trait]
impl Tool for TidepoolClaimTaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_claim_task".to_string(),
            description: "Claim a task in a Tidepool domain to coordinate with other agents. \
                Posts a [CLAIM handle] message that signals you are working on this task. \
                Other agents can see your claim and avoid duplicating effort. \
                Use tidepool_list_claims to see active claims before starting work. \
                Use tidepool_complete_task when done."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "required": ["domain_id", "task"],
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "description": "Domain ID to post the claim in."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Brief description of the task you are claiming."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_claim_task", "domain_id")?;
        let task = params
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidToolParameters {
                tool: "tidepool_claim_task".to_string(),
                reason: "missing or invalid 'task' field".to_string(),
            })?;
        if task.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_claim_task".to_string(),
                reason: "'task' cannot be blank".to_string(),
            });
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_claim_task", "domain_id")?;
        let task = params["task"].as_str().unwrap().trim();

        let client = shared_tidepool_client("tidepool_claim_task").await?;
        let handle = client
            .account()
            .map(|a| a.handle)
            .unwrap_or_else(|| "unknown".to_string());

        let body = format!("[CLAIM {handle}] {task}");
        client
            .post_message(domain_id, &body, None)
            .map_err(|e| tool_execution("tidepool_claim_task", e))?;

        Ok(json!({
            "status": "claimed",
            "handle": handle,
            "domain_id": domain_id,
            "task": task,
            "message": body,
        }))
    }
}

// ── tidepool_complete_task ──────────────────────────────────────────────────

pub struct TidepoolCompleteTaskTool;

#[async_trait]
impl Tool for TidepoolCompleteTaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_complete_task".to_string(),
            description: "Mark a previously claimed task as complete. \
                Posts a [DONE handle] message matching the original claim. \
                This removes the task from tidepool_list_claims results. \
                The task description must exactly match the original claim."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "required": ["domain_id", "task"],
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "description": "Domain ID where the task was claimed."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The exact task description from the original claim."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_complete_task", "domain_id")?;
        let task = params
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidToolParameters {
                tool: "tidepool_complete_task".to_string(),
                reason: "missing or invalid 'task' field".to_string(),
            })?;
        if task.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_complete_task".to_string(),
                reason: "'task' cannot be blank".to_string(),
            });
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_complete_task", "domain_id")?;
        let task = params["task"].as_str().unwrap().trim();

        let client = shared_tidepool_client("tidepool_complete_task").await?;
        let handle = client
            .account()
            .map(|a| a.handle)
            .unwrap_or_else(|| "unknown".to_string());

        let body = format!("[DONE {handle}] {task}");
        client
            .post_message(domain_id, &body, None)
            .map_err(|e| tool_execution("tidepool_complete_task", e))?;

        Ok(json!({
            "status": "completed",
            "handle": handle,
            "domain_id": domain_id,
            "task": task,
            "message": body,
        }))
    }
}

// ── tidepool_list_claims ────────────────────────────────────────────────────

pub struct TidepoolListClaimsTool;

#[async_trait]
impl Tool for TidepoolListClaimsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_list_claims".to_string(),
            description: "List active (incomplete) task claims in a Tidepool domain. \
                Scans for [CLAIM handle] messages and filters out those with matching \
                [DONE handle] messages. Shows who is working on what. \
                Use this before claiming a task to avoid duplication."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "description": "Filter to a specific domain. Omit to scan all subscribed domains."
                    },
                    "handle": {
                        "type": "string",
                        "description": "Filter claims by a specific agent handle."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of claims to return. Defaults to 50."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u64(params, "tidepool_list_claims", "domain_id")?;
        optional_u32(params, "tidepool_list_claims", "limit")?;
        if let Some(handle) = params.get("handle") {
            if !handle.is_string() {
                return Err(RuntimeError::InvalidToolParameters {
                    tool: "tidepool_list_claims".to_string(),
                    reason: "'handle' must be a string".to_string(),
                });
            }
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = optional_u64(&params, "tidepool_list_claims", "domain_id")?;
        let handle_filter = params.get("handle").and_then(Value::as_str);
        let limit = optional_u32(&params, "tidepool_list_claims", "limit")?
            .unwrap_or(50) as usize;

        let client = shared_tidepool_client("tidepool_list_claims").await?;

        // Search for all CLAIM messages
        let claim_messages = client.search_messages(CLAIM_PREFIX, domain_id, None, None, 200);
        // Search for all DONE messages
        let done_messages = client.search_messages(DONE_PREFIX, domain_id, None, None, 200);

        // Pre-fetch agent health for staleness detection (by account_id)
        let health_entries = client.agent_health(None, domain_id, 200);
        let health_map: std::collections::HashMap<u64, &crate::tidepool::AgentHealthEntry> =
            health_entries.iter().map(|h| (h.account_id, h)).collect();

        let now_micros = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0);

        // Parse claims and filter completed ones
        let mut active_claims: Vec<Value> = Vec::new();
        for msg in &claim_messages {
            if let Some(claim) = parse_claim_message(msg) {
                // Apply handle filter
                if let Some(hf) = handle_filter {
                    if !claim.handle.eq_ignore_ascii_case(hf) {
                        continue;
                    }
                }
                // Skip completed claims
                if is_claim_completed(&claim, &done_messages) {
                    continue;
                }

                // Compute claim age in seconds from message timestamp
                let claim_age_secs =
                    ((now_micros - msg.created_at_micros) as f64 / 1_000_000.0).max(0.0);

                // Detect staleness via agent health (matched by author_account_id)
                let (agent_health_status, agent_seconds_since_active, is_stale) =
                    if let Some(health) = health_map.get(&msg.author_account_id) {
                        let stale = matches!(
                            health.health_status.as_str(),
                            "stale" | "silent" | "unknown"
                        );
                        (
                            Some(health.health_status.clone()),
                            health.seconds_since_last_message,
                            stale,
                        )
                    } else {
                        // No health data — consider stale if claim is > 30min old
                        (None, None, claim_age_secs > 1800.0)
                    };

                active_claims.push(json!({
                    "domain_id": claim.domain_id,
                    "handle": claim.handle,
                    "task": claim.description,
                    "message_id": claim.message_id,
                    "domain_sequence": claim.domain_sequence,
                    "age_seconds": claim_age_secs,
                    "agent_health_status": agent_health_status,
                    "agent_seconds_since_active": agent_seconds_since_active,
                    "is_stale": is_stale,
                }));
            }
            if active_claims.len() >= limit {
                break;
            }
        }

        let stale_count = active_claims.iter().filter(|c| c["is_stale"].as_bool().unwrap_or(false)).count();

        Ok(json!({
            "active_claims": active_claims,
            "total": active_claims.len(),
            "stale_count": stale_count,
        }))
    }
}

// ── tidepool_handoff_task ───────────────────────────────────────────────────

pub struct TidepoolHandoffTaskTool;

#[async_trait]
impl Tool for TidepoolHandoffTaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_handoff_task".to_string(),
            description: "Take over a stale task claim from another agent. \
                Posts a [HANDOFF from→to] message for audit trail, then claims the task \
                under your own handle. Use tidepool_list_claims with is_stale=true first \
                to find claims that can be handed off. Only take over claims where the \
                claiming agent is clearly stale or silent."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "required": ["domain_id", "from_handle", "task"],
                "properties": {
                    "domain_id": {
                        "type": "integer",
                        "description": "Domain ID where the task is claimed."
                    },
                    "from_handle": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Handle of the agent whose stale claim you are taking over."
                    },
                    "task": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The exact task description from the original claim."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_u64(params, "tidepool_handoff_task", "domain_id")?;
        let from = params
            .get("from_handle")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidToolParameters {
                tool: "tidepool_handoff_task".to_string(),
                reason: "missing or invalid 'from_handle' field".to_string(),
            })?;
        if from.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_handoff_task".to_string(),
                reason: "'from_handle' cannot be blank".to_string(),
            });
        }
        let task = params
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidToolParameters {
                tool: "tidepool_handoff_task".to_string(),
                reason: "missing or invalid 'task' field".to_string(),
            })?;
        if task.trim().is_empty() {
            return Err(RuntimeError::InvalidToolParameters {
                tool: "tidepool_handoff_task".to_string(),
                reason: "'task' cannot be blank".to_string(),
            });
        }
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let domain_id = require_u64(&params, "tidepool_handoff_task", "domain_id")?;
        let from_handle = params["from_handle"].as_str().unwrap().trim();
        let task = params["task"].as_str().unwrap().trim();

        let client = shared_tidepool_client("tidepool_handoff_task").await?;
        let to_handle = client
            .account()
            .map(|a| a.handle)
            .unwrap_or_else(|| "unknown".to_string());

        // Post handoff audit message
        let handoff_body = format!("[HANDOFF {from_handle}→{to_handle}] {task}");
        client
            .post_message(domain_id, &handoff_body, None)
            .map_err(|e| tool_execution("tidepool_handoff_task", e))?;

        // Post new claim under our handle
        let claim_body = format!("[CLAIM {to_handle}] {task}");
        client
            .post_message(domain_id, &claim_body, None)
            .map_err(|e| tool_execution("tidepool_handoff_task", e))?;

        Ok(json!({
            "status": "handed_off",
            "from_handle": from_handle,
            "to_handle": to_handle,
            "domain_id": domain_id,
            "task": task,
            "handoff_message": handoff_body,
            "claim_message": claim_body,
        }))
    }
}

// ── tidepool_my_dashboard ──────────────────────────────────────────────────
//
// Single-call personal coordination dashboard. Returns an agent's own claims,
// mentions directed at them, health status, and recent messages in one call.
// Replaces the need to call tidepool_list_claims, tidepool_find_mentions,
// tidepool_agent_health, and tidepool_my_account separately.

pub struct TidepoolMyDashboardTool;

#[async_trait]
impl Tool for TidepoolMyDashboardTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "tidepool_my_dashboard".to_string(),
            description: "Get your personal coordination dashboard in a single call. \
                Returns your active task claims, mentions directed at you, your health status, \
                and recent messages across all subscribed domains. \
                Use this at the start of a turn to quickly understand what needs your attention. \
                Replaces calling tidepool_my_account, tidepool_list_claims(handle=you), \
                tidepool_find_mentions(handle=you), and tidepool_agent_health(account_id=you) separately."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "recent_message_limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Number of recent messages to include per domain. Defaults to 3."
                    },
                    "domain_id": {
                        "type": "integer",
                        "description": "Filter to a specific domain. Omit for all subscribed domains."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        optional_u32(params, "tidepool_my_dashboard", "recent_message_limit")?;
        optional_u64(params, "tidepool_my_dashboard", "domain_id")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let per_domain_limit = optional_u32(&params, "tidepool_my_dashboard", "recent_message_limit")?
            .unwrap_or(3) as usize;
        let domain_filter = optional_u64(&params, "tidepool_my_dashboard", "domain_id")?;

        let client = shared_tidepool_client("tidepool_my_dashboard").await?;

        // 1. Get own identity
        let account = client.account();
        let (my_account_id, my_handle) = match &account {
            Some(a) => (a.account_id, a.handle.clone()),
            None => {
                return Ok(json!({
                    "error": "No account identity available. Tidepool client may not be connected."
                }));
            }
        };

        // 2. Get active claims for this handle
        let claim_messages = client.search_messages(CLAIM_PREFIX, domain_filter, None, None, 200);
        let done_messages = client.search_messages(DONE_PREFIX, domain_filter, None, None, 200);

        let mut my_active_claims: Vec<Value> = Vec::new();
        for msg in &claim_messages {
            if let Some(claim) = parse_claim_message(msg) {
                if !claim.handle.eq_ignore_ascii_case(&my_handle) {
                    continue;
                }
                if is_claim_completed(&claim, &done_messages) {
                    continue;
                }
                let claim_age_secs = {
                    let now_micros = std::time::SystemTime::now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_micros() as i64)
                        .unwrap_or(0);
                    ((now_micros - msg.created_at_micros) as f64 / 1_000_000.0).max(0.0)
                };
                my_active_claims.push(json!({
                    "domain_id": claim.domain_id,
                    "task": claim.description,
                    "message_id": claim.message_id,
                    "age_seconds": claim_age_secs,
                }));
            }
        }

        // 3. Get mentions directed at this agent
        let my_mentions = client.find_mentions(&my_handle, domain_filter, 20);
        let mention_entries: Vec<Value> = my_mentions
            .iter()
            .map(|m| {
                json!({
                    "message_id": m.message_id,
                    "domain_id": m.domain_id,
                    "author_account_id": m.author_account_id,
                    "body_preview": m.body.chars().take(120).collect::<String>(),
                })
            })
            .collect();

        // 4. Get own health
        let health_entries = client.agent_health(Some(my_account_id), domain_filter, 200);
        let my_health = health_entries
            .iter()
            .find(|h| h.account_id == my_account_id)
            .map(|h| {
                json!({
                    "health_status": h.health_status,
                    "seconds_since_last_message": h.seconds_since_last_message,
                    "message_count": h.message_count,
                    "active_domain_ids": h.active_domain_ids,
                })
            })
            .unwrap_or(json!({
                "health_status": "unknown",
                "seconds_since_last_message": null,
                "message_count": 0,
                "active_domain_ids": [],
            }));

        // 5. Get recent messages across domains
        let all_messages = client.read_messages_filtered(domain_filter, None, 500);
        let mut recent_by_domain: std::collections::HashMap<u64, Vec<Value>> =
            std::collections::HashMap::new();
        let subscriptions = client.subscriptions();
        for sub in &subscriptions {
            if let Some(df) = domain_filter {
                if sub.domain_id != df {
                    continue;
                }
            }
            let domain_msgs: Vec<Value> = all_messages
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
            if !domain_msgs.is_empty() {
                recent_by_domain.insert(sub.domain_id, domain_msgs);
            }
        }

        // 6. Get all active claims for workload context (who else is working on what)
        let all_active_claims: Vec<Value> = claim_messages
            .iter()
            .filter_map(|msg| {
                parse_claim_message(msg).and_then(|claim| {
                    if is_claim_completed(&claim, &done_messages) {
                        return None;
                    }
                    Some(json!({
                        "handle": claim.handle,
                        "task": claim.description,
                        "domain_id": claim.domain_id,
                    }))
                })
            })
            .collect();

        // 7. Detect potential attention items
        let mut attention_items: Vec<Value> = Vec::new();

        // Mentions from other agents (not self)
        for mention in &my_mentions {
            if mention.author_account_id != my_account_id {
                attention_items.push(json!({
                    "type": "mention",
                    "message_id": mention.message_id,
                    "domain_id": mention.domain_id,
                    "from_account_id": mention.author_account_id,
                    "preview": mention.body.chars().take(80).collect::<String>(),
                }));
            }
        }

        // Stale claims by other agents that match task keywords in my claims
        // (potential duplicate/overlapping work)

        Ok(json!({
            "identity": {
                "account_id": my_account_id,
                "handle": my_handle,
            },
            "health": my_health,
            "my_active_claims": my_active_claims,
            "my_claim_count": my_active_claims.len(),
            "mentions_for_me": mention_entries,
            "mention_count": mention_entries.len(),
            "attention_items": attention_items,
            "attention_count": attention_items.len(),
            "all_active_claims": all_active_claims,
            "total_active_claims": all_active_claims.len(),
            "recent_messages_by_domain": recent_by_domain,
            "domains_with_activity": recent_by_domain.len(),
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
        Some(Value::Null) => Ok(None),
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
        Some(Value::Null) => Ok(None),
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
        Some(Value::Null) => Ok(None),
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

    #[test]
    fn lookup_account_validation_requires_at_least_one_param() {
        let tool = TidepoolLookupAccountTool;
        let error = tool.validate(&json!({})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn lookup_account_validation_accepts_handle_only() {
        let tool = TidepoolLookupAccountTool;
        tool.validate(&json!({"handle": "buzz"})).unwrap();
    }

    #[test]
    fn lookup_account_validation_accepts_account_id_only() {
        let tool = TidepoolLookupAccountTool;
        tool.validate(&json!({"account_id": 1})).unwrap();
    }

    #[test]
    fn lookup_account_validation_accepts_both_params() {
        let tool = TidepoolLookupAccountTool;
        tool.validate(&json!({"handle": "buzz", "account_id": 1})).unwrap();
    }

    #[test]
    fn lookup_account_validation_rejects_blank_handle() {
        let tool = TidepoolLookupAccountTool;
        let error = tool.validate(&json!({"handle": "   "})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn lookup_account_validation_rejects_invalid_account_id() {
        let tool = TidepoolLookupAccountTool;
        let error = tool.validate(&json!({"account_id": "abc"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_lookup_account_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_lookup_account".to_string()));
    }

    #[test]
    fn message_agent_validation_requires_handle() {
        let tool = TidepoolMessageAgentTool;
        let error = tool.validate(&json!({"body": "hello"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn message_agent_validation_requires_body() {
        let tool = TidepoolMessageAgentTool;
        let error = tool.validate(&json!({"handle": "buzz"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn message_agent_validation_rejects_blank_handle() {
        let tool = TidepoolMessageAgentTool;
        let error = tool.validate(&json!({"handle": "   ", "body": "hello"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn message_agent_validation_rejects_blank_body() {
        let tool = TidepoolMessageAgentTool;
        let error = tool.validate(&json!({"handle": "buzz", "body": "   "})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn message_agent_validation_accepts_valid_params() {
        let tool = TidepoolMessageAgentTool;
        tool.validate(&json!({"handle": "buzz", "body": "hello from horus"})).unwrap();
    }

    #[test]
    fn default_registry_includes_message_agent_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_message_agent".to_string()));
    }

    // ── Task Claiming Protocol Tests ─────────────────────────────────────

    #[test]
    fn parse_claim_message_extracts_handle_and_task() {
        let msg = crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 42,
            domain_sequence: 10,
            author_account_id: 1,
            body: "[CLAIM horus] Fix the cursor seeding bug".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        };
        let claim = parse_claim_message(&msg).unwrap();
        assert_eq!(claim.handle, "horus");
        assert_eq!(claim.description, "Fix the cursor seeding bug");
        assert_eq!(claim.message_id, 42);
    }

    #[test]
    fn parse_claim_message_rejects_non_claim() {
        let msg = crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            body: "Just a normal message".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        };
        assert!(parse_claim_message(&msg).is_none());
    }

    #[test]
    fn parse_claim_message_rejects_empty_task() {
        let msg = crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            body: "[CLAIM horus]   ".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        };
        assert!(parse_claim_message(&msg).is_none());
    }

    #[test]
    fn parse_claim_message_rejects_empty_handle() {
        let msg = crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            body: "[CLAIM ] some task".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        };
        assert!(parse_claim_message(&msg).is_none());
    }

    #[test]
    fn parse_done_message_extracts_handle_and_task() {
        let result = parse_done_message("[DONE horus] Fix the cursor seeding bug");
        assert_eq!(result.unwrap(), ("horus".to_string(), "Fix the cursor seeding bug".to_string()));
    }

    #[test]
    fn parse_done_message_rejects_non_done() {
        assert!(parse_done_message("Not a done message").is_none());
    }

    #[test]
    fn is_claim_completed_matches_done_message() {
        let claim = TaskClaim {
            message_id: 1,
            domain_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            handle: "horus".into(),
            description: "Fix the cursor bug".into(),
        };
        let done_messages = vec![crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 2,
            domain_sequence: 2,
            author_account_id: 1,
            body: "[DONE horus] Fix the cursor bug".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        }];
        assert!(is_claim_completed(&claim, &done_messages));
    }

    #[test]
    fn is_claim_completed_ignores_different_domain() {
        let claim = TaskClaim {
            message_id: 1,
            domain_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            handle: "horus".into(),
            description: "Fix the cursor bug".into(),
        };
        let done_messages = vec![crate::tidepool::TidepoolInboundMessage {
            domain_id: 2, // different domain
            domain_title: "other".into(),
            domain_slug: "other".into(),
            message_id: 2,
            domain_sequence: 2,
            author_account_id: 1,
            body: "[DONE horus] Fix the cursor bug".into(),
            reply_to_message_id: None,
                created_at_micros: 0,
        }];
        assert!(!is_claim_completed(&claim, &done_messages));
    }

    #[test]
    fn is_claim_completed_ignores_different_handle() {
        let claim = TaskClaim {
            message_id: 1,
            domain_id: 1,
            domain_sequence: 1,
            author_account_id: 1,
            handle: "horus".into(),
            description: "Fix the cursor bug".into(),
        };
        let done_messages = vec![crate::tidepool::TidepoolInboundMessage {
            domain_id: 1,
            domain_title: "test".into(),
            domain_slug: "test".into(),
            message_id: 2,
            domain_sequence: 2,
            author_account_id: 2,
            body: "[DONE buzz] Fix the cursor bug".into(), // different handle
            reply_to_message_id: None,
                created_at_micros: 0,
        }];
        assert!(!is_claim_completed(&claim, &done_messages));
    }

    #[test]
    fn claim_task_validation_requires_domain_id() {
        let tool = TidepoolClaimTaskTool;
        let error = tool.validate(&json!({"task": "test"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn claim_task_validation_requires_task() {
        let tool = TidepoolClaimTaskTool;
        let error = tool.validate(&json!({"domain_id": 1})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn claim_task_validation_rejects_blank_task() {
        let tool = TidepoolClaimTaskTool;
        let error = tool.validate(&json!({"domain_id": 1, "task": "   "})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn claim_task_validation_accepts_valid_params() {
        let tool = TidepoolClaimTaskTool;
        tool.validate(&json!({"domain_id": 1, "task": "Fix the cursor bug"})).unwrap();
    }

    #[test]
    fn complete_task_validation_requires_domain_id() {
        let tool = TidepoolCompleteTaskTool;
        let error = tool.validate(&json!({"task": "test"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn complete_task_validation_requires_task() {
        let tool = TidepoolCompleteTaskTool;
        let error = tool.validate(&json!({"domain_id": 1})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn complete_task_validation_accepts_valid_params() {
        let tool = TidepoolCompleteTaskTool;
        tool.validate(&json!({"domain_id": 1, "task": "Fix the cursor bug"})).unwrap();
    }

    #[test]
    fn list_claims_validation_accepts_empty_params() {
        let tool = TidepoolListClaimsTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn list_claims_validation_accepts_all_filters() {
        let tool = TidepoolListClaimsTool;
        tool.validate(&json!({"domain_id": 1, "handle": "horus", "limit": 10})).unwrap();
    }

    #[test]
    fn list_claims_validation_rejects_invalid_domain_id() {
        let tool = TidepoolListClaimsTool;
        let error = tool.validate(&json!({"domain_id": "abc"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_claim_tools() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_claim_task".to_string()));
        assert!(names.contains(&"tidepool_complete_task".to_string()));
        assert!(names.contains(&"tidepool_list_claims".to_string()));
    }

    #[test]
    fn join_domain_validation_requires_domain_id() {
        let tool = TidepoolJoinDomainTool;
        let error = tool.validate(&json!({})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn join_domain_validation_rejects_non_numeric() {
        let tool = TidepoolJoinDomainTool;
        let error = tool.validate(&json!({"domain_id": "abc"})).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn join_domain_validation_accepts_valid_params() {
        let tool = TidepoolJoinDomainTool;
        tool.validate(&json!({"domain_id": 1})).unwrap();
        tool.validate(&json!({"domain_id": 42})).unwrap();
    }

    #[test]
    fn default_registry_includes_join_domain_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_join_domain".to_string()));
    }

    #[test]
    fn handoff_task_validation_rejects_missing_params() {
        let tool = TidepoolHandoffTaskTool;
        assert!(tool.validate(&json!({})).is_err());
        assert!(tool.validate(&json!({"domain_id": 1})).is_err());
        assert!(tool.validate(&json!({"domain_id": 1, "from_handle": "buzz"})).is_err());
    }

    #[test]
    fn handoff_task_validation_rejects_empty_handle() {
        let tool = TidepoolHandoffTaskTool;
        let error = tool.validate(&json!({
            "domain_id": 1,
            "from_handle": "",
            "task": "do something"
        })).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn handoff_task_validation_rejects_empty_task() {
        let tool = TidepoolHandoffTaskTool;
        let error = tool.validate(&json!({
            "domain_id": 1,
            "from_handle": "buzz",
            "task": ""
        })).unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn handoff_task_validation_accepts_valid_params() {
        let tool = TidepoolHandoffTaskTool;
        tool.validate(&json!({
            "domain_id": 1,
            "from_handle": "buzz",
            "task": "Fix the rendering bug"
        })).unwrap();
    }

    #[test]
    fn default_registry_includes_handoff_task_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_handoff_task".to_string()));
    }

    #[test]
    fn my_dashboard_validation_accepts_empty_params() {
        let tool = TidepoolMyDashboardTool;
        tool.validate(&json!({})).unwrap();
    }

    #[test]
    fn my_dashboard_validation_accepts_recent_message_limit() {
        let tool = TidepoolMyDashboardTool;
        tool.validate(&json!({"recent_message_limit": 5})).unwrap();
        tool.validate(&json!({"recent_message_limit": 1})).unwrap();
        tool.validate(&json!({"recent_message_limit": 20})).unwrap();
    }

    #[test]
    fn my_dashboard_validation_accepts_domain_filter() {
        let tool = TidepoolMyDashboardTool;
        tool.validate(&json!({"domain_id": 1})).unwrap();
    }

    #[test]
    fn my_dashboard_validation_accepts_null_domain_filter() {
        let tool = TidepoolMyDashboardTool;
        tool.validate(&json!({"domain_id": null})).unwrap();
    }

    #[test]
    fn my_dashboard_validation_accepts_all_params() {
        let tool = TidepoolMyDashboardTool;
        tool.validate(&json!({
            "recent_message_limit": 10,
            "domain_id": 42
        }))
        .unwrap();
    }

    #[test]
    fn my_dashboard_validation_rejects_invalid_limit() {
        let tool = TidepoolMyDashboardTool;
        let error = tool
            .validate(&json!({"recent_message_limit": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn my_dashboard_validation_rejects_invalid_domain() {
        let tool = TidepoolMyDashboardTool;
        let error = tool
            .validate(&json!({"domain_id": "abc"}))
            .unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidToolParameters { .. }));
    }

    #[test]
    fn default_registry_includes_my_dashboard_tool() {
        let registry = ToolRegistry::with_defaults();
        let definitions = registry.definitions();
        let names = definitions.into_iter().map(|item| item.name).collect::<Vec<_>>();
        assert!(names.contains(&"tidepool_my_dashboard".to_string()));
    }
}
