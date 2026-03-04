//! Workspace utilities.
//!
//! BetterClaw treats "workspace files" as real files on disk (see `FsWorkspace`).
//! Persistent memory/recall is handled separately via the append-only ledger.

mod embeddings;
mod chunker;
mod fs_workspace;

pub mod hygiene;
pub mod paths;

pub use embeddings::{
    EmbeddingProvider, MockEmbeddings, OllamaEmbeddings, OpenAiCompatibleEmbeddings, OpenAiEmbeddings,
};
pub use chunker::{ChunkConfig, chunk_by_paragraphs, chunk_document};
pub use fs_workspace::FsWorkspace;
