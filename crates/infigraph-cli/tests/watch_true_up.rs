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
