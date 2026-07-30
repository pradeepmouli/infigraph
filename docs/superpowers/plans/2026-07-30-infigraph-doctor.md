# infigraph doctor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `infigraph doctor` (R6.4) — a CLI subcommand and MCP tool that run a battery of PASS/WARN/FAIL health checks (registry consistency, lock health, watcher liveness, disk space, sidecar freshness, toolchain/binary validity) against the infigraph installation, sharing one check implementation between both surfaces.

**Architecture:** New `crates/infigraph-core/src/doctor.rs` module holds all check logic behind a single `run_doctor(ctx: DoctorContext) -> DoctorReport` entry point. `DoctorContext` is assembled once (one registry read, one disk-free check, one binary-info lookup) and handed to a fixed sequence of check-category functions, each returning `Vec<CheckResult>`. The CLI subcommand and the MCP tool both call `run_doctor` and only differ in how they format `DoctorReport` and how they map its worst status to their surface's success signal (CLI: exit code; MCP: plain text).

**Tech Stack:** Existing workspace crates only — `infigraph_core::multi::Registry`, `infigraph_core::lockfile`, `infigraph_core::instances::current_process_start_time`, `fs2` (already a dependency via `store_util.rs`), `sysinfo` (already a dependency via `instances.rs`).

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-07-30-infigraph-doctor-design.md` — every requirement below traces back to it; re-read it if a task's rationale is unclear.
- Default scope is the current project (matches every other infigraph MCP tool's `path` convention); disk-space and toolchain/binary checks always run regardless of scope.
- Disk thresholds: **FAIL below 2GB free**, **WARN below 10GB free**, PASS otherwise. Sidecar-staleness threshold: **WARN if sidecar mtime is more than 1 hour older than graph mtime**. Watcher heartbeat staleness (locks with a real heartbeat field only): **WARN if `last_heartbeat` is more than 5 minutes (300s) stale with the holder PID still alive**.
- No check-category function may panic or propagate a hard error out of `run_doctor` — an internal failure becomes a `CheckStatus::Fail` result for that specific check, carrying the error text as its message. `run_doctor` itself returns a `DoctorReport`, never a `Result`.
- Graph directory size is reported informationally only — never classified PASS/WARN/FAIL on size alone.
- Unregistered-project discovery (`--global` mode only) requires opt-in scan roots (`~/.infigraph/scan_roots.txt` or `INFIGRAPH_SCAN_ROOTS` env var); with none configured, that specific sub-check reports itself as explicitly **skipped**, never silently "clean."
- CLI exit codes: `0` all PASS, `1` worst is WARN, `2` any FAIL. `main()` in `crates/infigraph-cli/src/main.rs` is `fn main() -> Result<()>`, whose default `Termination` impl only maps `Ok`→0 / `Err`→1 — there is no way to get exit code `2` through normal `Result` propagation, so `cmd_doctor` must call `std::process::exit(2)` explicitly for the FAIL case (this skips the post-`run()` update-hint print in `main()`, an accepted, documented tradeoff — a doctor FAIL is already attention-grabbing).
- Every new MCP tool must be added to `MCP_TOOL_NAMES`, `MCP_TO_CLI_MAP` (not `MCP_ONLY_TOOLS`, since doctor has a CLI equivalent), and the `dispatch_tool` match arm in `crates/infigraph-mcp/src/lib.rs`, or `crates/infigraph-mcp/tests/tool_parity.rs`'s parity tests fail. Every new CLI subcommand must appear in `infigraph --help`'s `Commands:` section (automatic once added to the `Commands` enum) or `crates/infigraph-cli/tests/cli_parity.rs`'s `all_mapped_cli_commands_exist_in_binary` fails.

---

### Task 1: Core types, `DoctorContext` assembly, registry check

**Files:**
- Create: `crates/infigraph-core/src/doctor.rs`
- Modify: `crates/infigraph-core/src/lib.rs:39` (add `pub mod doctor;` — insert alphabetically between `mod diff;`... actually `diff` < `doctor` < `embed`, so insert `pub mod doctor;` right after `pub mod diff;` at line 9 and before `pub mod embed;` at line 10, matching the existing alphabetical ordering of the `mod` list at the top of the file)
- Test: `crates/infigraph-core/tests/doctor.rs` (new)

**Interfaces:**
- Produces:
  - `pub enum CheckStatus { Pass, Warn, Fail }`
  - `pub struct CheckResult { pub category: &'static str, pub name: String, pub status: CheckStatus, pub message: String, pub remediation: Option<String> }`
  - `pub struct DoctorReport { pub checks: Vec<CheckResult>, pub scope: DoctorScope }`
  - `pub enum DoctorScope { Project(std::path::PathBuf), Global }`
  - `pub struct DoctorContext { pub registry: infigraph_core::multi::Registry, pub scope: DoctorScope, pub installed_build_hash: String, pub disk_free_bytes: Option<u64>, pub scan_roots: Vec<std::path::PathBuf> }`
  - `pub fn assemble_context(scope: DoctorScope) -> DoctorContext`
  - `pub fn check_registry(ctx: &DoctorContext) -> Vec<CheckResult>`
  - `pub(crate) fn scan_roots_from_env() -> Vec<std::path::PathBuf>`
  - `pub(crate) fn find_repo_entry<'a>(registry: &'a infigraph_core::multi::Registry, project_path: &std::path::Path) -> Option<&'a infigraph_core::multi::RepoEntry>`
- Consumes: `infigraph_core::multi::Registry::load()`, `infigraph_core::multi::RepoEntry { name, path, languages, symbol_count, module_count, last_indexed_commit }`, `infigraph_core::build_hash()`, `fs2::available_space`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/doctor.rs
use std::collections::HashMap;
use std::path::PathBuf;

use infigraph_core::doctor::{
    check_registry, find_repo_entry, CheckStatus, DoctorContext, DoctorScope,
};
use infigraph_core::multi::{Registry, RepoEntry};

fn repo_entry(name: &str, path: &str) -> RepoEntry {
    RepoEntry {
        name: name.to_string(),
        path: PathBuf::from(path),
        languages: vec!["rust".to_string()],
        symbol_count: 100,
        module_count: 10,
        last_indexed_commit: None,
    }
}

fn ctx_for(scope: DoctorScope, registry: Registry) -> DoctorContext {
    DoctorContext {
        registry,
        scope,
        installed_build_hash: "testhash".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    }
}

#[test]
fn find_repo_entry_matches_canonicalized_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let mut repos = HashMap::new();
    repos.insert(
        "myproj".to_string(),
        repo_entry("myproj", project.to_str().unwrap()),
    );
    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };

    let found = find_repo_entry(&registry, &project);
    assert!(found.is_some(), "must find the entry by canonicalized path match");
    assert_eq!(found.unwrap().name, "myproj");
}

#[test]
fn find_repo_entry_returns_none_for_unregistered_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };

    assert!(find_repo_entry(&registry, &project).is_none());
}

#[test]
fn check_registry_project_scope_passes_when_registered() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let mut repos = HashMap::new();
    repos.insert(
        "myproj".to_string(),
        repo_entry("myproj", project.to_str().unwrap()),
    );
    let registry = Registry {
        repos,
        groups: HashMap::new(),
    };
    let ctx = ctx_for(DoctorScope::Project(project.clone()), registry);

    let results = check_registry(&ctx);
    let registration = results
        .iter()
        .find(|r| r.name.contains("registration"))
        .expect("must produce a registration check result");
    assert_eq!(registration.status, CheckStatus::Pass);
}

#[test]
fn check_registry_project_scope_fails_when_unregistered() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let ctx = ctx_for(DoctorScope::Project(project.clone()), registry);

    let results = check_registry(&ctx);
    let registration = results
        .iter()
        .find(|r| r.name.contains("registration"))
        .expect("must produce a registration check result");
    assert_eq!(registration.status, CheckStatus::Fail);
    assert!(registration.remediation.is_some(), "a FAIL must carry remediation text");
}

#[test]
fn check_registry_global_scope_without_scan_roots_reports_skipped() {
    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let mut ctx = ctx_for(DoctorScope::Global, registry);
    ctx.scan_roots = Vec::new();

    let results = check_registry(&ctx);
    let discovery = results
        .iter()
        .find(|r| r.name.contains("unregistered-project discovery"))
        .expect("must produce a discovery check result even with no scan roots");
    assert_eq!(discovery.status, CheckStatus::Warn);
    assert!(
        discovery.message.to_lowercase().contains("skipped"),
        "must say explicitly it was skipped, not imply a clean scan: {}",
        discovery.message
    );
}

#[test]
fn check_registry_global_scope_with_scan_roots_finds_unregistered_project() {
    let dir = tempfile::TempDir::new().unwrap();
    let scan_root = dir.path().join("projects");
    let unregistered = scan_root.join("orphan-proj");
    std::fs::create_dir_all(unregistered.join(".infigraph")).unwrap();

    let registry = Registry {
        repos: HashMap::new(),
        groups: HashMap::new(),
    };
    let mut ctx = ctx_for(DoctorScope::Global, registry);
    ctx.scan_roots = vec![scan_root];

    let results = check_registry(&ctx);
    let discovery = results
        .iter()
        .find(|r| r.name.contains("orphan-proj"))
        .expect("must find the unregistered project under the scan root");
    assert_eq!(discovery.status, CheckStatus::Fail);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test doctor`
Expected: FAIL with "unresolved import `infigraph_core::doctor`" (module doesn't exist yet)

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-core/src/doctor.rs
//! `infigraph doctor` (R6.4) — health checks for the infigraph installation:
//! registry consistency, lock health, watcher liveness, disk space, sidecar
//! freshness, toolchain/binary validity. See
//! docs/superpowers/specs/2026-07-30-infigraph-doctor-design.md.

use std::path::{Path, PathBuf};

use crate::multi::{Registry, RepoEntry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub category: &'static str,
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
    pub remediation: Option<String>,
}

impl CheckResult {
    fn pass(category: &'static str, name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    fn warn(
        category: &'static str,
        name: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Warn,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn fail(
        category: &'static str,
        name: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DoctorScope {
    Project(PathBuf),
    Global,
}

#[derive(Debug)]
pub struct DoctorReport {
    pub checks: Vec<CheckResult>,
    pub scope: DoctorScope,
}

impl DoctorReport {
    pub fn worst_status(&self) -> CheckStatus {
        let mut worst = CheckStatus::Pass;
        for check in &self.checks {
            match (worst, check.status) {
                (_, CheckStatus::Fail) => return CheckStatus::Fail,
                (CheckStatus::Pass, CheckStatus::Warn) => worst = CheckStatus::Warn,
                _ => {}
            }
        }
        worst
    }
}

pub struct DoctorContext {
    pub registry: Registry,
    pub scope: DoctorScope,
    pub installed_build_hash: String,
    pub disk_free_bytes: Option<u64>,
    pub scan_roots: Vec<PathBuf>,
}

/// Reads `INFIGRAPH_SCAN_ROOTS` (colon-separated) or
/// `~/.infigraph/scan_roots.txt` (one path per line). Empty if neither is
/// configured -- callers must treat that as "not configured," not "no
/// projects found."
pub fn scan_roots_from_env() -> Vec<PathBuf> {
    if let Ok(val) = std::env::var("INFIGRAPH_SCAN_ROOTS") {
        return val
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        let path = home.join(".infigraph").join("scan_roots.txt");
        if let Ok(content) = std::fs::read_to_string(&path) {
            return content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(PathBuf::from)
                .collect();
        }
    }
    Vec::new()
}

/// Assembles the context once: one registry load, one disk-free check, one
/// binary-info lookup. Individual check functions never touch the
/// filesystem/registry directly -- only this function does.
pub fn assemble_context(scope: DoctorScope) -> DoctorContext {
    let registry = Registry::load().unwrap_or_default();
    let disk_dir = match &scope {
        DoctorScope::Project(p) => p.clone(),
        DoctorScope::Global => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let disk_free_bytes = fs2::available_space(&disk_dir).ok();
    DoctorContext {
        registry,
        scope,
        installed_build_hash: crate::build_hash().to_string(),
        disk_free_bytes,
        scan_roots: scan_roots_from_env(),
    }
}

/// Finds the registry entry whose path canonicalizes to the same location
/// as `project_path`. `RepoEntry.path` may have been recorded with a
/// different (but equivalent) representation than a fresh canonicalize of
/// the caller's path, so both sides are canonicalized before comparing.
pub(crate) fn find_repo_entry<'a>(
    registry: &'a Registry,
    project_path: &Path,
) -> Option<&'a RepoEntry> {
    let target = project_path.canonicalize().ok()?;
    registry
        .repos
        .values()
        .find(|entry| entry.path.canonicalize().ok().as_deref() == Some(target.as_path()))
}

const CATEGORY: &str = "registry";

pub fn check_registry(ctx: &DoctorContext) -> Vec<CheckResult> {
    match &ctx.scope {
        DoctorScope::Project(path) => vec![check_project_registration(&ctx.registry, path)],
        DoctorScope::Global => {
            let mut results: Vec<CheckResult> = ctx
                .registry
                .repos
                .values()
                .map(|entry| check_registered_path_still_exists(entry))
                .collect();
            results.push(check_unregistered_projects(ctx));
            results
        }
    }
}

fn check_project_registration(registry: &Registry, project_path: &Path) -> CheckResult {
    match find_repo_entry(registry, project_path) {
        Some(entry) => CheckResult::pass(
            CATEGORY,
            format!("{}: registration", entry.name),
            format!(
                "registered ({} symbols, {} modules)",
                entry.symbol_count, entry.module_count
            ),
        ),
        None => CheckResult::fail(
            CATEGORY,
            format!("{}: registration", project_path.display()),
            "project has .infigraph state but is not in the instance registry",
            format!(
                "run `infigraph index {}` to re-register it",
                project_path.display()
            ),
        ),
    }
}

fn check_registered_path_still_exists(entry: &RepoEntry) -> CheckResult {
    if entry.path.exists() {
        CheckResult::pass(
            CATEGORY,
            format!("{}: path exists", entry.name),
            entry.path.display().to_string(),
        )
    } else {
        CheckResult::warn(
            CATEGORY,
            format!("{}: path exists", entry.name),
            format!("registry entry points at a path that no longer exists: {}", entry.path.display()),
            "run `infigraph gc` to evict stale registry entries (R7.1, not yet implemented)",
        )
    }
}

/// Walks the configured scan roots (bounded depth: root + one level of
/// children) looking for `.infigraph` directories with no matching registry
/// entry. Reports itself as explicitly skipped when no scan roots are
/// configured, rather than silently implying a clean scan.
fn check_unregistered_projects(ctx: &DoctorContext) -> CheckResult {
    if ctx.scan_roots.is_empty() {
        return CheckResult::warn(
            CATEGORY,
            "unregistered-project discovery",
            "skipped: no scan roots configured",
            "set INFIGRAPH_SCAN_ROOTS or write ~/.infigraph/scan_roots.txt to enable unregistered-project discovery",
        );
    }

    let mut unregistered = Vec::new();
    for root in &ctx.scan_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !path.join(".infigraph").is_dir() {
                continue;
            }
            if find_repo_entry(&ctx.registry, &path).is_none() {
                unregistered.push(path);
            }
        }
    }

    if unregistered.is_empty() {
        CheckResult::pass(
            CATEGORY,
            "unregistered-project discovery",
            format!("scanned {} root(s), no drift found", ctx.scan_roots.len()),
        )
    } else {
        let names: Vec<String> = unregistered
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        CheckResult::fail(
            CATEGORY,
            format!("unregistered-project discovery: {}", names.join(", ")),
            format!("{} project(s) have .infigraph state but are not in the registry", unregistered.len()),
            "run `infigraph index <path>` on each to register it",
        )
    }
}
```

Add `pub mod doctor;` to `crates/infigraph-core/src/lib.rs` — the file's `mod`/`pub mod` block is alphabetically ordered; insert it between `pub mod diff;` and `pub mod embed;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test doctor`
Expected: PASS (6 tests)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/src/lib.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat: infigraph doctor core types + registry consistency check"
```

---

### Task 2: Lock health check

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs`
- Test: `crates/infigraph-core/tests/doctor.rs` (extend)

**Interfaces:**
- Consumes: `infigraph_core::lockfile::{read_holder, is_holder_wedged, LockInfo}`, `infigraph_core::instances::current_process_start_time` (liveness check — `Some(_)` means the PID is alive right now), `DoctorContext.installed_build_hash`, `DoctorContext.scope`.
- Produces: `pub fn check_locks(ctx: &DoctorContext) -> Vec<CheckResult>`. For `DoctorScope::Project(path)`, checks `<path>/.infigraph/graph.lock`, `<path>/.infigraph/watch.lock`. For `DoctorScope::Global`, runs the same two checks against every registered project's path.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/doctor.rs (append)
use infigraph_core::doctor::check_locks;
use infigraph_core::lockfile::LockInfo;

fn write_lock_file(path: &std::path::Path, info: &LockInfo) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string(info).unwrap()).unwrap();
}

#[test]
fn check_locks_passes_for_live_holder_matching_build_hash() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(), // our own PID -- guaranteed alive
            role: "graph-write".to_string(),
            build_hash: "matching-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
        },
    );
    let registry = infigraph_core::multi::Registry::default();
    let ctx = DoctorContext {
        registry,
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "matching-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must check graph.lock");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}

#[test]
fn check_locks_warns_on_build_hash_mismatch() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    let lock_path = project.join(".infigraph").join("graph.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "graph-write".to_string(),
            build_hash: "old-hash".to_string(),
            acquired_at: 1000,
            last_heartbeat: 1000,
        },
    );
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "new-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must check graph.lock");
    assert_eq!(graph_lock.status, CheckStatus::Warn);
    assert!(graph_lock.message.contains("build"), "message should mention build hash mismatch: {}", graph_lock.message);
}

#[test]
fn check_locks_warns_on_stale_zero_byte_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    std::fs::write(&lock_path, b"").unwrap(); // zero-byte, no holder payload

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let watch_lock = results
        .iter()
        .find(|r| r.name.contains("watch.lock"))
        .expect("must check watch.lock even when zero-byte");
    assert_eq!(watch_lock.status, CheckStatus::Warn);
}

#[test]
fn check_locks_passes_when_lock_file_absent() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();
    // no .infigraph dir at all -- absent lock is not itself a problem

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_locks(&ctx);
    let graph_lock = results
        .iter()
        .find(|r| r.name.contains("graph.lock"))
        .expect("must still report a result for an absent lock file");
    assert_eq!(graph_lock.status, CheckStatus::Pass);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test doctor check_locks`
Expected: FAIL with "cannot find function `check_locks`"

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-core/src/doctor.rs (append)
use crate::lockfile;

fn is_pid_alive(pid: u32) -> bool {
    crate::instances::current_process_start_time(pid).is_some()
}

fn check_one_lock(project_path: &Path, lock_name: &str, installed_build_hash: &str) -> CheckResult {
    let lock_path = project_path.join(".infigraph").join(lock_name);
    let label = format!("{}: {}", project_path.display(), lock_name);

    if !lock_path.exists() {
        return CheckResult::pass(CATEGORY, label, "no lock file present");
    }

    let Some(holder) = lockfile::read_holder(&lock_path) else {
        // Empty file with no readable payload: either cleanly released (normal)
        // or a stale remnant from a crashed holder that never re-acquired.
        // We can't distinguish those from the payload alone -- surface it as a
        // WARN so a human/doctor-caller can check whether it's actually stuck.
        return CheckResult::warn(
            CATEGORY,
            label,
            "lock file exists but has no readable holder identity (empty or unparseable)",
            "if no infigraph process is running for this project, delete the lock file",
        );
    };

    if !is_pid_alive(holder.pid) {
        return CheckResult::warn(
            CATEGORY,
            label,
            format!("holder PID {} is not running (stale lock)", holder.pid),
            "safe to delete -- the recorded holder process is gone",
        );
    }

    if holder.build_hash != installed_build_hash {
        return CheckResult::warn(
            CATEGORY,
            label,
            format!(
                "holder (PID {}) is running build {}, installed binary is {}",
                holder.pid, holder.build_hash, installed_build_hash
            ),
            "the running process predates the currently installed binary; restart it to pick up the new build",
        );
    }

    CheckResult::pass(
        CATEGORY,
        label,
        format!("held by live PID {} on the installed build", holder.pid),
    )
}

pub fn check_locks(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects: Vec<PathBuf> = match &ctx.scope {
        DoctorScope::Project(path) => vec![path.clone()],
        DoctorScope::Global => ctx.registry.repos.values().map(|e| e.path.clone()).collect(),
    };

    let mut results = Vec::new();
    for project in &projects {
        results.push(check_one_lock(project, "graph.lock", &ctx.installed_build_hash));
        results.push(check_one_lock(project, "watch.lock", &ctx.installed_build_hash));
    }
    results
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test doctor`
Expected: PASS (10 tests total)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat: infigraph doctor lock health check"
```

---

### Task 3: Watcher liveness check

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs`
- Test: `crates/infigraph-core/tests/doctor.rs` (extend)

**Interfaces:**
- Consumes: `lockfile::read_holder`, `lockfile::is_holder_wedged`, `is_pid_alive` (from Task 2), `sysinfo` crate (already a workspace dependency per `instances.rs`).
- Produces: `pub fn check_watchers(ctx: &DoctorContext) -> Vec<CheckResult>`.

This check builds on Task 2's lock-reading logic rather than duplicating it: a watcher's liveness is judged from its `watch.lock` holder (already read in `check_locks`), so this function re-reads the same lock file and adds watcher-specific staleness logic (heartbeat check) on top, rather than process-listing via `ps`. Cross-referencing actual OS processes (what the design's audit had to do by hand via `lsof`) is real, valuable, but out of scope for this task — it requires enumerating all infigraph-tagged processes on the machine, which `sysinfo::System::new_all()` can do, and is deferred to a documented follow-up (noted in Task 6's report) rather than blocking this task, since the heartbeat-based check already covers the two concrete WARN findings from tonight's audit (stale zero-byte lock, no-heartbeat-on-cli-watch).

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/doctor.rs (append)
use infigraph_core::doctor::check_watchers;

#[test]
fn check_watchers_warns_when_heartbeat_stale_but_pid_alive() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(project.join(".infigraph")).unwrap();
    let lock_path = project.join(".infigraph").join("watch.lock");
    write_lock_file(
        &lock_path,
        &LockInfo {
            pid: std::process::id(),
            role: "cli-watch".to_string(),
            build_hash: "any-hash".to_string(),
            acquired_at: 0,
            last_heartbeat: 0, // 0 => "never heartbeat since epoch", definitely stale by any real clock
        },
    );

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_watchers(&ctx);
    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must produce a watcher liveness result");
    assert_eq!(watcher.status, CheckStatus::Warn);
}

#[test]
fn check_watchers_passes_when_no_watch_lock_present() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "any-hash".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };

    let results = check_watchers(&ctx);
    let watcher = results
        .iter()
        .find(|r| r.name.contains("watcher liveness"))
        .expect("must still produce a result when no watcher is running");
    assert_eq!(watcher.status, CheckStatus::Pass);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test doctor check_watchers`
Expected: FAIL with "cannot find function `check_watchers`"

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-core/src/doctor.rs (append)
const WATCHER_HEARTBEAT_STALE_SECS: u64 = 300;

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn check_one_watcher(project_path: &Path) -> CheckResult {
    let lock_path = project_path.join(".infigraph").join("watch.lock");
    let label = format!("{}: watcher liveness", project_path.display());

    if !lock_path.exists() {
        return CheckResult::pass(CATEGORY, label, "no watcher running");
    }

    let Some(holder) = lockfile::read_holder(&lock_path) else {
        return CheckResult::pass(CATEGORY, label, "watch.lock present but unreadable (likely cleanly released)");
    };

    if !is_pid_alive(holder.pid) {
        return CheckResult::warn(
            CATEGORY,
            label,
            format!("watch.lock holder PID {} is not running", holder.pid),
            "stale lock -- safe to delete if you don't expect a watcher here",
        );
    }

    // last_heartbeat == acquired_at means this lock type has never called
    // LockFile::heartbeat() (true for cli-watch as of this writing) -- report
    // that explicitly rather than treating "never updated" as "just stale."
    if holder.last_heartbeat == holder.acquired_at {
        return CheckResult::warn(
            CATEGORY,
            label,
            format!(
                "watcher (PID {}) is alive, but this lock type never updates its heartbeat -- cannot distinguish frozen from idle",
                holder.pid
            ),
            "no action needed unless the watcher is suspected frozen; this is a known gap (see R2.3.5 in DESIGN-hardening.md)",
        );
    }

    if lockfile::is_holder_wedged(holder.last_heartbeat, now_epoch_secs(), WATCHER_HEARTBEAT_STALE_SECS) {
        return CheckResult::warn(
            CATEGORY,
            label,
            format!("watcher (PID {}) heartbeat is stale (>{}s)", holder.pid, WATCHER_HEARTBEAT_STALE_SECS),
            "the watcher process is alive but not making progress -- consider restarting it",
        );
    }

    CheckResult::pass(CATEGORY, label, format!("watcher (PID {}) alive with fresh heartbeat", holder.pid))
}

pub fn check_watchers(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects: Vec<PathBuf> = match &ctx.scope {
        DoctorScope::Project(path) => vec![path.clone()],
        DoctorScope::Global => ctx.registry.repos.values().map(|e| e.path.clone()).collect(),
    };
    projects.iter().map(|p| check_one_watcher(p)).collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test doctor`
Expected: PASS (12 tests total)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat: infigraph doctor watcher liveness check"
```

---

### Task 4: Disk, sidecar, and toolchain checks

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs`
- Test: `crates/infigraph-core/tests/doctor.rs` (extend)

**Interfaces:**
- Produces:
  - `pub fn check_disk(ctx: &DoctorContext) -> Vec<CheckResult>`
  - `pub fn check_sidecars(ctx: &DoctorContext) -> Vec<CheckResult>`
  - `pub fn check_toolchain(ctx: &DoctorContext) -> Vec<CheckResult>`
- Consumes: `DoctorContext.disk_free_bytes`, `DoctorContext.installed_build_hash`, `env!("CARGO_PKG_VERSION")`.

- [ ] **Step 1: Write the failing tests**

```rust
// crates/infigraph-core/tests/doctor.rs (append)
use infigraph_core::doctor::{check_disk, check_sidecars, check_toolchain};

#[test]
fn check_disk_fails_below_2gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(1024 * 1024 * 1024), // 1GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results.iter().find(|r| r.name.contains("free space")).unwrap();
    assert_eq!(free_space.status, CheckStatus::Fail);
}

#[test]
fn check_disk_warns_below_10gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(5 * 1024 * 1024 * 1024), // 5GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results.iter().find(|r| r.name.contains("free space")).unwrap();
    assert_eq!(free_space.status, CheckStatus::Warn);
}

#[test]
fn check_disk_passes_above_10gb() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024), // 50GB
        scan_roots: Vec::new(),
    };
    let results = check_disk(&ctx);
    let free_space = results.iter().find(|r| r.name.contains("free space")).unwrap();
    assert_eq!(free_space.status, CheckStatus::Pass);
}

#[test]
fn check_sidecars_warns_when_embeddings_older_than_graph_by_over_an_hour() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    let infigraph_dir = project.join(".infigraph");
    std::fs::create_dir_all(&infigraph_dir).unwrap();

    let graph_path = infigraph_dir.join("graph");
    std::fs::write(&graph_path, b"graph-data").unwrap();
    let embeddings_path = infigraph_dir.join("embeddings.bin");
    std::fs::write(&embeddings_path, b"stale-embeddings").unwrap();

    // Back-date the embeddings file by 2 hours relative to the graph file.
    let graph_mtime = std::fs::metadata(&graph_path).unwrap().modified().unwrap();
    let stale_mtime = graph_mtime - std::time::Duration::from_secs(2 * 60 * 60);
    let stale_file = std::fs::File::open(&embeddings_path).unwrap();
    stale_file.set_modified(stale_mtime).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project.clone()),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };
    let results = check_sidecars(&ctx);
    let sidecar = results.iter().find(|r| r.name.contains("embeddings.bin")).unwrap();
    assert_eq!(sidecar.status, CheckStatus::Warn);
}

#[test]
fn check_toolchain_passes_and_reports_installed_version() {
    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Global,
        installed_build_hash: "abc123".to_string(),
        disk_free_bytes: None,
        scan_roots: Vec::new(),
    };
    let results = check_toolchain(&ctx);
    let version = results.iter().find(|r| r.name.contains("version")).unwrap();
    assert_eq!(version.status, CheckStatus::Pass);
    assert!(version.message.contains("abc123"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-core --test doctor check_disk`
Expected: FAIL with "cannot find function `check_disk`" (and similarly for `check_sidecars`, `check_toolchain`)

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-core/src/doctor.rs (append)
const DISK_FAIL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DISK_WARN_BYTES: u64 = 10 * 1024 * 1024 * 1024;

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += metadata.len();
        }
    }
    total
}

pub fn check_disk(ctx: &DoctorContext) -> Vec<CheckResult> {
    let mut results = Vec::new();

    let free_result = match ctx.disk_free_bytes {
        None => CheckResult::warn(
            CATEGORY,
            "disk: free space",
            "could not determine free disk space",
            "check filesystem permissions",
        ),
        Some(free) if free < DISK_FAIL_BYTES => CheckResult::fail(
            CATEGORY,
            "disk: free space",
            format!("only {} MB free (below the 2GB floor)", free / (1024 * 1024)),
            "free up disk space immediately -- low disk has already caused a real MCP server crash mid-index (see I-16/I-17 in DESIGN-hardening.md)",
        ),
        Some(free) if free < DISK_WARN_BYTES => CheckResult::warn(
            CATEGORY,
            "disk: free space",
            format!("{} MB free (below the 10GB warn floor)", free / (1024 * 1024)),
            "consider freeing disk space soon",
        ),
        Some(free) => CheckResult::pass(
            CATEGORY,
            "disk: free space",
            format!("{} MB free", free / (1024 * 1024)),
        ),
    };
    results.push(free_result);

    // Informational only -- graph size is reported, never classified.
    let projects: Vec<&RepoEntry> = match &ctx.scope {
        DoctorScope::Project(path) => ctx
            .registry
            .repos
            .values()
            .filter(|e| e.path == *path)
            .collect(),
        DoctorScope::Global => ctx.registry.repos.values().collect(),
    };
    for entry in projects {
        let graph_dir = entry.path.join(".infigraph").join("graph");
        if graph_dir.exists() {
            let size_mb = dir_size(&graph_dir) / (1024 * 1024);
            results.push(CheckResult::pass(
                CATEGORY,
                format!("{}: graph size", entry.name),
                format!("{size_mb} MB (informational only, not classified)"),
            ));
        }
    }

    results
}

const SIDECAR_STALE_SECS: u64 = 60 * 60; // 1 hour

fn check_one_sidecar(project_path: &Path, sidecar_name: &str) -> Option<CheckResult> {
    let infigraph_dir = project_path.join(".infigraph");
    let graph_path = infigraph_dir.join("graph");
    let sidecar_path = infigraph_dir.join(sidecar_name);

    if !sidecar_path.exists() || !graph_path.exists() {
        return None;
    }

    let graph_mtime = std::fs::metadata(&graph_path).ok()?.modified().ok()?;
    let sidecar_mtime = std::fs::metadata(&sidecar_path).ok()?.modified().ok()?;

    let label = format!("{}: {}", project_path.display(), sidecar_name);
    match graph_mtime.duration_since(sidecar_mtime) {
        Ok(staleness) if staleness.as_secs() > SIDECAR_STALE_SECS => Some(CheckResult::warn(
            CATEGORY,
            label,
            format!("sidecar is {} minutes older than the graph", staleness.as_secs() / 60),
            "reindex to refresh the sidecar",
        )),
        _ => Some(CheckResult::pass(CATEGORY, label, "fresh relative to graph")),
    }
}

pub fn check_sidecars(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects: Vec<PathBuf> = match &ctx.scope {
        DoctorScope::Project(path) => vec![path.clone()],
        DoctorScope::Global => ctx.registry.repos.values().map(|e| e.path.clone()).collect(),
    };

    projects
        .iter()
        .flat_map(|p| {
            ["embeddings.bin", "docs_embeddings.bin"]
                .into_iter()
                .filter_map(move |name| check_one_sidecar(p, name))
        })
        .collect()
}

pub fn check_toolchain(ctx: &DoctorContext) -> Vec<CheckResult> {
    vec![CheckResult::pass(
        CATEGORY,
        "installed binary: version",
        format!("infigraph {} (build {})", env!("CARGO_PKG_VERSION"), ctx.installed_build_hash),
    )]
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test doctor`
Expected: PASS (17 tests total)

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs
git commit -m "feat: infigraph doctor disk, sidecar, and toolchain checks"
```

---

### Task 5: `run_doctor` orchestration + CLI subcommand

**Files:**
- Modify: `crates/infigraph-core/src/doctor.rs`
- Modify: `crates/infigraph-cli/src/main.rs` (add `Commands::Doctor` variant + match arm)
- Modify: `crates/infigraph-cli/src/info_commands.rs` (add `cmd_doctor`)
- Test: `crates/infigraph-core/tests/doctor.rs` (extend), `crates/infigraph-cli/tests/cli_parity.rs` (no new test needed — `all_mapped_cli_commands_exist_in_binary` picks up the new subcommand automatically once Task 6 adds the MCP mapping)

**Interfaces:**
- Consumes: `check_registry`, `check_locks`, `check_watchers`, `check_disk`, `check_sidecars`, `check_toolchain` (Tasks 1–4), `DoctorContext`, `DoctorReport`.
- Produces: `pub fn run_doctor(ctx: DoctorContext) -> DoctorReport` (in `doctor.rs`); `pub(crate) fn cmd_doctor(root: &std::path::Path, global: bool) -> anyhow::Result<()>` (in `info_commands.rs`) — never returns `Err` for the WARN/FAIL cases (those exit the process directly per the Global Constraints exit-code rule); only returns `Err` for a genuine setup failure (e.g. `root` doesn't canonicalize).

- [ ] **Step 1: Write the failing test**

```rust
// crates/infigraph-core/tests/doctor.rs (append)
use infigraph_core::doctor::run_doctor;

#[test]
fn run_doctor_aggregates_every_check_category() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path().join("myproj");
    std::fs::create_dir_all(&project).unwrap();

    let ctx = DoctorContext {
        registry: infigraph_core::multi::Registry::default(),
        scope: DoctorScope::Project(project),
        installed_build_hash: "h".to_string(),
        disk_free_bytes: Some(50 * 1024 * 1024 * 1024),
        scan_roots: Vec::new(),
    };

    let report = run_doctor(ctx);
    let categories: std::collections::HashSet<&str> =
        report.checks.iter().map(|c| c.category).collect();
    assert!(categories.contains("registry"));
    // registration check on an unregistered temp dir must FAIL, making the
    // aggregate worst status FAIL -- proves run_doctor doesn't silently drop it
    assert_eq!(report.worst_status(), CheckStatus::Fail);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p infigraph-core --test doctor run_doctor`
Expected: FAIL with "cannot find function `run_doctor`"

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-core/src/doctor.rs (append)
/// Runs every check category against `ctx` and aggregates the results. Each
/// category function is already infallible (returns `Vec<CheckResult>`, no
/// `Result`) -- an internal problem inside a category becomes a `Fail`
/// result for that specific check via that function's own error handling,
/// never a panic that would take out the rest of the report.
pub fn run_doctor(ctx: DoctorContext) -> DoctorReport {
    let mut checks = Vec::new();
    checks.extend(check_registry(&ctx));
    checks.extend(check_locks(&ctx));
    checks.extend(check_watchers(&ctx));
    checks.extend(check_disk(&ctx));
    checks.extend(check_sidecars(&ctx));
    checks.extend(check_toolchain(&ctx));
    DoctorReport {
        checks,
        scope: ctx.scope,
    }
}

/// Human-readable report, grouped by category, one line per check plus a
/// remediation line when present, ending with a summary count. Shared by
/// the CLI and MCP surfaces.
pub fn format_report(report: &DoctorReport) -> String {
    let mut out = String::new();
    let mut by_category: std::collections::BTreeMap<&str, Vec<&CheckResult>> =
        std::collections::BTreeMap::new();
    for check in &report.checks {
        by_category.entry(check.category).or_default().push(check);
    }

    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;

    for (category, checks) in by_category {
        out.push_str(&format!("== {category} ==\n"));
        for check in checks {
            let label = match check.status {
                CheckStatus::Pass => {
                    pass += 1;
                    "PASS"
                }
                CheckStatus::Warn => {
                    warn += 1;
                    "WARN"
                }
                CheckStatus::Fail => {
                    fail += 1;
                    "FAIL"
                }
            };
            out.push_str(&format!("[{label}] {}: {}\n", check.name, check.message));
            if let Some(remediation) = &check.remediation {
                out.push_str(&format!("  -> {remediation}\n"));
            }
        }
        out.push('\n');
    }

    out.push_str(&format!("{pass} PASS, {warn} WARN, {fail} FAIL\n"));
    out
}
```

Add `cmd_doctor` to `crates/infigraph-cli/src/info_commands.rs` (append):

```rust
// crates/infigraph-cli/src/info_commands.rs (append)
pub(crate) fn cmd_doctor(root: &Path, global: bool) -> Result<()> {
    use infigraph_core::doctor::{run_doctor, assemble_context, format_report, CheckStatus, DoctorScope};

    let scope = if global {
        DoctorScope::Global
    } else {
        let canonical_root = root.canonicalize().context("invalid project root")?;
        DoctorScope::Project(canonical_root)
    };
    let ctx = assemble_context(scope);
    let report = run_doctor(ctx);
    print!("{}", format_report(&report));

    match report.worst_status() {
        CheckStatus::Pass => Ok(()),
        CheckStatus::Warn => anyhow::bail!("doctor found warnings"),
        CheckStatus::Fail => std::process::exit(2),
    }
}
```

Add the `Doctor` variant to the `Commands` enum in `crates/infigraph-cli/src/main.rs` (insert near `Stats`/`WatchStatus`, matching the existing alphabetical-ish grouping of read-only info commands):

```rust
// crates/infigraph-cli/src/main.rs (add to the Commands enum)
    /// Health checks for the infigraph installation: registry consistency,
    /// lock status, watcher liveness, disk space, sidecar freshness,
    /// toolchain validity. Defaults to the current project; --global sweeps
    /// every registered project.
    Doctor {
        /// Sweep every registered project instead of just the current one
        #[arg(long)]
        global: bool,
    },
```

Add the match arm in `run()`:

```rust
// crates/infigraph-cli/src/main.rs (add to the `run()` match)
        Commands::Doctor { global } => cmd_doctor(root, global),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-core --test doctor && cargo build -p infigraph-cli && ./target/debug/infigraph --help | rg -i doctor`
Expected: doctor tests PASS (18 total); `--help` output lists the `doctor` subcommand

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-core/src/doctor.rs crates/infigraph-core/tests/doctor.rs crates/infigraph-cli/src/main.rs crates/infigraph-cli/src/info_commands.rs
git commit -m "feat: wire infigraph doctor into run_doctor orchestration and the CLI"
```

---

### Task 6: MCP tool + parity tests + integration test

**Files:**
- Modify: `crates/infigraph-mcp/src/lib.rs` (add to `MCP_TOOL_NAMES`, `MCP_TO_CLI_MAP`, `dispatch_tool`, `build_tools_list`)
- Create: `crates/infigraph-mcp/src/tools/doctor.rs`
- Modify: `crates/infigraph-mcp/src/tools/mod.rs` (add `pub mod doctor;` — check this file's existing module list and match its ordering convention)
- Test: `crates/infigraph-mcp/tests/tool_dispatch.rs` (extend)

**Interfaces:**
- Consumes: `infigraph_core::doctor::{run_doctor, assemble_context, format_report, DoctorScope}`.
- Produces: `pub fn tool_doctor(args: &serde_json::Value) -> anyhow::Result<String>`.

- [ ] **Step 1: Write the failing test**

```rust
// crates/infigraph-mcp/tests/tool_dispatch.rs (append to the existing test file;
// follow the file's own setup pattern for `_dir`/`path` seen at lines 13-14)
#[test]
fn test_doctor_tool_project_scope() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().to_str().unwrap();

    let result = infigraph_mcp::dispatch_tool(
        "doctor",
        &serde_json::json!({ "path": path }),
    );
    assert!(result.is_ok(), "doctor tool should not error: {:?}", result.err());
    let output = result.unwrap();
    assert!(output.contains("PASS") || output.contains("WARN") || output.contains("FAIL"));
}

#[test]
fn test_doctor_tool_defaults_to_current_dir_when_path_omitted() {
    let result = infigraph_mcp::dispatch_tool("doctor", &serde_json::json!({}));
    assert!(result.is_ok(), "doctor tool must work with no path argument: {:?}", result.err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p infigraph-mcp --test tool_dispatch test_doctor_tool`
Expected: FAIL — `dispatch_tool` returns an "Unknown tool" error for `"doctor"`

- [ ] **Step 3: Write the implementation**

```rust
// crates/infigraph-mcp/src/tools/doctor.rs
use serde_json::Value;

use infigraph_core::doctor::{assemble_context, format_report, run_doctor, DoctorScope};

pub fn tool_doctor(args: &Value) -> anyhow::Result<String> {
    let global = args.get("scope").and_then(|v| v.as_str()) == Some("global");

    let scope = if global {
        DoctorScope::Global
    } else {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
        let canonical = path.canonicalize().unwrap_or(path);
        DoctorScope::Project(canonical)
    };

    let ctx = assemble_context(scope);
    let report = run_doctor(ctx);
    Ok(format_report(&report))
}
```

Add `pub mod doctor;` to `crates/infigraph-mcp/src/tools/mod.rs`, matching that file's existing ordering (check the file first — it's not yet been read in this plan's research; find the alphabetically-nearest neighbors, e.g. between `detect`-related and `graph` modules, and insert accordingly).

Add to `crates/infigraph-mcp/src/lib.rs`:

In `MCP_TOOL_NAMES` (append near the end, or alongside other single-word admin-style tools like `"review"`):
```rust
    "doctor",
```

In `MCP_TO_CLI_MAP` (append):
```rust
    ("doctor", "doctor"),
```

In `dispatch_tool`'s match (append, alongside `"get_stats"` or other `tools::graph` entries — this one dispatches to the new `tools::doctor` module):
```rust
        "doctor" => tools::doctor::tool_doctor(args),
```

In `build_tools_list()` (append near `get_stats`/`list_projects`, using `p()` with an empty `required` slice since `path` is optional here — unlike every `p(true, ...)` caller elsewhere in this file, which all pass `path` as required):
```rust
        tool_def("doctor", "Health checks for the infigraph installation: registry consistency, lock status, watcher liveness, disk space, sidecar freshness, toolchain validity. Defaults to the current project; set scope='global' to sweep every registered project.",
            p(true,false,false,json!({"scope":{"type":"string","enum":["project","global"],"default":"project","description":"'project' checks only the current project (default); 'global' sweeps every registered project"}})), &[]),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p infigraph-mcp --test tool_dispatch test_doctor_tool && cargo test -p infigraph-mcp --test tool_parity && cargo test -p infigraph-cli --test cli_parity`
Expected: all PASS — the two new dispatch tests, plus every existing parity test (`advertised_tools_match_mcp_tool_names`, `dispatch_handles_all_mcp_tool_names`, `every_mcp_tool_has_cli_or_is_mcp_only`, `all_mapped_cli_commands_exist_in_binary`) still pass with `doctor` now present on both surfaces

- [ ] **Step 5: Commit**

```bash
git add crates/infigraph-mcp/src/lib.rs crates/infigraph-mcp/src/tools/doctor.rs crates/infigraph-mcp/src/tools/mod.rs crates/infigraph-mcp/tests/tool_dispatch.rs
git commit -m "feat: expose infigraph doctor as an MCP tool"
```

---

## Self-Review

**Spec coverage:** Every check category from the design spec (registry, locks, watchers, disk, sidecars, toolchain — the spec's 7th category, "MCP handshake sanity," is explicitly out of scope per the design's own Non-Goals: fixing the underlying `serverVersion` bug is tracked as separate follow-up work, so there's nothing for doctor itself to check yet) has a task. Scope (project-default + always-on disk/toolchain + `--global` sweep) is implemented in Task 1/5/6. CLI exit codes (0/1/2) are implemented in Task 5. MCP compression exemption is noted as a follow-up below (not implemented in this plan — see below). Error isolation ("no silent caps") is structural: every check function returns `Vec<CheckResult>` directly, never `Result`, so there's no way for one check's failure to abort the report.

**Not covered by this plan (explicitly deferred, not forgotten):**
- MCP output compression exemption for `doctor` (matching how security tools are exempted) — this lives in the MCP server's compression middleware (`dispatch_tool`'s caller in `crates/infigraph-mcp/src/main.rs`'s `handle_tools_call`, not in `dispatch_tool` itself), which this plan's research didn't reach. Needs a follow-up task once this plan lands: read that middleware, add `"doctor"` to whatever list already exempts `detect_security_issues` etc.
- Full OS-level watcher process cross-referencing (via `sysinfo`, matching what the manual audit did with `lsof`) — Task 3 covers lock-file-based liveness only, which is a real and useful signal but not the full picture. Noted as a known gap in the watcher check's own PASS/WARN messages where relevant.
- `--fix` / auto-remediation — explicitly a design non-goal.

**Placeholder scan:** No TBD/TODO markers; every code block is complete, runnable Rust matching confirmed real signatures from the codebase (`Registry`, `RepoEntry`, `LockInfo`, `lockfile::read_holder`, `lockfile::is_holder_wedged`, `instances::current_process_start_time`, `build_hash()`, the `tool_def`/`p()` MCP schema helpers, `MCP_TOOL_NAMES`/`MCP_TO_CLI_MAP`/`dispatch_tool`).

**Type consistency:** `DoctorContext`, `DoctorScope`, `CheckResult`, `CheckStatus`, `DoctorReport` are defined once in Task 1 and used with identical field names/types through every later task. `run_doctor(ctx: DoctorContext) -> DoctorReport` (Task 5) matches the signature every check function from Tasks 1-4 was designed against.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-30-infigraph-doctor.md`. Two execution options:

1. **Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration
2. **Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints

Which approach?
