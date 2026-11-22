//! Guards the read/checkpoint gate (`graph::gate`, `graph::gated`): a code-graph
//! connection is made only by `GraphStore::connection()`, which gates every
//! statement. A raw `Connection::new` on the graph's `Database` anywhere else
//! skips the gate, and a checkpoint can then run beside its statements.

use std::path::{Path, PathBuf};

/// Files that may call `Connection::new`, each with the reason it is not the code graph
/// (or is the one place that must).
const ALLOWED: &[(&str, &str)] = &[
    (
        "infigraph-core/src/graph/store.rs",
        "`GraphStore::connection` itself (wraps it in `GatedConnection`), the schema-version read \
         at open (before any other statement exists), and tests that open a bare `Database`",
    ),
    (
        "infigraph-core/src/graph/session_store.rs",
        "`sessions.kuzu`, a different `Database` with its own writer",
    ),
    (
        "infigraph-docs/src/store.rs",
        "`docs.kuzu`, a different `Database`: its readers and writers exclude each other with \
         `docs-op.lock`, so its checkpoints cannot overlap a read",
    ),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_raw_connection_on_the_code_graph_outside_the_gated_route() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut files = Vec::new();
    for krate in std::fs::read_dir(&crates).unwrap().flatten() {
        let src = krate.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    let mut offenders = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(&crates)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED.iter().any(|(allowed, _)| *allowed == rel) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for (i, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("Connection::new(") && !code.contains("LspConnection") {
                offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a raw `Connection::new` skips the read/checkpoint gate; use `GraphStore::connection()` \
         (or, for another Database, add the file to ALLOWED with the reason):\n{}",
        offenders.join("\n")
    );
}

/// The allow list must not rot: every entry still names a file that makes a connection.
#[test]
fn every_allowed_file_still_makes_a_raw_connection() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    for (rel, why) in ALLOWED {
        let text = std::fs::read_to_string(crates.join(rel))
            .unwrap_or_else(|_| panic!("{rel} is on the allow list but is gone ({why})"));
        assert!(
            text.contains("Connection::new("),
            "{rel} no longer calls `Connection::new`: drop it from ALLOWED ({why})"
        );
    }
}
