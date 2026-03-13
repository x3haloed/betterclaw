//! Best-effort ledger capture helpers.
//!
//! The ledger is append-only and should never be allowed to break the main
//! interaction loop. These helpers swallow errors after logging.

use std::sync::Arc;

use uuid::Uuid;

use crate::db::Database;
use crate::ledger::NewLedgerEvent;

/// Append a ledger event without ever failing the caller.
pub async fn append_event_best_effort(
    store: Option<Arc<dyn Database>>,
    user_id: String,
    episode_id: Option<Uuid>,
    kind: String,
    source: String,
    content: Option<String>,
    payload: serde_json::Value,
) -> Option<Uuid> {
    let Some(store) = store else {
        return None;
    };

    // Do not truncate by default. Turns (`user_turn`, `agent_turn`) should be captured
    // verbatim so the ledger remains a faithful transcript.
    let payload_ref = payload;
    let kind_ref = kind;
    let source_ref = source;

    let ev = NewLedgerEvent {
        user_id: &user_id,
        episode_id,
        kind: &kind_ref,
        source: &source_ref,
        content: content.as_deref(),
        payload: &payload_ref,
    };

    match store.append_ledger_event(&ev).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                user_id = user_id,
                kind = kind_ref,
                source = source_ref,
                error = %e,
                "Ledger capture failed"
            );
            None
        }
    }
}
