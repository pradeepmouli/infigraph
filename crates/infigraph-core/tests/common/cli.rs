//! The `infigraph` CLI under test, with implicit SCIP enrichment off.
//!
//! infigraph-core has no dev-dependency on infigraph-cli, so
//! `env!("CARGO_BIN_EXE_infigraph")` is not available here; the binary is found
//! beside this test binary in the shared target directory. `enrichment_guard.rs`
//! requires a core test that runs an index through the binary to use
//! `cli_command()`, which is why this is the one definition.

use std::path::PathBuf;
use std::process::Command;

/// The CLI binary, from the shared target directory.
pub fn cli_binary() -> PathBuf {
    infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(
        &std::env::current_exe().unwrap(),
    )
    .expect("infigraph CLI binary must already be built (shared target dir)")
}

/// The CLI with implicit SCIP enrichment off: `infigraph index` otherwise
/// leaves a detached `scip-enrich` child running after it returns, which would
/// hold `index.lock` and keep writing into the tempdir this test is about to
/// delete. The env name comes from the settings definition.
pub fn cli_command() -> Command {
    let mut command = Command::new(cli_binary());
    command.env(infigraph_core::scip_switch::enabled_env_name(), "0");
    command
}
