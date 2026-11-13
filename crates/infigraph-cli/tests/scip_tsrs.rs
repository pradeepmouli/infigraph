//! Exercise the public native CLI and SCIP import with a real daemon. The
//! producer is optional on CI; release/local validation installs scip-tsrs.
mod support;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

fn write(root: &Path, name: &str, text: &str) {
    std::fs::write(root.join(name), text).unwrap();
}

#[test]
fn native_project_graph_preserves_interface_members_and_document_local_variables() {
    if !Command::new("scip-tsrs")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        eprintln!("scip-tsrs is not installed; native CLI integration skipped");
        return;
    }
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = project.path();
    std::fs::create_dir(root.join("lib")).unwrap();
    std::fs::create_dir(root.join("app")).unwrap();
    write(
        root,
        "package.json",
        r#"{"name":"native-integration","version":"1.0.0"}"#,
    );
    write(
        root,
        "tsconfig.json",
        r#"{"files":[],"references":[{"path":"./lib"},{"path":"./app"}]}"#,
    );
    write(
        root,
        "lib/tsconfig.json",
        r#"{"compilerOptions":{"types":[]},"files":["api.ts"]}"#,
    );
    write(
        root,
        "app/tsconfig.json",
        r#"{"compilerOptions":{"types":[]},"files":["main.ts"],"references":[{"path":"../lib"}]}"#,
    );
    write(root, "lib/api.ts", "export interface Root { root(): number }\nexport interface Contract extends Root { run(): number }\nexport class Parent { run() { return 1; } }\n/** Counts API values.\n * @param {number} seed - Starting value.\n * @returns {number} The current count.\n * @example\n * ```ts\n * apiCount(3);\n * ```\n */\nexport function apiCount(seed = 3) { const count = seed; return count; }\n");
    write(root, "app/main.ts", "import { Parent, Contract } from '../lib/api';\nexport class Widget extends Parent implements Contract {\n  run() { return 2; }\n  root() { return 3; }\n}\nexport class LikeParent implements Parent { run() { return 7; } }\nexport function one() { const count = 1; return count; }\nexport function two() { const count = 2; return count; }\nexport function unicode() { const prefix = 'é😀'; return { key: prefix }; }\n");
    let _daemon = support::start_daemon(root, home.path());
    let index = root.join("native.scip");
    for round in 0..2 {
        let generated = Command::new("scip-tsrs")
            .args(["index", ".", "--output"])
            .arg(&index)
            .current_dir(root)
            .output()
            .unwrap();
        support::assert_ok(&generated, "generate native project-graph index");
        let imported = support::run(
            root,
            home.path(),
            infigraph_core::DAEMON_BACKEND,
            &["scip-import", "--index", index.to_str().unwrap()],
        );
        support::assert_ok(&imported, "import native SCIP through daemon");
        if round == 1 {
            assert!(support::stdout(&imported).contains("symbols added: 0"));
            assert!(support::stdout(&imported).contains("relations added: 0"));
            assert!(support::stdout(&imported).contains("references added: 0"));
        }
    }
    let query = |cypher: &str| {
        let out = support::run(
            root,
            home.path(),
            infigraph_core::DAEMON_BACKEND,
            &["query", cypher],
        );
        support::assert_ok(&out, "query native graph");
        support::stdout(&out)
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    };
    let implementations = query("MATCH (a:Symbol)-[:IMPLEMENTS]->(b:Symbol) RETURN a.id, b.id");
    assert_eq!(
        implementations,
        [
            "app/main.ts::LikeParent | lib/api.ts::Parent",
            "app/main.ts::LikeParent::run | lib/api.ts::Parent::run",
            "app/main.ts::Widget | lib/api.ts::Contract",
            "app/main.ts::Widget::root | lib/api.ts::Root::root",
            "app/main.ts::Widget::run | lib/api.ts::Contract::run",
            "app/main.ts::Widget::run | lib/api.ts::Parent::run",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert!(
        !query("MATCH (a:Symbol)-[:INHERITS]->(b:Symbol) RETURN a.id, b.id")
            .iter()
            .any(|edge| edge.contains("LikeParent"))
    );
    let locals = query(
        "MATCH (s:Symbol) WHERE s.name = 'count' RETURN s.id, s.parent, s.visibility, s.language",
    );
    assert_eq!(
        locals,
        [
            "app/main.ts::one::count | app/main.ts::one | private | typescript",
            "app/main.ts::two::count | app/main.ts::two | private | typescript",
            "lib/api.ts::apiCount::count | lib/api.ts::apiCount | private | typescript",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert_eq!(
        query("MATCH (s:Symbol) WHERE s.name STARTS WITH 'key' RETURN s.name"),
        BTreeSet::from(["key".to_owned()])
    );
    let docs = support::run(
        root,
        home.path(),
        infigraph_core::DAEMON_BACKEND,
        &[
            "query",
            "MATCH (s:Symbol {id: 'lib/api.ts::apiCount'}) RETURN s.docstring",
        ],
    );
    support::assert_ok(&docs, "read imported JSDoc");
    let docs = support::stdout(&docs);
    for expected in [
        "Counts API values.",
        "**@param**",
        "Starting value.",
        "**@returns**",
        "The current count.",
        "**@example**",
        "apiCount(3);",
    ] {
        assert!(
            docs.contains(expected),
            "missing {expected:?} in imported documentation: {docs}"
        );
    }
}
