use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::DataType;
use kuzu::{Connection, Database, SystemConfig};

use super::parquet_loader;
use super::schema::CREATE_SCHEMA;
use crate::model::{FileExtraction, RelationKind};

/// Persistent graph store backed by Kuzu.
pub struct GraphStore {
    db: Database,
}

impl GraphStore {
    /// Open or create a Kuzu database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::new(path, SystemConfig::default())
            .map_err(|e| anyhow::anyhow!("failed to open kuzu db: {e}"))?;
        let store = Self { db };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<()> {
        let conn = self.connection()?;
        for ddl in CREATE_SCHEMA {
            conn.query(ddl)
                .map_err(|e| anyhow::anyhow!("schema error: {e}\n  DDL: {ddl}"))?;
        }
        Ok(())
    }

    pub fn connection(&self) -> Result<Connection<'_>> {
        Connection::new(&self.db)
            .map_err(|e| anyhow::anyhow!("failed to create connection: {e}"))
    }

    /// Insert a file extraction into the graph.
    /// Removes old data for the file first (incremental update).
    pub fn upsert_file(&self, extraction: &FileExtraction) -> Result<()> {
        let conn = self.connection()?;
        self.upsert_file_conn(&conn, extraction)
    }

    pub fn upsert_file_conn(&self, conn: &Connection<'_>, extraction: &FileExtraction) -> Result<()> {
        // Remove old symbols for this file
        let _ = conn.query(&format!("MATCH (s:Symbol) WHERE s.file = '{}' DETACH DELETE s", escape(&extraction.file)));
        let _ = conn.query(&format!("MATCH (m:Module) WHERE m.file = '{}' DETACH DELETE m", escape(&extraction.file)));
        let _ = conn.query(&format!("MATCH (f:File) WHERE f.id = '{}' DETACH DELETE f", escape(&extraction.file)));
        self.upsert_file_conn_no_delete(conn, extraction)
    }

    pub fn upsert_file_conn_no_delete(&self, conn: &Connection<'_>, extraction: &FileExtraction) -> Result<()> {
        // Insert module node
        let module_id = &extraction.file;
        let module_name = extraction
            .file
            .rsplit_once('/')
            .map(|(_, f)| f)
            .unwrap_or(&extraction.file);
        let insert_module = format!(
            "CREATE (m:Module {{id: '{}', name: '{}', file: '{}', language: '{}', content_hash: '{}'}})",
            escape(module_id),
            escape(module_name),
            escape(&extraction.file),
            escape(&extraction.language),
            escape(&extraction.content_hash),
        );
        conn.query(&insert_module)
            .context("failed to insert module")?;

        // Insert File node
        let file_name = extraction
            .file
            .rsplit_once('/')
            .map(|(_, f)| f)
            .unwrap_or(&extraction.file);
        let symbol_count = extraction.symbols.len() as i32;
        let insert_file = format!(
            "CREATE (f:File {{id: '{}', name: '{}', path: '{}', language: '{}', symbol_count: {}}})",
            escape(&extraction.file),
            escape(file_name),
            escape(&extraction.file),
            escape(&extraction.language),
            symbol_count,
        );
        conn.query(&insert_file)
            .context("failed to insert file node")?;

        // Folder hierarchy is handled in bulk by upsert_folders_bulk — skip per-file here

        // Batch insert symbols via UNWIND
        if !extraction.symbols.is_empty() {
            let sym_rows: Vec<String> = extraction.symbols.iter().map(|sym| {
                format!(
                    "{{id: '{}', name: '{}', kind: '{}', file: '{}', start_line: {}, end_line: {}, signature_hash: '{}', language: '{}', visibility: '{}', parent: '{}', docstring: '{}', complexity: {}}}",
                    escape(&sym.id),
                    escape(&sym.name),
                    sym.kind.as_str(),
                    escape(&extraction.file),
                    sym.span.start_line,
                    sym.span.end_line,
                    escape(&sym.signature_hash),
                    escape(&sym.language),
                    escape(sym.visibility.as_deref().unwrap_or("")),
                    escape(sym.parent.as_deref().unwrap_or("")),
                    escape(sym.docstring.as_deref().unwrap_or("")),
                    sym.complexity,
                )
            }).collect();
            let batch_insert = format!(
                "UNWIND [{}] AS s CREATE (:Symbol {{id: s.id, name: s.name, kind: s.kind, file: s.file, start_line: s.start_line, end_line: s.end_line, signature_hash: s.signature_hash, language: s.language, visibility: s.visibility, parent: s.parent, docstring: s.docstring, complexity: s.complexity}})",
                sym_rows.join(", ")
            );
            conn.query(&batch_insert).context("failed to batch insert symbols")?;

            // Batch CONTAINS edges: module -> symbols
            let sym_ids: Vec<String> = extraction.symbols.iter().map(|s| format!("'{}'", escape(&s.id))).collect();
            let contains_batch = format!(
                "MATCH (m:Module), (s:Symbol) WHERE m.id = '{}' AND s.id IN [{}] CREATE (m)-[:CONTAINS]->(s)",
                escape(module_id),
                sym_ids.join(", ")
            );
            let _ = conn.query(&contains_batch);

            // Batch DEFINES edges: file -> symbols
            let defines_batch = format!(
                "MATCH (f:File), (s:Symbol) WHERE f.id = '{}' AND s.id IN [{}] CREATE (f)-[:DEFINES]->(s)",
                escape(&extraction.file),
                sym_ids.join(", ")
            );
            let _ = conn.query(&defines_batch);
        }

        // Batch insert relationships grouped by type
        let mut calls_pairs: Vec<(&str, &str)> = Vec::new();
        let mut inherits_pairs: Vec<(&str, &str)> = Vec::new();
        let mut tested_by_pairs: Vec<(&str, &str)> = Vec::new();
        let mut imports_pairs: Vec<(&str, &str)> = Vec::new();
        let mut reads_pairs: Vec<(&str, &str)> = Vec::new();
        let mut writes_pairs: Vec<(&str, &str)> = Vec::new();
        for rel in &extraction.relations {
            match rel.kind {
                RelationKind::Calls | RelationKind::CalledBy => calls_pairs.push((&rel.source_id, &rel.target_id)),
                RelationKind::Inherits | RelationKind::InheritedBy => inherits_pairs.push((&rel.source_id, &rel.target_id)),
                RelationKind::TestedBy | RelationKind::Tests => tested_by_pairs.push((&rel.source_id, &rel.target_id)),
                RelationKind::Imports | RelationKind::ImportedBy => imports_pairs.push((&rel.source_id, &rel.target_id)),
                RelationKind::Reads => reads_pairs.push((&rel.source_id, &rel.target_id)),
                RelationKind::Writes => writes_pairs.push((&rel.source_id, &rel.target_id)),
                _ => {}
            }
        }
        for (pairs, rel_type) in [
            (&calls_pairs, "CALLS"),
            (&inherits_pairs, "INHERITS"),
            (&tested_by_pairs, "TESTED_BY"),
            (&reads_pairs, "READS"),
            (&writes_pairs, "WRITES"),
        ] {
            if pairs.is_empty() { continue; }
            let pair_list: Vec<String> = pairs.iter().map(|(a, b)| {
                format!("{{a: '{}', b: '{}'}}", escape(a), escape(b))
            }).collect();
            let batch_rel = format!(
                "UNWIND [{}] AS p MATCH (a:Symbol), (b:Symbol) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:{}]->(b)",
                pair_list.join(", "),
                rel_type
            );
            let _ = conn.query(&batch_rel);
        }
        if !imports_pairs.is_empty() {
            let pair_list: Vec<String> = imports_pairs.iter().map(|(a, b)| {
                format!("{{a: '{}', b: '{}'}}", escape(a), escape(b))
            }).collect();
            let _ = conn.query(&format!(
                "UNWIND [{}] AS p MATCH (a:Module), (b:Module) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:IMPORTS]->(b)",
                pair_list.join(", ")
            ));
        }

        Ok(())
    }

    /// Bulk insert all extractions in minimal queries — one UNWIND per node/edge type.
    /// Much faster than calling upsert_file_conn_no_delete per file.
    pub fn upsert_all_bulk(&self, conn: &Connection<'_>, extractions: &[FileExtraction]) -> Result<()> {
        if extractions.is_empty() { return Ok(()); }

        // 1. All Module nodes
        let module_rows: Vec<String> = extractions.iter().map(|e| {
            let name = e.file.rsplit_once('/').map(|(_, f)| f).unwrap_or(&e.file);
            format!("{{id: '{}', name: '{}', file: '{}', language: '{}', content_hash: '{}'}}",
                escape(&e.file), escape(name), escape(&e.file), escape(&e.language), escape(&e.content_hash))
        }).collect();
        conn.query(&format!("UNWIND [{}] AS m CREATE (:Module {{id: m.id, name: m.name, file: m.file, language: m.language, content_hash: m.content_hash}})", module_rows.join(", ")))
            .context("bulk module insert")?;

        // 2. All File nodes
        let file_rows: Vec<String> = extractions.iter().map(|e| {
            let name = e.file.rsplit_once('/').map(|(_, f)| f).unwrap_or(&e.file);
            format!("{{id: '{}', name: '{}', path: '{}', language: '{}', symbol_count: {}}}",
                escape(&e.file), escape(name), escape(&e.file), escape(&e.language), e.symbols.len())
        }).collect();
        conn.query(&format!("UNWIND [{}] AS f CREATE (:File {{id: f.id, name: f.name, path: f.path, language: f.language, symbol_count: f.symbol_count}})", file_rows.join(", ")))
            .context("bulk file insert")?;

        // 3. All Symbol nodes in chunks (query string size limit)
        const SYM_CHUNK: usize = 2000;
        let all_syms: Vec<String> = extractions.iter().flat_map(|e| {
            e.symbols.iter().map(move |sym| format!(
                "{{id: '{}', name: '{}', kind: '{}', file: '{}', start_line: {}, end_line: {}, signature_hash: '{}', language: '{}', visibility: '{}', parent: '{}', docstring: '{}', complexity: {}}}",
                escape(&sym.id), escape(&sym.name), sym.kind.as_str(), escape(&e.file),
                sym.span.start_line, sym.span.end_line, escape(&sym.signature_hash),
                escape(&sym.language), escape(sym.visibility.as_deref().unwrap_or("")),
                escape(sym.parent.as_deref().unwrap_or("")),
                escape(sym.docstring.as_deref().unwrap_or("")), sym.complexity
            ))
        }).collect();
        for chunk in all_syms.chunks(SYM_CHUNK) {
            conn.query(&format!(
                "UNWIND [{}] AS s CREATE (:Symbol {{id: s.id, name: s.name, kind: s.kind, file: s.file, start_line: s.start_line, end_line: s.end_line, signature_hash: s.signature_hash, language: s.language, visibility: s.visibility, parent: s.parent, docstring: s.docstring, complexity: s.complexity}})",
                chunk.join(", ")
            )).context("bulk symbol insert")?;
        }

        // 4. CONTAINS edges (module -> symbols) in chunks
        let contains_pairs: Vec<String> = extractions.iter().flat_map(|e| {
            e.symbols.iter().map(move |sym| format!("{{m: '{}', s: '{}'}}", escape(&e.file), escape(&sym.id)))
        }).collect();
        for chunk in contains_pairs.chunks(SYM_CHUNK) {
            let _ = conn.query(&format!(
                "UNWIND [{}] AS p MATCH (m:Module), (s:Symbol) WHERE m.id = p.m AND s.id = p.s CREATE (m)-[:CONTAINS]->(s)",
                chunk.join(", ")
            ));
        }

        // 5. DEFINES edges (file -> symbols) in chunks
        let defines_pairs: Vec<String> = extractions.iter().flat_map(|e| {
            e.symbols.iter().map(move |sym| format!("{{f: '{}', s: '{}'}}", escape(&e.file), escape(&sym.id)))
        }).collect();
        for chunk in defines_pairs.chunks(SYM_CHUNK) {
            let _ = conn.query(&format!(
                "UNWIND [{}] AS p MATCH (f:File), (s:Symbol) WHERE f.id = p.f AND s.id = p.s CREATE (f)-[:DEFINES]->(s)",
                chunk.join(", ")
            ));
        }

        // 6. All relation edges grouped by type
        let mut calls_pairs: Vec<String> = Vec::new();
        let mut inherits_pairs: Vec<String> = Vec::new();
        let mut tested_by_pairs: Vec<String> = Vec::new();
        let mut imports_pairs: Vec<String> = Vec::new();
        let mut reads_pairs: Vec<String> = Vec::new();
        let mut writes_pairs: Vec<String> = Vec::new();
        for e in extractions {
            for rel in &e.relations {
                let pair = format!("{{a: '{}', b: '{}'}}", escape(&rel.source_id), escape(&rel.target_id));
                match rel.kind {
                    RelationKind::Calls | RelationKind::CalledBy => calls_pairs.push(pair),
                    RelationKind::Inherits | RelationKind::InheritedBy => inherits_pairs.push(pair),
                    RelationKind::TestedBy | RelationKind::Tests => tested_by_pairs.push(pair),
                    RelationKind::Imports | RelationKind::ImportedBy => imports_pairs.push(pair),
                    RelationKind::Reads => reads_pairs.push(pair),
                    RelationKind::Writes => writes_pairs.push(pair),
                    _ => {}
                }
            }
        }
        for (pairs, rel_type) in [(&calls_pairs, "CALLS"), (&inherits_pairs, "INHERITS"), (&tested_by_pairs, "TESTED_BY"), (&reads_pairs, "READS"), (&writes_pairs, "WRITES")] {
            for chunk in pairs.chunks(SYM_CHUNK) {
                let _ = conn.query(&format!(
                    "UNWIND [{}] AS p MATCH (a:Symbol), (b:Symbol) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:{rel_type}]->(b)",
                    chunk.join(", ")
                ));
            }
        }
        for chunk in imports_pairs.chunks(SYM_CHUNK) {
            let _ = conn.query(&format!(
                "UNWIND [{}] AS p MATCH (a:Module), (b:Module) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:IMPORTS]->(b)",
                chunk.join(", ")
            ));
        }

        Ok(())
    }

    /// Create Folder nodes for each ancestor directory and wire up
    /// CONTAINS_FOLDER (parent -> child) and CONTAINS_FILE (leaf folder -> file) edges.
    #[allow(dead_code)]
    fn upsert_folder_hierarchy(&self, conn: &Connection<'_>, file_path: &str) -> Result<()> {
        // Split the file path into components: "src/graph/store.rs" -> ["src", "graph"]
        let parts: Vec<&str> = file_path.rsplitn(2, '/').collect();
        let dir_path = if parts.len() == 2 { parts[1] } else { return Ok(()) };

        // Collect all ancestor folders: "src/graph" -> ["src", "src/graph"]
        let segments: Vec<&str> = dir_path.split('/').collect();
        let mut folder_paths: Vec<String> = Vec::with_capacity(segments.len());
        for i in 0..segments.len() {
            let path = segments[..=i].join("/");
            folder_paths.push(path);
        }

        // Create Folder nodes (MERGE-style: only create if not exists)
        for folder_path in &folder_paths {
            let folder_name = folder_path
                .rsplit_once('/')
                .map(|(_, n)| n)
                .unwrap_or(folder_path);
            let merge_folder = format!(
                "MERGE (d:Folder {{id: '{}'}})",
                escape(folder_path),
            );
            // Try MERGE first; if Kuzu doesn't support MERGE, fall back to conditional create
            if conn.query(&merge_folder).is_err() {
                // Check if it already exists
                let check = format!(
                    "MATCH (d:Folder) WHERE d.id = '{}' RETURN d.id",
                    escape(folder_path)
                );
                let mut result = conn.query(&check)
                    .map_err(|e| anyhow::anyhow!("folder check failed: {e}"))?;
                if result.next().is_none() {
                    let create = format!(
                        "CREATE (d:Folder {{id: '{}', name: '{}', path: '{}'}})",
                        escape(folder_path),
                        escape(folder_name),
                        escape(folder_path),
                    );
                    let _ = conn.query(&create);
                }
            } else {
                // MERGE succeeded but may not have set name/path; update them
                let update = format!(
                    "MATCH (d:Folder) WHERE d.id = '{}' SET d.name = '{}', d.path = '{}'",
                    escape(folder_path),
                    escape(folder_name),
                    escape(folder_path),
                );
                let _ = conn.query(&update);
            }
        }

        // Create CONTAINS_FOLDER edges between consecutive folders
        for i in 1..folder_paths.len() {
            let parent = &folder_paths[i - 1];
            let child = &folder_paths[i];
            // Check if edge already exists
            let check_edge = format!(
                "MATCH (p:Folder)-[:CONTAINS_FOLDER]->(c:Folder) WHERE p.id = '{}' AND c.id = '{}' RETURN p.id",
                escape(parent),
                escape(child),
            );
            let mut result = conn.query(&check_edge)
                .map_err(|e| anyhow::anyhow!("edge check failed: {e}"))?;
            if result.next().is_none() {
                let create_edge = format!(
                    "MATCH (p:Folder), (c:Folder) WHERE p.id = '{}' AND c.id = '{}' CREATE (p)-[:CONTAINS_FOLDER]->(c)",
                    escape(parent),
                    escape(child),
                );
                let _ = conn.query(&create_edge);
            }
        }

        // Create CONTAINS_FILE edge from leaf folder to File node
        if let Some(leaf_folder) = folder_paths.last() {
            let check_edge = format!(
                "MATCH (d:Folder)-[:CONTAINS_FILE]->(f:File) WHERE d.id = '{}' AND f.id = '{}' RETURN d.id",
                escape(leaf_folder),
                escape(file_path),
            );
            let mut result = conn.query(&check_edge)
                .map_err(|e| anyhow::anyhow!("edge check failed: {e}"))?;
            if result.next().is_none() {
                let create_edge = format!(
                    "MATCH (d:Folder), (f:File) WHERE d.id = '{}' AND f.id = '{}' CREATE (d)-[:CONTAINS_FILE]->(f)",
                    escape(leaf_folder),
                    escape(file_path),
                );
                let _ = conn.query(&create_edge);
            }
        }

        Ok(())
    }

    /// Remove all graph data for a deleted file.
    pub fn remove_file(&self, file: &str) -> Result<()> {
        let conn = self.connection()?;
        let _ = conn.query(&format!(
            "MATCH (s:Symbol) WHERE s.file = '{}' DETACH DELETE s",
            escape(file)
        ));
        let _ = conn.query(&format!(
            "MATCH (m:Module) WHERE m.file = '{}' DETACH DELETE m",
            escape(file)
        ));
        let _ = conn.query(&format!(
            "MATCH (f:File) WHERE f.id = '{}' DETACH DELETE f",
            escape(file)
        ));
        Ok(())
    }

    /// Return map of file path -> content_hash for all indexed modules.
    /// Used by incremental indexing to skip unchanged files.
    pub fn get_file_hashes(&self) -> Result<HashMap<String, String>> {
        let conn = self.connection()?;
        let mut result = conn
            .query("MATCH (m:Module) RETURN m.file, m.content_hash")
            .map_err(|e| anyhow::anyhow!("get_file_hashes failed: {e}"))?;
        let mut map = HashMap::new();
        while let Some(row) = result.next() {
            if row.len() >= 2 {
                map.insert(row[0].to_string(), row[1].to_string());
            }
        }
        Ok(map)
    }

    /// Return all symbols as (name, id, file, kind) tuples — used by resolve_calls.
    pub fn get_all_symbols(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.connection()?;
        let mut result = conn
            .query("MATCH (s:Symbol) RETURN s.name, s.id, s.file, s.kind")
            .map_err(|e| anyhow::anyhow!("get_all_symbols failed: {e}"))?;
        let mut symbols = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 4 {
                symbols.push((row[0].to_string(), row[1].to_string(), row[2].to_string(), row[3].to_string()));
            }
        }
        Ok(symbols)
    }

    /// Create Folder nodes and edges for a set of file paths in bulk.
    /// More efficient than per-file upsert_folder_hierarchy calls.
    pub fn upsert_folders_bulk(&self, file_paths: &[&str]) -> Result<()> {
        let conn = self.connection()?;
        self.upsert_folders_bulk_conn(&conn, file_paths)
    }

    pub fn upsert_folders_bulk_conn(&self, conn: &Connection<'_>, file_paths: &[&str]) -> Result<()> {
        let mut all_folders: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for file_path in file_paths {
            let parts: Vec<&str> = file_path.rsplitn(2, '/').collect();
            if parts.len() < 2 { continue; }
            let dir_path = parts[1];
            let segments: Vec<&str> = dir_path.split('/').collect();
            for i in 0..segments.len() {
                all_folders.insert(segments[..=i].join("/"));
            }
        }

        if all_folders.is_empty() { return Ok(()); }

        // Write Folder nodes to parquet
        let folder_pq = std::env::temp_dir().join("infigraph_folders.parquet");
        {
            let ids: Vec<&str> = all_folders.iter().map(|s| s.as_str()).collect();
            let names: Vec<&str> = all_folders.iter().map(|fp| fp.rsplit_once('/').map(|(_, n)| n).unwrap_or(fp.as_str())).collect();
            let paths: Vec<&str> = all_folders.iter().map(|s| s.as_str()).collect();
            parquet_loader::write_node_parquet(
                &folder_pq,
                &[("id", DataType::Utf8), ("name", DataType::Utf8), ("path", DataType::Utf8)],
                vec![Arc::new(StringArray::from(ids)), Arc::new(StringArray::from(names)), Arc::new(StringArray::from(paths))],
            )?;
        }

        // Collect edge pairs in memory
        let cf_pairs: Vec<(String, String)> = all_folders.iter()
            .filter_map(|child| {
                child.rsplit_once('/').map(|(p, _)| p).and_then(|parent_path| {
                    if all_folders.contains(parent_path) { Some((parent_path.to_string(), child.clone())) } else { None }
                })
            }).collect();

        let cfile_pairs: Vec<(String, String)> = file_paths.iter()
            .filter_map(|fp| {
                let parts: Vec<&str> = fp.rsplitn(2, '/').collect();
                if parts.len() < 2 { return None; }
                Some((parts[1].to_string(), fp.to_string()))
            }).collect();

        let copy_ok = conn.query(&format!("COPY Folder FROM '{}'", fwd_slash_path(&folder_pq))).is_ok();

        if copy_ok {
            // Write edge parquet files and COPY FROM
            let cf_pq = std::env::temp_dir().join("infigraph_contains_folder.parquet");
            let cf_refs: Vec<(&str, &str)> = cf_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            parquet_loader::write_edge_parquet(&cf_pq, &cf_refs)?;
            if let Err(e) = conn.query(&format!("COPY CONTAINS_FOLDER FROM '{}'", fwd_slash_path(&cf_pq))) {
                eprintln!("warn: COPY CONTAINS_FOLDER failed ({e}), using UNWIND fallback");
                unwind_edges_from_pairs(conn, &cf_refs, "CONTAINS_FOLDER", "Folder", "Folder");
            }
            let _ = std::fs::remove_file(&cf_pq);

            let cfile_pq = std::env::temp_dir().join("infigraph_contains_file.parquet");
            let cfile_refs: Vec<(&str, &str)> = cfile_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            parquet_loader::write_edge_parquet(&cfile_pq, &cfile_refs)?;
            if let Err(e) = conn.query(&format!("COPY CONTAINS_FILE FROM '{}'", fwd_slash_path(&cfile_pq))) {
                eprintln!("warn: COPY CONTAINS_FILE failed ({e}), using UNWIND fallback");
                unwind_edges_from_pairs(conn, &cfile_refs, "CONTAINS_FILE", "Folder", "File");
            }
            let _ = std::fs::remove_file(&cfile_pq);
        } else {
            // Incremental path: some folders may already exist. Use UNWIND with MERGE semantics.
            const CHUNK: usize = 500;
            for chunk in all_folders.iter().collect::<Vec<_>>().chunks(CHUNK) {
                let items: Vec<String> = chunk.iter().map(|fp| {
                    let name = fp.rsplit_once('/').map(|(_, n)| n).unwrap_or(fp);
                    format!("{{id: '{}', name: '{}', path: '{}'}}", escape(fp), escape(name), escape(fp))
                }).collect();
                let _ = conn.query(&format!(
                    "UNWIND [{}] AS f MERGE (d:Folder {{id: f.id}}) ON CREATE SET d.name = f.name, d.path = f.path ON MATCH SET d.name = f.name, d.path = f.path",
                    items.join(", ")
                ));
            }
            let cf_refs: Vec<(&str, &str)> = cf_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            unwind_edges_from_pairs(conn, &cf_refs, "CONTAINS_FOLDER", "Folder", "Folder");
            let cfile_refs: Vec<(&str, &str)> = cfile_pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            unwind_edges_from_pairs(conn, &cfile_refs, "CONTAINS_FILE", "Folder", "File");
        }

        let _ = std::fs::remove_file(&folder_pq);
        Ok(())
    }

    /// Get total counts for stats.
    pub fn stats(&self) -> Result<GraphStats> {
        let conn = self.connection()?;

        let symbol_count = count_query(&conn, "MATCH (s:Symbol) RETURN count(s)")?;
        let module_count = count_query(&conn, "MATCH (m:Module) RETURN count(m)")?;
        let file_count = count_query(&conn, "MATCH (f:File) RETURN count(f)")?;
        let folder_count = count_query(&conn, "MATCH (d:Folder) RETURN count(d)")?;
        let calls_count = count_query(&conn, "MATCH ()-[r:CALLS]->() RETURN count(r)")?;
        let inherits_count = count_query(&conn, "MATCH ()-[r:INHERITS]->() RETURN count(r)")?;
        let contains_count = count_query(&conn, "MATCH ()-[r:CONTAINS]->() RETURN count(r)")?;

        Ok(GraphStats {
            symbols: symbol_count,
            modules: module_count,
            files: file_count,
            folders: folder_count,
            calls: calls_count,
            inherits: inherits_count,
            contains: contains_count,
        })
    }

    /// Test: DELETE + COPY FROM parquet produces identical data to MERGE/UNWIND.
    /// Covers edge cases: <>, quotes, unicode, empty strings, backslashes, newlines.
    pub fn test_parquet_quality(&self) -> Result<()> {
        let conn = self.connection()?;

        let full_schema = "CREATE NODE TABLE %TABLE%(id STRING, name STRING, kind STRING, file STRING, start_line INT64, end_line INT64, signature_hash STRING, language STRING, visibility STRING, parent STRING, docstring STRING, complexity INT64, PRIMARY KEY(id))";

        // Edge case test data — every known problematic pattern
        let long_doc = "A".repeat(10000);
        let test_rows: Vec<(&str, &str, &str, &str, i64, i64, &str, &str, &str, &str, &str, i64)> = vec![
            ("t1", "normal_func", "Function", "src/main.rs", 1, 10, "abc", "rust", "public", "", "Normal docstring", 3),
            ("t2", "angle_brackets", "Function", "src/lib.rs", 5, 20, "def", "java", "", "", "Returns List<String> from <code>parse</code>", 1),
            ("t3", "flask_route", "Function", "app.py", 2, 8, "ghi", "python", "public", "", "@app.route(\"/api/users/<int:id>\", methods=[\"GET\"])", 2),
            ("t4", "regex_group", "Function", "src/re.py", 10, 50, "jkl", "python", "", "", "(?P<query>.+)/$", 5),
            ("t5", "html_javadoc", "Method", "Foo.java", 3, 15, "mno", "java", "public", "Foo", "/** Wraps <p>text</p> in {@link List<T>} */", 4),
            ("t6", "double_quotes", "Function", "bar.rs", 1, 5, "pqr", "rust", "", "", "Returns \"hello world\" and \"goodbye\"", 1),
            ("t7", "single_quotes", "Function", "baz.py", 1, 5, "stu", "python", "", "", "It's a test with 'single' quotes", 1),
            ("t8", "backslashes", "Function", "esc.rs", 1, 5, "vwx", "rust", "", "", "Path is C:\\Users\\test\\file.txt", 1),
            ("t09", "unicode", "Class", "uni.py", 1, 5, "yza", "python", "", "Parent", "Ünïcödé: 日本語テスト 🚀", 0),
            ("t10", "empty_all", "Variable", "e.rs", 0, 0, "", "", "", "", "", 0),
            ("t11", "tab_content", "Function", "tab.rs", 1, 5, "tab", "rust", "", "", "col1\tcol2\tcol3", 1),
            ("t12", "newline_content", "Function", "nl.rs", 1, 5, "nln", "rust", "", "", "line1\nline2\nline3", 1),
            ("t13", "mixed_evil", "Function", "evil.java", 1, 99, "evil", "java", "public", "", "/** @param <T extends Comparable<? super T>> \\n uses 'single' and \"double\" */", 9),
            // Real-world: Java Javadoc with HTML tags (tto-engine pattern, 332 mismatches)
            ("t14", "javadoc_html", "Class", "Util.java", 1, 200, "jdoc", "java", "public", "", "/** Perl's split function and <b>s</b> operation inspired. Uses {@link #substitute substitute()} */", 3),
            ("t15", "javadoc_code", "Method", "StreamSearcher.java", 1, 50, "jcod", "java", "public", "", "/**  * performs a function similar to the Unix <code>strings</code> command */", 2),
            ("t16", "javadoc_p_tag", "Method", "GlobFilenameFilter.java", 1, 30, "jpag", "java", "public", "", "/**    * Filters a filename.    * <p>    * @param dir  The directory.    * @return True if match.    */", 1),
            ("t17", "javadoc_link_generic", "Method", "PatternCache.java", 1, 60, "jlnk", "java", "public", "", "/**    * Returns a {@link PatternCache<T>} instance.    * <p>    * Uses {@link #getPattern getPattern()} internally.    */", 4),
            // Real-world: Ruby paths with backslashes (WTax pattern)
            ("t18", "ruby_backslash_path", "Constant", "consts.rb", 1, 5, "rbsp", "ruby", "", "", "Update allows: <anyBasefolderStructureDesired>\\Protax\\LacerteTax\\...", 0),
            ("t19", "ruby_interpolation", "Constant", "consts.rb", 2, 5, "rbin", "ruby", "", "", "lacerte\\#{YEAR_YY}tax\\\\ + NETBRANCH + \\\\Loader\\\\CDROMWIN\\\\", 0),
            // Real-world: VB6 comments (EasyAcct pattern)
            ("t20", "vb6_comment", "Function", "ad911cal.bas", 1, 20, "vb6c", "basic", "", "", "'---PDB 04/02/02 verify if asset complies with sept 11 01 30% rules", 1),
            ("t21", "vb6_include", "Variable", "ad911cal.bas", 3, 3, "vb6i", "basic", "", "", "'$INCLUDE: 'EZDIMCOM.INC'", 0),
            // Real-world: C# XML doc comments (federal pattern)
            ("t22", "csharp_xmldoc", "Method", "TaxCalc.cs", 1, 15, "csxd", "csharp", "public", "TaxCalc", "/// <summary>Calculates <see cref=\"TaxResult\"/> for given <paramref name=\"input\"/></summary>", 2),
            ("t23", "csharp_generic", "Class", "Repository.cs", 1, 100, "csgn", "csharp", "public", "", "/// <typeparam name=\"T\">Must implement <see cref=\"IEntity{T}\"/></typeparam>", 5),
            // SQL injection-style content
            ("t24", "sql_in_doc", "Function", "db.py", 1, 10, "sqli", "python", "", "", "Runs: SELECT * FROM users WHERE name = 'O\\'Brien' AND id > 0; -- drop table", 1),
            // Markdown in docstrings
            ("t25", "markdown_doc", "Function", "lib.rs", 1, 20, "mkdn", "rust", "public", "", "# Header\n\n```rust\nfn main() { println!(\"hello\"); }\n```\n\n- item `<T>`\n- [link](http://example.com?a=1&b=2)", 3),
            // JSON in docstrings
            ("t26", "json_doc", "Function", "api.py", 1, 10, "json", "python", "", "", "Returns {\"key\": \"value\", \"list\": [1, 2, 3], \"nested\": {\"a\": true}}", 1),
            // XML/HTML entities
            ("t27", "entity_doc", "Function", "parser.rs", 1, 10, "enty", "rust", "", "", "Handles &amp; &lt; &gt; &quot; &#39; entities plus raw < > & \" '", 2),
            // Very long docstring (stress test)
            ("t28", "long_doc", "Function", "big.java", 1, 500, "long", "java", "public", "", &long_doc, 99),
            // Null bytes and control characters
            ("t29", "control_chars", "Function", "ctrl.rs", 1, 5, "ctrl", "rust", "", "", "has \x01 \x02 \x03 control chars and \x7f DEL", 1),
            // Windows CRLF
            ("t30", "crlf_doc", "Function", "win.cs", 1, 5, "crlf", "csharp", "", "", "line1\r\nline2\r\nline3", 1),
            // Deeply nested generics (Java/C#)
            ("t31", "nested_generics", "Method", "Deep.java", 1, 10, "deep", "java", "public", "", "Map<String, List<Pair<Integer, Consumer<? super T>>>> process()", 8),
            // Percent and special URL chars
            ("t32", "url_doc", "Function", "http.py", 1, 5, "urls", "python", "", "", "GET /api/v1/users?name=John%20Doe&age=30#section HTTP/1.1", 1),
            // Pipe chars (can confuse some parsers)
            ("t33", "pipe_doc", "Function", "sh.rs", 1, 5, "pipe", "rust", "", "", "cat file.txt | grep 'pattern' | awk '{print $1}' | sort -u", 1),
            // Regex with all special chars
            ("t34", "regex_full", "Function", "re.py", 1, 5, "regx", "python", "", "", "^(?:https?://)?(?:www\\.)?([^/?#]+)(?:[/?#]|$)", 3),
            // Triple quotes and mixed quotes
            ("t35", "triple_quote", "Function", "doc.py", 1, 5, "trpl", "python", "", "", "\"\"\"This is a '''triple quoted''' \"docstring\" with 'mixed' quotes\"\"\"", 1),
        ];

        println!("=== Parquet Quality Test ({} edge cases) ===\n", test_rows.len());

        // === Method A: Direct parquet COPY FROM (proposed new path) ===
        let _ = conn.query("DROP TABLE IF EXISTS QualParquet");
        conn.query(&full_schema.replace("%TABLE%", "QualParquet"))?;

        let pq_path = std::env::temp_dir().join("quality_test.parquet");
        {
            let ids: Vec<&str> = test_rows.iter().map(|r| r.0).collect();
            let names: Vec<&str> = test_rows.iter().map(|r| r.1).collect();
            let kinds: Vec<&str> = test_rows.iter().map(|r| r.2).collect();
            let files: Vec<&str> = test_rows.iter().map(|r| r.3).collect();
            let sls: Vec<i64> = test_rows.iter().map(|r| r.4).collect();
            let els: Vec<i64> = test_rows.iter().map(|r| r.5).collect();
            let sigs: Vec<&str> = test_rows.iter().map(|r| r.6).collect();
            let langs: Vec<&str> = test_rows.iter().map(|r| r.7).collect();
            let viss: Vec<&str> = test_rows.iter().map(|r| r.8).collect();
            let pars: Vec<&str> = test_rows.iter().map(|r| r.9).collect();
            let docs: Vec<&str> = test_rows.iter().map(|r| r.10).collect();
            let comps: Vec<i64> = test_rows.iter().map(|r| r.11).collect();

            parquet_loader::write_node_parquet(
                &pq_path,
                &[
                    ("id", DataType::Utf8), ("name", DataType::Utf8), ("kind", DataType::Utf8),
                    ("file", DataType::Utf8), ("start_line", DataType::Int64), ("end_line", DataType::Int64),
                    ("signature_hash", DataType::Utf8), ("language", DataType::Utf8),
                    ("visibility", DataType::Utf8), ("parent", DataType::Utf8),
                    ("docstring", DataType::Utf8), ("complexity", DataType::Int64),
                ],
                vec![
                    Arc::new(StringArray::from(ids)), Arc::new(StringArray::from(names)),
                    Arc::new(StringArray::from(kinds)), Arc::new(StringArray::from(files)),
                    Arc::new(Int64Array::from(sls)), Arc::new(Int64Array::from(els)),
                    Arc::new(StringArray::from(sigs)), Arc::new(StringArray::from(langs)),
                    Arc::new(StringArray::from(viss)), Arc::new(StringArray::from(pars)),
                    Arc::new(StringArray::from(docs)), Arc::new(Int64Array::from(comps)),
                ],
            )?;
        }
        conn.query(&format!("COPY QualParquet (id, name, kind, file, start_line, end_line, signature_hash, language, visibility, parent, docstring, complexity) FROM '{}'", fwd_slash_path(&pq_path)))?;

        // === Method B: DELETE + COPY FROM parquet (proposed incremental path) ===
        let _ = conn.query("DROP TABLE IF EXISTS QualDeleteCopy");
        conn.query(&full_schema.replace("%TABLE%", "QualDeleteCopy"))?;

        // Seed with dummy data first
        conn.query("CREATE (:QualDeleteCopy {id: 'dummy_1', name: 'old', kind: 'X', file: 'old.rs', start_line: 0, end_line: 0, signature_hash: '', language: '', visibility: '', parent: '', docstring: '', complexity: 0})")?;
        conn.query("CREATE (:QualDeleteCopy {id: 'dummy_2', name: 'old2', kind: 'X', file: 'old.rs', start_line: 0, end_line: 0, signature_hash: '', language: '', visibility: '', parent: '', docstring: '', complexity: 0})")?;

        // DELETE old rows then COPY FROM parquet
        conn.query("MATCH (n:QualDeleteCopy) DELETE n")?;
        conn.query(&format!("COPY QualDeleteCopy (id, name, kind, file, start_line, end_line, signature_hash, language, visibility, parent, docstring, complexity) FROM '{}'", fwd_slash_path(&pq_path)))?;

        // === Read back and compare ===
        let fields = ["id","name","kind","file","start_line","end_line","signature_hash","language","visibility","parent","docstring","complexity"];
        let field_list = fields.iter().map(|f| format!("s.{f}")).collect::<Vec<_>>().join(", ");

        let read_all = |table: &str| -> Result<Vec<Vec<String>>> {
            let mut r = conn.query(&format!("MATCH (s:{table}) RETURN {field_list} ORDER BY s.id"))?;
            let mut out = Vec::new();
            while let Some(row) = r.next() {
                out.push(row.iter().map(|v| v.to_string()).collect());
            }
            Ok(out)
        };

        let pq_rows = read_all("QualParquet")?;
        let dc_rows = read_all("QualDeleteCopy")?;

        // Compare Parquet vs DELETE+COPY
        println!("--- Parquet vs DELETE+COPY ---");
        let mut pass = 0;
        let mut fail = 0;
        for (i, (pr, dr)) in pq_rows.iter().zip(dc_rows.iter()).enumerate() {
            for (fi, field) in fields.iter().enumerate() {
                if pr.get(fi) != dr.get(fi) {
                    println!("  MISMATCH row={i} field={field}:");
                    println!("    parquet:      {:?}", pr.get(fi));
                    println!("    delete+copy:  {:?}", dr.get(fi));
                    fail += 1;
                } else {
                    pass += 1;
                }
            }
        }
        println!("  Result: {} passed, {} failed", pass, fail);

        // Compare Parquet vs expected (ground truth = input test data)
        // Use ID-based lookup since ORDER BY sorts lexicographically (t10 < t2)
        println!("\n--- Parquet vs Ground Truth ---");
        let mut gt_pass = 0;
        let mut gt_fail = 0;
        let stored_by_id: HashMap<&str, &Vec<String>> = pq_rows.iter()
            .filter_map(|r| r.first().map(|id| (id.as_str(), r)))
            .collect();
        for row in &test_rows {
            let expected = vec![
                row.0.to_string(), row.1.to_string(), row.2.to_string(), row.3.to_string(),
                row.4.to_string(), row.5.to_string(), row.6.to_string(), row.7.to_string(),
                row.8.to_string(), row.9.to_string(), row.10.to_string(), row.11.to_string(),
            ];
            if let Some(stored) = stored_by_id.get(row.0) {
                for (fi, field) in fields.iter().enumerate() {
                    let stored_val = stored.get(fi).map(|s| s.as_str()).unwrap_or("");
                    let expected_val = &expected[fi];
                    if stored_val == expected_val {
                        gt_pass += 1;
                    } else {
                        println!("  MISMATCH id={} field={field}:", row.0);
                        println!("    expected: {:?}", expected_val);
                        println!("    stored:   {:?}", stored_val);
                        gt_fail += 1;
                    }
                }
            } else {
                println!("  MISSING: id={} not found in stored data", row.0);
                gt_fail += 1;
            }
        }
        println!("  Result: {} passed, {} failed", gt_pass, gt_fail);

        if fail == 0 && gt_fail == 0 {
            println!("\n=== ALL TESTS PASSED — zero quality loss ===");
        } else {
            println!("\n=== QUALITY ISSUES DETECTED ===");
        }

        // Cleanup
        let _ = conn.query("DROP TABLE QualParquet");
        let _ = conn.query("DROP TABLE QualDeleteCopy");
        let _ = std::fs::remove_file(&pq_path);
        Ok(())
    }

    /// Benchmark: compare COPY FROM CSV vs UNWIND for bulk symbol inserts.
    /// Creates isolated test tables, measures both approaches, prints results.
    pub fn benchmark_bulk_write(&self, n: usize) -> Result<()> {
        let conn = self.connection()?;

        // Setup isolated test tables
        let _ = conn.query("DROP TABLE IF EXISTS BenchSymbolCopy");
        let _ = conn.query("DROP TABLE IF EXISTS BenchSymbolUnwind");
        conn.query("CREATE NODE TABLE BenchSymbolCopy(id STRING, name STRING, kind STRING, file STRING, PRIMARY KEY(id))")?;
        conn.query("CREATE NODE TABLE BenchSymbolUnwind(id STRING, name STRING, kind STRING, file STRING, PRIMARY KEY(id))")?;

        // --- COPY FROM CSV ---
        let csv_path = std::env::temp_dir().join("infigraph_bench_symbols.csv");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&csv_path)?;
            writeln!(f, "id,name,kind,file")?;
            for i in 0..n {
                writeln!(f, "copy_{i},func_{i},Function,bench.rs")?;
            }
        }
        let t0 = std::time::Instant::now();
        conn.query(&format!("COPY BenchSymbolCopy FROM '{}' (header=true)", fwd_slash_path(&csv_path)))?;
        let copy_ms = t0.elapsed().as_millis();

        // --- UNWIND ---
        const CHUNK: usize = 2000;
        let rows: Vec<String> = (0..n)
            .map(|i| format!("{{id: 'unwind_{i}', name: 'func_{i}', kind: 'Function', file: 'bench.rs'}}"))
            .collect();
        let t1 = std::time::Instant::now();
        for chunk in rows.chunks(CHUNK) {
            conn.query(&format!(
                "UNWIND [{}] AS s CREATE (:BenchSymbolUnwind {{id: s.id, name: s.name, kind: s.kind, file: s.file}})",
                chunk.join(", ")
            ))?;
        }
        let unwind_ms = t1.elapsed().as_millis();

        println!("Bulk write benchmark ({n} symbols):");
        println!("  COPY FROM CSV : {}ms", copy_ms);
        println!("  UNWIND chunks : {}ms", unwind_ms);
        println!("  Speedup       : {:.1}x", unwind_ms as f64 / copy_ms.max(1) as f64);

        // Cleanup
        let _ = conn.query("DROP TABLE BenchSymbolCopy");
        let _ = conn.query("DROP TABLE BenchSymbolUnwind");
        let _ = std::fs::remove_file(&csv_path);

        Ok(())
    }

    /// Bulk write all extractions using COPY FROM Parquet — binary format eliminates escaping issues.
    /// Used for --full index. Incremental index still uses upsert_file_conn_no_delete.
    pub fn upsert_all_parquet(&self, extractions: &[FileExtraction]) -> Result<()> {
        if extractions.is_empty() { return Ok(()); }

        let conn = self.connection()?;
        let tmp = std::env::temp_dir();

        let mut known_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for e in extractions {
            for sym in &e.symbols { known_ids.insert(sym.id.clone()); }
        }
        let mut sym_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let known_module_ids: std::collections::HashSet<String> = extractions.iter().map(|e| e.file.clone()).collect();

        // Collect all data into vecs
        let mut mod_ids = Vec::new(); let mut mod_names = Vec::new(); let mut mod_files = Vec::new();
        let mut mod_langs = Vec::new(); let mut mod_hashes = Vec::new(); let mut mod_summaries = Vec::new();
        let mut file_ids = Vec::new(); let mut file_names = Vec::new(); let mut file_paths = Vec::new();
        let mut file_langs = Vec::new(); let mut file_symcounts: Vec<i64> = Vec::new();
        let mut sym_ids = Vec::new(); let mut sym_names = Vec::new(); let mut sym_kinds = Vec::new();
        let mut sym_files = Vec::new(); let mut sym_slines: Vec<i64> = Vec::new(); let mut sym_elines: Vec<i64> = Vec::new();
        let mut sym_sighashes = Vec::new(); let mut sym_languages = Vec::new(); let mut sym_visibilities = Vec::new();
        let mut sym_parents = Vec::new(); let mut sym_docstrings = Vec::new(); let mut sym_complexities: Vec<i64> = Vec::new();
        let mut contains_pairs: Vec<(String, String)> = Vec::new();
        let mut defines_pairs: Vec<(String, String)> = Vec::new();

        let mut calls_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut inh_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut test_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut imp_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut reads_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut writes_seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
        let mut calls_pairs: Vec<(String, String)> = Vec::new();
        let mut inh_pairs: Vec<(String, String)> = Vec::new();
        let mut test_pairs: Vec<(String, String)> = Vec::new();
        let mut imp_pairs: Vec<(String, String)> = Vec::new();
        let mut reads_pairs: Vec<(String, String)> = Vec::new();
        let mut writes_pairs: Vec<(String, String)> = Vec::new();

        for e in extractions {
            let mod_name = e.file.rsplit_once('/').map(|(_, f)| f).unwrap_or(&e.file);
            mod_ids.push(e.file.clone()); mod_names.push(mod_name.to_string()); mod_files.push(e.file.clone());
            mod_langs.push(e.language.clone()); mod_hashes.push(e.content_hash.clone()); mod_summaries.push(String::new());

            file_ids.push(e.file.clone()); file_names.push(mod_name.to_string()); file_paths.push(e.file.clone());
            file_langs.push(e.language.clone()); file_symcounts.push(e.symbols.len() as i64);

            for sym in &e.symbols {
                if sym_seen.insert(sym.id.clone()) {
                    sym_ids.push(sym.id.clone()); sym_names.push(sym.name.clone());
                    sym_kinds.push(sym.kind.as_str().to_string()); sym_files.push(e.file.clone());
                    sym_slines.push(sym.span.start_line as i64); sym_elines.push(sym.span.end_line as i64);
                    sym_sighashes.push(sym.signature_hash.clone()); sym_languages.push(sym.language.clone());
                    sym_visibilities.push(sym.visibility.as_deref().unwrap_or("").to_string());
                    sym_parents.push(sym.parent.as_deref().unwrap_or("").to_string());
                    sym_docstrings.push(sym.docstring.as_deref().unwrap_or("").to_string());
                    sym_complexities.push(sym.complexity as i64);
                    contains_pairs.push((e.file.clone(), sym.id.clone()));
                    defines_pairs.push((e.file.clone(), sym.id.clone()));
                }
            }

            for rel in &e.relations {
                let src = rel.source_id.clone();
                let tgt = rel.target_id.clone();
                match rel.kind {
                    RelationKind::Imports | RelationKind::ImportedBy => {
                        if known_module_ids.contains(&src) && known_module_ids.contains(&tgt) {
                            if imp_seen.insert((src.clone(), tgt.clone())) { imp_pairs.push((src, tgt)); }
                        }
                    }
                    _ => {
                        if !known_ids.contains(&src) || !known_ids.contains(&tgt) { continue; }
                        match rel.kind {
                            RelationKind::Calls | RelationKind::CalledBy => {
                                if calls_seen.insert((src.clone(), tgt.clone())) { calls_pairs.push((src, tgt)); }
                            }
                            RelationKind::Inherits | RelationKind::InheritedBy => {
                                if inh_seen.insert((src.clone(), tgt.clone())) { inh_pairs.push((src, tgt)); }
                            }
                            RelationKind::TestedBy | RelationKind::Tests => {
                                if test_seen.insert((src.clone(), tgt.clone())) { test_pairs.push((src, tgt)); }
                            }
                            RelationKind::Reads => {
                                if reads_seen.insert((src.clone(), tgt.clone())) { reads_pairs.push((src, tgt)); }
                            }
                            RelationKind::Writes => {
                                if writes_seen.insert((src.clone(), tgt.clone())) { writes_pairs.push((src, tgt)); }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Write node parquet files
        let mod_pq = tmp.join("infigraph_index_modules.parquet");
        parquet_loader::write_node_parquet(&mod_pq, &[
            ("id", DataType::Utf8), ("name", DataType::Utf8), ("file", DataType::Utf8),
            ("language", DataType::Utf8), ("content_hash", DataType::Utf8), ("summary", DataType::Utf8),
        ], vec![
            Arc::new(StringArray::from(mod_ids)), Arc::new(StringArray::from(mod_names)),
            Arc::new(StringArray::from(mod_files)), Arc::new(StringArray::from(mod_langs)),
            Arc::new(StringArray::from(mod_hashes)), Arc::new(StringArray::from(mod_summaries)),
        ])?;

        let file_pq = tmp.join("infigraph_index_files.parquet");
        parquet_loader::write_node_parquet(&file_pq, &[
            ("id", DataType::Utf8), ("name", DataType::Utf8), ("path", DataType::Utf8),
            ("language", DataType::Utf8), ("symbol_count", DataType::Int64),
        ], vec![
            Arc::new(StringArray::from(file_ids)), Arc::new(StringArray::from(file_names)),
            Arc::new(StringArray::from(file_paths)), Arc::new(StringArray::from(file_langs)),
            Arc::new(Int64Array::from(file_symcounts)),
        ])?;

        let sym_pq = tmp.join("infigraph_index_symbols.parquet");
        parquet_loader::write_node_parquet(&sym_pq, &[
            ("id", DataType::Utf8), ("name", DataType::Utf8), ("kind", DataType::Utf8),
            ("file", DataType::Utf8), ("start_line", DataType::Int64), ("end_line", DataType::Int64),
            ("signature_hash", DataType::Utf8), ("language", DataType::Utf8),
            ("visibility", DataType::Utf8), ("parent", DataType::Utf8),
            ("docstring", DataType::Utf8), ("complexity", DataType::Int64),
        ], vec![
            Arc::new(StringArray::from(sym_ids)), Arc::new(StringArray::from(sym_names)),
            Arc::new(StringArray::from(sym_kinds)), Arc::new(StringArray::from(sym_files)),
            Arc::new(Int64Array::from(sym_slines)), Arc::new(Int64Array::from(sym_elines)),
            Arc::new(StringArray::from(sym_sighashes)), Arc::new(StringArray::from(sym_languages)),
            Arc::new(StringArray::from(sym_visibilities)), Arc::new(StringArray::from(sym_parents)),
            Arc::new(StringArray::from(sym_docstrings)), Arc::new(Int64Array::from(sym_complexities)),
        ])?;

        // COPY FROM parquet — node tables first
        conn.query(&format!("COPY Module FROM '{}'", fwd_slash_path(&mod_pq)))
            .map_err(|e| anyhow::anyhow!("COPY Module failed: {e}"))?;
        conn.query(&format!("COPY File FROM '{}'", fwd_slash_path(&file_pq)))
            .map_err(|e| anyhow::anyhow!("COPY File failed: {e}"))?;
        conn.query(&format!(
            "COPY Symbol (id, name, kind, file, start_line, end_line, signature_hash, language, visibility, parent, docstring, complexity) FROM '{}'",
            fwd_slash_path(&sym_pq)
        )).map_err(|e| anyhow::anyhow!("COPY Symbol failed: {e}"))?;

        // Edge tables — write parquet and COPY FROM with in-memory UNWIND fallback
        let edge_tables: Vec<(&str, &[(String, String)], &str, &str)> = vec![
            ("CONTAINS", &contains_pairs, "Module", "Symbol"),
            ("DEFINES", &defines_pairs, "File", "Symbol"),
            ("CALLS", &calls_pairs, "Symbol", "Symbol"),
            ("INHERITS", &inh_pairs, "Symbol", "Symbol"),
            ("TESTED_BY", &test_pairs, "Symbol", "Symbol"),
            ("IMPORTS", &imp_pairs, "Module", "Module"),
            ("READS", &reads_pairs, "Symbol", "Symbol"),
            ("WRITES", &writes_pairs, "Symbol", "Symbol"),
        ];

        for (table, pairs, src_label, dst_label) in &edge_tables {
            if pairs.is_empty() { continue; }
            let edge_pq = tmp.join(format!("infigraph_index_{}.parquet", table.to_lowercase()));
            let refs: Vec<(&str, &str)> = pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            parquet_loader::write_edge_parquet(&edge_pq, &refs)?;
            if let Err(e) = conn.query(&format!("COPY {table} FROM '{}'", fwd_slash_path(&edge_pq))) {
                eprintln!("warn: COPY {table} via parquet failed ({e}), falling back to UNWIND");
                unwind_edges_from_pairs(&conn, &refs, table, src_label, dst_label);
            }
            let _ = std::fs::remove_file(&edge_pq);
        }

        // Cleanup node parquet files
        let _ = std::fs::remove_file(&mod_pq);
        let _ = std::fs::remove_file(&file_pq);
        let _ = std::fs::remove_file(&sym_pq);

        Ok(())
    }

    /// Benchmark: CSV vs Parquet vs UNWIND — apple-to-apple with real symbol data.
    /// Tests performance AND data integrity (docstrings with <, >, quotes, unicode).
    pub fn benchmark_parquet_vs_csv(&self) -> Result<()> {
        let conn = self.connection()?;

        let mut result = conn.query(
            "MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line, s.signature_hash, s.language, s.visibility, s.parent, s.docstring, s.complexity"
        )?;
        let mut rows: Vec<Vec<String>> = Vec::new();
        while let Some(row) = result.next() {
            rows.push(row.iter().map(|v| v.to_string()).collect());
        }
        let n = rows.len();
        println!("Loaded {} real symbols from graph", n);

        let full_schema = "CREATE NODE TABLE %TABLE%(id STRING, name STRING, kind STRING, file STRING, start_line INT64, end_line INT64, signature_hash STRING, language STRING, visibility STRING, parent STRING, docstring STRING, complexity INT64, PRIMARY KEY(id))";
        let fields_list = "id, name, kind, file, start_line, end_line, signature_hash, language, visibility, parent, docstring, complexity";

        // ===== 1. COPY FROM CSV (TSV) =====
        let _ = conn.query("DROP TABLE IF EXISTS BenchCSV");
        conn.query(&full_schema.replace("%TABLE%", "BenchCSV"))?;

        let csv_path = std::env::temp_dir().join("infigraph_bench_csv.csv");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&csv_path)?;
            writeln!(f, "id\tname\tkind\tfile\tstart_line\tend_line\tsignature_hash\tlanguage\tvisibility\tparent\tdocstring\tcomplexity")?;
            let tsv_field = |s: &str| -> String {
                s.replace('\t', " ").replace('\n', " ").replace('\r', " ")
            };
            for row in &rows {
                writeln!(f, "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    tsv_field(&row[0]), tsv_field(&row[1]), tsv_field(&row[2]), tsv_field(&row[3]),
                    row[4], row[5],
                    tsv_field(&row[6]), tsv_field(&row[7]), tsv_field(&row[8]),
                    tsv_field(&row[9]), tsv_field(&row[10]), row[11])?;
            }
        }
        let csv_size = std::fs::metadata(&csv_path).map(|m| m.len()).unwrap_or(0);
        let t0 = std::time::Instant::now();
        conn.query(&format!("COPY BenchCSV FROM '{}' (header=true, delim='\\t')", fwd_slash_path(&csv_path)))?;
        let csv_ms = t0.elapsed().as_millis();

        // ===== 2. COPY FROM Parquet =====
        let _ = conn.query("DROP TABLE IF EXISTS BenchParquet");
        conn.query(&full_schema.replace("%TABLE%", "BenchParquet"))?;

        let pq_path = std::env::temp_dir().join("infigraph_bench.parquet");
        {
            let ids: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
            let names: Vec<&str> = rows.iter().map(|r| r[1].as_str()).collect();
            let kinds: Vec<&str> = rows.iter().map(|r| r[2].as_str()).collect();
            let files: Vec<&str> = rows.iter().map(|r| r[3].as_str()).collect();
            let start_lines: Vec<i64> = rows.iter().map(|r| r[4].parse().unwrap_or(0)).collect();
            let end_lines: Vec<i64> = rows.iter().map(|r| r[5].parse().unwrap_or(0)).collect();
            let sig_hashes: Vec<&str> = rows.iter().map(|r| r[6].as_str()).collect();
            let languages: Vec<&str> = rows.iter().map(|r| r[7].as_str()).collect();
            let visibilities: Vec<&str> = rows.iter().map(|r| r[8].as_str()).collect();
            let parents: Vec<&str> = rows.iter().map(|r| r[9].as_str()).collect();
            let docstrings: Vec<&str> = rows.iter().map(|r| r[10].as_str()).collect();
            let complexities: Vec<i64> = rows.iter().map(|r| r[11].parse().unwrap_or(0)).collect();

            parquet_loader::write_node_parquet(
                &pq_path,
                &[
                    ("id", DataType::Utf8), ("name", DataType::Utf8), ("kind", DataType::Utf8),
                    ("file", DataType::Utf8), ("start_line", DataType::Int64), ("end_line", DataType::Int64),
                    ("signature_hash", DataType::Utf8), ("language", DataType::Utf8),
                    ("visibility", DataType::Utf8), ("parent", DataType::Utf8),
                    ("docstring", DataType::Utf8), ("complexity", DataType::Int64),
                ],
                vec![
                    Arc::new(StringArray::from(ids)),
                    Arc::new(StringArray::from(names)),
                    Arc::new(StringArray::from(kinds)),
                    Arc::new(StringArray::from(files)),
                    Arc::new(Int64Array::from(start_lines)),
                    Arc::new(Int64Array::from(end_lines)),
                    Arc::new(StringArray::from(sig_hashes)),
                    Arc::new(StringArray::from(languages)),
                    Arc::new(StringArray::from(visibilities)),
                    Arc::new(StringArray::from(parents)),
                    Arc::new(StringArray::from(docstrings)),
                    Arc::new(Int64Array::from(complexities)),
                ],
            )?;
        }
        let pq_size = std::fs::metadata(&pq_path).map(|m| m.len()).unwrap_or(0);
        let t1 = std::time::Instant::now();
        conn.query(&format!("COPY BenchParquet ({fields_list}) FROM '{}'", fwd_slash_path(&pq_path)))?;
        let pq_ms = t1.elapsed().as_millis();

        // ===== 3. UNWIND =====
        let _ = conn.query("DROP TABLE IF EXISTS BenchUnwind");
        conn.query(&full_schema.replace("%TABLE%", "BenchUnwind"))?;

        const CHUNK: usize = 2000;
        let unwind_rows: Vec<String> = rows.iter().map(|row| {
            format!("{{id: '{}', name: '{}', kind: '{}', file: '{}', start_line: {}, end_line: {}, signature_hash: '{}', language: '{}', visibility: '{}', parent: '{}', docstring: '{}', complexity: {}}}",
                escape(&row[0]), escape(&row[1]), escape(&row[2]), escape(&row[3]),
                row[4], row[5],
                escape(&row[6]), escape(&row[7]), escape(&row[8]),
                escape(&row[9]), escape(&row[10]), row[11])
        }).collect();
        let t2 = std::time::Instant::now();
        for chunk in unwind_rows.chunks(CHUNK) {
            conn.query(&format!(
                "UNWIND [{}] AS s CREATE (:BenchUnwind {{id: s.id, name: s.name, kind: s.kind, file: s.file, start_line: s.start_line, end_line: s.end_line, signature_hash: s.signature_hash, language: s.language, visibility: s.visibility, parent: s.parent, docstring: s.docstring, complexity: s.complexity}})",
                chunk.join(", ")
            ))?;
        }
        let unwind_ms = t2.elapsed().as_millis();

        // ===== Results =====
        println!("\n=== Bulk Write Benchmark ({n} symbols) ===\n");
        println!("  {:20} {:>8} {:>12} {:>10}", "Method", "Time", "Throughput", "File Size");
        println!("  {:20} {:>8} {:>12} {:>10}", "------", "----", "----------", "---------");
        println!("  {:20} {:>7}ms {:>9.0}/sec {:>9}KB",
            "COPY FROM CSV (TSV)", csv_ms, n as f64 / csv_ms.max(1) as f64 * 1000.0, csv_size / 1024);
        println!("  {:20} {:>7}ms {:>9.0}/sec {:>9}KB",
            "COPY FROM Parquet", pq_ms, n as f64 / pq_ms.max(1) as f64 * 1000.0, pq_size / 1024);
        println!("  {:20} {:>7}ms {:>9.0}/sec {:>10}",
            "UNWIND chunks", unwind_ms, n as f64 / unwind_ms.max(1) as f64 * 1000.0, "N/A");
        println!("\n  CSV vs Parquet     : {:.2}x", csv_ms as f64 / pq_ms.max(1) as f64);
        println!("  Parquet vs UNWIND  : {:.1}x", unwind_ms as f64 / pq_ms.max(1) as f64);

        // ===== Data Integrity =====
        println!("\n=== Data Integrity Check ===\n");
        let fields = ["id","name","kind","file","start_line","end_line","signature_hash","language","visibility","parent","docstring","complexity"];
        let field_list = fields.iter().map(|f| format!("s.{f}")).collect::<Vec<_>>().join(", ");

        let read_all = |table: &str| -> Result<Vec<Vec<String>>> {
            let mut r = conn.query(&format!("MATCH (s:{table}) RETURN {field_list} ORDER BY s.id"))?;
            let mut out = Vec::new();
            while let Some(row) = r.next() {
                out.push(row.iter().map(|v| v.to_string()).collect());
            }
            Ok(out)
        };

        let csv_rows = read_all("BenchCSV")?;
        let pq_rows = read_all("BenchParquet")?;
        let uw_rows = read_all("BenchUnwind")?;

        let compare = |name: &str, a: &[Vec<String>], b: &[Vec<String>]| {
            let mut mismatches = 0usize;
            if a.len() != b.len() {
                println!("  {name}: ROW COUNT MISMATCH ({} vs {})", a.len(), b.len());
                return;
            }
            for (i, (ar, br)) in a.iter().zip(b.iter()).enumerate() {
                for (fi, field) in fields.iter().enumerate() {
                    if ar.get(fi) != br.get(fi) {
                        if mismatches < 5 {
                            println!("  {name} MISMATCH row={i} field={field}:");
                            println!("    left:  {:?}", ar.get(fi));
                            println!("    right: {:?}", br.get(fi));
                        }
                        mismatches += 1;
                    }
                }
            }
            if mismatches == 0 {
                println!("  {name}: PASS — all {n} symbols × {} fields match", fields.len());
            } else {
                println!("  {name}: FAIL — {mismatches} mismatches");
            }
        };

        compare("CSV vs Parquet", &csv_rows, &pq_rows);
        compare("CSV vs UNWIND", &csv_rows, &uw_rows);
        compare("Parquet vs UNWIND", &pq_rows, &uw_rows);

        // Cleanup
        let _ = conn.query("DROP TABLE BenchCSV");
        let _ = conn.query("DROP TABLE BenchParquet");
        let _ = conn.query("DROP TABLE BenchUnwind");
        let _ = std::fs::remove_file(&csv_path);
        let _ = std::fs::remove_file(&pq_path);

        Ok(())
    }
}

#[derive(Debug)]
pub struct GraphStats {
    pub symbols: u64,
    pub modules: u64,
    pub files: u64,
    pub folders: u64,
    pub calls: u64,
    pub inherits: u64,
    pub contains: u64,
}

impl std::fmt::Display for GraphStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Graph Statistics:")?;
        writeln!(f, "  Symbols:      {}", self.symbols)?;
        writeln!(f, "  Modules:      {}", self.modules)?;
        writeln!(f, "  Files:        {}", self.files)?;
        writeln!(f, "  Folders:      {}", self.folders)?;
        writeln!(f, "  Calls edges:  {}", self.calls)?;
        writeln!(f, "  Inherits:     {}", self.inherits)?;
        writeln!(f, "  Contains:     {}", self.contains)
    }
}

fn count_query(conn: &Connection, query: &str) -> Result<u64> {
    let mut result = conn
        .query(query)
        .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
    if let Some(row) = result.next() {
        if let Some(val) = row.first() {
            return Ok(val.to_string().parse().unwrap_or(0));
        }
    }
    Ok(0)
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', " ")
        .replace('\r', "")
        .replace('\t', " ")
}

fn fwd_slash_path(p: &std::path::Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn unwind_edges_from_pairs(conn: &Connection, pairs: &[(&str, &str)], rel_type: &str, src_label: &str, dst_label: &str) {
    const CHUNK: usize = 500;
    for chunk in pairs.chunks(CHUNK) {
        let pair_list: Vec<String> = chunk.iter()
            .map(|(a, b)| format!("{{a: '{}', b: '{}'}}", escape(a), escape(b)))
            .collect();
        let _ = conn.query(&format!(
            "UNWIND [{}] AS p MATCH (a:{src_label}), (b:{dst_label}) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:{rel_type}]->(b)",
            pair_list.join(", ")
        ));
    }
}
