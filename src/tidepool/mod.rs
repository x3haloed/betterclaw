use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use tokio::sync::RwLock;

pub mod client;

pub use client::{
    TidepoolBootstrapOutcome, TidepoolClient, TidepoolConfig, TidepoolInboundMessage,
};

static SHARED_TIDEPOOL_CLIENT: OnceLock<RwLock<Option<TidepoolClient>>> = OnceLock::new();

fn shared_client_slot() -> &'static RwLock<Option<TidepoolClient>> {
    SHARED_TIDEPOOL_CLIENT.get_or_init(|| RwLock::new(None))
}

pub async fn connect_shared_client(config: TidepoolConfig) -> Result<TidepoolClient> {
    let client = TidepoolClient::connect(config).await?;
    replace_shared_client(client.clone()).await;
    Ok(client)
}

pub async fn shared_client() -> Option<TidepoolClient> {
    shared_client_slot().read().await.clone()
}

pub async fn require_shared_client() -> Result<TidepoolClient> {
    shared_client()
        .await
        .ok_or_else(|| anyhow!("Tidepool channel is not active; no shared Tidepool client is available"))
}

pub async fn clear_shared_client() {
    let previous = {
        let mut slot = shared_client_slot().write().await;
        slot.take()
    };
    if let Some(client) = previous {
        client.shutdown().await;
    }
}

async fn replace_shared_client(client: TidepoolClient) {
    let previous = {
        let mut slot = shared_client_slot().write().await;
        slot.replace(client)
    };
    if let Some(old) = previous {
        old.shutdown().await;
    }
}
