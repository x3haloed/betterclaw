use crate::config::helpers::{parse_bool_env, parse_optional_env};
use crate::error::ConfigError;

#[derive(Debug, Clone)]
pub struct ObservationRoutineConfig {
    pub enabled: bool,
    pub user_id: String,
    pub tension_every_messages: u64,
    pub pattern_every_messages: u64,
    pub hypothesis_every_messages: u64,
    pub recent_ledger_events: i64,
    pub active_invariants: i64,
    pub unresolved_observations: i64,
    pub max_tokens: u32,
}

impl Default for ObservationRoutineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            user_id: "default".to_string(),
            tension_every_messages: 6,
            pattern_every_messages: 24,
            hypothesis_every_messages: 72,
            recent_ledger_events: 12,
            active_invariants: 24,
            unresolved_observations: 12,
            max_tokens: 2048,
        }
    }
}

/// Routines configuration.
#[derive(Debug, Clone)]
pub struct RoutineConfig {
    /// Whether the routines system is enabled.
    pub enabled: bool,
    /// How often (seconds) to poll for cron routines that need firing.
    pub cron_check_interval_secs: u64,
    /// Max routines executing concurrently across all users.
    pub max_concurrent_routines: usize,
    /// Default cooldown between fires (seconds).
    pub default_cooldown_secs: u64,
    /// Max output tokens for lightweight routine LLM calls.
    pub max_lightweight_tokens: u32,
    /// Built-in observation loops.
    pub observation: ObservationRoutineConfig,
}

impl Default for RoutineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cron_check_interval_secs: 15,
            max_concurrent_routines: 10,
            default_cooldown_secs: 300,
            max_lightweight_tokens: 4096,
            observation: ObservationRoutineConfig::default(),
        }
    }
}

impl RoutineConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        Ok(Self {
            enabled: parse_bool_env("ROUTINES_ENABLED", true)?,
            cron_check_interval_secs: parse_optional_env("ROUTINES_CRON_INTERVAL", 15)?,
            max_concurrent_routines: parse_optional_env("ROUTINES_MAX_CONCURRENT", 10)?,
            default_cooldown_secs: parse_optional_env("ROUTINES_DEFAULT_COOLDOWN", 300)?,
            max_lightweight_tokens: parse_optional_env("ROUTINES_MAX_TOKENS", 4096)?,
            observation: ObservationRoutineConfig {
                enabled: parse_bool_env("OBSERVATION_ROUTINES_ENABLED", true)?,
                user_id: std::env::var("OBSERVATION_ROUTINES_USER_ID")
                    .unwrap_or_else(|_| "default".to_string()),
                tension_every_messages: parse_optional_env(
                    "OBSERVATION_TENSION_EVERY_MESSAGES",
                    6,
                )?,
                pattern_every_messages: parse_optional_env(
                    "OBSERVATION_PATTERN_EVERY_MESSAGES",
                    24,
                )?,
                hypothesis_every_messages: parse_optional_env(
                    "OBSERVATION_HYPOTHESIS_EVERY_MESSAGES",
                    72,
                )?,
                recent_ledger_events: parse_optional_env("OBSERVATION_RECENT_LEDGER_EVENTS", 12)?,
                active_invariants: parse_optional_env("OBSERVATION_ACTIVE_INVARIANTS", 24)?,
                unresolved_observations: parse_optional_env(
                    "OBSERVATION_UNRESOLVED_OBSERVATIONS",
                    12,
                )?,
                max_tokens: parse_optional_env("OBSERVATION_MAX_TOKENS", 2048)?,
            },
        })
    }
}
