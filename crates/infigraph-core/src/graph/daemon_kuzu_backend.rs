use anyhow::Result;

use crate::graph::queries::GraphQuery;
use crate::graph::query_exec::QueryExec;
use std::collections::HashMap;
use std::path::Path;

use crate::learned::LearnedStore;
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;

use super::backend::{
    CallsServiceEdge, Concern, CrossServiceEdgeCandidate, GraphBackend, ResolvesToEdge,
    TaintFlowEdge,
};
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
        // No validation probe here any more.
        //
        // This used to `drop(KuzuBackend::open_read_only(&db_path)?)` so a
        // missing or unopenable graph failed eagerly rather than on a later
        // read. Since reads route through the daemon that is both pointless
        // and harmful: pointless because no read opens this file any more,
        // and harmful because a read-only open fails while the daemon holds
        // an uncheckpointed WAL ("Corrupted wal file ... held by a live
        // writer", #149) -- so the probe broke routed reads exactly when the
        // daemon was busy writing, which is when routing matters most.
        //
        // The eager check that replaces it is reachability, not openability:
        // `Infigraph::init`/`init_read_only` call
        // `lifecycle::ensure_daemon_for_routed_access`, which starts a daemon
        // or fails with an actionable message.
        Ok(Self {
            db_path,
            root: root.to_path_buf(),
        })
    }

    /// Reads no longer reopen the graph per call.
    ///
    /// `open_read` used to reopen the whole `Database` for every read, because
    /// an embedded read-only Kuzu `Database` serves the snapshot it loaded at
    /// open time and never observes another process's later commits -- so a
    /// held handle went permanently stale the moment the daemon wrote. That
    /// problem is gone rather than solved: the read service answers from the
    /// daemon's own live `Database`, so a read sees the daemon's writes by
    /// construction. The hatch arm below still reopens, and inherits the old
    /// staleness caveat.
    /// Run one read against the daemon's read service, or -- only when the
    /// escape hatch is set -- directly against the graph file.
    ///
    /// This is the single place that decides local-vs-remote, and every read
    /// method above goes through it. Because `GraphQuery` is generic over
    /// `QueryExec`, both arms run the *same* query bodies; there is no
    /// second implementation of any read to drift.
    ///
    /// Reads are daemon-mandatory. The hatch exists so a graph stays
    /// recoverable when the daemon itself is broken, and it is deliberately
    /// explicit rather than an automatic fallback: a silent fallback keeps
    /// both paths permanently live, which is how `Infigraph::init` and
    /// `GraphStore::open_read_only_or_degrade` drifted apart until one
    /// quarantined healthy graphs (5818aa1).
    fn with_reader<T>(
        &self,
        f: impl FnOnce(&GraphQuery<&dyn QueryExec>) -> Result<T>,
    ) -> Result<T> {
        if direct_reads_enabled() {
            let store = crate::graph::GraphStore::open_read_only(&self.db_path)?;
            let conn = store.connection()?;
            let local = crate::graph::query_exec::LocalExec::new(&conn);
            let exec: &dyn QueryExec = &local;
            f(&GraphQuery::new_with(exec))
        } else {
            let remote = crate::graph::remote_exec::RemoteExec::new(&self.root);
            let exec: &dyn QueryExec = &remote;
            f(&GraphQuery::new_with(exec))
        }
    }

    fn not_supported(method: &str, alternative: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "not supported via direct backend access under DaemonKuzu -- use {alternative} instead ({method})"
        )
    }

    fn staging_dir(&self) -> std::path::PathBuf {
        self.root.join(".infigraph").join("requests")
    }

    /// Budget for the two whole-index-sized writes (`upsert_files_bulk`,
    /// `resolve_calls`). A full first index of a large repo puts every file's
    /// extraction through one of these, and the daemon only writes its
    /// `.result` once the whole batch commits -- matching the 600s
    /// `Infigraph::index()` allows for the same work under the
    /// `INFIGRAPH_WATCH_INDEX_VIA_DAEMON` path.
    const BULK_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
}

// clear_all_data is deliberately left un-overridden: the trait's own
// default (a no-op, backend.rs's `clear_all_data`) is the correct
// behavior for DaemonKuzu too, matching KuzuBackend's own reliance on the
// same default -- this is a deliberate choice, not an oversight.
impl GraphBackend for DaemonKuzuBackend {
    // ── Tier 1: reads pass through to a freshly opened read-only
    //    connection (see `open_read` for why it is not held open) ──

    fn stats(&self) -> Result<GraphStats> {
        self.with_reader(|q| q.stats())
    }
    fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        self.with_reader(|q| q.get_file_hashes())
    }
    fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        self.with_reader(|q| q.get_all_symbols())
    }
    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> {
        self.with_reader(|q| q.symbols_in_file(file))
    }
    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>> {
        self.with_reader(|q| q.find_symbol_by_id(id))
    }
    fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> {
        self.with_reader(|q| q.symbols_in_range(file, start, end))
    }
    fn skeleton(&self, file: &str) -> Result<String> {
        self.with_reader(|q| q.skeleton(file))
    }
    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.with_reader(|q| q.callers_of(symbol_id))
    }
    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        self.with_reader(|q| q.callees_of(symbol_id))
    }
    fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> {
        self.with_reader(|q| q.branches_of(symbol_id))
    }
    fn transitive_impact(&self, id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        self.with_reader(|q| q.transitive_impact(id, max_depth))
    }
    fn find_all_references(&self, id: &str) -> Result<Vec<ReferenceRow>> {
        self.with_reader(|q| q.find_all_references(id))
    }
    fn cross_cutting_for(&self, id: &str) -> Result<Vec<(String, String)>> {
        self.with_reader(|q| q.cross_cutting_for(id))
    }
    fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> {
        self.with_reader(|q| q.get_api_surface())
    }
    fn get_file_deps(&self, file: &str) -> Result<FileDeps> {
        self.with_reader(|q| q.get_file_deps(file))
    }
    fn get_type_hierarchy(&self, id: &str, max_depth: u32) -> Result<TypeHierarchy> {
        self.with_reader(|q| q.get_type_hierarchy(id, max_depth))
    }
    fn get_test_coverage(&self) -> Result<TestCoverage> {
        self.with_reader(|q| q.get_test_coverage())
    }
    fn generate_test_context(
        &self,
        file_filter: Option<&str>,
        limit: usize,
        test_type: Option<&str>,
    ) -> Result<TestContext> {
        self.with_reader(|q| q.generate_test_context(file_filter, limit, test_type))
    }
    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> {
        if crate::graph::is_transaction_control(query) {
            // Same answer the local backend gives, and the same answer the
            // read service gives (it reaches `raw_query_on` too).
            return Ok(Vec::new());
        }
        self.with_reader(|q| q.raw_query(query))
    }
    fn get_symbols_for_search(&self) -> Result<Vec<Vec<String>>> {
        self.with_reader(|q| q.get_symbols_for_search())
    }
    fn symbol_metadata(&self, id: &str) -> Result<Option<SymbolMeta>> {
        self.with_reader(|q| q.symbol_metadata(id))
    }
    fn get_complexity_ranking(&self, file_filter: Option<&str>) -> Result<Vec<ComplexityRow>> {
        self.with_reader(|q| q.get_complexity_ranking(file_filter))
    }
    fn list_indexed_files(&self) -> Result<Vec<String>> {
        self.with_reader(|q| q.list_indexed_files())
    }
    fn find_uncalled_symbols(&self) -> Result<Vec<DeadCodeRow>> {
        self.with_reader(|q| q.find_uncalled_symbols())
    }
    fn get_architecture_stats(&self) -> Result<ArchitectureStats> {
        self.with_reader(|q| q.get_architecture_stats())
    }
    fn symbols_with_docstring(
        &self,
        kind_filter: Option<&[&str]>,
    ) -> Result<Vec<SymbolWithDocstring>> {
        self.with_reader(|q| q.symbols_with_docstring(kind_filter))
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
        extractions: &[FileExtraction],
        existing_hashes_empty: bool,
    ) -> Result<()> {
        if extractions.is_empty() {
            return Ok(());
        }
        let staging_dir = self.staging_dir();
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let extractions_path = staging_dir.join(format!("{name}.extractions.json"));
        crate::daemon_protocol::write_extractions_json(&extractions_path, extractions)?;

        let request = crate::daemon_protocol::WriteRequest::UpsertFilesBulk {
            extractions_path: extractions_path.clone(),
            existing_hashes_empty,
        };
        match crate::daemon_protocol::submit_write_request_named(
            &staging_dir,
            &name,
            &request,
            Self::BULK_WRITE_TIMEOUT,
        ) {
            Ok(crate::daemon_protocol::WriteResult::Ok { .. }) => Ok(()),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => {
                Err(anyhow::anyhow!(message))
            }
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected WriteResult for UpsertFilesBulk: {other:?}"
            )),
            Err(e) => {
                std::fs::remove_file(&extractions_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
    }
    /// Sends a one-element `RemoveFiles` batch. The trait's per-file
    /// signature is what forces one round-trip per file here; the request
    /// itself is already batch-shaped, so a future bulk caller needs no
    /// protocol change.
    fn remove_file(&self, file: &str) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::RemoveFiles {
            files: vec![file.to_string()],
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(60),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for RemoveFiles: {other:?}"
            )),
        }
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
        extractions: &[FileExtraction],
        learned: Option<&LearnedStore>,
    ) -> Result<ResolveStats> {
        if extractions.is_empty() {
            return Ok(ResolveStats::default());
        }
        let staging_dir = self.staging_dir();
        std::fs::create_dir_all(&staging_dir)?;
        let name = crate::daemon_protocol::generate_request_name();
        let extractions_path = staging_dir.join(format!("{name}.extractions.json"));
        crate::daemon_protocol::write_extractions_json(&extractions_path, extractions)?;

        let request = crate::daemon_protocol::WriteRequest::ResolveCalls {
            extractions_path: extractions_path.clone(),
            // Only the caller's intent travels; the daemon loads the store
            // itself. A caller passing `None` still gets no learned patterns
            // applied, so this is not a silent behavior change.
            use_learned: learned.is_some(),
        };
        match crate::daemon_protocol::submit_write_request_named(
            &staging_dir,
            &name,
            &request,
            Self::BULK_WRITE_TIMEOUT,
        ) {
            Ok(crate::daemon_protocol::WriteResult::ResolveOk(stats)) => Ok(stats),
            Ok(crate::daemon_protocol::WriteResult::Err { message }) => {
                Err(anyhow::anyhow!(message))
            }
            Ok(other) => Err(anyhow::anyhow!(
                "unexpected WriteResult for ResolveCalls: {other:?}"
            )),
            Err(e) => {
                std::fs::remove_file(&extractions_path).ok(); // clean up on timeout -- the daemon never consumed it
                Err(e)
            }
        }
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
        project_root: Option<&Path>,
    ) -> Result<crate::scip::ImportStats> {
        self.import_scip_index_enriched_at(index_path, project_root, None)
    }
    fn import_scip_index_enriched_at(
        &self,
        index_path: &Path,
        _project_root: Option<&Path>,
        enriched_ast_generation: Option<i64>,
    ) -> Result<crate::scip::ImportStats> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::ScipImport {
            scip_path: index_path.to_path_buf(),
            enriched_ast_generation,
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
    fn replace_taint_flows(&self, flows: &[TaintFlowEdge]) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::ReplaceTaintFlows {
            flows: flows.to_vec(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for ReplaceTaintFlows: {other:?}"
            )),
        }
    }

    fn replace_concerns(&self, concerns: &[Concern]) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::ReplaceConcerns {
            concerns: concerns.to_vec(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for ReplaceConcerns: {other:?}"
            )),
        }
    }
    fn replace_resolves_to(&self, edges: &[ResolvesToEdge]) -> Result<()> {
        let staging_dir = self.staging_dir();
        let request = crate::daemon_protocol::WriteRequest::ReplaceResolvesTo {
            edges: edges.to_vec(),
        };
        match crate::daemon_protocol::submit_write_request(
            &staging_dir,
            &request,
            std::time::Duration::from_secs(30),
        )? {
            crate::daemon_protocol::WriteResult::Ok { .. } => Ok(()),
            crate::daemon_protocol::WriteResult::Err { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected WriteResult for ReplaceResolvesTo: {other:?}"
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

/// Whether reads bypass the daemon and open the graph file directly.
///
/// Set `INFIGRAPH_DIRECT_READS=1` to recover a graph when the daemon itself
/// is broken. Any value but empty or `0` enables it.
pub fn direct_reads_enabled() -> bool {
    std::env::var_os("INFIGRAPH_DIRECT_READS").is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the env mutation below against other env-mutating tests,
    /// following the pattern in `settings.rs` and `multi/mod.rs`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Reads are daemon-mandatory. The escape hatch exists so a graph is
    /// recoverable when the daemon itself is broken, and is deliberately
    /// explicit: a silent fallback would keep both paths permanently live,
    /// which is how `Infigraph::init` and
    /// `GraphStore::open_read_only_or_degrade` drifted apart until one
    /// quarantined healthy graphs (5818aa1).
    #[test]
    fn the_escape_hatch_restores_a_direct_read_when_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let graph = root.join(".infigraph").join("graph");
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        drop(crate::graph::GraphStore::open(&graph).unwrap());

        // No daemon is running, so without the hatch this must fail.
        let backend = DaemonKuzuBackend::open(root).unwrap();
        assert!(
            backend.stats().is_err(),
            "with no daemon and no escape hatch, a read must fail rather than silently \
             opening the file directly"
        );

        std::env::set_var("INFIGRAPH_DIRECT_READS", "1");
        let got = backend.stats();
        std::env::remove_var("INFIGRAPH_DIRECT_READS");
        assert!(
            got.is_ok(),
            "the escape hatch must restore a direct read: {got:?}"
        );
    }

    /// Regression test: `open` used to eagerly probe with
    /// `KuzuBackend::open_read_only` unconditionally, which fails on any
    /// nonexistent graph (read-only mode can never create a database) --
    /// breaking indexing of a brand-new project under
    /// `INFIGRAPH_BACKEND=daemon`. The probe must be skipped entirely when
    /// there's nothing on disk yet.
    #[test]
    fn open_succeeds_on_a_project_with_no_graph_yet() {
        let dir = tempfile::tempdir().unwrap();

        let backend = DaemonKuzuBackend::open(dir.path()).unwrap();

        assert_eq!(backend.db_path, dir.path().join(".infigraph").join("graph"));
        assert!(
            !backend.db_path.exists(),
            "open must not create the graph itself -- that's the daemon's write path's job"
        );
    }

    #[test]
    fn open_still_validates_an_existing_graph() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join(".infigraph").join("graph");
        drop(super::super::store::GraphStore::open(&db_path).unwrap());

        DaemonKuzuBackend::open(dir.path()).unwrap();
    }
}
