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
    let dir = tempdir().unwrap();
    let root = dir.keep();
    let db = Db::open(&root.join("test.db")).await.unwrap();
    let runtime = Arc::new(Runtime::new(db).await.unwrap());
    web::app(runtime)
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
    assert!(detail["request_body"].is_object());
    assert!(detail["response_body"].is_object());
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
                    r#"{"model":"custom-model","system_prompt":"Be extremely concise.","temperature":0.7,"max_tokens":77,"stream":false,"allow_tools":false,"max_history_turns":3}"#,
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
    assert_eq!(trace["trace"]["model"], "custom-model");
    assert_eq!(trace["request_body"]["model"], "custom-model");
    let request_body_text = trace["request_body"].to_string();
    assert!(
        request_body_text.contains("\"temperature\":0.7")
            || request_body_text.contains("\"temperature\":0.699"),
        "temperature should round-trip"
    );
    assert_eq!(trace["request_body"]["max_tokens"], 77);
    assert_eq!(trace["request_body"]["stream"], false);
    assert_eq!(trace["request_body"]["tools"], Value::Array(vec![]));
    assert_eq!(trace["request_body"]["messages"][0]["role"], "system");
    assert_eq!(
        trace["request_body"]["messages"][0]["content"],
        "Be extremely concise."
    );
}
