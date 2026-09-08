//! Read queries for the document store, generic over how they execute.
//!
//! The mirror of `infigraph-core`'s `GraphQuery`: every document read query
//! and its row-parsing lives here once and runs on a `QueryExec`, which is
//! either a local `docs.kuzu` connection or the daemon's read service. These
//! bodies used to sit on `DocStore` and open a connection themselves, which
//! meant the daemon-routed path would have needed a second copy of each.
//!
//! The move was safe because every one already consumed its rows stringly
//! (`row[n].to_string()`), which is exactly what `QueryExec` returns.

use std::collections::HashMap;

use anyhow::Result;
use infigraph_core::escape_str;
use infigraph_core::graph::query_exec::QueryExec;

use crate::store::{
    parse_string_list, ChunkDetail, DocStoreStats, ImpactResult, PipelineCoreRecord,
};

pub struct DocQuery<E: QueryExec> {
    exec: E,
}

impl<'a, 'db> DocQuery<infigraph_core::graph::query_exec::LocalExec<'a, 'db>> {
    /// Build over a local `docs.kuzu` connection.
    pub fn new(conn: &'a kuzu::Connection<'db>) -> Self {
        Self {
            exec: infigraph_core::graph::query_exec::LocalExec::new(conn),
        }
    }
}

impl<E: QueryExec> DocQuery<E> {
    /// Build over any executor -- notably the daemon's read service.
    pub fn new_with(exec: E) -> Self {
        Self { exec }
    }

    /// Execute arbitrary read Cypher and return stringly rows. The
    /// primitive every method here is built on.
    pub fn raw_query(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        self.exec.query_rows(cypher)
    }

    /// `count(...)` for a single-row, single-column count query. Replaces
    /// `store::count_query`, which needed a connection.
    fn count(&self, cypher: &str) -> usize {
        self.exec
            .query_rows(cypher)
            .ok()
            .and_then(|rows| rows.into_iter().next())
            .and_then(|row| row.into_iter().next())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    pub fn get_doc_hashes(&self) -> Result<HashMap<String, String>> {
        let result = self
            .exec
            .query_rows("MATCH (d:Document) RETURN d.file, d.content_hash")
            .map_err(|e| anyhow::anyhow!("query doc hashes: {e}"))?;
        let mut hashes = HashMap::new();
        for row in result {
            if row.len() >= 2 {
                hashes.insert(row[0].to_string(), row[1].to_string());
            }
        }
        Ok(hashes)
    }

    pub fn get_docs_by_source(&self, source_id: &str) -> Result<Vec<String>> {
        let result = self
            .exec
            .query_rows(&format!(
                "MATCH (d:Document)-[:FROM_SOURCE]->(s:Source) WHERE s.id = '{}' RETURN d.id",
                escape_str(source_id)
            ))
            .map_err(|e| anyhow::anyhow!("query docs by source: {e}"))?;
        let mut ids = Vec::new();
        for row in result {
            if !row.is_empty() {
                ids.insert(ids.len(), row[0].to_string());
            }
        }
        Ok(ids)
    }

    pub fn stats(&self) -> Result<DocStoreStats> {
        let doc_count = self.count("MATCH (d:Document) RETURN count(d)");
        let chunk_count = self.count("MATCH (c:Chunk) RETURN count(c)");
        Ok(DocStoreStats {
            document_count: doc_count,
            chunk_count,
        })
    }

    // ── PipelineCore methods ──────────────────────────────────────────────

    /// Create a per-plugin node table from schema definition.
    pub fn get_all_pipeline_cores(
        &self,
        plugin_id: Option<&str>,
    ) -> Result<Vec<PipelineCoreRecord>> {
        let query = match plugin_id {
            Some(pid) => format!(
                "MATCH (p:PipelineCore) WHERE p.plugin_id = '{}' RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs",
                escape_str(pid)
            ),
            None => "MATCH (p:PipelineCore) RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs".to_string(),
        };
        let result = self
            .exec
            .query_rows(&query)
            .map_err(|e| anyhow::anyhow!("query pipeline cores: {e}"))?;
        let mut records = Vec::new();
        for row in result {
            if row.len() >= 6 {
                records.push(PipelineCoreRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    plugin_id: row[3].to_string(),
                    inputs: parse_string_list(&row[4].to_string()),
                    outputs: parse_string_list(&row[5].to_string()),
                });
            }
        }
        Ok(records)
    }

    /// Get a PipelineCore record by id.
    pub fn get_pipeline_core(&self, pipeline_id: &str) -> Result<Option<PipelineCoreRecord>> {
        let result = self.exec.query_rows(&format!(
                "MATCH (p:PipelineCore) WHERE p.id = '{}' RETURN p.id, p.name, p.doc_id, p.plugin_id, p.inputs, p.outputs",
                escape_str(pipeline_id)
            ))
            .map_err(|e| anyhow::anyhow!("query pipeline core: {e}"))?;
        if let Some(row) = result.into_iter().next() {
            if row.len() >= 6 {
                return Ok(Some(PipelineCoreRecord {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    doc_id: row[2].to_string(),
                    plugin_id: row[3].to_string(),
                    inputs: parse_string_list(&row[4].to_string()),
                    outputs: parse_string_list(&row[5].to_string()),
                }));
            }
        }
        Ok(None)
    }

    /// Impact analysis using PipelineCore inputs/outputs.
    pub fn impact_analysis(&self, table_name: &str, max_depth: u32) -> Result<Vec<ImpactResult>> {
        let esc = escape_str(table_name);
        let mut results = Vec::new();

        // Direct impact: pipelines that consume this table
        let direct = self
            .exec
            .query_rows(&format!(
                "MATCH (p:PipelineCore) WHERE list_contains(p.inputs, '{}') RETURN p.id, p.name",
                esc
            ))
            .map_err(|e| anyhow::anyhow!("impact_analysis direct: {e}"))?;
        let mut affected_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for row in direct {
            if row.len() >= 2 {
                let id = row[0].to_string();
                let name = row[1].to_string();
                affected_ids.insert(id.clone());
                results.push(ImpactResult {
                    pipeline_id: id,
                    pipeline_name: name,
                    impact_type: "direct".to_string(),
                    depth: 1,
                    path: table_name.to_string(),
                });
            }
        }

        // Transitive impact via DEPENDS_ON edges
        if max_depth > 1 && !affected_ids.is_empty() {
            for depth in 2..=max_depth {
                let current_ids: Vec<String> = affected_ids.iter().cloned().collect();
                let mut new_ids = Vec::new();

                for src_id in &current_ids {
                    let trans = self.exec.query_rows(&format!(
                            "MATCH (a:PipelineCore)-[:DEPENDS_ON]->(b:PipelineCore) WHERE b.id = '{}' RETURN a.id, a.name",
                            escape_str(src_id)
                        ))
                        .map_err(|e| anyhow::anyhow!("impact_analysis transitive: {e}"))?;

                    for row in trans {
                        if row.len() >= 2 {
                            let id = row[0].to_string();
                            if !affected_ids.contains(&id) {
                                results.push(ImpactResult {
                                    pipeline_id: id.clone(),
                                    pipeline_name: row[1].to_string(),
                                    impact_type: "transitive".to_string(),
                                    depth,
                                    path: format!("{} → ... (depth {})", table_name, depth),
                                });
                                new_ids.push(id);
                            }
                        }
                    }
                }

                if new_ids.is_empty() {
                    break;
                }
                affected_ids.extend(new_ids);
            }
        }

        Ok(results)
    }

    /// Get all DEPENDS_ON edges as (from_name, to_name, dep_type) tuples.
    pub fn get_pipeline_deps(&self) -> Result<Vec<(String, String, String)>> {
        let result = self
            .exec
            .query_rows(
                "MATCH (c:PipelineCore)-[r:DEPENDS_ON]->(p:PipelineCore) \
                 RETURN c.name, p.name, r.dep_type",
            )
            .map_err(|e| anyhow::anyhow!("query pipeline deps: {e}"))?;
        let mut deps = Vec::new();
        for row in result {
            if row.len() >= 3 {
                deps.push((row[0].to_string(), row[1].to_string(), row[2].to_string()));
            }
        }
        Ok(deps)
    }

    /// Query a plugin-specific table by field value.
    pub fn query_plugin_table(
        &self,
        plugin_id: &str,
        field: &str,
        value: &str,
    ) -> Result<Vec<serde_json::Value>> {
        let table = format!("Pipeline_{}", plugin_id);
        let esc_val = escape_str(value);
        let result = self
            .exec
            .query_rows(&format!(
                "MATCH (p:{}) WHERE lower(p.{}) CONTAINS lower('{}') RETURN p.*",
                table, field, esc_val
            ))
            .map_err(|e| anyhow::anyhow!("query plugin table: {e}"))?;
        let mut rows = Vec::new();
        for row in result {
            let vals: Vec<serde_json::Value> = row
                .iter()
                .map(|v| serde_json::Value::String(v.to_string()))
                .collect();
            rows.push(serde_json::Value::Array(vals));
        }
        Ok(rows)
    }

    /// Pipeline count for stats (using PipelineCore).
    pub fn pipeline_core_count(&self) -> Result<usize> {
        Ok(self.count("MATCH (p:PipelineCore) RETURN count(p)"))
    }

    pub fn get_all_chunks(&self) -> Result<Vec<(String, String)>> {
        let result = self
            .exec
            .query_rows("MATCH (c:Chunk) RETURN c.id, c.text")
            .map_err(|e| anyhow::anyhow!("query chunks: {e}"))?;
        let mut chunks = Vec::new();
        for row in result {
            if row.len() >= 2 {
                chunks.push((row[0].to_string(), row[1].to_string()));
            }
        }
        Ok(chunks)
    }

    pub fn get_chunk_ids(&self) -> Result<std::collections::HashSet<String>> {
        let result = self
            .exec
            .query_rows("MATCH (c:Chunk) RETURN c.id")
            .map_err(|e| anyhow::anyhow!("query chunk ids: {e}"))?;
        let mut ids = std::collections::HashSet::new();
        for row in result {
            if !row.is_empty() {
                ids.insert(row[0].to_string());
            }
        }
        Ok(ids)
    }

    pub fn get_chunk_details(&self, chunk_ids: &[&str]) -> Result<Vec<ChunkDetail>> {
        let id_list: String = chunk_ids
            .iter()
            .map(|id| format!("'{}'", escape_str(id)))
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "MATCH (c:Chunk) WHERE c.id IN [{}] RETURN c.id, c.doc_file, c.idx, c.heading, c.text, c.start_offset, c.end_offset, c.page",
            id_list
        );
        let result = self
            .exec
            .query_rows(&query)
            .map_err(|e| anyhow::anyhow!("chunk details: {e}"))?;
        let mut details = Vec::new();
        for row in result {
            if row.len() >= 8 {
                let heading_str = row[3].to_string();
                let page_val: i64 = row[7].to_string().parse().unwrap_or(0);
                details.push(ChunkDetail {
                    id: row[0].to_string(),
                    doc_file: row[1].to_string(),
                    index: row[2].to_string().parse().unwrap_or(0),
                    heading: if heading_str.is_empty() {
                        None
                    } else {
                        Some(heading_str)
                    },
                    text: row[4].to_string(),
                    start_offset: row[5].to_string().parse().unwrap_or(0),
                    end_offset: row[6].to_string().parse().unwrap_or(0),
                    page: if page_val > 0 {
                        Some(page_val as usize)
                    } else {
                        None
                    },
                });
            }
        }
        Ok(details)
    }
}
