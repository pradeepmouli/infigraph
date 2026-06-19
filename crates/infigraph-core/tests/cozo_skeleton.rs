use tempfile::TempDir;
use infigraph_core::graph::CozoStore;

fn make_cozo() -> (TempDir, CozoStore) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test_cozo.db");
    let store = CozoStore::open(&db_path).unwrap();
    (dir, store)
}

fn insert_symbol(store: &CozoStore, id: &str, name: &str, kind: &str, file: &str, line: i64, complexity: i64, params: &str, ret: &str, vis: &str, parent: &str) {
    store.import_symbols(&[(
        id.into(), name.into(), kind.into(), file.into(), line, line + 10,
        "".into(), "python".into(), vis.into(), parent.into(), "".into(),
        complexity, params.into(), ret.into(),
    )]).unwrap();
}

fn insert_file(store: &CozoStore, file: &str) {
    store.import_files(&[(file.into(), file.into(), file.into(), "python".into(), 0)]).unwrap();
}

fn insert_defines(store: &CozoStore, file: &str, sym_id: &str) {
    store.import_edges("defines", &[(file.into(), sym_id.into())]).unwrap();
}

fn insert_calls(store: &CozoStore, caller: &str, callee: &str) {
    store.import_edges("calls", &[(caller.into(), callee.into())]).unwrap();
}

fn insert_statement(store: &CozoStore, sym_id: &str, stmt_id: &str, depth: i64) {
    store.import_statements(&[(
        stmt_id.into(), "if".into(), "".into(), 1, 2, depth, sym_id.into(),
    )]).unwrap();
    store.import_edges("has_statement", &[(sym_id.into(), stmt_id.into())]).unwrap();
}

#[test]
fn test_cozo_skeleton_output_format() {
    let (_dir, store) = make_cozo();
    let file = "src/utils.py";
    insert_file(&store, file);
    insert_symbol(&store, "s1", "calculate", "Function", file, 10, 5, "(x, y)", "int", "public", "");
    insert_defines(&store, file, "s1");

    let result = store.skeleton(file).unwrap();
    assert!(result.contains("# src/utils.py"), "should contain file header");
    assert!(result.contains("calculate(x, y) -> int"), "should contain function signature");
    assert!(result.contains("complexity: 5"), "should show complexity");
}

#[test]
fn test_cozo_skeleton_nesting() {
    let (_dir, store) = make_cozo();
    let file = "src/deep.py";
    insert_file(&store, file);
    insert_symbol(&store, "s1", "nested_fn", "Function", file, 1, 1, "()", "", "", "");
    insert_defines(&store, file, "s1");
    insert_statement(&store, "s1", "st1", 1);
    insert_statement(&store, "s1", "st2", 3);
    insert_statement(&store, "s1", "st3", 2);

    let result = store.skeleton(file).unwrap();
    assert!(result.contains("nesting: 3"), "should show max nesting depth");
    assert!(result.contains("stmts: 3"), "should count all statements");
}

#[test]
fn test_cozo_skeleton_fan_in() {
    let (_dir, store) = make_cozo();
    let file = "src/target.py";
    insert_file(&store, file);
    insert_symbol(&store, "target", "do_work", "Function", file, 1, 1, "()", "", "", "");
    insert_symbol(&store, "caller1", "a", "Function", file, 20, 1, "()", "", "", "");
    insert_symbol(&store, "caller2", "b", "Function", file, 30, 1, "()", "", "", "");
    insert_defines(&store, file, "target");
    insert_defines(&store, file, "caller1");
    insert_defines(&store, file, "caller2");
    insert_calls(&store, "caller1", "target");
    insert_calls(&store, "caller2", "target");

    let result = store.skeleton(file).unwrap();
    assert!(result.contains("fan-in: 2"), "should show 2 callers for do_work");
}

#[test]
fn test_cozo_skeleton_class_members_indented() {
    let (_dir, store) = make_cozo();
    let file = "src/cls.py";
    insert_file(&store, file);
    insert_symbol(&store, "c1", "MyClass", "Class", file, 1, 1, "", "", "", "");
    insert_symbol(&store, "m1", "my_method", "Method", file, 5, 2, "(self)", "None", "", "c1");
    insert_defines(&store, file, "c1");
    insert_defines(&store, file, "m1");

    let result = store.skeleton(file).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    let class_line = lines.iter().find(|l| l.contains("MyClass")).unwrap();
    let method_line = lines.iter().find(|l| l.contains("my_method")).unwrap();

    fn content_after_colon(line: &str) -> &str {
        line.split_once(": ").map(|(_, c)| c).unwrap_or(line)
    }

    let class_indent = content_after_colon(class_line).len() - content_after_colon(class_line).trim_start().len();
    let method_indent = content_after_colon(method_line).len() - content_after_colon(method_line).trim_start().len();
    assert!(method_indent > class_indent, "method should be more indented than class");
}

#[test]
fn test_cozo_skeleton_no_annotations_on_class() {
    let (_dir, store) = make_cozo();
    let file = "src/cls2.py";
    insert_file(&store, file);
    insert_symbol(&store, "c1", "MyClass", "Class", file, 1, 1, "", "", "", "");
    insert_defines(&store, file, "c1");

    let result = store.skeleton(file).unwrap();
    let lines: Vec<&str> = result.lines().collect();
    let class_idx = lines.iter().position(|l| l.contains("MyClass")).unwrap();
    if let Some(next_line) = lines.get(class_idx + 1) {
        assert!(!next_line.contains("complexity:"), "class should not have annotation line");
    }
}

#[test]
fn test_cozo_skeleton_empty_file() {
    let (_dir, store) = make_cozo();
    let result = store.skeleton("nonexistent.py").unwrap();
    assert!(result.contains("No symbols found"), "should show 'no symbols' for empty file");
}

#[test]
fn test_cozo_skeleton_visibility_prefix() {
    let (_dir, store) = make_cozo();
    let file = "src/priv.py";
    insert_file(&store, file);
    insert_symbol(&store, "p1", "_private_fn", "Function", file, 1, 1, "()", "", "private", "");
    insert_defines(&store, file, "p1");

    let result = store.skeleton(file).unwrap();
    assert!(result.contains("private _private_fn"), "should show visibility prefix");
}
