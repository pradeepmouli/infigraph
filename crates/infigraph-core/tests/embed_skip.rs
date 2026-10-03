use infigraph_core::lang::{LanguagePack, LanguageRegistry};
use infigraph_core::Infigraph;

const PYTHON_ENTITIES: &str = r#"
(module
  (function_definition
    name: (identifier) @func.name
    body: (block
      (expression_statement
        (string) @func.docstring)?)) @func.def)

(module
  (decorated_definition
    (decorator) @func.decorator
    definition: (function_definition
      name: (identifier) @func.name
      body: (block
        (expression_statement
          (string) @func.docstring)?)) @func.def))

(class_definition
  name: (identifier) @class.name
  body: (block
    (expression_statement
      (string) @class.docstring)?)) @class.def

(class_definition
  body: (block
    (function_definition
      name: (identifier) @method.name
      body: (block
        (expression_statement
          (string) @method.docstring)?)) @method.def))

(class_definition
  body: (block
    (decorated_definition
      (decorator) @method.decorator
      definition: (function_definition
        name: (identifier) @method.name
        body: (block
          (expression_statement
            (string) @method.docstring)?)) @method.def)))

(module
  (function_definition
    name: (identifier) @test.name
    (#match? @test.name "^test_")
    body: (block
      (expression_statement
        (string) @test.docstring)?)) @test.def)

(module
  (expression_statement
    (assignment
      left: (identifier) @var.name)) @var.def)
"#;

const PYTHON_RELATIONS: &str = r#"
(call
  function: (identifier) @call.func) @call.site

(call
  function: (attribute
    object: (_) @call.receiver
    attribute: (identifier) @call.func)) @call.site

(import_statement
  name: (dotted_name) @import.module)

(import_from_statement
  module_name: (dotted_name) @import.module)

(class_definition
  name: (identifier) @inherit.child
  superclasses: (argument_list
    (identifier) @inherit.parent))
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

const FILE_A: &str = "src/alpha.py";
// Kept alive across every test so the graph is never fully empty: deleting
// FILE_A alone must not make `rows` empty, which would trip the unrelated
// `if rows.is_empty() { return Ok(0); }` early-return at the top of
// `update_embeddings` and mask the pruning behavior under test.
const FILE_ANCHOR: &str = "src/anchor.py";

fn write_alpha(root: &std::path::Path, body_line: &str, docstring: &str) {
    let src = format!(
        "def helper(x):\n    \"\"\"{docstring}\"\"\"\n    {body_line}\n    return x\n\ndef other():\n    \"\"\"Stable other function.\"\"\"\n    return 1\n"
    );
    std::fs::write(root.join(FILE_A), src).unwrap();
}

fn write_anchor(root: &std::path::Path) {
    std::fs::write(
        root.join(FILE_ANCHOR),
        "def anchor():\n    \"\"\"Never touched.\"\"\"\n    return True\n",
    )
    .unwrap();
}

// `Infigraph::index()` does not itself call `update_embeddings` (that wiring
// is a separate, not-yet-landed task per docs/superpowers/plans/2026-08-04-
// daemon-routed-full-reindex.md) — so every index step here is paired with
// an explicit `update_embeddings` call, exactly the shape that future call
// site uses (`update_embeddings(backend, root, &[])`).
fn reembed(root: &std::path::Path, ig: &Infigraph) -> usize {
    let backend = ig.backend().expect("backend available after init/index");
    infigraph_core::embed::update_embeddings(backend, root, &[]).unwrap()
}

fn setup() -> (tempfile::TempDir, Infigraph) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    write_alpha(dir.path(), "x = 1", "Helper doc.");
    write_anchor(dir.path());
    let mut ig = Infigraph::open(dir.path(), python_registry()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
    reembed(dir.path(), &ig);
    (dir, ig)
}

fn emb_path(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".infigraph").join("embeddings.bin")
}

#[test]
fn full_index_writes_v3_with_real_hashes() {
    let (dir, _ig) = setup();
    let data = std::fs::read(emb_path(dir.path())).unwrap();
    assert_eq!(data[4], 3, "indexing should write hashed v3 format");
    let entries = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    assert!(!entries.is_empty());
    assert!(
        entries.iter().all(|(_, _, h)| *h != 0),
        "all hashes should be known"
    );
}

#[test]
fn body_only_edit_skips_rewrite_entirely() {
    let (dir, ig) = setup();
    let before = std::fs::read(emb_path(dir.path())).unwrap();

    write_alpha(dir.path(), "x = 2", "Helper doc."); // body change only
    ig.index().unwrap();
    reembed(dir.path(), &ig);

    let after = std::fs::read(emb_path(dir.path())).unwrap();
    assert_eq!(
        before, after,
        "no input changed → embeddings.bin must not be rewritten"
    );
}

#[test]
fn docstring_edit_reembeds_only_that_symbol() {
    let (dir, ig) = setup();
    let before = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();

    write_alpha(dir.path(), "x = 1", "Helper doc CHANGED."); // docstring change
    ig.index().unwrap();
    reembed(dir.path(), &ig);

    let after = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    let get = |set: &[(String, Vec<f32>, u64)], name: &str| {
        set.iter()
            .find(|(id, _, _)| id.ends_with(name))
            .cloned()
            .unwrap()
    };
    let (_, _, h_helper_before) = get(&before, "::helper");
    let (_, v_other_before, h_other_before) = get(&before, "::other");
    let (_, _, h_helper_after) = get(&after, "::helper");
    let (_, v_other_after, h_other_after) = get(&after, "::other");

    assert_ne!(
        h_helper_before, h_helper_after,
        "changed docstring → new input hash"
    );
    assert_eq!(
        h_other_before, h_other_after,
        "untouched symbol keeps its hash"
    );
    assert_eq!(
        v_other_before, v_other_after,
        "untouched symbol keeps its exact vector"
    );
}

#[test]
fn deleted_file_still_prunes_embeddings() {
    let (dir, ig) = setup();
    std::fs::remove_file(dir.path().join(FILE_A)).unwrap();
    ig.index().unwrap();
    reembed(dir.path(), &ig);
    let entries = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    assert!(
        !entries.iter().any(|(id, _, _)| id.contains("alpha.py")),
        "symbols from the deleted file must be pruned"
    );
}

// Regression test: `update_embeddings` used to return `Ok(0)` as soon as the
// graph query came back empty, before loading or pruning `existing` at all.
// Deleting the last indexed file (or symbol) hit that early-return, leaving
// embeddings.bin's stale vectors on disk forever.
#[test]
fn deleting_every_indexed_file_prunes_embeddings_to_empty() {
    let (dir, ig) = setup();
    std::fs::remove_file(dir.path().join(FILE_A)).unwrap();
    std::fs::remove_file(dir.path().join(FILE_ANCHOR)).unwrap();
    ig.index().unwrap();
    let count = reembed(dir.path(), &ig);
    assert_eq!(count, 0, "no symbols remain once every file is deleted");
    let entries = infigraph_core::embed::load_embeddings_hashed(&emb_path(dir.path())).unwrap();
    assert!(
        entries.is_empty(),
        "embeddings.bin must be pruned to empty, found {} stale entries",
        entries.len()
    );
}

#[test]
fn deleting_every_indexed_file_removes_a_stale_hnsw_index() {
    let (dir, ig) = setup();
    let hnsw_path = dir.path().join(".infigraph").join("hnsw_index.usearch");
    std::fs::write(&hnsw_path, b"stale placeholder index").unwrap();
    std::fs::remove_file(dir.path().join(FILE_A)).unwrap();
    std::fs::remove_file(dir.path().join(FILE_ANCHOR)).unwrap();
    ig.index().unwrap();
    reembed(dir.path(), &ig);
    assert!(
        !hnsw_path.exists(),
        "a stale HNSW index (referencing deleted symbols) must not survive an empty-graph re-embed"
    );
}

/// #75: the embedder that built `embeddings.bin` is recorded beside it, so a
/// daemon that indexed with the trigram fallback is visible after it is gone.
#[test]
fn update_embeddings_records_which_embedder_built_the_file() {
    let (dir, ig) = setup();
    assert!(reembed(dir.path(), &ig) > 0);

    let sidecar = dir.path().join(".infigraph").join("embeddings.bin");
    let recorded = infigraph_core::embed::read_embedder_marker(&sidecar);
    assert_eq!(
        recorded.as_deref(),
        infigraph_core::embed::process_embedder(),
        "the marker must name the embedder this process embedded with"
    );
    assert!(recorded.is_some());
}

/// The generation marker says which graph generation `embeddings.bin`
/// reflects. A body-only edit rewrites nothing, but the file still reflects
/// the new generation, so the marker must move: otherwise every such edit
/// leaves the embeddings reading as stale forever (#75).
#[test]
fn a_body_only_edit_still_moves_the_generation_marker() {
    let (dir, ig) = setup();
    write_alpha(dir.path(), "x = 2", "Helper doc."); // body change only
    ig.index().unwrap();
    reembed(dir.path(), &ig);

    let current = ig.backend().unwrap().current_ast_generation().unwrap();
    assert!(current > 0);
    assert_eq!(
        infigraph_core::embed::read_generation_marker(&emb_path(dir.path())),
        Some(current)
    );
}

struct FailingEmbedder;

impl infigraph_core::embed::EmbedProvider for FailingEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        anyhow::bail!("embedder unavailable")
    }
    fn embed_batch(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        anyhow::bail!("embedder unavailable")
    }
    fn dimension(&self) -> usize {
        256
    }
}

/// An update whose embedder fails embeds nothing, and with nothing pruned it
/// takes the same exit as "nothing to do". The marker must not move there:
/// the vectors do not reflect the new generation, and a marker saying they do
/// is the false "fresh" #75 exists to prevent.
#[test]
fn a_failed_embedding_does_not_move_the_generation_marker() {
    let (dir, ig) = setup();
    let backend = ig.backend().unwrap();
    let before = infigraph_core::embed::read_generation_marker(&emb_path(dir.path()));
    assert_eq!(before, Some(backend.current_ast_generation().unwrap()));

    write_alpha(dir.path(), "x = 1", "A different docstring."); // must re-embed
    ig.index().unwrap();
    let current = backend.current_ast_generation().unwrap();
    assert!(Some(current) > before);
    infigraph_core::embed::update_embeddings_with(backend, dir.path(), &[], &|| {
        Box::new(FailingEmbedder)
    })
    .unwrap();

    assert_eq!(
        infigraph_core::embed::read_generation_marker(&emb_path(dir.path())),
        before,
        "the marker moved over embeddings that were not updated"
    );
    let live = infigraph_core::degraded::live::for_root(dir.path());
    assert!(
        live.iter()
            .any(|n| n.key == infigraph_core::degraded::EMBEDDINGS_STALE),
        "the failed update is not reported as stale: {live:?}"
    );
}

/// A write to the graph makes the last staleness comparison out of date, so
/// the process holding the graph stops claiming to have judged it until the
/// next embedding attempt compares again.
#[test]
fn a_graph_write_without_an_embedding_attempt_is_not_a_judgment() {
    let (dir, ig) = setup();
    let judged = || {
        infigraph_core::degraded::live::judged_for(dir.path())
            .contains(&infigraph_core::degraded::EMBEDDINGS_STALE.to_string())
    };
    assert!(judged(), "setup embedded, which compares");

    write_alpha(dir.path(), "x = 3", "Helper doc.");
    ig.index().unwrap();
    assert!(!judged(), "the graph moved and nothing compared since");

    reembed(dir.path(), &ig);
    assert!(judged());
}

// ---- #209 item 11: the embeddings update leaves a record of what it lost ----

use infigraph_core::last_run::{self, Kind};

/// The tally of runs is process-wide and these tests share a process: the one
/// that fails an embedder on purpose must not note into another's run.
static RECORD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn embeddings_record(root: &std::path::Path) -> last_run::KindRecord {
    last_run::read(&root.join(".infigraph"), Kind::Embeddings).unwrap_or_default()
}

/// Re-index with a changed docstring so the symbol must re-embed, then update
/// with an embedder that fails.
fn fail_an_update(dir: &tempfile::TempDir, ig: &Infigraph) {
    write_alpha(dir.path(), "x = 1", "A different docstring.");
    ig.index().unwrap();
    infigraph_core::embed::update_embeddings_with(ig.backend().unwrap(), dir.path(), &[], &|| {
        Box::new(FailingEmbedder)
    })
    .unwrap();
}

#[test]
fn an_embedder_that_fails_is_recorded_with_its_reason() {
    let _lock = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, ig) = setup();

    fail_an_update(&dir, &ig);

    let problem = embeddings_record(dir.path())
        .last_problem
        .expect("a failed embedder left no record");
    let loss = problem
        .losses
        .iter()
        .find(|l| l.what == "symbols not embedded")
        .unwrap_or_else(|| panic!("no loss names the symbols: {problem:?}"));
    assert!(loss.count >= 1, "{loss:?}");
    assert!(
        loss.first_reason.contains("embedder unavailable"),
        "{loss:?}"
    );
}

#[test]
fn a_good_update_after_a_failed_one_clears_the_embeddings_problem() {
    let _lock = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, ig) = setup();
    fail_an_update(&dir, &ig);
    assert!(embeddings_record(dir.path()).last_problem.is_some());

    reembed(dir.path(), &ig);

    assert_eq!(embeddings_record(dir.path()).last_problem, None);
}

#[test]
fn an_update_that_returns_an_error_is_recorded_as_failed() {
    let _lock = RECORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, ig) = setup();
    // A directory where the file goes: the save cannot replace it.
    std::fs::remove_file(emb_path(dir.path())).unwrap();
    std::fs::create_dir(emb_path(dir.path())).unwrap();
    write_alpha(dir.path(), "x = 1", "Yet another docstring.");
    ig.index().unwrap();

    let result = infigraph_core::embed::update_embeddings(ig.backend().unwrap(), dir.path(), &[]);

    assert!(result.is_err(), "the update was expected to fail");
    let problem = embeddings_record(dir.path())
        .last_problem
        .expect("a failed update left no record");
    assert!(!problem.ok, "{problem:?}");
    assert!(
        problem.summary.contains("embedding update failed"),
        "{problem:?}"
    );
}
