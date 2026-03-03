use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env};
use crate::error::ConfigError;

/// Background compressor loop configuration.
///
/// This runs in-process (same runtime as the named agent) and periodically
/// performs a bounded "micro distill" pass over the ledger to produce a
/// `wake_pack.v0` artifact plus optional derived actions.
#[derive(Debug, Clone)]
pub struct CompressorLoopConfig {
    /// Whether the compressor loop is enabled.
    pub enabled: bool,
    /// Interval between distill passes (seconds).
    pub interval_secs: u64,
    /// Initial delay after startup (seconds) before the first pass.
    pub startup_delay_secs: u64,
    /// User id namespace for the ledger.
    pub user_id: String,
    /// Local window size (most recent N events).
    pub window_events: i64,
    /// Anchor invariants to include (kind prefix `invariant.`).
    pub anchor_invariants: i64,
    /// Drift/contradiction candidates to include (kind prefix `drift.`).
    pub drift_candidates: i64,
    /// Max output tokens for the compressor tool-call response.
    pub max_tokens: u32,
    /// Whether to commit derived artifacts to the ledger.
    ///
    /// Default ON: the whole point is to create `wake_pack.v0` for the named agent to read.
    pub commit: bool,
}

impl Default for CompressorLoopConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 300,
            startup_delay_secs: 5,
            user_id: "default".to_string(),
            window_events: 200,
            anchor_invariants: 30,
            drift_candidates: 30,
            max_tokens: 2048,
            commit: true,
        }
    }
}

impl CompressorLoopConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_bool_env("COMPRESSOR_LOOP_ENABLED", defaults.enabled)?,
            interval_secs: parse_optional_env("COMPRESSOR_LOOP_INTERVAL_SECS", defaults.interval_secs)?,
            startup_delay_secs: parse_optional_env(
                "COMPRESSOR_LOOP_STARTUP_DELAY_SECS",
                defaults.startup_delay_secs,
            )?,
            user_id: optional_env("COMPRESSOR_LOOP_USER_ID")?.unwrap_or(defaults.user_id),
            window_events: parse_optional_env("COMPRESSOR_LOOP_WINDOW_EVENTS", defaults.window_events)?,
            anchor_invariants: parse_optional_env(
                "COMPRESSOR_LOOP_ANCHOR_INVARIANTS",
                defaults.anchor_invariants,
            )?,
            drift_candidates: parse_optional_env(
                "COMPRESSOR_LOOP_DRIFT_CANDIDATES",
                defaults.drift_candidates,
            )?,
            max_tokens: parse_optional_env("COMPRESSOR_LOOP_MAX_TOKENS", defaults.max_tokens)?,
            commit: parse_bool_env("COMPRESSOR_LOOP_COMMIT", defaults.commit)?,
        })
    }
}

