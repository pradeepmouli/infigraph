//! Instance registry (R2.2.1) — every `infigraph-mcp` process registers
//! itself at `~/.infigraph/instances/<pid>.json` on startup and removes it
//! on clean shutdown (RAII). Orphan detection (R2.2.2) reads this registry
//! to tell a live peer from a dead-or-reused-PID orphan, via a PID-reuse
//! guard: a registry entry records the process's OS-reported start time,
//! and a fresh lookup at classification time must match it exactly — a
//! bare PID match is not proof it's the same process the entry named.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceInfo {
    pub pid: u32,
    /// Unix epoch seconds this OS process itself started, per `sysinfo` —
    /// not when this registry file was written. The PID-reuse guard
    /// compares this against a fresh lookup of the same PID.
    pub started_at: u64,
    pub project_path: String,
    pub transport: String,
    pub host_agent_hint: Option<String>,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Directory holding one JSON file per live-or-recently-live instance.
/// Overridable via `INFIGRAPH_INSTANCES_DIR` (tests).
pub fn instances_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("INFIGRAPH_INSTANCES_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".infigraph")
        .join("instances")
}

fn instance_path(pid: u32) -> PathBuf {
    instances_dir().join(format!("{pid}.json"))
}

/// Best-effort hint at which coding agent launched this process, from a
/// small set of well-known env vars. `None` if none are set — this is a
/// diagnostic aid, never load-bearing for orphan classification.
fn host_agent_hint() -> Option<String> {
    for (var, label) in [
        ("CLAUDECODE", "claude-code"),
        ("CURSOR_TRACE_ID", "cursor"),
        ("TERM_PROGRAM", "term"),
    ] {
        if std::env::var(var).is_ok() {
            return Some(label.to_string());
        }
    }
    None
}

/// Fresh lookup of a PID's OS-reported start time (Unix epoch seconds).
/// `None` if no such process exists right now.
pub fn current_process_start_time(pid: u32) -> Option<u64> {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    sys.process(spid).map(|p| p.start_time())
}

impl InstanceInfo {
    /// Builds this process's own instance record, using a fresh lookup of
    /// our own PID so `started_at` matches exactly what a later PID-reuse
    /// check will compare against.
    pub fn current(project_path: &str, transport: &str) -> Self {
        let pid = std::process::id();
        let started_at = current_process_start_time(pid).unwrap_or_else(now_epoch_secs);
        Self {
            pid,
            started_at,
            project_path: project_path.to_string(),
            transport: transport.to_string(),
            host_agent_hint: host_agent_hint(),
        }
    }
}

/// RAII guard: removes this process's instance file on drop (the
/// clean-shutdown path). A crash or `kill -9` leaves the file behind —
/// that's exactly what orphan reaping exists to clean up from the outside.
#[derive(Debug)]
pub struct InstanceGuard {
    path: PathBuf,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Writes this process's instance file and returns a guard that removes it
/// on drop.
pub fn register_instance(info: &InstanceInfo) -> Result<InstanceGuard> {
    let dir = instances_dir();
    std::fs::create_dir_all(&dir)?;
    let path = instance_path(info.pid);
    let json = serde_json::to_string_pretty(info)?;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(json.as_bytes())?;
    Ok(InstanceGuard { path })
}

/// Scans the instance registry directory. Skips entries that don't parse
/// (partial write mid-crash, or a schema from a different binary version)
/// rather than failing the whole scan.
pub fn list_instances() -> Vec<(PathBuf, InstanceInfo)> {
    let dir = instances_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|e| {
            let path = e.path();
            let content = std::fs::read_to_string(&path).ok()?;
            let info: InstanceInfo = serde_json::from_str(&content).ok()?;
            Some((path, info))
        })
        .collect()
}

/// Pure: is a registry entry stale (should be reaped)? `actual_start` is a
/// fresh lookup for the entry's `pid` — `None` means no such process
/// exists right now; `Some(t)` where `t != recorded_start` means the PID
/// was reused by an unrelated process; either case means the process the
/// entry originally named is gone.
pub fn is_stale(recorded_start: u64, actual_start: Option<u64>) -> bool {
    actual_start != Some(recorded_start)
}

/// One registry entry's classification against the current process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceStatus {
    LivePeer,
    Orphan,
}

/// Pure: classifies every entry except `own_pid` (a process never reaps
/// itself — self-termination is R2.2.3's job) via an injectable start-time
/// lookup, so this is fully testable without touching real processes.
pub fn classify_instances(
    entries: &[(PathBuf, InstanceInfo)],
    own_pid: u32,
    lookup_start_time: impl Fn(u32) -> Option<u64>,
) -> Vec<(PathBuf, InstanceInfo, InstanceStatus)> {
    entries
        .iter()
        .filter(|(_, info)| info.pid != own_pid)
        .map(|(path, info)| {
            let actual = lookup_start_time(info.pid);
            let status = if is_stale(info.started_at, actual) {
                InstanceStatus::Orphan
            } else {
                InstanceStatus::LivePeer
            };
            (path.clone(), info.clone(), status)
        })
        .collect()
}

/// Grace period between SIGTERM and SIGKILL when reaping an orphan.
/// Overridable via `INFIGRAPH_REAP_GRACE_SECS` (seconds) — kept small in
/// tests.
pub fn reap_grace_period() -> Duration {
    std::env::var("INFIGRAPH_REAP_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(5))
}

/// How often the periodic orphan scan runs. Overridable via
/// `INFIGRAPH_REAP_SCAN_SECS` (seconds).
pub fn reap_scan_interval() -> Duration {
    std::env::var("INFIGRAPH_REAP_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(600))
}

/// SIGTERM the orphan's PID, wait `reap_grace_period()`, SIGKILL if still
/// alive, then remove its registry file regardless (a removed file for an
/// already-dead PID is correct cleanup either way). Best-effort: a process
/// that exits on its own between the classify scan and this call is not an
/// error.
pub fn reap_orphan(path: &Path, pid: u32) {
    let spid = sysinfo::Pid::from_u32(pid);
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
    if let Some(process) = sys.process(spid) {
        if process.kill_with(sysinfo::Signal::Term).is_none() {
            // SIGTERM not supported on this platform — skip straight to
            // an unconditional kill rather than waiting out a grace period
            // for a signal that was never actually sent.
            process.kill();
        } else {
            std::thread::sleep(reap_grace_period());
            sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[spid]), true);
            if let Some(still_alive) = sys.process(spid) {
                still_alive.kill();
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

/// Runs one full scan-and-reap pass: lists the registry, classifies every
/// other entry, reaps every orphan found. Returns the count reaped, for
/// the caller to log.
pub fn reap_orphans_once(own_pid: u32) -> usize {
    let entries = list_instances();
    let classified = classify_instances(&entries, own_pid, current_process_start_time);
    let mut reaped = 0;
    for (path, info, status) in classified {
        if status == InstanceStatus::Orphan {
            reap_orphan(&path, info.pid);
            reaped += 1;
        }
    }
    reaped
}
