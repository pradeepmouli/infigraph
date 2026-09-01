# Stale-build checks must compare against the installed binary, not the caller's own build

Tracking issue: [pradeepmouli/infigraph#135](https://github.com/pradeepmouli/infigraph/issues/135).

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `prune_stale_daemon` and `doctor` currently decide "is this process on a stale build?" by comparing the holder's recorded `build_hash` with **the judging process's own compile-time `crate::build_hash()`**. That is wrong whenever the judging process is itself the stale one — a long-running `infigraph-mcp` from before an install — and it turns a diagnostic error into a behavioral one: the old MCP SIGTERMs every daemon on the *new* build and respawns from the on-disk (new) binary, which it will judge stale again on the next call. Fix both call sites to compare against the hash of the **installed CLI binary on disk**, obtained the way #134's daemon self-check already does (spawn `infigraph print-build-hash`).

**Evidence (2026-09-01):** `doctor` on sittir reported "holder is running build 8bd7105…, installed binary is e23f4bc…" while `~/.local/bin/infigraph print-build-hash` printed `8bd7105…` — the "installed" value was the doctor-hosting MCP's own hash (`doctor.rs:147`). Earlier the same day, sittir daemon 57424 (started manually from a newer debug build) died mid-SCIP-import with no clean-shutdown log line and was replaced by 68384 on the older installed build: exactly `recovery.rs:111` → `prune_stale_daemon` → SIGTERM → respawn from the installed binary, driven by the old-build MCP's `crate::build_hash()`.

**Architecture:** One new primitive in `crates/infigraph-core/src/daemon/mod.rs`, `pub fn installed_build_hash_of(binary: &Path) -> Option<String>` (the existing private `current_on_disk_build_hash` becomes `installed_build_hash_of(&current_exe)` — honoring the same `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` test hatch). `prune_stale_daemon` takes the installed hash as a parameter (`Option<&str>`: `None` = "could not determine" → never prune on hash grounds, only on dead PID). Its two production callers compute it from the binary they would spawn: `ensure_daemon_running_required` from `watch_binary`; MCP `recovery.rs` from `resolve_cli_binary_sibling_of(current_exe)`. `doctor::assemble_context` fills `installed_build_hash` the same way, falling back to `crate::build_hash()` only when the CLI binary cannot be located (and says so in the toolchain check line).

**Tech Stack:** Rust; `std::process::Command`; existing hidden `infigraph print-build-hash` subcommand (`crates/infigraph-cli/src/main.rs`).

## Global Constraints

- A daemon whose recorded hash equals the installed binary's hash is **current** regardless of the caller's own hash. The caller being stale is not the daemon's problem (the MCP's own staleness is a separate, existing concern: `mcp-instances` doctor check).
- `None` from `installed_build_hash_of` (binary missing, spawn failure, non-zero exit) must never be treated as "stale". Dead-PID pruning stays unconditional.
- Keep `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` as the single test hatch (already used by `crates/infigraph-core/tests/daemon_stale_build_self_check.rs` and `crates/infigraph-cli/tests/print_build_hash.rs`).
- Cost: one subprocess spawn per prune/doctor call. `prune_stale_daemon` runs only on the lock-contended path of `ensure_daemon_running_required` and once at MCP startup per registered root; acceptable. Do not add it to any per-tick path.
- Verification per crate: `env -u INFIGRAPH_WATCH_DAEMON -u INFIGRAPH_BACKEND cargo test -p <crate> -- --test-threads=1`, one crate at a time; rebuild `infigraph-cli` before `groups_watch_perf` if the debug binary is stale.

---

### Task 1: `installed_build_hash_of` primitive

**Files:** Modify `crates/infigraph-core/src/daemon/mod.rs` (around the existing `current_on_disk_build_hash`, ~line 57).

- [ ] Refactor:

```rust
/// Build hash of the binary at `binary`, as a fresh subprocess of it
/// reports via the hidden `print-build-hash` subcommand. This is the only
/// way to learn what is *installed*: `crate::build_hash()` is a compile-time
/// constant baked into whichever process is asking, which is exactly wrong
/// when that process is the one that is out of date (#134's daemon
/// self-check, and the stale-build judgments in `prune_stale_daemon` and
/// `doctor`). `None` means "couldn't check" -- callers must not treat it as
/// a mismatch.
///
/// Test hatch: `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` (a file whose
/// trimmed contents are returned instead of spawning anything).
pub fn installed_build_hash_of(binary: &std::path::Path) -> Option<String> {
    if let Ok(path) = std::env::var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE") {
        return std::fs::read_to_string(path).ok().map(|s| s.trim().to_string());
    }
    let output = std::process::Command::new(binary).arg("print-build-hash").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn current_on_disk_build_hash() -> Option<String> {
    installed_build_hash_of(&std::env::current_exe().ok()?)
}
```

- [ ] Run `cargo test -p infigraph-core --test daemon_stale_build_self_check` — unchanged behavior, must pass.

### Task 2: `prune_stale_daemon` judges against the installed hash

**Files:** Modify `crates/infigraph-core/src/daemon/lifecycle.rs` (`prune_stale_daemon` ~line 311, `ensure_daemon_running_required` ~line 133, the three unit tests ~lines 550-630); `crates/infigraph-mcp/src/recovery.rs:111`; `crates/infigraph-core/tests/watch_daemon.rs` (`ensure_daemon_running_prunes_a_dead_stale_holder_and_spawns_fresh`).

- [ ] **Failing test first** (unit test in `lifecycle.rs`'s test module, next to `prune_stale_daemon_leaves_live_current_build_alone`): write a lock file whose payload records this test process's PID with `build_hash = "installed-hash-X"`, call `prune_stale_daemon(&lock, Some("installed-hash-X"))`, assert it returns `false` and the process was not signalled — even though `"installed-hash-X" != crate::build_hash()`. Mirror the existing test's lock-writing helper exactly.
- [ ] Change the signature to `pub fn prune_stale_daemon(lock_path: &Path, installed_build_hash: Option<&str>) -> bool` and replace `if holder.build_hash == crate::build_hash()` with:

```rust
    match installed_build_hash {
        // Current relative to what is actually on disk -- leave it alone,
        // whatever *this* process was built from.
        Some(installed) if holder.build_hash == installed => return false,
        // Can't tell what's installed: never signal a live process on a
        // guess. (Dead-PID cleanup above already ran.)
        None => return false,
        Some(_) => {}
    }
```

- [ ] `ensure_daemon_running_required`: `let installed = crate::daemon::installed_build_hash_of(watch_binary); if !prune_stale_daemon(&lock_path, installed.as_deref()) { ... }`.
- [ ] `recovery.rs:111`: resolve the CLI binary the same way `tools/watch.rs::ensure_daemon_watcher` does (`resolve_cli_binary_sibling_of(&std::env::current_exe()?)`), compute `installed_build_hash_of`, pass `.as_deref()`; on any error pass `None`.
- [ ] Update the existing three unit tests and the `watch_daemon.rs` integration test to pass `Some(crate::build_hash())` / `Some(infigraph_core::build_hash())` where they previously relied on the implicit comparison (preserves their intent: "installed == this test binary's build").
- [ ] Run: `cargo test -p infigraph-core --lib lifecycle`, `cargo test -p infigraph-core --test watch_daemon`, `cargo test -p infigraph-mcp --lib`.

### Task 3: `doctor` reports the installed binary's hash

**Files:** Modify `crates/infigraph-core/src/doctor.rs` (`assemble_context` ~line 137, `check_toolchain` ~line 1084); `crates/infigraph-core/tests/doctor.rs`.

- [ ] **Failing test first** (in `tests/doctor.rs`, using the existing test helpers): with `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE` pointing at a file containing `"on-disk-hash"`, `assemble_context(DoctorScope::Global).installed_build_hash == "on-disk-hash"` (not `build_hash()`).
- [ ] `assemble_context`: `installed_build_hash: installed_cli_build_hash().unwrap_or_else(|| crate::build_hash().to_string())` where `installed_cli_build_hash()` = `std::env::current_exe().ok().and_then(|exe| crate::daemon::lifecycle::resolve_cli_binary_sibling_of(&exe).ok()).and_then(|cli| crate::daemon::installed_build_hash_of(&cli))` (for the CLI itself the sibling is itself). **Executed without a fallback flag:** `DoctorContext` is built as a struct literal in 26 places in `tests/doctor.rs`, so a new field wasn't worth the churn; the silent fallback is documented on `assemble_context` instead. Likewise `prune_stale_daemon`'s decision was extracted into the pure `holder_is_stale_build(holder, installed)` so the logic gets a real unit test (the live-PID tests can't distinguish correct logic from the process-name guard saving them).
- [ ] Run `cargo test -p infigraph-core --test doctor`.

### Task 4: docs + verification

- [ ] `docs/DESIGN-hardening.md`: add a Shipped line under R2.2.5's neighbourhood: "R2.2.6 — stale-build judgments (`prune_stale_daemon`, `doctor`) compare against the installed binary's hash (`daemon::installed_build_hash_of`), not the judging process's own compile-time hash; an out-of-date MCP no longer kills/respawns daemons on the new build."
- [ ] Per-crate suites for core, cli, mcp; `cargo fmt --all -- --check`; `cargo clippy --all-targets -- -D warnings`.
- [ ] Commit: `fix(core,mcp): judge stale builds against the installed binary, not the caller's own build`
