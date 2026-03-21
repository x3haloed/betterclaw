use super::internal::redact_json;
use super::*;
use chrono::{Duration, Utc};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn redacts_secret_keys() {
    let value = json!({
        "authorization": "Bearer abc",
        "nested": {
            "api_key": "secret"
        }
    });
    let redacted = redact_json(&value);
    assert_eq!(redacted["authorization"], "[REDACTED]");
    assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
}

#[test]
fn preserves_non_secret_token_counters() {
    let value = json!({
        "max_tokens": 512,
        "prompt_tokens": 128
    });
    let redacted = redact_json(&value);
    assert_eq!(redacted["max_tokens"], 512);
    assert_eq!(redacted["prompt_tokens"], 128);
}

#[tokio::test]
async fn prune_trace_blobs_replaces_body_with_placeholder() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("prune.db")).await.unwrap();
    let blob = db
        .store_trace_blob_json(&json!({"hello":"world"}))
        .await
        .unwrap();
    let report = db
        .prune_trace_blobs_older_than(Utc::now() + Duration::days(1), Utc::now())
        .await
        .unwrap();
    assert_eq!(report.pruned_blob_count, 1);
    let payload = db.fetch_trace_blob_json(&blob.id).await.unwrap();
    assert_eq!(payload["pruned"], true);
}

#[tokio::test]
async fn fresh_connections_apply_busy_timeout() {
    let dir = tempdir().unwrap();
    let db = Db::open(&dir.path().join("busy-timeout.db")).await.unwrap();
    let conn = db.connect().await.unwrap();
    let mut rows = conn.query("PRAGMA busy_timeout", params![]).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let timeout_ms = row.get::<i64>(0).unwrap();
    assert_eq!(timeout_ms, Db::BUSY_TIMEOUT_MS as i64);
}
