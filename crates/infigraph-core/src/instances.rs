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
use clap::Parser;
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
    /// This process's own `build_hash()` at registration time. Lets
    /// `doctor` flag a live instance that predates the currently installed
    /// binary -- the same staleness check `check_one_lock` already does for
    /// `graph.lock`/`watch.lock` holders, extended to MCP server instances.
    /// `#[serde(default)]` so a registry file written by a pre-this-field
    /// instance still deserializes (as an empty string, which never equals
    /// a real build hash and so is reported the same way an actual
    /// mismatch would be -- accurate, since an unlabeled instance predates
    /// this check's own build too).
    #[serde(default)]
    pub build_hash: String,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Registry/instance plumbing. Declared here rather than in `multi` (where
// `registry_path` lives) because `multi::Registry` -- the registry data
// type -- would collide with the `Registry` struct this generates.
// - home: INFIGRAPH_REGISTRY_HOME (fits the convention); empty = $HOME
// - instances_dir: INFIGRAPH_REGISTRY_INSTANCES_DIR (renamed from the
//   fork-only INFIGRAPH_INSTANCES_DIR); empty = $HOME/.infigraph/instances
// - org: INFIGRAPH_ORG (upstream-inherited, seeded from the legacy name;
//   canonical INFIGRAPH_REGISTRY_ORG also works, legacy wins); empty = no
//   org scoping
crate::settings! {
    registry {
        home: String = String::new(),
        instances_dir: String = String::new(),
        org: String = String::new(),
    }
}

/// Resolves the `registry` group -- see the group's declaration above.
pub fn registry_settings() -> Registry {
    let cli = RawRegistry {
        registry_org: crate::settings::legacy_env("INFIGRAPH_ORG"),
        ..Default::default()
    };
    Registry::resolve(cli, None)
}

/// Directory holding one JSON file per live-or-recently-live instance.
/// Overridable via `INFIGRAPH_REGISTRY_INSTANCES_DIR` (tests).
pub fn instances_dir() -> PathBuf {
    let dir = registry_settings().instances_dir;
    if !dir.is_empty() {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".infigraph")
        .join("instances")
}

/// The registration file path for `pid` (pub for R5.4's signal-time
/// deregistration, which cannot reach the InstanceGuard from a handler).
pub fn instance_path(pid: u32) -> PathBuf {
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
            build_hash: crate::build_hash().to_string(),
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

/// How often the periodic orphan scan runs. Overridable via
/// `INFIGRAPH_WATCH_REAP_SCAN_SECS` (seconds).
pub fn reap_scan_interval() -> Duration {
    let cli = crate::watch::RawWatch::parse_from(std::iter::empty::<String>());
    Duration::from_secs(crate::watch::Watch::resolve(cli, None).reap_scan_secs)
}

/// Removes a stale registry file. This is remove-file-only, on purpose —
/// it never looks up or signals a process. `classify_instances` only marks
/// an entry `Orphan` in two cases: the PID is dead (nothing to signal), or
/// a live process exists at that PID but its start time doesn't match the
/// recorded one, which — per the PID-reuse guard — *proves* that live
/// process is not the one the entry named. Signaling in that second case
/// would mean sending SIGTERM/SIGKILL to a provably unrelated process, so
/// there is never a case where signaling here is correct; only cleaning up
/// the stale file is.
pub fn reap_orphan(path: &Path) {
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
            reap_orphan(&path);
            // R6.3: registry evictions are destructive ops -- one audit
            // line each, naming the dead instance, so "where did my
            // registration go" is answerable from the trail.
            crate::audit::audit_log(
                "instances",
                "reap-orphan-registration",
                &format!(
                    "pid {} (started {}) is gone or its pid was recycled",
                    info.pid, info.started_at
                ),
                &path.display().to_string(),
            );
            reaped += 1;
        }
    }
    reaped
}
