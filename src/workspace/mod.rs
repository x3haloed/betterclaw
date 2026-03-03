//! Workspace utilities.
//!
//! BetterClaw treats "workspace files" as real files on disk (see `FsWorkspace`).
//! Persistent memory/recall is handled separately via the append-only ledger.

mod embeddings;
mod fs_workspace;

pub mod hygiene;
pub mod paths;

pub use embeddings::{
    EmbeddingProvider, MockEmbeddings, OllamaEmbeddings, OpenAiEmbeddings,
};
pub use fs_workspace::FsWorkspace;
