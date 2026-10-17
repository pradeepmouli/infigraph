use std::collections::HashMap;

use anyhow::Result;

use crate::chunk::Chunk;
use infigraph_core::daemon_protocol::DocAck;

use crate::code_links::{CodeLinkRow, LinkKind};
use crate::extract::ExtractedDoc;
use crate::store::{ChunkDetail, DocStoreStats, ImpactResult, PipelineCoreRecord};

/// Trait abstracting document graph storage.
///
/// `DocStore` (Kùzu) is the local implementation.
/// `Neo4jDocStore` is the remote implementation (behind `feature = "remote"`).
pub trait DocBackend {
    // ── Document CRUD ────────────────────────────────────────────────────

    fn get_doc_hashes(&self) -> Result<HashMap<String, String>>;

    fn upsert_docs(&self, docs: &[&ExtractedDoc], chunks: &[&Chunk]) -> Result<()>;

    fn delete_docs_by_ids(&self, doc_ids: &[&str]) -> Result<()>;

    fn ensure_document_node(&self, doc_id: &str) -> Result<()>;

    // ── Source management ────────────────────────────────────────────────

    fn upsert_source(
        &self,
        id: &str,
        source_type: &str,
        base_url: &str,
        space_key: &str,
    ) -> Result<()>;

    fn link_doc_to_source(&self, doc_id: &str, source_id: &str) -> Result<()>;

    fn get_docs_by_source(&self, source_id: &str) -> Result<Vec<String>>;

    // ── Links ────────────────────────────────────────────────────────────

    fn create_link(
        &self,
        from_doc_id: &str,
        to_doc_id: &str,
        url: &str,
        link_type: &str,
    ) -> Result<()>;

    fn delete_links_from(&self, doc_id: &str) -> Result<()>;

    // ── Chunks ───────────────────────────────────────────────────────────

    fn get_all_chunks(&self) -> Result<Vec<(String, String)>>;

    fn get_chunk_ids(&self) -> Result<std::collections::HashSet<String>>;

    fn get_chunk_details(&self, chunk_ids: &[&str]) -> Result<Vec<ChunkDetail>>;

    // ── Code links ───────────────────────────────────────────────────────

    /// Replaces a document's code links and records that it was scanned
    /// under the current `CODE_LINKS_VERSION` (written last).
    fn replace_code_links(&self, doc_file: &str, rows: &[CodeLinkRow]) -> Result<()>;

    /// The links of `kind` whose target is one of `targets`, in document
    /// order.
    fn code_links_for(&self, kind: LinkKind, targets: &[String]) -> Result<Vec<CodeLinkRow>>;

    /// Every link of `kind`.
    fn code_link_rows(&self, kind: LinkKind) -> Result<Vec<CodeLinkRow>>;

    /// Document id -> the `CODE_LINKS_VERSION` it was last scanned under.
    fn code_link_scans(&self) -> Result<HashMap<String, i64>>;

    // ── Acknowledgements ─────────────────────────────────────────────────
    //
    // Kept in the local document store only: a remote store holds none, and
    // reads as holding none rather than failing.

    /// The acknowledgements of `targets`, by target.
    fn doc_acks(&self, _targets: &[String]) -> Result<HashMap<String, DocAck>> {
        Ok(HashMap::new())
    }

    /// Every acknowledgement.
    fn all_doc_acks(&self) -> Result<Vec<DocAck>> {
        Ok(Vec::new())
    }

    /// Records `acks` and forgets those of `drop`; returns the rows written
    /// and removed.
    fn upsert_acks(&self, _acks: &[DocAck], _drop: &[String]) -> Result<(usize, usize)> {
        anyhow::bail!("acknowledgements are kept in the local document store")
    }

    // ── Stats ────────────────────────────────────────────────────────────

    fn stats(&self) -> Result<DocStoreStats>;

    // ── Pipeline CRUD ────────────────────────────────────────────────────

    fn ensure_plugin_table(&self, plugin_id: &str, columns: &[(String, String)]) -> Result<()>;

    /// Deletes the pipeline rows (`PipelineCore` and `Pipeline_<plugin_id>`)
    /// of the given documents. `delete_docs_by_ids` does this itself; the method
    /// is on the trait for the callers that replace a document's pipelines.
    fn delete_pipelines_for_docs(&self, doc_ids: &[&str]) -> Result<()>;

    fn upsert_pipeline_core(&self, record: &PipelineCoreRecord) -> Result<()>;

    fn upsert_plugin_properties(
        &self,
        pipeline_id: &str,
        plugin_id: &str,
        properties: &serde_json::Map<String, serde_json::Value>,
        schema: &[(String, String)],
    ) -> Result<()>;

    fn link_pipeline_core_to_doc(&self, pipeline_id: &str, doc_id: &str) -> Result<()>;

    fn link_pipeline_dependencies(&self) -> Result<usize>;

    fn get_all_pipeline_cores(&self, plugin_id: Option<&str>) -> Result<Vec<PipelineCoreRecord>>;

    fn get_pipeline_core(&self, pipeline_id: &str) -> Result<Option<PipelineCoreRecord>>;

    fn impact_analysis(&self, table_name: &str, max_depth: u32) -> Result<Vec<ImpactResult>>;

    fn get_pipeline_deps(&self) -> Result<Vec<(String, String, String)>>;

    fn query_plugin_table(
        &self,
        plugin_id: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<serde_json::Value>>;

    fn pipeline_core_count(&self) -> Result<usize>;
}
