mod backend;
pub mod cozo_store;
pub mod daemon_kuzu_backend;
pub(crate) mod growth_gate;
mod kuzu_backend;
pub mod lock_probe;
#[cfg(feature = "neo4j")]
mod neo4j_backend;
pub mod parquet_loader;
mod queries;
pub(crate) mod schema;
mod session_store;
pub mod store;
mod store_bench;
mod store_bulk;
mod store_parquet;
pub(crate) mod store_util;
mod store_write;
pub mod test_templates;

pub use backend::{
    filter_dead_code_candidates, CallsServiceEdge, Concern, CrossServiceEdgeCandidate,
    GraphBackend, ResolvesToEdge,
};
pub use cozo_store::CozoStore;
pub use daemon_kuzu_backend::DaemonKuzuBackend;
pub use kuzu_backend::KuzuBackend;
#[cfg(feature = "neo4j")]
pub use neo4j_backend::Neo4jBackend;
pub use queries::{
    format_skeleton, ApiSymbol, ArchitectureStats, BranchInfo, ComplexityRow, CoverageRow,
    DeadCodeRow, ExampleTest, FileDeps, FileHotspot, GraphQuery, HierarchyNode, HubFunction,
    ImpactRow, KindCount, LanguageCount, ReferenceRow, SkeletonSymbol, SymbolDetail, SymbolMeta,
    SymbolRow, SymbolWithDocstring, TestContext, TestCoverage, TestTarget, TypeHierarchy,
};
pub use session_store::{SessionData, SessionStore};
pub use store::{
    db_lock_path, is_checkpoint_in_progress_error, is_lock_contention_error,
    is_storage_version_mismatch_error, is_transient_open_error, is_transient_wal_open_race_error,
    lock_contention_context, non_corruption_open_context, open_failure_is_not_corruption,
    remove_wal_family, storage_version_mismatch_context, unclean_shutdown_wal_holder,
    validate_db_file, wal_family_paths, DegradeReason, GraphCorruption, GraphStats, GraphStore,
    WriteLock,
};
pub use store_util::stamp_healthy_graph_size;
pub use test_templates::{test_templates_for, TestTemplate};

// Graph-store tunables. The `settings!` field pattern has no attribute
// capture, so per-field docs live on each accessor instead:
// - growth_max_ratio: runaway-growth circuit breaker
//   (`store_util::graph_growth_max_ratio`, #100)
// - quarantine_max_bytes: corrupt-base-image byte cap, 0 disables
//   (`quarantine::quarantine_max_bytes`, R7.3 / #100)
// - slow_lock_ms: slow-acquire recording threshold
//   (`lockfile::slow_wait_threshold`)
// - doc_hnsw_threshold: combined-docs HNSW build threshold; also readable
//   by its pre-macro upstream name `INFIGRAPH_DOC_HNSW_THRESHOLD`
//   (`infigraph-docs` `combined_hnsw_threshold`)
crate::settings! {
    graph {
        growth_max_ratio: u64 = 10,
        quarantine_max_bytes: u64 = 1024 * 1024 * 1024,
        slow_lock_ms: u64 = 2000,
        doc_hnsw_threshold: u64 = 200_000,
    }
}

pub fn schema_ddl() -> Vec<&'static str> {
    let mut all: Vec<&str> = schema::CREATE_SCHEMA.to_vec();
    all.extend_from_slice(schema::MIGRATIONS);
    all
}

pub fn cozo_schema_ddl() -> Vec<&'static str> {
    cozo_store::cozo_schema_ddl()
}
