use anyhow::Result;
use chrono::Utc;
use libsql::params;
use serde_json::Value;
use uuid::Uuid;

use crate::event::{Event, EventKind};

use super::Db;
use super::internal::*;


impl Db {
    pub async fn append_event(
        &self,
        turn_id: &str,
        thread_id: &str,
        kind: EventKind,
        payload: &Value,
    ) -> Result<Event> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT COALESCE(MAX(sequence), 0) + 1 FROM events WHERE turn_id = ?",
                params![turn_id.to_string()],
            )
            .await?;
        let sequence = if let Some(row) = rows.next().await? {
            row.get::<i64>(0)?
        } else {
            1
        };
        let event = Event {
            id: Uuid::new_v4().to_string(),
            turn_id: turn_id.to_string(),
            thread_id: thread_id.to_string(),
            sequence,
            kind,
            payload: payload.clone(),
            created_at: Utc::now(),
        };
        conn.execute(
            "INSERT INTO events (id, turn_id, thread_id, sequence, kind, payload, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                event.id.clone(),
                event.turn_id.clone(),
                event.thread_id.clone(),
                event.sequence,
                serde_json::to_string(&event.kind)?,
                serde_json::to_string(&event.payload)?,
                event.created_at.to_rfc3339()
            ],
        )
        .await?;
        Ok(event)
    }

    pub async fn list_thread_events(&self, thread_id: &str) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT id, turn_id, thread_id, sequence, kind, payload, created_at FROM events WHERE thread_id = ? ORDER BY created_at ASC, sequence ASC",
                params![thread_id.to_string()],
            )
            .await?;
        let mut events = Vec::new();
        while let Some(row) = rows.next().await? {
            events.push(Event {
                id: row.get::<String>(0)?,
                turn_id: row.get::<String>(1)?,
                thread_id: row.get::<String>(2)?,
                sequence: row.get::<i64>(3)?,
                kind: serde_json::from_str(&row.get::<String>(4)?)?,
                payload: serde_json::from_str(&row.get::<String>(5)?)?,
                created_at: parse_datetime(&row.get::<String>(6)?)?,
            });
        }
        Ok(events)
    }

}