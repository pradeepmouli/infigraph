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
    /// When the "which project answered" line was last shown, per project root.
    root_notice_shown: Mutex<BTreeMap<PathBuf, Instant>>,
    /// How this worker learns which build is installed, to say when it is not
    /// that one (`degraded::stale_build`). `None`: not asked, which is what a
    /// test's state does, since a test binary has no installed build of its own.
    installed_build: Option<fn() -> Option<String>>,
}

/// How long a degraded mode that is still in effect stays out of the footer
/// after it was shown. `doctor` and `get_stats` list every mode every time.
pub const FOOTER_REPEAT_AFTER: Duration = Duration::from_secs(600);

/// How long the "which project answered" line stays out of the footer once
/// shown, per project root.
pub const ROOT_NOTICE_REPEAT_AFTER: Duration = Duration::from_secs(1800);

pub static HEALTH: HealthState =
    HealthState::checking_installed_build(infigraph_core::daemon::installed_build_hash);

impl HealthState {
    pub const fn new() -> Self {
        Self {
            initialized: AtomicBool::new(false),
            restart_emitted: AtomicBool::new(false),
            degraded_shown: Mutex::new(BTreeMap::new()),
            root_notice_shown: Mutex::new(BTreeMap::new()),
            installed_build: None,
        }
    }

    /// A state that also compares this worker's build with `installed`'s answer.
    pub const fn checking_installed_build(installed: fn() -> Option<String>) -> Self {
        Self {
            initialized: AtomicBool::new(false),
            restart_emitted: AtomicBool::new(false),
            degraded_shown: Mutex::new(BTreeMap::new()),
            root_notice_shown: Mutex::new(BTreeMap::new()),
            installed_build: Some(installed),
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
    /// True when the root notice for `root` is due at `now`: not yet shown by
    /// this worker, or last shown [`ROOT_NOTICE_REPEAT_AFTER`] ago. Records the
    /// showing, so the answer is true once per period.
    pub fn root_notice_due(&self, root: &Path, now: Instant) -> bool {
        let mut shown = self
            .root_notice_shown
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let due = shown
            .get(root)
            .is_none_or(|at| now.saturating_duration_since(*at) >= ROOT_NOTICE_REPEAT_AFTER);
        if due {
            shown.insert(root.to_path_buf(), now);
        }
        due
    }

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
    /// The degraded modes `infigraph_core::degraded` reports (#75): defined
    /// and worded there, only rendered here.
    pub degraded: Vec<infigraph_core::degraded::Notice>,
    /// (lock file name, whole seconds waited) per slow acquisition drained
    /// for this call.
    pub slow_waits: Vec<(String, u64)>,
    /// A call that named no `path` was answered for a project with linked
    /// worktrees, and this is the period's first such call for it.
    pub unnamed_root: Option<UnnamedRoot>,
}

/// What the footer says about the project that answered a call with no `path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnnamedRoot {
    pub root: PathBuf,
    /// How many linked worktrees the project has.
    pub linked: usize,
    /// Names of those that are indexed projects of their own.
    pub indexed: Vec<String>,
}

/// Tools whose own output names the project they answered for.
const ROOT_NAMING_TOOLS: &[&str] = &["get_stats", "doctor", "doc_entry"];

/// The linked worktrees of `root` and which of them are indexed projects: the
/// shared cached check (`infigraph_core::worktree::linked_worktrees`) plus a
/// registry read. `None` when the project has none, or when that is not known.
pub fn worktree_view(root: &Path) -> Option<UnnamedRoot> {
    let linked = infigraph_core::worktree::linked_worktrees(root).filter(|l| !l.is_empty())?;
    let registered: Vec<PathBuf> = infigraph_core::multi::Registry::load()
        .map(|r| {
            r.repos
                .values()
                .map(|e| infigraph_core::project::canonicalize_lenient(&e.path))
                .collect()
        })
        .unwrap_or_default();
    let indexed = linked
        .iter()
        .filter(|p| registered.contains(&infigraph_core::project::canonicalize_lenient(p)))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    Some(UnnamedRoot {
        root: root.to_path_buf(),
        linked: linked.len(),
        indexed,
    })
}

/// The root notice for a call, when one is due: the call named no `path`, it
/// was answered for `project`, the tool does not name its project itself, the
/// project has linked worktrees (`view`), and it was not shown in the last
/// [`ROOT_NOTICE_REPEAT_AFTER`]. The server cannot see where the caller is, so
/// this says which project answered and what exists, never where the caller is.
pub fn unnamed_root_notice(
    state: &HealthState,
    tool_name: &str,
    project: Option<&Path>,
    named: bool,
    now: Instant,
    view: &dyn Fn(&Path) -> Option<UnnamedRoot>,
) -> Option<UnnamedRoot> {
    let root = project.filter(|_| !named && !ROOT_NAMING_TOOLS.contains(&tool_name))?;
    let found = view(root)?;
    state.root_notice_due(root, now).then_some(found)
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
    if !DEGRADED_REPORTING_TOOLS.contains(&tool_name) {
        let mut in_effect = match local_project {
            // Cached: this runs after every tool call, and the daemon's part
            // is a socket round trip.
            Some(root) => degraded::gather_cached(root),
            None => degraded::process_notices(),
        };
        // The build this worker runs against the one installed: about the
        // process, so with or without a project.
        if let Some(installed) = state.installed_build {
            in_effect.extend(
                degraded::stale_build(infigraph_core::build_hash(), installed().as_deref())
                    .map(|mode| mode.notice()),
            );
        }
        // A missing watcher is a degraded mode like the rest, so it is worded
        // in one place and shown once per repeat period, not after every call.
        // This worker's own watcher counts, as does a deliberate stop.
        if let Some(root) = local_project {
            if !WATCHER_LIFECYCLE_TOOLS.contains(&tool_name)
                && !crate::tools::watch::is_watching(&root.to_string_lossy().replace('\\', "/"))
            {
                in_effect.extend(degraded::watcher_missing(root).map(|mode| mode.notice()));
            }
        }
        sig.degraded = state.degraded_due(local_project, in_effect, Instant::now());
    }
    sig
}

/// Pure: render one ⚠ line per degraded condition, `None` when healthy.
/// One line: which project answered, and that its linked worktrees are separate
/// projects reached with `path`. Up to three indexed names, then a count.
fn render_unnamed_root(n: &UnnamedRoot) -> String {
    const NAMES: usize = 3;
    let mut indexed = String::new();
    if !n.indexed.is_empty() {
        let shown = n.indexed.iter().take(NAMES).cloned().collect::<Vec<_>>();
        indexed = format!(" (indexed: {}", shown.join(", "));
        if n.indexed.len() > NAMES {
            indexed.push_str(&format!(" and {} more", n.indexed.len() - NAMES));
        }
        indexed.push(')');
    }
    format!(
        "ℹ answered for {} (the project this server started in: this call named no `path`). \
         It has {} linked worktree{}{indexed}, each a separate project reached with `path`.",
        n.root.display(),
        n.linked,
        if n.linked == 1 { "" } else { "s" }
    )
}

pub fn compose_footer(sig: &Signals) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    if sig.worker_restarted {
        lines.push(
            "⚠ worker restarted since your previous call — in-memory state \
             (watchers, session context) was reset"
                .to_string(),
        );
    }
    lines.extend(sig.degraded.iter().map(|n| n.warning_line()));
    if let Some(notice) = &sig.unnamed_root {
        lines.push(render_unnamed_root(notice));
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
pub fn health_footer(tool_name: &str, args: &serde_json::Value, named: bool) -> Option<String> {
    let project = args
        .get("path")
        .and_then(|p| p.as_str())
        .map(|p| std::path::PathBuf::from(crate::tools::helpers::resolve_project_path(p)));
    let mut signals = gather_signals(&HEALTH, tool_name, project.as_deref());
    signals.unnamed_root = unnamed_root_notice(
        &HEALTH,
        tool_name,
        project.as_deref(),
        named,
        Instant::now(),
        &worktree_view,
    );
    compose_footer(&signals)
}
