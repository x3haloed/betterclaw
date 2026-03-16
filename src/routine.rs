use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Kinds of observations the routine engine can produce.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// An unresolved question or incomplete interaction.
    Tension,
    /// A recurring pattern detected across turns (e.g., repeated tool failures).
    Pattern,
    /// A suggested hypothesis or next action based on observed data.
    Hypothesis,
    /// A contradiction between memory artifacts or ledger entries.
    Contradiction,
}

impl ObservationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tension => "tension",
            Self::Pattern => "pattern",
            Self::Hypothesis => "hypothesis",
            Self::Contradiction => "contradiction",
        }
    }
}

impl std::str::FromStr for ObservationKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tension" => Ok(Self::Tension),
            "pattern" => Ok(Self::Pattern),
            "hypothesis" => Ok(Self::Hypothesis),
            "contradiction" => Ok(Self::Contradiction),
            _ => Err(format!("unknown observation kind: {s}")),
        }
    }
}

/// Severity level for an observation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            _ => Err(format!("unknown severity: {s}")),
        }
    }
}

/// A single observation produced by the routine engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub namespace_id: String,
    pub kind: ObservationKind,
    pub severity: Severity,
    pub summary: String,
    pub detail: Option<String>,
    pub citations: Vec<String>,
    pub payload: Value,
    pub resolved: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for creating a new observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewObservation {
    pub namespace_id: String,
    pub kind: ObservationKind,
    pub severity: Severity,
    pub summary: String,
    pub detail: Option<String>,
    pub citations: Vec<String>,
    pub payload: Value,
}

/// Aggregated counts of observations by kind and severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationSummary {
    pub total: usize,
    pub unresolved: usize,
    pub by_kind: std::collections::HashMap<String, usize>,
    pub by_severity: std::collections::HashMap<String, usize>,
}

/// Routine analysis configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineConfig {
    /// Maximum number of ledger entries to analyze per run.
    pub max_entries: usize,
    /// Minimum tool failure count to flag as a pattern.
    pub pattern_threshold: usize,
    /// Maximum age of entries to consider (in hours).
    pub max_age_hours: u64,
    /// Whether to auto-resolve observations older than max_age_hours.
    pub auto_resolve_stale: bool,
}

impl Default for RoutineConfig {
    fn default() -> Self {
        Self {
            max_entries: 100,
            pattern_threshold: 3,
            max_age_hours: 48,
            auto_resolve_stale: true,
        }
    }
}
