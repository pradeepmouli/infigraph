//! Append-only audit trail for destructive operations (R6.3,
//! docs/DESIGN-hardening.md §6.3). One line per act, to
//! `~/.infigraph/logs/audit.log`: epoch, pid, role, action, why, affected
//! path. Cheap, and turns "the registry entry is gone" into "here's the
//! line that says which process removed it and why".
//!
//! Best-effort by design: a destructive operation that already succeeded
//! must never be failed retroactively because its audit line couldn't be
//! written (full disk is exactly when destructive recovery runs). Failures
//! are printed to stderr instead of swallowed silently.

use std::io::Write;
use std::path::PathBuf;

fn audit_log_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs_next::home_dir)
        .map(|h| h.join(".infigraph").join("logs").join("audit.log"))
}

/// Append one audit line. `role` names the acting subsystem ("gc",
/// "quarantine", "restore", ...), `action` the verb ("evict-registry-entry"),
/// `why` the justification, `affected` the path or identifier acted on.
pub fn audit_log(role: &str, action: &str, why: &str, affected: &str) {
    let Some(path) = audit_log_path() else {
        eprintln!("[audit] no home directory -- cannot record: {role} {action} {affected}");
        return;
    };
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "{epoch} pid={} role={role} action={action} why={:?} affected={:?}\n",
        std::process::id(),
        why,
        affected
    );
    // R7.3 (#83): the audit trail is append-only by design but not
    // unbounded -- rotate at 5MB (one .1 generation retained; audit lines
    // are ~150 bytes, so ~2x 30k destructive acts of history survive).
    crate::logrotate::rotate_if_over(&path, 5 * 1024 * 1024);
    let result = std::fs::create_dir_all(path.parent().expect("audit path has a parent"))
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
        })
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = result {
        eprintln!("[audit] failed to append to {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HOME-dependent, so the test sets HOME to a tempdir for its own
    /// process. Serial-safe: this is the only test in the module touching
    /// HOME, and cargo runs same-binary tests in one process only when
    /// --test-threads=1 or by module isolation; the repo's suites run with
    /// --test-threads=1 per the disk-constrained convention.
    #[test]
    fn audit_lines_append_with_all_identity_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        audit_log(
            "gc",
            "evict-registry-entry",
            "path no longer exists",
            "/gone/repo",
        );
        audit_log("gc", "evict-registry-entry", "stale", "/old/repo");

        let content =
            std::fs::read_to_string(tmp.path().join(".infigraph/logs/audit.log")).unwrap();
        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        }

        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "each call must append exactly one line");
        assert!(lines[0].contains(&format!("pid={}", std::process::id())));
        assert!(lines[0].contains("role=gc"));
        assert!(lines[0].contains("action=evict-registry-entry"));
        assert!(lines[0].contains("/gone/repo"));
        assert!(lines[1].contains("/old/repo"));
    }
}
