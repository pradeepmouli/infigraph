//! Age-based cleanup for run-unique scratch files (#139, #204): a producer
//! removes its own files, and this is the backstop for one that crashed.

use std::path::Path;
use std::time::Duration;

/// Remove files in `dir` older than `age` whose extension is one of
/// `extensions`. Anything younger belongs, or may belong, to a run still in
/// progress. Returns how many were removed; a missing `dir` is zero.
pub fn sweep_older_than(dir: &Path, age: Duration, extensions: &[&str]) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let listed = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| extensions.contains(&e));
        let stale = listed
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|a| a > age);
        if stale && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_old_files_with_a_listed_extension_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("a.scip");
        let young = dir.path().join("b.scip");
        let other = dir.path().join("c.txt");
        for p in [&old, &young, &other] {
            std::fs::write(p, b"x").unwrap();
        }
        let past = std::time::SystemTime::now() - Duration::from_secs(7 * 3600);
        for p in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(past)
                .unwrap();
        }
        assert_eq!(
            sweep_older_than(dir.path(), Duration::from_secs(6 * 3600), &["scip"]),
            1
        );
        assert!(!old.exists() && young.exists() && other.exists());
        assert_eq!(
            sweep_older_than(&dir.path().join("missing"), Duration::ZERO, &["scip"]),
            0
        );
    }
}
