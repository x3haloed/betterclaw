use anyhow::Result;
use libsql::params;

use crate::settings::{RetentionSettings, RuntimeSettings};

use super::Db;
use super::internal::*;

impl Db {
    pub async fn seed_runtime_settings(&self, settings: &RuntimeSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO runtime_settings (agent_id, model, system_prompt, max_tokens, stream, allow_tools, max_history_turns, inject_wake_pack, inject_ledger_recall, enable_auto_distill, enable_observations, inject_observations, model_roles_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                settings.agent_id.clone(),
                settings.model.clone(),
                settings.system_prompt.clone(),
                settings.max_tokens as i64,
                if settings.stream { 1 } else { 0 },
                if settings.allow_tools { 1 } else { 0 },
                settings.max_history_turns as i64,
                if settings.inject_wake_pack { 1 } else { 0 },
                if settings.inject_ledger_recall { 1 } else { 0 },
                if settings.enable_auto_distill { 1 } else { 0 },
                if settings.enable_observations { 1 } else { 0 },
                if settings.inject_observations { 1 } else { 0 },
                serde_json::to_string(&settings.model_roles)?,
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn seed_retention_settings(&self, settings: &RetentionSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR IGNORE INTO retention_settings (agent_id, trace_blob_retention_days, created_at, updated_at) VALUES (?, ?, ?, ?)",
            params![
                settings.agent_id.clone(),
                settings.trace_blob_retention_days as i64,
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }
    pub async fn load_runtime_settings(&self, agent_id: &str) -> Result<Option<RuntimeSettings>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT agent_id, model, system_prompt, max_tokens, stream, allow_tools, max_history_turns, inject_wake_pack, inject_ledger_recall, enable_auto_distill, COALESCE(enable_observations, 1), COALESCE(inject_observations, 1), model_roles_json, created_at, updated_at FROM runtime_settings WHERE agent_id = ?",
                params![agent_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(RuntimeSettings {
                agent_id: row.get::<String>(0)?,
                model: row.get::<String>(1)?,
                system_prompt: row.get::<String>(2)?,
                max_tokens: row.get::<i64>(3)? as u32,
                stream: row.get::<i64>(4)? != 0,
                allow_tools: row.get::<i64>(5)? != 0,
                max_history_turns: row.get::<i64>(6)? as u32,
                inject_wake_pack: row.get::<i64>(7)? != 0,
                inject_ledger_recall: row.get::<i64>(8)? != 0,
                enable_auto_distill: row.get::<i64>(9)? != 0,
                enable_observations: row.get::<i64>(10)? != 0,
                inject_observations: row.get::<i64>(11)? != 0,
                model_roles: serde_json::from_str(&row.get::<String>(12)?)?,
                created_at: parse_datetime(&row.get::<String>(13)?)?,
                updated_at: parse_datetime(&row.get::<String>(14)?)?,
            })
        } else {
            None
        })
    }

    pub async fn load_retention_settings(
        &self,
        agent_id: &str,
    ) -> Result<Option<RetentionSettings>> {
        let conn = self.connect()?;
        let mut rows = conn
            .query(
                "SELECT agent_id, trace_blob_retention_days, created_at, updated_at FROM retention_settings WHERE agent_id = ?",
                params![agent_id.to_string()],
            )
            .await?;
        Ok(if let Some(row) = rows.next().await? {
            Some(RetentionSettings {
                agent_id: row.get::<String>(0)?,
                trace_blob_retention_days: row.get::<i64>(1)? as u32,
                created_at: parse_datetime(&row.get::<String>(2)?)?,
                updated_at: parse_datetime(&row.get::<String>(3)?)?,
            })
        } else {
            None
        })
    }
    pub async fn update_runtime_settings(&self, settings: &RuntimeSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO runtime_settings (agent_id, model, system_prompt, max_tokens, stream, allow_tools, max_history_turns, inject_wake_pack, inject_ledger_recall, enable_auto_distill, enable_observations, inject_observations, model_roles_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, COALESCE((SELECT created_at FROM runtime_settings WHERE agent_id = ?), ?), ?)",
            params![
                settings.agent_id.clone(),
                settings.model.clone(),
                settings.system_prompt.clone(),
                settings.max_tokens as i64,
                if settings.stream { 1 } else { 0 },
                if settings.allow_tools { 1 } else { 0 },
                settings.max_history_turns as i64,
                if settings.inject_wake_pack { 1 } else { 0 },
                if settings.inject_ledger_recall { 1 } else { 0 },
                if settings.enable_auto_distill { 1 } else { 0 },
                if settings.enable_observations { 1 } else { 0 },
                if settings.inject_observations { 1 } else { 0 },
                serde_json::to_string(&settings.model_roles)?,
                settings.agent_id.clone(),
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn update_retention_settings(&self, settings: &RetentionSettings) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT OR REPLACE INTO retention_settings (agent_id, trace_blob_retention_days, created_at, updated_at) VALUES (?, ?, COALESCE((SELECT created_at FROM retention_settings WHERE agent_id = ?), ?), ?)",
            params![
                settings.agent_id.clone(),
                settings.trace_blob_retention_days as i64,
                settings.agent_id.clone(),
                settings.created_at.to_rfc3339(),
                settings.updated_at.to_rfc3339(),
            ],
        )
        .await?;
        Ok(())
    }
}
