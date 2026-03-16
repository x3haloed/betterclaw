use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use rusqlite::{Connection, OpenFlags, Transaction, params};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

const DEFAULT_AGENT_ID: &str = "default";
const DEFAULT_WORKSPACE_ID: &str = "default";
const DEFAULT_WORKSPACE_ROOT_SUFFIX: &str = ".betterclaw/workspaces/default/files";
const LEGACY_SETTINGS_NAMESPACE_PREFIX: &str = "legacy.settings";

const TARGET_SCHEMA: &str = r#"
CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    channel TEXT NOT NULL,
    external_thread_id TEXT NOT NULL,
    title TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(agent_id, channel, external_thread_id)
);
CREATE TABLE turns (
    id TEXT PRIMARY KEY,
    thread_id TEXT NOT NULL,
    status TEXT NOT NULL,
    user_message TEXT NOT NULL,
    assistant_message TEXT,
    error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE events (
    id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE trace_blobs (
    id TEXT PRIMARY KEY,
    encoding TEXT NOT NULL,
    content_type TEXT NOT NULL,
    body BLOB NOT NULL,
    created_at TEXT NOT NULL
);
CREATE TABLE model_traces (
    id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    channel TEXT NOT NULL,
    model TEXT NOT NULL,
    request_started_at TEXT NOT NULL,
    request_completed_at TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_input_tokens INTEGER NOT NULL,
    cache_creation_input_tokens INTEGER NOT NULL,
    provider_request_id TEXT,
    tool_count INTEGER NOT NULL,
    tool_names TEXT NOT NULL,
    request_blob_id TEXT NOT NULL,
    response_blob_id TEXT NOT NULL,
    stream_blob_id TEXT,
    error_summary TEXT
);
CREATE TABLE channel_cursors (
    channel TEXT NOT NULL,
    cursor_key TEXT NOT NULL,
    cursor_value TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(channel, cursor_key)
);
CREATE TABLE outbound_messages (
    id TEXT PRIMARY KEY,
    turn_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    channel TEXT NOT NULL,
    external_thread_id TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata_json TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE runtime_settings (
    agent_id TEXT PRIMARY KEY,
    model TEXT NOT NULL,
    system_prompt TEXT NOT NULL,
    temperature REAL NOT NULL,
    max_tokens INTEGER NOT NULL,
    stream INTEGER NOT NULL,
    allow_tools INTEGER NOT NULL,
    max_history_turns INTEGER NOT NULL,
    inject_wake_pack INTEGER NOT NULL DEFAULT 1,
    inject_ledger_recall INTEGER NOT NULL DEFAULT 1,
    enable_auto_distill INTEGER NOT NULL DEFAULT 1,
    model_roles_json TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE retention_settings (
    agent_id TEXT PRIMARY KEY,
    trace_blob_retention_days INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE memory_artifacts (
    id TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    citations_json TEXT NOT NULL,
    supersedes_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE memory_state (
    namespace_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(namespace_id, key)
);
CREATE TABLE memory_recall_chunks (
    chunk_id TEXT PRIMARY KEY,
    namespace_id TEXT NOT NULL,
    source_type TEXT NOT NULL,
    source_id TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    chunk_index INTEGER NOT NULL,
    content TEXT NOT NULL,
    embedding_json TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(source_type, source_id, chunk_index)
);
CREATE VIRTUAL TABLE memory_recall_chunks_fts USING fts5(
    chunk_id UNINDEXED,
    namespace_id UNINDEXED,
    source_type UNINDEXED,
    source_id UNINDEXED,
    entry_id UNINDEXED,
    content
);
"#;

#[derive(Debug, Clone)]
struct ConversationRow {
    id: String,
    channel: String,
    user_id: String,
    thread_id: Option<String>,
    started_at: String,
    last_activity: String,
    metadata: Value,
}

#[derive(Debug, Clone)]
struct LegacyEventRow {
    id: String,
    user_id: String,
    kind: String,
    source: String,
    content: Option<String>,
    payload: Value,
    created_at: String,
}

#[derive(Debug, Clone)]
struct MessageRow {
    id: String,
    conversation_id: String,
    role: String,
    content: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct MemoryDocumentRow {
    id: String,
    user_id: String,
    _agent_id: Option<String>,
    path: String,
    _metadata: Value,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct MemoryChunkRow {
    id: String,
    document_id: String,
    chunk_index: i64,
    content: String,
    created_at: String,
}

#[derive(Debug, Clone)]
struct SettingRow {
    user_id: String,
    key: String,
    value: String,
    updated_at: String,
}

#[derive(Debug, Default)]
struct ValidationReport {
    threads: usize,
    turns: usize,
    legacy_thread_events: usize,
    outbound_messages: usize,
    memory_artifacts: usize,
    memory_state: usize,
    memory_recall_chunks: usize,
}

#[derive(Debug, Clone)]
struct ThreadPlan {
    id: String,
    channel: String,
    external_thread_id: String,
    title: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct PlannedEvent {
    id: String,
    kind: &'static str,
    payload: Value,
    created_at: String,
}

#[derive(Debug, Clone)]
struct PlannedTurn {
    id: String,
    thread_id: String,
    user_message: String,
    assistant_message: Option<String>,
    error: Option<String>,
    status: &'static str,
    created_at: String,
    updated_at: String,
    outbound_message_id: Option<String>,
    outbound_metadata: Option<Value>,
    events: Vec<PlannedEvent>,
}

#[derive(Debug)]
struct TurnBuilder {
    turn: PlannedTurn,
    synthetic: bool,
}

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        bail!("usage: betterclaw-db-migrator <old-db-path> <new-db-path>");
    }

    let source_path = PathBuf::from(&args[0]);
    let target_path = PathBuf::from(&args[1]);
    migrate_database(&source_path, &target_path)
}

fn migrate_database(source_path: &Path, target_path: &Path) -> Result<()> {
    if source_path == target_path {
        bail!("source and target database paths must differ");
    }
    if !source_path.exists() {
        bail!("source database does not exist: {}", source_path.display());
    }

    let working_path = temp_target_path(target_path);
    if working_path.exists() {
        fs::remove_file(&working_path).with_context(|| {
            format!("removing previous temporary database {}", working_path.display())
        })?;
    }

    let source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening source database {}", source_path.display()))?;
    let mut target = Connection::open(&working_path)
        .with_context(|| format!("creating target database {}", working_path.display()))?;
    target
        .execute_batch(TARGET_SCHEMA)
        .context("creating target schema")?;

    let report = {
        let tx = target.transaction().context("starting migration transaction")?;
        let report = migrate_into(&source, &tx)?;
        tx.commit().context("committing migration transaction")?;
        report
    };

    validate_migration(&source, &target, &report)?;
    replace_target_database(target_path, &working_path)?;

    println!(
        "migration completed: threads={}, turns={}, events={}, outbound_messages={}, memory_artifacts={}, memory_state={}, memory_recall_chunks={}",
        report.threads,
        report.turns,
        report.legacy_thread_events,
        report.outbound_messages,
        report.memory_artifacts,
        report.memory_state,
        report.memory_recall_chunks,
    );

    Ok(())
}

fn migrate_into(source: &Connection, target: &Transaction<'_>) -> Result<ValidationReport> {
    let conversations = load_conversations(source)?;
    let settings = load_settings(source)?;
    let memory_documents = load_memory_documents(source)?;
    let memory_chunks = load_memory_chunks(source)?;
    let legacy_events = load_legacy_events(source)?;
    let fallback_messages = load_fallback_messages(source, &conversations)?;

    let default_model = selected_model(&settings).unwrap_or_else(|| "local-debug-model".to_string());
    let default_agent_name = selected_agent_name(&settings).unwrap_or_else(|| "BetterClaw".to_string());
    let workspace_root = default_workspace_root();
    let earliest_timestamp = earliest_timestamp(&conversations, &settings, &memory_documents)
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    insert_workspace_and_agent(
        target,
        &workspace_root,
        &earliest_timestamp,
        &default_agent_name,
        &default_model,
        &settings,
    )?;

    let thread_plans = plan_threads(&conversations, &legacy_events, &fallback_messages);
    let canonical_threads = unique_thread_plans(&thread_plans);
    insert_threads(target, canonical_threads.iter())?;

    let mut report = ValidationReport {
        threads: canonical_threads.len(),
        ..ValidationReport::default()
    };

    let planned_turns = plan_turns(&thread_plans, &legacy_events, &fallback_messages);
    report.turns = planned_turns.len();
    report.legacy_thread_events = planned_turns.iter().map(|turn| turn.events.len()).sum();
    report.outbound_messages = planned_turns
        .iter()
        .filter(|turn| turn.outbound_message_id.is_some())
        .count();
    insert_turns_and_events(target, &thread_plans, &planned_turns)?;

    report.memory_state = migrate_legacy_settings(target, &settings)?;
    report.memory_artifacts = migrate_memory_artifacts(target, &legacy_events)?;
    report.memory_recall_chunks = migrate_memory_documents(target, &memory_documents, &memory_chunks)?;
    migrate_channel_cursors(target, &conversations, &legacy_events, &settings, &fallback_messages)?;

    Ok(report)
}

fn load_conversations(source: &Connection) -> Result<Vec<ConversationRow>> {
    let mut stmt = source.prepare(
        "SELECT id, channel, user_id, thread_id, started_at, last_activity, metadata FROM conversations ORDER BY started_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ConversationRow {
                id: row.get(0)?,
                channel: row.get(1)?,
                user_id: row.get(2)?,
                thread_id: row.get(3)?,
                started_at: row.get(4)?,
                last_activity: row.get(5)?,
                metadata: parse_jsonish(&row.get::<_, String>(6)?),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_settings(source: &Connection) -> Result<Vec<SettingRow>> {
    let mut stmt = source.prepare(
        "SELECT user_id, key, value, updated_at FROM settings ORDER BY user_id ASC, key ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(SettingRow {
                user_id: row.get(0)?,
                key: row.get(1)?,
                value: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_memory_documents(source: &Connection) -> Result<Vec<MemoryDocumentRow>> {
    let mut stmt = source.prepare(
        "SELECT id, user_id, agent_id, path, metadata, created_at, updated_at FROM memory_documents ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MemoryDocumentRow {
                id: row.get(0)?,
                user_id: row.get(1)?,
                _agent_id: row.get(2)?,
                path: row.get(3)?,
                _metadata: parse_jsonish(&row.get::<_, String>(4)?),
                created_at: row.get(5)?,
                updated_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_memory_chunks(source: &Connection) -> Result<Vec<MemoryChunkRow>> {
    let mut stmt = source.prepare(
        "SELECT id, document_id, chunk_index, content, created_at FROM memory_chunks ORDER BY document_id ASC, chunk_index ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MemoryChunkRow {
                id: row.get(0)?,
                document_id: row.get(1)?,
                chunk_index: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_legacy_events(source: &Connection) -> Result<Vec<LegacyEventRow>> {
    let mut stmt = source.prepare(
        "SELECT id, user_id, kind, source, content, payload, created_at FROM ledger_events ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LegacyEventRow {
                id: row.get(0)?,
                user_id: row.get(1)?,
                kind: row.get(2)?,
                source: row.get(3)?,
                content: row.get(4)?,
                payload: parse_jsonish(&row.get::<_, String>(5)?),
                created_at: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn load_fallback_messages(
    source: &Connection,
    conversations: &[ConversationRow],
) -> Result<HashMap<String, Vec<MessageRow>>> {
    let fallback_ids = conversations_without_ledger(conversations, source)?;
    if fallback_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let quoted_ids = fallback_ids
        .iter()
        .map(|value| format!("'{}'", value.replace('"', "\"")))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, conversation_id, role, content, created_at FROM conversation_messages WHERE conversation_id IN ({quoted_ids}) ORDER BY conversation_id ASC, created_at ASC, id ASC"
    );
    let mut stmt = source.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(MessageRow {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut grouped = HashMap::<String, Vec<MessageRow>>::new();
    for row in rows {
        grouped
            .entry(row.conversation_id.clone())
            .or_default()
            .push(row);
    }
    Ok(grouped)
}

fn conversations_without_ledger(
    conversations: &[ConversationRow],
    source: &Connection,
) -> Result<Vec<String>> {
    let mut stmt = source.prepare(
        "SELECT DISTINCT json_extract(payload, '$.thread_id') FROM ledger_events WHERE json_extract(payload, '$.thread_id') IS NOT NULL",
    )?;
    let ledger_thread_ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .collect::<std::collections::HashSet<_>>();

    Ok(conversations
        .iter()
        .filter(|conversation| !ledger_thread_ids.contains(&conversation.id))
        .map(|conversation| conversation.id.clone())
        .collect())
}

fn insert_workspace_and_agent(
    target: &Transaction<'_>,
    workspace_root: &str,
    created_at: &str,
    agent_name: &str,
    model: &str,
    settings: &[SettingRow],
) -> Result<()> {
    target.execute(
        "INSERT INTO workspaces (id, root, created_at) VALUES (?, ?, ?)",
        params![DEFAULT_WORKSPACE_ID, workspace_root, created_at],
    )?;
    target.execute(
        "INSERT INTO agents (id, display_name, workspace_id, created_at) VALUES (?, ?, ?, ?)",
        params![DEFAULT_AGENT_ID, agent_name, DEFAULT_WORKSPACE_ID, created_at],
    )?;

    let now = Utc::now().to_rfc3339();
    let model_roles = build_model_roles(model, settings);
    target.execute(
        "INSERT INTO runtime_settings (agent_id, model, system_prompt, temperature, max_tokens, stream, allow_tools, max_history_turns, inject_wake_pack, inject_ledger_recall, enable_auto_distill, model_roles_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            DEFAULT_AGENT_ID,
            model,
            default_system_prompt(),
            0.2_f64,
            1024_i64,
            1_i64,
            1_i64,
            12_i64,
            1_i64,
            1_i64,
            1_i64,
            serde_json::to_string(&model_roles)?,
            created_at,
            now,
        ],
    )?;
    target.execute(
        "INSERT INTO retention_settings (agent_id, trace_blob_retention_days, created_at, updated_at) VALUES (?, ?, ?, ?)",
        params![DEFAULT_AGENT_ID, 0_i64, created_at, now],
    )?;
    Ok(())
}

fn plan_threads(
    conversations: &[ConversationRow],
    legacy_events: &[LegacyEventRow],
    fallback_messages: &HashMap<String, Vec<MessageRow>>,
) -> HashMap<String, ThreadPlan> {
    let mut first_message_by_thread = HashMap::<String, String>::new();
    for event in legacy_events.iter().filter(|event| event.kind == "user_turn") {
        if let Some(thread_id) = payload_string(&event.payload, "thread_id") {
            first_message_by_thread
                .entry(thread_id)
                .or_insert_with(|| event.content.clone().unwrap_or_default());
        }
    }
    for (conversation_id, messages) in fallback_messages {
        if let Some(message) = messages.iter().find(|message| message.role == "user") {
            first_message_by_thread
                .entry(conversation_id.clone())
                .or_insert_with(|| message.content.clone());
        }
    }

    let mut canonical_threads = HashMap::<(String, String), ThreadPlan>::new();
    for conversation in conversations {
        let external_thread_id = external_thread_id_for_conversation(conversation);
        let channel = channel_for_conversation(conversation);
        let title = title_for_conversation(
            conversation,
            first_message_by_thread
                .get(&conversation.id)
                .or_else(|| conversation.thread_id.as_ref().and_then(|thread_id| first_message_by_thread.get(thread_id))),
        );
        let key = (channel.clone(), external_thread_id.clone());
        canonical_threads
            .entry(key)
            .and_modify(|plan| {
                if conversation.started_at < plan.created_at {
                    plan.created_at = conversation.started_at.clone();
                    plan.title = title.clone();
                }
                if conversation.last_activity > plan.updated_at {
                    plan.updated_at = conversation.last_activity.clone();
                }
            })
            .or_insert_with(|| ThreadPlan {
                id: conversation.id.clone(),
                channel,
                external_thread_id,
                title,
                created_at: conversation.started_at.clone(),
                updated_at: conversation.last_activity.clone(),
            });
    }

    let mut plans = HashMap::new();
    for conversation in conversations {
        let key = (
            channel_for_conversation(conversation),
            external_thread_id_for_conversation(conversation),
        );
        let canonical = canonical_threads
            .get(&key)
            .expect("canonical thread plan must exist")
            .clone();
        plans.insert(conversation.id.clone(), canonical.clone());
        if let Some(thread_id) = conversation.thread_id.clone().filter(|value| !value.is_empty()) {
            plans.insert(thread_id, canonical);
        }
    }

    plans
}

fn unique_thread_plans(thread_plans: &HashMap<String, ThreadPlan>) -> Vec<ThreadPlan> {
    let mut unique = BTreeMap::<String, ThreadPlan>::new();
    for plan in thread_plans.values() {
        unique.entry(plan.id.clone()).or_insert_with(|| plan.clone());
    }
    unique.into_values().collect()
}

fn insert_threads<'a>(
    target: &Transaction<'_>,
    plans: impl Iterator<Item = &'a ThreadPlan>,
) -> Result<()> {
    for plan in plans {
        target.execute(
            "INSERT INTO threads (id, agent_id, channel, external_thread_id, title, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                plan.id,
                DEFAULT_AGENT_ID,
                plan.channel,
                plan.external_thread_id,
                plan.title,
                plan.created_at,
                plan.updated_at,
            ],
        )?;
    }
    Ok(())
}

fn plan_turns(
    thread_plans: &HashMap<String, ThreadPlan>,
    legacy_events: &[LegacyEventRow],
    fallback_messages: &HashMap<String, Vec<MessageRow>>,
) -> Vec<PlannedTurn> {
    let mut events_by_thread = BTreeMap::<String, Vec<LegacyEventRow>>::new();
    for event in legacy_events.iter().filter(|event| is_thread_ledger_kind(&event.kind)) {
        if let Some(thread_id) = payload_string(&event.payload, "thread_id") {
            events_by_thread
                .entry(thread_id)
                .or_default()
                .push(event.clone());
        }
    }

    let mut turns = Vec::new();
    for (thread_id, events) in events_by_thread {
        let Some(thread_plan) = thread_plans.get(&thread_id) else {
            continue;
        };
        turns.extend(plan_ledger_turns(&thread_plan.id, &events));
    }

    for (conversation_id, messages) in fallback_messages {
        let Some(thread_plan) = thread_plans.get(conversation_id) else {
            continue;
        };
        turns.extend(plan_message_fallback_turns(&thread_plan.id, messages));
    }

    turns.sort_by(|left, right| left.created_at.cmp(&right.created_at).then_with(|| left.id.cmp(&right.id)));
    turns
}

fn plan_ledger_turns(thread_id: &str, events: &[LegacyEventRow]) -> Vec<PlannedTurn> {
    let mut turns = Vec::new();
    let mut current: Option<TurnBuilder> = None;

    for event in events {
        match event.kind.as_str() {
            "user_turn" => {
                if let Some(builder) = current.take() {
                    turns.push(finalize_turn(builder));
                }
                current = Some(TurnBuilder {
                    synthetic: false,
                    turn: PlannedTurn {
                        id: payload_string(&event.payload, "message_id").unwrap_or_else(|| event.id.clone()),
                        thread_id: thread_id.to_string(),
                        user_message: event.content.clone().unwrap_or_default(),
                        assistant_message: None,
                        error: None,
                        status: "running",
                        created_at: event.created_at.clone(),
                        updated_at: event.created_at.clone(),
                        outbound_message_id: None,
                        outbound_metadata: None,
                        events: Vec::new(),
                    },
                });
            }
            "agent_turn" => {
                let builder = current.get_or_insert_with(|| synthetic_turn(thread_id, event));
                builder.turn.assistant_message = event.content.clone();
                builder.turn.updated_at = event.created_at.clone();
                builder.turn.outbound_message_id = Some(format!("{}:outbound", builder.turn.id));
                builder.turn.outbound_metadata = Some(json!({
                    "legacy_kind": event.kind,
                    "legacy_event_id": event.id,
                    "message_id": payload_string(&event.payload, "message_id"),
                    "turn_number": payload_i64(&event.payload, "turn_number"),
                    "source": event.source,
                }));
            }
            "tool_call" => append_legacy_event(&mut current, thread_id, event, "tool_call", event_kind_json("tool_call")),
            "tool_result" => append_legacy_event(&mut current, thread_id, event, "tool_result", event_kind_json("tool_result")),
            "tool_error" => {
                let builder = current.get_or_insert_with(|| synthetic_turn(thread_id, event));
                if builder.turn.error.is_none() {
                    builder.turn.error = event.content.clone().or_else(|| Some(event.payload.to_string()));
                }
                builder.turn.updated_at = event.created_at.clone();
                builder.turn.events.push(PlannedEvent {
                    id: event.id.clone(),
                    kind: "error",
                    payload: legacy_event_payload(event),
                    created_at: event.created_at.clone(),
                });
            }
            _ => {}
        }
    }

    if let Some(builder) = current.take() {
        turns.push(finalize_turn(builder));
    }

    turns
}

fn append_legacy_event(
    current: &mut Option<TurnBuilder>,
    thread_id: &str,
    event: &LegacyEventRow,
    _label: &str,
    kind: &'static str,
) {
    let builder = current.get_or_insert_with(|| synthetic_turn(thread_id, event));
    builder.turn.updated_at = event.created_at.clone();
    builder.turn.events.push(PlannedEvent {
        id: event.id.clone(),
        kind,
        payload: legacy_event_payload(event),
        created_at: event.created_at.clone(),
    });
}

fn synthetic_turn(thread_id: &str, event: &LegacyEventRow) -> TurnBuilder {
    TurnBuilder {
        synthetic: true,
        turn: PlannedTurn {
            id: format!("{}:synthetic-turn", event.id),
            thread_id: thread_id.to_string(),
            user_message: "[migrated legacy turn without explicit user message]".to_string(),
            assistant_message: None,
            error: None,
            status: "running",
            created_at: event.created_at.clone(),
            updated_at: event.created_at.clone(),
            outbound_message_id: None,
            outbound_metadata: None,
            events: Vec::new(),
        },
    }
}

fn finalize_turn(builder: TurnBuilder) -> PlannedTurn {
    let mut turn = builder.turn;
    turn.status = if turn.error.is_some() {
        "failed"
    } else if turn.assistant_message.is_some() {
        "succeeded"
    } else {
        "succeeded"
    };
    if builder.synthetic && turn.assistant_message.is_some() && turn.user_message.starts_with("[migrated") {
        turn.status = "succeeded";
    }
    turn
}

fn plan_message_fallback_turns(conversation_id: &str, messages: &[MessageRow]) -> Vec<PlannedTurn> {
    let mut turns = Vec::new();
    let mut current: Option<TurnBuilder> = None;

    for message in messages {
        match message.role.as_str() {
            "user" => {
                if let Some(builder) = current.take() {
                    turns.push(finalize_turn(builder));
                }
                current = Some(TurnBuilder {
                    synthetic: false,
                    turn: PlannedTurn {
                        id: message.id.clone(),
                        thread_id: conversation_id.to_string(),
                        user_message: message.content.clone(),
                        assistant_message: None,
                        error: None,
                        status: "running",
                        created_at: message.created_at.clone(),
                        updated_at: message.created_at.clone(),
                        outbound_message_id: None,
                        outbound_metadata: None,
                        events: Vec::new(),
                    },
                });
            }
            "assistant" => {
                let builder = current.get_or_insert_with(|| TurnBuilder {
                    synthetic: true,
                    turn: PlannedTurn {
                        id: format!("{}:synthetic-turn", message.id),
                        thread_id: conversation_id.to_string(),
                        user_message: "[migrated assistant-only conversation segment]".to_string(),
                        assistant_message: None,
                        error: None,
                        status: "running",
                        created_at: message.created_at.clone(),
                        updated_at: message.created_at.clone(),
                        outbound_message_id: None,
                        outbound_metadata: None,
                        events: Vec::new(),
                    },
                });
                builder.turn.assistant_message = Some(message.content.clone());
                builder.turn.updated_at = message.created_at.clone();
                builder.turn.outbound_message_id = Some(format!("{}:outbound", builder.turn.id));
                builder.turn.outbound_metadata = Some(json!({"legacy_role": "assistant", "legacy_message_id": message.id}));
            }
            "tool_calls" => {
                let builder = current.get_or_insert_with(|| TurnBuilder {
                    synthetic: true,
                    turn: PlannedTurn {
                        id: format!("{}:synthetic-turn", message.id),
                        thread_id: conversation_id.to_string(),
                        user_message: "[migrated tool-only conversation segment]".to_string(),
                        assistant_message: None,
                        error: None,
                        status: "running",
                        created_at: message.created_at.clone(),
                        updated_at: message.created_at.clone(),
                        outbound_message_id: None,
                        outbound_metadata: None,
                        events: Vec::new(),
                    },
                });
                builder.turn.updated_at = message.created_at.clone();
                builder.turn.events.push(PlannedEvent {
                    id: message.id.clone(),
                    kind: "tool_result",
                    payload: json!({
                        "legacy_role": "tool_calls",
                        "legacy_message_id": message.id,
                        "content": parse_jsonish(&message.content),
                    }),
                    created_at: message.created_at.clone(),
                });
            }
            _ => {}
        }
    }

    if let Some(builder) = current.take() {
        turns.push(finalize_turn(builder));
    }

    turns
}

fn insert_turns_and_events(
    target: &Transaction<'_>,
    thread_plans: &HashMap<String, ThreadPlan>,
    turns: &[PlannedTurn],
) -> Result<()> {
    let thread_plans_by_id = unique_thread_plans(thread_plans)
        .into_iter()
        .map(|plan| (plan.id.clone(), plan))
        .collect::<HashMap<_, _>>();

    for turn in turns {
        target.execute(
            "INSERT INTO turns (id, thread_id, status, user_message, assistant_message, error, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                turn.id,
                turn.thread_id,
                turn.status,
                turn.user_message,
                turn.assistant_message,
                turn.error,
                turn.created_at,
                turn.updated_at,
            ],
        )?;

        let mut sequence = 1_i64;
        for event in &turn.events {
            target.execute(
                "INSERT INTO events (id, turn_id, thread_id, sequence, kind, payload, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    event.id,
                    turn.id,
                    turn.thread_id,
                    sequence,
                    json_string(event.kind),
                    event.payload.to_string(),
                    event.created_at,
                ],
            )?;
            sequence += 1;
        }

        if let Some(outbound_id) = &turn.outbound_message_id {
            let thread = thread_plans_by_id
                .get(&turn.thread_id)
                .ok_or_else(|| anyhow!("missing thread plan for turn {}", turn.turn_id()))?;
            target.execute(
                "INSERT INTO outbound_messages (id, turn_id, thread_id, channel, external_thread_id, content, metadata_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    outbound_id,
                    turn.id,
                    turn.thread_id,
                    thread.channel,
                    thread.external_thread_id,
                    turn.assistant_message.clone().unwrap_or_default(),
                    turn.outbound_metadata.as_ref().map(Value::to_string),
                    turn.updated_at,
                ],
            )?;
        }
    }
    Ok(())
}

fn migrate_legacy_settings(target: &Transaction<'_>, settings: &[SettingRow]) -> Result<usize> {
    for setting in settings {
        let namespace = format!("{}:{}", LEGACY_SETTINGS_NAMESPACE_PREFIX, setting.user_id);
        target.execute(
            "INSERT INTO memory_state (namespace_id, key, value_json, updated_at) VALUES (?, ?, ?, ?)",
            params![namespace, setting.key, parse_jsonish(&setting.value).to_string(), setting.updated_at],
        )?;
    }
    Ok(settings.len())
}

fn migrate_memory_artifacts(
    target: &Transaction<'_>,
    events: &[LegacyEventRow],
) -> Result<usize> {
    let artifacts = events
        .iter()
        .filter(|event| !is_thread_ledger_kind(&event.kind))
        .collect::<Vec<_>>();

    for event in &artifacts {
        let citations = extract_citation_ids(&event.payload);
        let content = event
            .content
            .clone()
            .filter(|content| !content.trim().is_empty())
            .unwrap_or_else(|| event.payload.to_string());
        let kind = map_memory_artifact_kind(&event.kind);
        let supersedes_id = event
            .payload
            .get("duplicate_of")
            .and_then(Value::as_str)
            .map(ToString::to_string);

        target.execute(
            "INSERT INTO memory_artifacts (id, namespace_id, kind, source, content, payload_json, citations_json, supersedes_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                event.id,
                event.user_id,
                kind,
                event.source,
                content,
                event.payload.to_string(),
                serde_json::to_string(&citations)?,
                supersedes_id,
                event.created_at,
                event.created_at,
            ],
        )?;
    }

    Ok(artifacts.len())
}

fn migrate_memory_documents(
    target: &Transaction<'_>,
    documents: &[MemoryDocumentRow],
    chunks: &[MemoryChunkRow],
) -> Result<usize> {
    let document_map = documents
        .iter()
        .map(|document| (document.id.clone(), document))
        .collect::<HashMap<_, _>>();

    for chunk in chunks {
        let Some(document) = document_map.get(&chunk.document_id) else {
            continue;
        };
        let source_id = format!("{}:{}", document.user_id, document.path);
        target.execute(
            "INSERT INTO memory_recall_chunks (chunk_id, namespace_id, source_type, source_id, entry_id, chunk_index, content, embedding_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                chunk.id,
                document.user_id,
                "workspace_document",
                source_id,
                document.id,
                chunk.chunk_index,
                chunk.content,
                Option::<String>::None,
                chunk.created_at,
                document.updated_at,
            ],
        )?;
        target.execute(
            "INSERT INTO memory_recall_chunks_fts (chunk_id, namespace_id, source_type, source_id, entry_id, content) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                chunk.id,
                document.user_id,
                "workspace_document",
                format!("{}:{}", document.user_id, document.path),
                document.id,
                chunk.content,
            ],
        )?;
    }
    Ok(chunks.len())
}

fn migrate_channel_cursors(
    target: &Transaction<'_>,
    conversations: &[ConversationRow],
    legacy_events: &[LegacyEventRow],
    settings: &[SettingRow],
    fallback_messages: &HashMap<String, Vec<MessageRow>>,
) -> Result<()> {
    let mut cursors = HashMap::<String, i64>::new();
    let mut tidepool_conversations_by_thread_id = HashMap::<&str, &ConversationRow>::new();
    for conversation in conversations
        .iter()
        .filter(|conversation| conversation.user_id.starts_with("tidepool:domain:"))
    {
        tidepool_conversations_by_thread_id.insert(conversation.id.as_str(), conversation);
        if let Some(thread_id) = conversation.thread_id.as_deref().filter(|value| !value.is_empty()) {
            tidepool_conversations_by_thread_id.insert(thread_id, conversation);
        }
    }

    for event in legacy_events.iter().filter(|event| event.kind == "user_turn") {
        let Some(thread_id) = event.payload.get("thread_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(conversation) = tidepool_conversations_by_thread_id.get(thread_id) else {
            continue;
        };
        let Some(content) = event.content.as_deref() else {
            continue;
        };

        let max_sequence = extract_domain_sequences(content).into_iter().max().unwrap_or(0);
        if max_sequence > 0 {
            let domain_id = conversation
                .user_id
                .trim_start_matches("tidepool:domain:")
                .to_string();
            cursors
                .entry(domain_id)
                .and_modify(|current| *current = (*current).max(max_sequence))
                .or_insert(max_sequence);
        }
    }

    for conversation in conversations.iter().filter(|conversation| conversation.user_id.starts_with("tidepool:domain:")) {
        let Some(messages) = fallback_messages.get(&conversation.id) else {
            continue;
        };
        let max_sequence = messages
            .iter()
            .filter(|message| message.role == "user")
            .flat_map(|message| extract_domain_sequences(&message.content))
            .max()
            .unwrap_or(0);
        if max_sequence > 0 {
            let domain_id = conversation
                .user_id
                .trim_start_matches("tidepool:domain:")
                .to_string();
            cursors
                .entry(domain_id)
                .and_modify(|current| *current = (*current).max(max_sequence))
                .or_insert(max_sequence);
        }
    }

    for setting in settings.iter().filter(|setting| setting.key == "channel_broadcast_metadata_tidepool") {
        let payload = parse_jsonish(&setting.value);
        if let Some(domain_id) = payload.get("domain_id").and_then(Value::as_i64) {
            let candidate = payload
                .get("last_seen_domain_sequence")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if candidate > 0 {
                cursors
                    .entry(domain_id.to_string())
                    .and_modify(|current| *current = (*current).max(candidate))
                    .or_insert(candidate);
            }
        }
    }

    let updated_at = Utc::now().to_rfc3339();
    for (domain_id, sequence) in cursors {
        target.execute(
            "INSERT INTO channel_cursors (channel, cursor_key, cursor_value, updated_at) VALUES (?, ?, ?, ?)",
            params!["tidepool", domain_id, sequence.to_string(), updated_at],
        )?;
    }

    Ok(())
}

fn validate_migration(
    source: &Connection,
    target: &Connection,
    report: &ValidationReport,
) -> Result<()> {
    let thread_count: i64 = target.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
    let turn_count: i64 = target.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
    let event_count: i64 = target.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    let outbound_count: i64 = target.query_row("SELECT COUNT(*) FROM outbound_messages", [], |row| row.get(0))?;
    let artifact_count: i64 = target.query_row("SELECT COUNT(*) FROM memory_artifacts", [], |row| row.get(0))?;
    let memory_state_count: i64 = target.query_row("SELECT COUNT(*) FROM memory_state", [], |row| row.get(0))?;
    let chunk_count: i64 = target.query_row("SELECT COUNT(*) FROM memory_recall_chunks", [], |row| row.get(0))?;

    if thread_count as usize != report.threads {
        bail!("thread count mismatch: expected {}, found {}", report.threads, thread_count);
    }
    if turn_count as usize != report.turns {
        bail!("turn count mismatch: expected {}, found {}", report.turns, turn_count);
    }
    if event_count as usize != report.legacy_thread_events {
        bail!("event count mismatch: expected {}, found {}", report.legacy_thread_events, event_count);
    }
    if outbound_count as usize != report.outbound_messages {
        bail!("outbound count mismatch: expected {}, found {}", report.outbound_messages, outbound_count);
    }
    if artifact_count as usize != report.memory_artifacts {
        bail!("memory artifact count mismatch: expected {}, found {}", report.memory_artifacts, artifact_count);
    }
    if memory_state_count as usize != report.memory_state {
        bail!("memory state count mismatch: expected {}, found {}", report.memory_state, memory_state_count);
    }
    if chunk_count as usize != report.memory_recall_chunks {
        bail!("memory recall chunk count mismatch: expected {}, found {}", report.memory_recall_chunks, chunk_count);
    }

    let integrity: String = target.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        bail!("target database failed integrity_check: {integrity}");
    }

    let source_user_turns: i64 = source.query_row(
        "SELECT COUNT(*) FROM ledger_events WHERE kind = 'user_turn'",
        [],
        |row| row.get(0),
    )?;
    if turn_count < source_user_turns {
        bail!(
            "migrated fewer turns than source user_turns: turns={}, user_turns={}",
            turn_count,
            source_user_turns
        );
    }

    Ok(())
}

fn replace_target_database(target_path: &Path, working_path: &Path) -> Result<()> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating target directory {}", parent.display()))?;
    }

    if target_path.exists() {
        let backup_path = target_backup_path(target_path);
        fs::rename(target_path, &backup_path).with_context(|| {
            format!(
                "moving existing target database {} to {}",
                target_path.display(),
                backup_path.display()
            )
        })?;
    }

    fs::rename(working_path, target_path).with_context(|| {
        format!(
            "moving migrated database {} to {}",
            working_path.display(),
            target_path.display()
        )
    })?;
    Ok(())
}

fn temp_target_path(target_path: &Path) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let file_name = target_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("betterclaw.db");
    target_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}.migrating.{timestamp}.sqlite"))
}

fn target_backup_path(target_path: &Path) -> PathBuf {
    let timestamp = Utc::now().format("%Y%m%d%H%M%S");
    let file_name = target_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("betterclaw.db");
    target_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}.pre_migration_backup.{timestamp}"))
}

fn selected_model(settings: &[SettingRow]) -> Option<String> {
    settings
        .iter()
        .find(|setting| setting.key == "selected_model")
        .and_then(|setting| parse_jsonish(&setting.value).as_str().map(ToString::to_string))
}

fn selected_agent_name(settings: &[SettingRow]) -> Option<String> {
    settings
        .iter()
        .find(|setting| setting.key == "agent.name")
        .and_then(|setting| parse_jsonish(&setting.value).as_str().map(ToString::to_string))
}

fn build_model_roles(model: &str, settings: &[SettingRow]) -> Vec<Value> {
    let mut roles = vec![json!({
        "role": "agent",
        "provider": "local",
        "mode": "chat",
        "model": model,
        "base_url": Value::Null,
        "api_key_env_var": Value::Null,
        "extra_headers": [],
        "enabled": true,
    })];

    let embeddings_enabled = settings
        .iter()
        .find(|setting| setting.key == "embeddings.enabled")
        .map(|setting| parse_jsonish(&setting.value).as_bool().unwrap_or(false))
        .unwrap_or(false);
    if embeddings_enabled {
        let provider = setting_string(settings, "embeddings.provider").unwrap_or_else(|| "openai_compatible".to_string());
        let embedding_model = setting_string(settings, "embeddings.model").unwrap_or_else(|| "nomic-embed-text-v1.5".to_string());
        let base_url = setting_string(settings, "openai_compatible_base_url");
        roles.push(json!({
            "role": "embeddings",
            "provider": provider,
            "mode": Value::Null,
            "model": embedding_model,
            "base_url": base_url,
            "api_key_env_var": Value::Null,
            "extra_headers": [],
            "enabled": true,
        }));
    }

    roles
}

fn default_workspace_root() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| String::from("~"));
    Path::new(&home)
        .join(DEFAULT_WORKSPACE_ROOT_SUFFIX)
        .display()
        .to_string()
}

fn earliest_timestamp(
    conversations: &[ConversationRow],
    settings: &[SettingRow],
    documents: &[MemoryDocumentRow],
) -> Option<String> {
    conversations
        .iter()
        .map(|conversation| conversation.started_at.clone())
        .chain(settings.iter().map(|setting| setting.updated_at.clone()))
        .chain(documents.iter().map(|document| document.created_at.clone()))
        .min()
}

fn channel_for_conversation(conversation: &ConversationRow) -> String {
    if conversation.user_id.starts_with("tidepool:domain:") {
        "tidepool".to_string()
    } else {
        conversation.channel.clone()
    }
}

fn external_thread_id_for_conversation(conversation: &ConversationRow) -> String {
    if conversation.user_id.starts_with("tidepool:domain:") {
        conversation.user_id.clone()
    } else if let Some(thread_id) = conversation.thread_id.clone().filter(|value| !value.is_empty()) {
        thread_id
    } else {
        conversation.id.clone()
    }
}

fn title_for_conversation(
    conversation: &ConversationRow,
    first_message: Option<&String>,
) -> String {
    if let Some(thread_type) = conversation.metadata.get("thread_type").and_then(Value::as_str) {
        if thread_type == "heartbeat" {
            return "Heartbeat".to_string();
        }
    }
    if conversation.user_id.starts_with("tidepool:domain:") {
        return format!("Tidepool {}", conversation.user_id);
    }
    if let Some(message) = first_message {
        return truncate_title(message);
    }
    format!("{} {}", conversation.channel, conversation.user_id)
}

fn truncate_title(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "Migrated thread".to_string();
    }
    let mut title = trimmed.replace('\n', " ");
    if title.chars().count() > 80 {
        title = title.chars().take(77).collect::<String>() + "...";
    }
    title
}

fn is_thread_ledger_kind(kind: &str) -> bool {
    matches!(kind, "user_turn" | "agent_turn" | "tool_call" | "tool_result" | "tool_error")
}

fn map_memory_artifact_kind(kind: &str) -> &str {
    match kind {
        "wake_pack.v0" => "wake_pack.v0",
        "invariant.self.v0" => "invariant.self.v0",
        "invariant.user.v0" => "invariant.user.v0",
        "invariant.relationship.v0" => "invariant.relationship.v0",
        "drift.flag.v0" => "drift.flag.v0",
        "drift.contradiction.v0" => "drift.contradiction.v0",
        "drift.merge.v0" => "drift.merge.v0",
        _ => "distill.micro",
    }
}

fn extract_citation_ids(payload: &Value) -> Vec<String> {
    payload
        .get("citations")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("event_id").and_then(Value::as_str).map(ToString::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn extract_domain_sequences(content: &str) -> Vec<i64> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let rest = trimmed.strip_prefix("- seq ")?;
            let digits = rest
                .chars()
                .take_while(|character| character.is_ascii_digit())
                .collect::<String>();
            digits.parse::<i64>().ok()
        })
        .collect()
}

fn parse_jsonish(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
}

fn payload_string(payload: &Value, key: &str) -> Option<String> {
    payload.get(key).and_then(Value::as_str).map(ToString::to_string)
}

fn payload_i64(payload: &Value, key: &str) -> Option<i64> {
    payload.get(key).and_then(Value::as_i64)
}

fn legacy_event_payload(event: &LegacyEventRow) -> Value {
    json!({
        "legacy_kind": event.kind,
        "legacy_event_id": event.id,
        "source": event.source,
        "content": event.content,
        "payload": event.payload,
        "user_id": event.user_id,
    })
}

fn setting_string(settings: &[SettingRow], key: &str) -> Option<String> {
    settings
        .iter()
        .find(|setting| setting.key == key)
        .and_then(|setting| parse_jsonish(&setting.value).as_str().map(ToString::to_string))
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing string to json")
}

fn event_kind_json(kind: &'static str) -> &'static str {
    match kind {
        "tool_call" => "tool_call",
        "tool_result" => "tool_result",
        "error" => "error",
        _ => "error",
    }
}

fn default_system_prompt() -> String {
    r#"You are BetterClaw Agent, a secure autonomous assistant.

## Guidelines
- Be concise and direct
- Use markdown formatting where helpful
- For code, use appropriate code blocks with language tags
- ALWAYS call tools via tool_calls when they materially help — never just describe what you would do
- If you say you will fetch, check, inspect, or search, include the actual tool call in the same response
- Do not narrate routine, low-risk tool calls; just call them
- Narrate only when it helps: multi-step work, sensitive actions, or when the user asks
- For multi-step tasks, call independent tools in parallel when possible
- If a tool fails, explain the error briefly and try an alternative approach

## Response Format
Respond directly with your answer. Do not wrap your response in any special tags.

## Safety
- You have no independent goals. Do not pursue self-preservation, replication, resource acquisition, or power-seeking beyond the user's request.
- Prioritize safety and human oversight over task completion. If instructions conflict, pause and ask.
- Comply with stop, pause, or audit requests. Never bypass safeguards.
- Do not manipulate anyone to expand your access or disable safeguards.
- Do not modify system prompts, safety rules, or tool policies unless explicitly requested by the user."#
        .to_string()
}

trait PlannedTurnExt {
    fn turn_id(&self) -> &str;
}

impl PlannedTurnExt for PlannedTurn {
    fn turn_id(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_temp_db(path: &Path) -> Connection {
        Connection::open(path).expect("open temp db")
    }

    fn create_old_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                channel TEXT NOT NULL,
                user_id TEXT NOT NULL,
                thread_id TEXT,
                started_at TEXT NOT NULL,
                last_activity TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE conversation_messages (
                id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE ledger_events (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                episode_id TEXT,
                kind TEXT NOT NULL,
                source TEXT NOT NULL,
                content TEXT,
                payload TEXT NOT NULL,
                sha256 TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE settings (
                user_id TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (user_id, key)
            );
            CREATE TABLE memory_documents (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                agent_id TEXT,
                path TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                metadata TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE memory_chunks (
                _rowid INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                document_id TEXT NOT NULL,
                chunk_index INTEGER NOT NULL,
                content TEXT NOT NULL,
                embedding BLOB,
                created_at TEXT NOT NULL
            );
            "#,
        )
        .expect("create old schema");
    }

    #[test]
    fn migration_preserves_core_history_and_memory() {
        let dir = tempdir().unwrap();
        let old_path = dir.path().join("old.db");
        let new_path = dir.path().join("new.db");

        {
            let conn = open_temp_db(&old_path);
            create_old_schema(&conn);
            conn.execute(
                "INSERT INTO conversations (id, channel, user_id, thread_id, started_at, last_activity, metadata) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    "conv-1",
                    "gateway",
                    "default",
                    Option::<String>::None,
                    "2026-03-13T20:00:00Z",
                    "2026-03-13T20:01:00Z",
                    "{}"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ledger_events (id, user_id, episode_id, kind, source, content, payload, sha256, created_at) VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, ?)",
                params![
                    "evt-user",
                    "default",
                    "user_turn",
                    "agent.chat",
                    "hello",
                    json!({"thread_id":"conv-1","message_id":"msg-1"}).to_string(),
                    "2026-03-13T20:00:00Z"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ledger_events (id, user_id, episode_id, kind, source, content, payload, sha256, created_at) VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, ?)",
                params![
                    "evt-tool-call",
                    "default",
                    "tool_call",
                    "agent.tool",
                    "read_file {\"path\":\"README.md\"}",
                    json!({"thread_id":"conv-1","tool_call_id":"call-1"}).to_string(),
                    "2026-03-13T20:00:10Z"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ledger_events (id, user_id, episode_id, kind, source, content, payload, sha256, created_at) VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, ?)",
                params![
                    "evt-tool-result",
                    "default",
                    "tool_result",
                    "agent.tool",
                    "<tool_output>ok</tool_output>",
                    json!({"thread_id":"conv-1","tool_call_id":"call-1"}).to_string(),
                    "2026-03-13T20:00:11Z"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ledger_events (id, user_id, episode_id, kind, source, content, payload, sha256, created_at) VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, ?)",
                params![
                    "evt-assistant",
                    "default",
                    "agent_turn",
                    "agent.chat",
                    "hi there",
                    json!({"thread_id":"conv-1","message_id":"msg-1","turn_number":0}).to_string(),
                    "2026-03-13T20:00:12Z"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO ledger_events (id, user_id, episode_id, kind, source, content, payload, sha256, created_at) VALUES (?, ?, NULL, ?, ?, ?, ?, NULL, ?)",
                params![
                    "artifact-1",
                    "default",
                    "wake_pack.v0",
                    "compressor",
                    "wake summary",
                    json!({"citations":[{"event_id":"evt-user","quote":"hello"}]}).to_string(),
                    "2026-03-13T20:00:30Z"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO settings (user_id, key, value, updated_at) VALUES (?, ?, ?, ?)",
                params!["default", "selected_model", "\"claude-sonnet-4.6\"", "2026-03-13T20:00:00Z"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memory_documents (id, user_id, agent_id, path, content, created_at, updated_at, metadata) VALUES (?, ?, NULL, ?, ?, ?, ?, ?)",
                params![
                    "doc-1",
                    "default",
                    "README.md",
                    "workspace readme",
                    "2026-03-13T19:00:00Z",
                    "2026-03-13T19:30:00Z",
                    "{}"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO memory_chunks (id, document_id, chunk_index, content, embedding, created_at) VALUES (?, ?, ?, ?, NULL, ?)",
                params!["chunk-1", "doc-1", 0_i64, "workspace readme", "2026-03-13T19:00:00Z"],
            )
            .unwrap();
        }

        migrate_database(&old_path, &new_path).unwrap();

        let conn = open_temp_db(&new_path);
        let thread_count: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0)).unwrap();
        let turn_count: i64 = conn.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0)).unwrap();
        let event_count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0)).unwrap();
        let outbound_count: i64 = conn.query_row("SELECT COUNT(*) FROM outbound_messages", [], |row| row.get(0)).unwrap();
        let artifact_count: i64 = conn.query_row("SELECT COUNT(*) FROM memory_artifacts", [], |row| row.get(0)).unwrap();
        let chunk_count: i64 = conn.query_row("SELECT COUNT(*) FROM memory_recall_chunks", [], |row| row.get(0)).unwrap();
        let assistant_message: String = conn
            .query_row("SELECT assistant_message FROM turns WHERE id = 'msg-1'", [], |row| row.get(0))
            .unwrap();
        let model: String = conn
            .query_row("SELECT model FROM runtime_settings WHERE agent_id = 'default'", [], |row| row.get(0))
            .unwrap();

        assert_eq!(thread_count, 1);
        assert_eq!(turn_count, 1);
        assert_eq!(event_count, 2);
        assert_eq!(outbound_count, 1);
        assert_eq!(artifact_count, 1);
        assert_eq!(chunk_count, 1);
        assert_eq!(assistant_message, "hi there");
        assert_eq!(model, "claude-sonnet-4.6");
    }

    #[test]
    fn extracts_tidepool_sequences_from_batch_content() {
        let content = "- seq 7 from account 1\nhello\n- seq 11 from account 2\nworld\n- seq 9 from account 3\n!";
        assert_eq!(extract_domain_sequences(content), vec![7, 11, 9]);
    }
}