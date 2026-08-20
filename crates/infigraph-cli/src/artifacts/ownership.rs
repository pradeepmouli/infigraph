//! Tracks the content hash of files written by the `overwrite` strategy so
//! uninstall can tell an untouched managed file from one the user hand-edited
//! after install, and preserve the latter instead of silently deleting it.
//!
//! The other four strategies don't need this: `json_deep_merge`,
//! `json_key_path`, and `toml_section` remove only the specific key/section
//! they own, and `marker_delimited` removes only the content between its own
//! markers -- a user's edits elsewhere in the same file are untouched by
//! construction. `overwrite` has no such boundary; it owns the whole file,
//! so it's the one strategy where "did the user touch this since install"
//! has to be checked explicitly before deleting.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

fn manifest_path(home: &Path) -> std::path::PathBuf {
    home.join(".infigraph").join("installed-files.json")
}

fn load_manifest(home: &Path) -> Result<BTreeMap<String, String>> {
    let path = manifest_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(BTreeMap::new());
    };
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn save_manifest(home: &Path, manifest: &BTreeMap<String, String>) -> Result<()> {
    let path = manifest_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn hash_hex(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// Records the hash of content just written to `target_path`, keyed by its
/// absolute path, so a later uninstall can verify it's unchanged.
pub(crate) fn record_written(home: &Path, target_path: &Path, content: &[u8]) -> Result<()> {
    let mut manifest = load_manifest(home)?;
    manifest.insert(
        target_path.to_string_lossy().into_owned(),
        hash_hex(content),
    );
    save_manifest(home, &manifest)
}

/// Returns true if `target_path` was hand-edited since infigraph last wrote
/// it: a hash was recorded for this path, the file still exists, and its
/// current content no longer matches that recorded hash. Read-only -- unlike
/// `verify_unchanged_and_clear`, this does NOT clear the manifest entry,
/// since it's consulted before a write (install), not before a delete
/// (uninstall), and the entry must survive to be checked again next install.
///
/// This is the pre-write counterpart the install path was missing: `apply_overwrite`
/// used to write unconditionally regardless of what was on disk, so a hand-edited
/// hook (or any other `overwrite`-strategy artifact) was silently destroyed by the
/// next `infigraph install` -- the same manifest that already protected hand-edits
/// on uninstall was simply never consulted on the write side.
pub(crate) fn hand_edited_since_install(home: &Path, target_path: &Path) -> Result<bool> {
    let manifest = load_manifest(home)?;
    let key = target_path.to_string_lossy().into_owned();
    let Some(recorded_hash) = manifest.get(&key) else {
        return Ok(false);
    };
    match std::fs::read(target_path) {
        Ok(on_disk) => Ok(&hash_hex(&on_disk) != recorded_hash),
        Err(_) => Ok(false), // gone -- nothing to preserve, install will just create it
    }
}

/// Returns true if `target_path` is safe to delete on uninstall: either it
/// was never recorded (an install that predates this tracking, or content
/// written outside the artifact engine), or its current on-disk content
/// still matches what was written at install time. Clears the manifest
/// entry either way, so a later reinstall starts fresh.
pub(crate) fn verify_unchanged_and_clear(home: &Path, target_path: &Path) -> Result<bool> {
    let mut manifest = load_manifest(home)?;
    let key = target_path.to_string_lossy().into_owned();
    let Some(recorded_hash) = manifest.remove(&key) else {
        return Ok(true);
    };
    save_manifest(home, &manifest)?;

    match std::fs::read(target_path) {
        Ok(on_disk) => Ok(hash_hex(&on_disk) == recorded_hash),
        Err(_) => Ok(true), // already gone -- nothing to preserve
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecorded_path_is_treated_as_safe_to_delete() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("some/file.sh");
        assert!(verify_unchanged_and_clear(home.path(), &target).unwrap());
    }

    #[test]
    fn unrecorded_path_is_not_flagged_as_hand_edited() {
        // A path infigraph never wrote (predates tracking, or written outside
        // the artifact engine) has nothing to compare against -- treat as clean.
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/never-tracked.sh");
        assert!(!hand_edited_since_install(home.path(), &target).unwrap());
    }

    #[test]
    fn unchanged_content_is_not_flagged_as_hand_edited() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        assert!(!hand_edited_since_install(home.path(), &target).unwrap());
    }

    #[test]
    fn modified_content_is_flagged_as_hand_edited() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        std::fs::write(&target, b"user edited this").unwrap();

        assert!(hand_edited_since_install(home.path(), &target).unwrap());
    }

    #[test]
    fn hand_edited_check_does_not_clear_the_manifest_entry() {
        // Unlike verify_unchanged_and_clear (uninstall-side), this is consulted
        // before a write, not before a delete -- the entry must survive so the
        // next install can check it again.
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        std::fs::write(&target, b"user edited this").unwrap();
        hand_edited_since_install(home.path(), &target).unwrap();

        assert_eq!(load_manifest(home.path()).unwrap().len(), 1);
    }

    #[test]
    fn deleted_file_is_not_flagged_as_hand_edited() {
        // Nothing on disk to preserve or compare -- install will just recreate it.
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        std::fs::remove_file(&target).unwrap();

        assert!(!hand_edited_since_install(home.path(), &target).unwrap());
    }

    #[test]
    fn unchanged_content_is_safe_to_delete() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        assert!(verify_unchanged_and_clear(home.path(), &target).unwrap());
    }

    #[test]
    fn modified_content_is_not_safe_to_delete() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        std::fs::write(&target, b"user edited this").unwrap();

        assert!(!verify_unchanged_and_clear(home.path(), &target).unwrap());
    }

    #[test]
    fn verify_clears_the_manifest_entry_regardless_of_verdict() {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("hooks/script.sh");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"original").unwrap();
        record_written(home.path(), &target, b"original").unwrap();

        verify_unchanged_and_clear(home.path(), &target).unwrap();

        assert!(load_manifest(home.path()).unwrap().is_empty());
    }
}
