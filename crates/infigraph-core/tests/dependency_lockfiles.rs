//! The dependency-lockfile skip at file discovery: `Infigraph::collect_files`
//! consults `graph::store_util::is_lockfile` alongside `registry.for_file`.
//!
//! Named `dependency_lockfiles` deliberately -- `tests/lockfile.rs` covers
//! this crate's advisory *graph* lock, an unrelated subject with a colliding
//! name.

use infigraph_core::Infigraph;

fn bundled() -> infigraph_core::lang::LanguageRegistry {
    infigraph_languages::bundled_registry().unwrap()
}

/// `pnpm-lock.yaml` and `package-lock.json` are claimed by the bundled YAML
/// and JSON packs, so the `registry.for_file` gate lets them straight through
/// -- that is how sittir accumulated 3,498 symbols from a single lockfile.
/// Discovery must drop them while still indexing ordinary source beside them.
///
/// The positive controls carry as much weight as the negative ones: a skip
/// test that passes because *nothing* got indexed would be worse than no test
/// at all. `settings.yaml` specifically pins that the skip is not simply
/// "every `.yaml`" -- and `.yaml` is certainly claimed, since this whole fix
/// exists because the YAML pack claims `pnpm-lock.yaml`.
#[test]
fn discovery_skips_dependency_lockfiles_but_keeps_real_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(root.join("main.py"), "def main():\n    return 1\n").unwrap();
    std::fs::write(
        root.join("settings.yaml"),
        "service:\n  name: web\n  replicas: 2\n",
    )
    .unwrap();
    std::fs::write(
        root.join("pnpm-lock.yaml"),
        "lockfileVersion: '9.0'\nimporters:\n  .:\n    dependencies:\n      left-pad:\n        version: 1.3.0\n",
    )
    .unwrap();
    std::fs::write(
        root.join("package-lock.json"),
        r#"{"name":"x","lockfileVersion":3,"packages":{"":{"dependencies":{"left-pad":"1.3.0"}}}}"#,
    )
    .unwrap();

    let mut ig = Infigraph::open(root, bundled()).unwrap();
    ig.init().unwrap();
    let result = ig.index().unwrap();

    let indexed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();

    assert!(
        indexed.iter().any(|f| f.ends_with("main.py")),
        "ordinary source must still be indexed -- otherwise this test proves \
         nothing about the skip, got: {indexed:?}"
    );
    assert!(
        indexed.iter().any(|f| f.ends_with("settings.yaml")),
        "a non-lockfile YAML must still be indexed: the skip must not swallow \
         every .yaml, got: {indexed:?}"
    );

    for lock in ["pnpm-lock.yaml", "package-lock.json"] {
        assert!(
            !indexed.iter().any(|f| f.ends_with(lock)),
            "{lock} must be skipped at discovery, got: {indexed:?}"
        );
    }
}
