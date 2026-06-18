use anyhow::Result;
use kuzu::Connection;
use serde::Serialize;

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
        let result = self
            .conn
            .query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        for row in result {
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

    pub fn branches_of(&self, symbol_id: &str) -> Result<Vec<BranchInfo>> {
        let query = format!(
            "MATCH (s:Symbol)-[:HAS_STATEMENT]->(st:Statement) WHERE s.id = '{}' \
             RETURN st.kind, st.condition, st.start_line, st.depth ORDER BY st.start_line",
            symbol_id.replace('\'', "\\'")
        );
        let mut result = self.conn.query(&query)
            .map_err(|e| anyhow::anyhow!("branches_of failed: {e}"))?;
        let mut branches = Vec::new();
        while let Some(row) = result.next() {
            if row.len() >= 4 {
                branches.push(BranchInfo {
                    kind: row[0].to_string(),
                    condition: row[1].to_string(),
                    line: row[2].to_string().parse().unwrap_or(0),
                    depth: row[3].to_string().parse().unwrap_or(0),
                });
            }
        }
        Ok(branches)
    }

    /// Transitive impact: all symbols affected by a change to the given symbol.
    /// Follows CALLS edges in reverse (who calls this, who calls those, etc.).
    pub fn transitive_impact(&self, symbol_id: &str, max_depth: u32) -> Result<Vec<ImpactRow>> {
        let query = format!(
            "MATCH (changed:Symbol)<-[:CALLS* 1..{}]-(affected:Symbol) WHERE changed.id = '{}' RETURN DISTINCT affected.id, affected.name, affected.file, affected.kind",
            max_depth,
            symbol_id.replace('\'', "\\'")
        );
        let result = self
            .conn
            .query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        for row in result {
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
        let result = self
            .conn
            .query(&query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        for row in result {
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
        let mut result = self
            .conn
            .query(&query)
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
        let result = self
            .conn
            .query(&q)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut rows = Vec::new();
        for row in result {
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
        let result = self
            .conn
            .query(q)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut rows = Vec::new();
        for row in result {
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
        let r = self
            .conn
            .query(&q_out)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut imports = Vec::new();
        for row in r {
            if let Some(v) = row.first() {
                let s = v.to_string().trim_matches('"').to_string();
                if !s.is_empty() {
                    imports.push(s);
                }
            }
        }

        // Files that import this file (incoming)
        let q_in = format!(
            "MATCH (m:Module)-[:IMPORTS]->(dep:Module) WHERE dep.file = '{}' RETURN m.file",
            esc
        );
        let r2 = self
            .conn
            .query(&q_in)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut imported_by = Vec::new();
        for row in r2 {
            if let Some(v) = row.first() {
                let s = v.to_string().trim_matches('"').to_string();
                if !s.is_empty() {
                    imported_by.push(s);
                }
            }
        }

        Ok(FileDeps {
            file: file.to_string(),
            imports,
            imported_by,
        })
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
        let r = self
            .conn
            .query(&q_up)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut ancestors = Vec::new();
        for row in r {
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
        let r2 = self
            .conn
            .query(&q_down)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut descendants = Vec::new();
        for row in r2 {
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
            root_name: root_detail
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_default(),
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
        let r = self
            .conn
            .query(q_covered)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut covered = Vec::new();
        for row in r {
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
        let r2 = self
            .conn
            .query(q_uncovered)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut uncovered = Vec::new();
        for row in r2 {
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
        let pct = if total > 0 {
            (covered.len() * 100) / total
        } else {
            0
        };

        Ok(TestCoverage {
            covered_count: covered.len(),
            uncovered_count: uncovered.len(),
            coverage_pct: pct,
            covered,
            uncovered,
        })
    }

    pub fn cross_cutting_for(&self, symbol_id: &str) -> Result<Vec<(String, String)>> {
        let esc = crate::escape_str(symbol_id);
        let result = self.conn.query(&format!(
            "MATCH (s:Symbol)-[:HAS_CONCERN]->(c:Concern) WHERE s.id = '{}' RETURN c.kind, c.detail",
            esc
        )).map_err(|e| anyhow::anyhow!("cross_cutting query failed: {e}"))?;

        let mut out = Vec::new();
        for row in result {
            if row.len() >= 2 {
                out.push((row[0].to_string(), row[1].to_string()));
            }
        }
        Ok(out)
    }

    /// Run a raw Cypher query and return string results.
    pub fn raw_query(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let result = self
            .conn
            .query(cypher)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;

        let mut rows = Vec::new();
        for row in result {
            let string_row: Vec<String> = row.iter().map(|v| v.to_string()).collect();
            rows.push(string_row);
        }
        Ok(rows)
    }

    /// Derive TESTED_BY edges from CALLS: if a Test symbol calls a non-test symbol,
    /// create (called)-[:TESTED_BY]->(test). Returns number of edges created.
    pub fn derive_tested_by_edges(&self) -> Result<usize> {
        let _ = self.conn.query("MATCH (s:Symbol)-[r:TESTED_BY]->(t:Symbol) DELETE r");
        self.conn.query(
            "MATCH (t:Symbol)-[:CALLS]->(s:Symbol) \
             WHERE t.kind = 'Test' AND s.kind <> 'Test' \
             CREATE (s)-[:TESTED_BY]->(t)"
        ).map_err(|e| anyhow::anyhow!("derive TESTED_BY failed: {e}"))?;
        let mut r = self.conn.query("MATCH ()-[r:TESTED_BY]->() RETURN count(r)")
            .map_err(|e| anyhow::anyhow!("count TESTED_BY failed: {e}"))?;
        let count = r.next()
            .and_then(|row| row.first().map(|v| v.to_string()))
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        Ok(count)
    }

    pub fn generate_test_context(&self, file_filter: Option<&str>, limit: usize) -> Result<TestContext> {
        let framework = self.detect_test_framework()?;
        let example_test = self.find_example_test(file_filter)?;

        let q = String::from(
            "MATCH (s:Symbol) \
             WHERE s.kind IN ['Function','Method','Class','Struct','Trait','Interface'] \
             AND NOT EXISTS { MATCH (s)-[:TESTED_BY]->(:Symbol) } \
             RETURN s.id, s.name, s.kind, s.file, s.start_line, s.end_line, \
                    s.visibility, s.parameters, s.return_type, s.complexity \
             ORDER BY s.complexity DESC, s.file, s.start_line"
        );
        let mut result = self.conn.query(&q)
            .map_err(|e| anyhow::anyhow!("generate_test_context query failed: {e}"))?;

        let mut targets = Vec::new();
        while let Some(row) = result.next() {
            if row.len() < 10 { continue; }
            let file = row[3].to_string();
            if let Some(f) = file_filter {
                if !file.contains(f) { continue; }
            }
            let visibility = row[6].to_string();
            let complexity: u32 = row[9].to_string().parse().unwrap_or(1);
            let vis_score: u32 = if visibility == "public" || visibility == "pub" { 10 } else { 0 };
            let priority_score = complexity * 5 + vis_score;

            targets.push(TestTarget {
                symbol_id: row[0].to_string(),
                name: row[1].to_string(),
                kind: row[2].to_string(),
                file,
                start_line: row[4].to_string().parse().unwrap_or(0),
                end_line: row[5].to_string().parse().unwrap_or(0),
                visibility,
                parameters: row[7].to_string(),
                return_type: row[8].to_string(),
                complexity,
                callers: Vec::new(),
                callees: Vec::new(),
                branches: Vec::new(),
                priority_score,
            });
        }

        targets.sort_by(|a, b| b.priority_score.cmp(&a.priority_score));
        targets.truncate(limit);

        for t in &mut targets {
            t.callers = self.callers_of(&t.symbol_id).unwrap_or_default();
            t.callees = self.callees_of(&t.symbol_id).unwrap_or_default();
            t.branches = self.branches_of(&t.symbol_id).unwrap_or_default();
            t.priority_score += t.callers.len() as u32 * 3;
        }

        targets.sort_by(|a, b| b.priority_score.cmp(&a.priority_score));

        Ok(TestContext { framework, example_test, targets })
    }

    fn detect_test_framework(&self) -> Result<String> {
        let q = "MATCH (s:Symbol) WHERE s.kind = 'Test' RETURN s.docstring LIMIT 20";
        let mut result = self.conn.query(q)
            .map_err(|e| anyhow::anyhow!("detect_test_framework failed: {e}"))?;

        let mut frameworks = std::collections::HashMap::new();
        while let Some(row) = result.next() {
            let doc = row.first().map(|v| v.to_string()).unwrap_or_default();
            if doc.contains("#[test]") || doc.contains("#[tokio::test]") || doc.contains("#[rstest]") {
                *frameworks.entry("rust (cargo test)").or_insert(0u32) += 1;
            }
            if doc.contains("@Test") || doc.contains("@ParameterizedTest") {
                *frameworks.entry("java (junit)").or_insert(0) += 1;
            }
            if doc.contains("[Test]") || doc.contains("[Fact]") || doc.contains("[Theory]") {
                *frameworks.entry("csharp (nunit/xunit)").or_insert(0) += 1;
            }
            if doc.contains("[TestMethod]") {
                *frameworks.entry("csharp (mstest)").or_insert(0) += 1;
            }
            if doc.contains("@pytest") || doc.contains("@unittest") {
                *frameworks.entry("python (pytest)").or_insert(0) += 1;
            }
        }

        if let Some((fw, _)) = frameworks.into_iter().max_by_key(|(_, count)| *count) {
            return Ok(fw.to_string());
        }

        let q2 = "MATCH (d:Dependency) WHERE d.is_dev = true RETURN d.name LIMIT 100";
        if let Ok(mut r2) = self.conn.query(q2) {
            while let Some(row) = r2.next() {
                let dep = row.first().map(|v| v.to_string()).unwrap_or_default();
                match dep.as_str() {
                    "jest" | "vitest" | "mocha" | "ava" | "tap" | "cypress" =>
                        return Ok(format!("javascript ({})", dep)),
                    "pytest" => return Ok("python (pytest)".to_string()),
                    "rspec" | "rspec-core" => return Ok("ruby (rspec)".to_string()),
                    "minitest" => return Ok("ruby (minitest)".to_string()),
                    "phpunit/phpunit" => return Ok("php (phpunit)".to_string()),
                    "flutter_test" => return Ok("dart (flutter_test)".to_string()),
                    "busted" => return Ok("lua (busted)".to_string()),
                    "pfunit" => return Ok("fortran (pfunit)".to_string()),
                    "hspec" | "tasty" | "HUnit" => return Ok(format!("haskell ({})", dep)),
                    "Test::More" | "Test2" => return Ok(format!("perl ({})", dep)),
                    _ => {
                        if dep.contains("kotlin-test") || dep.contains("kotest") {
                            return Ok(format!("kotlin ({})", dep));
                        }
                        if dep.contains("scalatest") || dep.contains("specs2") || dep.contains("munit") {
                            return Ok(format!("scala ({})", dep));
                        }
                    }
                }
            }
        }

        let q3 = "MATCH (s:Symbol) WHERE s.kind = 'Test' \
                   RETURN s.language, count(s) ORDER BY count(s) DESC LIMIT 1";
        if let Ok(mut r3) = self.conn.query(q3) {
            if let Some(row) = r3.next() {
                let lang = row.first().map(|v| v.to_string()).unwrap_or_default();
                let fw = match lang.as_str() {
                    "go" => "go (go test)",
                    "elixir" => "elixir (ExUnit)",
                    "swift" => "swift (XCTest)",
                    "erlang" => "erlang (EUnit/CT)",
                    "zig" => "zig (builtin test)",
                    "dart" => "dart (test)",
                    "julia" => "julia (Test)",
                    "rust" => "rust (cargo test)",
                    "python" => "python (unittest/pytest)",
                    "ruby" => "ruby (minitest/rspec)",
                    "lua" => "lua (busted)",
                    "r" => "r (testthat)",
                    "haskell" => "haskell (hspec/tasty)",
                    "ocaml" => "ocaml (alcotest/ounit)",
                    "fortran" => "fortran (pfunit)",
                    "powershell" => "powershell (pester)",
                    "bash" => "bash (bats)",
                    _ if !lang.is_empty() => return Ok(format!("{} (detected)", lang)),
                    _ => "unknown",
                };
                if fw != "unknown" {
                    return Ok(fw.to_string());
                }
            }
        }

        Ok("unknown".to_string())
    }

    fn find_example_test(&self, file_filter: Option<&str>) -> Result<Option<ExampleTest>> {
        let q = if let Some(f) = file_filter {
            format!(
                "MATCH (s:Symbol) WHERE s.kind = 'Test' AND s.file CONTAINS '{}' \
                 RETURN s.id, s.name, s.file, s.start_line, s.end_line LIMIT 1",
                f.replace('\'', "\\'")
            )
        } else {
            "MATCH (s:Symbol) WHERE s.kind = 'Test' \
             RETURN s.id, s.name, s.file, s.start_line, s.end_line LIMIT 1".to_string()
        };

        let mut result = self.conn.query(&q)
            .map_err(|e| anyhow::anyhow!("find_example_test failed: {e}"))?;

        if let Some(row) = result.next() {
            if row.len() >= 5 {
                return Ok(Some(ExampleTest {
                    symbol_id: row[0].to_string(),
                    name: row[1].to_string(),
                    file: row[2].to_string(),
                    start_line: row[3].to_string().parse().unwrap_or(0),
                    end_line: row[4].to_string().parse().unwrap_or(0),
                }));
            }
        }
        Ok(None)
    }

    fn collect_strings(&self, query: &str) -> Result<Vec<String>> {
        let result = self
            .conn
            .query(query)
            .map_err(|e| anyhow::anyhow!("query failed: {e}"))?;
        let mut out = Vec::new();
        for row in result {
            if let Some(val) = row.first() {
                out.push(val.to_string());
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Serialize)]
pub struct SymbolRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub start_line: u32,
    pub end_line: u32,
}

/// Extended symbol info including file path (for snippet retrieval).
#[derive(Debug, Serialize)]
pub struct SymbolDetail {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Serialize)]
pub struct ImpactRow {
    pub id: String,
    pub name: String,
    pub file: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct ReferenceRow {
    pub caller_id: String,
    pub caller_name: String,
    pub file: String,
    pub line: u32,
    pub target_id: String,
}

#[derive(Debug, Serialize)]
pub struct ApiSymbol {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub visibility: String,
    pub docstring: String,
}

#[derive(Debug, Serialize)]
pub struct FileDeps {
    pub file: String,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct HierarchyNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
}

#[derive(Debug, Serialize)]
pub struct TypeHierarchy {
    pub root_id: String,
    pub root_name: String,
    pub ancestors: Vec<HierarchyNode>,
    pub descendants: Vec<HierarchyNode>,
}

#[derive(Debug, Serialize)]
pub struct CoverageRow {
    pub symbol_id: String,
    pub symbol_name: String,
    pub kind: String,
    pub file: String,
    pub test_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestCoverage {
    pub covered_count: usize,
    pub uncovered_count: usize,
    pub coverage_pct: usize,
    pub covered: Vec<CoverageRow>,
    pub uncovered: Vec<CoverageRow>,
}

#[derive(Debug, Serialize)]
pub struct BranchInfo {
    pub kind: String,
    pub condition: String,
    pub line: u32,
    pub depth: u32,
}

#[derive(Debug, Serialize)]
pub struct TestTarget {
    pub symbol_id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub visibility: String,
    pub parameters: String,
    pub return_type: String,
    pub complexity: u32,
    pub callers: Vec<String>,
    pub callees: Vec<String>,
    pub branches: Vec<BranchInfo>,
    pub priority_score: u32,
}

#[derive(Debug, Serialize)]
pub struct TestContext {
    pub framework: String,
    pub example_test: Option<ExampleTest>,
    pub targets: Vec<TestTarget>,
}

#[derive(Debug, Serialize)]
pub struct ExampleTest {
    pub symbol_id: String,
    pub name: String,
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
}
