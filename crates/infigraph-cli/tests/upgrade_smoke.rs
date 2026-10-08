//! R8.5 (#88): the upgrade smoke test. A graph written by the previous
//! release must, when this build opens it, either still answer the same
//! queries or be refused cleanly -- never be wiped, quarantined, rebuilt
//! empty, or served back with fewer symbols than it had.
//!
//! The previous release is a binary the workflow builds from the newest tag
//! that is an ancestor of HEAD and hands over as `OLD_INFIGRAPH_BIN`. Plain
//! `cargo test --all` has no such binary, so the end-to-end test skips; the
//! workflow also sets `INFIGRAPH_REQUIRE_UPGRADE_TEST=1`, which turns a
//! missing or unrunnable binary into a failure, so a broken job cannot pass
//! by skipping.
//!
//! A wipe-and-rebuild-from-source would look like success on the symbol ids
//! alone: the fixture's source is still there to re-index. Two things tell it
//! apart. `judge` fails any open that printed `OPEN_FAILED_NOTICE`, the line
//! `init()` prints before it destroys a graph that will not open; and the old
//! side ingests one structured row (`infigraph ingest`, present since well
//! before v3.2.16) that no source file can regenerate, which must survive all
//! three passes.
//!
//! Which outcome the real pair of builds gives depends on their lbug
//! versions. While both read the same storage version (or the newer one reads
//! the older and upgrades it in place, as lbug 0.16 -> 0.20 does) the outcome
//! is `SameResults`. The day a tag's lbug cannot be read by HEAD's, the same
//! test exercises the refusal branch of `judge` with no change here.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use infigraph_core::graph::storage_version_mismatch_context;
use infigraph_core::OPEN_FAILED_NOTICE;

/// Path of the previous release's `infigraph`, set by the workflow.
const OLD_BIN_ENV: &str = "OLD_INFIGRAPH_BIN";
/// When set, a missing or unrunnable old binary is a failure, not a skip.
const REQUIRE_ENV: &str = "INFIGRAPH_REQUIRE_UPGRADE_TEST";
/// Printed by the end-to-end test; the workflow greps for it so a test that
/// was filtered out or skipped cannot count as a pass.
const RAN_MARKER: &str = "UPGRADE_SMOKE_RAN";
/// Every symbol id, in a fixed order: one id per line on stdout.
const SYMBOL_IDS: &str = "MATCH (s:Symbol) RETURN s.id ORDER BY s.id";
/// The structured row the old side ingests (below): it lives only in the graph.
const PROBE_ROWS: &str = "MATCH (p:UpgradeProbe) RETURN p.id, p.note ORDER BY p.id";
const PROBE_SCHEMA_ID: &str = "upgrade_probe";
const PROBE_SCHEMA: &str = "[schema]\nschema_id = \"upgrade_probe\"\nname = \"Upgrade probe\"\n\
                            node_table = \"UpgradeProbe\"\n\n\
                            [[schema.columns]]\nname = \"note\"\ncol_type = \"STRING\"\n";
const PROBE_DATA: &str = r#"[{"id": "probe-1", "note": "no source file can regenerate this"}]"#;

/// What a graph answers: every symbol id, and the probe rows.
#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    ids: Vec<String>,
    probe: Vec<String>,
}

/// What this build did when it opened the previous release's graph.
struct Observed {
    /// The opening command exited zero.
    succeeded: bool,
    /// What the graph answered afterwards (empty if nothing could be read).
    snapshot: Snapshot,
    /// stdout + stderr of the opening command.
    output: String,
    /// The graph file's bytes are identical to before the open.
    graph_unchanged: bool,
    /// A `graph.corrupt.*` entry exists: the graph was quarantined.
    quarantined: bool,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    SameResults,
    CleanRefusal,
}

/// The one place that decides which outcomes R8.1 allows.
fn judge(baseline: &Snapshot, seen: &Observed) -> Result<Outcome, String> {
    if seen.quarantined {
        return Err(
            "the graph was quarantined: an open must never treat a good graph as corrupt".into(),
        );
    }
    if seen.succeeded {
        // A wipe that re-indexed from source leaves the same symbols behind;
        // only its notice and the probe row give it away.
        if seen.output.contains(OPEN_FAILED_NOTICE) {
            return Err(format!(
                "the open succeeded only by destroying the graph and starting over:\n{}",
                seen.output
            ));
        }
        if seen.snapshot.ids != baseline.ids {
            return Err(format!(
                "the open succeeded but the symbols differ: {} before, {} after",
                baseline.ids.len(),
                seen.snapshot.ids.len()
            ));
        }
        if seen.snapshot.probe != baseline.probe {
            return Err(format!(
                "the open succeeded but the ingested row did not survive: {:?} before, {:?} after",
                baseline.probe, seen.snapshot.probe
            ));
        }
        return Ok(Outcome::SameResults);
    }
    // The text a refusal carries, minus the path it starts with.
    let refusal = storage_version_mismatch_context(Path::new(""));
    let refusal = refusal.trim_start();
    if !seen.output.contains(refusal) {
        return Err(format!(
            "the open failed, but not with the storage-version refusal:\n{}",
            seen.output
        ));
    }
    if !seen.graph_unchanged {
        return Err("the open refused but still changed the graph file".into());
    }
    Ok(Outcome::CleanRefusal)
}

fn old_command(old_bin: &Path, root: &Path, home: &Path) -> Command {
    let mut command = Command::new(old_bin);
    command
        .current_dir(root)
        .env("HOME", home)
        // The old side opens the graph itself, as the rest of CI does.
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        // Ignored by builds that predate them; honored by newer ones.
        .env("INFIGRAPH_NO_WATCH", "1")
        .env(infigraph_core::scip_switch::enabled_env_name(), "0")
        .env_remove("INFIGRAPH_WATCH_DAEMON");
    command
}

fn lines(out: &Output) -> Vec<String> {
    support::stdout(out).lines().map(str::to_owned).collect()
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        support::stdout(out),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn graph_bytes(root: &Path) -> Vec<u8> {
    std::fs::read(root.join(".infigraph").join("graph")).unwrap_or_default()
}

fn quarantined(root: &Path) -> bool {
    std::fs::read_dir(root.join(".infigraph"))
        .map(|dir| {
            dir.filter_map(|e| e.ok()).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("graph.corrupt.")
            })
        })
        .unwrap_or(false)
}

/// True if nothing holds `.infigraph/<name>`: the lock can be taken at once.
fn lock_is_free(root: &Path, name: &str) -> bool {
    use fs2::FileExt;
    let path = root.join(".infigraph").join(name);
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
    else {
        return true; // no lock file: no holder
    };
    file.try_lock_exclusive().is_ok()
}

/// Ask a graph both questions through `query` (one cypher string in, the
/// command's output out). `None` if either fails.
fn snapshot(query: impl Fn(&str) -> Output) -> Option<Snapshot> {
    let ids = query(SYMBOL_IDS);
    let probe = query(PROBE_ROWS);
    (ids.status.success() && probe.status.success()).then(|| Snapshot {
        ids: lines(&ids),
        probe: lines(&probe),
    })
}

/// Open the project with this build through `open_args`, then ask it both
/// questions.
fn open_with_head(root: &Path, home: &Path, backend: &str, open_args: &[&str]) -> Observed {
    let before = graph_bytes(root);
    let opened = support::run(root, home, backend, open_args);
    let succeeded = opened.status.success();
    let snapshot = succeeded
        .then(|| snapshot(|q| support::run(root, home, backend, &["query", q])))
        .flatten()
        .unwrap_or(Snapshot {
            ids: Vec::new(),
            probe: Vec::new(),
        });
    Observed {
        succeeded,
        snapshot,
        output: combined(&opened),
        graph_unchanged: graph_bytes(root) == before,
        quarantined: quarantined(root),
    }
}

fn old_binary() -> Option<PathBuf> {
    let required = std::env::var_os(REQUIRE_ENV).is_some();
    let candidate = std::env::var_os(OLD_BIN_ENV).map(PathBuf::from);
    let runnable = candidate.as_ref().is_some_and(|bin| {
        Command::new(bin)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    });
    match (runnable, required) {
        (true, _) => candidate,
        (false, true) => panic!(
            "{REQUIRE_ENV} is set but {OLD_BIN_ENV}={candidate:?} is missing or not runnable: \
             the previous release was not built, and a skipped upgrade test would hide that"
        ),
        (false, false) => None,
    }
}

#[test]
fn upgrading_from_the_previous_release_keeps_the_graph_readable() {
    let Some(old_bin) = old_binary() else {
        eprintln!("skipping the upgrade smoke test: {OLD_BIN_ENV} is not set");
        return;
    };

    let sandbox = tempfile::TempDir::new().unwrap();
    let home = sandbox.path().join("home");
    let root = sandbox.path().join("order-service");
    std::fs::create_dir_all(&home).unwrap();
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/microservices/order-service"),
        &root,
    );

    // The previous release indexes the fixture and says what it holds.
    // `index` only: the old build's `query`, `search` and `stats` each start
    // a background watcher, which is stopped again below.
    let indexed = old_command(&old_bin, &root, &home)
        .args(["index", "--no-embed"])
        .output()
        .unwrap();
    support::assert_ok(&indexed, "previous release: index");
    // One row no source file can regenerate: if an open wipes the graph and
    // re-indexes, the symbols come back and this does not.
    let schemas = root.join(".infigraph").join("structured-schemas");
    std::fs::create_dir_all(&schemas).unwrap();
    std::fs::write(
        schemas.join(format!("{PROBE_SCHEMA_ID}.toml")),
        PROBE_SCHEMA,
    )
    .unwrap();
    let data = sandbox.path().join("probe.json");
    std::fs::write(&data, PROBE_DATA).unwrap();
    let ingested = old_command(&old_bin, &root, &home)
        .args(["ingest", "--schema", PROBE_SCHEMA_ID, "--data-file"])
        .arg(&data)
        .output()
        .unwrap();
    support::assert_ok(&ingested, "previous release: ingest");
    let baseline = snapshot(|q| {
        old_command(&old_bin, &root, &home)
            .args(["query", q])
            .output()
            .unwrap()
    })
    .expect("the previous release could not answer its own queries");
    assert!(
        !baseline.ids.is_empty() && baseline.probe.len() == 1,
        "the previous release's graph is not what the test needs: {baseline:?}"
    );
    let stopped = old_command(&old_bin, &root, &home)
        .arg("watch-stop")
        .output()
        .unwrap();
    support::assert_ok(&stopped, "previous release: watch-stop");
    assert!(
        support::eventually(Duration::from_secs(30), || lock_is_free(
            &root,
            "watch.lock"
        ) && lock_is_free(
            &root,
            "graph.lock"
        )),
        "the previous release's watcher is still running, so this build cannot open the graph"
    );

    // This build opens that same `.infigraph/`: read-only, then a write
    // (the open that used to wipe), then through the daemon (the default).
    let passes: [(&str, &str, &[&str]); 3] = [
        (
            "read",
            infigraph_core::LOCAL_BACKEND,
            &["query", SYMBOL_IDS],
        ),
        (
            "write",
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        (
            "daemon",
            infigraph_core::DAEMON_BACKEND,
            &["query", SYMBOL_IDS],
        ),
    ];
    let mut failures = Vec::new();
    for (name, backend, args) in passes {
        let seen = open_with_head(&root, &home, backend, args);
        match judge(&baseline, &seen) {
            Ok(outcome) => eprintln!("{RAN_MARKER} pass={name} outcome={outcome:?}"),
            Err(why) => failures.push(format!("{name} pass: {why}")),
        }
    }

    // Leave nothing behind: the daemon the last pass started, and any lock.
    let stop = support::run(
        &root,
        &home,
        infigraph_core::LOCAL_BACKEND,
        &["daemon-stop", "--wait"],
    );
    support::assert_ok(&stop, "daemon-stop --wait");
    assert!(
        support::eventually(Duration::from_secs(30), || lock_is_free(
            &root,
            "watch.lock"
        ) && lock_is_free(
            &root,
            "graph.lock"
        )),
        "an infigraph process still holds the project's locks"
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    eprintln!("{RAN_MARKER} passes=3");
}

// --- the checker must be able to fail ---------------------------------

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn snap(symbols: &[&str], probe: &[&str]) -> Snapshot {
    Snapshot {
        ids: ids(symbols),
        probe: ids(probe),
    }
}

fn base() -> Snapshot {
    snap(&["a", "b"], &["p | row"])
}

fn good() -> Observed {
    Observed {
        succeeded: true,
        snapshot: base(),
        output: String::new(),
        graph_unchanged: false,
        quarantined: false,
    }
}

#[test]
fn judge_accepts_the_same_symbols() {
    assert_eq!(judge(&base(), &good()), Ok(Outcome::SameResults));
}

#[test]
fn judge_fails_a_graph_wiped_to_nothing() {
    let wiped = Observed {
        snapshot: snap(&[], &[]),
        ..good()
    };
    assert!(judge(&base(), &wiped).is_err());
}

#[test]
fn judge_fails_a_graph_that_lost_symbols() {
    let shrunk = Observed {
        snapshot: snap(&["a"], &["p | row"]),
        ..good()
    };
    assert!(judge(&base(), &shrunk).is_err());
}

#[test]
fn judge_fails_a_quarantined_graph_even_if_it_was_rebuilt_to_match() {
    let rebuilt = Observed {
        quarantined: true,
        ..good()
    };
    assert!(judge(&base(), &rebuilt).is_err());
}

#[test]
fn judge_accepts_a_clean_refusal() {
    let refused = Observed {
        succeeded: false,
        snapshot: snap(&[], &[]),
        output: format!(
            "Error: {}: ...",
            storage_version_mismatch_context(Path::new("/p/.infigraph/graph"))
        ),
        graph_unchanged: true,
        quarantined: false,
    };
    assert_eq!(judge(&base(), &refused), Ok(Outcome::CleanRefusal));
}

#[test]
fn judge_fails_a_refusal_that_touched_the_graph() {
    let refused = Observed {
        succeeded: false,
        snapshot: snap(&[], &[]),
        output: storage_version_mismatch_context(Path::new("/p/.infigraph/graph")),
        graph_unchanged: false,
        quarantined: false,
    };
    assert!(judge(&base(), &refused).is_err());
}

#[test]
fn judge_fails_a_crash_that_is_not_the_refusal() {
    let crashed = Observed {
        succeeded: false,
        snapshot: snap(&[], &[]),
        output: "thread 'main' panicked".into(),
        graph_unchanged: true,
        quarantined: false,
    };
    assert!(judge(&base(), &crashed).is_err());
}

#[test]
fn judge_fails_a_wipe_and_rebuild_that_brought_every_symbol_back() {
    // Same symbols, same probe row (a lucky one), exit zero -- but the open
    // announced it was destroying the graph first.
    let rebuilt = Observed {
        output: format!("{OPEN_FAILED_NOTICE} after 4 attempts (...), quarantining the graph"),
        ..good()
    };
    assert!(judge(&base(), &rebuilt).is_err());
}

#[test]
fn judge_fails_a_rebuild_that_lost_the_row_source_cannot_regenerate() {
    // No notice at all, every symbol present: only the ingested row is gone.
    let rebuilt = Observed {
        snapshot: snap(&["a", "b"], &[]),
        ..good()
    };
    assert!(judge(&base(), &rebuilt).is_err());
}
