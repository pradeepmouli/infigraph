//! Health beacons: one-line ⚠ footers appended to MCP tool responses only
//! when a degraded condition exists — silent when healthy, so no
//! steady-state token cost (spec: write-safety-locks-design §PR 6).
//! Ground-truth rule: every condition derives from durable state (lock
//! files, sidecar files) or directly-observed process facts, never from
//! cached beliefs about what should be running.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use infigraph_core::{degraded, lockfile};

/// Process-local health flags for this worker incarnation.
pub struct HealthState {
    /// Set when this process serves an `initialize` request. A tools/call
    /// arriving before any initialize means the client's MCP session
    /// predates this process: the previous worker died and the supervisor
    /// respawned us mid-session with inherited stdio (the I-13
    /// crash-was-invisible failure mode, de-cloaked).
    initialized: AtomicBool,
    /// The restart beacon fires once per incarnation — after the first
    /// warning the client knows, and repeating it is pure token cost.
    restart_emitted: AtomicBool,
    /// When each degraded mode was last shown in a footer, per project
    /// (`None`: no project, the process's own modes).
    degraded_shown: Mutex<BTreeMap<(Option<PathBuf>, String), Instant>>,
}

/// How long a degraded mode that is still in effect stays out of the footer
/// after it was shown. `doctor` and `get_stats` list every mode every time.
pub const FOOTER_REPEAT_AFTER: Duration = Duration::from_secs(600);

pub static HEALTH: HealthState = HealthState::new();

impl HealthState {
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            restart_emitted: AtomicBool::new(false),
            degraded_shown: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn mark_initialized(&self) {
        self.initialized.store(true, Ordering::Relaxed);
    }

    /// True exactly once: the first call served by a worker that never saw
    /// `initialize`. Short-circuit keeps the latch untouched on
    /// initialized workers.
    fn restart_beacon(&self) -> bool {
        !self.initialized.load(Ordering::Relaxed)
            && !self.restart_emitted.swap(true, Ordering::Relaxed)
    }
}

impl HealthState {
    /// The `notices` in effect for `root` that are due in a footer at `now`:
    /// those not yet shown by this worker, and those last shown at least
    /// [`FOOTER_REPEAT_AFTER`] ago. The mode's key decides, not its wording.
    /// A mode missing from `notices` is forgotten, so one that clears and
    /// comes back is shown at once.
    pub fn degraded_due(
        &self,
        root: Option<&Path>,
        notices: Vec<degraded::Notice>,
        now: Instant,
    ) -> Vec<degraded::Notice> {
        let root = root.map(Path::to_path_buf);
        let mut shown = self
            .degraded_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        shown.retain(|(r, key), _| *r != root || notices.iter().any(|n| n.key == *key));
        notices
            .into_iter()
            .filter(|notice| {
                let slot = (root.clone(), notice.key.clone());
                let due = shown
                    .get(&slot)
                    .is_none_or(|at| now.saturating_duration_since(*at) >= FOOTER_REPEAT_AFTER);
                if due {
                    shown.insert(slot, now);
                }
                due
            })
            .collect()
    }
}

impl Default for HealthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything the footer depends on, gathered up front so composition is a
/// pure function.
#[derive(Debug, Default)]
pub struct Signals {
    pub worker_restarted: bool,
    pub watcher_missing: bool,
    /// The degraded modes `infigraph_core::degraded` reports (#75): defined
    /// and worded there, only rendered here.
    pub degraded: Vec<infigraph_core::degraded::Notice>,
    /// (lock file name, whole seconds waited) per slow acquisition drained
    /// for this call.
    pub slow_waits: Vec<(String, u64)>,
}

/// Tools whose output *is* watcher-lifecycle state — a "no watcher" beacon
/// on them is noise (e.g. immediately after a deliberate stop_watch).
const WATCHER_LIFECYCLE_TOOLS: &[&str] = &[
    "watch_project",
    "stop_watch",
    "get_watch_status",
    "watch_docs",
    "stop_watch_docs",
];

/// Tools whose own output lists the degraded modes: repeating them in the
/// footer of the same reply is noise.
const DEGRADED_REPORTING_TOOLS: &[&str] = &["get_stats", "doctor"];

pub fn gather_signals(state: &HealthState, tool_name: &str, project: Option<&Path>) -> Signals {
    let mut sig = Signals {
        worker_restarted: state.restart_beacon(),
        ..Default::default()
    };
    sig.slow_waits = lockfile::take_slow_waits()
        .into_iter()
        .map(|w| {
            let name = w
                .lock_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| w.lock_path.display().to_string());
            (name, w.waited.as_secs())
        })
        .collect();
    // Watcher, sidecars and the daemon are local-filesystem concepts; in
    // remote mode (Neo4j backend) none applies. A project without
    // .infigraph has nothing to be stale against.
    let local_project = project.filter(|root| {
        !infigraph_core::daemon::lifecycle::is_remote_backend() && root.join(".infigraph").is_dir()
    });
    if let Some(root) = local_project {
        if !WATCHER_LIFECYCLE_TOOLS.contains(&tool_name) {
            sig.watcher_missing = !crate::tools::watch::watcher_running(root);
        }
    }
    if !DEGRADED_REPORTING_TOOLS.contains(&tool_name) {
        let in_effect = match local_project {
            // Cached: this runs after every tool call, and the daemon's part
            // is a socket round trip.
            Some(root) => degraded::gather_cached(root),
            None => degraded::process_notices(),
        };
        sig.degraded = state.degraded_due(local_project, in_effect, Instant::now());
    }
    sig
}

/// Pure: render one ⚠ line per degraded condition, `None` when healthy.
pub fn compose_footer(sig: &Signals) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if sig.worker_restarted {
        lines.push(
            "⚠ worker restarted since your previous call — in-memory state \
             (watchers, session context) was reset"
                .to_string(),
        );
    }
    if sig.watcher_missing {
        lines.push(
            "⚠ No file watcher running — results may be stale. \
             Run `infigraph watch` or re-index to refresh."
                .to_string(),
        );
    }
    lines.extend(sig.degraded.iter().map(|n| n.warning_line()));
    for (name, secs) in &sig.slow_waits {
        lines.push(format!(
            "⚠ lock contention: waited {secs}s for {name} while serving this call"
        ));
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Footer for one tool call against the process-global state; `None` when
/// fully healthy.
pub fn health_footer(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let project = args
        .get("path")
        .and_then(|p| p.as_str())
        .map(|p| std::path::PathBuf::from(crate::tools::helpers::resolve_project_path(p)));
    compose_footer(&gather_signals(&HEALTH, tool_name, project.as_deref()))
}
