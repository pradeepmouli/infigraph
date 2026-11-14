//! Every test that runs an `infigraph` index, rebuild or scip-enrich turns SCIP
//! enrichment off through its crate's one helper, unless it is here for a reason.
//!
//! Enrichment is on by default, and an index run that is not about enrichment
//! leaves a detached `scip-enrich` child behind with a real indexer on PATH: it
//! writes into a tempdir the test is about to delete, holds `index.lock`, and
//! spends minutes of CPU. Each crate's tests have one helper that turns it off
//! (`infigraph()` / `run()` in the CLI's support, `cli_command()` in core's
//! `tests/common/cli.rs`, `disable_scip_enrichment()` in the MCP's support).
//!
//! This test reads each crate's test sources and fails on a file that
//!
//! * spawns the binary (the CLI, core's cli binary, or the MCP server, which
//!   runs `index_project` through the CLI as a child),
//! * names an index-like command (`"index"`, `"rebuild"`, `"scip-enrich"`,
//!   `"index_project"`), and
//! * does not call its crate's helper,
//!
//! unless it is on `ALLOWED` with the reason it must keep enrichment on. A new
//! test file is covered the day it is written; no count goes stale.

use std::path::{Path, PathBuf};

use regex::Regex;

/// Files that run an index-like command with enrichment ON, and why. Each
/// reason says what the test asserts about enrichment, or why a real indexer
/// cannot start from it.
const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/infigraph-cli/tests/scip_enrich_lock.rs",
        "asserts the one-enrichment-at-a-time lock: it needs the enrichment a fake indexer runs",
    ),
    (
        "crates/infigraph-cli/tests/scip_index_rule.rs",
        "asserts when `index` starts an enrichment (full, never enriched, or neither)",
    ),
    (
        "crates/infigraph-cli/tests/last_run_scip.rs",
        "asserts a refused automatic enrichment is recorded with its reason",
    ),
    (
        "crates/infigraph-cli/tests/scip_empty_settles.rs",
        "asserts an empty index settles its ledger entry on each local enrichment path",
    ),
    (
        "crates/infigraph-cli/tests/scip_enrich_teardown.rs",
        "asserts enrichment's own teardown: the indexer and scip-enrich are reaped",
    ),
];

/// Helper calls that turn enrichment off, per crate, and the spawns and index
/// tokens that make a file in that crate subject to the rule.
struct Rule {
    crate_dir: &'static str,
    helper: &'static str,
    spawn: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        crate_dir: "crates/infigraph-cli/tests",
        helper: r"(?:\binfigraph\(\)|(?:^|[^A-Za-z0-9_.])run\(|\bstart_daemon\()",
        spawn: r#"CARGO_BIN_EXE_infigraph""#,
    },
    Rule {
        crate_dir: "crates/infigraph-core/tests",
        helper: r"\bcli_command\(\)",
        spawn: r#"(?:CARGO_BIN_EXE_infigraph"|cli_binary\(\)|resolve_cli_binary_sibling_of)"#,
    },
    Rule {
        crate_dir: "crates/infigraph-mcp/tests",
        helper: r"\bdisable_scip_enrichment\(\)",
        spawn: r#"(?:CARGO_BIN_EXE_infigraph-mcp"|isolated_command_for\()"#,
    },
];

/// The string literals that name an index-like command.
const INDEX_LIKE: &str = r#""(?:index|rebuild|scip-enrich|index_project)""#;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root")
}

/// Every `.rs` file under `dir`, skipping the shared helper directories, whose
/// own spawns are the helpers and are not tests.
fn test_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            if name != "support" && name != "common" {
                test_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn violations() -> Vec<String> {
    let root = workspace();
    let index_like = Regex::new(INDEX_LIKE).unwrap();
    let mut found = Vec::new();
    for rule in RULES {
        let helper = Regex::new(rule.helper).unwrap();
        let spawn = Regex::new(rule.spawn).unwrap();
        let mut files = Vec::new();
        test_sources(&root.join(rule.crate_dir), &mut files);
        for file in files {
            let rel = file
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.ends_with("enrichment_guard.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let runs_index = spawn.is_match(&text) && index_like.is_match(&text);
            if !runs_index || helper.is_match(&text) {
                continue;
            }
            if ALLOWED.iter().any(|(path, _)| *path == rel) {
                continue;
            }
            found.push(rel);
        }
    }
    found.sort();
    found
}

#[test]
fn every_index_run_turns_enrichment_off_or_says_why_not() {
    let found = violations();
    assert!(
        found.is_empty(),
        "these test files run an index-like command with SCIP enrichment on. Turn it off \
         through the crate's helper (infigraph() / run() in the CLI, cli_command() in core, \
         disable_scip_enrichment() in the MCP), or add the file to ALLOWED with the reason \
         it must keep enrichment on:\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn every_allowed_entry_still_exists_and_still_needs_its_exception() {
    let root = workspace();
    for (path, reason) in ALLOWED {
        assert!(!reason.is_empty(), "{path} has no reason");
        assert!(
            root.join(path).exists(),
            "ALLOWED names {path}, which no longer exists"
        );
    }
    // Every allowed file runs an index-like command with the spawn: otherwise
    // the exception is stale and should go.
    let index_like = Regex::new(INDEX_LIKE).unwrap();
    for (path, _) in ALLOWED {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let rule = RULES
            .iter()
            .find(|r| path.starts_with(r.crate_dir))
            .expect("every allowed file belongs to a crate with a rule");
        let spawn = Regex::new(rule.spawn).unwrap();
        assert!(
            spawn.is_match(&text) && index_like.is_match(&text),
            "ALLOWED names {path}, which no longer runs an index-like command with the binary; \
             remove it"
        );
    }
}
