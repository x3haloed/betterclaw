use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use anyhow::anyhow;
use tokio_stream::wrappers::BroadcastStream;

use crate::channel::InboundEvent;
use crate::error::RuntimeError;
use crate::runtime::{Runtime, RuntimeUpdate};
use crate::settings::ModelRoleConfig;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

pub fn app(runtime: Arc<Runtime>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route(
            "/api/settings/runtime",
            get(get_runtime_settings).put(update_runtime_settings),
        )
        .route(
            "/api/settings/retention",
            get(get_retention_settings).put(update_retention_settings),
        )
        .route("/api/runtime/recover", post(recover_runtime))
        .route("/api/runtime/prune-traces", post(prune_trace_blobs))
        .route("/api/threads", get(list_threads).post(create_thread))
        .route("/api/threads/{thread_id}", get(get_thread))
        .route("/api/threads/{thread_id}/messages", post(post_message))
        .route("/api/threads/{thread_id}/stream", get(stream_thread))
        .route("/api/threads/{thread_id}/timeline", get(get_timeline))
        .route(
            "/api/threads/{thread_id}/trace-details",
            get(get_thread_trace_details),
        )
        .route("/api/turns/{turn_id}/traces", get(get_turn_traces))
        .route("/api/turns/{turn_id}/replay", post(replay_turn))
        .route("/api/traces/{trace_id}", get(get_trace))
        .route("/api/runtime/check-update", get(check_update))
        .route("/api/runtime/self-update", post(self_update))
        .route("/health", get(health))
        .route("/api/status", get(api_status))
        .with_state(runtime)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        APP_JS,
    )
}

async fn style_css() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/css")], STYLE_CSS)
}

async fn list_threads(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<Vec<crate::thread::Thread>>, ApiError> {
    Ok(Json(runtime.list_threads().await?))
}

async fn get_runtime_settings(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<crate::settings::RuntimeSettings>, ApiError> {
    Ok(Json(runtime.get_runtime_settings("default").await?))
}

#[derive(Debug, Deserialize)]
struct UpdateRuntimeSettingsRequest {
    model: String,
    system_prompt: String,
    max_tokens: u32,
    stream: bool,
    allow_tools: bool,
    max_history_turns: u32,
    #[serde(default)]
    inject_wake_pack: Option<bool>,
    #[serde(default)]
    inject_ledger_recall: Option<bool>,
    #[serde(default)]
    enable_auto_distill: Option<bool>,
    #[serde(default)]
    enable_observations: Option<bool>,
    #[serde(default)]
    inject_observations: Option<bool>,
    #[serde(default)]
    inject_skills: Option<bool>,
    #[serde(default)]
    model_roles: Option<Vec<ModelRoleConfig>>,
}

async fn update_runtime_settings(
    State(runtime): State<Arc<Runtime>>,
    Json(payload): Json<UpdateRuntimeSettingsRequest>,
) -> Result<Json<crate::settings::RuntimeSettings>, ApiError> {
    let current = runtime.get_runtime_settings("default").await?;
    let updated = crate::settings::RuntimeSettings {
        agent_id: current.agent_id,
        model: payload.model,
        system_prompt: payload.system_prompt,
        max_tokens: payload.max_tokens,
        stream: payload.stream,
        allow_tools: payload.allow_tools,
        max_history_turns: payload.max_history_turns,
        inject_wake_pack: payload.inject_wake_pack.unwrap_or(current.inject_wake_pack),
        inject_ledger_recall: payload
            .inject_ledger_recall
            .unwrap_or(current.inject_ledger_recall),
        enable_auto_distill: payload
            .enable_auto_distill
            .unwrap_or(current.enable_auto_distill),
        enable_observations: payload
            .enable_observations
            .unwrap_or(current.enable_observations),
        inject_observations: payload
            .inject_observations
            .unwrap_or(current.inject_observations),
        inject_skills: payload
            .inject_skills
            .unwrap_or(current.inject_skills),
        model_roles: payload.model_roles.unwrap_or(current.model_roles),
        created_at: current.created_at,
        updated_at: current.updated_at,
    };
    Ok(Json(runtime.update_runtime_settings(updated).await?))
}

async fn get_retention_settings(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<crate::settings::RetentionSettings>, ApiError> {
    Ok(Json(runtime.get_retention_settings("default").await?))
}

#[derive(Debug, Deserialize)]
struct UpdateRetentionSettingsRequest {
    trace_blob_retention_days: u32,
}

async fn update_retention_settings(
    State(runtime): State<Arc<Runtime>>,
    Json(payload): Json<UpdateRetentionSettingsRequest>,
) -> Result<Json<crate::settings::RetentionSettings>, ApiError> {
    let current = runtime.get_retention_settings("default").await?;
    let updated = crate::settings::RetentionSettings {
        agent_id: current.agent_id,
        trace_blob_retention_days: payload.trace_blob_retention_days,
        created_at: current.created_at,
        updated_at: current.updated_at,
    };
    Ok(Json(runtime.update_retention_settings(updated).await?))
}

async fn recover_runtime(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<crate::runtime::RecoveryReport>, ApiError> {
    Ok(Json(runtime.recover_incomplete_turns().await?))
}

async fn prune_trace_blobs(
    State(runtime): State<Arc<Runtime>>,
) -> Result<Json<crate::runtime::TracePruneReport>, ApiError> {
    Ok(Json(runtime.prune_trace_blobs("default").await?))
}

#[derive(Debug, Deserialize)]
struct CreateThreadRequest {
    title: Option<String>,
}

async fn create_thread(
    State(runtime): State<Arc<Runtime>>,
    payload: Option<Json<CreateThreadRequest>>,
) -> Result<Json<crate::thread::Thread>, ApiError> {
    let title = payload.map(|payload| payload.0.title).unwrap_or(None);
    Ok(Json(runtime.create_web_thread(title).await?))
}

async fn get_thread(
    State(runtime): State<Arc<Runtime>>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadDetail>, ApiError> {
    let thread = runtime
        .get_thread(&thread_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("thread not found: {thread_id}")))?;
    let turns = runtime.list_thread_turns(&thread_id).await?;
    Ok(Json(ThreadDetail { thread, turns }))
}

async fn get_timeline(
    State(runtime): State<Arc<Runtime>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<crate::event::Event>>, ApiError> {
    Ok(Json(runtime.list_thread_timeline(&thread_id).await?))
}

async fn get_thread_trace_details(
    State(runtime): State<Arc<Runtime>>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<crate::model::TraceDetail>>, ApiError> {
    Ok(Json(runtime.list_thread_trace_details(&thread_id).await?))
}

async fn stream_thread(
    State(runtime): State<Arc<Runtime>>,
    Path(thread_id): Path<String>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let receiver = runtime.subscribe_updates();
    let stream = BroadcastStream::new(receiver).filter_map(move |message| {
        let thread_id = thread_id.clone();
        async move {
            let Ok(update) = message else {
                return None;
            };
            let update_thread_id = match &update {
                RuntimeUpdate::EventAdded { thread_id, .. }
                | RuntimeUpdate::TraceRecorded { thread_id, .. }
                | RuntimeUpdate::TurnUpdated { thread_id, .. } => thread_id,
            };
            if update_thread_id != &thread_id {
                return None;
            }
            Some(Ok(Event::default().json_data(update).unwrap()))
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
struct PostMessageRequest {
    content: String,
}

#[derive(Debug, Serialize)]
struct PostMessageResponse {
    thread_id: String,
    turn_id: String,
    response: String,
    trace_id: String,
    status: crate::turn::TurnStatus,
    outbound_messages: Vec<String>,
}

async fn post_message(
    State(runtime): State<Arc<Runtime>>,
    Path(thread_id): Path<String>,
    Json(payload): Json<PostMessageRequest>,
) -> Result<Json<PostMessageResponse>, ApiError> {
    let runtime_for_task = runtime.clone();
    let thread_id_for_task = thread_id.clone();
    let content = payload.content;
    let outcome = tokio::spawn(async move {
        runtime_for_task.handle_inbound(InboundEvent::web(
            "default",
            &thread_id_for_task,
            content,
        ))
        .await
    })
    .await
    .map_err(|error| ApiError::Runtime(RuntimeError::Other(anyhow!(error))))??;
    Ok(Json(PostMessageResponse {
        thread_id: outcome.thread.id,
        turn_id: outcome.turn_id,
        response: outcome.response,
        trace_id: outcome.trace_id,
        status: outcome.status,
        outbound_messages: outcome.outbound_messages,
    }))
}

async fn get_turn_traces(
    State(runtime): State<Arc<Runtime>>,
    Path(turn_id): Path<String>,
) -> Result<Json<Vec<crate::model::ModelTrace>>, ApiError> {
    Ok(Json(runtime.list_turn_traces(&turn_id).await?))
}

#[derive(Debug, Serialize)]
struct ReplayTurnResponse {
    thread_id: String,
    turn_id: String,
    response: String,
    trace_id: String,
    status: crate::turn::TurnStatus,
    outbound_messages: Vec<String>,
}

async fn replay_turn(
    State(runtime): State<Arc<Runtime>>,
    Path(turn_id): Path<String>,
) -> Result<Json<ReplayTurnResponse>, ApiError> {
    let runtime_for_task = runtime.clone();
    let turn_id_for_task = turn_id.clone();
    let outcome = tokio::spawn(async move { runtime_for_task.replay_turn(&turn_id_for_task).await })
        .await
        .map_err(|error| ApiError::Runtime(RuntimeError::Other(anyhow!(error))))??;
    Ok(Json(ReplayTurnResponse {
        thread_id: outcome.thread.id,
        turn_id: outcome.turn_id,
        response: outcome.response,
        trace_id: outcome.trace_id,
        status: outcome.status,
        outbound_messages: outcome.outbound_messages,
    }))
}

async fn get_trace(
    State(runtime): State<Arc<Runtime>>,
    Path(trace_id): Path<String>,
) -> Result<Json<crate::model::TraceDetail>, ApiError> {
    let trace = runtime
        .get_trace_detail(&trace_id)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("trace not found: {trace_id}")))?;
    Ok(Json(trace))
}

#[derive(Debug, Serialize)]
struct ThreadDetail {
    thread: crate::thread::Thread,
    turns: Vec<crate::turn::Turn>,
}

/// Check if new commits are available without pulling.
async fn check_update() -> Result<Json<serde_json::Value>, ApiError> {
    match crate::update::check_for_updates(None) {
        Ok(status) => Ok(Json(json!({ "status": status }))),
        Err(e) => Err(ApiError::Runtime(RuntimeError::Other(e))),
    }
}

/// Trigger a self-update: git pull → cargo build → exec into new binary.
async fn self_update() -> Result<Json<crate::update::UpdateStatus>, ApiError> {
    // Run the update in a blocking thread since it involves process spawning
    let result = tokio::task::spawn_blocking(|| crate::update::perform_update(true))
        .await
        .map_err(|e| ApiError::Runtime(RuntimeError::Other(anyhow!(e))))?;

    match result {
        Ok(status) => Ok(Json(status)),
        Err(e) => Err(ApiError::Runtime(RuntimeError::Other(e))),
    }
}

#[derive(Debug)]
enum ApiError {
    Runtime(crate::error::RuntimeError),
    NotFound(String),
}

impl From<crate::error::RuntimeError> for ApiError {
    fn from(value: crate::error::RuntimeError) -> Self {
        Self::Runtime(value)
    }
}

/// Simple health check — returns 200 OK immediately.
/// Used by watchdog scripts to detect if the process is alive and responsive.
async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// Runtime status endpoint — returns agent identity, version, and uptime info.
/// Used by coordination tools and watchdog scripts for richer health checks.
async fn api_status(State(runtime): State<Arc<Runtime>>) -> Result<Json<Value>, ApiError> {
    let threads = runtime.list_threads().await?;
    let thread_count = threads.len();

    // Count active Tidepool subscriptions from env
    let tidepool_connected =
        std::env::var("TIDEPOOL_DATABASE").is_ok() && std::env::var("TIDEPOOL_HANDLE").is_ok();
    let discord_connected = std::env::var("DISCORD_BOT_TOKEN").is_ok();

    Ok(Json(json!({
        "status": "ok",
        "thread_count": thread_count,
        "channels": {
            "tidepool": tidepool_connected,
            "discord": discord_connected,
        }
    })))
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        match self {
            ApiError::Runtime(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response(),
            ApiError::NotFound(message) => {
                (StatusCode::NOT_FOUND, Json(json!({ "error": message }))).into_response()
            }
        }
    }
}
