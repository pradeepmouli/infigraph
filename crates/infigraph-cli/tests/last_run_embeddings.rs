//! #209 item 11: an embedding update that fails after a drain leaves a
//! durable record of why, instead of a line in `daemon.log`.
//!
//! A real daemon watching a real project. `embeddings.bin` is made a
//! directory, so the save after the next drain cannot replace it; the daemon
//! logs "embedding update failed" and, now, records it.

mod support;

use std::time::Duration;

use infigraph_core::last_run::{self, Kind};
use support::{eventually, start_daemon};

#[test]
fn a_daemon_whose_embeddings_cannot_be_saved_records_why() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (root, home) = (project.path(), home.path());
    std::fs::write(root.join("hello.py"), "def hello():\n    pass\n").unwrap();

    let _daemon = start_daemon(root, home);
    let ig = root.join(".infigraph");
    // The index above ran with --no-embed, so there is no file to replace.
    std::fs::create_dir(ig.join("embeddings.bin")).unwrap();

    // A new symbol each time: the drain that picks it up must embed it and
    // save. Rewritten every few seconds because the daemon holds `watch.lock`
    // a moment before its filesystem watch is registered, and an edit made in
    // that gap is never seen.
    let mut edits = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    let mut recorded = false;
    while std::time::Instant::now() < deadline && !recorded {
        edits += 1;
        let mut source = String::from("def hello():\n    pass\n");
        for n in 0..edits {
            source.push_str(&format!("\ndef extra_{n}():\n    pass\n"));
        }
        std::fs::write(root.join("hello.py"), source).unwrap();
        recorded = eventually(Duration::from_secs(4), || {
            last_run::read(&ig, Kind::Embeddings).is_some_and(|r| r.last_problem.is_some())
        });
    }
    assert!(
        recorded,
        "no embeddings problem was recorded; daemon.log:\n{}",
        std::fs::read_to_string(ig.join("daemon.log")).unwrap_or_default()
    );

    let problem = last_run::read(&ig, Kind::Embeddings)
        .unwrap()
        .last_problem
        .unwrap();
    assert!(!problem.ok, "{problem:?}");
    assert!(
        problem.summary.contains("embedding update failed"),
        "{problem:?}"
    );
}
