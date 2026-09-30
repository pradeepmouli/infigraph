//! Shared by the CLI integration tests that spawn the real binary: each
//! declares `mod support;`.

#![allow(dead_code)] // every test file compiles its own copy, and uses some of it

use std::process::Command;

/// The `infigraph` binary under test, with implicit SCIP enrichment off.
///
/// `infigraph index` otherwise leaves a detached `scip-enrich` child running
/// after it returns, and that child keeps writing into the throwaway
/// project's `.infigraph/` -- racing the tempdir's (or a `git worktree
/// remove`'s) deletion of it, and, for a language with an indexer, spending
/// minutes of CPU per fixture. The env name comes from the settings
/// definition, never a string literal here. A test that is about enrichment
/// builds its own command instead (`scip_enrich_teardown.rs`).
pub fn infigraph() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_infigraph"));
    command.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    command
}
