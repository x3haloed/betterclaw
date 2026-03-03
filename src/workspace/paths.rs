//! Well-known workspace file paths and conventions.
//!
//! These are conventional paths that have special meaning in the workspace.
//! Agents can create arbitrary paths beyond these.

/// Long-term curated memory (legacy; slated to be replaced by ledger recall).
pub const MEMORY: &str = "MEMORY.md";
/// Agent identity (name, nature, vibe).
pub const IDENTITY: &str = "IDENTITY.md";
/// Core values and principles.
pub const SOUL: &str = "SOUL.md";
/// Behavior instructions.
pub const AGENTS: &str = "AGENTS.md";
/// User context (name, preferences).
pub const USER: &str = "USER.md";
/// Periodic checklist for heartbeat.
pub const HEARTBEAT: &str = "HEARTBEAT.md";
/// Root runbook/readme.
pub const README: &str = "README.md";

/// Daily logs directory (legacy convention).
pub const DAILY_DIR: &str = "daily/";
/// Context directory (legacy convention).
pub const CONTEXT_DIR: &str = "context/";

