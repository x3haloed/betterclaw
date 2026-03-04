//! Background ledger embedding indexer.
//!
//! Chunks and embeds new (non-derived) ledger events into `ledger_event_chunks`.

use std::sync::Arc;

use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};

use crate::config::LedgerIndexConfig;
use crate::db::Database;
use crate::workspace::{ChunkConfig, EmbeddingProvider, chunk_by_paragraphs};

const CURSOR_KEY: &str = "ledger.index.cursor.v0";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LedgerIndexCursorV0 {
    created_at: String,
    id: String,
}

fn is_indexable_kind(kind: &str) -> bool {
    // Keep this conservative: avoid tool noise by default.
    // Users can always force re-indexing by wiping cursor/chunks later.
    if kind == "user_turn" || kind == "agent_turn" {
        return true;
    }
    if kind.starts_with("note.") || kind.starts_with("isnad.") || kind.starts_with("memory.") {
        return true;
    }
    false
}

fn nomic_prefix_for_document(model: &str) -> Option<&'static str> {
    if model.contains("nomic-embed-text") {
        Some("search_document: ")
    } else {
        None
    }
}

fn clamp_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let byte_offset = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s[..byte_offset].to_string()
}

fn vec_f32_to_json_array(v: &[f32]) -> Option<String> {
    serde_json::to_string(v).ok()
}

/// Run a single indexing sweep (bounded by cfg.batch_events).
pub(crate) async fn index_sweep(
    store: &Arc<dyn Database>,
    embeddings: &Arc<dyn EmbeddingProvider>,
    cfg: &LedgerIndexConfig,
) -> Result<i64, crate::error::DatabaseError> {
    // Load cursor from settings.
    let cursor_val = store.get_setting(&cfg.user_id, CURSOR_KEY).await.ok().flatten();
    let cursor: Option<LedgerIndexCursorV0> =
        cursor_val.and_then(|v| serde_json::from_value(v).ok());

    let after_created_at = cursor.as_ref().map(|c| c.created_at.as_str());
    let after_id = cursor.as_ref().map(|c| c.id.as_str());

    let events = store
        .list_ledger_events_after_for_compression(
            &cfg.user_id,
            after_created_at,
            after_id,
            cfg.batch_events,
        )
        .await?;

    if events.is_empty() {
        return Ok(0);
    }

    let chunk_cfg = ChunkConfig {
        chunk_size: cfg.chunk_size_words,
        overlap_percent: cfg.overlap_percent,
        min_chunk_size: cfg.min_chunk_size_words,
    };

    let doc_prefix = nomic_prefix_for_document(embeddings.model_name()).unwrap_or("");

    let mut indexed = 0i64;

    for ev in &events {
        let cursor_next = LedgerIndexCursorV0 {
            created_at: ev
                .created_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            id: ev.id.to_string(),
        };

        let Some(content) = ev.content.as_deref() else {
            // No content: advance cursor.
            let _ = store
                .set_setting(
                    &cfg.user_id,
                    CURSOR_KEY,
                    &serde_json::to_value(&cursor_next)
                        .unwrap_or_else(|_| serde_json::json!({})),
                )
                .await;
            continue;
        };

        if !is_indexable_kind(&ev.kind) {
            // Intentionally not indexed: still advance cursor.
            let _ = store
                .set_setting(
                    &cfg.user_id,
                    CURSOR_KEY,
                    &serde_json::to_value(&cursor_next)
                        .unwrap_or_else(|_| serde_json::json!({})),
                )
                .await;
            continue;
        }

        let content = clamp_chars(content, cfg.max_chunk_chars * 4); // allow chunker to see more

        // Chunk and clamp each chunk to both cfg and provider max length.
        let mut chunks = chunk_by_paragraphs(&content, chunk_cfg.clone());
        if chunks.is_empty() {
            continue;
        }

        // Re-index: delete existing chunks for this event_id (idempotence).
        store
            .delete_ledger_event_chunks_for_event(&cfg.user_id, ev.id)
            .await?;

        let max_in = embeddings.max_input_length().min(cfg.max_chunk_chars);
        for c in &mut chunks {
            *c = clamp_chars(c, max_in);
        }

        // Embed in one batch per event (keeps ordering stable).
        let to_embed: Vec<String> = chunks
            .iter()
            .map(|c| format!("{doc_prefix}{c}"))
            .collect();

        let embs = match embeddings.embed_batch(&to_embed).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    event_id = %ev.id,
                    kind = %ev.kind,
                    error = %e,
                    "Ledger indexer: embedding failed for event (skipping)"
                );
                // Do not advance cursor on embedding errors: retry next sweep.
                break;
            }
        };

        let mut all_ok = true;
        for (i, (chunk, emb)) in chunks.into_iter().zip(embs.into_iter()).enumerate() {
            let embedding_json = vec_f32_to_json_array(&emb);
            if let Err(e) = store
                .upsert_ledger_event_chunk(
                    &cfg.user_id,
                    ev.id,
                    i as i64,
                    &chunk,
                    embedding_json.as_deref(),
                )
                .await
            {
                tracing::warn!(
                    event_id = %ev.id,
                    chunk_index = i,
                    error = %e,
                    "Ledger indexer: failed to upsert chunk"
                );
                all_ok = false;
                break;
            }
        }

        if all_ok {
            indexed += 1;

            // Advance cursor only after successfully indexing this event.
            store
                .set_setting(
                    &cfg.user_id,
                    CURSOR_KEY,
                    &serde_json::to_value(&cursor_next)
                        .unwrap_or_else(|_| serde_json::json!({})),
                )
                .await?;
        } else {
            // Do not advance cursor: retry this event next sweep.
            break;
        }
    }

    Ok(indexed)
}

/// Spawn the ledger indexer loop.
pub fn spawn_ledger_indexer(
    store: Arc<dyn Database>,
    embeddings: Arc<dyn EmbeddingProvider>,
    cfg: LedgerIndexConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if cfg.startup_delay_secs > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(cfg.startup_delay_secs)).await;
        }

        loop {
            match index_sweep(&store, &embeddings, &cfg).await {
                Ok(0) => {
                    tokio::time::sleep(std::time::Duration::from_secs(cfg.interval_secs)).await;
                }
                Ok(n) => {
                    tracing::info!(indexed_events = n, "Ledger indexer sweep completed");
                    // Short yield: keep burning backlog fast until caught up.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    tracing::warn!("Ledger indexer sweep failed: {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(cfg.interval_secs)).await;
                }
            }
        }
    })
}
