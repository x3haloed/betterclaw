//! Routine execution engine.
//!
//! Handles loading routines, checking triggers, enforcing guardrails,
//! and executing both lightweight (single LLM call) and full-job routines.
//!
//! The engine runs two independent loops:
//! - A **cron ticker** that polls the DB every N seconds for due cron routines
//! - An **event matcher** called synchronously from the agent main loop
//!
//! Lightweight routines execute inline (single LLM call, no scheduler slot).
//! Full-job routines are delegated to the existing `Scheduler`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::Utc;
use regex::Regex;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::agent::Scheduler;
use crate::agent::ledger_capture::append_event_best_effort;
use crate::agent::routine::{
    NotifyConfig, Routine, RoutineAction, RoutineRun, RunStatus, Trigger, next_cron_fire,
};
use crate::channels::{IncomingMessage, OutgoingResponse};
use crate::config::RoutineConfig;
use crate::db::Database;
use crate::error::RoutineError;
use crate::llm::{ChatMessage, CompletionRequest, FinishReason, LlmProvider};
use crate::workspace::FsWorkspace;

/// The routine execution engine.
pub struct RoutineEngine {
    config: RoutineConfig,
    store: Arc<dyn Database>,
    llm: Arc<dyn LlmProvider>,
    workspace: Arc<FsWorkspace>,
    /// Sender for notifications (routed to channel manager).
    notify_tx: mpsc::Sender<OutgoingResponse>,
    /// Currently running routine count (across all routines).
    running_count: Arc<AtomicUsize>,
    /// Compiled event regex cache: routine_id -> compiled regex.
    event_cache: Arc<RwLock<Vec<(Uuid, Routine, Regex)>>>,
    /// Scheduler for dispatching jobs (FullJob mode).
    scheduler: Option<Arc<Scheduler>>,
}

impl RoutineEngine {
    pub fn new(
        config: RoutineConfig,
        store: Arc<dyn Database>,
        llm: Arc<dyn LlmProvider>,
        workspace: Arc<FsWorkspace>,
        notify_tx: mpsc::Sender<OutgoingResponse>,
        scheduler: Option<Arc<Scheduler>>,
    ) -> Self {
        Self {
            config,
            store,
            llm,
            workspace,
            notify_tx,
            running_count: Arc::new(AtomicUsize::new(0)),
            event_cache: Arc::new(RwLock::new(Vec::new())),
            scheduler,
        }
    }

    /// Refresh the in-memory event trigger cache from DB.
    pub async fn refresh_event_cache(&self) {
        match self.store.list_event_routines().await {
            Ok(routines) => {
                let mut cache = Vec::new();
                for routine in routines {
                    if let Trigger::Event { ref pattern, .. } = routine.trigger {
                        match Regex::new(pattern) {
                            Ok(re) => cache.push((routine.id, routine.clone(), re)),
                            Err(e) => {
                                tracing::warn!(
                                    routine = %routine.name,
                                    "Invalid event regex '{}': {}",
                                    pattern, e
                                );
                            }
                        }
                    }
                }
                let count = cache.len();
                *self.event_cache.write().await = cache;
                tracing::debug!("Refreshed event cache: {} routines", count);
            }
            Err(e) => {
                tracing::error!("Failed to refresh event cache: {}", e);
            }
        }
    }

    pub async fn ensure_builtin_observation_routines(&self) {
        let cfg = &self.config.observation;
        if !cfg.enabled {
            tracing::debug!("Built-in observation routines disabled");
            return;
        }

        let specs = builtin_observation_specs(cfg);
        for spec in specs {
            if let Err(e) = self.upsert_builtin_observation_routine(&spec).await {
                tracing::error!(routine = %spec.name, "Failed to sync builtin observation routine: {}", e);
            }
        }
    }

    /// Check incoming message against event triggers. Returns number of routines fired.
    ///
    /// Called synchronously from the main loop after handle_message(). The actual
    /// execution is spawned async so this returns quickly.
    pub async fn check_event_triggers(&self, message: &IncomingMessage) -> usize {
        let cache = self.event_cache.read().await;
        let mut fired = 0;

        for (_, routine, re) in cache.iter() {
            // Channel filter
            if let Trigger::Event {
                channel: Some(ch), ..
            } = &routine.trigger
                && ch != &message.channel
            {
                continue;
            }

            // Regex match
            if !re.is_match(&message.content) {
                continue;
            }

            // Cooldown check
            if !self.check_cooldown(routine) {
                tracing::debug!(routine = %routine.name, "Skipped: cooldown active");
                continue;
            }

            // Concurrent run check
            if !self.check_concurrent(routine).await {
                tracing::debug!(routine = %routine.name, "Skipped: max concurrent reached");
                continue;
            }

            // Global capacity check
            if self.running_count.load(Ordering::Relaxed) >= self.config.max_concurrent_routines {
                tracing::warn!(routine = %routine.name, "Skipped: global max concurrent reached");
                continue;
            }

            let detail = truncate(&message.content, 200);
            self.spawn_fire(routine.clone(), "event", Some(detail));
            fired += 1;
        }

        fired
    }

    /// Check all due cron routines and fire them. Called by the cron ticker.
    pub async fn check_cron_triggers(&self) {
        let routines = match self.store.list_due_cron_routines().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to load due cron routines: {}", e);
                return;
            }
        };

        for routine in routines {
            if self.running_count.load(Ordering::Relaxed) >= self.config.max_concurrent_routines {
                tracing::warn!("Global max concurrent routines reached, skipping remaining");
                break;
            }

            if !self.check_cooldown(&routine) {
                continue;
            }

            if !self.check_concurrent(&routine).await {
                continue;
            }

            let detail = if let Trigger::Cron { ref schedule } = routine.trigger {
                Some(schedule.clone())
            } else {
                None
            };

            self.spawn_fire(routine, "cron", detail);
        }
    }

    /// Fire a routine manually (from tool call or CLI).
    pub async fn fire_manual(&self, routine_id: Uuid) -> Result<Uuid, RoutineError> {
        let routine = self
            .store
            .get_routine(routine_id)
            .await
            .map_err(|e| RoutineError::Database {
                reason: e.to_string(),
            })?
            .ok_or(RoutineError::NotFound { id: routine_id })?;

        if !routine.enabled {
            return Err(RoutineError::Disabled {
                name: routine.name.clone(),
            });
        }

        if !self.check_concurrent(&routine).await {
            return Err(RoutineError::MaxConcurrent {
                name: routine.name.clone(),
            });
        }

        let run_id = Uuid::new_v4();
        let run = RoutineRun {
            id: run_id,
            routine_id: routine.id,
            trigger_type: "manual".to_string(),
            trigger_detail: None,
            started_at: Utc::now(),
            completed_at: None,
            status: RunStatus::Running,
            result_summary: None,
            tokens_used: None,
            job_id: None,
            created_at: Utc::now(),
        };

        if let Err(e) = self.store.create_routine_run(&run).await {
            return Err(RoutineError::Database {
                reason: format!("failed to create run record: {e}"),
            });
        }

        // Execute inline for manual triggers (caller wants to wait)
        let engine = EngineContext {
            store: self.store.clone(),
            llm: self.llm.clone(),
            workspace: self.workspace.clone(),
            notify_tx: self.notify_tx.clone(),
            running_count: self.running_count.clone(),
            scheduler: self.scheduler.clone(),
        };

        tokio::spawn(async move {
            execute_routine(engine, routine, run).await;
        });

        Ok(run_id)
    }

    /// Spawn a fire in a background task.
    fn spawn_fire(&self, routine: Routine, trigger_type: &str, trigger_detail: Option<String>) {
        let run = RoutineRun {
            id: Uuid::new_v4(),
            routine_id: routine.id,
            trigger_type: trigger_type.to_string(),
            trigger_detail,
            started_at: Utc::now(),
            completed_at: None,
            status: RunStatus::Running,
            result_summary: None,
            tokens_used: None,
            job_id: None,
            created_at: Utc::now(),
        };

        let engine = EngineContext {
            store: self.store.clone(),
            llm: self.llm.clone(),
            workspace: self.workspace.clone(),
            notify_tx: self.notify_tx.clone(),
            running_count: self.running_count.clone(),
            scheduler: self.scheduler.clone(),
        };

        // Record the run in DB, then spawn execution
        let store = self.store.clone();
        tokio::spawn(async move {
            if let Err(e) = store.create_routine_run(&run).await {
                tracing::error!(routine = %routine.name, "Failed to record run: {}", e);
                return;
            }
            execute_routine(engine, routine, run).await;
        });
    }

    fn check_cooldown(&self, routine: &Routine) -> bool {
        if let Some(last_run) = routine.last_run_at {
            let elapsed = Utc::now().signed_duration_since(last_run);
            let cooldown = chrono::Duration::from_std(routine.guardrails.cooldown)
                .unwrap_or(chrono::Duration::seconds(300));
            if elapsed < cooldown {
                return false;
            }
        }
        true
    }

    async fn check_concurrent(&self, routine: &Routine) -> bool {
        match self.store.count_running_routine_runs(routine.id).await {
            Ok(count) => count < routine.guardrails.max_concurrent as i64,
            Err(e) => {
                tracing::error!(
                    routine = %routine.name,
                    "Failed to check concurrent runs: {}", e
                );
                false
            }
        }
    }

    async fn upsert_builtin_observation_routine(
        &self,
        spec: &BuiltinObservationSpec<'_>,
    ) -> Result<(), RoutineError> {
        next_cron_fire(&spec.schedule)?;

        let action = RoutineAction::SessionObservation {
            prompt: spec.prompt.to_string(),
            ledger_kind: spec.ledger_kind.to_string(),
            recent_ledger_events: spec.recent_ledger_events,
            active_invariants: spec.active_invariants,
            unresolved_observations: spec.unresolved_observations,
            max_tokens: spec.max_tokens,
        };

        let notify = NotifyConfig {
            channel: None,
            user: spec.user_id.to_string(),
            on_attention: false,
            on_failure: false,
            on_success: false,
        };

        let trigger = Trigger::Cron {
            schedule: spec.schedule.to_string(),
        };
        let next_fire_at = next_cron_fire(spec.schedule)?;

        if let Some(mut routine) = self
            .store
            .get_routine_by_name(spec.user_id, spec.name)
            .await
            .map_err(|e| RoutineError::Database {
                reason: e.to_string(),
            })?
        {
            routine.description = spec.description.to_string();
            routine.enabled = true;
            routine.trigger = trigger;
            routine.action = action;
            routine.notify = notify;
            routine.guardrails.cooldown = Duration::from_secs(0);
            routine.next_fire_at = next_fire_at;
            routine.updated_at = Utc::now();
            routine.state = serde_json::json!({
                "managed_by": "builtin_observation_routines",
                "ledger_kind": spec.ledger_kind,
            });

            self.store
                .update_routine(&routine)
                .await
                .map_err(|e| RoutineError::Database {
                    reason: e.to_string(),
                })?;
        } else {
            let now = Utc::now();
            let routine = Routine {
                id: Uuid::new_v4(),
                name: spec.name.to_string(),
                description: spec.description.to_string(),
                user_id: spec.user_id.to_string(),
                enabled: true,
                trigger,
                action,
                guardrails: crate::agent::routine::RoutineGuardrails {
                    cooldown: Duration::from_secs(0),
                    max_concurrent: 1,
                    dedup_window: None,
                },
                notify,
                last_run_at: None,
                next_fire_at,
                run_count: 0,
                consecutive_failures: 0,
                state: serde_json::json!({
                    "managed_by": "builtin_observation_routines",
                    "ledger_kind": spec.ledger_kind,
                }),
                created_at: now,
                updated_at: now,
            };

            self.store
                .create_routine(&routine)
                .await
                .map_err(|e| RoutineError::Database {
                    reason: e.to_string(),
                })?;
        }

        Ok(())
    }
}

/// Shared context passed to the execution function.
struct EngineContext {
    store: Arc<dyn Database>,
    llm: Arc<dyn LlmProvider>,
    workspace: Arc<FsWorkspace>,
    notify_tx: mpsc::Sender<OutgoingResponse>,
    running_count: Arc<AtomicUsize>,
    scheduler: Option<Arc<Scheduler>>,
}

/// Execute a routine run. Handles both lightweight and full_job modes.
async fn execute_routine(ctx: EngineContext, routine: Routine, run: RoutineRun) {
    // Increment running count (atomic: survives panics in the execution below)
    ctx.running_count.fetch_add(1, Ordering::Relaxed);

    let result = match &routine.action {
        RoutineAction::Lightweight {
            prompt,
            context_paths,
            max_tokens,
        } => execute_lightweight(&ctx, &routine, prompt, context_paths, *max_tokens).await,
        RoutineAction::FullJob {
            title,
            description,
            max_iterations,
        } => execute_full_job(&ctx, &routine, &run, title, description, *max_iterations).await,
        RoutineAction::SessionObservation {
            prompt,
            ledger_kind,
            recent_ledger_events,
            active_invariants,
            unresolved_observations,
            max_tokens,
        } => {
            execute_session_observation(
                &ctx,
                &routine,
                prompt,
                ledger_kind,
                *recent_ledger_events,
                *active_invariants,
                *unresolved_observations,
                *max_tokens,
            )
            .await
        }
    };

    // Decrement running count
    ctx.running_count.fetch_sub(1, Ordering::Relaxed);

    // Process result
    let (status, summary, tokens) = match result {
        Ok(execution) => execution,
        Err(e) => {
            tracing::error!(routine = %routine.name, "Execution failed: {}", e);
            (RunStatus::Failed, Some(e.to_string()), None)
        }
    };

    // Complete the run record
    if let Err(e) = ctx
        .store
        .complete_routine_run(run.id, status, summary.as_deref(), tokens)
        .await
    {
        tracing::error!(routine = %routine.name, "Failed to complete run record: {}", e);
    }

    // Update routine runtime state
    let now = Utc::now();
    let next_fire = if let Trigger::Cron { ref schedule } = routine.trigger {
        next_cron_fire(schedule).unwrap_or(None)
    } else {
        None
    };

    let new_failures = if status == RunStatus::Failed {
        routine.consecutive_failures + 1
    } else {
        0
    };

    if let Err(e) = ctx
        .store
        .update_routine_runtime(
            routine.id,
            now,
            next_fire,
            routine.run_count + 1,
            new_failures,
            &routine.state,
        )
        .await
    {
        tracing::error!(routine = %routine.name, "Failed to update runtime state: {}", e);
    }

    // Send notifications based on config
    send_notification(
        &ctx.notify_tx,
        &routine.notify,
        &routine.name,
        status,
        summary.as_deref(),
    )
    .await;
}

/// Sanitize a routine name for use in workspace paths.
/// Only keeps alphanumeric, dash, and underscore characters; replaces everything else.
fn sanitize_routine_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Execute a full-job routine by dispatching to the scheduler.
///
/// Fire-and-forget: creates a job via `Scheduler::dispatch_job` (which handles
/// creation, metadata, persistence, and scheduling), links the routine run to
/// the job, and returns immediately. The job runs independently via the
/// existing Worker/Scheduler with full tool access.
async fn execute_full_job(
    ctx: &EngineContext,
    routine: &Routine,
    run: &RoutineRun,
    title: &str,
    description: &str,
    max_iterations: u32,
) -> Result<(RunStatus, Option<String>, Option<i32>), RoutineError> {
    let scheduler = ctx
        .scheduler
        .as_ref()
        .ok_or_else(|| RoutineError::JobDispatchFailed {
            reason: "scheduler not available".to_string(),
        })?;

    let metadata = serde_json::json!({ "max_iterations": max_iterations });

    let job_id = scheduler
        .dispatch_job(&routine.user_id, title, description, Some(metadata))
        .await
        .map_err(|e| RoutineError::JobDispatchFailed {
            reason: format!("failed to dispatch job: {e}"),
        })?;

    // Link the routine run to the dispatched job
    if let Err(e) = ctx.store.link_routine_run_to_job(run.id, job_id).await {
        tracing::error!(
            routine = %routine.name,
            "Failed to link run to job: {}", e
        );
    }

    tracing::info!(
        routine = %routine.name,
        job_id = %job_id,
        max_iterations = max_iterations,
        "Dispatched full job for routine"
    );

    let summary = format!(
        "Dispatched job {job_id} for full execution with tool access (max_iterations: {max_iterations})"
    );
    Ok((RunStatus::Ok, Some(summary), None))
}

/// Execute a lightweight routine (single LLM call).
async fn execute_lightweight(
    ctx: &EngineContext,
    routine: &Routine,
    prompt: &str,
    context_paths: &[String],
    max_tokens: u32,
) -> Result<(RunStatus, Option<String>, Option<i32>), RoutineError> {
    // Load context from workspace
    let mut context_parts = Vec::new();
    for path in context_paths {
        match ctx.workspace.read_optional_rel(path).await {
            Ok(Some(content)) => {
                context_parts.push(format!("## {}\n\n{}", path, content));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(
                    routine = %routine.name,
                    "Failed to read context path {}: {}", path, e
                );
            }
        }
    }

    // Load routine state from workspace (name sanitized to prevent path traversal)
    let safe_name = sanitize_routine_name(&routine.name);
    let state_path = format!("routines/{safe_name}/state.md");
    let state_content = match ctx.workspace.read_optional_rel(&state_path).await {
        Ok(Some(content)) => Some(content),
        Err(_) => None,
        Ok(None) => None,
    };

    // Build the prompt
    let mut full_prompt = String::new();
    full_prompt.push_str(prompt);

    if !context_parts.is_empty() {
        full_prompt.push_str("\n\n---\n\n# Context\n\n");
        full_prompt.push_str(&context_parts.join("\n\n"));
    }

    if let Some(state) = &state_content {
        full_prompt.push_str("\n\n---\n\n# Previous State\n\n");
        full_prompt.push_str(state);
    }

    full_prompt.push_str(
        "\n\n---\n\nIf nothing needs attention, reply EXACTLY with: ROUTINE_OK\n\
         If something needs attention, provide a concise summary.",
    );

    // Get system prompt
    let system_prompt = match ctx.workspace.system_prompt().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(routine = %routine.name, "Failed to get system prompt: {}", e);
            String::new()
        }
    };

    let messages = if system_prompt.is_empty() {
        vec![ChatMessage::user(&full_prompt)]
    } else {
        vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&full_prompt),
        ]
    };

    // Determine max_tokens from model metadata with fallback
    let effective_max_tokens = match ctx.llm.model_metadata().await {
        Ok(meta) => {
            let from_api = meta.context_length.map(|ctx| ctx / 2).unwrap_or(max_tokens);
            from_api.max(max_tokens)
        }
        Err(_) => max_tokens,
    };

    let request = CompletionRequest::new(messages)
        .with_max_tokens(effective_max_tokens)
        .with_temperature(0.3);

    let response = ctx
        .llm
        .complete(request)
        .await
        .map_err(|e| RoutineError::LlmFailed {
            reason: e.to_string(),
        })?;

    let content = response.content.trim();
    let tokens_used = Some((response.input_tokens + response.output_tokens) as i32);

    // Empty content guard (same as heartbeat)
    if content.is_empty() {
        return if response.finish_reason == FinishReason::Length {
            Err(RoutineError::TruncatedResponse)
        } else {
            Err(RoutineError::EmptyResponse)
        };
    }

    // Check for the "nothing to do" sentinel
    if content == "ROUTINE_OK" || content.contains("ROUTINE_OK") {
        return Ok((RunStatus::Ok, None, tokens_used));
    }

    Ok((RunStatus::Attention, Some(content.to_string()), tokens_used))
}

async fn execute_session_observation(
    ctx: &EngineContext,
    routine: &Routine,
    prompt: &str,
    ledger_kind: &str,
    recent_ledger_events: i64,
    active_invariants: i64,
    unresolved_observations: i64,
    max_tokens: u32,
) -> Result<(RunStatus, Option<String>, Option<i32>), RoutineError> {
    let system_prompt = match ctx.workspace.system_prompt().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(routine = %routine.name, "Failed to get system prompt: {}", e);
            String::new()
        }
    };

    let recent_events = if recent_ledger_events > 0 {
        let mut events = ctx
            .store
            .list_recent_ledger_events_for_compression(&routine.user_id, recent_ledger_events)
            .await
            .map_err(|e| RoutineError::Database {
                reason: format!("failed to load recent ledger events: {e}"),
            })?;
        events.reverse();
        events
    } else {
        Vec::new()
    };

    let active_invariant_events = if active_invariants > 0 {
        let mut events = ctx
            .store
            .list_recent_ledger_events_by_kind_prefix(
                &routine.user_id,
                "invariant.",
                active_invariants,
            )
            .await
            .map_err(|e| RoutineError::Database {
                reason: format!("failed to load active invariants: {e}"),
            })?;
        events.reverse();
        events
    } else {
        Vec::new()
    };

    let unresolved_events = if unresolved_observations > 0 {
        let per_kind = (unresolved_observations / 2).max(1);
        let mut tension = ctx
            .store
            .list_recent_ledger_events_by_kind_prefix(
                &routine.user_id,
                "observation.tension",
                per_kind,
            )
            .await
            .map_err(|e| RoutineError::Database {
                reason: format!("failed to load unresolved tensions: {e}"),
            })?;
        let mut pattern = ctx
            .store
            .list_recent_ledger_events_by_kind_prefix(
                &routine.user_id,
                "observation.pattern",
                per_kind,
            )
            .await
            .map_err(|e| RoutineError::Database {
                reason: format!("failed to load unresolved patterns: {e}"),
            })?;
        tension.reverse();
        pattern.reverse();
        let mut combined = Vec::with_capacity(tension.len() + pattern.len());
        combined.extend(tension);
        combined.extend(pattern);
        combined
    } else {
        Vec::new()
    };

    let full_prompt = build_session_observation_prompt(
        prompt,
        &recent_events,
        &active_invariant_events,
        &unresolved_events,
    );

    let messages = if system_prompt.is_empty() {
        vec![ChatMessage::user(&full_prompt)]
    } else {
        vec![
            ChatMessage::system(&system_prompt),
            ChatMessage::user(&full_prompt),
        ]
    };

    let effective_max_tokens = match ctx.llm.model_metadata().await {
        Ok(meta) => {
            let from_api = meta.context_length.map(|ctx| ctx / 2).unwrap_or(max_tokens);
            from_api.max(max_tokens)
        }
        Err(_) => max_tokens,
    };

    let request = CompletionRequest::new(messages)
        .with_max_tokens(effective_max_tokens)
        .with_temperature(0.2);

    let response = ctx
        .llm
        .complete(request)
        .await
        .map_err(|e| RoutineError::LlmFailed {
            reason: e.to_string(),
        })?;

    let content = response.content.trim();
    let tokens_used = Some((response.input_tokens + response.output_tokens) as i32);

    if content.is_empty() {
        return if response.finish_reason == FinishReason::Length {
            Err(RoutineError::TruncatedResponse)
        } else {
            Err(RoutineError::EmptyResponse)
        };
    }

    let event_id = append_event_best_effort(
        Some(Arc::clone(&ctx.store)),
        routine.user_id.clone(),
        None,
        ledger_kind.to_string(),
        format!("routine:{}", routine.name),
        Some(content.to_string()),
        serde_json::json!({
            "routine_id": routine.id.to_string(),
            "routine_name": routine.name,
            "trigger": "routine",
            "recent_ledger_event_ids": recent_events.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            "active_invariant_event_ids": active_invariant_events.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
            "unresolved_observation_event_ids": unresolved_events.iter().map(|e| e.id.to_string()).collect::<Vec<_>>(),
        }),
    )
    .await;

    let preview = truncate(content, 240);
    let summary = match event_id {
        Some(id) => format!("Recorded {} as {} ({})", routine.name, ledger_kind, id),
        None => format!(
            "Generated {} observation, but ledger capture failed: {}",
            ledger_kind, preview
        ),
    };

    Ok((RunStatus::Ok, Some(summary), tokens_used))
}

fn build_session_observation_prompt(
    prompt: &str,
    recent_events: &[crate::ledger::LedgerEvent],
    active_invariants: &[crate::ledger::LedgerEvent],
    unresolved_events: &[crate::ledger::LedgerEvent],
) -> String {
    let mut out = String::new();
    out.push_str(prompt.trim());

    if !recent_events.is_empty() {
        out.push_str("\n\n## Recent Ledger Events\n");
        out.push_str(&format_events_for_prompt(recent_events));
    }

    if !active_invariants.is_empty() {
        out.push_str("\n\n## Active Invariants\n");
        out.push_str(&format_events_for_prompt(active_invariants));
    }

    if !unresolved_events.is_empty() {
        out.push_str("\n\n## Unresolved Patterns Or Tensions\n");
        out.push_str(&format_events_for_prompt(unresolved_events));
    }

    out.push_str("\n\nTools are disabled for this pass.");
    out
}

fn format_events_for_prompt(events: &[crate::ledger::LedgerEvent]) -> String {
    if events.is_empty() {
        return "(none)\n".to_string();
    }

    events
        .iter()
        .map(|event| {
            let content = event
                .content
                .as_deref()
                .map(|c| truncate(c, 1200))
                .unwrap_or_else(|| truncate(&event.payload.to_string(), 1200));
            format!(
                "- {} {} {} {}\n  content: {}\n",
                event.id,
                event.created_at.to_rfc3339(),
                event.kind,
                event.source,
                content.replace('\n', "\\n")
            )
        })
        .collect::<Vec<_>>()
        .join("")
}

struct BuiltinObservationSpec<'a> {
    name: &'a str,
    description: &'a str,
    user_id: &'a str,
    schedule: &'a str,
    prompt: &'a str,
    ledger_kind: &'a str,
    recent_ledger_events: i64,
    active_invariants: i64,
    unresolved_observations: i64,
    max_tokens: u32,
}

fn builtin_observation_specs(
    cfg: &crate::config::ObservationRoutineConfig,
) -> Vec<BuiltinObservationSpec<'_>> {
    vec![
        BuiltinObservationSpec {
            name: "observation.tension",
            description: "Invariant maintenance loop that scans recent evidence for tensions or contradictions.",
            user_id: &cfg.user_id,
            schedule: &cfg.tension_schedule,
            prompt: "You are performing invariant maintenance.\n\nInputs:\n- recent ledger events\n- active invariants\n\nTask:\nIdentify tensions or contradictions between events and invariants.\n\nIf you find one:\n- describe it\n- cite evidence\n- suggest whether the invariant may be conditional, outdated, or incomplete.\n\nDo not modify invariants directly.\nOnly report observations.",
            ledger_kind: "observation.tension",
            recent_ledger_events: cfg.recent_ledger_events,
            active_invariants: cfg.active_invariants,
            unresolved_observations: 0,
            max_tokens: cfg.max_tokens,
        },
        BuiltinObservationSpec {
            name: "observation.pattern",
            description: "Pattern scout loop that scans recent experience for repeated behaviors and correlations.",
            user_id: &cfg.user_id,
            schedule: &cfg.pattern_schedule,
            prompt: "You are scanning recent experience for repeated patterns.\n\nInputs:\n- recent ledger events\n\nTask:\nIdentify repeated behaviors or correlations that might deserve an invariant.\n\nFor each pattern:\n- describe pattern\n- cite events\n- estimate confidence",
            ledger_kind: "observation.pattern",
            recent_ledger_events: cfg.recent_ledger_events,
            active_invariants: 0,
            unresolved_observations: 0,
            max_tokens: cfg.max_tokens,
        },
        BuiltinObservationSpec {
            name: "observation.hypothesis",
            description: "Hypothesis loop that turns unresolved patterns or tensions into small reasoning experiments.",
            user_id: &cfg.user_id,
            schedule: &cfg.hypothesis_schedule,
            prompt: "You are evaluating hypotheses about behavior patterns.\n\nInputs:\n- unresolved patterns or tensions\n\nTask:\nDesign a small reasoning experiment or observation to test the pattern.\n\nReport the hypothesis and expected signal.",
            ledger_kind: "observation.hypothesis",
            recent_ledger_events: 0,
            active_invariants: 0,
            unresolved_observations: cfg.unresolved_observations,
            max_tokens: cfg.max_tokens,
        },
    ]
}

/// Send a notification based on the routine's notify config and run status.
async fn send_notification(
    tx: &mpsc::Sender<OutgoingResponse>,
    notify: &NotifyConfig,
    routine_name: &str,
    status: RunStatus,
    summary: Option<&str>,
) {
    let should_notify = match status {
        RunStatus::Ok => notify.on_success,
        RunStatus::Attention => notify.on_attention,
        RunStatus::Failed => notify.on_failure,
        RunStatus::Running => false,
    };

    if !should_notify {
        return;
    }

    let icon = match status {
        RunStatus::Ok => "✅",
        RunStatus::Attention => "🔔",
        RunStatus::Failed => "❌",
        RunStatus::Running => "⏳",
    };

    let message = match summary {
        Some(s) => format!("{} *Routine '{}'*: {}\n\n{}", icon, routine_name, status, s),
        None => format!("{} *Routine '{}'*: {}", icon, routine_name, status),
    };

    let response = OutgoingResponse {
        content: message,
        thread_id: None,
        attachments: Vec::new(),
        metadata: serde_json::json!({
            "source": "routine",
            "routine_name": routine_name,
            "status": status.to_string(),
            "notify_user": notify.user,
            "notify_channel": notify.channel,
        }),
    };

    if let Err(e) = tx.send(response).await {
        tracing::error!(routine = %routine_name, "Failed to send notification: {}", e);
    }
}

/// Spawn the cron ticker background task.
pub fn spawn_cron_ticker(
    engine: Arc<RoutineEngine>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // Skip immediate first tick
        ticker.tick().await;

        loop {
            ticker.tick().await;
            engine.check_cron_triggers().await;
        }
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = crate::util::floor_char_boundary(s, max);
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::routine::{NotifyConfig, RunStatus};

    #[test]
    fn test_notification_gating() {
        let config = NotifyConfig {
            on_success: false,
            on_failure: true,
            on_attention: true,
            ..Default::default()
        };

        // on_success = false means Ok status should not notify
        assert!(!config.on_success);
        assert!(config.on_failure);
        assert!(config.on_attention);
    }

    #[test]
    fn test_run_status_icons() {
        // Just verify the mapping doesn't panic
        for status in [
            RunStatus::Ok,
            RunStatus::Attention,
            RunStatus::Failed,
            RunStatus::Running,
        ] {
            let _ = status.to_string();
        }
    }
}
