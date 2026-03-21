use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use betterclaw::channels::discord::{DiscordChannel, DiscordConfig};
use betterclaw::channels::tidepool::TidepoolChannel;
use betterclaw::db::Db;
use betterclaw::logging;
use betterclaw::runtime::Runtime;
use betterclaw::tidepool::TidepoolConfig;
use betterclaw::web;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let loaded_env_path = load_startup_env()?;
    logging::init()?;

    if let Some(env_path) = loaded_env_path.as_ref() {
        tracing::info!(env_path = %env_path.display(), "Loaded startup environment file");
    }

    let db_path = env::var("BETTERCLAW_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| default_db_path());
    if matches!(env::args().nth(1).as_deref(), Some("memory-rebuild")) {
        let db = Db::open(&db_path).await?;
        let runtime = Runtime::from_env(db).await?;
        let namespace = env::args()
            .skip(2)
            .find_map(|arg| arg.strip_prefix("--namespace=").map(str::to_string))
            .unwrap_or_else(|| "default".to_string());
        let report = runtime.rebuild_memory_namespace(&namespace).await?;
        println!(
            "memory rebuild complete: namespace={} turns_processed={}",
            report.namespace_id, report.turns_processed
        );
        return Ok(());
    }
    let db = Db::open(&db_path).await?;
    let runtime = Arc::new(Runtime::from_env(db).await?);

    let port = env::var("BETTERCLAW_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(3000);
    let address = SocketAddr::from(([127, 0, 0, 1], port));

    tracing::info!(db_path = %db_path.display(), %address, "Starting BetterClaw");

    let _discord = if let Some(config) = DiscordConfig::from_env() {
        tracing::info!("Starting Discord channel");
        Some(DiscordChannel::new(Arc::clone(&runtime), config)?.spawn())
    } else {
        tracing::info!("Discord channel disabled (DISCORD_BOT_TOKEN not set)");
        None
    };

    let _tidepool = if let Some(config) = TidepoolConfig::from_env() {
        if config.token_exists() {
            tracing::info!(token_path = %config.token_path.display(), "Starting Tidepool channel");
            Some(TidepoolChannel::new(Arc::clone(&runtime), config).spawn())
        } else {
            tracing::info!(
                token_path = %config.token_path.display(),
                "Tidepool configured but inactive because the token file is missing"
            );
            None
        }
    } else {
        tracing::info!("Tidepool channel disabled (TIDEPOOL_DATABASE/TIDEPOOL_HANDLE not set)");
        None
    };

    let app = web::app(runtime);
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn load_startup_env() -> Result<Option<PathBuf>> {
    let env_path = env::var("BETTERCLAW_ENV_PATH")
        .ok()
        .map(PathBuf::from)
        .or_else(default_env_path);

    let Some(env_path) = env_path else {
        return Ok(None);
    };

    if !env_path.exists() {
        return Ok(None);
    }

    dotenvy::from_path(&env_path).with_context(|| {
        format!(
            "failed to load startup environment file {}",
            env_path.display()
        )
    })?;
    Ok(Some(env_path))
}

fn default_env_path() -> Option<PathBuf> {
    dirs::home_dir().map(|path| path.join(".betterclaw").join(".env"))
}

fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .map(|path| path.join(".betterclaw").join("betterclaw.db"))
        .unwrap_or_else(|| PathBuf::from("betterclaw.db"))
}

#[cfg(test)]
mod tests {
    use super::{default_db_path, default_env_path};

    #[test]
    fn default_env_path_targets_betterclaw_home() {
        let path = default_env_path().expect("home directory should exist in test environment");
        assert!(path.ends_with(".betterclaw/.env"));
    }

    #[test]
    fn default_db_path_targets_betterclaw_home() {
        let path = default_db_path();
        assert!(path.ends_with(".betterclaw/betterclaw.db"));
    }
}
