use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env};
use crate::error::ConfigError;

/// Background ledger embedding indexer configuration.
///
/// This loop chunks and embeds new (non-derived) `ledger_events` into
/// `ledger_event_chunks` for semantic and keyword recall.
#[derive(Debug, Clone)]
pub struct LedgerIndexConfig {
    /// Whether the indexer loop is enabled.
    pub enabled: bool,
    /// Interval between sweeps (seconds) when caught up.
    pub interval_secs: u64,
    /// Initial delay after startup (seconds) before first sweep.
    pub startup_delay_secs: u64,
    /// User id namespace for the ledger.
    pub user_id: String,
    /// Max number of ledger events to index per sweep.
    pub batch_events: i64,
    /// Max characters per stored chunk (hard cap before embedding).
    pub max_chunk_chars: usize,
    /// Chunk size in words (approx tokens).
    pub chunk_size_words: usize,
    /// Overlap percentage between adjacent chunks.
    pub overlap_percent: f32,
    /// Minimum trailing chunk size in words.
    pub min_chunk_size_words: usize,
}

impl Default for LedgerIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 10,
            startup_delay_secs: 2,
            user_id: "default".to_string(),
            batch_events: 25,
            max_chunk_chars: 40_000,
            chunk_size_words: 800,
            overlap_percent: 0.15,
            min_chunk_size_words: 50,
        }
    }
}

impl LedgerIndexConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let defaults = Self::default();
        Ok(Self {
            enabled: parse_bool_env("LEDGER_INDEX_ENABLED", defaults.enabled)?,
            interval_secs: parse_optional_env(
                "LEDGER_INDEX_INTERVAL_SECS",
                defaults.interval_secs,
            )?,
            startup_delay_secs: parse_optional_env(
                "LEDGER_INDEX_STARTUP_DELAY_SECS",
                defaults.startup_delay_secs,
            )?,
            user_id: optional_env("LEDGER_INDEX_USER_ID")?.unwrap_or(defaults.user_id),
            batch_events: parse_optional_env("LEDGER_INDEX_BATCH_EVENTS", defaults.batch_events)?,
            max_chunk_chars: parse_optional_env(
                "LEDGER_INDEX_MAX_CHUNK_CHARS",
                defaults.max_chunk_chars,
            )?,
            chunk_size_words: parse_optional_env(
                "LEDGER_INDEX_CHUNK_SIZE_WORDS",
                defaults.chunk_size_words,
            )?,
            overlap_percent: parse_optional_env(
                "LEDGER_INDEX_OVERLAP_PERCENT",
                defaults.overlap_percent,
            )?,
            min_chunk_size_words: parse_optional_env(
                "LEDGER_INDEX_MIN_CHUNK_SIZE_WORDS",
                defaults.min_chunk_size_words,
            )?,
        })
    }
}
