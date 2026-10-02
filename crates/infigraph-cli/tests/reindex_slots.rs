//! #150: at most `[index] max_concurrent_reindexes` daemons run a full
//! reindex at once on a machine. Through real processes: a real daemon, the
//! real `infigraph index --full` and `infigraph doctor`, and a cap of one
//! whose only slot this test holds itself, standing in for another
//! project's daemon mid-rebuild.

mod support;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use support::{assert_ok, eventually, infigraph, run, start_daemon, stdout};

const BUDGET: Duration = Duration::from_secs(120);

fn doctor(root: &Path, home: &Path) -> String {
    let out = run(root, home, "daemon", &["doctor"]);
    format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr))
}

#[test]
fn a_full_reindex_waits_for_the_machines_slot_and_runs_when_it_is_free() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let home = home.path().canonicalize().unwrap();
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();

    // One rebuild at a time on this "machine" (the scratch HOME).
    let user_dir = home.join(".infigraph");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::write(
        user_dir.join("config.toml"),
        "[index]\nmax_concurrent_reindexes = 1\n",
    )
    .unwrap();

    let _daemon = start_daemon(&root, &home);

    // Another daemon is rebuilding: the one slot is taken.
    let pool = infigraph_core::slots::SlotPool::new(user_dir.join("reindex-slots"), 1);
    let other_daemons_rebuild = pool.try_claim().unwrap().expect("the slot was free");

    let asked = infigraph()
        .args(["index", "--full", "--no-embed"])
        .current_dir(&root)
        .env("HOME", &home)
        .env(infigraph_core::BACKEND_ENV, "daemon")
        .env("INFIGRAPH_NO_WATCH", "1")
        .env_remove("INFIGRAPH_DOCS_ENABLED")
        .env_remove("INFIGRAPH_WATCH_DAEMON")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let key = infigraph_core::degraded::REINDEX_WAITING_FOR_SLOT;
    assert!(
        eventually(BUDGET, || doctor(&root, &home).contains(key)),
        "the daemon is not reported as waiting for a slot:\n{}",
        doctor(&root, &home)
    );

    // Waiting holds nothing: no rebuild has begun, the index operation is
    // free, and the graph still answers.
    let infigraph_dir = root.join(".infigraph");
    assert!(
        !infigraph_dir.join("graph.rebuilding").exists(),
        "a rebuild started without a slot"
    );
    let index_lock =
        infigraph_core::lockfile::try_acquire(&infigraph_dir.join("index.lock"), "test").unwrap();
    assert!(index_lock.is_some(), "waiting for a slot holds index.lock");
    drop(index_lock);
    let read = run(&root, &home, "daemon", &["stats"]);
    assert_ok(&read, "a read while the rebuild waits");

    // The other rebuild ends. This one runs, and says it had waited.
    drop(other_daemons_rebuild);
    let done = asked.wait_with_output().unwrap();
    assert_ok(&done, "index --full after the slot was freed");
    let said = String::from_utf8_lossy(&done.stderr);
    assert!(
        said.contains("waiting for a machine-wide slot"),
        "the client never said it was waiting:\n{said}"
    );
    assert!(
        eventually(BUDGET, || !doctor(&root, &home).contains(key)),
        "still reported as waiting after the rebuild ran:\n{}",
        doctor(&root, &home)
    );
    // The slot is free again once the rebuild has been swapped in.
    assert!(eventually(BUDGET, || pool.try_claim().unwrap().is_some()));
}
