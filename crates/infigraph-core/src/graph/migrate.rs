//! Moving a code graph written under the previous stem to the current one.
//!
//! The previous build named the graph `graph`; this one names it by
//! [`names::STEM`]. Every file that starts with the old stem moves with it
//! (WAL family, health stamps, rebuild leftovers, restore pools, the open
//! sentinel), by swapping the prefix, so no hand-kept list can miss one.
//!
//! Rules this module keeps:
//! - **Only a process that may write migrates**, and only through
//!   [`ensure_migrated`]. A reader that meets a legacy store reports it
//!   ([`journal_exists`], [`names::store_present`], the degraded mode
//!   `store-migration-pending`) and touches nothing.
//! - **No process holds the store.** The previous build's lock
//!   ([`names::legacy::LOCK`]) is held for the whole move, and `watch.lock`
//!   must name no other live process. A daemon of the previous build is asked
//!   to stop (`watch.stop`, which every build honours) and waited for; if it
//!   does not go, the migration is [`Blocked`] and the open fails. An open
//!   never creates a fresh store beside a legacy one it could not migrate.
//! - **A crash resumes.** The renames are written to a journal before the
//!   first one runs; every step is idempotent, and the next start finishes
//!   the journal before it opens anything.
//! - **Both present: the new one wins.** A previous-build process that ran
//!   after a migration made a fresh `graph` beside it; that family is filed as
//!   a retired graph (`<stem>.previous.<ts>`, a name the pools and
//!   `worktree clean` already know) and the new store is untouched.
//!
//! No older build understands the new names. One that starts afterwards finds
//! no `graph` and builds a fresh one beside the migrated store; the next start
//! of this build files it as above. The way back is to rename the `<stem>*`
//! files to `graph*` with no daemon running.

use super::names::{self, legacy};
use anyhow::{Context, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// Serialises migrators. Its name never changes, because the lock it guards
/// is the one being renamed.
pub const MIGRATION_LOCK: &str = "store-migration.lock";
/// The renames still to do, written before the first of them.
pub const JOURNAL: &str = "store-migration.journal";
/// How long a migrator waits for a previous-build daemon to leave.
pub const STOP_WAIT: Duration = Duration::from_secs(30);

pub fn journal_exists(infigraph_dir: &Path) -> bool {
    infigraph_dir.join(JOURNAL).exists()
}

/// Why the store could not be migrated now.
#[derive(Debug)]
pub struct Blocked {
    pub holder: String,
    /// The holder is a daemon, which `watch.stop` can end.
    pub daemon: bool,
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the code graph is stored under its old name and cannot be renamed while {} \
             holds it: run `infigraph daemon-stop --wait`, then retry",
            self.holder
        )
    }
}

impl std::error::Error for Blocked {}

/// What a migration did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Entries renamed (or removed, for locks and duplicates).
    pub steps: usize,
    /// Where an unmigrated store went because a migrated one already existed.
    pub set_aside: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Move { from: String, to: String },
    Remove { name: String },
}

fn is_lock(name: &str) -> bool {
    name.ends_with(".lock")
}

/// The legacy entries of `dir` that are data (not locks).
fn legacy_data(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| legacy::is_entry(n) && !is_lock(n))
        .collect();
    found.sort();
    found
}

/// Whether anything in `dir` still carries the old stem, locks included.
fn legacy_left(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_str().is_some_and(legacy::is_entry))
}

fn to_new(name: &str) -> String {
    let rest = name.strip_prefix(legacy::STEM).unwrap_or(name);
    format!("{}{rest}", names::STEM)
}

/// The steps that bring `dir` to the new names. Pure of side effects.
fn plan(dir: &Path, ts: u64) -> (Vec<Step>, Option<String>) {
    let data = legacy_data(dir);
    let live_exists = names::live(dir).exists();
    let mut steps = Vec::new();
    let mut set_aside = None;
    // The graph's own family (file and WAL) is the part a second copy would
    // clobber; the pools and sidecars move on their own names.
    let in_family = |n: &str| n == legacy::STEM || n.starts_with("graph.wal");
    let mut aside_stem = None;
    if live_exists && data.iter().any(|n| n == legacy::STEM) {
        let mut t = ts;
        while dir.join(format!("{}{t}", names::PREVIOUS_PREFIX)).exists() {
            t += 1;
        }
        let stem = format!("{}{t}", names::PREVIOUS_PREFIX);
        set_aside = Some(stem.clone());
        aside_stem = Some(stem);
    }
    // WAL first, the file last: a build that starts between two steps and
    // finds `graph` then finds it without a stale WAL.
    let mut ordered: Vec<&String> = data.iter().filter(|n| n.as_str() != legacy::STEM).collect();
    if data.iter().any(|n| n == legacy::STEM) {
        // Placed last.
        ordered.push(data.iter().find(|n| n.as_str() == legacy::STEM).unwrap());
    }
    for name in ordered {
        let target = match (&aside_stem, in_family(name)) {
            (Some(stem), true) => {
                let rest = name.strip_prefix(legacy::STEM).unwrap_or("");
                format!("{stem}{rest}")
            }
            _ => to_new(name),
        };
        if dir.join(&target).exists() && !in_family(name) {
            // A sidecar the new store already has its own: the new one is
            // right, the old one is derived data.
            let derived = name.starts_with("graph.health.")
                || name == "graph.compaction.json"
                || name.starts_with("graph.rebuilding");
            if derived {
                steps.push(Step::Remove { name: name.clone() });
            } else {
                steps.push(Step::Move {
                    from: name.clone(),
                    to: format!("{target}.legacy"),
                });
            }
            continue;
        }
        steps.push(Step::Move {
            from: name.clone(),
            to: target,
        });
    }
    // Stale locks go last; the one held for the move is the very last.
    let mut locks: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| legacy::is_entry(n) && is_lock(n) && n != legacy::LOCK)
        .collect();
    locks.sort();
    for name in locks {
        steps.push(Step::Remove { name });
    }
    if dir.join(legacy::LOCK).exists() {
        steps.push(Step::Remove {
            name: legacy::LOCK.to_string(),
        });
    }
    (steps, set_aside)
}

fn journal_text(steps: &[Step]) -> String {
    steps
        .iter()
        .map(|s| match s {
            Step::Move { from, to } => format!("mv\t{from}\t{to}\n"),
            Step::Remove { name } => format!("rm\t{name}\n"),
        })
        .collect()
}

fn read_journal(dir: &Path) -> Result<Vec<Step>> {
    let text = std::fs::read_to_string(dir.join(JOURNAL))
        .with_context(|| format!("reading {}", dir.join(JOURNAL).display()))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            match (f.next()?, f.next(), f.next()) {
                ("mv", Some(from), Some(to)) => Some(Step::Move {
                    from: from.to_string(),
                    to: to.to_string(),
                }),
                ("rm", Some(name), None) => Some(Step::Remove {
                    name: name.to_string(),
                }),
                _ => None,
            }
        })
        .collect())
}

/// Runs `steps`; each is a no-op if already done. `stop_after` is the crash
/// test's way of dying between two renames.
fn apply(dir: &Path, steps: &[Step], stop_after: Option<usize>) -> Result<usize> {
    let mut done = 0;
    for (i, step) in steps.iter().enumerate() {
        if stop_after == Some(i) {
            anyhow::bail!("stopped after {i} steps (test)");
        }
        match step {
            Step::Move { from, to } => {
                let (src, dst) = (dir.join(from), dir.join(to));
                if src.exists() && !dst.exists() {
                    std::fs::rename(&src, &dst).with_context(|| {
                        format!("renaming {} to {}", src.display(), dst.display())
                    })?;
                    done += 1;
                }
            }
            Step::Remove { name } => {
                let path = dir.join(name);
                let removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if removed.is_ok() {
                    done += 1;
                }
            }
        }
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(done)
}

/// A live process other than this one that holds the store, as a sentence.
fn other_holder(dir: &Path) -> Option<String> {
    let info = crate::lockfile::read_holder(&dir.join("watch.lock"))?;
    (info.pid != std::process::id() && crate::lockfile::holder_is_alive(&info)).then(|| {
        format!(
            "process {} ({}, build {})",
            info.pid, info.role, info.build_hash
        )
    })
}

/// One attempt, without waiting for anyone.
pub fn migrate(dir: &Path) -> Result<Outcome> {
    migrate_impl(dir, None)
}

fn migrate_impl(dir: &Path, stop_after: Option<usize>) -> Result<Outcome> {
    if names::STEM == legacy::STEM || !dir.is_dir() {
        return Ok(Outcome::default());
    }
    if !journal_exists(dir) && legacy_data(dir).is_empty() && !legacy_left(dir) {
        return Ok(Outcome::default());
    }
    let _migrating = crate::lockfile::acquire(
        &dir.join(MIGRATION_LOCK),
        "store-migration",
        Duration::from_secs(30),
    )?;
    if let Some(holder) = other_holder(dir) {
        return Err(Blocked {
            holder,
            daemon: true,
        }
        .into());
    }
    let Some(_legacy_lock) =
        crate::lockfile::try_acquire(&dir.join(legacy::LOCK), "store-migration")?
    else {
        return Err(Blocked {
            holder: "a process of the previous build".to_string(),
            daemon: false,
        }
        .into());
    };
    // Re-judge under the lock: another migrator may have finished.
    let (steps, set_aside) = if journal_exists(dir) {
        (read_journal(dir)?, None)
    } else {
        if legacy_data(dir).is_empty() && !legacy_left(dir) {
            return Ok(Outcome::default());
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (steps, aside) = plan(dir, ts);
        crate::daemon_protocol::write_atomic(&dir.join(JOURNAL), &journal_text(&steps))?;
        (steps, aside)
    };
    let done = apply(dir, &steps, stop_after)?;
    let _ = std::fs::remove_file(dir.join(JOURNAL));
    if let Some(stem) = &set_aside {
        eprintln!(
            "[migrate] a previous-build graph appeared beside {}: filed as {stem}",
            names::STEM
        );
    } else if done > 0 {
        eprintln!(
            "[migrate] code graph renamed from {} to {} ({done} entries)",
            legacy::STEM,
            names::STEM
        );
    }
    Ok(Outcome {
        steps: done,
        set_aside,
    })
}

/// [`migrate`], and when a previous-build daemon is in the way, ask it to
/// stop and wait for it. The entry point for every writer-capable start.
pub fn ensure_migrated(dir: &Path) -> Result<Outcome> {
    ensure_migrated_within(dir, STOP_WAIT)
}

pub fn ensure_migrated_within(dir: &Path, wait: Duration) -> Result<Outcome> {
    match migrate(dir) {
        Err(e) if e.downcast_ref::<Blocked>().is_some_and(|b| b.daemon) => {
            // The sentinel every build's daemon honours; no signal is sent.
            let _ = std::fs::write(dir.join("watch.stop"), b"");
            let deadline = Instant::now() + wait;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                match migrate(dir) {
                    Err(e2)
                        if e2.downcast_ref::<Blocked>().is_some() && Instant::now() < deadline => {}
                    other => return other,
                }
            }
        }
        other => other,
    }
}

/// For an open of the store at `db_path`: migrate first when `db_path` is the
/// project's code graph and an older one is waiting. A failure is returned,
/// never turned into "no store".
pub fn before_open(db_path: &Path) -> Result<()> {
    if db_path.file_name().is_none_or(|n| n != names::STEM) {
        return Ok(());
    }
    let Some(dir) = db_path.parent() else {
        return Ok(());
    };
    if db_path.exists() && !journal_exists(dir) && !legacy_left_family(dir) {
        return Ok(());
    }
    ensure_migrated(dir).map(|_| ())
}

/// A stray previous-build graph beside a migrated one.
fn legacy_left_family(dir: &Path) -> bool {
    dir.join(legacy::STEM).exists()
}

/// Whether a reader should report a migration as pending.
pub fn pending(dir: &Path) -> bool {
    names::STEM != legacy::STEM
        && (journal_exists(dir) || (!names::live(dir).exists() && dir.join(legacy::STEM).exists()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Every file the previous build keeps beside its graph.
    const OLD_FILES: &[&str] = &[
        "graph",
        "graph.wal",
        "graph.wal.checkpoint",
        "graph.health.json",
        "graph.health.recorded",
        "graph.compaction.json",
        "graph.rebuilding",
        "graph.rebuilding.wal",
        "graph.rebuilding.lock",
        "graph.ckpt.lock",
        "graph.lock",
        "graph.previous.100",
        "graph.previous.100.wal",
        "graph.corrupt.200",
        "graph.corrupt.200.wal.checkpoint",
        "graph.torn-wal.300.wal.checkpoint",
        "graph-opening.4242.0",
    ];

    fn write_all(dir: &Path, names: &[&str]) {
        for n in names {
            std::fs::write(dir.join(n), n).unwrap();
        }
    }

    fn tree(dir: &Path) -> BTreeMap<String, String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != MIGRATION_LOCK)
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    std::fs::read_to_string(e.path()).unwrap_or_default(),
                )
            })
            .collect()
    }

    fn legacy_store() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_all(dir.path(), OLD_FILES);
        // Files that start with the old stem and are not the store.
        write_all(dir.path(), &["graph.html", "embeddings.bin"]);
        dir
    }

    #[test]
    fn a_legacy_store_is_renamed_with_everything_that_carried_its_stem() {
        let dir = legacy_store();
        let out = migrate(dir.path()).unwrap();
        assert!(out.steps > 0 && out.set_aside.is_none());
        let after = tree(dir.path());
        let leftovers: Vec<_> = after.keys().filter(|n| legacy::is_entry(n)).collect();
        assert!(
            leftovers.is_empty(),
            "still under the old stem: {leftovers:?}"
        );
        for old in OLD_FILES.iter().filter(|n| !n.ends_with(".lock")) {
            let new = to_new(old);
            assert_eq!(after.get(&new).map(String::as_str), Some(*old), "{new}");
        }
        assert!(after.contains_key("graph.html"), "not part of the store");
        assert!(after.contains_key("embeddings.bin"));
        assert!(!journal_exists(dir.path()));
        assert!(names::store_present(dir.path()));
        assert!(!pending(dir.path()));
    }

    #[test]
    fn the_restore_pools_survive_the_move_as_pools() {
        let dir = legacy_store();
        for n in ["graph.previous.100", "graph.corrupt.200"] {
            assert!(crate::snapshot::is_restore_pool_entry(n), "{n} before");
        }
        migrate(dir.path()).unwrap();
        for n in ["previous.100", "previous.100.wal", "corrupt.200"] {
            let name = names::sibling(&format!(".{n}"));
            assert!(dir.path().join(&name).exists(), "{name}");
            assert!(crate::snapshot::is_restore_pool_entry(&name), "{name}");
        }
    }

    #[test]
    fn an_already_migrated_store_is_left_alone_and_a_second_run_does_nothing() {
        let dir = legacy_store();
        migrate(dir.path()).unwrap();
        let once = tree(dir.path());
        assert_eq!(migrate(dir.path()).unwrap(), Outcome::default());
        assert_eq!(tree(dir.path()), once);
    }

    #[test]
    fn a_crash_between_any_two_renames_resumes_to_the_uninterrupted_end_state() {
        let clean = legacy_store();
        migrate(clean.path()).unwrap();
        let expected = tree(clean.path());
        let steps = plan(legacy_store().path(), 1).0.len();
        assert!(steps > 5);
        for crash_at in 0..steps {
            let dir = legacy_store();
            let err = migrate_impl(dir.path(), Some(crash_at)).unwrap_err();
            assert!(err.to_string().contains("stopped after"), "{err}");
            assert!(journal_exists(dir.path()), "the journal outlives the crash");
            // The store is still readable under one name or the other.
            assert!(names::store_present(dir.path()));
            migrate(dir.path()).unwrap();
            assert_eq!(tree(dir.path()), expected, "crashed after {crash_at} steps");
        }
    }

    #[test]
    fn both_present_keeps_the_new_store_and_files_the_stray_as_a_retired_graph() {
        let dir = tempfile::tempdir().unwrap();
        write_all(dir.path(), &["code.kuzu", "code.kuzu.wal"]);
        std::fs::write(dir.path().join("code.kuzu"), "the migrated one").unwrap();
        write_all(dir.path(), &["graph", "graph.wal", "graph.health.json"]);
        let out = migrate(dir.path()).unwrap();
        let aside = out.set_aside.expect("the stray is set aside");
        assert!(crate::snapshot::is_restore_pool_entry(&aside), "{aside}");
        let after = tree(dir.path());
        assert_eq!(after["code.kuzu"], "the migrated one", "never clobbered");
        assert_eq!(after[&aside], "graph");
        assert_eq!(after[&format!("{aside}.wal")], "graph.wal");
        assert!(!after.keys().any(|n| legacy::is_entry(n)), "{after:?}");
        // The old build keeps coming back; the second visit is the same.
        write_all(dir.path(), &["graph"]);
        let again = migrate(dir.path()).unwrap();
        assert_ne!(again.set_aside.unwrap(), aside, "a new name, nothing lost");
        assert_eq!(tree(dir.path())["code.kuzu"], "the migrated one");
        assert_eq!(migrate(dir.path()).unwrap(), Outcome::default());
    }

    /// A process that stays alive and holds `watch.lock` in name.
    fn live_daemon(dir: &Path) -> std::process::Child {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let mut info = crate::lockfile::LockInfo::current("daemon");
        info.pid = child.id();
        info.holder_started_at = 0;
        std::fs::write(
            dir.join("watch.lock"),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
        child
    }

    #[test]
    fn a_live_holder_blocks_the_move_and_no_second_store_is_created() {
        let dir = legacy_store();
        let mut daemon = live_daemon(dir.path());
        let before = tree(dir.path());
        let err = ensure_migrated_within(dir.path(), Duration::from_millis(600)).unwrap_err();
        let blocked = err.downcast_ref::<Blocked>().expect("Blocked");
        assert!(
            blocked.holder.contains(&daemon.id().to_string()),
            "{blocked}"
        );
        // The open path refuses too, and creates nothing at the new name.
        let open = crate::graph::GraphStore::open(&names::live(dir.path()));
        assert!(open.is_err());
        assert!(!names::live(dir.path()).exists(), "forked the store");
        let mut after = tree(dir.path());
        after.remove("watch.stop");
        let mut before = before;
        before.remove("watch.lock");
        after.remove("watch.lock");
        assert_eq!(after, before, "nothing moved");
        daemon.kill().unwrap();
        daemon.wait().unwrap();
    }

    #[test]
    fn a_daemon_that_leaves_on_watch_stop_lets_the_move_proceed() {
        let dir = legacy_store();
        let mut daemon = live_daemon(dir.path());
        let root = dir.path().to_path_buf();
        let stopper = std::thread::spawn(move || {
            while !root.join("watch.stop").exists() {
                std::thread::sleep(Duration::from_millis(50));
            }
            daemon.kill().unwrap();
            daemon.wait().unwrap();
        });
        let out = ensure_migrated_within(dir.path(), Duration::from_secs(10)).unwrap();
        stopper.join().unwrap();
        assert!(out.steps > 0);
        assert!(names::live(dir.path()).exists());
        assert!(!dir.path().join(legacy::STEM).exists());
    }

    #[test]
    fn a_read_never_migrates_and_says_so() {
        let dir = legacy_store();
        let err = match crate::graph::GraphStore::open_read_only(&names::live(dir.path())) {
            Ok(_) => panic!("opened"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("old name"), "{err}");
        assert!(pending(dir.path()));
        assert!(dir.path().join(legacy::STEM).exists() && !names::live(dir.path()).exists());
        let root = tempfile::tempdir().unwrap();
        let ig = root.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        write_all(&ig, &["graph"]);
        let modes = crate::degraded::derived_from_disk(root.path());
        assert!(
            modes.contains(&crate::degraded::DegradedMode::StoreMigrationPending),
            "{modes:?}"
        );
        let verify = crate::verify::run_verify(root.path());
        assert!(
            verify.iter().any(|r| r.message.contains("old name")),
            "{verify:?}"
        );
    }

    #[test]
    fn opening_the_store_for_writing_migrates_a_real_graph_and_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join(legacy::STEM);
        {
            let store = crate::graph::GraphStore::open(&old).unwrap();
            store
                .connection()
                .unwrap()
                .query(
                    "CREATE (:Symbol {id: 'a.rs::f', name: 'f', kind: 'function', file: 'a.rs', \
                     start_line: 1, end_line: 1, signature_hash: '', language: 'rust', \
                     visibility: '', parent: '', docstring: '', complexity: 0, parameters: '', \
                     return_type: ''})",
                )
                .unwrap();
        }
        let store = crate::graph::GraphStore::open(&names::live(dir.path())).unwrap();
        let rows = store.get_all_symbols().unwrap();
        assert_eq!(rows.len(), 1, "the symbol came across the rename");
        assert!(!old.exists());
    }

    #[test]
    fn a_clone_of_a_legacy_source_copies_it_by_the_name_it_has_and_the_copy_migrates() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let ig = src.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        write_all(
            &ig,
            &["graph", "graph.wal", "graph.previous.1", "graph.lock"],
        );
        crate::clone::clone_infigraph_dir(src.path(), dst.path()).unwrap();
        let copied = dst.path().join(".infigraph");
        assert!(copied.join("graph").exists());
        assert!(!copied.join("graph.previous.1").exists(), "pools stay home");
        assert!(!copied.join("graph.lock").exists());
        migrate(&copied).unwrap();
        assert!(names::live(&copied).exists() && !copied.join("graph").exists());
        assert!(copied.join(names::sibling(".wal")).exists());
    }

    #[test]
    fn an_observer_with_no_daemon_reports_the_pending_migration_and_moves_nothing() {
        let root = tempfile::tempdir().unwrap();
        let ig = root.path().join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        write_all(&ig, &["graph", "graph.wal"]);
        let err = match crate::graph::observe::ObservedGraph::open(root.path(), &names::live(&ig)) {
            Ok(_) => panic!("observed a store that has not been renamed"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("old name"), "{err}");
        assert!(ig.join("graph").exists() && !names::live(&ig).exists());
        assert!(!journal_exists(&ig));
    }
}
