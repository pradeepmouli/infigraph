//! `infigraph verify` (R3.4.1, docs/DESIGN-hardening.md §3.4,
//! github.com/pradeepmouli/infigraph#10): offline, read-only consistency
//! check of one project's index -- does the graph open cleanly, do its
//! internal references hold, do the sidecars parse and match the graph's
//! generation. Reuses `doctor`'s `CheckResult` vocabulary so `doctor`,
//! `verify`, and CI all speak one PASS/WARN/FAIL language; unlike
//! `doctor` (cheap, fleet-wide, mtime-level), `verify` opens the real
//! database and reads real data, so it's deliberately per-project and
//! explicitly invoked.

use crate::doctor::CheckResult;
use crate::graph::GraphBackend;
use std::path::Path;

const CATEGORY: &str = "verify";

/// Run every verification against the project at `root`. Read-only: never
/// mutates, quarantines, or rebuilds -- it reports, the operator (or
/// `doctor`'s remediation hints) decides.
pub fn run_verify(root: &Path) -> Vec<CheckResult> {
    let ig = root.join(".infigraph");
    let graph_path = ig.join("graph");
    let mut results = Vec::new();

    if !graph_path.exists() {
        results.push(CheckResult::warn(
            CATEGORY,
            "graph: open",
            "no graph database present",
            "run `infigraph index` to build one",
        ));
        return results;
    }

    // 1. The graph opens cleanly read-only. This inherits every open-time
    //    preflight for free: the truncation check, and the unreplayed-WAL/
    //    dead-holder guard (R3.1.3) whose refusal shows up here as FAIL
    //    with the guard's own actionable message.
    let backend = match crate::graph::KuzuBackend::open_read_only(&graph_path) {
        Ok(b) => {
            results.push(CheckResult::pass(CATEGORY, "graph: open", "opens cleanly"));
            b
        }
        Err(e) => {
            results.push(CheckResult::fail(
                CATEGORY,
                "graph: open",
                format!("{e:#}"),
                "run `infigraph index --full` to rebuild (a snapshot is taken first)",
            ));
            return results; // nothing below is answerable without the graph
        }
    };

    results.push(check_symbol_file_references(&backend));
    results.extend(check_embeddings_sidecar(&backend, &ig));
    results
}

/// Symbols whose `file` names a File node that doesn't exist. Kuzu's rel
/// tables can't dangle by construction, but `Symbol.file` is a plain
/// string property -- a partial write or interrupted removal CAN leave
/// symbols pointing at files the graph no longer knows. Computed
/// client-side as a set difference: Kuzu's Cypher subset has no reliable
/// NOT-EXISTS subquery to push this down.
fn check_symbol_file_references(backend: &crate::graph::KuzuBackend) -> CheckResult {
    let label = "graph: symbol->file references";
    let distinct_symbol_files = backend.raw_query("MATCH (s:Symbol) RETURN DISTINCT s.file");
    let file_ids = backend.raw_query("MATCH (f:File) RETURN f.id");
    match (distinct_symbol_files, file_ids) {
        (Ok(sym_files), Ok(files)) => {
            let known: std::collections::HashSet<&str> = files
                .iter()
                .filter_map(|row| row.first().map(String::as_str))
                .collect();
            let orphaned: Vec<&str> = sym_files
                .iter()
                .filter_map(|row| row.first().map(String::as_str))
                .filter(|f| !known.contains(*f))
                .collect();
            if orphaned.is_empty() {
                CheckResult::pass(
                    CATEGORY,
                    label,
                    format!("every symbol's file exists ({} files)", known.len()),
                )
            } else {
                CheckResult::fail(
                    CATEGORY,
                    label,
                    format!(
                        "{} distinct file path(s) referenced by symbols have no File node \
                         (first: {})",
                        orphaned.len(),
                        orphaned.first().unwrap_or(&"?")
                    ),
                    "run `infigraph index --full` to rebuild consistently",
                )
            }
        }
        (Err(e), _) | (_, Err(e)) => CheckResult::warn(
            CATEGORY,
            label,
            format!("could not query the graph: {e:#}"),
            "if this persists, the graph may be corrupt -- `infigraph index --full` rebuilds",
        ),
    }
}

/// The embeddings sidecar parses, its recorded generation matches the live
/// graph's, and its entry count is sane relative to the symbol count.
fn check_embeddings_sidecar(backend: &crate::graph::KuzuBackend, ig: &Path) -> Vec<CheckResult> {
    let mut results = Vec::new();
    let emb_path = ig.join("embeddings.bin");
    if !emb_path.exists() {
        results.push(CheckResult::warn(
            CATEGORY,
            "embeddings.bin: parse",
            "sidecar not present",
            "run `infigraph index` to build embeddings (search falls back to BM25 without them)",
        ));
        return results;
    }

    let symbol_count = backend
        .raw_query("MATCH (s:Symbol) RETURN count(s)")
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.first()).cloned())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    match crate::embed::load_embeddings(&emb_path) {
        Ok(embeddings) => {
            results.push(CheckResult::pass(
                CATEGORY,
                "embeddings.bin: parse",
                format!(
                    "{} embeddings ({} symbols in graph)",
                    embeddings.len(),
                    symbol_count
                ),
            ));
            if embeddings.is_empty() && symbol_count > 0 {
                results.push(CheckResult::warn(
                    CATEGORY,
                    "embeddings.bin: coverage",
                    format!(
                        "0 embeddings for {symbol_count} symbols -- semantic search is dead weight"
                    ),
                    "run `infigraph index` to rebuild embeddings",
                ));
            }
        }
        Err(e) => {
            results.push(CheckResult::fail(
                CATEGORY,
                "embeddings.bin: parse",
                format!("sidecar does not parse: {e:#}"),
                "delete it and run `infigraph index` -- embeddings are derived data, rebuilding is safe",
            ));
            return results; // generation check is meaningless for an unparseable file
        }
    }

    // Generation match (R3.3.3): the marker records which graph generation
    // the sidecar was built from; 0/absent means "predates generation
    // tracking or backend doesn't track" -- can't judge, stay silent.
    let current_gen = backend.current_ast_generation().unwrap_or(0);
    let recorded = crate::embed::read_generation_marker(&emb_path);
    match (current_gen, recorded) {
        (0, _) | (_, None) => {}
        (current, Some(recorded)) if recorded == current => {
            results.push(CheckResult::pass(
                CATEGORY,
                "embeddings.bin: generation",
                format!("matches the live graph (generation {current})"),
            ));
        }
        (current, Some(recorded)) => {
            results.push(CheckResult::warn(
                CATEGORY,
                "embeddings.bin: generation",
                format!(
                    "built from graph generation {recorded}, live graph is at {current} -- \
                     search may return stale symbols"
                ),
                "run `infigraph index` to refresh",
            ));
        }
    }
    results
}
