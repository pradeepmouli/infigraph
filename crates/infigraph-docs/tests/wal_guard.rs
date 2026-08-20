//! Regression coverage for pradeepmouli/infigraph#93: `docs.kuzu` had the
//! same WAL-replay SIGBUS exposure as the code graph (#92) but no
//! cross-process identity lock to detect the dead-holder half of the
//! signal, and `DocIndex::clean()`'s `with_extension` bug meant the
//! wipe-and-rebuild recovery never actually deleted the real
//! `docs.kuzu.wal`/`docs.kuzu.lock` siblings.

use infigraph_core::graph::db_lock_path;
use infigraph_docs::store::DocStore;
use infigraph_docs::DocIndex;
use std::fs;
use std::path::Path;

/// A PID essentially guaranteed not to be a running process.
const DEAD_PID: u32 = 999_999;

fn write_holder_lock(lock_path: &Path, pid: u32) {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let info = infigraph_core::lockfile::LockInfo {
        pid,
        role: "test".to_string(),
        build_hash: "test".to_string(),
        acquired_at: 0,
        last_heartbeat: 0,
        holder_started_at: 0,
    };
    fs::write(lock_path, serde_json::to_string(&info).unwrap()).unwrap();
}

/// Seeds the exact on-disk state that crashed the MCP server in #92, but
/// for the docs store: a plausibly-sized base image, an unreplayed WAL
/// sibling (APPENDED name), and a lock payload naming a dead holder.
fn poison_docs_store(ig_dir: &Path) -> std::path::PathBuf {
    fs::create_dir_all(ig_dir).unwrap();
    let db_path = ig_dir.join("docs.kuzu");
    // Large enough to pass validate_db_file's truncation preflight -- this
    // test is about the WAL guard, not that one.
    fs::write(&db_path, vec![0u8; 4096]).unwrap();
    fs::write(ig_dir.join("docs.kuzu.wal"), b"unreplayed wal").unwrap();
    write_holder_lock(&db_lock_path(&db_path), DEAD_PID);
    db_path
}

#[test]
fn open_refuses_docs_store_with_unreplayed_wal_from_dead_process() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = poison_docs_store(&tmp.path().join(".infigraph"));

    let err = DocStore::open(&db_path)
        .map(|_| ())
        .expect_err("must refuse rather than attempt Database::new");
    assert!(err.to_string().contains("unreplayed WAL"), "{err}");
    assert!(err.to_string().contains(&DEAD_PID.to_string()), "{err}");
}

#[test]
fn open_proceeds_when_wal_holder_is_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();
    let db_path = ig.join("docs.kuzu");
    // Fresh create (no base image yet), but a WAL sibling plus a LIVE
    // holder payload -- the routine mid-write state, must not be refused.
    fs::write(ig.join("docs.kuzu.wal"), b"live writer's wal").unwrap();
    write_holder_lock(&db_lock_path(&db_path), std::process::id());

    // The stray WAL from a "different database" makes the real open fail
    // inside Kuzu for unrelated reasons on some setups; the point here is
    // only that the guard did NOT trip -- so accept either success or an
    // error that is not the guard's.
    match DocStore::open(&db_path) {
        Ok(_) => {}
        Err(e) => assert!(
            !e.to_string().contains("unreplayed WAL"),
            "guard must not trip for a live holder: {e}"
        ),
    }
}

#[test]
fn clean_removes_the_real_appended_wal_and_lock_siblings() {
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    let db_path = poison_docs_store(&ig);

    let mut index = DocIndex::open(tmp.path()).unwrap();
    index.clean().unwrap();

    assert!(!db_path.exists(), "base image must be wiped");
    assert!(
        !ig.join("docs.kuzu.wal").exists(),
        "the REAL WAL sibling (appended name) must be wiped -- \
         with_extension used to compute docs.wal and miss it"
    );
    assert!(
        !ig.join("docs.kuzu.lock").exists(),
        "the REAL lock sibling (appended name) must be wiped"
    );
}

/// End-to-end: the refuse -> wipe -> reopen recovery loop must converge.
/// Before the clean() fix, the wipe left the real WAL and the dead-holder
/// lock payload in place, so the reopen re-tripped the guard and init()
/// failed permanently ("still unreadable after wipe").
#[test]
fn init_recovers_from_poisoned_docs_store_by_wiping_and_rebuilding() {
    let tmp = tempfile::tempdir().unwrap();
    poison_docs_store(&tmp.path().join(".infigraph"));

    let mut index = DocIndex::open(tmp.path()).unwrap();
    index
        .init()
        .expect("init must recover via wipe-and-rebuild, not wedge on the guard");
    assert!(index.store().is_some(), "a fresh store must be open");
}
