//! Every command a test builds for the `infigraph` binary that runs `index`,
//! `rebuild` or `scip-enrich` turns SCIP enrichment off, through the crate's
//! helper, or sits in a file on `ALLOWED` with the reason it needs its own.
//!
//! Enrichment is on by default, and an index run that is not about enrichment
//! leaves a detached `scip-enrich` child behind when a real indexer is on PATH.
//! The switch is per command (an env var set on the `Command`), so the check is
//! per command too. Each crate's helper sets it:
//!
//! * the CLI's `infigraph()` and `run()` (`tests/support`),
//! * core's `cli_command()` (`tests/common/cli.rs`),
//! * the MCP's `disable_scip_enrichment()`, which sets it for the whole test
//!   process and so is inherited by every CLI child; a file-level check is the
//!   right grain there.
//!
//! The rule, per file:
//!
//! * In the CLI and core, a file that builds its own command for the binary
//!   (`CARGO_BIN_EXE_infigraph`, a binary run by path as `Command::new(bin)` or
//!   `Command::new(old_bin)`, or core's `cli_binary()` /
//!   `resolve_cli_binary_sibling_of`) and names an index-like command is a mixed
//!   file. It fails unless it is on `ALLOWED` with the reason. A file that goes
//!   only through the helper needs no entry: the helper is the one place such a
//!   command is built.
//! * In the MCP, a file that drives `index_project` (or names an index-like
//!   command) must call `disable_scip_enrichment()`, or be on `ALLOWED`.
//!
//! `ALLOWED` names the mixed files and says what each one needs its own
//! commands for. A second test fails when an entry goes stale.

use std::path::{Path, PathBuf};

use regex::Regex;

/// Files that build their own command for the binary and also run an index-like
/// command, or that drive the MCP's index tool without its helper. Each reason
/// says what the file needs the command it builds itself for.
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
    (
        "crates/infigraph-cli/tests/scip_routed_generation.rs",
        "mixed: its daemon and scip-enrich runs build their own commands with enrichment on \
         (the routed import is the thing under test); its bootstrap and index steps go through \
         run()",
    ),
    (
        "crates/infigraph-cli/tests/upgrade_smoke.rs",
        "mixed: its previous-release commands (`old_command`) run an old binary, which is \
         what the test is about, and set enrichment off by hand; its new-binary runs go \
         through run()",
    ),
    (
        "crates/infigraph-core/tests/daemon_kuzu_e2e.rs",
        "mixed: its index runs go through cli_command(); cli_binary() starts the daemon the \
         test drives, which is not an index run",
    ),
];

/// How a crate's tests prove enrichment is off.
enum Rule {
    /// Each command is built by a test: a file that builds one for the binary
    /// must not run an index-like command outside `ALLOWED`.
    PerCommand {
        crate_dir: &'static str,
        direct: &'static str,
    },
    /// The helper sets enrichment off for the whole process: a file that drives
    /// an index (the MCP's `tool_index_project`, or a CLI it resolves and runs
    /// with an index-like command) must reach the helper somewhere in its
    /// process, or be `ALLOWED`.
    PerProcess {
        crate_dir: &'static str,
        /// Every call that turns enrichment off for the process.
        helper: &'static str,
        /// A call that always runs an index.
        tool: &'static str,
        /// A call that resolves or spawns the CLI; an index only with an
        /// index-like command.
        cli: &'static str,
    },
}

const RULES: &[Rule] = &[
    Rule::PerCommand {
        crate_dir: "crates/infigraph-cli/tests",
        // The binary built by cargo, or an old release's binary run by path (the
        // upgrade test's `Command::new(old_bin)` and its `Command::new(bin)` probe).
        direct: r#"(?:CARGO_BIN_EXE_infigraph"|Command::new\((?:old_)?bin\))"#,
    },
    Rule::PerCommand {
        crate_dir: "crates/infigraph-core/tests",
        direct: r#"(?:CARGO_BIN_EXE_infigraph"|\bcli_binary\(\)|resolve_cli_binary_sibling_of)"#,
    },
    Rule::PerProcess {
        crate_dir: "crates/infigraph-mcp/tests",
        // `TestProject::new` / `with_files` call `disable_scip_enrichment`;
        // `disable_background_watchers` calls it too.
        helper: r"(?:\bdisable_scip_enrichment\(|\bdisable_background_watchers\(|\bTestProject::(?:new|with_files)\()",
        tool: r"\btool_index_project\(",
        cli: r#"(?:\bfind_infigraph_cli\(|CARGO_BIN_EXE_infigraph")"#,
    },
];

/// The string literals that name an index-like command, or the MCP tool that
/// runs one.
const INDEX_LIKE: &str = r#""(?:index|rebuild|scip-enrich|index_project)""#;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root")
}

/// Every `.rs` file under `dir`, skipping the shared helper directories, whose
/// own commands are the helpers and are not tests.
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

/// Whether one file's text breaks a rule.
type Violates = Box<dyn Fn(&str) -> bool>;

fn violations() -> Vec<String> {
    let root = workspace();
    let index_like = Regex::new(INDEX_LIKE).unwrap();
    let mut found = Vec::new();
    for rule in RULES {
        let (crate_dir, violates): (&str, Violates) = match rule {
            Rule::PerCommand { crate_dir, direct } => {
                let direct = Regex::new(direct).unwrap();
                let index_like = index_like.clone();
                (
                    crate_dir,
                    Box::new(move |text: &str| direct.is_match(text) && index_like.is_match(text)),
                )
            }
            Rule::PerProcess {
                crate_dir,
                helper,
                tool,
                cli,
            } => {
                let helper = Regex::new(helper).unwrap();
                let tool = Regex::new(tool).unwrap();
                let cli = Regex::new(cli).unwrap();
                let index_like = index_like.clone();
                (
                    crate_dir,
                    Box::new(move |text: &str| {
                        let runs_index = tool.is_match(text)
                            || (cli.is_match(text) && index_like.is_match(text));
                        runs_index && !helper.is_match(text)
                    }),
                )
            }
        };
        let mut files = Vec::new();
        test_sources(&root.join(crate_dir), &mut files);
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
            if violates(&text) && !ALLOWED.iter().any(|(path, _)| *path == rel) {
                found.push(rel);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn every_index_command_turns_enrichment_off_or_says_why_not() {
    let found = violations();
    assert!(
        found.is_empty(),
        "these test files build a command for the binary that runs an index-like command, or \
         drive the MCP's index tool, without the crate's enrichment-off helper. Route the command \
         through the helper (infigraph() / run() in the CLI, cli_command() in core, \
         disable_scip_enrichment() in the MCP), or add the file to ALLOWED with the reason it \
         needs its own commands:\n  {}",
        found.join("\n  ")
    );
}

#[test]
fn every_allowed_entry_still_exists_and_still_needs_its_exception() {
    let root = workspace();
    let index_like = Regex::new(INDEX_LIKE).unwrap();
    for (path, reason) in ALLOWED {
        assert!(!reason.is_empty(), "{path} has no reason");
        let full = root.join(path);
        assert!(
            full.exists(),
            "ALLOWED names {path}, which no longer exists"
        );
        let text = std::fs::read_to_string(&full).unwrap();
        let rule = RULES
            .iter()
            .find(|rule| match rule {
                Rule::PerCommand { crate_dir, .. } | Rule::PerProcess { crate_dir, .. } => {
                    path.starts_with(crate_dir)
                }
            })
            .expect("every allowed file belongs to a crate with a rule");
        let needed = match rule {
            Rule::PerCommand { direct, .. } => Regex::new(direct).unwrap().is_match(&text),
            Rule::PerProcess { .. } => true,
        };
        assert!(
            needed && index_like.is_match(&text),
            "ALLOWED names {path}, which no longer builds its own command for the binary with an \
             index-like command; remove it"
        );
    }
}
