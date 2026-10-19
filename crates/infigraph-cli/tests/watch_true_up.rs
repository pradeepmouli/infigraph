//! A daemon started on a project that changed while none ran indexes the
//! change. A filesystem watcher sees only what happens after it is
//! registered, so an edit made during downtime produces no event for it:
//! the producer queues one whole-project pass after registering, which is
//! what makes the code graph catch up the way `infigraph index` would.

mod support;

use std::time::Duration;
use support::{assert_ok, eventually, run, spawn_daemon, stdout};

fn symbol_is_indexed(root: &std::path::Path, home: &std::path::Path, name: &str) -> bool {
    let out = run(
        root,
        home,
        infigraph_core::DAEMON_BACKEND,
        &[
            "query",
            &format!("MATCH (s:Symbol) WHERE s.name = '{name}' RETURN s.id"),
        ],
    );
    out.status.success() && stdout(&out).contains(name)
}

#[test]
fn a_daemon_indexes_what_changed_while_none_was_running() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();
    assert_ok(
        &run(
            root,
            home,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    // Edited with no daemon running: nothing can have seen it happen.
    std::fs::write(
        root.join("hello.py"),
        "def hello():\n    pass\n\ndef written_during_downtime():\n    pass\n",
    )
    .unwrap();

    let _daemon = spawn_daemon(root, home);
    assert!(
        eventually(Duration::from_secs(60), || symbol_is_indexed(
            root,
            home,
            "written_during_downtime"
        )),
        "the daemon never indexed an edit made while it was not running; daemon.log:\n{}",
        std::fs::read_to_string(root.join(".infigraph/daemon.log")).unwrap_or_default()
    );
}

/// The watcher is deaf while it registers, and registering a subscription per
/// directory took 27s for 1,021 directories on macOS (each `watch()` rebuilds
/// the FSEvents stream). An edit made two seconds after the daemon took
/// `watch.lock` -- a person saving a file just after a restart -- must still
/// be indexed promptly, not only once registration finally ends.
#[test]
fn an_edit_made_while_a_big_tree_is_being_watched_is_indexed_promptly() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (project.path(), home.path());
    for top in 0..20 {
        for sub in 0..50 {
            let dir = root.join(format!("pkg{top}/mod{sub}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("m.py"), "def m():\n    pass\n").unwrap();
        }
    }
    assert_ok(
        &run(
            root,
            home,
            infigraph_core::LOCAL_BACKEND,
            &["index", "--no-embed"],
        ),
        "bootstrap index",
    );

    let _daemon = spawn_daemon(root, home);
    assert!(
        eventually(Duration::from_secs(30), || root
            .join(".infigraph/watch.lock")
            .exists()),
        "the daemon never took watch.lock"
    );
    std::thread::sleep(Duration::from_secs(2));
    std::fs::write(
        root.join("pkg3/mod7/m.py"),
        "def m():\n    pass\n\ndef written_while_registering():\n    pass\n",
    )
    .unwrap();
    assert!(
        eventually(Duration::from_secs(15), || symbol_is_indexed(
            root,
            home,
            "written_while_registering"
        )),
        "an edit made 2s after watch.lock was not indexed within 15s; daemon.log:\n{}",
        std::fs::read_to_string(root.join(".infigraph/daemon.log")).unwrap_or_default()
    );
}
