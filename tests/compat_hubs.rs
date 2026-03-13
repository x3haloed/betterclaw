use betterclaw::config::{
    CompressorLoopConfig, DatabaseConfig, EmbeddingsConfig, LedgerIndexConfig,
    LedgerRecallConfig,
};
use betterclaw::db::{Database, LedgerChunkHit, LedgerChunkStore, LedgerStore, WorkspaceStore};
use betterclaw::tools::builtin::{
    MemoryReadTool, MemorySearchTool, MemoryTreeTool, MemoryWriteTool,
};
use betterclaw::workspace::{ChunkConfig, FsWorkspace, SearchConfig};

#[test]
fn config_hub_exposes_continuity_types() {
    fn assert_default<T: Default>() {}

    assert_default::<CompressorLoopConfig>();
    assert_default::<LedgerIndexConfig>();
    assert_default::<LedgerRecallConfig>();

    let _ = std::mem::size_of::<DatabaseConfig>();
    let _ = std::mem::size_of::<EmbeddingsConfig>();
}

#[test]
fn db_hub_exposes_ledger_and_workspace_traits() {
    fn assert_database_shape<T: ?Sized + Database + LedgerStore + LedgerChunkStore + WorkspaceStore>() {}

    assert_database_shape::<dyn Database>();
    let _ = std::mem::size_of::<LedgerChunkHit>();
}

#[test]
fn workspace_and_builtin_hubs_expose_document_memory_surfaces() {
    let _ = std::mem::size_of::<ChunkConfig>();
    let _ = std::mem::size_of::<SearchConfig>();
    let _ = std::mem::size_of::<FsWorkspace>();
    let _ = std::mem::size_of::<MemorySearchTool>();
    let _ = std::mem::size_of::<MemoryWriteTool>();
    let _ = std::mem::size_of::<MemoryReadTool>();
    let _ = std::mem::size_of::<MemoryTreeTool>();
}
