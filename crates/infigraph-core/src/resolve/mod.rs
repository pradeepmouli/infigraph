use std::collections::HashMap;

use anyhow::Result;

use crate::graph::store::GraphStore;
use crate::model::{FileExtraction, RelationKind};

/// Post-indexing pass that resolves call edges using cross-file symbol lookup.
/// Builds symbol map from the full graph (not just re-indexed files) so
/// incremental indexing doesn't lose cross-file resolution.
pub fn resolve_calls_incremental(
    store: &GraphStore,
    extractions: &[FileExtraction],
) -> Result<ResolveStats> {
    if extractions.is_empty() {
        return Ok(ResolveStats {
            total_calls: 0,
            resolved: 0,
            unresolved: 0,
        });
    }

    let conn = store.connection()?;

    // Build global symbol table from full graph: name -> [(id, file, kind)]
    let mut symbol_map: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for (name, id, file, kind) in store.get_all_symbols()? {
        symbol_map.entry(name).or_default().push((id, file, kind));
    }

    resolve_with_map(&conn, extractions, &symbol_map)
}

/// Post-indexing pass that resolves call edges using cross-file symbol lookup.
///
/// Problem: During extraction, `authenticate()` called in `main.py` creates
/// a CALLS relation targeting `main.py::authenticate`. But the real symbol
/// is `auth.py::authenticate`. This pass:
///
/// 1. Builds a symbol table from all extractions
/// 2. For each CALLS relation where the target doesn't exist locally,
///    searches the global symbol table by name
/// 3. Creates the resolved CALLS edge in the graph
pub fn resolve_calls(store: &GraphStore, extractions: &[FileExtraction]) -> Result<ResolveStats> {
    let conn = store.connection()?;

    // Build global symbol table: name -> list of (id, file, kind)
    let mut symbol_map: HashMap<String, Vec<(String, String, String)>> = HashMap::new();
    for ext in extractions {
        for sym in &ext.symbols {
            symbol_map.entry(sym.name.clone()).or_default().push((
                sym.id.clone(),
                ext.file.clone(),
                sym.kind.as_str().to_string(),
            ));
        }
    }

    resolve_with_map(&conn, extractions, &symbol_map)
}

fn resolve_with_map(
    conn: &kuzu::Connection<'_>,
    extractions: &[FileExtraction],
    symbol_map: &HashMap<String, Vec<(String, String, String)>>,
) -> Result<ResolveStats> {
    let mut resolved = 0;
    let mut unresolved = 0;
    let mut total_dangling = 0;
    let mut resolved_pairs: Vec<(String, String)> = Vec::new();

    for ext in extractions {
        let local_symbols: HashMap<&str, &str> = ext
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.id.as_str()))
            .collect();

        let imported_stems: std::collections::HashSet<String> = ext
            .relations
            .iter()
            .filter(|r| r.kind == RelationKind::Imports)
            .map(|r| {
                let raw = r
                    .target_id
                    .rsplit(['/', '\\', '.'])
                    .next()
                    .unwrap_or(&r.target_id);
                raw.to_lowercase()
            })
            .collect();

        let source_is_sql = ext.file.ends_with(".sql");

        for rel in &ext.relations {
            if rel.kind != RelationKind::Calls {
                continue;
            }

            let target_name = rel.target_id.rsplit("::").next().unwrap_or(&rel.target_id);

            if local_symbols.contains_key(target_name) {
                continue;
            }

            total_dangling += 1;

            if let Some(candidates) = symbol_map.get(target_name) {
                // Filter out same-file candidates and SQL CTEs (file-local scope).
                // SQL CTEs are extracted as Function kind — they should never
                // resolve cross-file since CTE names are scoped to their query.
                let cross_file: Vec<_> = candidates
                    .iter()
                    .filter(|(_, f, kind)| {
                        if *f == ext.file {
                            return false;
                        }
                        if source_is_sql && f.ends_with(".sql") && kind == "Function" {
                            return false;
                        }
                        true
                    })
                    .collect();

                let resolved_id = if cross_file.len() == 1 {
                    Some(cross_file[0].0.clone())
                } else if cross_file.len() > 1 {
                    let in_scope: Vec<_> = if !imported_stems.is_empty() {
                        cross_file
                            .iter()
                            .filter(|(_, f, _)| {
                                let stem = std::path::Path::new(f)
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .map(|s| s.to_lowercase())
                                    .unwrap_or_default();
                                imported_stems.contains(&stem)
                            })
                            .collect()
                    } else {
                        vec![]
                    };
                    if !in_scope.is_empty() {
                        Some(in_scope[0].0.clone())
                    } else if source_is_sql {
                        // SQL tables are project-global — pick first Class candidate
                        cross_file
                            .iter()
                            .find(|(_, _, k)| k == "Class")
                            .map(|(id, _, _)| id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(target_id) = resolved_id {
                    resolved_pairs.push((rel.source_id.clone(), target_id));
                    resolved += 1;
                } else {
                    unresolved += 1;
                }
            } else {
                unresolved += 1;
            }
        }
    }

    // Batch insert resolved CALLS edges via COPY FROM parquet
    if !resolved_pairs.is_empty() {
        // Build set of known symbol IDs to filter out dangling references.
        // Includes all symbols from the global map plus all from current extractions
        // (source_ids may reference symbols not yet in symbol_map during incremental).
        let mut known_ids: std::collections::HashSet<&str> = symbol_map
            .values()
            .flat_map(|v| v.iter().map(|(id, _, _)| id.as_str()))
            .collect();
        for ext in extractions {
            for sym in &ext.symbols {
                known_ids.insert(&sym.id);
            }
        }
        let valid_pairs: Vec<&(String, String)> = resolved_pairs
            .iter()
            .filter(|(src, tgt)| {
                known_ids.contains(src.as_str()) && known_ids.contains(tgt.as_str())
            })
            .collect();

        let refs: Vec<(&str, &str)> = valid_pairs
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let pq_path = std::env::temp_dir().join("infigraph_resolve_calls.parquet");
        crate::graph::parquet_loader::write_edge_parquet(&pq_path, &refs)?;
        let copy_result = conn.query(&format!(
            "COPY CALLS FROM '{}'",
            pq_path.to_string_lossy().replace('\\', "/")
        ));
        if let Err(e) = copy_result {
            eprintln!("[resolve] COPY FROM parquet failed ({e}), falling back to UNWIND");
            const CHUNK_SIZE: usize = 500;
            for chunk in refs.chunks(CHUNK_SIZE) {
                let pair_list: Vec<String> = chunk
                    .iter()
                    .map(|(a, b)| format!("{{a: '{}', b: '{}'}}", escape(a), escape(b)))
                    .collect();
                let _ = conn.query(&format!(
                    "UNWIND [{}] AS p MATCH (a:Symbol), (b:Symbol) WHERE a.id = p.a AND b.id = p.b CREATE (a)-[:CALLS]->(b)",
                    pair_list.join(", ")
                ));
            }
        }
        let _ = std::fs::remove_file(&pq_path);
    }

    Ok(ResolveStats {
        total_calls: total_dangling,
        resolved,
        unresolved,
    })
}

#[derive(Debug)]
pub struct ResolveStats {
    pub total_calls: usize,
    pub resolved: usize,
    pub unresolved: usize,
}

impl std::fmt::Display for ResolveStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Call resolution: {} cross-file calls, {} resolved, {} unresolved (builtins/externals)",
            self.total_calls, self.resolved, self.unresolved
        )
    }
}

fn escape(s: &str) -> String {
    s.replace('\'', "\\'")
}
