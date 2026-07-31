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
            format!(
                "{} project(s) have .infigraph state but are not in the registry",
                unregistered.len()
            ),
            "run `infigraph index <path>` on each to register it",
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
        // Empty file with no readable payload: either cleanly released (normal)
        // or a stale remnant from a crashed holder that never re-acquired.
        // We can't distinguish those from the payload alone -- surface it as a
        // WARN so a human/doctor-caller can check whether it's actually stuck.
        return CheckResult::warn(
            LOCK_CATEGORY,
            label,
            "lock file exists but has no readable holder identity (empty or unparseable)",
            "if no infigraph process is running for this project, delete the lock file",
        );
    };

    if !is_pid_alive(holder.pid) {
        return CheckResult::warn(
            LOCK_CATEGORY,
            label,
            format!("holder PID {} is not running (stale lock)", holder.pid),
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
        return CheckResult::pass(
            WATCHER_CATEGORY,
            label,
            "watch.lock present but unreadable (likely cleanly released)",
        );
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

    CheckResult::pass(
        WATCHER_CATEGORY,
        label,
        format!("watcher (PID {}) alive with fresh heartbeat", holder.pid),
    )
}

pub fn check_watchers(ctx: &DoctorContext) -> Vec<CheckResult> {
    let projects = projects_in_scope(ctx);
    projects.iter().map(|p| check_one_watcher(p)).collect()
}
