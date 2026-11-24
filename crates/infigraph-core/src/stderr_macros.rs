//! `eprintln!` and `eprint!` that a long-running process can make unable to panic.
//!
//! std's `eprintln!` panics when the write to stderr fails -- a log file on a full disk or past its size
//! limit, a pipe or socket whose client went away -- so a daemon or an MCP server died of its own logging,
//! and its replacement died the same way while the cause lasted. These macros hand their arguments to
//! `proclog::eprintln_args` / `eprint_args`, which does one of two things:
//!
//! * by default it calls std's own macro, so tests (libtest captures it) and foreground commands (which
//!   should fail loudly on a closed pipe) behave exactly as before;
//! * once the process's own `main` has called `proclog::keep_running_on_stderr_failure()`, it writes through
//!   `proclog::stderr_line` / `stderr_text`, which ignore the error, in one `write_all` per line so that
//!   concurrent lines do not interleave.
//!
//! The switch is a flag in the process, set by the process: nothing inherits it, and a daemon a test spawns
//! turns it on by itself. A crate takes the macros by naming them on its `extern crate` at its root
//! (`#[macro_use(eprintln, eprint)] extern crate infigraph_core;`; core itself declares this module first,
//! `#[macro_use]`), which shadows std's in every module below with no change to a call site. `eprintln_guard`
//! fails when a production root lacks it or source calls std's directly. `println!` is not touched: fd 1 is
//! the MCP protocol channel, and a daemon's stdout is /dev/null.

/// Like `std::eprintln!`, but a long-running process that asked for it survives a failed write.
#[macro_export]
macro_rules! eprintln {
    () => {
        $crate::proclog::eprintln_args(::std::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::proclog::eprintln_args(::std::format_args!($($arg)*))
    };
}

/// Like `std::eprint!`, but a long-running process that asked for it survives a failed write.
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::proclog::eprint_args(::std::format_args!($($arg)*))
    };
}
