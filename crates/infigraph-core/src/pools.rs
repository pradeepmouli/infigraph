//! How many threads this process may use for its heavy work: lbug's query
//! workers and rayon's global pool. One budget, one place.
//!
//! lbug sizes its task scheduler to every hardware thread unless told
//! otherwise (`SystemConfig::max_num_threads(0)` = auto), and rayon's global
//! pool does the same, so a daemon that rebuilds an index or imports a SCIP
//! run took every core of a machine it shares with editors, compilers and a
//! dozen other daemons (693% CPU on a loaded machine, 2026-10-18). A process
//! that works in the background -- the daemon, the MCP server -- therefore
//! takes a bounded share by default; a command a person waits on (`infigraph
//! index`, `search`) keeps every core, as it always has.
//!
//! `[index] max_threads` (env `INFIGRAPH_INDEX_MAX_THREADS`, user layer only:
//! a budget is a property of the machine) overrides the default for every
//! process: `0` means "the role's default", any other value is the exact
//! count.

use std::sync::OnceLock;

use crate::settings_file::ConfigScope;

/// What a process is for, which decides its default thread budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A command a person waits on, or a test: every core, as before.
    Foreground,
    /// The daemon and the MCP server: a bounded share of the machine.
    Background,
}

static ROLE: OnceLock<Role> = OnceLock::new();

/// Declares this process's role. Called once, first thing in `main`, before
/// anything opens a `Database` or touches rayon; a second call changes nothing.
pub fn set_role(role: Role) {
    let _ = ROLE.set(role);
}

fn role() -> Role {
    ROLE.get().copied().unwrap_or(Role::Foreground)
}

crate::settings! {
    index {
        // Threads a heavy path may use. 0 = the role's default.
        max_threads: u64 = 0,
    }
}

/// The background default for a machine with `cores` hardware threads: a
/// quarter of them, but at least two (an import must make progress) and at
/// most four (past that it is only contention).
pub fn background_default(cores: usize) -> usize {
    (cores / 4).clamp(2, 4)
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// The thread budget for `role` given the configured value (`0` = unset).
fn budget(role: Role, configured: u64, cores: usize) -> usize {
    match (configured, role) {
        (0, Role::Foreground) => 0,
        (0, Role::Background) => background_default(cores),
        (n, _) => n as usize,
    }
}

/// This process's thread budget; `0` means "no bound, use the library's own
/// default" (what a foreground command has always had).
pub fn max_threads() -> usize {
    let configured = Index::resolve_or_default(RawIndex::default(), ConfigScope::User).max_threads;
    budget(role(), configured, cores())
}

/// `config` with this process's thread budget applied: the one place a
/// `Database` is told how many query workers it may start. Every open site
/// passes its `SystemConfig` through here (lbug otherwise sizes the scheduler
/// to every hardware thread).
pub fn lbug_config(config: kuzu::SystemConfig) -> kuzu::SystemConfig {
    config.max_num_threads(max_threads() as u64)
}

/// Builds rayon's global pool for this process: the budget above and the
/// 32 MiB stacks the ANTLR parsers need (rayon's 2 MiB default overflows, and
/// so does Windows' 1 MiB main-thread stack). Called once, from `main`.
pub fn init_global_rayon() {
    let threads = max_threads();
    let mut builder = rayon::ThreadPoolBuilder::new().stack_size(32 * 1024 * 1024);
    if threads > 0 {
        builder = builder.num_threads(threads);
    }
    let _ = builder.build_global();
}

/// The env var that overrides `[index] max_threads`, derived from the settings
/// definition so a caller never spells it as a literal.
pub fn max_threads_env_name() -> String {
    crate::settings::env_name("index", "max_threads")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_process_takes_a_quarter_of_the_cores_between_two_and_four() {
        for (cores, want) in [(1, 2), (2, 2), (8, 2), (12, 3), (16, 4), (64, 4)] {
            assert_eq!(background_default(cores), want, "{cores} cores");
        }
    }

    /// The setting reaches `max_threads()` through the environment, like the
    /// other `[index]` settings, and `0` leaves a foreground process unbounded.
    #[test]
    fn the_environment_sets_the_budget() {
        use crate::settings_file::test_support::{PinnedHome, ENV_LOCK};
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _home = PinnedHome::empty();
        std::env::remove_var(max_threads_env_name());
        assert_eq!(max_threads(), 0, "a test process is foreground: unbounded");
        std::env::set_var(max_threads_env_name(), "3");
        assert_eq!(max_threads(), 3);
        std::env::set_var(max_threads_env_name(), "0");
        assert_eq!(max_threads(), 0);
        std::env::remove_var(max_threads_env_name());
        assert_eq!(max_threads_env_name(), "INFIGRAPH_INDEX_MAX_THREADS");
    }

    #[test]
    fn the_budget_follows_the_role_unless_a_value_is_configured() {
        // Foreground: unbounded unless asked.
        assert_eq!(budget(Role::Foreground, 0, 16), 0);
        // Background: the clamp.
        assert_eq!(budget(Role::Background, 0, 16), 4);
        assert_eq!(budget(Role::Background, 0, 4), 2);
        // A configured count is exact for both.
        assert_eq!(budget(Role::Foreground, 6, 16), 6);
        assert_eq!(budget(Role::Background, 1, 16), 1);
    }
}
