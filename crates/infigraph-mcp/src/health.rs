//! Health beacons: one-line ⚠ footers appended to MCP tool responses only
//! when a degraded condition exists — silent when healthy, so no
//! steady-state token cost (spec: write-safety-locks-design §PR 6).
//! Ground-truth rule: every condition derives from durable state (lock
//! files, sidecar files) or directly-observed process facts, never from
//! cached beliefs about what should be running.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use infigraph_core::{embed, lockfile};

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
}

pub static HEALTH: HealthState = HealthState::new();

impl HealthState {
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            restart_emitted: AtomicBool::new(false),
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
    pub trigram_fallback: bool,
    pub hnsw_missing: bool,
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

pub fn gather_signals(state: &HealthState, tool_name: &str, project: Option<&Path>) -> Signals {
    let mut sig = Signals {
        worker_restarted: state.restart_beacon(),
        trigram_fallback: embed::trigram_fallback_active(),
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
    if let Some(root) = project {
        // Watcher and HNSW are local-filesystem concepts; in remote mode
        // (Neo4j backend) neither applies. A project without .infigraph
        // has nothing to be stale against.
        if !infigraph_core::daemon::lifecycle::is_remote_backend()
            && root.join(".infigraph").is_dir()
        {
            if !WATCHER_LIFECYCLE_TOOLS.contains(&tool_name) {
                sig.watcher_missing = !crate::tools::watch::watcher_running(root);
            }
            sig.hnsw_missing = embed::hnsw_expected_but_missing(root);
        }
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
    if sig.trigram_fallback {
        lines.push(
            "⚠ semantic search degraded: Model2Vec model unavailable, using trigram fallback"
                .to_string(),
        );
    }
    if sig.hnsw_missing {
        lines.push(
            "⚠ HNSW index missing — vector search is on a linear scan this \
             project has outgrown; re-index to rebuild"
                .to_string(),
        );
    }
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
