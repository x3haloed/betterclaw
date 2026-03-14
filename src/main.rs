use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
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
    logging::init()?;

    let db_path = env::var("BETTERCLAW_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("betterclaw.db"));
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
