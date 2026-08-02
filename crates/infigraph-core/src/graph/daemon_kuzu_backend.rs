use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::learned::LearnedStore;
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;

use super::backend::{CallsServiceEdge, CrossServiceEdgeCandidate, GraphBackend};
use super::kuzu_backend::KuzuBackend;
use super::{
    ApiSymbol, ArchitectureStats, BranchInfo, ComplexityRow, DeadCodeRow, FileDeps, GraphStats,
    ImpactRow, ReferenceRow, SymbolDetail, SymbolMeta, SymbolRow, SymbolWithDocstring, TestContext,
    TestCoverage, TypeHierarchy,
};

/// Routes writes through the DaemonKuzu file-drop protocol instead of
/// opening a direct embedded Kuzu connection. See
/// docs/superpowers/specs/2026-08-01-daemonkuzu-daemon-wiring-design.md.
///
/// Three-tier contract:
/// 1. Reads delegate to `read_conn`, a real directly-opened read-only
///    Kuzu connection -- reads never route through the daemon.
/// 2. The write methods covered by WriteRequest (see daemon_protocol.rs)
///    route through submit_write_request (Task 13).
/// 3. Any other write method returns a clear error rather than silently
///    writing through read_conn (which would fail at the DB level, per
///    read_only_connection_rejects_write_statements) or reintroducing a
///    real collision some other way.
pub struct DaemonKuzuBackend {
    read_conn: KuzuBackend,
    // Unused until Task 13 wires submit_write_request, which needs the
    // project root to locate the daemon's staging directory.
    #[allow(dead_code)]
    root: std::path::PathBuf,
}

impl DaemonKuzuBackend {
    pub fn open(root: &Path) -> Result<Self> {
        let db_path = root.join(".infigraph").join("graph");
        let read_conn = KuzuBackend::open_read_only(&db_path)?;
        Ok(Self {
            read_conn,
            root: root.to_path_buf(),
        })
    }

    fn not_supported(method: &str, alternative: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "not supported via direct backend access under DaemonKuzu -- use {alternative} instead ({method})"
        )
    }
}

// clear_all_data is deliberately left un-overridden: the trait's own
// default (a no-op, backend.rs's `clear_all_data`) is the correct
// behavior for DaemonKuzu too, matching KuzuBackend's own reliance on the
// same default -- this is a deliberate choice, not an oversight.
impl GraphBackend for DaemonKuzuBackend {
    // ── Tier 1: reads pass through to the real read-only connection ──

    fn stats(&self) -> Result<GraphStats> {
        self.read_conn.stats()
    }
    fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        self.read_conn.get_file_hashes()
    }
    fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        self.read_conn.get_all_symbols()
    }
    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> {
        self.read_conn.symbols_in_file(file)
    }
    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>> {
        self.read_conn.find_symbol_by_id(id)
    }
    fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> {
        self.read_conn.symbols_in_range(file, start, end)
    }
    fn skeleton(&self, file: &str) -> Result<String> {
        self.read_conn.skeleton(file)
    }
    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.read_conn.callers_of(symbol_id)
    }
    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.read_conn.callees_of(symbol_id)
    }
    fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> {
        self.read_conn.branches_of(symbol_id)
    }
    fn transitive_impact(&self, id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        self.read_conn.transitive_impact(id, max_depth)
    }
    fn find_all_references(&self, id: &str) -> Result<Vec<ReferenceRow>> {
        self.read_conn.find_all_references(id)
    }
    fn cross_cutting_for(&self, id: &str) -> Result<Vec<(String, String)>> {
        self.read_conn.cross_cutting_for(id)
    }
    fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> {
        self.read_conn.get_api_surface()
    }
    fn get_file_deps(&self, file: &str) -> Result<FileDeps> {
        self.read_conn.get_file_deps(file)
    }
    fn get_type_hierarchy(&self, id: &str, max_depth: u32) -> Result<TypeHierarchy> {
        self.read_conn.get_type_hierarchy(id, max_depth)
    }
    fn get_test_coverage(&self) -> Result<TestCoverage> {
        self.read_conn.get_test_coverage()
    }
    fn generate_test_context(
        &self,
        file_filter: Option<&str>,
        limit: usize,
        test_type: Option<&str>,
    ) -> Result<TestContext> {
        self.read_conn
            .generate_test_context(file_filter, limit, test_type)
    }
    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> {
        self.read_conn.raw_query(query)
    }
    fn get_symbols_for_search(&self) -> Result<Vec<Vec<String>>> {
        self.read_conn.get_symbols_for_search()
    }
    fn symbol_metadata(&self, id: &str) -> Result<Option<SymbolMeta>> {
        self.read_conn.symbol_metadata(id)
    }
    fn get_complexity_ranking(&self, file_filter: Option<&str>) -> Result<Vec<ComplexityRow>> {
        self.read_conn.get_complexity_ranking(file_filter)
    }
    fn list_indexed_files(&self) -> Result<Vec<String>> {
        self.read_conn.list_indexed_files()
    }
    fn find_uncalled_symbols(&self) -> Result<Vec<DeadCodeRow>> {
        self.read_conn.find_uncalled_symbols()
    }
    fn get_architecture_stats(&self) -> Result<ArchitectureStats> {
        self.read_conn.get_architecture_stats()
    }
    fn symbols_with_docstring(
        &self,
        kind_filter: Option<&[&str]>,
    ) -> Result<Vec<SymbolWithDocstring>> {
        self.read_conn.symbols_with_docstring(kind_filter)
    }
    fn repo_filter(&self) -> Option<&str> {
        self.read_conn.repo_filter()
    }

    // ── Tier 3 placeholders: Task 13 replaces each of these with a Tier 2
    //    submit_write_request call. Left as loud errors here so this task
    //    compiles as a complete GraphBackend impl on its own. ──

    fn upsert_similar_edge(&self, _id_a: &str, _id_b: &str, _score: f32) -> Result<()> {
        Err(Self::not_supported(
            "upsert_similar_edge",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn upsert_file(&self, _extraction: &FileExtraction) -> Result<()> {
        Err(Self::not_supported(
            "upsert_file",
            "Infigraph::index()/index_files()",
        ))
    }
    fn upsert_files_bulk(
        &self,
        _extractions: &[FileExtraction],
        _existing_hashes_empty: bool,
    ) -> Result<()> {
        Err(Self::not_supported(
            "upsert_files_bulk",
            "Infigraph::index()/index_files()",
        ))
    }
    fn remove_file(&self, _file: &str) -> Result<()> {
        Err(Self::not_supported(
            "remove_file",
            "Infigraph::index()/index_files() (internal only)",
        ))
    }
    fn derive_tested_by_edges(&self, _changed_files: Option<&[&str]>) -> Result<usize> {
        Err(Self::not_supported(
            "derive_tested_by_edges",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn upsert_repo(&self, _repo_name: &str) -> Result<()> {
        // Deliberately overridden (not left as the trait's no-op default)
        // -- see Task 6's warning about the inherited-default trap.
        Err(Self::not_supported(
            "upsert_repo",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn write_calls_service_edges(&self, _edges: &[CallsServiceEdge]) -> Result<()> {
        Err(Self::not_supported(
            "write_calls_service_edges",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn resolve_calls(
        &self,
        _extractions: &[FileExtraction],
        _learned: Option<&LearnedStore>,
    ) -> Result<ResolveStats> {
        Err(Self::not_supported(
            "resolve_calls",
            "Infigraph::index()/index_files() (internal only)",
        ))
    }
    fn re_resolve_for_files(
        &self,
        _files: &[String],
        _extractions: &[FileExtraction],
        _learned: Option<&LearnedStore>,
    ) -> Result<ResolveStats> {
        Err(Self::not_supported(
            "re_resolve_for_files",
            "Infigraph::index()/index_files() (internal only)",
        ))
    }
    fn import_scip_index(
        &self,
        _index_path: &Path,
        _project_root: Option<&Path>,
    ) -> Result<crate::scip::ImportStats> {
        Err(Self::not_supported(
            "import_scip_index",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn ingest_structured_data(
        &self,
        _schema: &crate::structured::SchemaMeta,
        _data: &[serde_json::Value],
    ) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported(
            "ingest_structured_data",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn ingest_structured_file(
        &self,
        _schema: &crate::structured::SchemaMeta,
        _path: &Path,
    ) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported(
            "ingest_structured_file",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn ingest_structured_directory(
        &self,
        _schema: &crate::structured::SchemaMeta,
        _dir: &Path,
    ) -> Result<crate::structured::IngestResult> {
        Err(Self::not_supported(
            "ingest_structured_directory",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn upsert_dependencies(&self, _result: &crate::manifest::ManifestResult) -> Result<()> {
        Err(Self::not_supported(
            "upsert_dependencies",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn store_clusters(
        &self,
        _idx_to_id: &[String],
        _community: &[usize],
        _modularity: f64,
    ) -> Result<crate::cluster::ClusterStats> {
        Err(Self::not_supported(
            "store_clusters",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn store_config_bindings(&self, _bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        Err(Self::not_supported(
            "store_config_bindings",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
    fn write_cross_service_edges(
        &self,
        _candidates: &[CrossServiceEdgeCandidate],
    ) -> Result<usize> {
        Err(Self::not_supported(
            "write_cross_service_edges",
            "Infigraph's daemon protocol (wired in Task 13)",
        ))
    }
}
