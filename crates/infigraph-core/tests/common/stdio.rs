//! Capturing a child's output to a file, for tests that do not drain a pipe.
//!
//! One definition for every crate's integration tests: infigraph-core's
//! `common` module declares it, and the cli and mcp `support` modules
//! include this file by path, since integration tests of different crates
//! cannot share a module any other way.

use std::path::Path;
use std::process::Stdio;

/// A file under `dir` to hand a child as its stdout or stderr.
///
/// Files rather than `Stdio::piped()` wherever a test polls `try_wait`
/// without draining: a child that outgrows the pipe buffer blocks in
/// `write` and never exits, which turns a diagnostic into the hang the test
/// exists to catch. That buffer is not the ~64KB one might assume --
/// macOS hands out 512-byte pipes once the kernel's pipe memory is
/// exhausted, and a plain `infigraph index` prints more than that (its
/// `.infigraphignore` hint alone is ~600 bytes), so a polling test failed
/// there at its 30s budget with the child parked in `eprintln!`.
///
/// A pipe is still right for a child the test reads as it runs (a reader
/// thread, or `wait_with_output` straight after the spawn); read what this
/// one captured with [`captured_text`].
pub fn captured(dir: &Path, name: &str) -> Stdio {
    Stdio::from(std::fs::File::create(dir.join(name)).unwrap())
}

/// What a child wrote to the file [`captured`] made, lossily decoded; empty
/// when the file is missing.
pub fn captured_text(dir: &Path, name: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(dir.join(name)).unwrap_or_default()).into_owned()
}
