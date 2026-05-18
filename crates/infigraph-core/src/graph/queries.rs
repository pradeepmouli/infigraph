use anyhow::Result;
use kuzu::Connection;

/// High-level graph query interface for analysis.
pub struct GraphQuery<'a, 'db> {
    conn: &'a Connection<'db>,
}

impl<'a, 'db> GraphQuery<'a, 'db> {
    pub fn new(conn: &'a Connection<'db>) -> Self {
        Self { conn }
    }

    /// Find all symbols in a file.
    pub fn symbols_in_file(&self, file: &str) -> Result<Vec<SymbolRow>> {
        let query = format!(
            "MATCH (s:Symbol) WHERE s.file = '{}' RETURN s.id, s.name, s.kind, s.start_line, s.end_line ORDER BY s.start_line",
            file.replace('\'', "\\'")
        );
        let mut result = self.conn.query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 5 {
                rows.push(SymbolRow {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    start_line: row[3].to_string().parse().unwrap_or(0),
                    end_line: row[4].to_string().parse().unwrap_or(0),
                });
            }
        }
        Ok(rows)
    }

    /// Find direct callers of a symbol.
    pub fn callers_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        let query = format!(
            "MATCH (caller:Symbol)-[:CALLS]->(target:Symbol) WHERE target.id = '{}' RETURN caller.id",
            symbol_id.replace('\'', "\\'")
        );
        self.collect_strings(&query)
    }

    /// Find what a symbol calls.
    pub fn callees_of(&self, symbol_id: &str) -> Result<Vec<String>> {
        let query = format!(
            "MATCH (source:Symbol)-[:CALLS]->(callee:Symbol) WHERE source.id = '{}' RETURN callee.id",
            symbol_id.replace('\'', "\\'")
        );
        self.collect_strings(&query)
    }

    /// Transitive impact: all symbols affected by a change to the given symbol.
    /// Follows CALLS edges in reverse (who calls this, who calls those, etc.).
    pub fn transitive_impact(&self, symbol_id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        let query = format!(
            "MATCH (changed:Symbol)<-[:CALLS* 1..{}]-(affected:Symbol) WHERE changed.id = '{}' RETURN DISTINCT affected.id, affected.name, affected.file, affected.kind",
            max_depth,
            symbol_id.replace('\'', "\\'")
        );
        let mut result = self.conn.query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 4 {
                rows.push(ImpactRow {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    file: row[2].to_string(),
                    kind: row[3].to_string(),
                });
            }
        }
        Ok(rows)
    }

    /// Find symbols in a file whose line range overlaps [start, end].
    pub fn symbols_in_range(&self, file: &str, start: u32, end: u32) -> Result<Vec<SymbolDetail>> {
        let query = format!(
            "MATCH (s:Symbol) WHERE s.file = '{}' AND s.start_line <= {} AND s.end_line >= {} RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line ORDER BY s.start_line",
            file.replace('\'', "\\'"),
            end,
            start
        );
        let mut result = self.conn.query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 6 {
                rows.push(SymbolDetail {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                    start_line: row[4].to_string().parse().unwrap_or(0),
                    end_line: row[5].to_string().parse().unwrap_or(0),
                });
            }
        }
        Ok(rows)
    }

    /// Look up a symbol by its ID and return its file, start_line, end_line.
    pub fn find_symbol_by_id(&self, symbol_id: &str) -> Result<Option<SymbolDetail>> {
        let query = format!(
            "MATCH (s:Symbol) WHERE s.id = '{}' RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line",
            symbol_id.replace('\'', "\\'")
        );
        let mut result = self.conn.query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        if let Some(row) = result.next() {
            if row.len() >= 6 {
                return Ok(Some(SymbolDetail {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                    start_line: row[4].to_string().parse().unwrap_or(0),
                    end_line: row[5].to_string().parse().unwrap_or(0),
                }));
            }
        }
        Ok(None)
    }

    /// Find all reference locations for a symbol — file, line, column, and calling symbol.
    /// Returns every place the symbol is called/used, for rename/refactor workflows.
    pub fn find_all_references(&self, symbol_id: &str) -> Result<Vec<ReferenceRow>> {
        let q = format!(
            "MATCH (caller:Symbol)-[:CALLS]->(target:Symbol) \
             WHERE target.id = '{}' \
             RETURN caller.id, caller.name, caller.file, caller.start_line, target.id",
            symbol_id.replace('\'', "\\'")
        );
        let mut result = self.conn.query(&q)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 5 {
                rows.push(ReferenceRow {
                    caller_id: row[0].to_string(),
                    caller_name: row[1].to_string(),
                    file: row[2].to_string(),
                    line: row[3].to_string().parse().unwrap_or(0),
                    target_id: row[4].to_string(),
                });
            }
        }
        Ok(rows)
    }

    /// Get the public API surface: all public symbols + all routes.
    pub fn get_api_surface(&self) -> Result<Vec<ApiSymbol>> {
        let q = "MATCH (s:Symbol) \
                 WHERE s.visibility = 'public' OR s.kind = 'Route' \
                 RETURN s.id, s.name, s.kind, s.file, s.start_line, s.visibility, s.docstring \
                 ORDER BY s.file, s.start_line";
        let mut result = self.conn.query(q)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 7 {
                rows.push(ApiSymbol {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                    line: row[4].to_string().parse().unwrap_or(0),
                    visibility: row[5].to_string(),
                    docstring: row[6].to_string(),
                });
            }
        }
        Ok(rows)
    }

    /// Get file-level dependency graph: what this file imports and what imports it.
    pub fn get_file_deps(&self, file: &str) -> Result<FileDeps> {
        let esc = file.replace('\'', "\\'");

        // Files this file imports (outgoing)
        let q_out = format!(
            "MATCH (m:Module)-[:IMPORTS]->(dep:Module) WHERE m.file = '{}' RETURN dep.file",
            esc
        );
        let mut r = self.conn.query(&q_out)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut imports = Vec::new();
        while let Some(row) = r.next() {
            if let Some(v) = row.first() {
                let s = v.to_string().trim_matches('"').to_string();
                if !s.is_empty() { imports.push(s); }
            }
        }

        // Files that import this file (incoming)
        let q_in = format!(
            "MATCH (m:Module)-[:IMPORTS]->(dep:Module) WHERE dep.file = '{}' RETURN m.file",
            esc
        );
        let mut r2 = self.conn.query(&q_in)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut imported_by = Vec::new();
        while let Some(row) = r2.next() {
            if let Some(v) = row.first() {
                let s = v.to_string().trim_matches('"').to_string();
                if !s.is_empty() { imported_by.push(s); }
            }
        }

        Ok(FileDeps { file: file.to_string(), imports, imported_by })
    }

    /// Get full type hierarchy for a class/interface: ancestors (up) and descendants (down).
    pub fn get_type_hierarchy(&self, symbol_id: &str, max_depth: u32) -> Result<TypeHierarchy> {
        let esc = symbol_id.replace('\'', "\\'");

        // Ancestors: walk INHERITS edges upward
        let q_up = format!(
            "MATCH (root:Symbol)-[:INHERITS* 1..{}]->(ancestor:Symbol) \
             WHERE root.id = '{}' \
             RETURN ancestor.id, ancestor.name, ancestor.kind, ancestor.file",
            max_depth, esc
        );
        let mut r = self.conn.query(&q_up)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut ancestors = Vec::new();
        while let Some(row) = r.next() {
            if row.len() >= 4 {
                ancestors.push(HierarchyNode {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                });
            }
        }

        // Descendants: walk INHERITS edges downward
        let q_down = format!(
            "MATCH (descendant:Symbol)-[:INHERITS* 1..{}]->(root:Symbol) \
             WHERE root.id = '{}' \
             RETURN descendant.id, descendant.name, descendant.kind, descendant.file",
            max_depth, esc
        );
        let mut r2 = self.conn.query(&q_down)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut descendants = Vec::new();
        while let Some(row) = r2.next() {
            if row.len() >= 4 {
                descendants.push(HierarchyNode {
                    id: row[0].to_string(),
                    name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                });
            }
        }

        // Also get root symbol info
        let root_detail = self.find_symbol_by_id(symbol_id)?;

        Ok(TypeHierarchy {
            root_id: symbol_id.to_string(),
            root_name: root_detail.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            ancestors,
            descendants,
        })
    }

    /// Get test coverage: which symbols have TESTED_BY edges, which don't.
    pub fn get_test_coverage(&self) -> Result<TestCoverage> {
        // Testable kinds
        let q_covered = "MATCH (s:Symbol)-[:TESTED_BY]->(t:Symbol) \
                         WHERE s.kind IN ['Function','Method','Class','Struct','Trait','Interface'] \
                         RETURN DISTINCT s.id, s.name, s.kind, s.file, t.id";
        let mut r = self.conn.query(q_covered)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut covered = Vec::new();
        while let Some(row) = r.next() {
            if row.len() >= 5 {
                covered.push(CoverageRow {
                    symbol_id: row[0].to_string(),
                    symbol_name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                    test_id: Some(row[4].to_string()),
                });
            }
        }

        let q_uncovered = "MATCH (s:Symbol) \
                           WHERE s.kind IN ['Function','Method','Class','Struct','Trait','Interface'] \
                           AND NOT EXISTS { MATCH (s)-[:TESTED_BY]->(:Symbol) } \
                           RETURN s.id, s.name, s.kind, s.file \
                           ORDER BY s.file, s.start_line";
        let mut r2 = self.conn.query(q_uncovered)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut uncovered = Vec::new();
        while let Some(row) = r2.next() {
            if row.len() >= 4 {
                uncovered.push(CoverageRow {
                    symbol_id: row[0].to_string(),
                    symbol_name: row[1].to_string(),
                    kind: row[2].to_string(),
                    file: row[3].to_string(),
                    test_id: None,
                });
            }
        }

        let total = covered.len() + uncovered.len();
        let pct = if total > 0 { covered.len() * 100 / total } else { 0 };

        Ok(TestCoverage {
            covered_count: covered.len(),
            uncovered_count: uncovered.len(),
            coverage_pct: pct,
            covered,
            uncovered,
        })
    }

    /// Run a raw Cypher query and return string results.
    pub fn raw_query(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let mut result = self.conn.query(cypher)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        while let Some(row) = result.next() {
            let string_row: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            rows.push(string_row);
        }
        Ok(rows)
    }

    fn collect_strings(&self, query: &str) -> Result<Vec<String>> {
        let mut result = self.conn.query(query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut out = Vec::new();
        while let Some(row) = result.next() {
            if let Some(val) = row.first() {
                out.push(val.to_string());
            }
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct SymbolRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Extended symbol info including file path (for snippet retrieval).
#[derive(Debug)]
pub struct SymbolDetail {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug)]
pub struct ImpactRow {
    pub id: String,
    pub name: String,
    pub file: String,
    pub kind: String,
}

#[derive(Debug)]
pub struct ReferenceRow {
    pub caller_id: String,
    pub caller_name: String,
    pub file: String,
    pub line: u32,
    pub target_id: String,
}

#[derive(Debug)]
pub struct ApiSymbol {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub visibility: String,
    pub docstring: String,
}

#[derive(Debug)]
pub struct FileDeps {
    pub file: String,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

#[derive(Debug)]
pub struct HierarchyNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
}

#[derive(Debug)]
pub struct TypeHierarchy {
    pub root_id: String,
    pub root_name: String,
    pub ancestors: Vec<HierarchyNode>,
    pub descendants: Vec<HierarchyNode>,
}

#[derive(Debug)]
pub struct CoverageRow {
    pub symbol_id: String,
    pub symbol_name: String,
    pub kind: String,
    pub file: String,
    pub test_id: Option<String>,
}

#[derive(Debug)]
pub struct TestCoverage {
    pub covered_count: usize,
    pub uncovered_count: usize,
    pub coverage_pct: usize,
    pub covered: Vec<CoverageRow>,
    pub uncovered: Vec<CoverageRow>,
}
