use crate::memory::MemoryArtifactKind;
use crate::turn::TurnStatus;
use anyhow::Result;
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value;
use std::io::{Read, Write};

pub(crate) fn turn_status_string(status: &TurnStatus) -> String {
    match status {
        TurnStatus::Pending => "pending",
        TurnStatus::Running => "running",
        TurnStatus::AwaitingUser => "awaiting_user",
        TurnStatus::Succeeded => "succeeded",
        TurnStatus::Failed => "failed",
    }
    .to_string()
}

pub(crate) fn turn_status_from_string(value: &str) -> TurnStatus {
    match value {
        "pending" => TurnStatus::Pending,
        "running" => TurnStatus::Running,
        "awaiting_user" => TurnStatus::AwaitingUser,
        "succeeded" => TurnStatus::Succeeded,
        "failed" => TurnStatus::Failed,
        _ => TurnStatus::Failed,
    }
}

pub(crate) fn memory_artifact_kind_from_str(value: &str) -> MemoryArtifactKind {
    match value {
        "wake_pack.v0" => MemoryArtifactKind::WakePackV0,
        "fact.v0" => MemoryArtifactKind::FactV0,
        "invariant.candidate.v0" => MemoryArtifactKind::InvariantCandidateV0,
        "invariant.v1"
        | "invariant.self.v0"
        | "invariant.user.v0"
        | "invariant.relationship.v0"
        | "policy.v0"
        | "preference.v0"
        | "hypothesis.v0" => MemoryArtifactKind::InvariantV0,
        "drift.flag.v0" => MemoryArtifactKind::DriftFlagV0,
        "drift.contradiction.v0" => MemoryArtifactKind::DriftContradictionV0,
        "drift.merge.v0" => MemoryArtifactKind::DriftMergeV0,
        "distill.micro" => MemoryArtifactKind::DistillMicro,
        _ => MemoryArtifactKind::DistillMicro,
    }
}

pub(crate) fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }

    const LEGACY_FORMATS: [&str; 2] = ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S"];
    for format in LEGACY_FORMATS {
        if let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(value, format) {
            return Ok(parsed.and_utc());
        }
    }

    Err(anyhow::anyhow!("unsupported datetime format: {value}"))
}

pub(crate) fn compress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

pub(crate) fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

pub(crate) fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let lower = key.to_ascii_lowercase();
                    if [
                        "authorization",
                        "api_key",
                        "x-api-key",
                        "cookie",
                        "token",
                        "access_token",
                        "refresh_token",
                        "session_token",
                        "bearer_token",
                        "bearer",
                        "x-auth-token",
                    ]
                    .iter()
                    .any(|needle| lower == *needle)
                        || lower.ends_with("_api_key")
                        || lower.ends_with("_secret")
                    {
                        (key.clone(), Value::String("[REDACTED]".to_string()))
                    } else {
                        (key.clone(), redact_json(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, Timelike};

    use super::parse_datetime;

    #[test]
    fn parses_rfc3339_datetimes() {
        let parsed = parse_datetime("2026-03-16T02:30:54.109921+00:00").unwrap();
        assert_eq!(parsed.year(), 2026);
        assert_eq!(parsed.month(), 3);
        assert_eq!(parsed.day(), 16);
    }

    #[test]
    fn parses_legacy_sqlite_datetimes() {
        let parsed = parse_datetime("2026-03-11 15:32:09").unwrap();
        assert_eq!(parsed.year(), 2026);
        assert_eq!(parsed.month(), 3);
        assert_eq!(parsed.day(), 11);
        assert_eq!(parsed.hour(), 15);
        assert_eq!(parsed.minute(), 32);
        assert_eq!(parsed.second(), 9);
    }
}
