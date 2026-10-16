//! `.mts` and `.cts` are TypeScript, `.cjs` is JavaScript: the same grammars as
//! `.ts` and `.js`, differing only in which module system they declare. A
//! project that uses them (sittir has 81 tracked `.mts` files) must index them
//! and every consumer keyed by extension must treat them as the language they
//! are.

use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

#[test]
fn the_registry_claims_the_module_system_variants() {
    let registry = bundled_registry().unwrap();
    for (ext, language) in [
        (".mts", "typescript"),
        (".cts", "typescript"),
        (".cjs", "javascript"),
    ] {
        let pack = registry
            .for_extension(ext)
            .unwrap_or_else(|| panic!("no language pack claims {ext}"));
        assert_eq!(pack.name, language, "{ext}");
    }
}

fn write(root: &std::path::Path, name: &str, body: &str) {
    std::fs::write(root.join(name), body).unwrap();
}

/// One file of each kind, an `.mts` calling into a `.ts`: the files are
/// discovered, their symbols are extracted, and the cross-file call resolves.
#[test]
fn a_project_of_module_system_files_is_indexed_and_resolves_across_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "lib.ts",
        "export function helper(): number {\n  return 1;\n}\n",
    );
    write(
        root,
        "app.mts",
        "import { helper } from \"./lib\";\nexport function runApp(): number {\n  return helper();\n}\n",
    );
    write(
        root,
        "tool.cts",
        "export function toolMain(): number {\n  return 2;\n}\n",
    );
    write(
        root,
        "legacy.cjs",
        "function legacyEntry() {\n  return 3;\n}\nmodule.exports = { legacyEntry };\n",
    );

    let mut ig = Infigraph::open(root, bundled_registry().unwrap()).unwrap();
    ig.init().unwrap();
    let result = ig.index().unwrap();

    let indexed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
    for file in ["lib.ts", "app.mts", "tool.cts", "legacy.cjs"] {
        assert!(
            indexed.iter().any(|f| f.ends_with(file)),
            "{file} was not indexed, got: {indexed:?}"
        );
    }

    let names: Vec<&str> = result
        .extractions
        .iter()
        .flat_map(|e| e.symbols.iter().map(|s| s.name.as_str()))
        .collect();
    for symbol in ["helper", "runApp", "toolMain", "legacyEntry"] {
        assert!(names.contains(&symbol), "no symbol {symbol}: {names:?}");
    }

    let rows = ig
        .backend()
        .unwrap()
        .raw_query("MATCH (a)-[r:CALLS]->(b) RETURN a.name, b.name")
        .unwrap();
    let calls: Vec<String> = rows
        .iter()
        .map(|row| format!("{} -> {}", row[0], row[1]))
        .collect();
    assert!(
        calls.iter().any(|c| c == "runApp -> helper"),
        "the .mts -> .ts call did not resolve: {calls:?}"
    );
}
