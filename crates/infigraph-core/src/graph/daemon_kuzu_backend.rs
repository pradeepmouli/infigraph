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
/// 1. Reads delegate to a real directly-opened read-only Kuzu connection --
///    reads never route through the daemon. The connection is opened
///    *fresh per read call* (see `open_read`), not held for the wrapper's
///    lifetime.
/// 2. The write methods covered by WriteRequest (see daemon_protocol.rs)
///    route through submit_write_request (Task 13).
/// 3. Any other write method returns a clear error rather than silently
///    writing through the read connection (which would fail at the DB
///    level, per read_only_connection_rejects_write_statements) or
///    reintroducing a real collision some other way.
pub struct DaemonKuzuBackend {
    db_path: std::path::PathBuf,
    root: std::path::PathBuf,
}

impl DaemonKuzuBackend {
    pub fn open(root: &Path) -> Result<Self> {
        let db_path = root.join(".infigraph").join("graph");
        // Validation probe only -- immediately dropped. Reads open their own
        // connection, but callers (notably `Infigraph::init`'s "daemon" arm,
        // which propagates this `?`) rely on `open` failing eagerly when the
        // graph is missing or unopenable rather than surfacing that on some
        // later read.
        drop(KuzuBackend::open_read_only(&db_path)?);
        Ok(Self {
            db_path,
            root: root.to_path_buf(),
        })
    }

    /// A fresh read-only connection for a single read call.
    ///
    /// Reopening per read is what makes reads see daemon-side commits at
    /// all: a Kuzu embedded read-only `Database` serves the snapshot it
    /// loaded at open time and never observes another process's later
    /// commits, so one connection held for this wrapper's (potentially very
    /// long) lifetime goes permanently stale the moment the daemon writes --
    /// including writes from the daemon's own watcher-driven reindexing or
    /// another client, not just this instance's. A fresh `Connection` on a
    /// held `Database` is not enough; the `Database` itself must be reopened.
    ///
    /// Measured at ~12.6ms, trivial against this backend's 30s-600s write
    /// timeouts. This does not reintroduce the write-amplification bug from
    /// upstream PR #43: that came from reopening a *write* connection, whose
    /// close triggers `forceCheckpointOnClose` and flushes accumulated WAL
    /// into the base file. A read-only connection never writes, so it has
    /// nothing to checkpoint -- verified empirically as zero bytes of file
    /// growth over 100 open/close cycles against a real indexed graph.
    fn open_read(&self) -> Result<KuzuBackend> {
        KuzuBackend::open_read_only(&self.db_path)
    }

    fn not_supported(method: &str, alternative: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "not supported via direct backend access under DaemonKuzu -- use {alternative} instead ({method})"
        )
    }

    fn staging_dir(&self) -> std::path::PathBuf {
        self.root.join(".infigraph").join("requests")
    }
}

// clear_all_data is deliberately left un-overridden: the trait's own
// default (a no-op, backend.rs's `clear_all_data`) is the correct
// behavior for DaemonKuzu too, matching KuzuBackend's own reliance on the
// same default -- this is a deliberate choice, not an oversight.
impl GraphBackend for DaemonKuzuBackend {
    // ── Tier 1: reads pass through to a freshly opened read-only
    //    connection (see `open_read` for why it is not held open) ──

    fn stats(&self) -> Result<GraphStats> {
        self.open_read()?.stats()
    }
    fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        self.open_read()?.get_file_hashes()
    }
    fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        self.open_read()?.get_all_symbols()
    }
    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> {
        self.open_read()?.symbols_in_file(file)
    }
    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>> {
        self.open_read()?.find_symbol_by_id(id)
    }
    fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> {
        self.open_read()?.symbols_in_range(file, start, end)
    }
    fn skeleton(&self, file: &str) -> Result<String> {
        self.open_read()?.skeleton(file)
    }
    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.open_read()?.callers_of(symbol_id)
    }
    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.open_read()?.callees_of(symbol_id)
    }
    fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> {
        self.open_read()?.branches_of(symbol_id)
    }
    fn transitive_impact(&self, id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        self.open_read()?.transitive_impact(id, max_depth)
    }
    fn find_all_references(&self, id: &str) -> Result<Vec<ReferenceRow>> {
        self.open_read()?.find_all_references(id)
    }
    fn cross_cutting_for(&self, id: &str) -> Result<Vec<(String, String)>> {
        self.open_read()?.cross_cutting_for(id)
    }
    fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> {
        self.open_read()?.get_api_surface()
    }
    fn get_file_deps(&self, file: &str) -> Result<FileDeps> {
        self.open_read()?.get_file_deps(file)
    }
    fn get_type_hierarchy(&self, id: &str, max_depth: u32) -> Result<TypeHierarchy> {
        self.open_read()?.get_type_hierarchy(id, max_depth)
    }
    fn get_test_coverage(&self) -> Result<TestCoverage> {
        self.open_read()?.get_test_coverage()
    }
    fn generate_test_context(
        &self,
        file_filter: Option<&str>,
        limit: usize,
        test_type: Option<&str>,
    ) -> Result<TestContext> {
        self.open_read()?
            .generate_test_context(file_filter, limit, test_type)
    }
    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> {
        self.open_read()?.raw_query(query)
    }
    fn get_symbols_for_search(&self) -> Result<Vec<Vec<String>>> {
        self.open_read()?.get_symbols_for_search()
    }
    fn symbol_metadata(&self, id: &str) -> Result<Option<SymbolMeta>> {
        self.open_read()?.symbol_metadata(id)
    }
    fn get_complexity_ranking(&self, file_filter: Option<&str>) -> Result<Vec<ComplexityRow>> {
        self.open_read()?.get_complexity_ranking(file_filter)
    }
    fn list_indexed_files(&self) -> Result<Vec<String>> {
        self.open_read()?.list_indexed_files()
    }
    fn find_uncalled_symbols(&self) -> Result<Vec<DeadCodeRow>> {
        self.open_read()?.find_uncalled_symbols()
    }
    fn get_architecture_stats(&self) -> Result<ArchitectureStats> {
        self.open_read()?.get_architecture_stats()
    }
    fn symbols_with_docstring(
        &self,
        kind_filter: Option<&[&str]>,
    ) -> Result<Vec<SymbolWithDocstring>> {
        self.open_read()?.symbols_with_docstring(kind_filter)
    }
    /// `KuzuBackend` never overrides `repo_filter`; it inherits the trait
    /// default, which is unconditionally `None` (Kuzu is single-repo by
    /// design). Returning `None` directly is therefore behavior-identical to
    /// delegating, and avoids both the pointless open and the borrow of a
    /// connection that would drop at the end of this function.
    fn repo_filter(&self) -> Option<&str> {
        None
    }

    // ── Tier 2: writes covered by WriteRequest route through the daemon
    //    protocol's submit_write_request(_named). ──

    fn upsert_similar_edge(&self, id_a: &str, id_b: &str, score: f32) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::UpsertSimilarEdge {
            id_a: id_a.to_string(),
            id_b: id_b.to_string(),
            score,
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for UpsertSimilarEdge: {other:?}"
            )),
        }
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
    fn derive_tested_by_edges(&self, changed_files: Option<&[&str]>) -> Result<usize> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::DeriveTestedBy {
            files: changed_files.map(|files| files.iter().map(|s| s.to_string()).collect()),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(60),
        )? {
            crate::daemon_protocol::WriteResult::Ok { indexed_files, .. } => Ok(indexed_files),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for DeriveTestedBy: {other:?}"
            )),
        }
    }
    fn upsert_repo(&self, repo_name: &str) -> Result<()> {
        // Deliberately overridden (not left as the trait's no-op default)
        // -- see Task 6's warning about the inherited-default trap.
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::UpsertRepo {
            namespace: repo_name.to_string(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for UpsertRepo: {other:?}"
            )),
        }
    }
    fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let staging_dir = self.staging_dir();
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let edges_path = staging_dir.join(format!("{name}.edges.arrow"));
        crate::daemon_protocol::write_calls_service_edges_arrow(&edges_path, edges)?;

        let request = crate::daemon_protocol::WriteRequest::WriteCallsServiceEdges {
            edges_path: edges_path.clone(),
        };
        match crate::daemon_protocol::submit_write_request_named(
            &staging_dir,
            &name,
            &request,
            std::time::Duration::from_secs(60),
        ) {
            Ok(crate::daemon_protocol::WriteResult::Ok { .. }) => Ok(()),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => {
                Err(anyhow::anyhow!(message))
            }
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected WriteResult for WriteCallsServiceEdges: {other:?}"
            )),
            Err(e) => {
                std::fs::remove_file(&edges_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
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
        index_path: &Path,
        _project_root: Option<&Path>,
    ) -> Result<crate::scip::ImportStats> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::ScipImport {
            scip_path: index_path.to_path_buf(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(120),
        )? {
            crate::daemon_protocol::WriteResult::ScipImportOk(stats) => Ok(stats),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for ScipImport: {other:?}"
            )),
        }
    }
    fn ingest_structured_data(
        &self,
        schema: &crate::structured::SchemaMeta,
        data: &[serde_json::Value],
    ) -> Result<crate::structured::IngestResult> {
        let staging_dir = self.staging_dir();
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let request_path = staging_dir.join(format!("{name}.request"));
        let data_path = crate::daemon_protocol::write_ingest_inline_sibling(&request_path, data)?;

        let request = crate::daemon_protocol::WriteRequest::IngestStructured {
            schema_id: schema.schema_id.clone(),
            source: crate::daemon_protocol::IngestSource::Inline,
        };
        match crate::daemon_protocol::submit_write_request_named(
            &staging_dir,
            &name,
            &request,
            std::time::Duration::from_secs(120),
        ) {
            Ok(crate::daemon_protocol::WriteResult::Ok {
                total_files,
                indexed_files,
            }) => Ok(crate::structured::IngestResult {
                nodes_created: indexed_files,
                edges_created: total_files.saturating_sub(indexed_files),
            }),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => {
                Err(anyhow::anyhow!(message))
            }
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected WriteResult for IngestStructured: {other:?}"
            )),
            Err(e) => {
                std::fs::remove_file(&data_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
    }
    fn ingest_structured_file(
        &self,
        schema: &crate::structured::SchemaMeta,
        path: &Path,
    ) -> Result<crate::structured::IngestResult> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::IngestStructured {
            schema_id: schema.schema_id.clone(),
            source: crate::daemon_protocol::IngestSource::File(path.to_path_buf()),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(120),
        )? {
            crate::daemon_protocol::WriteResult::Ok {
                total_files,
                indexed_files,
            } => Ok(crate::structured::IngestResult {
                nodes_created: indexed_files,
                edges_created: total_files.saturating_sub(indexed_files),
            }),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for IngestStructured: {other:?}"
            )),
        }
    }
    fn ingest_structured_directory(
        &self,
        schema: &crate::structured::SchemaMeta,
        dir: &Path,
    ) -> Result<crate::structured::IngestResult> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::IngestStructured {
            schema_id: schema.schema_id.clone(),
            source: crate::daemon_protocol::IngestSource::Directory(dir.to_path_buf()),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(120),
        )? {
            crate::daemon_protocol::WriteResult::Ok {
                total_files,
                indexed_files,
            } => Ok(crate::structured::IngestResult {
                nodes_created: indexed_files,
                edges_created: total_files.saturating_sub(indexed_files),
            }),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for IngestStructured: {other:?}"
            )),
        }
    }
    fn upsert_dependencies(&self, result: &crate::manifest::ManifestResult) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::UpsertDependencies {
            result: result.clone(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for UpsertDependencies: {other:?}"
            )),
        }
    }
    fn store_clusters(
        &self,
        idx_to_id: &[String],
        community: &[usize],
        modularity: f64,
    ) -> Result<crate::cluster::ClusterStats> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::StoreClusters {
            idx_to_id: idx_to_id.to_vec(),
            community: community.to_vec(),
            modularity,
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::ClustersOk(stats) => Ok(stats),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for StoreClusters: {other:?}"
            )),
        }
    }
    fn store_config_bindings(&self, bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::StoreConfigBindings {
            bindings: bindings.to_vec(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for StoreConfigBindings: {other:?}"
            )),
        }
    }
    fn write_cross_service_edges(&self, candidates: &[CrossServiceEdgeCandidate]) -> Result<usize> {
        if candidates.is_empty() {
            return Ok(0);
        }
        let staging_dir = self.staging_dir();
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let edges_path = staging_dir.join(format!("{name}.edges.arrow"));
        crate::daemon_protocol::write_cross_service_edges_arrow(&edges_path, candidates)?;

        let request = crate::daemon_protocol::WriteRequest::WriteCrossServiceEdges {
            edges_path: edges_path.clone(),
        };
        match crate::daemon_protocol::submit_write_request_named(
            &staging_dir,
            &name,
            &request,
            std::time::Duration::from_secs(60),
        ) {
            Ok(crate::daemon_protocol::WriteResult::Ok { indexed_files, .. }) => Ok(indexed_files),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => {
                Err(anyhow::anyhow!(message))
            }
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected WriteResult for WriteCrossServiceEdges: {other:?}"
            )),
            Err(e) => {
                std::fs::remove_file(&edges_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
    }
}
