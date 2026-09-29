//! The docs executor and `clean_docs` (docs opt-in).

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use infigraph_core::docs_switch::{docs_enabled_recorded, docs_store_path, lock_docs_op};
use infigraph_docs::ops::{clean_docs, index_docs};

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
