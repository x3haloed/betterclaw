use anyhow::Result;
use chrono::{DateTime, Utc};
use libsql::params;
use serde_json::Value;
use uuid::Uuid;

use crate::model::{ModelTrace, TraceBlob, TraceDetail};

use super::Db;
use super::internal::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceBlobPruneReport {
    pub pruned_blob_count: usize,
    pub reclaimed_bytes: i64,
}

impl Db {
    pub async fn store_trace_blob(&self, body: &Value) -> Result<TraceBlob> {
        self.store_trace_blob_json(body).await
    }

    pub async fn store_trace_blob_json<T: serde::Serialize>(&self, body: &T) -> Result<TraceBlob> {
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now();
        let value = redact_json(&serde_json::to_value(body)?);
        let body = compress_bytes(value.to_string().as_bytes())?;
        let blob = TraceBlob {
            id: id.clone(),
            encoding: "gzip".to_string(),
            content_type: "application/json".to_string(),
            body,
            created_at,
        };
        let (_write_guard, conn) = self.write_connection().await?;
        conn.execute(
            "INSERT INTO trace_blobs (id, encoding, content_type, body, created_at) VALUES (?, ?, ?, ?, ?)",
            params![
                blob.id.clone(),
                blob.encoding.clone(),
                blob.content_type.clone(),
                blob.body.clone(),
                blob.created_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(blob)
    }

    pub async fn fetch_trace_blob_json(&self, blob_id: &str) -> Result<Value> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT body FROM trace_blobs WHERE id = ?",
                params![blob_id.to_string()],
            )
            .await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| anyhow::anyhow!("trace blob not found: {blob_id}"))?;
        let body: Vec<u8> = row.get(0)?;
        Ok(serde_json::from_slice(&decompress_bytes(&body)?)?)
    }
    pub async fn prune_trace_blobs_older_than(
        &self,
        cutoff: DateTime<Utc>,
        pruned_at: DateTime<Utc>,
    ) -> Result<TraceBlobPruneReport> {
        const PRUNED_CONTENT_TYPE: &str = "application/vnd.betterclaw.pruned+json";

        let (_write_guard, conn) = self.write_connection().await?;
        let mut rows = conn
            .query(
                "SELECT id, encoding, content_type, body, created_at FROM trace_blobs WHERE created_at < ? AND content_type != ?",
                params![cutoff.to_rfc3339(), PRUNED_CONTENT_TYPE.to_string()],
            )
            .await?;

        let mut blobs = Vec::new();
        while let Some(row) = rows.next().await? {
            blobs.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<String>(2)?,
                row.get::<Vec<u8>>(3)?,
                row.get::<String>(4)?,
            ));
        }

        let mut report = TraceBlobPruneReport {
            pruned_blob_count: 0,
            reclaimed_bytes: 0,
        };

        for (blob_id, encoding, content_type, body, created_at) in blobs {
            let original_bytes = body.len() as i64;
            let placeholder = serde_json::json!({
                "pruned": true,
                "reason": "retention_policy",
                "pruned_at": pruned_at,
                "original_encoding": encoding,
                "original_content_type": content_type,
                "original_created_at": created_at,
                "original_size_bytes": original_bytes,
            });
            let replacement = compress_bytes(placeholder.to_string().as_bytes())?;
            let replacement_bytes = replacement.len() as i64;
            conn.execute(
                "UPDATE trace_blobs SET encoding = ?, content_type = ?, body = ? WHERE id = ?",
                params![
                    "gzip".to_string(),
                    PRUNED_CONTENT_TYPE.to_string(),
                    replacement,
                    blob_id,
                ],
            )
            .await?;
            report.pruned_blob_count += 1;
            report.reclaimed_bytes += (original_bytes - replacement_bytes).max(0);
        }

        Ok(report)
    }
    pub async fn record_model_trace(&self, trace: &ModelTrace) -> Result<()> {
        let (_write_guard, conn) = self.write_connection().await?;
        conn.execute(
            "INSERT INTO model_traces (id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                trace.id.clone(),
                trace.turn_id.clone(),
                trace.thread_id.clone(),
                trace.agent_id.clone(),
                trace.channel.clone(),
                trace.model.clone(),
                trace.request_started_at.to_rfc3339(),
                trace.request_completed_at.to_rfc3339(),
                trace.duration_ms,
                serde_json::to_string(&trace.outcome)?,
                trace.input_tokens,
                trace.output_tokens,
                trace.cache_read_input_tokens,
                trace.cache_creation_input_tokens,
                trace.provider_request_id.clone(),
                trace.tool_count,
                serde_json::to_string(&trace.tool_names)?,
                trace.request_blob_id.clone(),
                trace.response_blob_id.clone(),
                trace.stream_blob_id.clone(),
                trace.error_summary.clone()
            ],
        )
        .await?;
        Ok(())
    }
    pub async fn list_turn_traces(&self, turn_id: &str) -> Result<Vec<ModelTrace>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary FROM model_traces WHERE turn_id = ? ORDER BY request_started_at ASC",
                params![turn_id.to_string()],
            )
            .await?;
        let mut traces = Vec::new();
        while let Some(row) = rows.next().await? {
            traces.push(ModelTrace {
                id: row.get::<String>(0)?,
                turn_id: row.get::<String>(1)?,
                thread_id: row.get::<String>(2)?,
                agent_id: row.get::<String>(3)?,
                channel: row.get::<String>(4)?,
                model: row.get::<String>(5)?,
                request_started_at: parse_datetime(&row.get::<String>(6)?)?,
                request_completed_at: parse_datetime(&row.get::<String>(7)?)?,
                duration_ms: row.get::<i64>(8)?,
                outcome: serde_json::from_str(&row.get::<String>(9)?)?,
                input_tokens: row.get::<i64>(10)?,
                output_tokens: row.get::<i64>(11)?,
                cache_read_input_tokens: row.get::<i64>(12)?,
                cache_creation_input_tokens: row.get::<i64>(13)?,
                provider_request_id: row.get::<Option<String>>(14)?,
                tool_count: row.get::<i64>(15)?,
                tool_names: serde_json::from_str(&row.get::<String>(16)?)?,
                request_blob_id: row.get::<String>(17)?,
                response_blob_id: row.get::<String>(18)?,
                stream_blob_id: row.get::<Option<String>>(19)?,
                error_summary: row.get::<Option<String>>(20)?,
            });
        }
        Ok(traces)
    }
    pub async fn get_trace_detail(&self, trace_id: &str) -> Result<Option<TraceDetail>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, agent_id, channel, model, request_started_at, request_completed_at, duration_ms, outcome, input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens, provider_request_id, tool_count, tool_names, request_blob_id, response_blob_id, stream_blob_id, error_summary FROM model_traces WHERE id = ?",
                params![trace_id.to_string()],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let trace = ModelTrace {
            id: row.get::<String>(0)?,
            turn_id: row.get::<String>(1)?,
            thread_id: row.get::<String>(2)?,
            agent_id: row.get::<String>(3)?,
            channel: row.get::<String>(4)?,
            model: row.get::<String>(5)?,
            request_started_at: parse_datetime(&row.get::<String>(6)?)?,
            request_completed_at: parse_datetime(&row.get::<String>(7)?)?,
            duration_ms: row.get::<i64>(8)?,
            outcome: serde_json::from_str(&row.get::<String>(9)?)?,
            input_tokens: row.get::<i64>(10)?,
            output_tokens: row.get::<i64>(11)?,
            cache_read_input_tokens: row.get::<i64>(12)?,
            cache_creation_input_tokens: row.get::<i64>(13)?,
            provider_request_id: row.get::<Option<String>>(14)?,
            tool_count: row.get::<i64>(15)?,
            tool_names: serde_json::from_str(&row.get::<String>(16)?)?,
            request_blob_id: row.get::<String>(17)?,
            response_blob_id: row.get::<String>(18)?,
            stream_blob_id: row.get::<Option<String>>(19)?,
            error_summary: row.get::<Option<String>>(20)?,
        };
        let request_body = self.fetch_trace_blob_json(&trace.request_blob_id).await?;
        let response_blob = self.fetch_trace_blob_json(&trace.response_blob_id).await?;
        let (response_body, reduced_result) = match response_blob {
            Value::Object(map) => (
                map.get("raw_response").cloned().unwrap_or(Value::Null),
                map.get("reduced_result").cloned(),
            ),
            other => (other, None),
        };
        Ok(Some(TraceDetail {
            trace_role: infer_trace_role(&request_body),
            request_body,
            response_body,
            stream_body: match &trace.stream_blob_id {
                Some(blob_id) => Some(self.fetch_trace_blob_json(blob_id).await?),
                None => None,
            },
            reduced_result,
            trace,
        }))
    }
    pub async fn backdate_all_trace_blobs(&self, created_at: DateTime<Utc>) -> Result<()> {
        let (_write_guard, conn) = self.write_connection().await?;
        conn.execute(
            "UPDATE trace_blobs SET created_at = ?",
            params![created_at.to_rfc3339()],
        )
        .await?;
        Ok(())
    }
}

fn infer_trace_role(request_body: &Value) -> String {
    let response_format_name = request_body
        .get("response_format")
        .and_then(|format| format.get("json_schema"))
        .and_then(|schema| schema.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            request_body
                .get("text")
                .and_then(|text| text.get("format"))
                .and_then(|format| format.get("name"))
                .and_then(Value::as_str)
        });
    if response_format_name == Some("betterclaw_memory_distill") {
        return "compressor".to_string();
    }
    "agent".to_string()
}
