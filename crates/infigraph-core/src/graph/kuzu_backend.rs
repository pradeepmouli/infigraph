use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::learned::LearnedStore;
use crate::model::FileExtraction;
use crate::resolve::ResolveStats;

use super::backend::{CallsServiceEdge, CrossServiceEdgeCandidate, GraphBackend};
use super::queries::GraphQuery;
use super::store::GraphStore;
use super::{
    ApiSymbol, ArchitectureStats, BranchInfo, ComplexityRow, DeadCodeRow, FileDeps, FileHotspot,
    GraphStats, HubFunction, ImpactRow, KindCount, LanguageCount, ReferenceRow, SymbolDetail,
    SymbolMeta, SymbolRow, SymbolWithDocstring, TestContext, TestCoverage, TypeHierarchy,
};

/// Kùzu-backed graph storage (embedded, local mode).
///
/// Wraps existing `GraphStore` + `GraphQuery`. All write methods acquire
/// the write lock internally. Single-writer — concurrent `upsert_files_bulk`
/// calls will serialize on the lock.
pub struct KuzuBackend {
    store: GraphStore,
}

impl KuzuBackend {
    pub fn open(path: &Path) -> Result<Self> {
        let store = GraphStore::open(path)?;
        Ok(Self { store })
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let store = GraphStore::open_read_only(path)?;
        Ok(Self { store })
    }

    pub fn open_read_only_or_degrade(
        path: &Path,
    ) -> Result<(Self, Option<super::store::DegradeReason>)> {
        let (store, reason) = GraphStore::open_read_only_or_degrade(path)?;
        Ok((Self { store }, reason))
    }

    /// Wrap an already-opened GraphStore (avoids double-open).
    pub fn from_store(store: GraphStore) -> Self {
        Self { store }
    }

    /// Access underlying GraphStore (escape hatch for callers that
    /// still need raw Kùzu access during migration).
    pub fn inner(&self) -> &GraphStore {
        &self.store
    }
}

fn escape(s: &str) -> String {
    s.replace('\'', "\\'")
}

impl GraphBackend for KuzuBackend {
    // ── Lifecycle / metadata ─────────────────────────────────────────

    fn stats(&self) -> Result<GraphStats> {
        self.store.stats()
    }

    fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        self.store.get_file_hashes()
    }

    fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        self.store.get_all_symbols()
    }

    // ── Read: symbol queries ─────────────────────────────────────────

    fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.symbols_in_file(file)
    }

    fn find_symbol_by_id(&self, id: &str) -> Result<Option<SymbolDetail>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.find_symbol_by_id(id)
    }

    fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.symbols_in_range(file, start, end)
    }

    fn skeleton(&self, file: &str) -> Result<String> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.skeleton(file)
    }

    // ── Read: graph traversal ────────────────────────────────────────

    fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.callers_of(symbol_id)
    }

    fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.callees_of(symbol_id)
    }

    fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.branches_of(symbol_id)
    }

    fn transitive_impact(&self, id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.transitive_impact(id, max_depth)
    }

    fn find_all_references(&self, id: &str) -> Result<Vec<ReferenceRow>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.find_all_references(id)
    }

    fn cross_cutting_for(&self, id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.cross_cutting_for(id)
    }

    // ── Read: aggregate queries ──────────────────────────────────────

    fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.get_api_surface()
    }

    fn get_file_deps(&self, file: &str) -> Result<FileDeps> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.get_file_deps(file)
    }

    fn get_type_hierarchy(&self, id: &str, max_depth: u32) -> Result<TypeHierarchy> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.get_type_hierarchy(id, max_depth)
    }

    fn get_test_coverage(&self) -> Result<TestCoverage> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.get_test_coverage()
    }

    fn generate_test_context(
        &self,
        file_filter: Option<&str>,
        limit: usize,
        test_type: Option<&str>,
    ) -> Result<TestContext> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.generate_test_context(file_filter, limit, test_type)
    }

    // ── Read: raw query ──────────────────────────────────────────────

    fn raw_query(&self, query: &str) -> Result<Vec<Vec<String>>> {
        // Each call opens a fresh Connection (see GraphStore::connection), so
        // BEGIN TRANSACTION/COMMIT/ROLLBACK issued through this method can
        // never span multiple statements -- the transaction dies with the
        // connection that opened it, and a later call's COMMIT then fails
        // with "No active transaction." Kuzu auto-commits each statement
        // individually outside an explicit transaction, so no-op these
        // exactly like Neo4jBackend::raw_query already does, rather than
        // let every multi-statement "transactional" write silently break.
        let trimmed = query.trim_end_matches(';').trim();
        if trimmed.eq_ignore_ascii_case("BEGIN TRANSACTION")
            || trimmed.eq_ignore_ascii_case("BEGIN")
            || trimmed.eq_ignore_ascii_case("COMMIT")
            || trimmed.eq_ignore_ascii_case("ROLLBACK")
        {
            return Ok(Vec::new());
        }
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        q.raw_query(query)
    }

    // ── Phase-2: backend-agnostic query methods ──────────────────────

    fn symbol_metadata(&self, id: &str) -> Result<Option<SymbolMeta>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        let eid = escape(id);
        let meta_rows = q.raw_query(&format!(
            "MATCH (s:Symbol) WHERE s.id = '{}' RETURN s.docstring, s.complexity",
            eid
        ))?;
        if meta_rows.is_empty() {
            return Ok(None);
        }
        let row = &meta_rows[0];
        let docstring = row.first().cloned().unwrap_or_default();
        let complexity: u32 = row.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);

        let parent_rows = q.raw_query(&format!(
            "MATCH (parent)-[:CONTAINS]->(s:Symbol) WHERE s.id = '{}' RETURN parent.id, parent.name",
            eid
        ))?;
        let (parent_id, parent_name) = if let Some(pr) = parent_rows.first() {
            (pr.first().cloned(), pr.get(1).cloned())
        } else {
            (None, None)
        };

        Ok(Some(SymbolMeta {
            docstring,
            complexity,
            parent_id,
            parent_name,
        }))
    }

    fn get_complexity_ranking(&self, file_filter: Option<&str>) -> Result<Vec<ComplexityRow>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        let cypher = if let Some(f) = file_filter {
            format!(
                "MATCH (s:Symbol) WHERE (s.kind = 'Function' OR s.kind = 'Method' OR s.kind = 'Test') \
                 AND s.file CONTAINS '{}' RETURN s.name, s.file, s.start_line, s.complexity \
                 ORDER BY s.complexity DESC",
                escape(f)
            )
        } else {
            "MATCH (s:Symbol) WHERE (s.kind = 'Function' OR s.kind = 'Method' OR s.kind = 'Test') \
             RETURN s.name, s.file, s.start_line, s.complexity ORDER BY s.complexity DESC"
                .to_string()
        };
        let rows = q.raw_query(&cypher)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(ComplexityRow {
                    name: r.first()?.clone(),
                    file: r.get(1)?.clone(),
                    start_line: r.get(2)?.parse().unwrap_or(0),
                    complexity: r.get(3)?.parse().unwrap_or(0),
                })
            })
            .collect())
    }

    fn list_indexed_files(&self) -> Result<Vec<String>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        let rows = q.raw_query("MATCH (s:Symbol) RETURN DISTINCT s.file ORDER BY s.file")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .collect())
    }

    fn find_uncalled_symbols(&self) -> Result<Vec<DeadCodeRow>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        let rows = q.raw_query(
            "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] \
             AND NOT EXISTS { MATCH ()-[:CALLS]->(s) } \
             RETURN s.id, s.name, s.kind, s.file ORDER BY s.file, s.name",
        )?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(DeadCodeRow {
                    id: r.first()?.clone(),
                    name: r.get(1)?.clone(),
                    kind: r.get(2)?.clone(),
                    file: r.get(3)?.clone(),
                })
            })
            .collect())
    }

    fn get_architecture_stats(&self) -> Result<ArchitectureStats> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);

        let lang_rows =
            q.raw_query("MATCH (m:Module) RETURN m.language, count(m) ORDER BY count(m) DESC")?;
        let languages: Vec<LanguageCount> = lang_rows
            .into_iter()
            .filter_map(|r| {
                Some(LanguageCount {
                    language: r.first()?.clone(),
                    count: r.get(1)?.parse().unwrap_or(0),
                })
            })
            .collect();

        let kind_rows =
            q.raw_query("MATCH (s:Symbol) RETURN s.kind, count(s) ORDER BY count(s) DESC")?;
        let kind_counts: Vec<KindCount> = kind_rows
            .into_iter()
            .filter_map(|r| {
                Some(KindCount {
                    kind: r.first()?.clone(),
                    count: r.get(1)?.parse().unwrap_or(0),
                })
            })
            .collect();

        let hotspot_rows = q.raw_query(
            "MATCH (s:Symbol) RETURN s.file, count(s) AS cnt ORDER BY cnt DESC LIMIT 10",
        )?;
        let hotspot_files: Vec<FileHotspot> = hotspot_rows
            .into_iter()
            .filter_map(|r| {
                Some(FileHotspot {
                    file: r.first()?.clone(),
                    count: r.get(1)?.parse().unwrap_or(0),
                })
            })
            .collect();

        let hub_rows = q.raw_query(
            "MATCH ()-[r:CALLS]->(s:Symbol) RETURN s.name, s.file, count(r) AS calls \
             ORDER BY calls DESC LIMIT 10",
        )?;
        let hub_functions: Vec<HubFunction> = hub_rows
            .into_iter()
            .filter_map(|r| {
                Some(HubFunction {
                    name: r.first()?.clone(),
                    file: r.get(1)?.clone(),
                    calls: r.get(2)?.parse().unwrap_or(0),
                })
            })
            .collect();

        let entry_rows = q.raw_query(
            "MATCH (s:Symbol)-[:CALLS]->() WHERE s.kind IN ['Function', 'Method'] \
             AND NOT EXISTS { MATCH ()-[:CALLS]->(s) } \
             RETURN DISTINCT s.id, s.name, s.kind, s.file ORDER BY s.file, s.name LIMIT 20",
        )?;
        let entry_points: Vec<DeadCodeRow> = entry_rows
            .into_iter()
            .filter_map(|r| {
                Some(DeadCodeRow {
                    id: r.first()?.clone(),
                    name: r.get(1)?.clone(),
                    kind: r.get(2)?.clone(),
                    file: r.get(3)?.clone(),
                })
            })
            .collect();

        Ok(ArchitectureStats {
            languages,
            kind_counts,
            hotspot_files,
            hub_functions,
            entry_points,
        })
    }

    fn symbols_with_docstring(
        &self,
        kind_filter: Option<&[&str]>,
    ) -> Result<Vec<SymbolWithDocstring>> {
        let conn = self.store.connection()?;
        let q = GraphQuery::new(&conn);
        let cypher = if let Some(kinds) = kind_filter {
            let cond: Vec<String> = kinds
                .iter()
                .map(|k| format!("s.kind = '{}'", escape(k)))
                .collect();
            format!(
                "MATCH (s:Symbol) WHERE ({}) RETURN s.id, s.name, s.kind, s.file, s.docstring",
                cond.join(" OR ")
            )
        } else {
            "MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.docstring".to_string()
        };
        let rows = q.raw_query(&cypher)?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(SymbolWithDocstring {
                    id: r.first()?.clone(),
                    name: r.get(1)?.clone(),
                    kind: r.get(2)?.clone(),
                    file: r.get(3)?.clone(),
                    docstring: r.get(4).cloned().unwrap_or_default(),
                })
            })
            .collect())
    }

    fn upsert_similar_edge(&self, id_a: &str, id_b: &str, score: f32) -> Result<()> {
        let conn = self.store.connection()?;
        conn.query(&format!(
            "MATCH (a:Symbol), (b:Symbol) WHERE a.id = '{}' AND b.id = '{}' \
             MERGE (a)-[r:SIMILAR_TO]->(b) SET r.score = {}",
            escape(id_a),
            escape(id_b),
            score
        ))
        .map_err(|e| anyhow::anyhow!("upsert_similar_edge failed: {}", e))?;
        Ok(())
    }

    // ── Write ────────────────────────────────────────────────────────

    fn upsert_file(&self, extraction: &FileExtraction) -> Result<()> {
        self.store.upsert_file(extraction)
    }

    fn upsert_files_bulk(
        &self,
        extractions: &[FileExtraction],
        existing_hashes_empty: bool,
    ) -> Result<()> {
        if extractions.is_empty() {
            return Ok(());
        }

        let write_lock = self.store.write_lock()?;

        // Preflight disk headroom before any COPY/UNWIND write -- covers both
        // full reindex and incremental/watch batches, which both funnel
        // through this function (see store_util::check_disk_headroom).
        if let Some(dir) = self.store.db_dir() {
            let projected = crate::graph::store_util::estimate_extractions_write_bytes(extractions);
            if let Err(shortfall) = crate::graph::store_util::check_disk_headroom(dir, projected) {
                anyhow::bail!("refusing to index -- {shortfall}");
            }
            // R3.1.4d/#100: circuit breaker against the runaway-graph-growth
            // pattern, same call site as the disk-headroom preflight above.
            if let Err(msg) =
                crate::graph::store_util::check_graph_growth_ratio(dir, &dir.join("graph"))
            {
                anyhow::bail!("refusing to index -- {msg}");
            }
        }

        let use_csv = existing_hashes_empty || extractions.len() > 100;

        if use_csv {
            // Parquet bulk path: delete stale → COPY FROM → folders
            if !existing_hashes_empty {
                let conn = self.store.connection()?;
                conn.query("BEGIN TRANSACTION")
                    .context("failed to begin delete transaction")?;
                self.delete_files_data(&conn, extractions)?;
                conn.query("COMMIT")
                    .context("failed to commit delete transaction")?;
            }
            let conn = self.store.connection()?;
            self.store
                .upsert_all_parquet_conn(&conn, extractions, &write_lock)?;
        } else {
            // Per-file UNWIND path for small incremental updates
            let conn = self.store.connection()?;
            conn.query("BEGIN TRANSACTION")
                .context("failed to begin index transaction")?;
            self.delete_files_data(&conn, extractions)?;
            for extraction in extractions {
                self.store
                    .upsert_file_conn_no_delete(&conn, extraction, &write_lock)?;
            }
            conn.query("COMMIT")
                .context("failed to commit index transaction")?;
        }

        // Upsert folder hierarchy
        let file_paths: Vec<&str> = extractions.iter().map(|e| e.file.as_str()).collect();
        let conn = self.store.connection()?;
        self.store
            .upsert_folders_bulk_conn(&conn, &file_paths, &write_lock)?;

        // R3.3.3: bump once per completed write, so sidecars built from a
        // now-stale generation can be detected rather than served.
        self.store.bump_ast_generation_conn(&conn, &write_lock)?;

        if let Some(dir) = self.store.db_dir() {
            crate::graph::store_util::stamp_healthy_graph_size(dir, &dir.join("graph"));
        }

        Ok(())
    }

    fn remove_file(&self, file: &str) -> Result<()> {
        self.store.remove_file(file)
    }

    fn current_ast_generation(&self) -> Result<i64> {
        self.store.current_ast_generation()
    }

    fn current_scip_generation(&self) -> Result<i64> {
        self.store.current_scip_generation()
    }

    fn derive_tested_by_edges(&self, _changed_files: Option<&[&str]>) -> Result<usize> {
        self.store.derive_tested_by_edges()
    }

    fn write_calls_service_edges(&self, edges: &[CallsServiceEdge]) -> Result<()> {
        if edges.is_empty() {
            return Ok(());
        }
        let conn = self.store.connection()?;
        conn.query("BEGIN TRANSACTION")
            .map_err(|e| anyhow::anyhow!("failed to begin transaction: {e}"))?;
        for edge in edges {
            let src_esc = crate::escape_str(&edge.symbol_id);
            let tgt_esc = crate::escape_str(&edge.target_id);
            let method_esc = crate::escape_str(&edge.method);
            let path_esc = crate::escape_str(&edge.path);
            if let Err(e) = conn.query(&format!(
                "MATCH (s:Symbol), (t:Symbol) WHERE s.id = '{src_esc}' AND t.id = '{tgt_esc}' \
                 CREATE (s)-[:CALLS_SERVICE {{method: '{method_esc}', path: '{path_esc}', target_service: ''}}]->(t)"
            )) {
                // Roll back so we don't leave a half-applied batch or a
                // dangling open transaction on this connection, then
                // surface the real error instead of masking it.
                let _ = conn.query("ROLLBACK");
                return Err(anyhow::anyhow!("failed to create CALLS_SERVICE edge: {e}"));
            }
        }
        conn.query("COMMIT")
            .map_err(|e| anyhow::anyhow!("failed to commit CALLS_SERVICE edges: {e}"))?;
        Ok(())
    }

    fn write_cross_service_edges(&self, candidates: &[CrossServiceEdgeCandidate]) -> Result<usize> {
        let mut created = 0;
        for c in candidates {
            let target_id = escape(&c.target_id);
            let target_name = escape(&c.target_name);
            let docstring = escape(&c.docstring);
            let caller_sym = escape(&c.caller_symbol_id);
            let method = escape(&c.method);
            let path = escape(&c.path);
            let target_svc = escape(&c.target_service);
            let protocol = escape(&c.protocol);

            let create_target = format!(
                "MERGE (t:Symbol {{id: '{target_id}'}}) \
                 ON CREATE SET t.name = '{target_name}', t.kind = 'ExternalService', \
                 t.file = '(external)', t.start_line = 0, t.end_line = 0, \
                 t.signature_hash = '', t.language = 'external', t.visibility = 'public', \
                 t.parent = '', t.docstring = '{docstring}', t.complexity = 0"
            );
            self.raw_query(&create_target)?;

            let check_edge = format!(
                "MATCH (caller:Symbol {{id: '{caller_sym}'}})-[:CALLS_SERVICE]->(target:Symbol {{id: '{target_id}'}}) RETURN caller.id"
            );
            let existing = self.raw_query(&check_edge)?;
            if !existing.is_empty() {
                continue;
            }

            let create_edge = format!(
                "MATCH (caller:Symbol {{id: '{caller_sym}'}}), (target:Symbol {{id: '{target_id}'}}) \
                 CREATE (caller)-[:CALLS_SERVICE {{method: '{method}', path: '{path}', target_service: '{target_svc}', protocol: '{protocol}'}}]->(target)"
            );
            self.raw_query(&create_edge)?;
            created += 1;
        }
        Ok(created)
    }

    fn upsert_dependencies(&self, result: &crate::manifest::ManifestResult) -> Result<()> {
        for dep in &result.deps {
            let id = format!("{}::{}", dep.ecosystem, dep.name);
            let check = format!(
                "MATCH (d:Dependency) WHERE d.id = '{}' RETURN d.id",
                escape(&id)
            );
            let existing = self.raw_query(&check)?;
            if existing.is_empty() {
                let insert = format!(
                    "CREATE (d:Dependency {{id: '{}', name: '{}', version: '{}', ecosystem: '{}', is_dev: {}}})",
                    escape(&id), escape(&dep.name), escape(&dep.version), escape(&dep.ecosystem), dep.is_dev
                );
                self.raw_query(&insert)?;
            } else {
                let update = format!(
                    "MATCH (d:Dependency) WHERE d.id = '{}' SET d.version = '{}', d.is_dev = {}",
                    escape(&id),
                    escape(&dep.version),
                    dep.is_dev
                );
                self.raw_query(&update)?;
            }

            // Scope the DEPENDS_ON edge to THIS repo's modules. Without the repo guard,
            // `m.file CONTAINS 'pyproject.toml'` matches every repo's manifest module in a
            // shared graph, cross-linking one repo's deps onto all others.
            let manifest_base = escape(result.manifest_file.rsplit('/').next().unwrap_or(""));
            let rel = if let Some(repo) = self.repo_filter() {
                let r = escape(repo);
                format!(
                    "MATCH (m:Module), (d:Dependency) \
                     WHERE m.file STARTS WITH '{r}/' AND m.file CONTAINS '{manifest_base}' AND d.id = '{}' \
                     CREATE (m)-[:DEPENDS_ON {{is_dev: {}}}]->(d)",
                    escape(&id),
                    dep.is_dev
                )
            } else {
                format!(
                    "MATCH (m:Module), (d:Dependency) WHERE m.file CONTAINS '{manifest_base}' AND d.id = '{}' \
                     CREATE (m)-[:DEPENDS_ON {{is_dev: {}}}]->(d)",
                    escape(&id),
                    dep.is_dev
                )
            };
            self.raw_query(&rel)?;
        }
        Ok(())
    }

    fn store_clusters(
        &self,
        idx_to_id: &[String],
        community: &[usize],
        modularity: f64,
    ) -> Result<crate::cluster::ClusterStats> {
        self.raw_query("MATCH (s:Symbol)-[r:MEMBER_OF]->(c:Cluster) DELETE r")?;
        self.raw_query("MATCH (c:Cluster) DELETE c")?;

        let mut comm_members: HashMap<usize, Vec<usize>> = HashMap::new();
        for (node, &comm) in community.iter().enumerate() {
            comm_members.entry(comm).or_default().push(node);
        }

        let mut cluster_sizes = Vec::new();

        for (cluster_idx, members) in comm_members.values().enumerate() {
            let cluster_id = format!("cluster_{}", cluster_idx);
            let cluster_name = format!("Cluster {}", cluster_idx);

            let mut files: Vec<&str> = Vec::new();
            for &node in members {
                let sym_id = &idx_to_id[node];
                if let Some((file, _)) = sym_id.rsplit_once("::") {
                    if !files.contains(&file) {
                        files.push(file);
                    }
                }
            }
            files.truncate(5);
            let description = format!(
                "{} symbols across files: {}",
                members.len(),
                files.join(", ")
            );

            let create_cluster = format!(
                "CREATE (c:Cluster {{id: '{}', name: '{}', description: '{}'}})",
                escape(&cluster_id),
                escape(&cluster_name),
                escape(&description),
            );
            self.raw_query(&create_cluster)?;

            for &node in members {
                let sym_id = &idx_to_id[node];
                let create_edge = format!(
                    "MATCH (s:Symbol), (c:Cluster) WHERE s.id = '{}' AND c.id = '{}' CREATE (s)-[:MEMBER_OF]->(c)",
                    escape(sym_id),
                    escape(&cluster_id),
                );
                self.raw_query(&create_edge)?;
            }

            cluster_sizes.push(members.len());
        }

        Ok(crate::cluster::ClusterStats {
            num_clusters: cluster_sizes.len(),
            cluster_sizes,
            modularity,
        })
    }

    fn store_config_bindings(&self, bindings: &[crate::config::ConfigBindingWire]) -> Result<()> {
        self.raw_query("MATCH (c:ConfigBinding) DETACH DELETE c")?;

        for b in bindings {
            let id = format!("{}::{}::{}", b.symbol_id, b.kind, b.key);
            let id_esc = crate::escape_str(&id);
            let kind_esc = crate::escape_str(&b.kind);
            let key_esc = crate::escape_str(&b.key);
            let val_esc = crate::escape_str(&b.value);
            let profile_esc = crate::escape_str(&b.profile);
            let src_esc = crate::escape_str(&b.source_file);
            let sym_esc = crate::escape_str(&b.symbol_id);

            self.raw_query(&format!(
                "CREATE (c:ConfigBinding {{id: '{id_esc}', kind: '{kind_esc}', key: '{key_esc}', value: '{val_esc}', `profile`: '{profile_esc}', source_file: '{src_esc}'}})"
            ))?;
            self.raw_query(&format!(
                "MATCH (s:Symbol), (c:ConfigBinding) WHERE s.id = '{sym_esc}' AND c.id = '{id_esc}' CREATE (s)-[:HAS_CONFIG]->(c)"
            ))?;
        }

        Ok(())
    }

    // ── Resolve ──────────────────────────────────────────────────────

    fn resolve_calls(
        &self,
        extractions: &[FileExtraction],
        learned: Option<&LearnedStore>,
    ) -> Result<ResolveStats> {
        crate::resolve::resolve_calls_incremental(&self.store, extractions, learned)
    }

    fn re_resolve_for_files(
        &self,
        files: &[String],
        extractions: &[FileExtraction],
        learned: Option<&LearnedStore>,
    ) -> Result<ResolveStats> {
        crate::resolve::re_resolve_for_files(&self.store, files, extractions, learned)
    }

    fn import_scip_index(
        &self,
        index_path: &std::path::Path,
        project_root: Option<&std::path::Path>,
    ) -> Result<crate::scip::ImportStats> {
        crate::scip::import_scip_index(index_path, &self.store, project_root)
    }

    fn ingest_structured_data(
        &self,
        schema: &crate::structured::SchemaMeta,
        data: &[serde_json::Value],
    ) -> Result<crate::structured::IngestResult> {
        let _lock = self.store.write_lock()?;
        let conn = self.store.connection()?;
        crate::structured::ingest_data(&conn, schema, data)
    }

    fn ingest_structured_file(
        &self,
        schema: &crate::structured::SchemaMeta,
        path: &std::path::Path,
    ) -> Result<crate::structured::IngestResult> {
        let _lock = self.store.write_lock()?;
        let conn = self.store.connection()?;
        crate::structured::ingest_file(&conn, schema, path)
    }

    fn ingest_structured_directory(
        &self,
        schema: &crate::structured::SchemaMeta,
        dir: &std::path::Path,
    ) -> Result<crate::structured::IngestResult> {
        let _lock = self.store.write_lock()?;
        let conn = self.store.connection()?;
        crate::structured::ingest_directory(&conn, schema, dir)
    }
}

impl KuzuBackend {
    /// Delete all graph data for the given files. Caller manages the transaction.
    fn delete_files_data(
        &self,
        conn: &kuzu::Connection<'_>,
        extractions: &[FileExtraction],
    ) -> Result<()> {
        let file_list: Vec<String> = extractions
            .iter()
            .map(|e| format!("'{}'", escape(&e.file)))
            .collect();
        let files_in = file_list.join(", ");

        let _ = conn.query(&format!(
            "MATCH (f:File)-[:DEFINES]->(s:Symbol)-[:HAS_STATEMENT]->(st:Statement) WHERE f.id IN [{}] DETACH DELETE st",
            files_in
        ));
        let _ = conn.query(&format!(
            "MATCH (s:Symbol) WHERE s.file IN [{}] DETACH DELETE s",
            files_in
        ));
        let _ = conn.query(&format!(
            "MATCH (m:Module) WHERE m.file IN [{}] DETACH DELETE m",
            files_in
        ));
        let _ = conn.query(&format!(
            "MATCH (f:File) WHERE f.id IN [{}] DETACH DELETE f",
            files_in
        ));

        Ok(())
    }
}
