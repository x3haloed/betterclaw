use serde_json::json;

use crate::error::RuntimeError;
use crate::memory::{LedgerEntry, LedgerEntryKind};
use crate::routine::*;
use crate::turn::TurnStatus;

use super::Runtime;

impl Runtime {
    /// Run all observation routines after a turn completes.
    /// Produces tension, pattern, hypothesis, and contradiction observations
    /// from ledger entries and memory artifacts.
    pub(crate) async fn run_observation_routines(
        &self,
        namespace_id: &str,
        config: &RoutineConfig,
    ) -> Result<Vec<Observation>, RuntimeError> {
        let mut observations = Vec::new();

        if config.auto_resolve_stale {
            let stale_ids = self
                .db
                .stale_observation_ids(namespace_id, config.max_age_hours as i64)
                .await
                .map_err(RuntimeError::from)?;
            for id in &stale_ids {
                self.db
                    .resolve_observation(id)
                    .await
                    .map_err(RuntimeError::from)?;
            }
        }

        let entries = self.normalized_entries_for_namespace(namespace_id).await?;
        let recent = if entries.len() > config.max_entries {
            &entries[entries.len() - config.max_entries..]
        } else {
            &entries
        };

        observations.extend(self.detect_tensions(namespace_id, recent).await?);
        observations.extend(
            self.detect_tool_failure_patterns(namespace_id, recent, config.pattern_threshold)
                .await?,
        );
        observations.extend(self.detect_hypotheses(namespace_id, recent).await?);
        observations.extend(
            self.detect_contradictions(namespace_id, recent, config)
                .await?,
        );

        Ok(observations)
    }

    /// Detect unresolved tensions: tool chains that ended without a final_message,
    /// turns stuck in AwaitingUser, or errors without follow-up.
    async fn detect_tensions(
        &self,
        namespace_id: &str,
        entries: &[LedgerEntry],
    ) -> Result<Vec<Observation>, RuntimeError> {
        let mut observations = Vec::new();
        let mut turns_with_final_message = std::collections::HashSet::new();
        let mut awaiting_user_turns = Vec::new();
        let mut error_turns_without_recovery = std::collections::HashMap::new();

        for entry in entries {
            match &entry.kind {
                LedgerEntryKind::ToolResult => {
                    // Check if the payload contains a final_message control
                    if let Some(control) = entry.payload.get("__betterclaw_control") {
                        if control.get("kind").and_then(|v| v.as_str()) == Some("final_message") {
                            turns_with_final_message.insert(entry.turn_id.clone());
                        }
                    }
                }
                LedgerEntryKind::Error => {
                    error_turns_without_recovery
                        .entry(entry.turn_id.clone())
                        .or_insert_with(Vec::new)
                        .push(entry);
                }
                LedgerEntryKind::AgentTurn => {
                    // Agent turns that succeeded clear error flags
                    error_turns_without_recovery.remove(&entry.turn_id);
                }
                _ => {}
            }
        }

        // Check for turns that ended in AwaitingUser
        for thread in self.db.list_threads().await.map_err(RuntimeError::from)? {
            for turn in self
                .db
                .list_thread_turns(&thread.id)
                .await
                .map_err(RuntimeError::from)?
            {
                if turn.status == TurnStatus::AwaitingUser {
                    awaiting_user_turns.push((turn.id.clone(), turn.user_message.clone()));
                }
            }
        }

        // Tension: tool chain without final_message
        let tool_only_turns: std::collections::HashSet<_> = entries
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    LedgerEntryKind::ToolCall | LedgerEntryKind::ToolResult
                )
            })
            .map(|e| e.turn_id.clone())
            .collect();
        for turn_id in &tool_only_turns {
            if !turns_with_final_message.contains(turn_id) {
                // Check if there's an assistant turn after the tools (indicating completion)
                let has_assistant_after = entries
                    .iter()
                    .any(|e| e.turn_id == *turn_id && matches!(e.kind, LedgerEntryKind::AgentTurn));
                if !has_assistant_after {
                    let citations: Vec<String> = entries
                        .iter()
                        .filter(|e| e.turn_id == *turn_id)
                        .map(|e| e.entry_id.clone())
                        .collect();
                    let tool_names: Vec<String> = entries
                        .iter()
                        .filter(|e| {
                            e.turn_id == *turn_id && matches!(e.kind, LedgerEntryKind::ToolCall)
                        })
                        .filter_map(|e| {
                            e.payload
                                .get("tool_name")
                                .and_then(|v| v.as_str())
                                .map(String::from)
                        })
                        .collect();
                    observations.push(
                        self.store_observation(
                            namespace_id,
                            NewObservation {
                                namespace_id: namespace_id.to_string(),
                                kind: ObservationKind::Tension,
                                severity: Severity::Medium,
                                summary: format!(
                                    "Turn {} ended with tool calls but no final reply",
                                    &turn_id[..8.min(turn_id.len())]
                                ),
                                detail: Some(format!(
                                    "Tools called: {}. No final_message or assistant response followed.",
                                    tool_names.join(", ")
                                )),
                                citations,
                                payload: json!({
                                    "turn_id": turn_id,
                                    "tool_names": tool_names,
                                    "tension_type": "incomplete_tool_chain",
                                }),
                            },
                        )
                        .await?,
                    );
                }
            }
        }

        // Tension: unresolved AwaitingUser
        for (turn_id, question) in &awaiting_user_turns {
            let observation_exists = self
                .db
                .list_observations(namespace_id, Some(ObservationKind::Tension), true, 64)
                .await
                .map_err(RuntimeError::from)?
                .iter()
                .any(|o| {
                    o.payload.get("turn_id").and_then(|v| v.as_str()) == Some(turn_id.as_str())
                        && o.payload.get("tension_type").and_then(|v| v.as_str())
                            == Some("awaiting_user")
                });
            if !observation_exists {
                observations.push(
                    self.store_observation(
                        namespace_id,
                        NewObservation {
                            namespace_id: namespace_id.to_string(),
                            kind: ObservationKind::Tension,
                            severity: Severity::Low,
                            summary: format!(
                                "Awaiting user response for turn {}",
                                &turn_id[..8.min(turn_id.len())]
                            ),
                            detail: Some(format!(
                                "Question asked: {}",
                                question.chars().take(200).collect::<String>()
                            )),
                            citations: vec![format!("turn:{turn_id}")],
                            payload: json!({
                                "turn_id": turn_id,
                                "question": question,
                                "tension_type": "awaiting_user",
                            }),
                        },
                    )
                    .await?,
                );
            }
        }

        // Tension: errors without recovery
        for (turn_id, error_entries) in &error_turns_without_recovery {
            let last_error = error_entries.last().unwrap();
            observations.push(
                self.store_observation(
                    namespace_id,
                    NewObservation {
                        namespace_id: namespace_id.to_string(),
                        kind: ObservationKind::Tension,
                        severity: Severity::High,
                        summary: format!(
                            "Turn {} had errors without successful recovery",
                            &turn_id[..8.min(turn_id.len())]
                        ),
                        detail: last_error.content.clone(),
                        citations: error_entries.iter().map(|e| e.entry_id.clone()).collect(),
                        payload: json!({
                            "turn_id": turn_id,
                            "tension_type": "unresolved_error",
                        }),
                    },
                )
                .await?,
            );
        }

        Ok(observations)
    }

    /// Detect recurring tool failure patterns.
    async fn detect_tool_failure_patterns(
        &self,
        namespace_id: &str,
        entries: &[LedgerEntry],
        threshold: usize,
    ) -> Result<Vec<Observation>, RuntimeError> {
        let mut observations = Vec::new();

        // Count error occurrences per tool name
        let mut error_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut error_entries: std::collections::HashMap<String, Vec<&LedgerEntry>> =
            std::collections::HashMap::new();

        for entry in entries {
            if entry.kind == LedgerEntryKind::Error {
                if let Some(tool_name) = entry.payload.get("tool_name").and_then(|v| v.as_str()) {
                    *error_counts.entry(tool_name.to_string()).or_insert(0) += 1;
                    error_entries
                        .entry(tool_name.to_string())
                        .or_default()
                        .push(entry);
                }
            }
        }

        for (tool_name, count) in &error_counts {
            if *count >= threshold {
                let entries_for_tool = error_entries.get(tool_name).unwrap();
                let citations: Vec<String> = entries_for_tool
                    .iter()
                    .take(10)
                    .map(|e| e.entry_id.clone())
                    .collect();
                let error_samples: Vec<String> = entries_for_tool
                    .iter()
                    .take(3)
                    .filter_map(|e| e.content.clone())
                    .collect();

                // Check if this pattern observation already exists
                let pattern_key = format!("tool_failure:{tool_name}");
                let existing = self
                    .db
                    .list_observations(namespace_id, Some(ObservationKind::Pattern), true, 64)
                    .await
                    .map_err(RuntimeError::from)?;
                let already_observed = existing.iter().any(|o| {
                    o.payload.get("pattern_key").and_then(|v| v.as_str())
                        == Some(pattern_key.as_str())
                });

                if !already_observed {
                    let severity = if *count >= threshold * 2 {
                        Severity::High
                    } else {
                        Severity::Medium
                    };
                    observations.push(
                        self.store_observation(
                            namespace_id,
                            NewObservation {
                                namespace_id: namespace_id.to_string(),
                                kind: ObservationKind::Pattern,
                                severity,
                                summary: format!(
                                    "Tool '{}' failed {} times across recent turns",
                                    tool_name, count
                                ),
                                detail: Some(format!(
                                    "Recent errors:\n{}",
                                    error_samples
                                        .iter()
                                        .map(|s| format!(
                                            "- {}",
                                            s.chars().take(120).collect::<String>()
                                        ))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )),
                                citations,
                                payload: json!({
                                    "pattern_key": pattern_key,
                                    "tool_name": tool_name,
                                    "error_count": count,
                                    "pattern_type": "recurring_tool_failure",
                                }),
                            },
                        )
                        .await?,
                    );
                }
            }
        }

        Ok(observations)
    }

    /// Generate hypotheses about what might be useful to explore next,
    /// based on recent conversation patterns.
    async fn detect_hypotheses(
        &self,
        namespace_id: &str,
        entries: &[LedgerEntry],
    ) -> Result<Vec<Observation>, RuntimeError> {
        let mut observations = Vec::new();

        // Hypothesis: if a user keeps asking about the same topic, suggest deeper investigation
        let user_turns: Vec<&LedgerEntry> = entries
            .iter()
            .filter(|e| e.kind == LedgerEntryKind::UserTurn)
            .collect();

        if user_turns.len() >= 3 {
            // Simple keyword clustering: find shared terms across recent user messages
            let recent_user_texts: Vec<String> = user_turns
                .iter()
                .rev()
                .take(6)
                .filter_map(|e| e.content.clone())
                .collect();

            let word_freq = compute_word_frequency(&recent_user_texts, 4);
            let significant_words: Vec<&String> = word_freq
                .iter()
                .filter(|(_, count)| *count >= 3)
                .map(|(word, _)| word)
                .collect();

            if !significant_words.is_empty() {
                let topic = significant_words
                    .iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");

                // Check if hypothesis for this topic exists
                let existing = self
                    .db
                    .list_observations(namespace_id, Some(ObservationKind::Hypothesis), true, 64)
                    .await
                    .map_err(RuntimeError::from)?;
                let already_observed = existing.iter().any(|o| {
                    o.payload.get("topic").and_then(|v| v.as_str()) == Some(topic.as_str())
                });

                if !already_observed {
                    let citations: Vec<String> = user_turns
                        .iter()
                        .rev()
                        .take(6)
                        .map(|e| e.entry_id.clone())
                        .collect();
                    observations.push(
                        self.store_observation(
                            namespace_id,
                            NewObservation {
                                namespace_id: namespace_id.to_string(),
                                kind: ObservationKind::Hypothesis,
                                severity: Severity::Low,
                                summary: format!(
                                    "User appears focused on: {topic}"
                                ),
                                detail: Some(format!(
                                    "This topic appeared in {} of the last {} user messages. Consider proactive investigation or skill development in this area.",
                                    significant_words.len(),
                                    recent_user_texts.len()
                                )),
                                citations,
                                payload: json!({
                                    "topic": topic,
                                    "keywords": significant_words,
                                    "hypothesis_type": "recurring_topic",
                                }),
                            },
                        )
                        .await?,
                    );
                }
            }
        }

        // Hypothesis: if many tool results are being summarized, the user may need automation
        let tool_result_count = entries
            .iter()
            .filter(|e| e.kind == LedgerEntryKind::ToolResult)
            .count();
        let agent_turn_count = entries
            .iter()
            .filter(|e| e.kind == LedgerEntryKind::AgentTurn)
            .count();

        if tool_result_count > 10 && agent_turn_count > 5 {
            let ratio = tool_result_count as f64 / agent_turn_count.max(1) as f64;
            if ratio > 2.0 {
                let existing = self
                    .db
                    .list_observations(namespace_id, Some(ObservationKind::Hypothesis), true, 64)
                    .await
                    .map_err(RuntimeError::from)?;
                let already_observed = existing.iter().any(|o| {
                    o.payload.get("hypothesis_type").and_then(|v| v.as_str())
                        == Some("high_tool_to_response_ratio")
                });
                if !already_observed {
                    observations.push(
                        self.store_observation(
                            namespace_id,
                            NewObservation {
                                namespace_id: namespace_id.to_string(),
                                kind: ObservationKind::Hypothesis,
                                severity: Severity::Low,
                                summary: "High tool usage relative to responses detected".to_string(),
                                detail: Some(format!(
                                    "Tool results ({}) vs agent turns ({}): ratio {:.1}x. Consider batch operations or automation.",
                                    tool_result_count, agent_turn_count, ratio
                                )),
                                citations: entries
                                    .iter()
                                    .rev()
                                    .take(10)
                                    .map(|e| e.entry_id.clone())
                                    .collect(),
                                payload: json!({
                                    "tool_result_count": tool_result_count,
                                    "agent_turn_count": agent_turn_count,
                                    "ratio": ratio,
                                    "hypothesis_type": "high_tool_to_response_ratio",
                                }),
                            },
                        )
                        .await?,
                    );
                }
            }
        }

        Ok(observations)
    }

    /// Detect contradictions between memory artifacts or between
    /// artifact content and recent ledger entries.
    async fn detect_contradictions(
        &self,
        namespace_id: &str,
        _entries: &[LedgerEntry],
        _config: &RoutineConfig,
    ) -> Result<Vec<Observation>, RuntimeError> {
        use crate::memory::MemoryArtifactKind;
        let mut observations = Vec::new();

        // Check for drift flags/contradictions produced by the compressor
        let drift_flags = self
            .db
            .list_memory_artifacts(namespace_id, Some(MemoryArtifactKind::DriftFlagV0), 16)
            .await
            .map_err(RuntimeError::from)?;
        let drift_contradictions = self
            .db
            .list_memory_artifacts(
                namespace_id,
                Some(MemoryArtifactKind::DriftContradictionV0),
                16,
            )
            .await
            .map_err(RuntimeError::from)?;

        // Promote compressor drift flags to observations
        for flag in drift_flags.iter().chain(drift_contradictions.iter()) {
            let existing = self
                .db
                .list_observations(namespace_id, Some(ObservationKind::Contradiction), true, 64)
                .await
                .map_err(RuntimeError::from)?;
            let already_observed = existing.iter().any(|o| {
                o.citations
                    .iter()
                    .any(|c| c == &flag.id || flag.citations.contains(c))
            });
            if !already_observed {
                let severity = if matches!(flag.kind, MemoryArtifactKind::DriftContradictionV0) {
                    Severity::High
                } else {
                    Severity::Medium
                };
                observations.push(
                    self.store_observation(
                        namespace_id,
                        NewObservation {
                            namespace_id: namespace_id.to_string(),
                            kind: ObservationKind::Contradiction,
                            severity,
                            summary: flag.content.clone(),
                            detail: Some(format!("Source: compressor ({})", flag.kind.as_str())),
                            citations: flag.citations.clone(),
                            payload: json!({
                                "source_artifact_id": flag.id,
                                "artifact_kind": flag.kind.as_str(),
                            }),
                        },
                    )
                    .await?,
                );
            }
        }

        Ok(observations)
    }

    async fn store_observation(
        &self,
        _namespace_id: &str,
        new: NewObservation,
    ) -> Result<Observation, RuntimeError> {
        self.db
            .upsert_observation(&new)
            .await
            .map_err(RuntimeError::from)
    }

    /// Build an observations context block for injection into the system prompt.
    pub(crate) async fn build_observations_block(
        &self,
        namespace_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let observations = self
            .db
            .list_observations(namespace_id, None, true, 12)
            .await
            .map_err(RuntimeError::from)?;
        if observations.is_empty() {
            return Ok(None);
        }

        let mut block =
            String::from("<observations>\nActive observations from routine analysis:\n");
        for obs in &observations {
            let kind_label = obs.kind.as_str();
            let sev_label = obs.severity.as_str();
            block.push_str(&format!(
                "- [{}/{}] {}\n",
                kind_label, sev_label, obs.summary
            ));
        }
        block.push_str("</observations>");
        Ok(Some(block))
    }
}

/// Compute word frequency across texts, filtering stop words and short tokens.
fn compute_word_frequency(texts: &[String], min_length: usize) -> Vec<(String, usize)> {
    let stop_words: std::collections::HashSet<&str> = [
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
        "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall",
        "to", "of", "in", "for", "on", "with", "at", "by", "from", "as", "into", "through",
        "during", "before", "after", "above", "below", "between", "and", "but", "or", "nor", "not",
        "so", "yet", "both", "either", "neither", "each", "every", "all", "any", "few", "more",
        "most", "other", "some", "such", "no", "only", "own", "same", "than", "too", "very",
        "just", "about", "this", "that", "these", "those", "it", "its", "i", "me", "my", "we",
        "our", "you", "your", "he", "him", "his", "she", "her", "they", "them", "their", "what",
        "which", "who", "whom", "how", "when", "where", "why",
    ]
    .iter()
    .copied()
    .collect();

    let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for text in texts {
        for word in text.split_whitespace() {
            let cleaned: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            let lower = cleaned.to_lowercase();
            if lower.len() >= min_length && !stop_words.contains(lower.as_str()) {
                *freq.entry(lower).or_insert(0) += 1;
            }
        }
    }

    let mut sorted: Vec<(String, usize)> = freq.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_frequency_filters_stop_words() {
        let texts = vec![
            "BetterClaw should handle tool failures gracefully".to_string(),
            "BetterClaw tool failure is a recurring issue".to_string(),
            "the tool failure pattern needs investigation".to_string(),
        ];
        let freq = compute_word_frequency(&texts, 4);
        let words: Vec<&str> = freq.iter().map(|(w, _)| w.as_str()).collect();
        assert!(words.contains(&"betterclaw"));
        assert!(words.contains(&"tool"));
        assert!(words.contains(&"failure"));
        assert!(!words.contains(&"the"));
        assert!(!words.contains(&"should"));
    }

    #[test]
    fn observation_kind_round_trips() {
        for kind in [
            ObservationKind::Tension,
            ObservationKind::Pattern,
            ObservationKind::Hypothesis,
            ObservationKind::Contradiction,
        ] {
            let s = kind.as_str();
            let parsed: ObservationKind = s.parse().unwrap();
            assert_eq!(kind, parsed);
        }
    }

    #[test]
    fn severity_round_trips() {
        for sev in [
            Severity::Low,
            Severity::Medium,
            Severity::High,
            Severity::Critical,
        ] {
            let s = sev.as_str();
            let parsed: Severity = s.parse().unwrap();
            assert_eq!(sev, parsed);
        }
    }
}
