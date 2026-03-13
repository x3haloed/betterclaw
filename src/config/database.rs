use std::path::PathBuf;

use secrecy::SecretString;

use crate::bootstrap::betterclaw_base_dir;
use crate::config::helpers::optional_env;
use crate::error::ConfigError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
}

impl SslMode {
    pub fn from_env() -> Self {
        match std::env::var("DATABASE_SSL_MODE")
            .unwrap_or_else(|_| "prefer".to_string())
            .to_lowercase()
            .as_str()
        {
            "disable" => Self::Disable,
            "require" => Self::Require,
            _ => Self::Prefer,
        }
    }
}

/// Which database backend to use.
///
/// BetterClaw is currently libsql-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DatabaseBackend {
    /// libSQL/Turso embedded database.
    #[default]
    LibSql,
}

impl std::fmt::Display for DatabaseBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LibSql => write!(f, "libsql"),
        }
    }
}

impl std::str::FromStr for DatabaseBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "libsql" | "turso" | "sqlite" => Ok(Self::LibSql),
            _ => Err(format!(
                "invalid database backend '{}', expected 'libsql'",
                s
            )),
        }
    }
}

/// Database configuration (libsql-only).
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    /// Which backend to use (default: libsql).
    pub backend: DatabaseBackend,
    /// Legacy compatibility field for callers that still expect a URL.
    pub url: SecretString,
    /// Legacy compatibility field for callers that still expect pool sizing.
    pub pool_size: usize,
    /// Legacy compatibility field for callers that still expect SSL mode.
    pub ssl_mode: SslMode,

    /// Path to local libSQL database file (default: ~/.betterclaw/betterclaw.db).
    pub libsql_path: Option<PathBuf>,
    /// Turso cloud URL for remote sync (optional).
    pub libsql_url: Option<String>,
    /// Turso auth token (required when libsql_url is set).
    pub libsql_auth_token: Option<SecretString>,
}

impl DatabaseConfig {
    pub(crate) fn resolve() -> Result<Self, ConfigError> {
        let backend: DatabaseBackend = if let Some(b) = optional_env("DATABASE_BACKEND")? {
            b.parse().map_err(|e| ConfigError::InvalidValue {
                key: "DATABASE_BACKEND".to_string(),
                message: e,
            })?
        } else {
            DatabaseBackend::default()
        };

        let libsql_path = optional_env("LIBSQL_PATH")?
            .map(PathBuf::from)
            .or_else(|| Some(default_libsql_path()));

        let libsql_url = optional_env("LIBSQL_URL")?;
        let libsql_auth_token = optional_env("LIBSQL_AUTH_TOKEN")?.map(SecretString::from);

        if libsql_url.is_some() && libsql_auth_token.is_none() {
            return Err(ConfigError::MissingRequired {
                key: "LIBSQL_AUTH_TOKEN".to_string(),
                hint: "LIBSQL_AUTH_TOKEN is required when LIBSQL_URL is set".to_string(),
            });
        }

        Ok(Self {
            backend,
            url: SecretString::from("unused://libsql".to_string()),
            pool_size: 1,
            ssl_mode: SslMode::from_env(),
            libsql_path,
            libsql_url,
            libsql_auth_token,
        })
    }
}

/// Default libSQL database path (~/.betterclaw/betterclaw.db).
pub fn default_libsql_path() -> PathBuf {
    betterclaw_base_dir().join("betterclaw.db")
}
