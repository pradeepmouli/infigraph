//! `infigraph ps` / `infigraph kill` (R2.2.4, docs/DESIGN-hardening.md
//! §2.2, github.com/pradeepmouli/infigraph#11): enumerate every infigraph
//! process the durable state knows about -- MCP instance registrations and
//! per-project lock holders -- and terminate one safely. Turns "found 7
//! orphaned processes with ps aux" into a supported workflow.
//!
//! Ground truth, not process memory (same principle as R5.3): rows come
//! from the instance registry files and lock payloads on disk,
//! cross-checked against a fresh OS process lookup -- so a row can also
//! report a DEAD holder (stale lock), which is exactly what an operator
//! cleaning up after a crash needs to see.

use crate::multi::Registry;
use std::collections::BTreeMap;
use std::path::Path;

/// One process (or dead lock-holder) row.
#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub pid: u32,
    /// Role(s) the durable state records for this pid: an instance's
    /// transport ("mcp/stdio") and/or lock payload roles ("infigraph
    /// daemon", "graph-write", ...). Merged when one pid holds several.
    pub roles: Vec<String>,
    /// Project path(s) this pid is associated with.
    pub projects: Vec<String>,
    /// Where each claim came from ("instances/123.json", "watch.lock").
    pub evidence: Vec<String>,
    pub alive: bool,
    /// Seconds since the OS process started (alive rows only).
    pub uptime_secs: Option<u64>,
    /// Resident set size in bytes (alive rows only).
    pub rss_bytes: Option<u64>,
    /// OS start time the durable state recorded for this pid (0 = a
    /// pre-R2.1.2 payload that never captured one). When nonzero and it
    /// disagrees with the live process table, the pid was RECYCLED by an
    /// unrelated process -- the row reports dead, not the impostor's
    /// uptime/RSS.
    pub recorded_started_at: u64,
}

/// Lock files whose holder payload identifies a long-lived or in-flight
/// infigraph process for a project. `graph.lock`/`index.lock` holders are
/// normally sub-second, so a row here usually means either "an index run
/// is in progress" or "a dead holder left a stale lock" -- both worth
/// showing.
const PROJECT_LOCKS: &[&str] = &[
    "watch.lock",
    "index.lock",
    "graph.lock",
    "docs.kuzu.lock",
    "mcp.lock",
];

/// Enumerate infigraph processes recorded by the instance registry plus
/// the lock files of every project in `projects` (typically the registry's
/// repo paths plus the current project). Rows are keyed and sorted by pid;
/// one pid appearing in several sources gets its roles/projects/evidence
/// merged into a single row.
pub fn list_infigraph_processes(projects: &[&Path]) -> Vec<ProcessRow> {
    // (pid -> accumulating row), BTreeMap for stable pid ordering.
    let mut rows: BTreeMap<u32, ProcessRow> = BTreeMap::new();

    let mut add = |pid: u32, role: String, project: String, evidence: String, started_at: u64| {
        let row = rows.entry(pid).or_insert_with(|| ProcessRow {
            pid,
            roles: vec![],
            projects: vec![],
            evidence: vec![],
            alive: false,
            uptime_secs: None,
            rss_bytes: None,
            recorded_started_at: 0,
        });
        if row.recorded_started_at == 0 {
            row.recorded_started_at = started_at;
        }
        if !row.roles.contains(&role) {
            row.roles.push(role);
        }
        if !project.is_empty() && !row.projects.contains(&project) {
            row.projects.push(project);
        }
        if !row.evidence.contains(&evidence) {
            row.evidence.push(evidence);
        }
    };

    for (path, info) in crate::instances::list_instances() {
        let evidence = path
            .file_name()
            .map(|n| format!("instances/{}", n.to_string_lossy()))
            .unwrap_or_else(|| "instances/?".to_string());
        add(
            info.pid,
            format!("mcp ({})", info.transport),
            info.project_path.clone(),
            evidence,
            info.started_at,
        );
    }

    for project in projects {
        let ig = project.join(".infigraph");
        for lock_name in PROJECT_LOCKS {
            let lock_path = ig.join(lock_name);
            if let Some(holder) = crate::lockfile::read_holder(&lock_path) {
                add(
                    holder.pid,
                    holder.role.clone(),
                    project.display().to_string(),
                    (*lock_name).to_string(),
                    holder.holder_started_at,
                );
            }
        }
    }

    // One OS lookup for the whole pid set, then annotate.
    let pids: Vec<sysinfo::Pid> = rows.keys().map(|&p| sysinfo::Pid::from_u32(p)).collect();
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&pids), true);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for row in rows.values_mut() {
        if let Some(proc) = sys.process(sysinfo::Pid::from_u32(row.pid)) {
            // R2.1.2 PID-reuse guard: a recorded start time that disagrees
            // with the live process table means this is a DIFFERENT process
            // wearing a recycled pid -- report the recorded holder as dead
            // rather than the impostor's stats.
            if row.recorded_started_at != 0 && proc.start_time() != row.recorded_started_at {
                continue;
            }
            row.alive = true;
            row.uptime_secs = Some(now.saturating_sub(proc.start_time()));
            row.rss_bytes = Some(proc.memory());
        }
    }

    rows.into_values().collect()
}

/// Why a kill request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum KillRefusal {
    NoSuchProcess,
    /// The pid is alive but its binary name is not one of infigraph's --
    /// almost certainly PID reuse; terminating it would kill an innocent
    /// bystander.
    NotAnInfigraphProcess {
        actual_name: String,
    },
}

impl std::fmt::Display for KillRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KillRefusal::NoSuchProcess => write!(f, "no such process"),
            KillRefusal::NotAnInfigraphProcess { actual_name } => write!(
                f,
                "process is '{actual_name}', not an infigraph binary -- refusing \
                 (the recorded pid was probably reused by an unrelated process)"
            ),
        }
    }
}

/// Names a process must have to be killable through this path. Exact
/// matches only -- a substring check would also match test binaries
/// (`infigraph_core-<hash>`) and unrelated `infigraph-*` artifacts,
/// defeating the PID-reuse guard (same reasoning as
/// `daemon::lifecycle::prune_stale_holder`).
fn is_infigraph_binary_name(name: &str) -> bool {
    matches!(
        name,
        "infigraph" | "infigraph.exe" | "infigraph-mcp" | "infigraph-mcp.exe"
    )
}

/// Name of a live process, if it exists. Used to label a lock holder or a
/// file holder a doctor check found by pid.
pub fn process_name(pid: u32) -> Option<String> {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    sys.process(spid)
        .map(|p| p.name().to_string_lossy().to_string())
}

/// Whether `pid` is a live process running one of our binaries -- the same
/// exact-name rule `kill_infigraph_process` enforces.
pub fn is_infigraph_process(pid: u32) -> bool {
    process_name(pid)
        .map(|n| is_infigraph_binary_name(&n.to_ascii_lowercase()))
        .unwrap_or(false)
}

/// Pids of live processes holding `path` open. Best-effort and
/// platform-specific: `/proc/<pid>/fd` on Linux, `lsof -t` on macOS (absent
/// `lsof`, or on other platforms, returns empty -- callers must treat empty
/// as "unknown", not "nobody"). This is how `doctor` finds a daemon still
/// pinning a quarantined graph's inode after the file was renamed away.
pub fn pids_holding_file(path: &Path) -> Vec<u32> {
    // Only the Linux and macOS branches below read `target`; every other
    // platform returns empty, so the binding is genuinely unused there. The
    // canonicalize still runs on all platforms on purpose -- an unresolvable
    // path must return empty everywhere, not only where we can inspect fds.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(unused_variables)
    )]
    let Ok(target) = std::fs::canonicalize(path) else {
        return Vec::new();
    };
    #[cfg(target_os = "linux")]
    {
        let mut out = Vec::new();
        let Ok(procs) = std::fs::read_dir("/proc") else {
            return out;
        };
        for entry in procs.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
                continue;
            };
            if fds
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).ok().as_deref() == Some(&*target))
            {
                out.push(pid);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = std::process::Command::new("lsof")
            .arg("-t")
            .arg("--")
            .arg(&target)
            .output()
        else {
            return Vec::new();
        };
        let mut out: Vec<u32> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Terminate an infigraph process: SIGTERM by default (the watch loop and
/// MCP server both release their locks cleanly on it -- R5.4's graceful
/// path), SIGKILL with `force`. Refuses anything that isn't verifiably an
/// infigraph binary. Returns the terminated process's name on success.
pub fn kill_infigraph_process(pid: u32, force: bool) -> Result<String, KillRefusal> {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    let Some(proc) = sys.process(spid) else {
        return Err(KillRefusal::NoSuchProcess);
    };
    let name = proc.name().to_string_lossy().to_string();
    if !is_infigraph_binary_name(&name.to_ascii_lowercase()) {
        return Err(KillRefusal::NotAnInfigraphProcess { actual_name: name });
    }

    #[cfg(unix)]
    unsafe {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        libc::kill(pid as libc::pid_t, sig);
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("taskkill");
        cmd.args(["/PID", &pid.to_string()]);
        if force {
            cmd.arg("/F");
        }
        let _ = cmd.output();
    }

    Ok(name)
}

/// Convenience for the CLI: the registry's repo paths plus `current`,
/// deduped, as owned paths.
pub fn ps_scope(registry: &Registry, current: &Path) -> Vec<std::path::PathBuf> {
    let mut scope: Vec<std::path::PathBuf> =
        registry.repos.values().map(|e| e.path.clone()).collect();
    if !scope.iter().any(|p| p == current) {
        scope.push(current.to_path_buf());
    }
    scope.sort();
    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DEAD_PID: u32 = 999_999;

    fn write_lock(ig: &Path, name: &str, pid: u32, role: &str) {
        std::fs::create_dir_all(ig).unwrap();
        let info = crate::lockfile::LockInfo {
            pid,
            role: role.to_string(),
            build_hash: "test".to_string(),
            acquired_at: 0,
            last_heartbeat: 0,
            holder_started_at: 0,
        };
        std::fs::write(ig.join(name), serde_json::to_string(&info).unwrap()).unwrap();
    }

    /// Points the instance registry at an empty tempdir so the host
    /// machine's real MCP instances never leak into these assertions.
    fn isolated_instances_dir(tmp: &tempfile::TempDir) -> PathBuf {
        let dir = tmp.path().join("instances");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("INFIGRAPH_REGISTRY_INSTANCES_DIR", &dir);
        dir
    }

    #[test]
    fn live_and_dead_lock_holders_are_both_listed_with_liveness() {
        let tmp = tempfile::tempdir().unwrap();
        isolated_instances_dir(&tmp);
        let project = tmp.path().join("proj");
        let ig = project.join(".infigraph");
        write_lock(&ig, "watch.lock", std::process::id(), "infigraph daemon");
        write_lock(&ig, "graph.lock", DEAD_PID, "graph-write");

        let rows = list_infigraph_processes(&[&project]);

        assert_eq!(rows.len(), 2, "{rows:?}");
        let me = rows.iter().find(|r| r.pid == std::process::id()).unwrap();
        assert!(me.alive);
        assert_eq!(me.roles, vec!["infigraph daemon".to_string()]);
        assert!(me.uptime_secs.is_some());
        assert!(me.rss_bytes.is_some());
        let dead = rows.iter().find(|r| r.pid == DEAD_PID).unwrap();
        assert!(
            !dead.alive,
            "a stale lock's dead holder must still be listed"
        );
        assert_eq!(dead.evidence, vec!["graph.lock".to_string()]);
    }

    #[test]
    fn one_pid_holding_several_locks_merges_into_one_row() {
        let tmp = tempfile::tempdir().unwrap();
        isolated_instances_dir(&tmp);
        let project = tmp.path().join("proj");
        let ig = project.join(".infigraph");
        write_lock(&ig, "watch.lock", std::process::id(), "infigraph daemon");
        write_lock(&ig, "index.lock", std::process::id(), "infigraph daemon");

        let rows = list_infigraph_processes(&[&project]);

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(
            rows[0].evidence,
            vec!["watch.lock".to_string(), "index.lock".to_string()]
        );
        assert_eq!(
            rows[0].roles,
            vec!["infigraph daemon".to_string()],
            "identical roles must not duplicate"
        );
    }

    #[test]
    fn instance_registry_entries_are_listed_as_mcp_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = isolated_instances_dir(&tmp);
        let info = crate::instances::InstanceInfo {
            pid: DEAD_PID,
            started_at: 0,
            project_path: "/some/project".to_string(),
            transport: "stdio".to_string(),
            host_agent_hint: None,
            build_hash: "test-build".to_string(),
        };
        std::fs::write(
            dir.join(format!("{DEAD_PID}.json")),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();

        let rows = list_infigraph_processes(&[]);

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].roles, vec!["mcp (stdio)".to_string()]);
        assert_eq!(rows[0].projects, vec!["/some/project".to_string()]);
        assert!(rows[0].evidence[0].starts_with("instances/"));
    }

    #[test]
    fn kill_refuses_a_dead_pid_and_a_non_infigraph_process() {
        assert_eq!(
            kill_infigraph_process(DEAD_PID, false),
            Err(KillRefusal::NoSuchProcess)
        );
        // Our own test binary is alive but named `infigraph_core-<hash>`,
        // which the exact-name guard must reject -- this is the PID-reuse
        // protection doing its job.
        match kill_infigraph_process(std::process::id(), false) {
            Err(KillRefusal::NotAnInfigraphProcess { .. }) => {}
            other => panic!("expected NotAnInfigraphProcess, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod pid_reuse_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recycled_pid_reads_as_dead_not_as_the_impostors_stats() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("instances");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("INFIGRAPH_REGISTRY_INSTANCES_DIR", &dir);
        let project = tmp.path().join("proj");
        let ig = project.join(".infigraph");
        std::fs::create_dir_all(&ig).unwrap();
        // Our own live pid, but a recorded start time that can't match --
        // the exact shape a recycled pid leaves behind.
        let own_start = crate::instances::current_process_start_time(std::process::id()).unwrap();
        let info = crate::lockfile::LockInfo {
            pid: std::process::id(),
            role: "infigraph daemon".to_string(),
            build_hash: "test".to_string(),
            acquired_at: 0,
            last_heartbeat: 0,
            holder_started_at: own_start + 999,
        };
        std::fs::write(ig.join("watch.lock"), serde_json::to_string(&info).unwrap()).unwrap();

        let rows = list_infigraph_processes(&[Path::new(&project)]);

        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            !rows[0].alive,
            "a recycled pid must report the recorded holder as dead: {rows:?}"
        );
        assert!(rows[0].rss_bytes.is_none(), "no impostor stats: {rows:?}");
    }
}
