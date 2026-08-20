//! `infigraph doctor` (R6.4) — health checks for the infigraph installation:
//! registry consistency, lock health, watcher liveness, disk space, sidecar
//! freshness, toolchain/binary validity. See
//! docs/superpowers/specs/2026-07-30-infigraph-doctor-design.md.

use std::path::{Path, PathBuf};

use crate::instances;
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
    pub fn pass(
        category: &'static str,
        name: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category,
            name: name.into(),
            status: CheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    pub fn warn(
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

    pub fn fail(
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
pub(crate) fn scan_roots_from_env() -> Vec<PathBuf> {
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
pub fn find_repo_entry<'a>(registry: &'a Registry, project_path: &Path) -> Option<&'a RepoEntry> {
    let target = project_path.canonicalize().ok()?;
    registry
        .repos
        .values()
        .find(|entry| entry.path.canonicalize().ok().as_deref() == Some(target.as_path()))
}

/// Resolves `ctx.scope` into the list of project paths this context covers:
/// a single path for `Project`, every registered repo's path for `Global`.
/// Shared by every check category that needs to iterate "the projects in
/// scope" rather than duplicating the match on `DoctorScope`.
pub fn projects_in_scope(ctx: &DoctorContext) -> Vec<PathBuf> {
    match &ctx.scope {
        DoctorScope::Project(path) => vec![path.clone()],
        DoctorScope::Global => ctx
            .registry
            .repos
            .values()
            .map(|e| e.path.clone())
            .collect(),
    }
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
                .map(check_registered_path_still_exists)
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
        None if !project_path.join(".infigraph").exists() => CheckResult::pass(
            CATEGORY,
            format!("{}: registration", project_path.display()),
            "not an infigraph project (no .infigraph directory) -- nothing to check",
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
            format!(
                "registry entry points at a path that no longer exists: {}",
                entry.path.display()
            ),
            "run `infigraph gc` to evict it (add --stale-days N to also evict long-unindexed projects)",
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
    let mut unreadable_roots = Vec::new();
    for root in &ctx.scan_roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            unreadable_roots.push(root.display().to_string());
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

    if !unregistered.is_empty() {
        let names: Vec<String> = unregistered
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        CheckResult::fail(
            CATEGORY,
            format!("unregistered-project discovery: {}", names.join(", ")),
            format!(
                "{} project(s) have .infigraph state but are not in the registry",
                unregistered.len()
            ),
            "run `infigraph index <path>` on each to register it",
        )
    } else if !unreadable_roots.is_empty() {
        CheckResult::warn(
            CATEGORY,
            "unregistered-project discovery",
            format!(
                "scanned {} of {} root(s); could not read: {}",
                ctx.scan_roots.len() - unreadable_roots.len(),
                ctx.scan_roots.len(),
                unreadable_roots.join(", ")
            ),
            "check that the scan root(s) exist and are readable",
        )
    } else {
        CheckResult::pass(
            CATEGORY,
            "unregistered-project discovery",
            format!("scanned {} root(s), no drift found", ctx.scan_roots.len()),
        )
    }
}

const LOCK_CATEGORY: &str = "locks";

use crate::lockfile;

fn is_pid_alive(pid: u32) -> bool {
    crate::instances::current_process_start_time(pid).is_some()
}

fn check_one_lock(project_path: &Path, lock_name: &str, installed_build_hash: &str) -> CheckResult {
    let lock_path = project_path.join(".infigraph").join(lock_name);
    let label = format!("{}: {}", project_path.display(), lock_name);

    if !lock_path.exists() {
        return CheckResult::pass(LOCK_CATEGORY, label, "no lock file present");
    }

    let Some(holder) = lockfile::read_holder(&lock_path) else {
        // `LockFile::Drop` truncates the payload to zero bytes on a clean
        // release so readers never see a stale identity -- a zero-byte lock
        // is the normal, healthy post-release state, not a problem. Only a
        // non-empty payload that fails to parse indicates something is
        // actually wrong (mid-write torn read, old/incompatible format).
        let empty = std::fs::metadata(&lock_path)
            .map(|m| m.len() == 0)
            .unwrap_or(false);
        return if empty {
            CheckResult::pass(LOCK_CATEGORY, label, "lock released (empty payload)")
        } else {
            CheckResult::warn(
                LOCK_CATEGORY,
                label,
                "lock file has an unreadable holder payload",
                "if no infigraph process is running for this project, delete the lock file",
            )
        };
    };

    if !lockfile::holder_is_alive(&holder) {
        return CheckResult::warn(
            LOCK_CATEGORY,
            label,
            format!(
                "holder PID {} is not running (stale lock{})",
                holder.pid,
                if holder.holder_started_at != 0 && is_pid_alive(holder.pid) {
                    " -- the PID was recycled by an unrelated process"
                } else {
                    ""
                }
            ),
            "safe to delete -- the recorded holder process is gone",
        );
    }

    if holder.build_hash != installed_build_hash {
        return CheckResult::warn(
            LOCK_CATEGORY,
            label,
            format!(
                "holder (PID {}) is running build {}, installed binary is {}",
                holder.pid, holder.build_hash, installed_build_hash
            ),
            "the running process predates the currently installed binary; restart it to pick up the new build",
        );
    }

    CheckResult::pass(
        LOCK_CATEGORY,
        label,
        format!("held by live PID {} on the installed build", holder.pid),
    )
}

pub fn check_locks(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects = projects_in_scope(ctx);

    let mut results = Vec::new();
    for project in &projects {
        results.push(check_one_lock(
            project,
            "graph.lock",
            &ctx.installed_build_hash,
        ));
        results.push(check_one_lock(
            project,
            "watch.lock",
            &ctx.installed_build_hash,
        ));
    }
    results
}

const WATCHER_HEARTBEAT_STALE_SECS: u64 = 300;

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const WATCHER_CATEGORY: &str = "watchers";

fn check_one_watcher(project_path: &Path) -> CheckResult {
    let lock_path = project_path.join(".infigraph").join("watch.lock");
    let label = format!("{}: watcher liveness", project_path.display());

    if !lock_path.exists() {
        return CheckResult::pass(WATCHER_CATEGORY, label, "no watcher running");
    }

    let Some(holder) = lockfile::read_holder(&lock_path) else {
        // Same empty-vs-unparseable distinction as check_one_lock: a
        // zero-byte watch.lock is the normal state after a clean release.
        let empty = std::fs::metadata(&lock_path)
            .map(|m| m.len() == 0)
            .unwrap_or(false);
        return if empty {
            CheckResult::pass(WATCHER_CATEGORY, label, "watcher released (empty payload)")
        } else {
            CheckResult::warn(
                WATCHER_CATEGORY,
                label,
                "watch.lock has an unreadable holder payload",
                "if no infigraph process is running for this project, delete the lock file",
            )
        };
    };

    if !is_pid_alive(holder.pid) {
        return CheckResult::warn(
            WATCHER_CATEGORY,
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
            WATCHER_CATEGORY,
            label,
            format!(
                "watcher (PID {}) is alive, but this lock type never updates its heartbeat -- cannot distinguish frozen from idle",
                holder.pid
            ),
            "no action needed unless the watcher is suspected frozen; this is a known gap (see R2.3.5 in DESIGN-hardening.md)",
        );
    }

    if lockfile::is_holder_wedged(
        holder.last_heartbeat,
        now_epoch_secs(),
        WATCHER_HEARTBEAT_STALE_SECS,
    ) {
        return CheckResult::warn(
            WATCHER_CATEGORY,
            label,
            format!(
                "watcher (PID {}) heartbeat is stale (>{}s)",
                holder.pid, WATCHER_HEARTBEAT_STALE_SECS
            ),
            "the watcher process is alive but not making progress -- consider restarting it",
        );
    }

    if !project_has_live_mcp_instance(project_path) {
        return CheckResult::warn(
            WATCHER_CATEGORY,
            label,
            format!(
                "watcher (PID {}) is alive and healthy, but no MCP server instance is \
                 currently serving this project",
                holder.pid
            ),
            "likely left running from a closed MCP session -- if you're not also using it \
             from a standalone CLI session, it's safe to stop; a future MCP session will \
             restart it and catch up automatically",
        );
    }

    CheckResult::pass(
        WATCHER_CATEGORY,
        label,
        format!("watcher (PID {}) alive with fresh heartbeat", holder.pid),
    )
}

/// Whether any live MCP server instance (per the instance registry --
/// `instances::list_instances`/`classify_instances`) is currently serving
/// `project_path`. Used to distinguish a watch daemon that's still useful
/// (some MCP server will query it) from one left spinning for a project no
/// MCP session is using anymore. `own_pid` is passed as `0` (never a real
/// PID) since doctor is not itself an MCP instance and must not exclude a
/// genuine entry.
fn project_has_live_mcp_instance(project_path: &Path) -> bool {
    let target = project_path
        .canonicalize()
        .unwrap_or_else(|_| project_path.to_path_buf());
    let entries = instances::list_instances();
    let classified =
        instances::classify_instances(&entries, 0, instances::current_process_start_time);
    classified.iter().any(|(_, info, status)| {
        *status == instances::InstanceStatus::LivePeer
            && Path::new(&info.project_path)
                .canonicalize()
                .map(|p| p == target)
                .unwrap_or(false)
    })
}

pub fn check_watchers(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects = projects_in_scope(ctx);
    projects.iter().map(|p| check_one_watcher(p)).collect()
}

const DISK_CATEGORY: &str = "disk";

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

/// Size of `path` regardless of whether it's a file or a directory. The
/// graph store (`.infigraph/graph`) is a single file in the current layout
/// but a directory in the legacy layout -- `dir_size` alone silently reports
/// 0 for the (now-common) file case since `read_dir` on a file errors.
fn path_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        dir_size(path)
    } else {
        meta.len()
    }
}

pub fn check_disk(ctx: &DoctorContext) -> Vec<CheckResult> {
    let mut results = Vec::new();

    let free_result = match ctx.disk_free_bytes {
        None => CheckResult::warn(
            DISK_CATEGORY,
            "disk: free space",
            "could not determine free disk space",
            "check filesystem permissions",
        ),
        Some(free) if free < DISK_FAIL_BYTES => CheckResult::fail(
            DISK_CATEGORY,
            "disk: free space",
            format!("only {} MB free (below the 2GB floor)", free / (1024 * 1024)),
            "free up disk space immediately -- low disk has already caused a real MCP server crash mid-index (see I-16/I-17 in DESIGN-hardening.md)",
        ),
        Some(free) if free < DISK_WARN_BYTES => CheckResult::warn(
            DISK_CATEGORY,
            "disk: free space",
            format!("{} MB free (below the 10GB warn floor)", free / (1024 * 1024)),
            "consider freeing disk space soon",
        ),
        Some(free) => CheckResult::pass(
            DISK_CATEGORY,
            "disk: free space",
            format!("{} MB free", free / (1024 * 1024)),
        ),
    };
    results.push(free_result);

    // Informational only -- graph size is reported, never classified.
    let projects: Vec<&RepoEntry> = match &ctx.scope {
        DoctorScope::Project(path) => {
            if let Some(entry) = find_repo_entry(&ctx.registry, path) {
                vec![entry]
            } else {
                vec![]
            }
        }
        DoctorScope::Global => ctx.registry.repos.values().collect(),
    };
    for entry in projects {
        let graph_dir = entry.path.join(".infigraph").join("graph");
        if graph_dir.exists() {
            let size_mb = path_size(&graph_dir) / (1024 * 1024);
            results.push(CheckResult::pass(
                DISK_CATEGORY,
                format!("{}: graph size", entry.name),
                format!("{size_mb} MB (informational only, not classified)"),
            ));
        }
    }

    results
}

const SIDECAR_CATEGORY: &str = "sidecars";
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
            SIDECAR_CATEGORY,
            label,
            format!(
                "sidecar is {} minutes older than the graph",
                staleness.as_secs() / 60
            ),
            "reindex to refresh the sidecar",
        )),
        _ => Some(CheckResult::pass(
            SIDECAR_CATEGORY,
            label,
            "fresh relative to graph",
        )),
    }
}

pub fn check_sidecars(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects = projects_in_scope(ctx);

    projects
        .iter()
        .flat_map(|p| {
            ["embeddings.bin", "docs_embeddings.bin"]
                .into_iter()
                .filter_map(move |name| check_one_sidecar(p, name))
        })
        .collect()
}

const SCIP_STALENESS_CATEGORY: &str = "scip-staleness";

/// R3.3.4 (docs/DESIGN-hardening.md §3.3.4): compares a project's AST vs.
/// SCIP generation counters (see `GraphStore::current_ast_generation`/
/// `current_scip_generation`) and warns when SCIP enrichment has fallen
/// behind the live-watched graph -- the watcher's incremental reindex is
/// AST-only, so this drift is otherwise silent.
///
/// `scip_generation == 0` means SCIP enrichment has never run for this
/// project at all (no applicable indexer for its languages, or it just
/// hasn't happened yet) -- that's "not yet judged," not "stale," so it's
/// deliberately not reported here at all rather than as a false-positive
/// warning on every project without SCIP support.
///
/// Unlike every other doctor check, this one opens the project's graph
/// (read-only) rather than just stat-ing files -- there's no cheaper way to
/// read `GraphMeta`'s counters. Bounded by `projects_in_scope`, same as
/// every other per-project check.
fn check_one_project_scip_staleness(project_path: &Path) -> Option<CheckResult> {
    let graph_path = project_path.join(".infigraph").join("graph");
    if !graph_path.exists() {
        return None;
    }
    let store = crate::graph::GraphStore::open_read_only(&graph_path).ok()?;
    let ast_gen = store.current_ast_generation().ok()?;
    let scip_gen = store.current_scip_generation().ok()?;

    if scip_gen == 0 {
        return None;
    }

    let label = format!("{}: SCIP enrichment", project_path.display());
    if scip_gen < ast_gen {
        let behind = ast_gen - scip_gen;
        Some(CheckResult::warn(
            SCIP_STALENESS_CATEGORY,
            label,
            format!(
                "SCIP enrichment is {behind} AST generation{} behind the live graph -- \
                 INHERITS edges and other compiler-verified data may be out of date",
                if behind == 1 { "" } else { "s" }
            ),
            "run `infigraph index --full` (or re-trigger SCIP enrichment) to refresh it",
        ))
    } else {
        Some(CheckResult::pass(
            SCIP_STALENESS_CATEGORY,
            label,
            "up to date with the live graph",
        ))
    }
}

pub fn check_scip_staleness(ctx: &DoctorContext) -> Vec<CheckResult> {
    projects_in_scope(ctx)
        .iter()
        .filter_map(|p| check_one_project_scip_staleness(p))
        .collect()
}

const WORKTREE_CATEGORY: &str = "worktrees";

/// Diffs the registry against live git worktrees (always a global scan,
/// regardless of `ctx.scope`, since worktree drift is a git-structure
/// property, not a registry scan-root property).
pub fn check_worktrees(ctx: &DoctorContext) -> Vec<CheckResult> {
    let drift = crate::worktree::find_worktree_drift(&ctx.registry, None);
    let mut results = Vec::new();

    for path in &drift.bootstrap_candidates {
        results.push(CheckResult::warn(
            WORKTREE_CATEGORY,
            format!("{}: unindexed worktree", path.display()),
            "git worktree exists but has not been indexed",
            format!("run `infigraph worktree init {}`", path.display()),
        ));
    }
    for path in &drift.teardown_candidates {
        results.push(CheckResult::warn(
            WORKTREE_CATEGORY,
            format!("{}: removed worktree still registered", path.display()),
            "git no longer lists this worktree, but it has a registry entry",
            format!("run `infigraph worktree teardown {}`", path.display()),
        ));
    }
    results
}

const TOOLCHAIN_CATEGORY: &str = "toolchain";

pub fn check_toolchain(ctx: &DoctorContext) -> Vec<CheckResult> {
    vec![CheckResult::pass(
        TOOLCHAIN_CATEGORY,
        "installed binary: version",
        format!(
            "infigraph {} (build {})",
            env!("CARGO_PKG_VERSION"),
            ctx.installed_build_hash
        ),
    )]
}

/// Runs every check category against `ctx` and aggregates the results. Each
/// category function is already infallible (returns `Vec<CheckResult>`, no
/// `Result`), so no category can panic or propagate a hard error out of
/// `run_doctor` and take out the rest of the report. That's a structural
/// guarantee about the report as a whole -- it does NOT mean every internal
/// problem inside a check is guaranteed to surface as a `Fail`; individual
/// checks are free to degrade to a `Warn`, a `Pass`, or silently omit a
/// result, per that check's own judgment about what the failure means.
pub fn run_doctor(ctx: DoctorContext) -> DoctorReport {
    let mut checks = Vec::new();
    checks.extend(check_registry(&ctx));
    checks.extend(check_locks(&ctx));
    checks.extend(check_watchers(&ctx));
    checks.extend(check_disk(&ctx));
    checks.extend(check_sidecars(&ctx));
    checks.extend(check_scip_staleness(&ctx));
    checks.extend(check_worktrees(&ctx));
    checks.extend(check_toolchain(&ctx));
    DoctorReport {
        checks,
        scope: ctx.scope,
    }
}

/// Human-readable report, grouped by category, one line per check plus a
/// remediation line when present, ending with a summary count. Shared by
/// the CLI and MCP surfaces.
///
/// `color` wraps each glyph in ANSI SGR codes (green/yellow/red) when true.
/// Callers own the decision of whether color is appropriate for their
/// output stream (a real TTY vs. a pipe, a file, or MCP tool-call text) —
/// this function only renders what it's told.
pub fn format_report(report: &DoctorReport, color: bool) -> String {
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
            let (glyph, sgr) = match check.status {
                CheckStatus::Pass => {
                    pass += 1;
                    ("✓", "32")
                }
                CheckStatus::Warn => {
                    warn += 1;
                    ("!", "33")
                }
                CheckStatus::Fail => {
                    fail += 1;
                    ("✗", "31")
                }
            };
            let tag = if color {
                format!("\x1b[{sgr}m{glyph}\x1b[0m")
            } else {
                glyph.to_string()
            };
            out.push_str(&format!("[{tag}] {}: {}\n", check.name, check.message));
            if let Some(remediation) = &check.remediation {
                out.push_str(&format!("  -> {remediation}\n"));
            }
        }
        out.push('\n');
    }

    out.push_str(&format!("{pass} PASS, {warn} WARN, {fail} FAIL\n"));
    out
}
