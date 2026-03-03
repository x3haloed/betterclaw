//! LedgerStore implementation for LibSqlBackend.

use async_trait::async_trait;
use chrono::Utc;
use libsql::params;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{LibSqlBackend, fmt_ts, get_json, get_opt_text, get_text, get_ts, opt_text};
use crate::db::LedgerStore;
use crate::error::DatabaseError;
use crate::ledger::{LedgerEvent, NewLedgerEvent};

fn compute_event_sha256(
    user_id: &str,
    episode_id: Option<&str>,
    kind: &str,
    source: &str,
    content: Option<&str>,
    payload_json: &str,
    created_at: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(b"\n");
    if let Some(e) = episode_id {
        hasher.update(e.as_bytes());
    }
    hasher.update(b"\n");
    hasher.update(kind.as_bytes());
    hasher.update(b"\n");
    hasher.update(source.as_bytes());
    hasher.update(b"\n");
    if let Some(c) = content {
        hasher.update(c.as_bytes());
    }
    hasher.update(b"\n");
    hasher.update(payload_json.as_bytes());
    hasher.update(b"\n");
    hasher.update(created_at.as_bytes());
    hex::encode(hasher.finalize())
}

#[async_trait]
impl LedgerStore for LibSqlBackend {
    async fn append_ledger_event(&self, event: &NewLedgerEvent<'_>) -> Result<Uuid, DatabaseError> {
        let conn = self.connect().await?;
        let id = Uuid::new_v4();
        let now = fmt_ts(&Utc::now());
        let payload_json = serde_json::to_string(event.payload)
            .map_err(|e| DatabaseError::Query(format!("failed to encode payload JSON: {e}")))?;
        let episode_id = event.episode_id.map(|u| u.to_string());

        let sha256 = compute_event_sha256(
            event.user_id,
            episode_id.as_deref(),
            event.kind,
            event.source,
            event.content,
            &payload_json,
            &now,
        );

        conn.execute(
            r#"
            INSERT INTO ledger_events
                (id, user_id, episode_id, kind, source, content, payload, sha256, created_at)
            VALUES
                (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            params![
                id.to_string(),
                event.user_id,
                opt_text(episode_id.as_deref()),
                event.kind,
                event.source,
                opt_text(event.content),
                payload_json,
                sha256,
                now
            ],
        )
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;

        Ok(id)
    }

    async fn get_ledger_event(&self, id: Uuid) -> Result<Option<LedgerEvent>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                r#"
                SELECT id, user_id, episode_id, kind, source, content, payload, sha256, created_at
                FROM ledger_events
                WHERE id = ?1
                "#,
                params![id.to_string()],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        let Some(row) = rows.next().await.map_err(|e| DatabaseError::Query(e.to_string()))?
        else {
            return Ok(None);
        };

        let id_str = get_text(&row, 0);
        let episode_id = get_opt_text(&row, 2).and_then(|s| Uuid::parse_str(&s).ok());
        let payload = get_json(&row, 6);
        let created_at = get_ts(&row, 8);

        Ok(Some(LedgerEvent {
            id: Uuid::parse_str(&id_str)
                .map_err(|e| DatabaseError::Query(format!("invalid ledger event id: {e}")))?,
            user_id: get_text(&row, 1),
            episode_id,
            kind: get_text(&row, 3),
            source: get_text(&row, 4),
            content: get_opt_text(&row, 5),
            payload,
            sha256: get_opt_text(&row, 7),
            created_at,
        }))
    }

    async fn list_recent_ledger_events(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<LedgerEvent>, DatabaseError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                r#"
                SELECT id, user_id, episode_id, kind, source, content, payload, sha256, created_at
                FROM ledger_events
                WHERE user_id = ?1
                ORDER BY created_at DESC
                LIMIT ?2
                "#,
                params![user_id, limit],
            )
            .await
            .map_err(|e| DatabaseError::Query(e.to_string()))?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| DatabaseError::Query(e.to_string()))?
        {
            let id = Uuid::parse_str(&get_text(&row, 0)).map_err(|e| {
                DatabaseError::Query(format!("invalid ledger event id: {e}"))
            })?;
            let episode_id = get_opt_text(&row, 2).and_then(|s| Uuid::parse_str(&s).ok());
            out.push(LedgerEvent {
                id,
                user_id: get_text(&row, 1),
                episode_id,
                kind: get_text(&row, 3),
                source: get_text(&row, 4),
                content: get_opt_text(&row, 5),
                payload: get_json(&row, 6),
                sha256: get_opt_text(&row, 7),
                created_at: get_ts(&row, 8),
            });
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    #[tokio::test]
    async fn append_and_read_roundtrip() {
        let backend = LibSqlBackend::new_memory().await.expect("backend");
        backend.run_migrations().await.expect("migrations");

        let payload = serde_json::json!({"foo": "bar"});
        let new_event = NewLedgerEvent {
            user_id: "test",
            episode_id: None,
            kind: "user_turn",
            source: "test",
            content: Some("hello"),
            payload: &payload,
        };

        let id = backend
            .append_ledger_event(&new_event)
            .await
            .expect("append");

        let fetched = backend.get_ledger_event(id).await.expect("get").unwrap();
        assert_eq!(fetched.user_id, "test");
        assert_eq!(fetched.kind, "user_turn");
        assert_eq!(fetched.source, "test");
        assert_eq!(fetched.content.as_deref(), Some("hello"));
        assert_eq!(fetched.payload["foo"], "bar");
        assert!(fetched.sha256.as_deref().unwrap_or("").len() >= 32);

        let recent = backend
            .list_recent_ledger_events("test", 10)
            .await
            .expect("recent");
        assert!(!recent.is_empty());
        assert_eq!(recent[0].id, id);
    }
}
