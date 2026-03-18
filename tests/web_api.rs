use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tempfile::tempdir;
use tower::ServiceExt;

use betterclaw::db::Db;
use betterclaw::runtime::Runtime;
use betterclaw::web;

async fn app() -> axum::Router {
    app_with_runtime().await.0
}

async fn app_with_runtime() -> (axum::Router, Arc<Runtime>) {
    let dir = tempdir().unwrap();
    let root = dir.keep();
    let db = Db::open(&root.join("test.db")).await.unwrap();
    let runtime = Arc::new(Runtime::new(db).await.unwrap());
    (web::app(runtime.clone()), runtime)
}

#[tokio::test]
async fn posting_message_creates_turn_and_trace() {
    let app = app().await;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"API Test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"hello world"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let posted: Value = serde_json::from_slice(&body).unwrap();
    assert!(posted["turn_id"].is_string());
    assert!(posted["trace_id"].is_string());

    let traces = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/turns/{}/traces",
                    posted["turn_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(traces.status(), StatusCode::OK);
    let body = axum::body::to_bytes(traces.into_body(), usize::MAX)
        .await
        .unwrap();
    let traces: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(traces.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn malformed_tool_call_records_trace_and_fails_cleanly() {
    let app = app().await;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Malformed"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from("{\"content\":\"/tool echo {\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let detail = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/threads/{thread_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(detail.into_body(), usize::MAX)
        .await
        .unwrap();
    let detail: Value = serde_json::from_slice(&body).unwrap();
    let turns = detail["turns"].as_array().unwrap();
    assert_eq!(turns.last().unwrap()["status"], "failed");

    let traces = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/turns/{}/traces",
                    turns.last().unwrap()["id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(traces.into_body(), usize::MAX)
        .await
        .unwrap();
    let traces: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(traces.as_array().unwrap()[0]["outcome"], "parse_error");
}

#[tokio::test]
async fn trace_endpoint_returns_full_payloads() {
    let app = app().await;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Trace Detail"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"hello trace"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let posted: Value = serde_json::from_slice(&body).unwrap();

    let detail = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/traces/{}",
                    posted["trace_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    let body = axum::body::to_bytes(detail.into_body(), usize::MAX)
        .await
        .unwrap();
    let detail: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(detail["trace_role"], "agent");
    assert!(detail["request_body"].is_object());
    assert!(detail["response_body"].is_object());
}

#[tokio::test]
async fn replay_endpoint_creates_a_new_turn() {
    let app = app().await;

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Replay"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"replay me"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first: Value = serde_json::from_slice(&body).unwrap();

    let replay = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/turns/{}/replay",
                    first["turn_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let body = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .unwrap();
    let replay: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(replay["thread_id"], thread_id);
    assert_ne!(replay["turn_id"], first["turn_id"]);

    let timeline = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/threads/{thread_id}/timeline"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(timeline.into_body(), usize::MAX)
        .await
        .unwrap();
    let timeline: Value = serde_json::from_slice(&body).unwrap();
    assert!(timeline.as_array().unwrap().iter().any(|event| {
        event["turn_id"] == replay["turn_id"] && event["kind"] == "replay_requested"
    }));
}

#[tokio::test]
async fn runtime_settings_round_trip_and_affect_request_payload() {
    let app = app().await;

    let settings = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/settings/runtime")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(settings.status(), StatusCode::OK);

    let updated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/runtime")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"system_prompt":"Be extremely concise.","max_tokens":77,"stream":false,"allow_tools":false,"max_history_turns":3}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Settings"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let posted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"hello settings"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(posted.status(), StatusCode::OK);
    let body = axum::body::to_bytes(posted.into_body(), usize::MAX)
        .await
        .unwrap();
    let posted: Value = serde_json::from_slice(&body).unwrap();

    let trace = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/traces/{}",
                    posted["trace_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(trace.status(), StatusCode::OK);
    let body = axum::body::to_bytes(trace.into_body(), usize::MAX)
        .await
        .unwrap();
    let trace: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(trace["trace"]["model"], "local-debug-model");
    assert_eq!(trace["request_body"]["model"], "local-debug-model");
    assert_eq!(trace["request_body"]["max_tokens"], 77);
    assert_eq!(trace["request_body"]["stream"], false);
    assert_eq!(trace["request_body"]["tools"], Value::Array(vec![]));
    assert_eq!(trace["request_body"]["messages"][0]["role"], "system");
    let system_content = trace["request_body"]["messages"][0]["content"]
        .as_str()
        .unwrap();
    assert!(
        system_content.ends_with("Be extremely concise."),
        "system prompt should end with the configured text, got: {system_content}"
    );
}

#[tokio::test]
async fn retention_settings_and_prune_endpoint_replace_old_blobs() {
    let (app, runtime) = app_with_runtime().await;

    let updated = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/retention")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"trace_blob_retention_days":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(updated.status(), StatusCode::OK);

    let create = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/threads")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Retention"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(create.into_body(), usize::MAX)
        .await
        .unwrap();
    let thread: Value = serde_json::from_slice(&body).unwrap();
    let thread_id = thread["id"].as_str().unwrap();

    let posted = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/threads/{thread_id}/messages"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"content":"retention test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(posted.into_body(), usize::MAX)
        .await
        .unwrap();
    let posted: Value = serde_json::from_slice(&body).unwrap();

    runtime
        .db()
        .backdate_all_trace_blobs(chrono::Utc::now() - chrono::Duration::days(2))
        .await
        .unwrap();

    let prune = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime/prune-traces")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(prune.status(), StatusCode::OK);
    let body = axum::body::to_bytes(prune.into_body(), usize::MAX)
        .await
        .unwrap();
    let report: Value = serde_json::from_slice(&body).unwrap();
    assert!(report["pruned_blob_count"].as_u64().unwrap() >= 2);

    let detail = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/traces/{}",
                    posted["trace_id"].as_str().unwrap()
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(detail.status(), StatusCode::OK);
    let body = axum::body::to_bytes(detail.into_body(), usize::MAX)
        .await
        .unwrap();
    let detail: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(detail["request_body"]["pruned"], true);
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    let app = app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json["commit"].is_string());
}

#[tokio::test]
async fn api_status_endpoint_returns_runtime_info() {
    let app = app().await;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json["agent_id"].is_string());
    assert!(json["thread_count"].is_number());
    assert!(json["channels"].is_object());
    assert!(json["channels"]["tidepool"].is_object());
    assert!(json["channels"]["discord"].is_object());
}
