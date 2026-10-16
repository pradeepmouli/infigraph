//! `detect_clones` compares function bodies, not names. A generated project
//! (see `clone_fixture.rs`) has two pairs the signature-text comparison got
//! wrong in each direction:
//! - a name, parameters and docstring shared by two functions with different
//!   bodies were reported as clones (`process`, and `check`, whose bodies call
//!   different functions);
//! - the same code under other names and signatures was not (`renamed` and
//!   `copied`, `twin_one` and `twin_two`).
//!
//! Hermetic: the project is embedded by the word-hashing stand-in in the
//! fixture, so no model is loaded or downloaded.

use serde_json::json;

use infigraph_mcp::tools::analysis::clones::tool_detect_clones;

mod support;

#[path = "../../infigraph-core/tests/common/clone_fixture.rs"]
mod clone_fixture;

/// The `(id, id)` of every pair line of a `detect_clones` report.
fn pairs(report: &str) -> Vec<(String, String)> {
    report
        .lines()
        .filter(|line| line.contains(" <-> "))
        .map(|line| {
            let ids: Vec<&str> = line
                .split('(')
                .skip(1)
                .filter_map(|part| part.split(')').next())
                .collect();
            let mut pair = [ids[0].to_string(), ids[1].to_string()];
            pair.sort();
            let [a, b] = pair;
            (a, b)
        })
        .collect()
}

fn pair(a: &str, b: &str) -> (String, String) {
    (a.to_string(), b.to_string())
}

#[test]
fn bodies_decide_not_names() {
    support::disable_background_watchers();
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    let (dir, ig) = clone_fixture::project();
    drop(ig); // the tool opens the project itself

    let report = tool_detect_clones(&json!({
        "path": dir.path().to_string_lossy(),
        "threshold": 0.75,
        "limit": 50,
        "store_edges": false,
    }))
    .unwrap();
    let found = pairs(&report);

    assert!(
        found.contains(&pair("src/c.py::renamed", "src/d.py::copied")),
        "the same code under other names is a clone:\n{report}"
    );
    assert!(
        found.contains(&pair("src/e.py::twin_one", "src/f.py::twin_two")),
        "identical bodies under different signatures are clones:\n{report}"
    );
    assert!(
        !found.contains(&pair("src/a.py::process", "src/b.py::process")),
        "a shared name and docstring are not a shared body:\n{report}"
    );
    assert!(
        !found.contains(&pair("src/g.py::check", "src/h.py::check")),
        "bodies calling different functions are not clones:\n{report}"
    );
    for label in ["exact-body", "body-embedding"] {
        assert!(report.contains(label), "no {label} signal in:\n{report}");
    }
}
