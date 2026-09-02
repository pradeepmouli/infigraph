//! #132: an uncaught C++ exception inside lbug calls `std::terminate`,
//! which aborts the process before any Rust `Result` or panic hook can run.
//! The daemon's only trace was libc++abi's one-liner naming the exception
//! type, never *what the daemon was doing*. `write_phase` records the
//! in-flight write phase without allocating, and the SIGABRT breadcrumb
//! prints it from the signal handler so the next abort names its phase.
#![cfg(unix)]

use infigraph_core::write_phase::{current, enter, format_abort_line, ABORT_LINE_CAP};

#[test]
fn entering_a_phase_makes_it_current_and_dropping_restores_the_previous_one() {
    assert_eq!(current(), None);
    {
        let _outer = enter(&"bulk-index: COPY FROM parquet", 120);
        assert_eq!(current(), Some(("bulk-index: COPY FROM parquet", 120)));
        {
            let _inner = enter(&"bulk-index: folders", 0);
            assert_eq!(current(), Some(("bulk-index: folders", 0)));
        }
        assert_eq!(
            current(),
            Some(("bulk-index: COPY FROM parquet", 120)),
            "dropping the inner guard restores the outer phase, not idle"
        );
    }
    assert_eq!(current(), None);
}

#[test]
fn abort_line_names_pid_phase_and_count_without_allocating() {
    let mut buf = [0u8; ABORT_LINE_CAP];
    let n = format_abort_line(&mut buf, Some(("scip-import: COPY Symbol", 8779)), 4242);
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        "[daemon] SIGABRT pid=4242 (uncaught C++ exception or abort) during write phase: \
         scip-import: COPY Symbol (n=8779)\n"
    );
    let n = format_abort_line(&mut buf, None, 7);
    assert_eq!(
        std::str::from_utf8(&buf[..n]).unwrap(),
        "[daemon] SIGABRT pid=7 (uncaught C++ exception or abort) outside any write phase\n"
    );
}

/// Re-exec helper: installs the breadcrumb, enters a phase, aborts.
#[test]
fn abort_helper() {
    if std::env::var_os("INFIGRAPH_TEST_ABORT_IN_PHASE").is_none() {
        return;
    }
    infigraph_core::write_phase::install_abort_breadcrumb();
    let _phase = enter(&"bulk-index: per-file UNWIND", 37);
    unsafe { libc::abort() };
}

#[test]
fn a_real_abort_inside_a_phase_writes_the_breadcrumb_to_stderr_and_still_dies_by_sigabrt() {
    use std::os::unix::process::ExitStatusExt;
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["abort_helper", "--exact", "--nocapture"])
        .env("INFIGRAPH_TEST_ABORT_IN_PHASE", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(
            "(uncaught C++ exception or abort) during write phase: bulk-index: per-file UNWIND (n=37)"
        ),
        "breadcrumb line missing from child stderr:\n{stderr}"
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGABRT),
        "the handler must re-raise so the process still dies by SIGABRT (status {:?})",
        out.status
    );
}
