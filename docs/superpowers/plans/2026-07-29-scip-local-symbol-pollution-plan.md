# SCIP Local-Symbol Pollution Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop SCIP-enriched projects from polluting the graph with bogus `Symbol` nodes and `CALLS` edges for function parameters (and any similarly-nested SCIP descriptor), verified live against `sittir`'s `get_doc_context` output showing raw unparsed SCIP monikers as callers/callees.

**Architecture:** `crates/infigraph-core/src/scip/mod.rs`'s `import_scip_index` already resolves SCIP definitions against existing tree-sitter symbols by `(file, name)` before ever creating a new graph node — that architecture is sound. The bug is narrowly in the "no match found" fallback: it unconditionally treats any unmatched definition occurrence as a legitimately-new symbol, with no check for whether the occurrence is structurally a *member* (e.g. a parameter) of a symbol the importer already knows about. Fix: one new function, one new call site, gating that single decision point.

**Tech Stack:** Rust, the `scip` crate (SCIP protobuf types), `tree-sitter`-derived `GraphStore`/Kuzu backend (unrelated to this fix — no tree-sitter/`.scm` involvement at all).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-28-scip-local-symbol-pollution-design.md` (read for full background/verification detail — this plan implements its "Corrected fix shape" section).
- The fix does **not** depend on SCIP's `SymbolInformation.kind` — verified against real `scip-typescript 0.4.0` output that it's always `UnspecifiedKind`, for everything, including genuine `local N` symbols.
- The fix does **not** touch tree-sitter/`.scm` query files, AST re-parsing, or `occ.range`-based lookups — it's pure SCIP-moniker string matching against data the importer already collects (`scip_sym_to_file_name`).
- Scope confirmed to exactly one call site: Pass 1's new-symbol decision (`crates/infigraph-core/src/scip/mod.rs`, inside the `else` branch around line 151 in the pre-fix source). Pass 2 (`CALLS` edges) needs no change — once Pass 1 stops creating the bogus node, its lookup naturally finds nothing and skips, same as any other unresolvable reference today. Pass 3 (`INHERITS` edges) needs no change — verified against a real `class X implements Y` fixture that relationship source/target are always clean top-level Type/Method monikers, never nested descriptors.
- `infigraph-core` is a shared/foundational crate (per this repo's root `CLAUDE.md`) — run targeted `scip::` tests first, then `cargo test --all` before considering this done.
- Environment gotcha: this machine's `~/.zshrc` exports `INFIGRAPH_WATCH_DAEMON=1` globally, contaminating watcher-related test runs. Prefix `cargo build`/`cargo test`/`git commit` with `env -u INFIGRAPH_WATCH_DAEMON`.
- Pre-authorized flake for this branch's `--no-verify` justification (only if genuinely hit and confirmed via `git stash` against baseline, not by default): `write_lock_perf::test_contended_lock_throughput`.

---

### Task 1: Suppress SCIP parameter/member descriptors via structural moniker matching

**Files:**
- Modify: `crates/infigraph-core/src/scip/mod.rs` (add `is_member_of_known_symbol`, wire it into Pass 1's loop around line 128-135 of the current source)
- Test: `crates/infigraph-core/src/scip/mod.rs`'s existing `#[cfg(test)] mod tests` block (starts at line 580 in current source)

**Interfaces:**
- Produces: `fn is_member_of_known_symbol(scip_sym: &str, known: &HashMap<String, (String, String)>) -> bool` — a private (module-local, no `pub`) helper. Not consumed by any other task; this plan is a single task.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `crates/infigraph-core/src/scip/mod.rs` (after the existing `enrichment_does_not_overwrite_existing_symbol_span` test, before the closing `}` of the `mod tests` block):

```rust
    #[test]
    fn scip_parameter_descriptor_does_not_become_a_new_symbol() {
        let env = TestEnv::new();
        let file = "test.ts";
        // Real scip-typescript shape, verified against real output: a
        // parameter's moniker is exactly its enclosing method's own moniker
        // (always ending in `.`) with `(paramName)` appended.
        let method_sym = "scip-test npm test 1.0.0 `test.ts`/mintFn().".to_string();
        let param_sym = "scip-test npm test 1.0.0 `test.ts`/mintFn().(rulesBag)".to_string();

        let doc = Document {
            relative_path: file.to_string(),
            occurrences: vec![
                Occurrence {
                    range: vec![0, 16, 22],
                    symbol: method_sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                },
                Occurrence {
                    range: vec![0, 23, 31],
                    symbol: param_sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                },
            ],
            symbols: vec![
                SymbolInformation {
                    symbol: method_sym.clone(),
                    ..Default::default()
                },
                SymbolInformation {
                    symbol: param_sym.clone(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let index = Index {
            documents: vec![doc],
            ..Default::default()
        };
        let bytes = index.write_to_bytes().unwrap();
        let index_path = env._dir.path().join("index.scip");
        std::fs::write(&index_path, bytes).unwrap();

        let stats = import_scip_index(&index_path, &env.store, None).unwrap();
        assert_eq!(
            stats.symbols_added, 1,
            "only the method itself should become a new symbol -- the parameter must be suppressed"
        );

        let conn = env.store.connection().unwrap();
        let rows = conn.query("MATCH (s:Symbol) RETURN s.name").unwrap();
        let names: Vec<String> = rows
            .into_iter()
            .map(|row| row[0].to_string().trim_matches('"').to_string())
            .collect();
        assert_eq!(
            names,
            vec!["mintFn".to_string()],
            "no node should exist for the parameter descriptor, and its name must not \
             leak through as a raw unparsed moniker on any node"
        );
    }

    #[test]
    fn calls_edge_still_attributes_to_enclosing_method_when_a_parameter_is_suppressed() {
        let env = TestEnv::new();
        let conn = env.store.connection().unwrap();

        // Seed two pre-existing tree-sitter symbols with real full-body
        // spans, the normal case: SCIP enriches an already-known function,
        // it doesn't need to add it.
        conn.query(
            "CREATE (:Symbol {id: 'test.ts::mintFn', name: 'mintFn', kind: 'function', \
             file: 'test.ts', start_line: 1, end_line: 10, signature_hash: '', \
             language: 'typescript', visibility: 'public', parent: '', docstring: '', \
             complexity: 0, parameters: '', return_type: ''})",
        )
        .unwrap();
        conn.query(
            "CREATE (:Symbol {id: 'test.ts::helper', name: 'helper', kind: 'function', \
             file: 'test.ts', start_line: 20, end_line: 25, signature_hash: '', \
             language: 'typescript', visibility: 'public', parent: '', docstring: '', \
             complexity: 0, parameters: '', return_type: ''})",
        )
        .unwrap();

        let file = "test.ts";
        let method_sym = "scip-test npm test 1.0.0 `test.ts`/mintFn().".to_string();
        let param_sym = "scip-test npm test 1.0.0 `test.ts`/mintFn().(rulesBag)".to_string();
        let helper_sym = "scip-test npm test 1.0.0 `test.ts`/helper().".to_string();

        let doc = Document {
            relative_path: file.to_string(),
            occurrences: vec![
                // mintFn's own definition occurrence (identifier token only,
                // line 0 == 1-based line 1, matching the seeded start_line).
                Occurrence {
                    range: vec![0, 16, 22],
                    symbol: method_sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                },
                // Its parameter -- must be suppressed, not create a node
                // that could steal container_id for the reference below.
                Occurrence {
                    range: vec![0, 23, 31],
                    symbol: param_sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                },
                // helper's own definition occurrence (1-based line 20).
                Occurrence {
                    range: vec![19, 9, 15],
                    symbol: helper_sym.clone(),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                },
                // A reference to helper() from inside mintFn's body (line 4,
                // 0-based -> 1-based line 5, within mintFn's seeded [1,10]
                // span and nowhere near the suppressed parameter's own
                // narrow single-line range).
                Occurrence {
                    range: vec![4, 2, 8],
                    symbol: helper_sym.clone(),
                    symbol_roles: 0, // reference, not definition
                    ..Default::default()
                },
            ],
            symbols: vec![
                SymbolInformation { symbol: method_sym.clone(), ..Default::default() },
                SymbolInformation { symbol: param_sym.clone(), ..Default::default() },
                SymbolInformation { symbol: helper_sym.clone(), ..Default::default() },
            ],
            ..Default::default()
        };
        let index = Index {
            documents: vec![doc],
            ..Default::default()
        };
        let bytes = index.write_to_bytes().unwrap();
        let index_path = env._dir.path().join("index.scip");
        std::fs::write(&index_path, bytes).unwrap();

        let stats = import_scip_index(&index_path, &env.store, None).unwrap();
        assert_eq!(
            stats.symbols_added, 0,
            "both mintFn and helper already exist -- only enrichment, and the \
             parameter must be suppressed, not added"
        );

        let rows = conn
            .query("MATCH (a:Symbol)-[:CALLS]->(b:Symbol) RETURN a.name, b.name")
            .unwrap();
        let pairs: Vec<(String, String)> = rows
            .into_iter()
            .map(|row| {
                (
                    row[0].to_string().trim_matches('"').to_string(),
                    row[1].to_string().trim_matches('"').to_string(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![("mintFn".to_string(), "helper".to_string())],
            "the CALLS edge must attribute to the real enclosing method, not be \
             dropped or misattributed to a suppressed parameter pseudo-symbol"
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib scip:: -- --test-threads=1`

Expected: `scip_parameter_descriptor_does_not_become_a_new_symbol` FAILS on the `stats.symbols_added` assertion (currently `2`, not `1` — today's code creates a bogus node for the parameter). `calls_edge_still_attributes_to_enclosing_method_when_a_parameter_is_suppressed` FAILS on `stats.symbols_added` (currently `1`, not `0`) — it may or may not also fail the `CALLS` assertion depending on whether the bogus parameter node's narrow single-line span happens to also contain the reference at line 4; either way, at least one assertion in each test must fail before the fix.

- [ ] **Step 3: Implement the fix**

In `crates/infigraph-core/src/scip/mod.rs`, add this function directly above `import_scip_index` (before line 21 in the current source, i.e. right after the module's `use` statements):

```rust
/// True when `scip_sym` is a member (e.g. a parameter) of a symbol we
/// already know about, per SCIP's own descriptor grammar: strip a single
/// trailing `(...)` group and check whether what remains is a moniker
/// already present in `known` (built from every other non-local
/// definition occurrence in this same import pass, before this check
/// ever runs -- see `scip_sym_to_file_name`, populated before Pass 1).
///
/// Verified against real `scip-typescript` output: a parameter's moniker
/// is exactly its enclosing method's own moniker (always ending in `.`)
/// with `(paramName)` appended -- nothing else -- so this is an exact
/// string match against already-known data, not a re-derived heuristic.
/// Deliberately does NOT use `SymbolInformation.kind`: verified always
/// `UnspecifiedKind` in real output, so it carries no usable signal.
///
/// A normal top-level definition's own moniker always ends in `.` (Term),
/// `#` (Type), or `().` (Method) per SCIP's descriptor grammar -- never a
/// bare `)` -- so this never fires for a legitimately-new symbol; it can
/// only match nested member descriptors.
fn is_member_of_known_symbol(scip_sym: &str, known: &HashMap<String, (String, String)>) -> bool {
    let Some(without_group) = scip_sym.strip_suffix(')') else {
        return false;
    };
    let Some(open) = without_group.rfind('(') else {
        return false;
    };
    known.contains_key(&without_group[..open])
}
```

Then modify the Pass 1 loop. Find this block (current source, inside the `for doc in &index.documents` loop that builds `enrichments`/`new_symbols`):

```rust
            let scip_sym = &occ.symbol;
            if scip_sym.starts_with("local ") || scip_sym.starts_with('<') {
                continue;
            }

            let name = scip_sym_to_name(scip_sym);
```

Replace with:

```rust
            let scip_sym = &occ.symbol;
            if scip_sym.starts_with("local ") || scip_sym.starts_with('<') {
                continue;
            }
            if is_member_of_known_symbol(scip_sym, &scip_sym_to_file_name) {
                continue;
            }

            let name = scip_sym_to_name(scip_sym);
```

Note: `scip_sym_to_file_name` is already fully populated by this point — it's built in the separate loop immediately before Pass 1 (current source lines 91-105), which runs over every document before Pass 1's own loop begins.

- [ ] **Step 4: Run tests to verify they pass**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test -p infigraph-core --lib scip:: -- --test-threads=1`

Expected: all tests in `scip::tests` pass, including the two new ones and every pre-existing test in that module (`scip_sym_to_name_strips_trailing_suffix_markers`, `scip_sym_to_name_handles_backtick_quoted_descriptors`, `is_implementation_relationship_creates_inherits_edge`, `non_implementation_relationship_does_not_create_inherits_edge`, `enrichment_does_not_overwrite_existing_symbol_span`).

- [ ] **Step 5: Run the full workspace test suite**

Run: `env -u INFIGRAPH_WATCH_DAEMON cargo test --all 2>&1 | tail -100`

Expected: green, modulo any already-known pre-existing flakes in this branch's history (`watcher_concurrency::test_graph_tools_with_group_watchers` is the one most recently confirmed pre-existing and unrelated). If anything else fails, investigate — this task touches a shared/foundational crate (`infigraph-core`), so don't assume a new failure is unrelated without checking.

- [ ] **Step 6: fmt + clippy**

Run:
```bash
cargo fmt --all
env -u INFIGRAPH_WATCH_DAEMON cargo clippy -p infigraph-core --all-targets -- -D warnings
```
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/infigraph-core/src/scip/mod.rs
env -u INFIGRAPH_WATCH_DAEMON git commit -m "$(cat <<'EOF'
fix: suppress SCIP parameter/member descriptors from becoming graph symbols

get_doc_context on SCIP-enriched (scip-typescript) TypeScript projects
showed raw, unparsed SCIP monikers as caller/callee entries -- one per
function parameter. Root cause: scip-typescript encodes parameters as
a full global-style moniker (<method>().(paramName)) rather than a
SCIP-spec `local <id>` symbol, so the existing local-symbol filter
missed them, and every parameter became its own bogus Symbol node
(each with a tiny declaration-line span that could also steal
container_id from real references via the smallest-span-first
containment lookup).

Fixed by recognizing this structurally rather than by SCIP kind
(verified always UnspecifiedKind in real output, unusable) or by
re-parsing with tree-sitter (would need per-language node-kind tables
across all 10 configured SCIP indexers): a parameter's moniker is
exactly its enclosing method's own already-known moniker with
`(paramName)` appended, so stripping the trailing group and checking
against scip_sym_to_file_name (already built before this decision
runs) exactly identifies membership without any new data or heuristics.
EOF
)"
```

---

## Self-Review Notes

- **Spec coverage**: the spec's "Corrected fix shape" section (via the earlier "Design principle" section) specifies exactly one code change — `is_member_of_known_symbol` gating Pass 1's new-symbol decision. This plan implements exactly that, nothing more. The spec's "complementary, secondary fix" (hardening `scip_sym_to_name` via source-text extraction) is explicitly NOT included here — it's optional hardening for a wider surface (name resolution for all symbols, not just this bug), not required to fix the reported issue, and changing it carries broader blast radius than this narrow, verified fix. Not silently dropped: noted here as a deliberate scope decision, consistent with the spec's own framing of it as complementary rather than required.
- **Placeholder scan**: no TBD/TODO, all code blocks complete and copy-pasteable, exact commands with expected outcomes given.
- **Type consistency**: `is_member_of_known_symbol(scip_sym: &str, known: &HashMap<String, (String, String)>) -> bool` matches the existing `scip_sym_to_file_name: HashMap<String, (String, String)>` type exactly (as declared in the current source, line ~92) — no signature mismatch.
- **Single task, no interfaces to hand off**: this plan is one self-contained task; there is no Task 2 depending on Task 1's produced function.
