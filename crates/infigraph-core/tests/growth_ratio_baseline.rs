// Regression tests (adversarial review of R3.1.4): `stamp_healthy_graph_size`
// used to be called unconditionally after every successful write -- not just
// after a verified full rebuild. That ratchets the growth-ratio breaker's
// baseline forward on every ordinary incremental write, so a graph that
// balloons via repeated sub-threshold growth (e.g. many 9x-under-the-cap
// writes in a row) never trips the breaker, even though its cumulative
// growth vastly exceeds the ratio relative to its *original* healthy size.

use infigraph_core::lang::{LanguagePack, LanguageRegistry};
use infigraph_core::Infigraph;

const PYTHON_ENTITIES: &str = r#"
(module
  (function_definition
    name: (identifier) @func.name
    body: (block
      (expression_statement
        (string) @func.docstring)?)) @func.def)
"#;

fn python_pack() -> LanguagePack {
    let grammar = tree_sitter_python::LANGUAGE.into();
    LanguagePack::new("python", vec![".py"], grammar, PYTHON_ENTITIES, "").unwrap()
}

fn python_registry() -> LanguageRegistry {
    let mut reg = LanguageRegistry::new();
    reg.register(python_pack());
    reg
}

fn health_json_bytes(root: &std::path::Path) -> Option<u64> {
    let content =
        std::fs::read_to_string(root.join(".infigraph").join("graph.health.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    v.get("healthy_size_bytes")?.as_u64()
}

#[test]
fn ordinary_incremental_writes_do_not_move_the_growth_ratio_baseline() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def a():\n    return 1\n").unwrap();

    let mut ig = Infigraph::open(dir.path(), python_registry()).unwrap();
    ig.init().unwrap();
    ig.index().unwrap();

    health_json_bytes(dir.path())
        .expect("first index establishes a baseline (stamp_healthy_graph_size_if_unset)");

    // Force a known, deliberately-huge baseline -- large enough that the
    // preflight's ratio check trivially passes (the real graph is nowhere
    // near 10x this), but if the ordinary write below still re-stamps
    // unconditionally, it will overwrite this with the real (much smaller)
    // current file size -- trivially detectable regardless of exactly how
    // many bytes any one write happens to add (Kuzu allocates in pages, so
    // comparing real before/after sizes directly is not reliable).
    const FORCED_BASELINE: u64 = 999_999_999;
    let health_path = dir.path().join(".infigraph").join("graph.health.json");
    std::fs::write(
        &health_path,
        format!(r#"{{"healthy_size_bytes": {FORCED_BASELINE}}}"#),
    )
    .unwrap();

    // A second, ordinary incremental write: add a file and reindex. This is
    // exactly the kind of write the growth-ratio breaker is supposed to be
    // measured *against*, not one that should redefine what "healthy" means.
    std::fs::write(
        dir.path().join("b.py"),
        "def b():\n    return 2\n\ndef c():\n    return 3\n",
    )
    .unwrap();
    ig.index().unwrap();

    let baseline_after_second_index = health_json_bytes(dir.path())
        .expect("baseline file must still exist after a second incremental write");

    assert_eq!(
        baseline_after_second_index, FORCED_BASELINE,
        "an ordinary incremental write must not ratchet the growth-ratio baseline forward \
         (baseline changed from the forced {FORCED_BASELINE} to {baseline_after_second_index})"
    );
}

/// Companion to the test above: a *full* reindex (a verified healthy
/// checkpoint) must still refresh the baseline -- otherwise removing the
/// unconditional per-write stamp would leave nothing establishing a fresh
/// one after a legitimate rebuild.
#[test]
fn a_full_reindex_does_refresh_the_growth_ratio_baseline() {
    let cli = infigraph_core::watch::daemon::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)");

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.py"), "def a():\n    return 1\n").unwrap();

    let status = std::process::Command::new(&cli)
        .arg("index")
        .arg("--no-embed")
        .current_dir(dir.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .status()
        .unwrap();
    assert!(status.success(), "bootstrap index failed");

    let health_path = dir.path().join(".infigraph").join("graph.health.json");
    assert!(
        health_path.exists(),
        "bootstrap index must establish a baseline"
    );

    // Force an obviously-stale bogus baseline, same trick as the test
    // above: any value a real rebuild writes back will differ from this.
    const FORCED_BASELINE: u64 = 999_999_999;
    std::fs::write(
        &health_path,
        format!(r#"{{"healthy_size_bytes": {FORCED_BASELINE}}}"#),
    )
    .unwrap();

    let status = std::process::Command::new(&cli)
        .arg("index")
        .arg("--full")
        .arg("--no-embed")
        .current_dir(dir.path())
        .env_remove("INFIGRAPH_BACKEND")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .status()
        .unwrap();
    assert!(status.success(), "full reindex failed");

    let refreshed =
        health_json_bytes(dir.path()).expect("baseline file must still exist after a full reindex");
    assert_ne!(
        refreshed, FORCED_BASELINE,
        "a full reindex must refresh the growth-ratio baseline to the newly-verified graph size"
    );
}
