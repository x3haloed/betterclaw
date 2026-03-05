use std::collections::HashMap;
use std::path::PathBuf;

use secrecy::SecretString;

use crate::bootstrap::betterclaw_base_dir;
use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env};
use crate::error::ConfigError;
use crate::settings::Settings;

/// Channel configurations.
#[derive(Debug, Clone)]
pub struct ChannelsConfig {
    pub cli: CliConfig,
    pub http: Option<HttpConfig>,
    pub gateway: Option<GatewayConfig>,
    pub signal: Option<SignalConfig>,
    pub discord: Option<DiscordConfig>,
    /// Directory containing WASM channel modules (default: ~/.betterclaw/channels/).
    pub wasm_channels_dir: std::path::PathBuf,
    /// Whether WASM channels are enabled.
    pub wasm_channels_enabled: bool,
    /// Per-channel owner user IDs. When set, the channel only responds to this user.
    /// Key: channel name (e.g., "telegram"), Value: owner user ID.
    pub wasm_channel_owner_ids: HashMap<String, i64>,
}

#[derive(Debug, Clone)]
pub struct CliConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub host: String,
    pub port: u16,
    pub webhook_secret: Option<SecretString>,
    pub user_id: String,
}

/// Web gateway configuration.
#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
    /// Bearer token for authentication. Random hex generated at startup if unset.
    pub auth_token: Option<String>,
    pub user_id: String,
}

/// Signal channel configuration (signal-cli daemon HTTP/JSON-RPC).
#[derive(Debug, Clone)]
pub struct SignalConfig {
    /// Base URL of the signal-cli daemon HTTP endpoint (e.g. `http://127.0.0.1:8080`).
    pub http_url: String,
    /// Signal account identifier (E.164 phone number, e.g. `+1234567890`).
    pub account: String,
    /// Users allowed to interact with the bot in DMs.
    ///
    /// Each entry is one of:
    /// - `*` — allow everyone
    /// - E.164 phone number (e.g. `+1234567890`)
    /// - bare UUID (e.g. `a1b2c3d4-e5f6-7890-abcd-ef1234567890`)
    /// - `uuid:<id>` prefix form (e.g. `uuid:a1b2c3d4-e5f6-7890-abcd-ef1234567890`)
    ///
    /// An empty list denies all senders (secure by default).
    pub allow_from: Vec<String>,
    /// Groups allowed to interact with the bot.
    ///
    /// - Empty list — deny all group messages (DMs only, secure by default).
    /// - `*` — allow all groups.
    /// - Specific group IDs — allow only those groups.
    pub allow_from_groups: Vec<String>,
    /// DM policy: "open", "allowlist", or "pairing". Default: "pairing".
    ///
    /// - "open" — allow all DM senders (ignores allow_from for DMs)
    /// - "allowlist" — only allow senders in allow_from list
    /// - "pairing" — allowlist + send pairing reply to unknown users
    pub dm_policy: String,
    /// Group policy: "allowlist", "open", or "disabled". Default: "allowlist".
    ///
    /// - "disabled" — deny all group messages
    /// - "allowlist" — check allow_from_groups and group_allow_from
    /// - "open" — accept all group messages (respects allow_from_groups for group ID)
    pub group_policy: String,
    /// Allow list for group message senders. If empty, inherits from allow_from.
    pub group_allow_from: Vec<String>,
    /// Skip messages that contain only attachments (no text).
    pub ignore_attachments: bool,
    /// Skip story messages.
    pub ignore_stories: bool,
}

/// Discord channel configuration (native gateway websocket mode).
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// Discord bot token (required to enable).
    pub bot_token: SecretString,
    /// BetterClaw user_id to attribute Discord conversations to.
    ///
    /// Default: "default".
    ///
    /// NOTE: This is distinct from the Discord sender ID. The sender ID is still
    /// recorded in message metadata (discord_sender_id) and enforced by allowlists.
    pub user_id: String,
    /// Primary Discord user ID for the "default" BetterClaw user, used only for
    /// labeling incoming guild messages.
    ///
    /// If set, guild messages authored by this user will be prefixed with a
    /// `YOU` marker (shared workspace mode). DMs are never prefixed.
    pub primary_discord_user_id: Option<String>,
    /// Optional guild to restrict group messages to (DMs still allowed).
    pub guild_id: Option<String>,
    /// Users allowed to interact with the bot via Discord DMs.
    ///
    /// - Empty list denies everyone.
    /// - `"*"` allows everyone.
    pub dm_allowed_users: Vec<String>,
    /// Users allowed to interact with the bot in guild channels.
    ///
    /// - Empty list denies everyone in guild channels.
    /// - `"*"` allows everyone in the configured guild(s).
    pub guild_allowed_users: Vec<String>,
    /// Whether to process messages from other bots.
    pub listen_to_bots: bool,
    /// If true, in guild channels only respond when mentioned (unless sender is in group_reply_allowed_sender_ids).
    pub mention_only: bool,
    /// Sender IDs that can trigger the bot in guild channels without mentioning it.
    pub group_reply_allowed_sender_ids: Vec<String>,
    /// Optional list of emojis to react with on accepted incoming messages.
    ///
    /// Empty list disables ACK reactions.
    pub ack_reactions: Vec<String>,
    /// If non-empty, only apply ACK reactions to these chat types.
    ///
    /// Supported values: "dm", "group".
    /// Empty list means "all".
    pub ack_reaction_chat_types: Vec<String>,
}

impl ChannelsConfig {
    pub(crate) fn resolve(settings: &Settings) -> Result<Self, ConfigError> {
        let http = if optional_env("HTTP_PORT")?.is_some() || optional_env("HTTP_HOST")?.is_some() {
            Some(HttpConfig {
                host: optional_env("HTTP_HOST")?.unwrap_or_else(|| "0.0.0.0".to_string()),
                port: parse_optional_env("HTTP_PORT", 8080)?,
                webhook_secret: optional_env("HTTP_WEBHOOK_SECRET")?.map(SecretString::from),
                user_id: optional_env("HTTP_USER_ID")?.unwrap_or_else(|| "http".to_string()),
            })
        } else {
            None
        };

        let gateway_enabled = parse_bool_env("GATEWAY_ENABLED", true)?;
        let gateway = if gateway_enabled {
            Some(GatewayConfig {
                host: optional_env("GATEWAY_HOST")?.unwrap_or_else(|| "127.0.0.1".to_string()),
                port: parse_optional_env("GATEWAY_PORT", 3000)?,
                auth_token: optional_env("GATEWAY_AUTH_TOKEN")?,
                user_id: optional_env("GATEWAY_USER_ID")?.unwrap_or_else(|| "default".to_string()),
            })
        } else {
            None
        };

        let signal = if let Some(http_url) = optional_env("SIGNAL_HTTP_URL")? {
            let account = optional_env("SIGNAL_ACCOUNT")?.ok_or(ConfigError::InvalidValue {
                key: "SIGNAL_ACCOUNT".to_string(),
                message: "SIGNAL_ACCOUNT is required when SIGNAL_HTTP_URL is set".to_string(),
            })?;
            let allow_from = match std::env::var_os("SIGNAL_ALLOW_FROM") {
                None => vec![account.clone()],
                Some(val) => {
                    let s = val.to_string_lossy();
                    s.split(',')
                        .map(|e| e.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
            };
            let dm_policy =
                optional_env("SIGNAL_DM_POLICY")?.unwrap_or_else(|| "pairing".to_string());
            let group_policy =
                optional_env("SIGNAL_GROUP_POLICY")?.unwrap_or_else(|| "allowlist".to_string());
            Some(SignalConfig {
                http_url,
                account,
                allow_from,
                allow_from_groups: optional_env("SIGNAL_ALLOW_FROM_GROUPS")?
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                dm_policy,
                group_policy,
                group_allow_from: optional_env("SIGNAL_GROUP_ALLOW_FROM")?
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                ignore_attachments: optional_env("SIGNAL_IGNORE_ATTACHMENTS")?
                    .map(|s| s.to_lowercase() == "true" || s == "1")
                    .unwrap_or(false),
                ignore_stories: optional_env("SIGNAL_IGNORE_STORIES")?
                    .map(|s| s.to_lowercase() == "true" || s == "1")
                    .unwrap_or(true),
            })
        } else {
            None
        };

        let discord = if let Some(token) = optional_env("DISCORD_BOT_TOKEN")? {
            let user_id = optional_env("DISCORD_USER_ID")?.unwrap_or_else(|| "default".to_string());
            let primary_discord_user_id = optional_env("DISCORD_PRIMARY_DISCORD_USER_ID")?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            // Legacy allowlist (applied to both DM + guild before the split).
            // Kept as fallback so existing setups keep working.
            let legacy_allowed_users = optional_env("DISCORD_ALLOWED_USERS")?
                .map(|s| {
                    s.split(',')
                        .map(|e| e.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                // Usability default: if you set a token but not an allowlist, allow all.
                // You can lock it down by setting DISCORD_ALLOWED_USERS to an empty string
                // (explicit deny) or to a specific list of IDs.
                .unwrap_or_else(|| vec!["*".to_string()]);

            let dm_allowed_users = optional_env("DISCORD_DM_ALLOWED_USERS")?
                .map(|s| {
                    s.split(',')
                        .map(|e| e.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| legacy_allowed_users.clone());

            let guild_allowed_users = optional_env("DISCORD_GUILD_ALLOWED_USERS")?
                .map(|s| {
                    s.split(',')
                        .map(|e| e.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                // For shared-workspace servers, default to allowing guild users to initiate
                // interactions unless explicitly locked down.
                .unwrap_or_else(|| vec!["*".to_string()]);

            let group_reply_allowed_sender_ids =
                optional_env("DISCORD_GROUP_REPLY_ALLOWED_SENDERS")?
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

            Some(DiscordConfig {
                bot_token: SecretString::from(token),
                user_id,
                primary_discord_user_id,
                guild_id: optional_env("DISCORD_GUILD_ID")?,
                dm_allowed_users,
                guild_allowed_users,
                listen_to_bots: parse_bool_env("DISCORD_LISTEN_TO_BOTS", false)?,
                mention_only: parse_bool_env("DISCORD_MENTION_ONLY", true)?,
                group_reply_allowed_sender_ids,
                ack_reactions: optional_env("DISCORD_ACK_REACTIONS")?
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                ack_reaction_chat_types: optional_env("DISCORD_ACK_REACTION_CHAT_TYPES")?
                    .map(|s| {
                        s.split(',')
                            .map(|e| e.trim().to_ascii_lowercase())
                            .filter(|s| s == "dm" || s == "group")
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        } else {
            None
        };

        let cli_enabled = optional_env("CLI_ENABLED")?
            .map(|s| s.to_lowercase() != "false" && s != "0")
            .unwrap_or(true);

        Ok(Self {
            cli: CliConfig {
                enabled: cli_enabled,
            },
            http,
            gateway,
            signal,
            discord,
            wasm_channels_dir: optional_env("WASM_CHANNELS_DIR")?
                .map(PathBuf::from)
                .unwrap_or_else(default_channels_dir),
            wasm_channels_enabled: parse_bool_env("WASM_CHANNELS_ENABLED", true)?,
            wasm_channel_owner_ids: {
                let mut ids = settings.channels.wasm_channel_owner_ids.clone();
                // Backwards compat: TELEGRAM_OWNER_ID env var
                if let Some(id_str) = optional_env("TELEGRAM_OWNER_ID")? {
                    let id: i64 = id_str.parse().map_err(|e: std::num::ParseIntError| {
                        ConfigError::InvalidValue {
                            key: "TELEGRAM_OWNER_ID".to_string(),
                            message: format!("must be an integer: {e}"),
                        }
                    })?;
                    ids.insert("telegram".to_string(), id);
                }
                ids
            },
        })
    }
}

/// Get the default channels directory (~/.betterclaw/channels/).
fn default_channels_dir() -> PathBuf {
    betterclaw_base_dir().join("channels")
}
