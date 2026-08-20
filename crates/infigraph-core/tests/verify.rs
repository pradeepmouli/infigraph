//! Integration coverage for `infigraph verify` (R3.4.1, #10): each check
//! against a real (tiny) Kuzu graph, plus the failure modes verify exists
//! to catch -- an unopenable graph, a dangling symbol->file reference, an
//! unparseable embeddings sidecar, and a stale generation marker.

use infigraph_core::doctor::CheckStatus;
use infigraph_core::graph::{db_lock_path, GraphStore};
use infigraph_core::verify::run_verify;
use std::fs;
use std::path::{Path, PathBuf};

fn project_with_graph(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let root = tmp.path().join("proj");
    let graph_path = root.join(".infigraph").join("graph");
    (root, graph_path)
}

fn status_of<'a>(
    results: &'a [infigraph_core::doctor::CheckResult],
    label: &str,
) -> &'a infigraph_core::doctor::CheckResult {
    results
        .iter()
        .find(|r| r.name == label)
        .unwrap_or_else(|| panic!("no check labeled {label:?} in {results:#?}"))
}

#[test]
fn missing_graph_reports_a_single_warn() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("empty");
    fs::create_dir_all(&root).unwrap();

    let results = run_verify(&root);

    assert_eq!(results.len(), 1, "{results:#?}");
    assert_eq!(results[0].status, CheckStatus::Warn);
}

#[test]
fn fresh_consistent_graph_passes_open_and_reference_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, graph_path) = project_with_graph(&tmp);
    drop(GraphStore::open(&graph_path).unwrap());

    let results = run_verify(&root);

    assert_eq!(status_of(&results, "graph: open").status, CheckStatus::Pass);
    assert_eq!(
        status_of(&results, "graph: symbol->file references").status,
        CheckStatus::Pass
    );
    // No embeddings sidecar yet: worth a warn, not a fail.
    assert_eq!(
        status_of(&results, "embeddings.bin: parse").status,
        CheckStatus::Warn
    );
}

#[test]
fn unopenable_graph_is_a_fail_that_short_circuits() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, graph_path) = project_with_graph(&tmp);
    drop(GraphStore::open(&graph_path).unwrap());
    // Poison with the R3.1.3 state: unreplayed WAL + dead lock holder. The
    // open-time guard's refusal must surface as verify's `graph: open` FAIL.
    fs::write(format!("{}.wal", graph_path.display()), b"wal").unwrap();
    let info = infigraph_core::lockfile::LockInfo {
        pid: 999_999,
        role: "graph-write".to_string(),
        build_hash: "test".to_string(),
        acquired_at: 0,
        last_heartbeat: 0,
        holder_started_at: 0,
    };
    fs::write(
        db_lock_path(&graph_path),
        serde_json::to_string(&info).unwrap(),
    )
    .unwrap();

    let results = run_verify(&root);

    let open = status_of(&results, "graph: open");
    assert_eq!(open.status, CheckStatus::Fail);
    assert!(
        open.message.contains("unreplayed WAL"),
        "the guard's actionable message must pass through: {}",
        open.message
    );
    assert_eq!(
        results.len(),
        1,
        "downstream checks are unanswerable without the graph: {results:#?}"
    );
}

fn insert_symbol_without_file(graph_path: &Path) {
    let store = GraphStore::open(graph_path).unwrap();
    let conn = store.connection().unwrap();
    conn.query(
        "CREATE (s:Symbol {id: 'ghost.py::orphan', name: 'orphan', \
         file: 'ghost.py', kind: 'function'})",
    )
    .unwrap();
}

#[test]
fn symbol_referencing_a_missing_file_node_is_a_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, graph_path) = project_with_graph(&tmp);
    insert_symbol_without_file(&graph_path);

    let results = run_verify(&root);

    let refs = status_of(&results, "graph: symbol->file references");
    assert_eq!(refs.status, CheckStatus::Fail);
    assert!(refs.message.contains("ghost.py"), "{}", refs.message);
}

#[test]
fn unparseable_embeddings_sidecar_is_a_fail() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, graph_path) = project_with_graph(&tmp);
    drop(GraphStore::open(&graph_path).unwrap());
    fs::write(root.join(".infigraph").join("embeddings.bin"), b"garbage").unwrap();

    let results = run_verify(&root);

    assert_eq!(
        status_of(&results, "embeddings.bin: parse").status,
        CheckStatus::Fail
    );
}

#[test]
fn stale_generation_marker_is_a_warn_and_matching_one_a_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let (root, graph_path) = project_with_graph(&tmp);
    let emb_path = root.join(".infigraph").join("embeddings.bin");
    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
        infigraph_core::embed::save_embeddings(&emb_path, &[("sym".to_string(), vec![0.1; 4])])
            .unwrap();
        infigraph_core::embed::write_generation_marker(&emb_path, 1).unwrap();
    }

    let results = run_verify(&root);
    assert_eq!(
        status_of(&results, "embeddings.bin: generation").status,
        CheckStatus::Pass,
        "marker == live generation must pass: {results:#?}"
    );

    // Advance the graph a generation without rebuilding the sidecar.
    {
        let store = GraphStore::open(&graph_path).unwrap();
        let lock = store.write_lock().unwrap();
        let conn = store.connection().unwrap();
        store.bump_ast_generation_conn(&conn, &lock).unwrap();
    }

    let results = run_verify(&root);
    let generation = status_of(&results, "embeddings.bin: generation");
    assert_eq!(generation.status, CheckStatus::Warn);
    assert!(
        generation.message.contains("stale"),
        "{}",
        generation.message
    );
}
