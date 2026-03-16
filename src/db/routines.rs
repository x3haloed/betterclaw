use anyhow::Result;
use chrono::Utc;
use libsql::params;
use uuid::Uuid;

use crate::routine::{NewObservation, Observation, ObservationKind, ObservationSummary};

use super::Db;

impl Db {
    pub async fn upsert_observation(&self, new: &NewObservation) -> Result<Observation> {
        let conn = self.connect()?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now();
        conn.execute(
            "INSERT INTO observations (id, namespace_id, kind, severity, summary, detail, citations_json, payload_json, resolved, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
            params![
                id.clone(),
                new.namespace_id.clone(),
                new.kind.as_str().to_string(),
                new.severity.as_str().to_string(),
                new.summary.clone(),
                new.detail.clone(),
                serde_json::to_string(&new.citations)?,
                new.payload.to_string(),
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )
        .await?;
        Ok(Observation {
            id,
            namespace_id: new.namespace_id.clone(),
            kind: new.kind.clone(),
            severity: new.severity.clone(),
            summary: new.summary.clone(),
            detail: new.detail.clone(),
            citations: new.citations.clone(),
            payload: new.payload.clone(),
            resolved: false,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn list_observations(
        &self,
        namespace_id: &str,
        kind: Option<ObservationKind>,
        unresolved_only: bool,
        limit: i64,
    ) -> Result<Vec<Observation>> {
        let conn = self.connect()?;
        let kind_value = kind
            .map(|k| k.as_str().to_string())
            .unwrap_or_default();
        let mut rows = conn
            .query(
                "SELECT id, namespace_id, kind, severity, summary, detail, citations_json, payload_json, resolved, created_at, updated_at FROM observations WHERE namespace_id = ? AND (? = '' OR kind = ?) AND (? = 0 OR resolved = 0) ORDER BY created_at DESC LIMIT ?",
                params![
                    namespace_id.to_string(),
                    kind_value.clone(),
                    kind_value,
                    if unresolved_only { 1 } else { 0 },
                    limit
                ],
            )
            .await?;
        let mut observations = Vec::new();
        while let Some(row) = rows.next().await? {
            observations.push(Observation {
                id: row.get::<String>(0)?,
                namespace_id: row.get::<String>(1)?,
                kind: row.get::<String>(2)?.parse().map_err(|e: String| anyhow::anyhow!(e))?,
                severity: row.get::<String>(3)?.parse().map_err(|e: String| anyhow::anyhow!(e))?,
                summary: row.get::<String>(4)?,
                detail: row.get::<Option<String>>(5)?,
                citations: serde_json::from_str(&row.get::<String>(6)?)?,
                payload: serde_json::from_str(&row.get::<String>(7)?)?,
                resolved: row.get::<i64>(8)? != 0,
                created_at: row.get::<String>(9)?.parse()?,
                updated_at: row.get::<String>(10)?.parse()?,
            });
        }
        Ok(observations)
    }

    pub async fn resolve_observation(&self, observation_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE observations SET resolved = 1, updated_at = ? WHERE id = ?",
            params![Utc::now().to_rfc3339(), observation_id.to_string()],
        )
        .await?;
        Ok(())
    }

    pub async fn resolve_observations_for_citations(
        &self,
        namespace_id: &str,
        entry_ids: &[String],
    ) -> Result<usize> {
        if entry_ids.is_empty() {
            return Ok(0);
        }
        let conn = self.connect()?;
        let mut resolved = 0;
        for entry_id in entry_ids {
            let rows_affected = conn
                .execute(
                    "UPDATE observations SET resolved = 1, updated_at = ? WHERE namespace_id = ? AND resolved = 0 AND citations_json LIKE ?",
                    params![
                        Utc::now().to_rfc3339(),
                        namespace_id.to_string(),
                        format!("%{entry_id}%"),
                    ],
                )
                .await?;
            resolved += rows_affected;
        }
        Ok(resolved as usize)
    }

    pub async fn observation_summary(&self, namespace_id: &str) -> Result<ObservationSummary> {
        let observations = self.list_observations(namespace_id, None, false, 512).await?;
        let mut by_kind = std::collections::HashMap::new();
        let mut by_severity = std::collections::HashMap::new();
        let mut unresolved = 0;
        for obs in &observations {
            *by_kind.entry(obs.kind.as_str().to_string()).or_insert(0) += 1;
            *by_severity.entry(obs.severity.as_str().to_string()).or_insert(0) += 1;
            if !obs.resolved {
                unresolved += 1;
            }
        }
        Ok(ObservationSummary {
            total: observations.len(),
            unresolved,
            by_kind,
            by_severity,
        })
    }

    pub async fn stale_observation_ids(
        &self,
        namespace_id: &str,
        max_age_hours: i64,
    ) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let cutoff = (Utc::now() - chrono::Duration::hours(max_age_hours)).to_rfc3339();
        let mut rows = conn
            .query(
                "SELECT id FROM observations WHERE namespace_id = ? AND resolved = 0 AND created_at < ?",
                params![namespace_id.to_string(), cutoff],
            )
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get::<String>(0)?);
        }
        Ok(ids)
    }

    pub async fn count_unresolved_observations(
        &self,
        namespace_id: &str,
        kind: &ObservationKind,
    ) -> Result<i64> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM observations WHERE namespace_id = ? AND kind = ? AND resolved = 0",
                params![namespace_id.to_string(), kind.as_str().to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            row.get::<i64>(0)?
        } else {
            0
        })
    }
}
