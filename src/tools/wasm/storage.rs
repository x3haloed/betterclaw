//! WASM binary storage with integrity verification.
//!
//! Stores compiled WASM tools in libSQL with BLAKE3 hash verification.
//! On load, the hash is verified to detect tampering.
//!
//! # Storage Flow
//!
//! ```text
//! WASM bytes ──► BLAKE3 hash ──► Store in libSQL
//!                    │               (binary + hash)
//!                    │
//!                    └──► Later: Load ──► Verify hash ──► Return bytes
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::tools::wasm::capabilities::Capabilities;
use crate::tools::wasm::capabilities_schema::CapabilitiesFile;

/// Trust level for a WASM tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustLevel {
    /// Built-in system tool (highest trust).
    System,
    /// Audited and verified tool.
    Verified,
    /// User-uploaded tool (untrusted).
    User,
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustLevel::System => write!(f, "system"),
            TrustLevel::Verified => write!(f, "verified"),
            TrustLevel::User => write!(f, "user"),
        }
    }
}

impl std::str::FromStr for TrustLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "system" => Ok(TrustLevel::System),
            "verified" => Ok(TrustLevel::Verified),
            "user" => Ok(TrustLevel::User),
            _ => Err(format!("Unknown trust level: {}", s)),
        }
    }
}

/// Status of a WASM tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Tool is active and can be used.
    Active,
    /// Tool is disabled (manually or due to errors).
    Disabled,
    /// Tool is quarantined (suspected malicious).
    Quarantined,
}

impl std::fmt::Display for ToolStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolStatus::Active => write!(f, "active"),
            ToolStatus::Disabled => write!(f, "disabled"),
            ToolStatus::Quarantined => write!(f, "quarantined"),
        }
    }
}

impl std::str::FromStr for ToolStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "active" => Ok(ToolStatus::Active),
            "disabled" => Ok(ToolStatus::Disabled),
            "quarantined" => Ok(ToolStatus::Quarantined),
            _ => Err(format!("Unknown status: {}", s)),
        }
    }
}

/// A stored WASM tool (metadata only).
#[derive(Debug, Clone)]
pub struct StoredWasmTool {
    pub id: Uuid,
    pub user_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
    pub source_url: Option<String>,
    pub trust_level: TrustLevel,
    pub status: ToolStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Full tool data including binary (not returned by default for efficiency).
#[derive(Debug)]
pub struct StoredWasmToolWithBinary {
    pub tool: StoredWasmTool,
    pub wasm_binary: Vec<u8>,
    pub binary_hash: Vec<u8>,
}

/// Capabilities stored in the database.
#[derive(Debug, Clone)]
pub struct StoredCapabilities {
    pub tool_id: Uuid,
    pub capabilities_file: CapabilitiesFile,
}

impl StoredCapabilities {
    pub fn to_capabilities(&self) -> Capabilities {
        self.capabilities_file.to_capabilities()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WasmStorageError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("Tool not found: {0}")]
    NotFound(String),
    #[error("Invalid data: {0}")]
    InvalidData(String),
    #[error("Integrity violation: stored hash mismatch")]
    IntegrityViolation,
}

/// Trait for WASM tool storage.
#[async_trait]
pub trait WasmToolStore: Send + Sync {
    async fn store(&self, params: StoreToolParams) -> Result<StoredWasmTool, WasmStorageError>;
    async fn get(&self, user_id: &str, name: &str) -> Result<StoredWasmTool, WasmStorageError>;
    async fn get_with_binary(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<StoredWasmToolWithBinary, WasmStorageError>;
    async fn list(&self, user_id: &str) -> Result<Vec<StoredWasmTool>, WasmStorageError>;
    async fn update_status(
        &self,
        user_id: &str,
        name: &str,
        status: ToolStatus,
    ) -> Result<(), WasmStorageError>;
    async fn delete(&self, user_id: &str, name: &str) -> Result<bool, WasmStorageError>;

    /// Get the stored capabilities for a tool.
    async fn get_capabilities(
        &self,
        tool_id: Uuid,
    ) -> Result<Option<StoredCapabilities>, WasmStorageError>;

    /// Set (upsert) the stored capabilities for a tool.
    async fn set_capabilities(
        &self,
        tool_id: Uuid,
        capabilities_file: &CapabilitiesFile,
    ) -> Result<(), WasmStorageError>;
}

/// Parameters for storing a new tool.
pub struct StoreToolParams {
    pub user_id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub wasm_binary: Vec<u8>,
    pub parameters_schema: serde_json::Value,
    pub source_url: Option<String>,
    pub trust_level: TrustLevel,
}

/// Compute BLAKE3 hash of WASM binary.
pub fn compute_binary_hash(binary: &[u8]) -> Vec<u8> {
    let hash = blake3::hash(binary);
    hash.as_bytes().to_vec()
}

/// Verify binary integrity against stored hash.
pub fn verify_binary_integrity(binary: &[u8], expected_hash: &[u8]) -> bool {
    let actual_hash = compute_binary_hash(binary);
    actual_hash == expected_hash
}

// ==================== libSQL implementation ====================

/// libSQL/Turso implementation of WasmToolStore.
///
/// Holds an `Arc<Database>` handle and creates a fresh connection per operation,
/// matching the connection-per-request pattern used by the main `LibSqlBackend`.
pub struct LibSqlWasmToolStore {
    db: Arc<libsql::Database>,
}

impl LibSqlWasmToolStore {
    pub fn new(db: Arc<libsql::Database>) -> Self {
        Self { db }
    }

    async fn connect(&self) -> Result<libsql::Connection, WasmStorageError> {
        let conn = self
            .db
            .connect()
            .map_err(|e| WasmStorageError::Database(format!("Connection failed: {}", e)))?;
        conn.query("PRAGMA busy_timeout = 5000", ())
            .await
            .map_err(|e| WasmStorageError::Database(format!("Failed to set busy_timeout: {}", e)))?;
        Ok(conn)
    }
}

#[async_trait]
impl WasmToolStore for LibSqlWasmToolStore {
    async fn store(&self, params: StoreToolParams) -> Result<StoredWasmTool, WasmStorageError> {
        let binary_hash = compute_binary_hash(&params.wasm_binary);
        let id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let schema_str = serde_json::to_string(&params.parameters_schema)
            .map_err(|e| WasmStorageError::InvalidData(e.to_string()))?;

        // Wrap INSERT + read-back in a transaction to prevent TOCTOU races
        let conn = self.connect().await?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        tx.execute(
            r#"
                INSERT INTO wasm_tools (
                    id, user_id, name, version, description, wasm_binary, binary_hash,
                    parameters_schema, source_url, trust_level, status, created_at, updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'active', ?11, ?11)
                ON CONFLICT (user_id, name, version) DO UPDATE SET
                    description = excluded.description,
                    wasm_binary = excluded.wasm_binary,
                    binary_hash = excluded.binary_hash,
                    parameters_schema = excluded.parameters_schema,
                    source_url = excluded.source_url,
                    updated_at = ?11
                "#,
            libsql::params![
                id.to_string(),
                params.user_id.as_str(),
                params.name.as_str(),
                params.version.as_str(),
                params.description.as_str(),
                libsql::Value::Blob(params.wasm_binary),
                libsql::Value::Blob(binary_hash),
                schema_str.as_str(),
                libsql_wasm_opt_text(params.source_url.as_deref()),
                params.trust_level.to_string(),
                now.as_str(),
            ],
        )
        .await
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let mut rows = tx
            .query(
                r#"
                SELECT id, user_id, name, version, description, parameters_schema,
                       source_url, trust_level, status, created_at, updated_at
                FROM wasm_tools
                WHERE user_id = ?1 AND name = ?2 AND version = ?3
                "#,
                libsql::params![params.user_id.as_str(), params.name.as_str(), params.version.as_str()],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let row = rows
            .next()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?
            .ok_or_else(|| WasmStorageError::Database("insert succeeded but row missing".into()))?;

        let tool = libsql_row_to_tool(&row)?;

        tx.commit()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        Ok(tool)
    }

    async fn get(&self, user_id: &str, name: &str) -> Result<StoredWasmTool, WasmStorageError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                r#"
                SELECT id, user_id, name, version, description, parameters_schema,
                       source_url, trust_level, status, created_at, updated_at
                FROM wasm_tools
                WHERE user_id = ?1 AND name = ?2 AND status = 'active'
                ORDER BY version DESC
                LIMIT 1
                "#,
                libsql::params![user_id, name],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let row = rows
            .next()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?
            .ok_or_else(|| WasmStorageError::NotFound(name.to_string()))?;
        libsql_row_to_tool(&row)
    }

    async fn get_with_binary(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<StoredWasmToolWithBinary, WasmStorageError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                r#"
                SELECT id, user_id, name, version, description,
                       wasm_binary, binary_hash, parameters_schema,
                       source_url, trust_level, status, created_at, updated_at
                FROM wasm_tools
                WHERE user_id = ?1 AND name = ?2 AND status = 'active'
                ORDER BY version DESC
                LIMIT 1
                "#,
                libsql::params![user_id, name],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let row = rows
            .next()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?
            .ok_or_else(|| WasmStorageError::NotFound(name.to_string()))?;

        let tool = libsql_row_to_tool_with_offset(&row)?;

        let wasm_binary: Vec<u8> = row
            .get(5)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;
        let binary_hash: Vec<u8> = row
            .get(6)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        if !verify_binary_integrity(&wasm_binary, &binary_hash) {
            return Err(WasmStorageError::IntegrityViolation);
        }

        Ok(StoredWasmToolWithBinary {
            tool,
            wasm_binary,
            binary_hash,
        })
    }

    async fn list(&self, user_id: &str) -> Result<Vec<StoredWasmTool>, WasmStorageError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                r#"
                SELECT id, user_id, name, version, description, parameters_schema,
                       source_url, trust_level, status, created_at, updated_at
                FROM wasm_tools
                WHERE user_id = ?1
                ORDER BY name, version DESC
                "#,
                libsql::params![user_id],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let mut out = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?
        {
            out.push(libsql_row_to_tool(&row)?);
        }
        Ok(out)
    }

    async fn update_status(
        &self,
        user_id: &str,
        name: &str,
        status: ToolStatus,
    ) -> Result<(), WasmStorageError> {
        let conn = self.connect().await?;
        let updated = conn
            .execute(
                "UPDATE wasm_tools SET status = ?1, updated_at = ?2 WHERE user_id = ?3 AND name = ?4",
                libsql::params![
                    status.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    user_id,
                    name
                ],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        if updated == 0 {
            return Err(WasmStorageError::NotFound(name.to_string()));
        }
        Ok(())
    }

    async fn delete(&self, user_id: &str, name: &str) -> Result<bool, WasmStorageError> {
        let conn = self.connect().await?;
        let deleted = conn
            .execute(
                "DELETE FROM wasm_tools WHERE user_id = ?1 AND name = ?2",
                libsql::params![user_id, name],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;
        Ok(deleted > 0)
    }

    async fn get_capabilities(
        &self,
        tool_id: Uuid,
    ) -> Result<Option<StoredCapabilities>, WasmStorageError> {
        let conn = self.connect().await?;
        let mut rows = conn
            .query(
                "SELECT capabilities_json FROM wasm_tool_capabilities WHERE tool_id = ?1",
                libsql::params![tool_id.to_string()],
            )
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;

        let Some(row) = rows
            .next()
            .await
            .map_err(|e| WasmStorageError::Database(e.to_string()))?
        else {
            return Ok(None);
        };

        let s: String = row
            .get(0)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?;
        let file =
            CapabilitiesFile::from_json(&s).map_err(|e| WasmStorageError::InvalidData(e.to_string()))?;
        Ok(Some(StoredCapabilities {
            tool_id,
            capabilities_file: file,
        }))
    }

    async fn set_capabilities(
        &self,
        tool_id: Uuid,
        capabilities_file: &CapabilitiesFile,
    ) -> Result<(), WasmStorageError> {
        let conn = self.connect().await?;
        let caps_json = serde_json::to_string(capabilities_file)
            .map_err(|e| WasmStorageError::InvalidData(e.to_string()))?;
        conn.execute(
            r#"
            INSERT INTO wasm_tool_capabilities (tool_id, capabilities_json, created_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(tool_id) DO UPDATE SET capabilities_json = excluded.capabilities_json
            "#,
            libsql::params![
                tool_id.to_string(),
                caps_json.as_str(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ],
        )
        .await
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
        Ok(())
    }
}

fn libsql_wasm_opt_text(s: Option<&str>) -> libsql::Value {
    match s {
        Some(s) => libsql::Value::Text(s.to_string()),
        None => libsql::Value::Null,
    }
}

fn libsql_wasm_parse_ts(s: &str) -> Result<DateTime<Utc>, WasmStorageError> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| WasmStorageError::InvalidData(format!("bad timestamp: {}", e)))
}

fn libsql_row_to_tool(row: &libsql::Row) -> Result<StoredWasmTool, WasmStorageError> {
    libsql_row_to_tool_at(row, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10)
}

fn libsql_row_to_tool_with_offset(row: &libsql::Row) -> Result<StoredWasmTool, WasmStorageError> {
    // when binary columns are present:
    // id(0), user_id(1), name(2), version(3), description(4),
    // wasm_binary(5), binary_hash(6),
    // parameters_schema(7), source_url(8), trust_level(9), status(10),
    // created_at(11), updated_at(12)
    libsql_row_to_tool_at(row, 0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 12)
}

#[allow(clippy::too_many_arguments)]
fn libsql_row_to_tool_at(
    row: &libsql::Row,
    id_idx: i32,
    user_id_idx: i32,
    name_idx: i32,
    version_idx: i32,
    description_idx: i32,
    schema_idx: i32,
    source_url_idx: i32,
    trust_level_idx: i32,
    status_idx: i32,
    created_at_idx: i32,
    updated_at_idx: i32,
) -> Result<StoredWasmTool, WasmStorageError> {
    let id_str: String = row
        .get(id_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
    let trust_level_str: String = row
        .get(trust_level_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
    let status_str: String = row
        .get(status_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
    let schema_str: String = row
        .get(schema_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
    let created_at_str: String = row
        .get(created_at_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;
    let updated_at_str: String = row
        .get(updated_at_idx)
        .map_err(|e| WasmStorageError::Database(e.to_string()))?;

    Ok(StoredWasmTool {
        id: id_str
            .parse()
            .map_err(|e: uuid::Error| WasmStorageError::InvalidData(e.to_string()))?,
        user_id: row
            .get(user_id_idx)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?,
        name: row
            .get(name_idx)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?,
        version: row
            .get(version_idx)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?,
        description: row
            .get(description_idx)
            .map_err(|e| WasmStorageError::Database(e.to_string()))?,
        parameters_schema: serde_json::from_str(&schema_str).unwrap_or_default(),
        source_url: row
            .get::<String>(source_url_idx)
            .ok()
            .filter(|s| !s.is_empty()),
        trust_level: trust_level_str
            .parse()
            .map_err(WasmStorageError::InvalidData)?,
        status: status_str.parse().map_err(WasmStorageError::InvalidData)?,
        created_at: libsql_wasm_parse_ts(&created_at_str)?,
        updated_at: libsql_wasm_parse_ts(&updated_at_str)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let binary = b"(module)";
        let hash = compute_binary_hash(binary);
        assert_eq!(hash.len(), 32); // BLAKE3 produces 32-byte hash
    }

    #[test]
    fn test_verify_integrity_success() {
        let binary = b"test wasm binary content";
        let hash = compute_binary_hash(binary);
        assert!(verify_binary_integrity(binary, &hash));
    }
}
