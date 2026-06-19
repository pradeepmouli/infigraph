pub mod cozo_store;
pub mod parquet_loader;
mod queries;
mod schema;
mod session_store;
pub mod store;
mod store_bench;
mod store_bulk;
mod store_parquet;
pub(crate) mod store_util;
mod store_write;

pub use cozo_store::CozoStore;
pub use queries::{
    format_skeleton, ApiSymbol, BranchInfo, CoverageRow, ExampleTest, FileDeps, GraphQuery,
    HierarchyNode, ImpactRow, ReferenceRow, SkeletonSymbol, SymbolDetail, SymbolRow, TestContext,
    TestCoverage, TestTarget, TypeHierarchy,
};
pub use session_store::{SessionData, SessionStore};
pub use store::{GraphStats, GraphStore};

pub fn schema_ddl() -> Vec<&'static str> {
    let mut all: Vec<&str> = schema::CREATE_SCHEMA.to_vec();
    all.extend_from_slice(schema::MIGRATIONS);
    all
}

pub fn cozo_schema_ddl() -> Vec<&'static str> {
    cozo_store::cozo_schema_ddl()
}
