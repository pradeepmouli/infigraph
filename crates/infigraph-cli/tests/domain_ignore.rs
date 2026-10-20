//! #56: `[ignore] code` and `[ignore] docs` keep a file out of one index
//! and leave it in the other, through the real indexers rather than the
//! walkers alone.

use infigraph_languages::bundled_registry;

#[test]
fn a_code_pattern_drops_a_file_from_the_code_graph_and_not_from_the_docs_index() {
    // The two indexers read the same project, so there is no env to vary
    // between them; this process opts out of the daemon and out of the
    // developer's own user-level config.
    std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
    std::env::set_var("INFIGRAPH_NO_WATCH", "1");
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (path, content) in [
        (
            ".infigraph/config.toml",
            "[ignore]\ncode = [\"docs/\"]\ndocs = [\"*.draft.md\"]\n",
        ),
        ("docs/guide.md", "# Guide\n\nHow it works.\n"),
        ("docs/example.py", "def example():\n    pass\n"),
        ("src/app.py", "def app():\n    pass\n"),
        ("README.md", "# Readme\n\nHello.\n"),
        ("notes.draft.md", "# Draft\n\nNot ready.\n"),
    ] {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    let mut code = infigraph_core::CodeIndex::open(root, bundled_registry().unwrap()).unwrap();
    code.init().unwrap();
    let indexed = code.index().unwrap();
    let code_files: Vec<&str> = indexed
        .extractions
        .iter()
        .map(|e| e.file.as_str())
        .collect();
    assert!(code_files.contains(&"src/app.py"), "{code_files:?}");
    assert!(
        !code_files.iter().any(|f| f.starts_with("docs/")),
        "`[ignore] code = [\"docs/\"]` keeps docs/ out of the code graph: {code_files:?}"
    );

    let mut docs = infigraph_docs::DocIndex::open(root).unwrap();
    docs.init().unwrap();
    let documents = docs.index().unwrap();
    assert!(
        documents.changed_files.iter().any(|f| f == "docs/guide.md"),
        "a code-only pattern must not reach the docs index: {:?}",
        documents.changed_files
    );
    assert!(
        documents.changed_files.iter().any(|f| f == "README.md"),
        "{:?}",
        documents.changed_files
    );
    assert!(
        !documents
            .changed_files
            .iter()
            .any(|f| f == "notes.draft.md"),
        "`[ignore] docs` keeps the draft out of the docs index: {:?}",
        documents.changed_files
    );
}
