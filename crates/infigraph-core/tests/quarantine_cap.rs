//! Regression coverage for the corrupt-pool size cap (#100 item 1 /
//! R7.3): the sittir incident quarantined a 9.9G corrupt store and filled
//! the disk. An oversized corrupt base image is dropped in favor of its
//! WAL family plus a forensics manifest; the healthy `previous` pool is
//! never size-capped.

use infigraph_core::quarantine::{quarantine_graph, retire_previous_graph};
use std::fs;
use std::sync::{Mutex, MutexGuard};

/// These tests mutate the process-global INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES;
/// serialize them so parallel test threads can't race the env var.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn with_cap(bytes: &str) -> MutexGuard<'static, ()> {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::env::set_var("INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES", bytes);
    guard
}

fn clear_cap(guard: MutexGuard<'_, ()>) {
    std::env::remove_var("INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES");
    drop(guard);
}

#[test]
fn oversized_corrupt_base_is_dropped_but_wal_and_manifest_survive() {
    let guard = with_cap("16");
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();
    fs::write(ig.join("graph"), vec![0u8; 64]).unwrap(); // 64 > 16-byte cap
    fs::write(ig.join("graph.wal"), b"the evidence that matters").unwrap();

    let returned = quarantine_graph(&ig, "graph").unwrap();
    clear_cap(guard);

    assert!(!ig.join("graph").exists(), "oversized base must be gone");
    let names: Vec<String> = fs::read_dir(&ig)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let manifest = names
        .iter()
        .find(|n| n.starts_with("graph.corrupt.") && n.ends_with(".manifest.json"))
        .unwrap_or_else(|| panic!("manifest must exist: {names:?}"));
    assert_eq!(returned.file_name().unwrap().to_string_lossy(), **manifest);
    let manifest_body = fs::read_to_string(ig.join(manifest)).unwrap();
    assert!(
        manifest_body.contains("\"dropped_bytes\": 64"),
        "{manifest_body}"
    );
    let wal = names
        .iter()
        .find(|n| n.starts_with("graph.corrupt.") && n.ends_with(".wal"))
        .unwrap_or_else(|| panic!("WAL sibling must be relocated into the pool: {names:?}"));
    assert_eq!(
        fs::read(ig.join(wal)).unwrap(),
        b"the evidence that matters"
    );
    assert!(
        !names.iter().any(|n| {
            n.strip_prefix("graph.corrupt.")
                .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_digit()))
        }),
        "no base entry may exist in the pool: {names:?}"
    );
}

#[test]
fn under_cap_corrupt_base_is_quarantined_whole() {
    let guard = with_cap("1000000");
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();
    fs::write(ig.join("graph"), b"small corrupt store").unwrap();

    let returned = quarantine_graph(&ig, "graph").unwrap();
    clear_cap(guard);

    assert!(returned.exists());
    assert_eq!(fs::read(&returned).unwrap(), b"small corrupt store");
}

#[test]
fn previous_pool_is_never_size_capped() {
    let guard = with_cap("16");
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();
    fs::write(ig.join("graph"), vec![7u8; 64]).unwrap(); // over the cap

    let returned = retire_previous_graph(&ig, "graph").unwrap();
    clear_cap(guard);

    assert!(
        returned.exists(),
        "a healthy superseded graph is a rollback candidate -- truncating it destroys \
         its entire value; retention=1 is that pool's bound"
    );
    assert_eq!(fs::read(&returned).unwrap(), vec![7u8; 64]);
}

#[test]
fn cap_zero_disables_the_size_check() {
    let guard = with_cap("0");
    let tmp = tempfile::tempdir().unwrap();
    let ig = tmp.path().join(".infigraph");
    fs::create_dir_all(&ig).unwrap();
    fs::write(ig.join("graph"), vec![0u8; 64]).unwrap();

    let returned = quarantine_graph(&ig, "graph").unwrap();
    clear_cap(guard);

    assert!(
        returned.exists(),
        "cap 0 must quarantine whole regardless of size"
    );
}
