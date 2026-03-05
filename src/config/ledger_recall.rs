use crate::config::helpers::{parse_bool_env, parse_optional_env};
use crate::error::ConfigError;

/// Per-turn ledger recall configuration.
///
/// Recall is injected as a small **system** message immediately after the wake pack.
/// It is a candidate generator, not "truth": the agent must cite `event_id` if used.
#[derive(Debug, Clone)]
pub struct LedgerRecallConfig {
    /// Whether recall injection is enabled.
    pub enabled: bool,
    /// Skip recall injection for group chats/channels to avoid leaking personal context.
    pub skip_group_chats: bool,
    /// Number of semantic (vector) candidates to fetch.
    pub vector_k: i64,
    /// Over-fetch multiplier for the global vector index before user_id filtering.
    pub vector_prefilter_multiplier: i64,
    /// Number of keyword (FTS) candidates to fetch.
    pub fts_k: i64,
    /// Final number of fused hits to inject.
    pub final_k: usize,
    /// Max characters for the injected recall block.
    pub max_injected_chars: usize,
    /// Whether to add a "task state" query derived from recent chat history.
    pub include_task_state_query: bool,
}

impl Default for LedgerRecallConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            skip_group_chats: true,
            vector_k: 24,
            vector_prefilter_multiplier: 10,
            fts_k: 24,
            final_k: 12,
            max_injected_chars: 8_000,
            include_task_state_query: true,
        }
    }
}

impl LedgerRecallConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_bool_env("LEDGER_RECALL_ENABLED", defaults.enabled)?,
            skip_group_chats: parse_bool_env(
                "LEDGER_RECALL_SKIP_GROUP_CHATS",
                defaults.skip_group_chats,
            )?,
            vector_k: parse_optional_env("LEDGER_RECALL_VECTOR_K", defaults.vector_k)?,
            vector_prefilter_multiplier: parse_optional_env(
                "LEDGER_RECALL_VECTOR_PREFILTER_MULTIPLIER",
                defaults.vector_prefilter_multiplier,
            )?,
            fts_k: parse_optional_env("LEDGER_RECALL_FTS_K", defaults.fts_k)?,
            final_k: parse_optional_env("LEDGER_RECALL_FINAL_K", defaults.final_k)?,
            max_injected_chars: parse_optional_env(
                "LEDGER_RECALL_MAX_INJECTED_CHARS",
                defaults.max_injected_chars,
            )?,
            include_task_state_query: parse_bool_env(
                "LEDGER_RECALL_INCLUDE_TASK_STATE_QUERY",
                defaults.include_task_state_query,
            )?,
        })
    }
}
