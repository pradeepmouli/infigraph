//! `find_clones` over a real indexed project: the pairs a body comparison
//! finds that the signature-text comparison it replaces could not, and the
//! pair that comparison reported and a body comparison does not.
//!
//! Hermetic: bodies are embedded by the word-hashing stand-in in
//! `common/clone_fixture.rs`, so no model is loaded or downloaded.

use std::path::Path;

use infigraph_core::clone_detect::find::{find_clones, Mode, Options, Signal};
use infigraph_core::path_scope::PathScope;

#[path = "common/clone_fixture.rs"]
mod clone_fixture;
use clone_fixture::{project, write};

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
    let report = find_clones(ig.backend().unwrap(), dir.path(), &options(dir.path())).unwrap();
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
    // The same shape and locals with different callees: different code.
    assert!(
        !found.iter().any(|(x, y, _)| x == "check" && y == "check"),
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
    let report = find_clones(ig.backend().unwrap(), dir.path(), &scoped).unwrap();
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
    let report = find_clones(ig.backend().unwrap(), dir.path(), &subtree).unwrap();
    assert_eq!(report.pairs.len(), 2, "{:?}", names(&report));
}

#[test]
fn functions_without_a_body_entry_are_compared_by_signature_and_labeled() {
    let (dir, ig) = project();
    std::fs::remove_file(dir.path().join(".infigraph/clone-bodies.bin")).unwrap();
    let report = find_clones(ig.backend().unwrap(), dir.path(), &options(dir.path())).unwrap();
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
        "[clones]\nmin_overlap = 0.25\n",
    );
    assert_eq!(options(dir.path()).min_overlap, 0.25);
    assert_eq!(options(dir.path()).min_tokens, 30);

    // min_overlap is a fraction: outside 0..=1 is an error naming the key.
    for bad in ["1.5", "-0.1"] {
        write(
            dir.path(),
            ".infigraph/config.toml",
            &format!("[clones]\nmin_overlap = {bad}\n"),
        );
        let err = Options::for_project(dir.path(), 0.9, 5, "Function", PathScope::everything())
            .unwrap_err();
        assert!(format!("{err:#}").contains("min_overlap"), "{err:#}");
    }

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
    let report = find_clones(ig.backend().unwrap(), dir.path(), &options(dir.path())).unwrap();
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
