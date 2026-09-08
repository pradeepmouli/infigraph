//! Document reads routed through the daemon.
//!
//! The docs counterpart of `DaemonKuzuBackend`, named for this crate's
//! `DocStore`/`Neo4jDocStore` convention rather than the graph side's, and
//! selected the same way:
//! `DocIndex::init` picks it when `INFIGRAPH_BACKEND=daemon`. The daemon
//! process itself is spawned with that variable removed
//! (`daemon::lifecycle`), so it keeps the local `DocStore` and never routes
//! back into its own read service -- which matters more here than on the
//! graph side, because `DocIndex` reads through a store it is *holding*
//! (`get_doc_hashes` during indexing), and a self-call would block on the
//! process-wide `DB_LOCK`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use infigraph_core::graph::query_exec::QueryExec;
use infigraph_core::graph::remote_exec::RemoteExec;

use crate::backend::DocBackend;
use crate::chunk::Chunk;
use crate::extract::ExtractedDoc;
use crate::query::DocQuery;
use crate::store::{ChunkDetail, DocStore, DocStoreStats, ImpactResult, PipelineCoreRecord};

pub struct DaemonDocStore {
    root: PathBuf,
}

impl DaemonDocStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Run one read against the daemon's read service, or -- only when
    /// `INFIGRAPH_DIRECT_READS` is set -- directly against `docs.kuzu`.
    ///
    /// The single place that decides local-vs-remote for documents, sharing
    /// the graph side's hatch so there is one switch, not two. Both arms
    /// run the same `DocQuery` bodies; there is no second implementation of
    /// any read to drift.
    fn with_reader<T>(&self, f: impl FnOnce(&DocQuery<&dyn QueryExec>) -> Result<T>) -> Result<T> {
        if infigraph_core::graph::daemon_kuzu_backend::direct_reads_enabled() {
            let store = DocStore::open(&self.root.join(".infigraph").join("docs.kuzu"))?;
            let conn = store.connection()?;
            let local = infigraph_core::graph::query_exec::LocalExec::new(&conn);
            let exec: &dyn QueryExec = &local;
            f(&DocQuery::new_with(exec))
        } else {
            let remote = RemoteExec::for_docs(&self.root);
            let exec: &dyn QueryExec = &remote;
            f(&DocQuery::new_with(exec))
        }
    }

    /// Documents have no daemon write protocol, unlike the code graph's
    /// file-drop `WriteRequest`. Under `INFIGRAPH_BACKEND=daemon` the daemon
    /// owns document writing through its own doc watcher, so a client-side
    /// write is refused explicitly rather than silently opening
    /// `docs.kuzu` beside the daemon's own handle.
    fn writes_not_routed(method: &str) -> anyhow::Error {
        anyhow::anyhow!(
            "document writes are not routed through the daemon ({method}); the daemon indexes \
             documents itself via its doc watcher. Run the command without \
             INFIGRAPH_BACKEND=daemon to write directly."
        )
    }
}

impl DocBackend for DaemonDocStore {
    // ── Reads: routed ────────────────────────────────────────────────

    fn get_doc_hashes(&self) -> Result<HashMap<String, String>> {
        self.with_reader(|q| q.get_doc_hashes())
    }

    fn get_docs_by_source(&self, source_id: &str) -> Result<Vec<String>> {
        self.with_reader(|q| q.get_docs_by_source(source_id))
    }

    fn get_all_chunks(&self) -> Result<Vec<(String, String)>> {
        self.with_reader(|q| q.get_all_chunks())
    }

    fn get_chunk_ids(&self) -> Result<HashSet<String>> {
        self.with_reader(|q| q.get_chunk_ids())
    }

    fn get_chunk_details(&self, chunk_ids: &[&str]) -> Result<Vec<ChunkDetail>> {
        self.with_reader(|q| q.get_chunk_details(chunk_ids))
    }

    fn stats(&self) -> Result<DocStoreStats> {
        self.with_reader(|q| q.stats())
    }

    fn get_all_pipeline_cores(&self, plugin_id: Option<&str>) -> Result<Vec<PipelineCoreRecord>> {
        self.with_reader(|q| q.get_all_pipeline_cores(plugin_id))
    }

    fn get_pipeline_core(&self, pipeline_id: &str) -> Result<Option<PipelineCoreRecord>> {
        self.with_reader(|q| q.get_pipeline_core(pipeline_id))
    }

    fn impact_analysis(&self, table_name: &str, max_depth: u32) -> Result<Vec<ImpactResult>> {
        self.with_reader(|q| q.impact_analysis(table_name, max_depth))
    }

    fn get_pipeline_deps(&self) -> Result<Vec<(String, String, String)>> {
        self.with_reader(|q| q.get_pipeline_deps())
    }

    fn query_plugin_table(
        &self,
        plugin_id: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<serde_json::Value>> {
        self.with_reader(|q| q.query_plugin_table(plugin_id, field, value))
    }

    fn pipeline_core_count(&self) -> Result<usize> {
        self.with_reader(|q| q.pipeline_core_count())
    }

    // ── Writes: not routed ───────────────────────────────────────────

    fn upsert_docs(&self, _docs: &[&ExtractedDoc], _chunks: &[&Chunk]) -> Result<()> {
        Err(Self::writes_not_routed("upsert_docs"))
    }

    fn delete_docs_by_ids(&self, _doc_ids: &[&str]) -> Result<()> {
        Err(Self::writes_not_routed("delete_docs_by_ids"))
    }

    fn ensure_document_node(&self, _doc_id: &str) -> Result<()> {
        Err(Self::writes_not_routed("ensure_document_node"))
    }

    fn upsert_source(
        &self,
        _id: &str,
        _source_type: &str,
        _base_url: &str,
        _space_key: &str,
    ) -> Result<()> {
        Err(Self::writes_not_routed("upsert_source"))
    }

    fn link_doc_to_source(&self, _doc_id: &str, _source_id: &str) -> Result<()> {
        Err(Self::writes_not_routed("link_doc_to_source"))
    }

    fn create_link(
        &self,
        _from_doc_id: &str,
        _to_doc_id: &str,
        _url: &str,
        _link_type: &str,
    ) -> Result<()> {
        Err(Self::writes_not_routed("create_link"))
    }

    fn delete_links_from(&self, _doc_id: &str) -> Result<()> {
        Err(Self::writes_not_routed("delete_links_from"))
    }

    fn ensure_plugin_table(&self, _plugin_id: &str, _columns: &[(String, String)]) -> Result<()> {
        Err(Self::writes_not_routed("ensure_plugin_table"))
    }

    fn upsert_pipeline_core(&self, _record: &PipelineCoreRecord) -> Result<()> {
        Err(Self::writes_not_routed("upsert_pipeline_core"))
    }

    fn upsert_plugin_properties(
        &self,
        _pipeline_id: &str,
        _plugin_id: &str,
        _properties: &serde_json::Map<String, serde_json::Value>,
        _schema: &[(String, String)],
    ) -> Result<()> {
        Err(Self::writes_not_routed("upsert_plugin_properties"))
    }

    fn link_pipeline_core_to_doc(&self, _pipeline_id: &str, _doc_id: &str) -> Result<()> {
        Err(Self::writes_not_routed("link_pipeline_core_to_doc"))
    }

    fn link_pipeline_dependencies(&self) -> Result<usize> {
        Err(Self::writes_not_routed("link_pipeline_dependencies"))
    }
}
