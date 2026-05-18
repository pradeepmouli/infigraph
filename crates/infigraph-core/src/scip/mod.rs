use std::path::Path;

use anyhow::{Context, Result};
use protobuf::Message;
use scip::types::{symbol_information, Index, SymbolRole};

use crate::graph::GraphStore;
use crate::model::{Span, SymbolKind};

/// Import a SCIP index.scip file into the Infigraph graph store.
///
/// SCIP (Sourcegraph Code Intelligence Protocol) provides compiler-grade
/// symbol definitions, references, and relationships. This supplements the
/// tree-sitter extractions with precise cross-file type/call resolution.
pub fn import_scip_index(index_path: &Path, store: &GraphStore) -> Result<ImportStats> {
    let bytes = std::fs::read(index_path)
        .with_context(|| format!("failed to read {}", index_path.display()))?;

    let index = Index::parse_from_bytes(&bytes)
        .with_context(|| format!("failed to parse SCIP index: {}", index_path.display()))?;

    let mut stats = ImportStats::default();
    let conn = store.connection()?;

    for doc in &index.documents {
        let file = &doc.relative_path;

        // Build a map of symbol string -> SymbolInformation for this document
        // so we can resolve documentation and kind
        let sym_info_map: std::collections::HashMap<&str, &scip::types::SymbolInformation> = doc
            .symbols
            .iter()
            .map(|si| (si.symbol.as_str(), si))
            .collect();

        for occ in &doc.occurrences {
            let is_def = (occ.symbol_roles & SymbolRole::Definition as i32) != 0;
            if !is_def {
                // We only import definitions here; references are handled as edges below
                continue;
            }

            let scip_sym = &occ.symbol;
            // Skip local and meta symbols (start with "local " or "<")
            if scip_sym.starts_with("local ") || scip_sym.starts_with('<') {
                continue;
            }

            let span = parse_range(&occ.range, file);
            let si = sym_info_map.get(scip_sym.as_str());

            let kind = si
                .map(|s| scip_kind_to_prism(&s.kind.enum_value_or_default()))
                .unwrap_or(SymbolKind::Function);

            let name = scip_sym_to_name(scip_sym);
            let sym_id = format!("scip::{}::{}", file, scip_sym);
            let docstring = si
                .and_then(|s| s.documentation.first())
                .map(|s| s.as_str())
                .unwrap_or("")
                .to_string();

            // Upsert symbol into graph (skip if already exists from tree-sitter)
            let q = format!(
                "MATCH (s:Symbol) WHERE s.id = '{}' RETURN s.id",
                escape(&sym_id)
            );
            let mut result = conn
                .query(&q)
                .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

            if result.next().is_none() {
                // Symbol not yet in graph — insert
                let insert = format!(
                    "CREATE (s:Symbol {{id: '{}', name: '{}', kind: '{}', file: '{}', \
                     start_line: {}, end_line: {}, signature_hash: '{}', language: 'scip', \
                     visibility: 'public', parent: '', docstring: '{}'}})",
                    escape(&sym_id),
                    escape(&name),
                    kind.as_str(),
                    escape(file),
                    span.start_line,
                    span.end_line,
                    escape(scip_sym),
                    escape(&docstring),
                );
                if conn.query(&insert).is_ok() {
                    stats.symbols_added += 1;
                } else {
                    stats.symbols_skipped += 1;
                }
            } else {
                // Symbol exists from tree-sitter — enrich span if SCIP is more precise
                let update = format!(
                    "MATCH (s:Symbol) WHERE s.id = '{}' \
                     SET s.start_line = {}, s.end_line = {}",
                    escape(&sym_id),
                    span.start_line,
                    span.end_line,
                );
                let _ = conn.query(&update);
                stats.symbols_enriched += 1;
            }

            // Process relationships from SymbolInformation
            if let Some(si) = si {
                for rel in &si.relationships {
                    let target_id = format!("scip::{}::{}", file, rel.symbol);
                    let rel_type = if rel.is_implementation {
                        "IMPLEMENTS"
                    } else if rel.is_type_definition {
                        "INHERITS"
                    } else if rel.is_reference {
                        "CALLS"
                    } else {
                        continue;
                    };

                    let create_rel = format!(
                        "MATCH (a:Symbol), (b:Symbol) WHERE a.id = '{}' AND b.id = '{}' \
                         CREATE (a)-[:{}]->(b)",
                        escape(&sym_id),
                        escape(&target_id),
                        rel_type,
                    );
                    if conn.query(&create_rel).is_ok() {
                        stats.relations_added += 1;
                    }
                }
            }
        }

        stats.files_processed += 1;
    }

    // Second pass: add reference edges (Occurrence with no Definition role = call/reference)
    for doc in &index.documents {
        let file = &doc.relative_path;

        for occ in &doc.occurrences {
            let is_def = (occ.symbol_roles & SymbolRole::Definition as i32) != 0;
            if is_def {
                continue;
            }
            let is_ref = occ.symbol_roles == 0
                || (occ.symbol_roles & SymbolRole::UnspecifiedSymbolRole as i32) != 0;
            if !is_ref {
                continue;
            }
            if occ.symbol.starts_with("local ") || occ.symbol.starts_with('<') {
                continue;
            }

            // Find the symbol that contains this reference by line number
            let ref_line = occ.range.first().copied().unwrap_or(0) as u32;
            let target_sym_id = format!("scip::{}::{}", file, occ.symbol);

            // Find containing definition in this file by line proximity
            let find_container = format!(
                "MATCH (s:Symbol) WHERE s.file = '{}' AND s.start_line <= {} AND s.end_line >= {} \
                 RETURN s.id ORDER BY (s.end_line - s.start_line) ASC LIMIT 1",
                escape(file),
                ref_line,
                ref_line,
            );
            if let Ok(mut rows) = conn.query(&find_container) {
                if let Some(row) = rows.next() {
                    if !row.is_empty() {
                        let container_str = row[0].to_string().trim_matches('"').to_string();
                        let create_call = format!(
                            "MATCH (a:Symbol), (b:Symbol) WHERE a.id = '{}' AND b.id = '{}' \
                             CREATE (a)-[:CALLS]->(b)",
                            escape(&container_str),
                            escape(&target_sym_id),
                        );
                        if conn.query(&create_call).is_ok() {
                            stats.references_added += 1;
                        }
                    }
                }
            }
        }
    }

    Ok(stats)
}

fn parse_range(range: &[i32], file: &str) -> Span {
    // SCIP range: [startLine, startChar, endLine, endChar] or [startLine, startChar, endChar]
    let (start_line, start_col, end_line, end_col) = match range.len() {
        4 => (range[0], range[1], range[2], range[3]),
        3 => (range[0], range[1], range[0], range[2]),
        _ => (0, 0, 0, 0),
    };
    Span {
        file: file.to_string(),
        start_line: start_line as u32,
        start_col: start_col as u32,
        end_line: end_line as u32,
        end_col: end_col as u32,
    }
}

fn scip_sym_to_name(scip_sym: &str) -> String {
    // SCIP symbol format: "scheme manager package version descriptor..."
    // Last space-separated descriptor is the human name; take the last segment after `/` or `#` or `.`
    scip_sym
        .rsplit_once('`')
        .map(|(_, n)| n)
        .or_else(|| scip_sym.rsplit(['#', '.', '/']).next())
        .unwrap_or(scip_sym)
        .trim_matches(|c| c == '(' || c == ')' || c == '`')
        .to_string()
}

fn scip_kind_to_prism(kind: &symbol_information::Kind) -> SymbolKind {
    use symbol_information::Kind::*;
    match kind {
        Function | AbstractMethod | StaticMethod | PureVirtualMethod | ProtocolMethod
        | TraitMethod | TypeClassMethod => SymbolKind::Function,
        Method | MethodAlias | MethodReceiver | MethodSpecification => SymbolKind::Method,
        Class | SingletonClass => SymbolKind::Class,
        Struct => SymbolKind::Struct,
        Interface => SymbolKind::Interface,
        Trait | TypeClass => SymbolKind::Trait,
        Enum | EnumMember => SymbolKind::Enum,
        Module | Namespace | Package => SymbolKind::Module,
        Variable | StaticVariable | Field | SelfParameter | Parameter => SymbolKind::Variable,
        Constant => SymbolKind::Constant,
        _ => SymbolKind::Function,
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub files_processed: usize,
    pub symbols_added: usize,
    pub symbols_enriched: usize,
    pub symbols_skipped: usize,
    pub relations_added: usize,
    pub references_added: usize,
}
