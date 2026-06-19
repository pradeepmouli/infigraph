use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use std::path::PathBuf;

#[test]
fn test_infigraph_open() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let tg = Infigraph::open(tmp.path(), registry);
    assert!(tg.is_ok());
}

#[test]
fn test_infigraph_root() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let tg = Infigraph::open(tmp.path(), registry).unwrap();
    assert_eq!(tg.root(), tmp.path().canonicalize().unwrap());
}

#[test]
fn test_infigraph_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let tg = Infigraph::open(tmp.path(), registry).unwrap();
    let reg = tg.registry();
    assert!(reg.for_extension(".py").is_some());
}

#[test]
fn test_infigraph_store_none_before_init() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let tg = Infigraph::open(tmp.path(), registry).unwrap();
    assert!(tg.store().is_none());
}

#[test]
fn test_infigraph_init_creates_store() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(tmp.path(), registry).unwrap();
    tg.init().unwrap();
    assert!(tg.store().is_some());
}

#[test]
fn test_infigraph_stats_after_init() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(tmp.path(), registry).unwrap();
    tg.init().unwrap();
    let stats = tg.stats().unwrap();
    assert_eq!(stats.symbols, 0);
    assert_eq!(stats.files, 0);
}

#[test]
fn test_infigraph_index_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(tmp.path(), registry).unwrap();
    tg.init().unwrap();
    let result = tg.index().unwrap();
    assert_eq!(result.indexed_files, 0);
}

#[test]
fn test_infigraph_index_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(
        root.join("hello.py"),
        "def greet(name):\n    return f'Hello {name}'\n",
    )
    .unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(&root, registry).unwrap();
    tg.init().unwrap();
    tg.index_file(&root.join("hello.py")).unwrap();
    let stats = tg.stats().unwrap();
    assert!(
        stats.symbols >= 1,
        "should have at least 1 symbol, got {}",
        stats.symbols
    );
}

#[test]
fn test_infigraph_index_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("a.py"), "def foo(): pass\n").unwrap();
    std::fs::write(root.join("b.py"), "def bar(): pass\n").unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(&root, registry).unwrap();
    tg.init().unwrap();
    let paths = vec![root.join("a.py"), root.join("b.py")];
    let result = tg.index_files(&paths).unwrap();
    assert_eq!(result.indexed_files, 2);
}

#[test]
fn test_infigraph_remove_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("a.py"), "def foo(): pass\n").unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(&root, registry).unwrap();
    tg.init().unwrap();
    tg.index_file(&root.join("a.py")).unwrap();
    let stats_before = tg.stats().unwrap();
    assert!(stats_before.symbols >= 1);

    tg.remove_file(&PathBuf::from("a.py")).unwrap();
    let stats_after = tg.stats().unwrap();
    assert_eq!(
        stats_after.symbols, 0,
        "symbols should be 0 after remove_file"
    );
}

#[test]
fn test_infigraph_detect_bridges_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("clean.py"), "def hello(): pass\n").unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(&root, registry).unwrap();
    tg.init().unwrap();
    tg.index().unwrap();
    let bridges = tg.detect_bridges().unwrap();
    assert_eq!(bridges.bridges.len(), 0);
}

#[test]
fn test_infigraph_index_incremental_skips_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(root.join("a.py"), "def foo(): pass\n").unwrap();
    let registry = bundled_registry().unwrap();
    let mut tg = Infigraph::open(&root, registry).unwrap();
    tg.init().unwrap();

    let r1 = tg.index().unwrap();
    assert_eq!(r1.indexed_files, 1);

    let r2 = tg.index().unwrap();
    assert_eq!(r2.indexed_files, 0, "unchanged file should be skipped");
}
