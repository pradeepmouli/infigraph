//! Where a SCIP import spends its time and memory on a graph the size of a
//! large TypeScript monorepo (~100k symbols over 800 files). Ignored: it takes
//! minutes and exists to be read, not to gate. Run it with
//! `cargo test -p infigraph-core --test scip_import_profile -- --ignored --nocapture`
//! and read the `[scip-import] phases:` line and the table printed after it.

use std::sync::Arc;
use std::time::Instant;

use infigraph_core::graph::GraphStore;
use infigraph_core::phase_clock::peak_rss_bytes;
use infigraph_core::scip::{import_scip_index, use_language_registry};
use protobuf::Message;
use scip::types::{Document, Index, Occurrence, SymbolInformation, SymbolRole};

const FILES: usize = 800;
/// Symbols per file the graph already holds (tree-sitter's).
const EXISTING: usize = 125;
/// Symbols per file only SCIP finds.
const NEW: usize = 25;
/// Reference occurrences per defined symbol.
const REFS: usize = 3;

fn moniker(file: &str, n: usize) -> String {
    format!("scip-typescript npm test 1.0.0 `{file}`/sym{n}().")
}

#[test]
#[ignore = "a profile, minutes long: run it by hand"]
fn profile_a_scip_import_on_a_graph_of_100k_symbols() {
    use_language_registry(Arc::new(infigraph_languages::bundled_registry().unwrap()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
    let store = GraphStore::open(
        &dir.path()
            .join(".infigraph")
            .join(infigraph_core::graph::names::STEM),
    )
    .unwrap();
    let conn = store.connection().unwrap();

    let built = Instant::now();
    for f in 0..FILES {
        let file = format!("src/f{f}.ts");
        conn.query(&format!(
            "CREATE (:File {{id: '{file}', name: '{file}', path: '{file}', \
             language: 'typescript', symbol_count: {EXISTING}}})"
        ))
        .unwrap();
        let rows: Vec<String> = (0..EXISTING)
            .map(|n| {
                format!(
                    "{{id: '{file}::sym{n}', name: 'sym{n}', sl: {}, el: {}}}",
                    n * 10 + 1,
                    n * 10 + 9
                )
            })
            .collect();
        conn.query(&format!(
            "UNWIND [{}] AS s CREATE (:Symbol {{id: s.id, name: s.name, kind: 'function', \
             file: '{file}', start_line: s.sl, end_line: s.el, signature_hash: '', \
             language: 'typescript', visibility: 'public', parent: '', docstring: '', \
             complexity: 0, parameters: '', return_type: ''}})",
            rows.join(", ")
        ))
        .unwrap();
    }
    eprintln!(
        "built a graph of {} symbols in {:.1}s",
        FILES * EXISTING,
        built.elapsed().as_secs_f64()
    );

    let documents: Vec<Document> = (0..FILES)
        .map(|f| {
            let file = format!("src/f{f}.ts");
            let total = EXISTING + NEW;
            let mut occurrences = Vec::new();
            for n in 0..total {
                let line = (n * 10) as i32;
                occurrences.push(Occurrence {
                    range: vec![line, 0, line, 6],
                    symbol: moniker(&file, n),
                    symbol_roles: SymbolRole::Definition as i32,
                    ..Default::default()
                });
                for r in 0..REFS {
                    let target = (n + r + 1) % total;
                    occurrences.push(Occurrence {
                        range: vec![line + 2 + r as i32, 4, line + 2 + r as i32, 10],
                        symbol: moniker(&file, target),
                        symbol_roles: 0,
                        ..Default::default()
                    });
                }
            }
            Document {
                relative_path: file.clone(),
                occurrences,
                symbols: (0..total)
                    .map(|n| SymbolInformation {
                        symbol: moniker(&file, n),
                        documentation: vec![format!("docs for sym{n}")],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
        })
        .collect();
    let path = dir.path().join("index.scip");
    std::fs::write(
        &path,
        Index {
            documents,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap(),
    )
    .unwrap();
    eprintln!(
        "index: {} MB; peak RSS before the import: {} MB",
        std::fs::metadata(&path).unwrap().len() / (1024 * 1024),
        peak_rss_bytes().unwrap_or(0) / (1024 * 1024)
    );

    let started = Instant::now();
    let stats = import_scip_index(&path, &store, Some(dir.path())).unwrap();
    eprintln!(
        "import took {:.1}s, peak RSS after: {} MB: {stats}",
        started.elapsed().as_secs_f64(),
        peak_rss_bytes().unwrap_or(0) / (1024 * 1024)
    );

    // What the daemon runs after every import (`finish_scip_import`): once
    // with the 20k new symbols to embed, then again with nothing to do.
    drop(conn);
    let backend = infigraph_core::graph::KuzuBackend::from_store(store);
    for round in ["after the import", "again, nothing changed"] {
        let started = Instant::now();
        let n = infigraph_core::embed::update_embeddings(&backend, dir.path(), &[]).unwrap();
        eprintln!(
            "embeddings {round}: {n} on disk, {:.1}s, peak RSS {} MB",
            started.elapsed().as_secs_f64(),
            peak_rss_bytes().unwrap_or(0) / (1024 * 1024)
        );
    }
}
