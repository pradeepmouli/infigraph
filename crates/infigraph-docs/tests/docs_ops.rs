//! The docs executor and `clean_docs` (docs opt-in).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use infigraph_core::docs_switch::{
    docs_enabled, docs_enabled_recorded, docs_store_path, lock_docs_op,
};
use infigraph_docs::ops::{clean_docs, index_docs, refresh_docs_if_enabled};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const POLL_MS_VAR: &str = "INFIGRAPH_WATCH_DOC_DAEMON_POLL_MS";

/// Holds `ENV_LOCK`, points `HOME` at an empty directory (so a developer's
/// `~/.infigraph/config.toml` cannot turn docs on), and clears the docs
/// variables. Restores all of it on drop, panics included.
struct Isolated {
    _lock: MutexGuard<'static, ()>,
    _home: tempfile::TempDir,
    orig_home: Option<std::ffi::OsString>,
}

impl Isolated {
    fn new() -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        let orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        std::env::remove_var("INFIGRAPH_DOCS_ENABLED");
        Self {
            _lock: lock,
            _home: home,
            orig_home,
        }
    }
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var(POLL_MS_VAR);
        match &self.orig_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn project_with_readme() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::write(
        root.join("README.md"),
        "# Hello\n\nThe zebra-crossing handbook.\n",
    )
    .unwrap();
    (tmp, root)
}

#[test]
fn index_docs_turns_docs_on_and_reports_real_counts() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();

    let stats = index_docs(&root, false).unwrap();

    assert_eq!(stats.files_indexed, 1, "{stats:?}");
    assert!(stats.files_scanned >= 1, "{stats:?}");
    assert!(stats.chunks_created >= 1, "{stats:?}");
    assert_eq!(stats.documents_in_store, 1, "{stats:?}");
    assert!(stats.chunks_in_store >= 1, "{stats:?}");
    assert_eq!(docs_enabled_recorded(&root), Some(true));
    assert!(docs_store_path(&root).exists());
}

#[test]
fn a_full_index_rebuilds_where_an_incremental_one_skips() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    assert_eq!(index_docs(&root, false).unwrap().files_indexed, 0);
    assert_eq!(index_docs(&root, true).unwrap().files_indexed, 1);
}

#[test]
fn clean_docs_turns_docs_off_and_removes_the_store() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    clean_docs(&root).unwrap();

    assert_eq!(docs_enabled_recorded(&root), Some(false));
    assert!(!docs_store_path(&root).exists());
    assert!(!root.join(".infigraph").join("docs_embeddings.bin").exists());
}

/// Review Focus 3: a docs operation already holding the lock (a watcher's
/// reindex, a `clean-docs`) runs to the end before `index_docs` touches the
/// store.
#[test]
fn index_docs_waits_for_a_docs_operation_already_running() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let held = lock_docs_op(&root, Duration::from_secs(1)).unwrap();

    let r = root.clone();
    let indexing = std::thread::spawn(move || index_docs(&r, false));
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !indexing.is_finished(),
        "index_docs ran alongside another docs operation"
    );
    assert!(
        !docs_store_path(&root).exists(),
        "index_docs touched the store while it waited"
    );

    drop(held);
    assert_eq!(indexing.join().unwrap().unwrap().files_indexed, 1);
}

#[test]
fn request_index_docs_if_enabled_leaves_a_project_that_has_not_opted_in_alone() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    assert_eq!(
        infigraph_docs::ops::request_index_docs_if_enabled(&root).unwrap(),
        None
    );
    assert!(!root.join(".infigraph").exists());
}

#[test]
fn request_index_docs_without_the_daemon_runs_the_executor_here() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let stats = infigraph_docs::ops::request_index_docs(&root, false).unwrap();
    assert_eq!(stats.files_indexed, 1);
    assert_eq!(docs_enabled_recorded(&root), Some(true));
}

/// Review Focus 3: `clean-docs` while the daemon's doc watcher is attached
/// and has a change pending. The switch goes off before the delete, and the
/// watcher re-reads it under the docs lock, so nothing brings the store back.
#[test]
fn clean_docs_is_not_undone_by_an_attached_watcher() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let _env = Isolated::new();
    std::env::set_var(POLL_MS_VAR, "20");
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();

    let shutdown = Arc::new(AtomicBool::new(false));
    let loop_root = root.clone();
    let loop_shutdown = Arc::clone(&shutdown);
    let watcher = std::thread::spawn(move || {
        infigraph_docs::watch::watch_docs_daemon_loop(&loop_root, 50, loop_shutdown)
    });
    std::thread::sleep(Duration::from_millis(500));

    std::fs::write(root.join("second.md"), "# Second\n\nmore\n").unwrap();
    clean_docs(&root).unwrap();
    std::thread::sleep(Duration::from_millis(1500));

    shutdown.store(true, Ordering::Relaxed);
    watcher.join().unwrap().unwrap();
    assert!(
        !docs_store_path(&root).exists(),
        "the watcher recreated the store clean-docs removed"
    );
    assert_eq!(docs_enabled_recorded(&root), Some(false));
}

#[test]
fn open_existing_refuses_a_project_that_has_not_opted_in_and_creates_nothing() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let err = infigraph_docs::DocIndex::open_existing(&root)
        .err()
        .expect("nothing to open");
    assert!(
        err.is::<infigraph_core::docs_switch::DocsNotIndexed>(),
        "{err}"
    );
    assert!(!root.join(".infigraph").exists());
}

#[test]
fn open_existing_refuses_an_opted_in_project_whose_store_is_missing() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
    assert!(infigraph_docs::DocIndex::open_existing(&root).is_err());
    assert!(
        !docs_store_path(&root).exists(),
        "a reader created the store"
    );
}

#[test]
fn open_existing_reads_an_opted_in_store() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();
    let idx = infigraph_docs::DocIndex::open_existing(&root).unwrap();
    let hashes = idx.store().unwrap().get_doc_hashes().unwrap();
    assert!(hashes.contains_key("README.md"), "{hashes:?}");
}

/// `index-docs` racing `clean-docs`: the switch is recorded once the lock is
/// held, not before. Otherwise a `clean-docs` that got the lock first would
/// leave the store deleted and the switch on, and this run would then create
/// a store nobody asked to keep.
#[test]
fn index_docs_records_the_switch_only_once_it_holds_the_lock() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    let held = lock_docs_op(&root, Duration::from_secs(1)).unwrap();

    let r = root.clone();
    let indexing = std::thread::spawn(move || index_docs(&r, false));
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !docs_enabled(&root),
        "index_docs switched documents on before it held the lock"
    );

    drop(held);
    indexing.join().unwrap().unwrap();
    assert!(docs_enabled(&root));
}

/// A reader never repairs: `open_existing` holds only the shared docs lock,
/// so a corrupt store is reported, not wiped and rebuilt under it. The
/// writers (`index-docs`, the watcher) still repair, under the exclusive lock.
#[test]
fn a_reader_does_not_wipe_a_corrupt_index() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    infigraph_core::docs_switch::set_docs_enabled(&root, true).unwrap();
    let store = docs_store_path(&root);
    std::fs::write(&store, b"not-a-valid-kuzu-database").unwrap();

    let err = match infigraph_docs::DocIndex::open_existing(&root) {
        Ok(_) => panic!("a corrupt index opened for reading"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains("reindex-docs"), "{err}");
    assert_eq!(
        std::fs::read(&store).unwrap(),
        b"not-a-valid-kuzu-database",
        "the reader rewrote the store"
    );
}

/// The shared step of a group build for one repo: refresh the documents of a
/// repo that opted in, and leave any other alone (nothing created, nothing
/// switched on).
#[test]
fn a_group_refresh_skips_a_repo_that_has_not_opted_in() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    assert_eq!(refresh_docs_if_enabled(&root).unwrap(), 0);
    assert!(!root.join(".infigraph").exists());
    assert!(!docs_enabled(&root));
}

#[test]
fn a_group_refresh_indexes_a_repo_that_opted_in() {
    let _env = Isolated::new();
    let (_tmp, root) = project_with_readme();
    index_docs(&root, false).unwrap();
    std::fs::write(
        root.join("second.md"),
        "# Second

A new page.
",
    )
    .unwrap();

    refresh_docs_if_enabled(&root).unwrap();

    let idx = infigraph_docs::DocIndex::open_existing(&root).unwrap();
    assert_eq!(idx.store().unwrap().stats().unwrap().document_count, 2);
}
