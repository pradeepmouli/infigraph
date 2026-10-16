//! `find_clones` over a real indexed project: the pairs a body comparison
//! finds that the signature-text comparison it replaces could not, and the
//! pair that comparison reported and a body comparison does not.
//!
//! Hermetic: bodies are embedded by a word-hashing embedder defined here.

use std::path::Path;

use infigraph_core::clone_detect::find::{find_clones, Mode, Options, Signal};
use infigraph_core::embed::EmbedProvider;
use infigraph_core::path_scope::PathScope;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

/// Embeds a text as its word 4-grams hashed into 512 buckets, so equal texts
/// have equal vectors and texts of another shape have unlike ones.
struct WordEmbedder;

impl EmbedProvider for WordEmbedder {
    fn name(&self) -> &'static str {
        "test-words"
    }
    fn dimension(&self) -> usize {
        512
    }
    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let words: Vec<&str> = text.split_whitespace().collect();
                let mut v = vec![0f32; 512];
                for gram in words.windows(4) {
                    v[infigraph_core::embed::fnv1a64(gram.join(" ").as_bytes()) as usize % 512] +=
                        1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1.0);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

/// A function body of `n` statements whose shape depends on `seed`.
fn body(seed: u32, n: u32) -> String {
    (0..n)
        .map(|i| {
            let h = infigraph_core::embed::fnv1a64(format!("{seed}:{i}").as_bytes()) as usize;
            let kw = [
                "if", "while", "for", "return", "await", "yield", "raise", "try",
            ][h % 8];
            let op = ["+", "-", "*", "/", "%", "<", ">", "=="][(h / 8) % 8];
            format!("    {kw} a {op} b")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn function(name: &str, doc: &str, code: &str) -> String {
    format!("def {name}(a, b):\n    \"\"\"{doc}\"\"\"\n{code}\n")
}

fn write(root: &Path, file: &str, text: &str) {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Three families, each in two files:
/// - `renamed` / `copied`: the same code under other names and another docstring;
/// - `process` in a.py and in b.py: the same name, parameters and docstring,
///   different bodies (the signature-text comparison called these clones);
/// - `twin_one` / `twin_two`: byte-identical bodies under different
///   signatures (it could not see these).
fn project() -> (tempfile::TempDir, Infigraph) {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(
        r,
        "src/a.py",
        &function("process", "Process the batch.", &body(1, 30)),
    );
    write(
        r,
        "src/b.py",
        &function("process", "Process the batch.", &body(2, 30)),
    );
    write(
        r,
        "src/c.py",
        &function("renamed", "Add things.", &body(3, 30)),
    );
    write(
        r,
        "src/d.py",
        &function("copied", "Combine items.", &body(3, 30)),
    );
    write(
        r,
        "src/e.py",
        &format!("def twin_one(x, y):\n{}\n", body(4, 30)),
    );
    write(
        r,
        "src/f.py",
        &format!("def twin_two(left, right):\n{}\n", body(4, 30)),
    );
    let mut ig = Infigraph::open(r, bundled_registry().unwrap()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();
    infigraph_core::embed::update_embeddings_with(ig.backend().unwrap(), r, &[], &|| {
        Box::new(WordEmbedder)
    })
    .unwrap();
    (dir, ig)
}

fn options(root: &Path) -> Options {
    Options::for_project(root, 0.92, 50, "Function,Method", PathScope::everything()).unwrap()
}

fn names(report: &infigraph_core::clone_detect::find::Report) -> Vec<(String, String, Signal)> {
    let mut found: Vec<_> = report
        .pairs
        .iter()
        .map(|p| {
            let mut pair = [p.a.name.clone(), p.b.name.clone()];
            pair.sort();
            let [x, y] = pair;
            (x, y, p.signal)
        })
        .collect();
    found.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    found
}

#[test]
fn bodies_decide_not_names() {
    let (dir, ig) = project();
    let report = find_clones(
        ig.backend().unwrap(),
        dir.path(),
        &options(dir.path()),
        None,
    )
    .unwrap();
    let found = names(&report);

    // The same code under other names and another docstring: found, by the
    // embedding of its normalized body.
    assert!(
        found.contains(&("copied".into(), "renamed".into(), Signal::BodyEmbedding)),
        "{found:?}"
    );
    // Different signatures, identical bodies: found.
    assert!(
        found.contains(&("twin_one".into(), "twin_two".into(), Signal::ExactBody)),
        "{found:?}"
    );
    // The same name, parameters and docstring with different bodies: not.
    assert!(
        !found
            .iter()
            .any(|(x, y, _)| x == "process" && y == "process"),
        "{found:?}"
    );
    assert_eq!(report.mode, Mode::Exact);
    assert!(!report.truncated);
    assert_eq!(report.signature_only, 0);
}

#[test]
fn a_scope_compares_only_pairs_within_it() {
    let (dir, ig) = project();
    let scoped = Options::for_project(
        dir.path(),
        0.92,
        50,
        "Function",
        PathScope::new(dir.path(), Some("src/e.py")),
    )
    .unwrap();
    let report = find_clones(ig.backend().unwrap(), dir.path(), &scoped, None).unwrap();
    // One file in scope: nothing to pair it with.
    assert!(report.pairs.is_empty(), "{:?}", report.pairs);

    let subtree = Options::for_project(
        dir.path(),
        0.92,
        50,
        "Function",
        PathScope::new(dir.path(), Some("src")),
    )
    .unwrap();
    let report = find_clones(ig.backend().unwrap(), dir.path(), &subtree, None).unwrap();
    assert_eq!(report.pairs.len(), 2, "{:?}", names(&report));
}

#[test]
fn functions_without_a_body_entry_are_compared_by_signature_and_labeled() {
    let (dir, ig) = project();
    std::fs::remove_file(dir.path().join(".infigraph/clone-bodies.bin")).unwrap();
    let report = find_clones(
        ig.backend().unwrap(),
        dir.path(),
        &options(dir.path()),
        None,
    )
    .unwrap();
    assert_eq!(report.signature_only, report.symbols);
    assert!(
        report.pairs.iter().all(|p| p.signal == Signal::Signature),
        "{:?}",
        report.pairs
    );
}

#[test]
fn min_tokens_comes_from_the_projects_config_and_a_bad_value_is_an_error() {
    let (dir, _ig) = project();
    assert_eq!(options(dir.path()).min_tokens, 30);

    write(
        dir.path(),
        ".infigraph/config.toml",
        "[clones]\nmin_tokens = 7\nexact_below = 3\n",
    );
    let configured = options(dir.path());
    assert_eq!((configured.min_tokens, configured.exact_below), (7, 3));

    write(
        dir.path(),
        ".infigraph/config.toml",
        "[clones]\nmin_tokens = \"many\"\n",
    );
    assert!(Options::for_project(dir.path(), 0.9, 5, "Function", PathScope::everything()).is_err());
}

#[test]
fn above_exact_below_lsh_runs_and_says_so() {
    let (dir, ig) = project();
    write(
        dir.path(),
        ".infigraph/config.toml",
        "[clones]\nexact_below = 2\n",
    );
    let report = find_clones(
        ig.backend().unwrap(),
        dir.path(),
        &options(dir.path()),
        None,
    )
    .unwrap();
    assert_eq!(report.mode, Mode::Lsh);
    let found = names(&report);
    assert!(
        found.contains(&("copied".into(), "renamed".into(), Signal::BodyEmbedding)),
        "{found:?}"
    );
    assert!(
        report.candidates_verified < 15,
        "{}",
        report.candidates_verified
    );
}
