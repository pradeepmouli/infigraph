//! `DocIndex::init` against a `docs.kuzu` another process has open.
//!
//! A daemon opens `docs.kuzu` for moments at a time: its read service at
//! startup, then its doc watcher's catch-up reindex as soon as the file
//! exists. `watch_daemon_docs`'s `cmd_watch_daemon_also_indexes_docs_without_restart`
//! failed on macOS CI when its own direct open landed in one of those
//! windows and was refused with "docs.kuzu is locked by another infigraph
//! process". The holder has to be another process: within one, `DB_LOCK`
//! serialises every open before lbug's file lock is ever reached.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use infigraph_docs::store::DocStore;
use infigraph_docs::DocIndex;

const HOLD_ROOT: &str = "INFIGRAPH_TEST_HOLD_DOCS_ROOT";
const HOLDING: &str = "holding docs.kuzu";

/// The other process: holds `docs.kuzu` open for a moment, then exits.
/// Runs only when spawned by the test below.
#[test]
#[ignore]
fn hold_docs_store_briefly() {
    let Some(root) = std::env::var_os(HOLD_ROOT) else {
        return;
    };
    let path = std::path::Path::new(&root)
        .join(".infigraph")
        .join("docs.kuzu");
    let store = DocStore::open(&path).unwrap();
    println!("{HOLDING}");
    std::thread::sleep(Duration::from_millis(800));
    drop(store);
}

#[test]
fn init_waits_out_another_process_briefly_holding_the_store() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    DocIndex::open(&root).unwrap().init().unwrap();

    let mut holder = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hold_docs_store_briefly",
            "--ignored",
            "--nocapture",
        ])
        .env(HOLD_ROOT, &root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(holder.stdout.take().unwrap());
    assert!(
        stdout
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains(HOLDING)),
        "the holder process never opened docs.kuzu"
    );

    let opened = DocIndex::open(&root).unwrap().init();
    holder.wait().unwrap();
    opened.expect("a brief hold by another process must be waited out, not refused");
}
