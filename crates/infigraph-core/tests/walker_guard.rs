//! Every walk of a project's source tree goes through `ignore_rules`.
//!
//! `walk_builder(root, Domain)` and `IgnoreMatcher::build(root, Domain)` are
//! the one place that knows what is in a project: the safety list,
//! `.gitignore`, `.infigraphignore`, `[index] include` and `[ignore]
//! <domain>`. A walker that recurses with its own `read_dir` and its own
//! skip list is how seven cross-service scans, the manifest, bridge, config
//! and diff scans, an analysis tool and the `agent` language census each
//! came to disagree with the code graph about what a project contains.
//!
//! This test reads the workspace's non-test source and fails on
//!
//! * (a) a `WalkDir`, `walkdir::`, `WalkBuilder::new` or `ignore::Walk`
//!   outside `ignore_rules.rs`, and
//! * (b) a function that calls `read_dir(` and recurses -- calls itself, or
//!   pops a work stack in a loop -- unless it is a fixed `.infigraph/`
//!   internal join (recognised, not listed).
//!
//! What remains is two allow-lists of (file, function, reason). `ALLOWED`
//! names a walk that is not a project's source; `NAMED_INPUT` names a
//! function that reads a directory its caller named (ignore rules apply to
//! *discovery*, not to an input the caller pointed at). A new entry is a
//! statement about what the function reads, and the reason has to say what.

use std::path::{Path, PathBuf};

use regex::Regex;

/// Walks that are deliberately not the project's source tree.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "crates/infigraph-cli/src/artifacts/discovery.rs",
        "walk_files",
        "installed agent artifacts under the tool's own config directories (~/.claude, ...), \
         not a project tree",
    ),
    (
        "crates/infigraph-cli/src/bin/cozo_vs_kuzu.rs",
        "collect_files",
        "benchmark binary: walks the directory named on its command line, unfiltered on purpose",
    ),
    (
        "crates/infigraph-cli/src/bin/cozo_vs_kuzu.rs",
        "dir_size",
        "benchmark binary: sizes a database directory",
    ),
    (
        "crates/infigraph-core/src/clone.rs",
        "copy_dir_excluding",
        "deliberately unfiltered: copies a project's own `.infigraph/` directory, excluding \
         only derived paths by name",
    ),
    (
        "crates/infigraph-core/src/doctor.rs",
        "dir_size",
        "deliberately unfiltered: totals the bytes under a directory `doctor` reports on, \
         ignored files included",
    ),
    (
        "crates/infigraph-core/src/quarantine.rs",
        "entry_size_bytes",
        "deliberately unfiltered: sizes a quarantined `.infigraph/` entry",
    ),
    (
        "crates/infigraph-core/src/snapshot.rs",
        "copy_entry_recursive",
        "deliberately unfiltered: copies `.infigraph/` state into a snapshot",
    ),
    (
        "crates/lsp-to-scip/src/main.rs",
        "collect_files_rec",
        "standalone helper binary, no infigraph-core dependency; its .scip output is filtered \
         on import (#114)",
    ),
];

/// Functions that read a directory (or fixed paths) the caller named. They are
/// not recursive, so the walk check never sees them; listing them records the
/// ruling that ignore rules do not apply to a named input, and fails the day one
/// of them starts discovering files below its input.
const NAMED_INPUT: &[(&str, &str, &str)] = &[
    (
        "crates/infigraph-core/src/structured/ingest.rs",
        "ingest_directory",
        "named input: the caller named this directory",
    ),
    (
        "crates/infigraph-core/src/structured/cozo.rs",
        "ingest_directory_cozo",
        "named input: the caller named this directory",
    ),
    (
        "crates/infigraph-core/src/graph/neo4j_backend.rs",
        "ingest_structured_directory",
        "named input: the caller named this directory",
    ),
    (
        "crates/infigraph-core/src/reflection/mod.rs",
        "scan_config_files",
        "named input: reads fixed, well-known config paths and one fixed `META-INF/services` \
         directory; nothing is discovered",
    ),
];

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    file: String,
    function: String,
}

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The file without its trailing `#[cfg(test)] mod ...`, where this crate
/// keeps its unit tests.
fn production_source(source: &str) -> &str {
    let marker =
        Regex::new(r"\n#\[cfg\(test\)\]\s*\n\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+").unwrap();
    match marker.find(source) {
        Some(m) => &source[..m.start()],
        None => source,
    }
}

fn fn_re() -> Regex {
    Regex::new(
        r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:const[ \t]+)?(?:async[ \t]+)?(?:unsafe[ \t]+)?fn[ \t]+(\w+)",
    )
    .unwrap()
}

/// `(name, body)` of every function; a body runs to the next `fn` header.
fn functions(source: &str) -> Vec<(String, &str)> {
    let starts: Vec<(usize, String)> = fn_re()
        .captures_iter(source)
        .map(|c| (c.get(0).unwrap().start(), c[1].to_string()))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(i, (start, name))| {
            let end = starts.get(i + 1).map_or(source.len(), |next| next.0);
            (name.clone(), &source[*start..end])
        })
        .collect()
}

fn hits_in(rel: &str, source: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    if !rel.ends_with("/ignore_rules.rs") {
        for needle in ["WalkDir", "walkdir::", "WalkBuilder::new", "ignore::Walk"] {
            if source.contains(needle) {
                hits.push(Hit {
                    file: rel.to_string(),
                    function: format!("<{needle}>"),
                });
            }
        }
    }

    let pop_loop =
        Regex::new(r"while let Some\(\w+\)\s*=\s*\w+\.(pop|pop_front|pop_back)\(").unwrap();
    // A fixed join under the project's own store, as opposed to a skip list
    // that merely names `.infigraph`.
    let internal = Regex::new(r#"join\("\.infigraph"\)|\binfigraph_dir\b"#).unwrap();

    for (name, body) in functions(source) {
        if !body.contains("read_dir(") {
            continue;
        }
        let self_call = Regex::new(&format!(r"\b{}\s*\(", regex::escape(&name)))
            .unwrap()
            .find_iter(body)
            .count()
            >= 2;
        let recurses = self_call || pop_loop.is_match(body);
        if recurses && !internal.is_match(body) {
            hits.push(Hit {
                file: rel.to_string(),
                function: name,
            });
        }
    }
    hits
}

/// Every function in `source` that calls `read_dir(`, recursive or not.
fn read_dir_callers(source: &str) -> Vec<String> {
    functions(source)
        .into_iter()
        .filter(|(_, body)| body.contains("read_dir("))
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn every_source_tree_walk_goes_through_ignore_rules() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let mut files = Vec::new();
    let crates = workspace.join("crates");
    for entry in std::fs::read_dir(&crates).unwrap().flatten() {
        source_files(&entry.path().join("src"), &mut files);
    }
    assert!(
        files.len() > 100,
        "the scan found only {} files",
        files.len()
    );

    let mut found = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&workspace)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let source = std::fs::read_to_string(path).unwrap();
        found.extend(hits_in(&rel, production_source(&source)));
    }
    found.sort();

    let allowed: Vec<Hit> = ALLOWED
        .iter()
        .map(|(file, function, _)| Hit {
            file: file.to_string(),
            function: function.to_string(),
        })
        .collect();

    let unlisted: Vec<&Hit> = found.iter().filter(|h| !allowed.contains(h)).collect();
    assert!(
        unlisted.is_empty(),
        "a project-tree walk outside `ignore_rules` -- route it through \
         `ignore_rules::walk_builder(root, Domain::Code | Domain::Docs)` (or \
         `IgnoreMatcher::build` for point queries), or, if it does not walk a project's \
         source, add it to ALLOWED with the reason: {unlisted:#?}"
    );
    let stale: Vec<&Hit> = allowed.iter().filter(|h| !found.contains(h)).collect();
    assert!(
        stale.is_empty(),
        "ALLOWED lists a walk that is no longer there; delete the entry: {stale:#?}"
    );
}

#[test]
fn named_input_readers_still_exist_and_do_not_recurse() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    for (file, function, reason) in NAMED_INPUT {
        assert!(!reason.is_empty(), "{file}::{function} needs a reason");
        let source = std::fs::read_to_string(workspace.join(file))
            .unwrap_or_else(|e| panic!("NAMED_INPUT names {file}, which cannot be read: {e}"));
        let source = production_source(&source);
        assert!(
            read_dir_callers(source).iter().any(|f| f == function),
            "NAMED_INPUT lists {file}::{function}, which no longer calls `read_dir(`; \
             delete the entry"
        );
        let recursing = hits_in(file, source)
            .into_iter()
            .any(|h| h.function == *function);
        assert!(
            !recursing,
            "{file}::{function} now recurses below a named directory: that is discovery, so \
             it goes through `ignore_rules::walk_builder`"
        );
    }
}

#[test]
fn the_guard_flags_a_hand_rolled_recursion_and_a_fixed_store_join_is_exempt() {
    let walker = "fn walk(dir: &Path) {\n    for e in std::fs::read_dir(dir).unwrap() {\n        walk(&e.unwrap().path());\n    }\n}\n";
    assert_eq!(hits_in("crates/x/src/a.rs", walker).len(), 1);

    let stack = "fn all(root: &Path) {\n    let mut s = vec![root.to_path_buf()];\n    while let Some(d) = s.pop() {\n        for e in std::fs::read_dir(d).unwrap() { s.push(e.unwrap().path()); }\n    }\n}\n";
    assert_eq!(hits_in("crates/x/src/a.rs", stack).len(), 1);

    let store = "fn sweep(root: &Path) {\n    let d = root.join(\".infigraph\");\n    for e in std::fs::read_dir(&d).unwrap() { sweep(&e.unwrap().path()); }\n}\n";
    assert!(hits_in("crates/x/src/a.rs", store).is_empty());

    let direct = "use ignore::WalkBuilder;\nfn f() { let _ = WalkBuilder::new(\".\"); }\n";
    assert!(!hits_in("crates/x/src/a.rs", direct).is_empty());
    assert!(hits_in("crates/infigraph-core/src/ignore_rules.rs", direct).is_empty());

    let single_level =
        "fn list(dir: &Path) { for e in std::fs::read_dir(dir).unwrap() { let _ = e; } }\n";
    assert!(hits_in("crates/x/src/a.rs", single_level).is_empty());
}
