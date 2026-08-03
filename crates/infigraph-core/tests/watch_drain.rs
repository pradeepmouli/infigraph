//! Regression coverage for the daemon index-work-queue coalescing fix,
//! driving IndexWorkQueue + execute_drain directly -- no real daemon
//! process needed to prove the coalescing logic itself is correct. See
//! docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.

// NOTE: this file is included as an in-crate `#[cfg(test)]` module (see
// lib.rs's `watch_drain_tests`), not compiled as a standalone Cargo
// integration test -- it exercises `watch::queue`/`watch::drain`, both
// intentionally `pub(crate)`, which an external integration test crate
// cannot see. `crate::` paths (not the crate's own name) are required as
// a result; see the `[[test]] test = false` override in Cargo.toml.
use crate::lang::{LanguagePack, LanguageRegistry};
use crate::Infigraph;
use std::fs;

// `LanguageRegistry::new()` starts empty -- no language packs are
// registered by default. Same minimal Python pack pattern used by
// `tests/index_perf.rs`/`combined_graph.rs`/`extract_pipeline.rs`.
const PYTHON_ENTITIES: &str = r#"
(module
  (function_definition
    name: (identifier) @func.name) @func.def)

(class_definition
  name: (identifier) @class.name) @class.def
"#;

const PYTHON_RELATIONS: &str = r#"
(call
  function: (identifier) @call.func) @call.site
"#;

fn python_pack() -> LanguagePack {
    let grammar = tree_sitter_python::LANGUAGE.into();
    LanguagePack::new(
        "python",
        vec![".py"],
        grammar,
        PYTHON_ENTITIES,
        PYTHON_RELATIONS,
    )
    .unwrap()
}

fn python_registry() -> LanguageRegistry {
    let mut reg = LanguageRegistry::new();
    reg.register(python_pack());
    reg
}

fn open_project(root: &std::path::Path) -> Infigraph {
    let mut prism = Infigraph::open(root, python_registry()).unwrap();
    prism.init().unwrap();
    prism
}

#[test]
fn a_raw_entry_and_an_index_waiter_for_the_same_new_file_coalesce_into_one_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("existing.py"), "def existing():\n    pass\n").unwrap();

    let prism = open_project(root);
    // Bootstrap: one file already indexed, matching the live repro's setup
    // (a project that already has SOME content before the race file appears).
    prism.index().unwrap();

    // The exact scenario from the live repro: a new file appears, and
    // BEFORE the watcher's own debounce would have settled, an ad-hoc
    // Index request also targets it -- both land in the SAME queue before
    // any execution happens.
    fs::write(root.join("fourth.py"), "def fourth():\n    pass\n").unwrap();

    let mut queue = crate::watch::queue::IndexWorkQueue::new();
    queue.add_raw("fourth.py".to_string());

    let result_path = tmp.path().join("waiter.result");
    queue.add_waiter(crate::watch::queue::Waiter {
        kind: crate::watch::queue::WaiterKind::Index,
        use_learned: false,
        reply_path: result_path.clone(),
    });

    let drained = queue.drain();
    let outcome = crate::watch::drain::execute_drain(&prism, drained).unwrap();

    // Exactly one extraction/upsert occurred for fourth.py -- the whole
    // point of this test. Before the fix, this scenario (a Raw entry
    // already queued, plus a second, independent decision to index the
    // same file) produced TWO separate index_files calls and a duplicate
    // primary key error.
    assert_eq!(outcome.extractions.len(), 1);
    assert_eq!(outcome.extractions[0].file, "fourth.py");

    // The waiter's reply reflects the real combined execution.
    let reply_contents = fs::read_to_string(&result_path).unwrap();
    let reply: crate::daemon_protocol::WriteResult = serde_json::from_str(&reply_contents).unwrap();
    match reply {
        crate::daemon_protocol::WriteResult::Ok { indexed_files, .. } => {
            assert_eq!(indexed_files, 1);
        }
        other => panic!("expected WriteResult::Ok, got {other:?}"),
    }
}

#[test]
fn resolve_only_extractions_do_not_trigger_a_redundant_upsert() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
    let prism = open_project(root);
    prism.index().unwrap();

    let backend = prism.backend().unwrap();
    let existing_hash_count_before = backend.get_file_hashes().unwrap().len();

    let mut queue = crate::watch::queue::IndexWorkQueue::new();
    let extraction = crate::model::FileExtraction {
        file: "a.py".to_string(),
        language: "python".to_string(),
        content_hash: "whatever".to_string(),
        symbols: Vec::new(),
        relations: Vec::new(),
        statements: Vec::new(),
    };
    queue.add_resolve_only(extraction);
    let drained = queue.drain();
    let outcome = crate::watch::drain::execute_drain(&prism, drained).unwrap();

    // resolve_only contributes to the resolve pass, not the upsert pass --
    // outcome.extractions (the upsert set) must be empty, even though the
    // resolve step ran against a.py.
    assert!(
        outcome.extractions.is_empty(),
        "ResolveOnly items must not appear in the upsert extraction set"
    );
    assert_eq!(
        backend.get_file_hashes().unwrap().len(),
        existing_hash_count_before,
        "a redundant upsert would not change the hash count here, but IS a real wasted write -- \
         this assertion documents the guarantee this test exists to protect"
    );
}
