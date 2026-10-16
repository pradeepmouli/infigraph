//! The rule tables keyed by extension -- security, bridges -- name `js` and
//! `ts`; a `.mts`, `.cts` or `.cjs` file is JavaScript or TypeScript to them
//! (see `lang::extension_selected`).

fn write(root: &std::path::Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

/// The extension lists of the security and bridge rules named `js` and `ts`
/// literally; a file of a variant was never scanned.
#[test]
fn security_rules_scan_the_module_system_variants() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.mts", "b.cts", "c.cjs"] {
        write(dir.path(), name, "function f(x) {\n  return eval(x);\n}\n");
    }
    let stats = infigraph_core::security::scan_project(dir.path()).unwrap();
    let flagged: std::collections::BTreeSet<&str> = stats
        .findings
        .iter()
        .filter(|f| f.rule_id == "SEC021")
        .map(|f| f.file.as_str())
        .collect();
    assert_eq!(
        flagged.into_iter().collect::<Vec<_>>(),
        ["a.mts", "b.cts", "c.cjs"],
        "the eval rule must fire in every variant"
    );
}

#[test]
fn bridge_rules_match_the_module_system_variants() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.mts", "b.cts", "c.cjs"] {
        write(
            dir.path(),
            name,
            "const m = WebAssembly.instantiate(bytes);\n",
        );
    }
    let found = infigraph_core::bridges::detect_bridges(dir.path()).unwrap();
    let files: std::collections::BTreeSet<&str> =
        found.bridges.iter().map(|b| b.file.as_str()).collect();
    assert_eq!(
        files.into_iter().collect::<Vec<_>>(),
        ["a.mts", "b.cts", "c.cjs"],
        "the WebAssembly bridge rule must match every variant"
    );
}
