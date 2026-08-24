//! Size-capped log rotation (R7.3, docs/DESIGN-hardening.md §7.3,
//! github.com/pradeepmouli/infigraph#83): nothing in `.infigraph/` (or
//! `~/.infigraph/`) may grow without a cap. Append-only logs (`daemon.log`,
//! `mcp.log`, `audit.log`) had none.
//!
//! Model: rotate-at-open, one generation. Before a writer opens a log for
//! append (or a spawner opens it as a child's stderr), it calls
//! [`rotate_if_over`]; a log at/over the cap is renamed to `<name>.1`
//! (replacing the previous `.1`), so each log's total footprint is bounded
//! by construction at ~2x its cap. Rotating by rename is safe even while a
//! long-lived child holds the old fd (the daemon's stderr): the child just
//! keeps appending to the renamed `.1` file, and the next spawn starts a
//! fresh current file -- no writes are lost, nothing needs coordination.

use std::path::{Path, PathBuf};

/// The rotated-generation path for `log_path`: `<full filename>.1`
/// (appended, not `with_extension` -- same reasoning as
/// `graph::db_lock_path`).
pub fn rotated_path(log_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.1", log_path.display()))
}

/// If `log_path` is at or over `max_bytes`, rotate it aside to `<name>.1`
/// (replacing any previous `.1`). Best-effort by design: a log that can't
/// be rotated must never fail the operation being logged, so errors are
/// swallowed after a stderr note. Returns whether a rotation happened.
pub fn rotate_if_over(log_path: &Path, max_bytes: u64) -> bool {
    let Ok(meta) = std::fs::metadata(log_path) else {
        return false; // absent (or unreadable) -- nothing to rotate
    };
    if meta.len() < max_bytes {
        return false;
    }
    let dest = rotated_path(log_path);
    match std::fs::rename(log_path, &dest) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "[logrotate] could not rotate {} ({e}) -- it will keep growing until this succeeds",
                log_path.display()
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn under_cap_log_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("x.log");
        std::fs::write(&log, b"small").unwrap();
        assert!(!rotate_if_over(&log, 1000));
        assert!(log.exists());
        assert!(!rotated_path(&log).exists());
    }

    #[test]
    fn over_cap_log_rotates_to_dot_one_replacing_the_previous_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("x.log");
        std::fs::write(rotated_path(&log), b"ancient generation").unwrap();
        std::fs::write(&log, vec![b'x'; 100]).unwrap();

        assert!(rotate_if_over(&log, 100)); // inclusive boundary

        assert!(!log.exists(), "current file must have moved aside");
        assert_eq!(
            std::fs::read(rotated_path(&log)).unwrap(),
            vec![b'x'; 100],
            "the .1 generation must be the just-rotated content, ancient one replaced"
        );
    }

    #[test]
    fn missing_log_is_a_quiet_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!rotate_if_over(&tmp.path().join("never-written.log"), 10));
    }

    #[test]
    fn rotated_path_appends_rather_than_replacing_an_extension() {
        assert_eq!(
            rotated_path(Path::new("/a/watch.log")),
            PathBuf::from("/a/watch.log.1")
        );
    }
}
