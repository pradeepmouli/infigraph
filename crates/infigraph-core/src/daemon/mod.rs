pub(crate) mod backoff;
pub(crate) mod drain;
pub mod lifecycle;
pub mod queue;
pub mod read_endpoint;
pub mod read_guard;
pub mod read_protocol;
pub mod read_service;
pub mod task;

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::daemon::backoff::ReopenBackoff;
use crate::daemon::queue::IndexWorkQueue;
use crate::daemon::task::Task;
use crate::daemon_protocol::{WatchAction, WatchRole};
use crate::ops::{begin_index_op, IndexOpOutcome};
use crate::watch::{config, producer, CodeWatch, WatchEvent, WatchEventKind};
use crate::Infigraph;

/// How long one coordinator tick waits before looking again. The fsevent
/// half moved to `producer::run_producer`, so this loop no longer blocks on
/// a watch receiver -- but every remaining job it has (reaping finished
/// background work, serving `.request` files, scheduling drains) is still
/// polled, on the same ~200ms cadence `rx.recv_timeout` used to impose.
const COORDINATOR_TICK: Duration = Duration::from_millis(200);

/// Read-service workers. Fixed so a burst of clients waits for a free
/// worker instead of spawning unbounded threads inside the daemon.
const READ_SERVICE_WORKERS: usize = 8;

/// How often the coordinator considers folding an idle WAL (#149). Coarse on
/// purpose: the fold itself takes the exclusive checkpoint window, so probing
/// it at `COORDINATOR_TICK` would contend with readers for no benefit.
const IDLE_CHECKPOINT_PROBE: Duration = Duration::from_secs(2);

/// How often the coordinator loop re-checks whether the on-disk binary has
/// changed since this process started. Deliberately independent of
/// `periodic_secs` (which can be 0 for the plain `infigraph daemon` -- see
/// its call site) -- staleness detection must run even when no other
/// periodic pass is configured.
const BUILD_HASH_CHECK_INTERVAL: Duration = Duration::from_secs(300);

fn build_hash_check_interval() -> Duration {
    std::env::var("INFIGRAPH_TEST_BUILD_HASH_CHECK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(BUILD_HASH_CHECK_INTERVAL)
}

crate::settings! {
    scip {
        // R3.3.4a: how many AST generations SCIP enrichment may lag before
        // the daemon re-runs it on its own. 0 disables the automatic
        // trigger entirely (`infigraph rebuild` still enriches).
        index_staleness_threshold: u64 = 50,
        // How often the coordinator compares the two counters -- the same
        // coarse cadence as the build-hash self-check, and for the same
        // reason: the check itself is cheap, what it can start is not.
        index_staleness_check_secs: u64 = 300,
    }
}

/// Resolved `scip` settings (env > TOML > default; no CLI surface today).
/// `RawScip::default()` rather than a clap parse: nothing on this path
/// takes command-line flags, and the daemon reads this once at startup.
pub fn scip_settings(root: &Path) -> Scip {
    Scip::resolve(
        RawScip::default(),
        crate::settings_file::ConfigScope::Project(root),
    )
}

/// The pure decision behind R3.3.4a's automatic SCIP re-enrichment: is the
/// graph's SCIP data stale enough to re-run the indexers *now*?
///
/// - `threshold == 0` disables the feature.
/// - `scip_generation <= 0` means SCIP has never run on this graph
///   (doctor's own R3.3.4 rule): a project that never opted into SCIP must
///   not have the daemon start running external indexers for it.
/// - `last_attempt_ast_generation` is the AST generation at which the
///   daemon last *started* an enrichment. Until the graph has moved a
///   further `threshold` past it, no retry -- otherwise a project whose
///   indexers are missing or whose import keeps failing would re-run
///   minutes of external indexers on every check interval for as long as
///   the user keeps editing, since a failed attempt never stamps
///   `scip_generation`. (A successful one does, so this gate only ever
///   bites after a failure.)
pub(crate) fn scip_enrichment_due(
    ast_generation: i64,
    scip_generation: i64,
    last_attempt_ast_generation: Option<i64>,
    threshold: u64,
) -> bool {
    if threshold == 0 || scip_generation <= 0 {
        return false;
    }
    let threshold = i64::try_from(threshold).unwrap_or(i64::MAX);
    if last_attempt_ast_generation
        .is_some_and(|last| ast_generation.saturating_sub(last) < threshold)
    {
        return false;
    }
    ast_generation.saturating_sub(scip_generation) >= threshold
}

/// What one SCIP-enrichment run is asked to cover: which languages'
/// indexers to run, and the AST generation the graph was at when the run
/// was decided -- the value the eventual import stamps as enriched (see
/// `GraphStore::stamp_scip_generation_conn`), since the graph keeps moving
/// while the indexers run.
#[derive(Debug, Clone, PartialEq)]
pub struct ScipEnrichJob {
    pub languages: Vec<String>,
    pub ast_generation: i64,
}

/// Starts SCIP enrichment as its own background task on `drain_rt`. The
/// one code path behind both triggers -- a just-finished full reindex and
/// the R3.3.4a staleness check -- so they can never drift apart.
/// `Task::spawn_blocking` dispatches via the ambient
/// `tokio::task::spawn_blocking`, which needs a runtime context on the
/// coordinator's (plain OS) thread -- `drain_rt.enter()` scopes that
/// context to just this call, matching `try_start_full_reindex`'s
/// identical need.
fn spawn_scip_enrich(
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
    cb: Arc<FullReindexCallback>,
    root: PathBuf,
    job: ScipEnrichJob,
) -> Task<()> {
    let _guard = drain_rt.enter();
    Task::spawn_blocking(daemon_token, "scip-enrich", move |token| {
        cb(root, job, token);
    })
}

/// Build hash of the binary at `binary`, as a fresh subprocess of it reports
/// via the hidden `print-build-hash` subcommand: trimmed stdout, or `None` if
/// the spawn failed or it exited non-zero. `None` means "couldn't check,"
/// not "confirmed stale" -- callers must never treat it as a mismatch.
///
/// This is the only way to learn what is *installed*: `crate::build_hash()`
/// is a compile-time constant baked into whichever process is asking, which
/// is exactly wrong when that process is the out-of-date one -- an
/// `infigraph-mcp` started before an install judging a daemon spawned after
/// it (#135), or a daemon judging itself after a rebuild (#134).
///
/// Test-only escape hatch: when `INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE`
/// is set, reads that file directly instead of spawning a subprocess.
/// `std::env::current_exe()` inside a `cargo test` binary resolves to the
/// test harness, not the real `infigraph` binary, so tests that exercise
/// these paths in-process have no other way to simulate a mismatch;
/// `print-build-hash`'s own handling of this same env var is covered
/// separately (`crates/infigraph-cli/tests/print_build_hash.rs`).
pub fn installed_build_hash_of(binary: &std::path::Path) -> Option<String> {
    if let Ok(path) = std::env::var("INFIGRAPH_TEST_BUILD_HASH_OVERRIDE_FILE") {
        return std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string());
    }
    let output = std::process::Command::new(binary)
        .arg("print-build-hash")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Warn, once per binary path per process, when the `infigraph` CLI this
/// process is about to spawn reports a different build than this process
/// itself (#141). Mixed builds are how a graph gets written on one lbug
/// storage version and read on another (#140): a `cargo test` binary next
/// to a stale `<target-dir>/debug/infigraph`, or an `infigraph-mcp` that
/// outlived an install. A silent mismatch used to surface only as "No
/// results across repos"; this names both hashes and the path up front.
/// Never refuses -- an undeterminable hash, or a genuinely mixed install,
/// is the caller's decision (`prune_stale_daemon` judges the daemon side).
pub fn warn_if_cli_build_differs(cli: &std::path::Path) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static CHECKED: OnceLock<Mutex<HashSet<std::path::PathBuf>>> = OnceLock::new();
    let first_time = CHECKED
        .get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert(cli.to_path_buf()))
        .unwrap_or(false);
    if !first_time {
        return;
    }
    let Some(theirs) = installed_build_hash_of(cli) else {
        return;
    };
    if theirs != crate::build_hash() {
        eprintln!(
            "[build] warning: {} reports build {theirs}, but this process is build {} -- \
             mixed builds index on one lbug storage version and read on another; rebuild or \
             reinstall so every infigraph binary matches (for a test target dir: \
             `cargo build -p infigraph-cli`; see pradeepmouli/infigraph#141)",
            cli.display(),
            crate::build_hash()
        );
    }
}

/// The daemon's own view: what does the binary it was launched from report
/// now? (The binary on disk may have been replaced since this process
/// started -- that is the whole point of the check.)
fn current_on_disk_build_hash() -> Option<String> {
    installed_build_hash_of(&std::env::current_exe().ok()?)
}

/// Callback for in-process SCIP enrichment after a successful daemon full
/// reindex, or when enrichment falls too far behind. It must NOT open an
/// `Infigraph`/`Database` on the live graph path -- Kuzu only allows safe
/// concurrent access within one process's `Database` object, not across two,
/// even in the same process -- so it writes by submitting a
/// `WriteRequest::ScipImport`, which the coordinator serves on its own handle.
///
/// The loop that invokes this callback does NOT hold `index.lock` around
/// the call -- the callback is responsible for acquiring it itself, scoped
/// narrowly around whichever part of its own work actually touches the
/// graph. Holding it for the callback's entire duration (as an earlier
/// version of this design did) blocks every other daemon write for
/// however long any graph-independent work inside the callback takes --
/// e.g. running external SCIP indexer binaries, which can take minutes on
/// a real repo.
/// The `CancellationToken` is this callback's own child token (from the
/// `Task::spawn_blocking` it runs inside) -- a cooperative-cancellation
/// checkpoint for whatever synchronous, potentially long-running work the
/// callback does (e.g. `run_scip_indexers`' between-indexer-launch check).
///
/// It receives the project root, not the daemon's `Infigraph`: enrichment
/// runs external indexers for minutes and submits its import as a request,
/// so it never needs the graph -- and holding the prism for that long kept
/// its `Database` alive past `poison_watch_db`, which is how a reopen came to
/// open a second `Database` on the same file (#166).
pub type FullReindexCallback = dyn Fn(PathBuf, ScipEnrichJob, CancellationToken) + Send + Sync;

/// Caller-supplied hook that acts on a `WatchControl { role: Docs, .. }`
/// request. Doc-watching lives in `infigraph-docs`, a crate this one does
/// not depend on, and its loop is still driven by its own
/// `Arc<AtomicBool>`/thread shape rather than a `Task<()>` -- so the
/// coordinator dispatches the request and the owner of that thread (today:
/// `cmd_daemon`) decides what start/stop actually mean for it. `Err(msg)`
/// becomes the request's `WriteResult::Err`.
pub type DocsControl = dyn Fn(WatchAction) -> std::result::Result<(), String> + Send + Sync;

/// A directory's identity: `(device, inode)` plus its birth time where the
/// platform and filesystem report one. `None` if the directory is gone or the
/// platform has no such notion. Cheap (one `stat`/`statx`), so it can ride
/// the coordinator's `COORDINATOR_TICK` cadence.
///
/// `(dev, ino)` alone is not enough. Linux recycles inode numbers eagerly --
/// deleting a directory and creating another at the same path routinely lands
/// on the same inode -- so the pair compares equal and a replaced root reads
/// as unchanged, which is precisely the #136 case this exists to catch. macOS
/// does not recycle as readily, which is why the gap survived until this tree
/// was first built and tested on Linux.
///
/// Birth time is an *additional* discriminator, never a replacement:
/// `Metadata::created()` is `statx(STATX_BTIME)` on Linux and is `Err` on
/// filesystems that record no birth time, in which case the identity degrades
/// to the previous `(dev, ino)` behaviour rather than failing open or closed.
///
/// Status-change time (`ctime`) is deliberately NOT used, despite also moving
/// on recreation: it moves whenever a subdirectory is created inside the root
/// too -- which the resurrection path itself does -- so it would report a
/// live root as gone. A false "the root vanished" shutdown is worse than the
/// missed detection this is fixing.
type DirectoryIdentity = (u64, u64, Option<std::time::SystemTime>);

fn directory_identity(dir: &Path) -> Option<DirectoryIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(dir)
            .ok()
            .map(|m| (m.dev(), m.ino(), m.created().ok()))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// Whether the watched root should be treated as gone: nothing exists at the
/// path any more, or (unix) the directory there is not the one the daemon
/// started on. The second case is what leaks daemons (#136): a test's
/// `TempDir` is removed while the daemon is still writing into `.infigraph/`
/// (health beacon, logs, a reindex reacting to the deletions themselves),
/// every one of those writes goes through `create_dir_all`, and the root is
/// resurrected under the same path -- so an `exists()` check keeps passing
/// and the daemon watches a directory nobody owns, forever (415 such roots
/// and a dozen daemons were found on one dev machine). Comparing against
/// the identity captured at startup catches the resurrected root; on
/// platforms with no directory identity it degrades to `exists()`.
fn root_is_gone(root: &Path, original: Option<DirectoryIdentity>) -> bool {
    if !root.exists() {
        return true;
    }
    match (original, directory_identity(root)) {
        (Some(started_on), Some(now)) => started_on != now,
        _ => false,
    }
}

/// The daemon's write coordinator: reaps and schedules index-shaped work
/// (drains, full reindexes, SCIP enrichment), serves `.request` files, and
/// owns the code-watch producer `Task<()>`'s lifecycle.
///
/// Filesystem watching itself is NOT done here -- it runs in
/// `producer::run_producer` on `CodeWatch`'s own runtime, feeding the same
/// `queue` this loop drains. That separation is the point: a
/// `WatchControl { role: Code, action: Stop }` request stops the producer
/// while this loop keeps running and keeps serving writes.
///
/// `docs_control` lets a caller that owns a doc-watch loop (the CLI daemon)
/// have `WatchControl { role: Docs, .. }` requests dispatched to it; `None`
/// answers those requests with an error instead.
/// Markers that make a directory a *project* rather than a place projects
/// live. Deliberately broader than "has a `.git` directory", which would be
/// wrong twice over: a git worktree's `.git` is a FILE, not a directory (this
/// repo's own `scratchpad/wt-*` worktrees are indexed), and a subdirectory of
/// a repo has no `.git` at all yet is a perfectly ordinary root to index
/// (`crates/infigraph-mcp` here has its own graph).
fn looks_like_a_project(dir: &Path) -> bool {
    // An already-indexed directory counts as a project when identifying
    // CHILDREN -- someone chose to index it. It must NOT count for the root
    // under test: `init()` creates `.infigraph/graph` before `index()` runs,
    // so a root that failed this check once would pass it forever after.
    // That is not hypothetical -- it let a test index 33,683 files across 57
    // sibling repositories, the exact outcome the check exists to prevent.
    has_project_marker(dir) || dir.join(".infigraph").join("graph").exists()
}

/// The durable markers only: a VCS root in any form, or a build manifest.
/// Deliberately excludes "has already been indexed" -- see above.
fn has_project_marker(dir: &Path) -> bool {
    // A VCS marker in any form -- `.git` may be a directory (clone) or a file
    // (worktree, submodule).
    for vcs in [".git", ".hg", ".svn", ".jj"] {
        if dir.join(vcs).exists() {
            return true;
        }
    }
    for manifest in [
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "setup.py",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "composer.json",
        "Gemfile",
        "mix.exs",
        "CMakeLists.txt",
    ] {
        if dir.join(manifest).is_file() {
            return true;
        }
    }
    false
}

/// Set to bypass [`ensure_watchable_root`] for a root that really is meant to
/// be watched as one project despite containing several.
pub const ALLOW_CONTAINER_ROOT_ENV: &str = "INFIGRAPH_ALLOW_CONTAINER_ROOT";

/// Refuse to watch a directory that is a *container of projects* rather than a
/// project.
///
/// A stale MCP instance was found on this machine rooted at
/// `~/GitHub.nosync` -- 57 sibling repositories -- where it would treat the
/// whole tree as a single project: one graph spanning everything, every
/// repo's `node_modules` and `target` reachable, and each repo's own
/// `.gitignore` out of scope because the root is above all of them.
///
/// The check is deliberately NOT "does this have a `.git` directory". That
/// test rejects worktrees (whose `.git` is a file) and repo subdirectories
/// (which have none), both of which are indexed here today. What actually
/// went wrong is narrower and is what this tests for: the root is not itself
/// a project, yet several of its immediate children are.
///
/// One child is allowed: a directory holding a single project is an ordinary
/// way to lay out a checkout, and refusing it would be surprising.
pub fn ensure_watchable_root(root: &Path) -> Result<()> {
    if std::env::var_os(ALLOW_CONTAINER_ROOT_ENV).is_some() || has_project_marker(root) {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(()); // unreadable is someone else's error to report
    };
    let mut children: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| looks_like_a_project(&e.path()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if children.len() < 2 {
        return Ok(());
    }
    children.sort();
    let shown: Vec<&str> = children.iter().take(3).map(|s| s.as_str()).collect();
    anyhow::bail!(
        "refusing to watch {} -- it is not a project itself, but {} of its subdirectories are \
         ({}{}). Watching it would index them all into one graph, with each project's own \
         ignore rules out of scope. Point the watcher at a project, or set {}=1 if this really \
         is meant to be one project.",
        root.display(),
        children.len(),
        shown.join(", "),
        if children.len() > 3 { ", ..." } else { "" },
        ALLOW_CONTAINER_ROOT_ENV,
    )
}

/// A read-only view of the coordinator's *current* `Infigraph`, shared with
/// the daemon's read service.
///
/// `Option` because the coordinator opens its prism lazily and drops it
/// again on `poison_watch_db`; the read service must cope with both.
pub type PrismBeacon = Arc<Mutex<Option<Arc<Infigraph>>>>;

/// The watch session's shared `Infigraph`, together with the beacon that
/// publishes it to the read service.
///
/// A newtype rather than a bare `Option<Arc<Infigraph>>` so that publishing
/// cannot be forgotten. The prism is opened lazily by `watch_db` and dropped
/// by `poison_watch_db` -- after a full reindex swaps the graph file, for
/// instance -- and a read service that captured the handle it saw at startup
/// would keep serving a replaced `Database`. That is the second-handle
/// failure `tests/read_service.rs` pins, and it is silent: stale or empty
/// rows, no error. Every mutation here goes through `set`/`clear`, which
/// update the beacon in the same breath.
pub(crate) struct HeldPrism {
    held: Option<Arc<Infigraph>>,
    beacon: PrismBeacon,
    /// The store `clear` let go of, for as long as anything else holds it.
    /// `GraphStore` rather than `Infigraph` because the store owns the
    /// `Database` and is shared on its own (`Infigraph::graph_store`).
    retired: Option<std::sync::Weak<crate::graph::GraphStore>>,
}

/// How long `watch_db` waits for a released graph handle to be dropped by
/// its last other holder before giving up for this attempt.
const RETIRED_STORE_WAIT: Duration = Duration::from_secs(30);

impl HeldPrism {
    pub(crate) fn new() -> Self {
        Self {
            held: None,
            beacon: Arc::new(Mutex::new(None)),
            retired: None,
        }
    }

    /// A handle the read service resolves per request.
    pub(crate) fn beacon(&self) -> PrismBeacon {
        self.beacon.clone()
    }

    pub(crate) fn current(&self) -> Option<Arc<Infigraph>> {
        self.held.clone()
    }

    pub(crate) fn as_ref(&self) -> Option<&Arc<Infigraph>> {
        self.held.as_ref()
    }

    pub(crate) fn is_none(&self) -> bool {
        self.held.is_none()
    }

    pub(crate) fn set(&mut self, prism: Arc<Infigraph>) {
        self.held = Some(Arc::clone(&prism));
        *self.beacon.lock().unwrap_or_else(|e| e.into_inner()) = Some(prism);
    }

    pub(crate) fn clear(&mut self) {
        // Beacon first, so the last `Arc` still drops inside
        // `poison_watch_db`'s `write_phase` breadcrumb (#132) rather than on
        // whichever read-service thread happened to hold the final clone.
        *self.beacon.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.retired = self
            .held
            .as_ref()
            .and_then(|p| p.graph_store())
            .map(|store| Arc::downgrade(&store));
        self.held = None;
    }

    /// Wait until no one still holds the store `clear` released, so that
    /// opening a new one cannot put two `Database`s on one file in this
    /// process (#149, #166). A reader mid-request or a background task can
    /// hold a clone past `poison_watch_db`; after a full-reindex swap the old
    /// handle still addresses its WAL by the *live* path.
    ///
    /// Waits rather than refuses: holders are normally brief (a read), and a
    /// refusal in `finish_full_reindex` would roll back a good rebuild. Counts
    /// holders through the `Weak` without upgrading it, so this never becomes
    /// the last owner and closes the `Database` here by accident.
    fn wait_for_retired_store(&mut self, budget: Duration) -> Result<()> {
        let Some(retired) = &self.retired else {
            return Ok(());
        };
        let start = std::time::Instant::now();
        while std::sync::Weak::strong_count(retired) > 0 {
            if start.elapsed() >= budget {
                anyhow::bail!(
                    "the previously released graph handle is still held by {} other owner(s) \
                     after {}s -- not opening a second Database on the same file (#166)",
                    std::sync::Weak::strong_count(retired),
                    budget.as_secs()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.retired = None;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_write_coordinator<MR, F>(
    root: &Path,
    make_registry: MR,
    debounce_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_event: impl Fn(WatchEvent) + Send + Sync + 'static,
    periodic_secs: u64,
    on_periodic: Option<F>,
    serve_requests: bool,
    on_full_reindex: Option<Arc<FullReindexCallback>>,
    daemon_token: &CancellationToken,
    docs_control: Option<Arc<DocsControl>>,
    // Serves `Store::Docs` reads. `None` leaves the daemon graph-only, and a
    // document read then gets an explicit refusal rather than silently
    // opening `docs.kuzu` in the client. Supplied by the caller because
    // `infigraph-docs` depends on this crate, not the reverse.
    docs_reads: Option<read_service::RowSource>,
) -> Result<()>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry> + Send + 'static,
    F: Fn(&crate::IndexResult) + Send + 'static,
{
    // Some watch backends (e.g. FSEvents on macOS) deliver absolute,
    // symlink-resolved event paths regardless of how `root` was specified.
    // If `root` is relative, or traverses a symlink (macOS temp dirs live
    // under /var, itself a symlink to /private/var), `path.strip_prefix(root)`
    // below silently fails for every event and all changes are dropped.
    let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    // Before anything else: a root that is really a folder of repositories
    // must not become one graph. Checked here rather than at each caller --
    // `watch_project`, `watch_project_auto_resolve` and the daemon all funnel
    // through this function, and guarding call sites individually is how a
    // guard ends up covering three of four of them.
    ensure_watchable_root(root)?;

    // Bound here, before the language-registry build below, which costs
    // seconds in a debug build. The CLI takes `watch.lock` -- every
    // caller's "daemon is ready" signal -- well before this function is
    // even entered, so anything slower than this leaves a window where the
    // daemon looks ready but answers no reads. The store is resolved per
    // request, so binding does not need it to exist yet.
    let mut held_prism = HeldPrism::new();
    // #149's idle-WAL fold, on its own interval rather than every tick.
    let mut last_idle_checkpoint = std::time::Instant::now();

    // The read service: bound here, alongside the write coordinator, and
    // torn down when this function returns (`ReadService` shuts down on
    // drop, so every early return is covered).
    //
    // It resolves the store per request through the beacon rather than
    // capturing one, because `held_prism` is opened lazily and dropped again
    // on `poison_watch_db`. It shares nothing else with the write pipeline:
    // no `index.lock`, no work queue, no place on this loop.
    //
    // A bind failure is logged, not fatal. The daemon's write duties are
    // independent of it, and killing the daemon here would stop writes too;
    // a client that cannot reach the service gets an explicit "no daemon
    // read service is listening" from `RemoteExec` rather than a silent
    // wrong answer.
    // Only a daemon serves reads. `run_write_coordinator` also drives plain
    // in-process watching (`watch_project`, serve_requests=false), which is
    // not a daemon: it must not bind this project's read endpoint -- a real
    // daemon may already own it -- and must not hold the graph open, or an
    // ordinary local `infigraph index` is locked out for the watcher's
    // whole lifetime.
    let _read_service = if !serve_requests {
        None
    } else {
        let beacon = held_prism.beacon();
        let source: read_service::StoreSource = Arc::new(move || {
            beacon
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|prism| prism.graph_store())
        });
        // Collect endpoints abandoned by daemons that died without
        // cleaning up (#162). Neither `ReadEndpoint::unlink` nor
        // `cmd_kill`'s sweep reaches a root nobody registered, so those
        // sockets otherwise accumulate forever. The age floor keeps this
        // from racing a peer daemon that has just bound its own endpoint
        // and not yet reached `accept`.
        const ORPHAN_SOCKET_AGE: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
        let swept = read_endpoint::sweep_orphaned_endpoints(ORPHAN_SOCKET_AGE);
        if swept > 0 {
            eprintln!("[read] swept {swept} read endpoint(s) left by daemons that are gone");
        }

        match read_service::ReadService::start_with_sources(
            root,
            source,
            docs_reads,
            READ_SERVICE_WORKERS,
        ) {
            Ok(svc) => Some(svc),
            Err(e) => {
                eprintln!(
                    "[read] could not bind the read service for {}: {e:#}",
                    root.display()
                );
                None
            }
        }
    };

    // Build the registry ONCE for the whole watch session (#58): it serves
    // both file-extension filtering here and every `watch_db` open below
    // via `Infigraph::open_shared`. It used to be built twice serially
    // (filter + first drain's open), which alone consumed ~5s in debug
    // builds and pushed the daemon's first request reply past callers'
    // timeouts. The full-reindex side-path build keeps its own fresh
    // `make_registry()` call -- a rebuild takes far longer than a registry
    // build, so sharing buys nothing there.
    let shared_registry: Arc<crate::lang::LanguageRegistry> = Arc::new(make_registry()?);

    // Open the graph now rather than on the first write.
    //
    // `watch_db` is lazy by design: when the daemon only served writes,
    // holding no `Database` until there was work to do was free. Now that
    // reads route through this process, "the daemon holds the graph" has to
    // be true from startup -- otherwise a freshly started daemon on an
    // already-indexed repo refuses every read until something happens to
    // trigger a write. Best-effort: a failure here (no graph yet, or another
    // process still holding it) is not fatal, and the loop's existing
    // `reopen_backoff` path retries on demand exactly as before.
    if serve_requests {
        if let Err(e) = watch_db(root, &shared_registry, &mut held_prism) {
            eprintln!("[read] graph not open at daemon start (will retry on demand): {e:#}");
        }
    }

    let mut changes_since_periodic: usize = 0;
    let mut last_periodic = std::time::Instant::now();

    // Shared DB connection for the watch session — see `watch_db`'s doc
    // comment for the platform split (held open on non-Windows, reopened
    // per call on Windows).
    // Paces reopen attempts after `watch_db` fails (typically: the graph is
    // locked by another process) -- see `backoff::ReopenBackoff`.
    let mut reopen_backoff = ReopenBackoff::new();
    // Paces idle folds that did not happen (#166). Separate from
    // `reopen_backoff` and deliberately surviving a reopen: a fold that
    // cannot succeed would otherwise run again on every fresh handle, and
    // sittir logged ~1,600 of them two seconds apart.
    let mut fold_backoff = ReopenBackoff::new();
    // The last reason a fold was skipped, so an unchanged reason is logged
    // once rather than once per attempt.
    let mut last_fold_skip: Option<String> = None;
    // What the graph weighed at the last growth sample, and whether a fold
    // was attempted since -- see `growth_note` (#166).
    let live_graph = root.join(".infigraph").join("graph");
    let mut growth_sample = (
        std::time::Instant::now(),
        crate::graph::store_util::graph_family_bytes(&live_graph),
    );
    let mut folded_since_sample = false;

    // Accumulates index-shaped work from every producer (the code-watch
    // task, the periodic mark below, ad-hoc daemon-protocol requests) so
    // it's drained as one combined execution per tick instead of each
    // producer racing its own stale plan against the others -- see
    // docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md.
    //
    // `Arc<Mutex<_>>` because it is the ONLY state the code-watch task and
    // this loop share -- no Kuzu connection, no `held_prism`, no drain
    // bookkeeping crosses that boundary -- and because the drain itself runs
    // on a background task while producers keep filling it under the mutex.
    let queue = Arc::new(Mutex::new(IndexWorkQueue::new()));
    let infigraph_dir = root.join(".infigraph");

    // R3.3.5's dirty-set recovery now runs inside `run_producer` (once per
    // producer start), not here -- it is watch-side recovery, and keeping a
    // second copy on this side would double-enqueue every recovered path
    // the moment both halves run against the same queue.

    // Code-watching runs as its own cancellable task feeding `queue`, so a
    // `WatchControl { role: Code, .. }` request can stop and restart it
    // without this loop noticing. Its registry is the same one built above:
    // building a second would cost seconds in debug builds (#58).
    let on_event_shared: Arc<dyn Fn(WatchEvent) + Send + Sync> = Arc::new(on_event);
    let mut code_watch = CodeWatch::new(
        daemon_token,
        producer::ProducerConfig {
            root: root.to_path_buf(),
            registry: Arc::clone(&shared_registry),
            debounce_ms,
            // Callers with a periodic pass keep that pass's cadence for the
            // ignore-matcher rebuild, exactly as this loop used to do
            // inline. Callers without one (the daemon: `periodic_secs == 0`)
            // used to never rebuild it at all; a 5-minute floor gives them
            // mid-session `.gitignore` edits without a restart.
            ignore_rebuild_secs: if periodic_secs > 0 {
                periodic_secs
            } else {
                300
            },
        },
        Arc::clone(&queue),
        Arc::clone(&on_event_shared),
    )?;
    // Honor the persisted enable/disable policy on every start of this
    // loop (fresh daemon, crash-restart, `daemon-restart`) -- not just when
    // spawning a new daemon. Without this gate, `watch disable` stops a
    // *live* daemon's code-watching but a restart silently resumes it,
    // since the policy previously only gated whether a daemon got spawned
    // at all, never what an already-running one does once it's up.
    if config::watch_enabled_at(root, "watch") {
        code_watch.start();
    }

    // Drains run here instead of inline so a large one (a whole-project
    // reindex can take minutes) doesn't stop this loop from accepting
    // fsevents, periodic ticks and further requests for its whole duration.
    // One worker is enough -- `drain_in_flight` allows at most one drain at
    // a time, and the drain itself is a `spawn_blocking` task.
    let drain_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("infigraph-drain")
        .build()?;
    let mut drain_in_flight: Option<InFlightDrain> = None;
    let mut full_reindex_in_flight: Option<PendingFullReindex> = None;
    let mut scip_in_flight: Option<Task<()>> = None;
    // A `WriteRequest::ScipImport` (client-submitted, or the daemon's own
    // `on_full_reindex` callback submitting one after `run_scip_indexers`
    // produces a `.scip` file) running in the background via
    // `try_start_scip_import`, reaped by `finish_scip_import` below.
    // Independent of `scip_in_flight` above -- that one tracks the OLD
    // direct-callback SCIP-enrichment path's own background task, not this
    // request-driven import.
    let mut scip_import_in_flight: Option<PendingScipImport> = None;

    let sentinel = root.join(".infigraph").join("watch.stop");

    // Set by a `WatchControl { role: Daemon, action: Stop|Restart }` request.
    // Deliberately NOT derived from `daemon_token.is_cancelled()`:
    // `daemon_token` is the root of the *background-work* cancellation
    // hierarchy, and a caller is entitled to hand this loop an
    // already-cancelled one to mean "run, but let nothing you spawn survive"
    // -- reading it as a loop-exit signal would make that caller's very first
    // tick a shutdown.
    let mut shutdown_requested = false;

    let build_hash_check_interval = build_hash_check_interval();
    let mut last_build_hash_check = std::time::Instant::now();

    // R3.3.4a: automatic SCIP re-enrichment. Read once here, like the
    // build-hash interval above -- a settings change takes a daemon
    // restart, which is how every other daemon setting behaves.
    let scip_settings = scip_settings(root);
    let scip_staleness_threshold = scip_settings.index_staleness_threshold;
    let scip_staleness_check_interval =
        Duration::from_secs(scip_settings.index_staleness_check_secs);
    let mut last_scip_staleness_check = std::time::Instant::now();
    // The AST generation at which the last enrichment attempt (either
    // trigger) started -- see `scip_enrichment_due` for why it gates retries.
    let mut last_scip_attempt_ast_generation: Option<i64> = None;

    // Which directory this daemon started on, not just whether *a* directory
    // exists at that path -- see `root_is_gone`.
    let root_identity = directory_identity(root);

    loop {
        if stop_rx.try_recv().is_ok() {
            eprintln!("[watch] stop channel signaled -- shutting down");
            break;
        }

        if sentinel.exists() {
            let _ = std::fs::remove_file(&sentinel);
            eprintln!("[watch] watch.stop sentinel found -- shutting down");
            break;
        }

        // Self-terminate once the watched root is gone (`rm -rf`'d project,
        // a test's tempdir, a removed worktree that skipped `worktree
        // teardown`) -- including a root that was deleted and then recreated
        // at the same path, which `exists()` alone cannot see (#136). No
        // dedicated poll timer needed for this -- the loop
        // already ticks every `COORDINATOR_TICK`, so this check rides that
        // existing cadence for free. Without it, a daemon whose target
        // directory disappeared keeps running forever: `prune_stale_holder`
        // only reaps a *dead* holder, and this process is very much alive,
        // just watching nothing. `infigraph gc --global` sweeps the
        // registry for the same condition as a backstop for a daemon that's
        // wedged and never reaches this check.
        if root_is_gone(root, root_identity) {
            eprintln!(
                "[watch] {} no longer exists (or was deleted and recreated) -- shutting down",
                root.display()
            );
            break;
        }

        // Self-terminate if the on-disk binary has changed since this
        // process started (#134) -- prune_stale_daemon already handles
        // this correctly for a daemon someone is actively trying to
        // (re)start, but a long-idle project's daemon never gets that
        // lazy check triggered. This rides its own coarse interval rather
        // than every COORDINATOR_TICK, since it spawns a real subprocess.
        if last_build_hash_check.elapsed() >= build_hash_check_interval {
            last_build_hash_check = std::time::Instant::now();
            match current_on_disk_build_hash() {
                Some(current) if current != crate::build_hash() => {
                    eprintln!(
                        "[watch] running build {} but the current binary on disk is {} -- \
                         shutting down so the next request starts a fresh daemon",
                        crate::build_hash(),
                        current
                    );
                    break;
                }
                Some(_) => {}
                None => {
                    eprintln!(
                        "[watch] build-hash self-check couldn't run this interval, will retry"
                    );
                }
            }
        }

        // Shared drain step, in two halves: reap whatever finished since the
        // last tick (here), then schedule the next one (at the end of the
        // tick). The drain itself combines everything every producer
        // (periodic mark, ad-hoc requests, and the code-watch task's batch
        // flushes and removals) contributed into ONE execution -- the fix for the
        // coalescing bug (see
        // docs/superpowers/specs/2026-08-03-daemon-index-work-queue-design.md).
        // Same lock, same role string, same cross-process contract the
        // batch-flush block used to acquire on its own; only *where* the
        // execution runs changed.
        //
        // Reaping runs first in the tick, before anything else that wants
        // `index.lock`: the task hands the guard back rather than dropping
        // it (so these downstream steps still run under it, exactly as they
        // did when the drain was inline), which means the lock stays held
        // until this block runs. Anything here that blocked on it first
        // would be waiting on itself.
        if drain_in_flight
            .as_ref()
            .is_some_and(|d| d.handle.is_finished())
        {
            let InFlightDrain {
                handle,
                prism,
                waiter_replies,
                removed_in_drain,
            } = drain_in_flight.take().expect("checked is_some just above");
            let (guard, finish) = finish_drain(drain_rt.block_on(handle), &waiter_replies);
            match finish {
                DrainFinish::Completed(outcome) => {
                    // Removals are counted here rather than off a raw
                    // fsevent (as they were before the producer split, when
                    // this loop owned the watcher): `add_watch_removal`
                    // writes into the same `queue` this drain came from, so
                    // what the drain actually removed is a strictly better
                    // signal than what an fsevent claimed -- and it is the
                    // only one still visible from this side of the split.
                    // Without it a removals-only session could never trip
                    // the periodic whole-project pass below.
                    changes_since_periodic += outcome.extractions.len() + removed_in_drain.len();

                    // R3.3.5: only extractions that actually made it into
                    // `outcome` were confirmed written (a per-file read/parse
                    // failure inside `extract_paths` silently drops that file
                    // rather than failing the whole drain -- see its own doc
                    // comment -- so it correctly stays dirty here for a later
                    // retry instead of being cleared alongside its batch).
                    let mut cleared: Vec<String> =
                        outcome.extractions.iter().map(|e| e.file.clone()).collect();
                    cleared.extend(removed_in_drain);
                    if !cleared.is_empty() {
                        if let Err(e) = crate::dirty::clear_dirty(&infigraph_dir, &cleared) {
                            eprintln!("[watch] failed to clear dirty set: {e}");
                        }
                    }
                    if let Some(ref cb) = on_periodic {
                        if !outcome.extractions.is_empty() {
                            cb(&crate::IndexResult {
                                total_files: outcome.extractions.len(),
                                indexed_files: outcome.extractions.len(),
                                extractions: outcome.extractions.clone(),
                                resolve_stats: outcome.resolve_stats.clone(),
                                skipped_errors: Vec::new(),
                            });
                        }
                    }
                    if let Some(backend) = prism.backend() {
                        let changed: Vec<&str> = outcome
                            .extractions
                            .iter()
                            .map(|e| e.file.as_str())
                            .collect();
                        if !changed.is_empty() {
                            if let Err(e) = crate::embed::update_embeddings(backend, root, &changed)
                            {
                                eprintln!("[watch] embedding update failed: {e}");
                            }
                        }
                    }
                    for extraction in &outcome.extractions {
                        let cross = has_cross_file_calls(&prism, &extraction.file);
                        let abs_path = root.join(&extraction.file);
                        on_event_shared(WatchEvent {
                            kind: WatchEventKind::Modified,
                            path: abs_path,
                            has_cross_file_calls: cross,
                        });
                    }
                }
                // A preflight declined this write before it reached the
                // graph, so the handle it would have used is untouched.
                // Dropping it here is what turned "your project is too
                // large to write to" into "your project cannot be read at
                // all" -- only a *successful* drain restores the handle,
                // and the guard that refused this one guarantees none is
                // coming (#161). `finish_drain` already logged and replied.
                DrainFinish::Refused => {}
                // `finish_drain` already logged and replied to the waiters.
                DrainFinish::Failed => poison_watch_db(&mut held_prism),
            }
            drop(guard);
        }

        // Reap a finished full-reindex build the same way the regular drain
        // above is reaped: pull the handle, run the fast held-touching finish
        // step on this thread, then (only on full success) schedule SCIP
        // enrichment as its own background task -- never inside the same task,
        // so a `--full` reindex still returns to the client as soon as the
        // rebuild lands, not after SCIP also finishes.
        if full_reindex_in_flight
            .as_ref()
            .is_some_and(|f| f.task.is_finished())
        {
            let PendingFullReindex {
                task,
                request_path,
                reply_path,
            } = full_reindex_in_flight
                .take()
                .expect("checked is_some just above");
            let (guard, scheduled_languages) = finish_full_reindex(
                root,
                &reply_path,
                &shared_registry,
                &mut held_prism,
                drain_rt.block_on(task.join()),
            );
            std::fs::remove_file(&request_path).ok();
            drop(guard);

            if let Some(languages) = scheduled_languages {
                // The swapped-in graph has fresh counters, so the staleness
                // trigger's retry gate from the old graph is meaningless --
                // whether or not enrichment gets spawned below. Left in
                // place it would suppress the trigger on the new graph
                // until its counter climbed past the old one's.
                last_scip_attempt_ast_generation = None;
                if scip_in_flight.is_some() {
                    // A previous full reindex's SCIP task is still running --
                    // don't overwrite its tracked handle (we'd lose the ability to
                    // reap/log it) or run two enrichment passes concurrently
                    // against the same connection. This round's enrichment is
                    // simply skipped; the graph itself is still correct (just not
                    // freshly re-enriched) and the next full reindex tries again.
                    eprintln!(
                        "[daemon] a previous SCIP-enrichment task is still running; skipping \
                         enrichment for this reindex (detected languages: {})",
                        languages.join(", ")
                    );
                } else if let (Some(cb), Some(prism)) =
                    (on_full_reindex.clone(), held_prism.current())
                {
                    // `finish_full_reindex` only hands back languages on a
                    // successful swap+reopen, so `prism` is the new graph
                    // and this is its generation.
                    let ast_generation = prism
                        .backend()
                        .and_then(|b| b.current_ast_generation().ok())
                        .unwrap_or(0);
                    last_scip_attempt_ast_generation = Some(ast_generation);
                    scip_in_flight = Some(spawn_scip_enrich(
                        &drain_rt,
                        daemon_token,
                        cb,
                        root.to_path_buf(),
                        ScipEnrichJob {
                            languages,
                            ast_generation,
                        },
                    ));
                }
            }
        }

        // Reap a finished SCIP-enrichment task. Nobody is waiting on a reply for
        // this one -- it isn't client-requested -- so this only needs to log a
        // panic and free the slot.
        if scip_in_flight.as_ref().is_some_and(|s| s.is_finished()) {
            let task = scip_in_flight.take().expect("checked is_some just above");
            if let Err(join_err) = drain_rt.block_on(task.join()) {
                eprintln!("[watch] scip-enrich task panicked: {join_err}");
            }
        }

        // R3.3.4a: re-run SCIP enrichment on the daemon's own initiative once
        // the graph's AST generation has drifted `scip_staleness_threshold`
        // past its SCIP generation. Until this existed, enrichment only ever
        // ran after a full reindex (the `periodic_secs` branch below is dead
        // for every caller, and marks an AST rescan anyway, not SCIP). Rides
        // its own coarse interval like the build-hash check: the comparison
        // is two cheap reads, but what it can start (external indexer runs,
        // a multi-minute import) is not. Only when nothing else is in
        // flight and the queue is empty -- enrichment is heavy, and
        // starting it into an edit storm just makes it stale again before
        // it lands. Same guard set the post-full-reindex trigger has for
        // `scip_in_flight` (never two enrichments against one connection).
        if serve_requests
            && on_full_reindex.is_some()
            && scip_staleness_threshold > 0
            && last_scip_staleness_check.elapsed() >= scip_staleness_check_interval
        {
            last_scip_staleness_check = std::time::Instant::now();
            let idle = drain_in_flight.is_none()
                && full_reindex_in_flight.is_none()
                && scip_in_flight.is_none()
                && scip_import_in_flight.is_none()
                && queue.lock().unwrap().is_empty();
            if idle && reopen_backoff.should_attempt() {
                // `watch_db` rather than `held_prism` alone: a daemon that
                // has served no writes yet holds nothing open, and the
                // graph can be stale from before this process started.
                match watch_db(root, &shared_registry, &mut held_prism) {
                    Ok(prism) => {
                        reopen_backoff.record_success();
                        let due = prism.backend().and_then(|backend| {
                            let ast = backend.current_ast_generation();
                            let scip = backend.current_scip_generation();
                            match (ast, scip) {
                                (Ok(ast), Ok(scip)) => scip_enrichment_due(
                                    ast,
                                    scip,
                                    last_scip_attempt_ast_generation,
                                    scip_staleness_threshold,
                                )
                                .then(|| (ast, scip, backend.distinct_languages())),
                                (Err(e), _) | (_, Err(e)) => {
                                    eprintln!(
                                        "[daemon] SCIP staleness check couldn't read the \
                                         generation counters: {e}"
                                    );
                                    None
                                }
                            }
                        });
                        match due {
                            Some((ast, scip, Ok(languages))) if !languages.is_empty() => {
                                eprintln!(
                                    "[daemon] SCIP enrichment is {} AST generations behind \
                                     (threshold {}) -- re-enriching {}",
                                    ast - scip,
                                    scip_staleness_threshold,
                                    languages.join(", ")
                                );
                                last_scip_attempt_ast_generation = Some(ast);
                                let cb = on_full_reindex.clone().expect("checked is_some above");
                                scip_in_flight = Some(spawn_scip_enrich(
                                    &drain_rt,
                                    daemon_token,
                                    cb,
                                    root.to_path_buf(),
                                    ScipEnrichJob {
                                        languages,
                                        ast_generation: ast,
                                    },
                                ));
                            }
                            // An empty graph has nothing to enrich; leave the
                            // retry gate alone so a later populated graph gets
                            // its chance.
                            Some((_, _, Ok(_))) => {}
                            Some((_, _, Err(e))) => {
                                eprintln!(
                                    "[daemon] SCIP staleness check couldn't list the graph's \
                                     languages: {e}"
                                );
                            }
                            None => {}
                        }
                    }
                    Err(e) => log_reopen_failure("watch", &mut reopen_backoff, &e),
                }
            }
        }

        // Reap a finished SCIP import the same way a full-reindex build is
        // reaped: pull the handle, run the fast `held`-touching finish step
        // on this thread (logging, embedding refresh, reply-write), then
        // give the touched files the same `on_event` notification an
        // ordinary drain gives its own extracted files -- otherwise a
        // consumer relying on it (cross-file-dependents awareness) never
        // learns these files changed, since a SCIP import writes directly
        // to the graph rather than through `IndexWorkQueue`/`execute_drain`.
        if scip_import_in_flight
            .as_ref()
            .is_some_and(|p| p.task.is_finished())
        {
            let PendingScipImport {
                task,
                request_path,
                reply_path,
            } = scip_import_in_flight
                .take()
                .expect("checked is_some just above");
            let (guard, touched_files) = finish_scip_import(
                root,
                &reply_path,
                &held_prism,
                drain_rt.block_on(task.join()),
            );
            std::fs::remove_file(&request_path).ok();
            drop(guard);

            if let Some(prism) = held_prism.as_ref() {
                for file in &touched_files {
                    let cross = has_cross_file_calls(prism, file);
                    on_event_shared(WatchEvent {
                        kind: WatchEventKind::Modified,
                        path: root.join(file),
                        has_cross_file_calls: cross,
                    });
                }
            }
        }

        // Periodic SCIP refresh: if changes accumulated and enough time passed
        if periodic_secs > 0
            && changes_since_periodic > 0
            && last_periodic.elapsed() >= Duration::from_secs(periodic_secs)
        {
            if on_periodic.is_some() {
                // Marks the queue rather than indexing directly -- the
                // shared drain step below runs the actual scan/upsert/
                // resolve pass and invokes `on_periodic` with the real
                // `DrainOutcome`, folded into whatever else this tick's
                // other producers also contributed.
                queue.lock().unwrap().mark_whole_project();
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            } else {
                changes_since_periodic = 0;
                last_periodic = std::time::Instant::now();
            }
        }

        // Serve file-dropped write requests -- daemon-mode only (never from
        // in-process MCP watcher threads, which always pass
        // serve_requests=false). Piggybacks on this loop's `COORDINATOR_TICK`
        // cadence rather than a separate notify-based watch on the requests
        // directory -- submit_write_request's own poll-with-backoff starts at
        // 10ms and only reaches 200ms after several rounds, so this cadence
        // is fine. The spec's event-driven upgrade (a second `notify::Watcher`
        // scoped to `.infigraph/requests/`) is deferred: it needs a `select!`
        // arm, and this coordinator is deliberately synchronous.
        if serve_requests {
            // R3.1.4a/c: translate a pending dead-holder-WAL sentinel into a
            // synthetic FullReindex request (or a crash-loop refusal) before
            // scanning for requests below, so this same tick's scan picks
            // the synthetic request up immediately rather than waiting a
            // full COORDINATOR_TICK.
            if let Err(e) = crate::recovery::drain_recovery_sentinel(&infigraph_dir) {
                eprintln!("[watch] recovery-sentinel handling failed: {e}");
            }

            let requests_dir = infigraph_dir.join("requests");
            if let Ok(entries) = std::fs::read_dir(&requests_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "request") {
                        if let Some(started) = route_or_serve_request(
                            root,
                            &path,
                            &queue,
                            &shared_registry,
                            &make_registry,
                            &mut held_prism,
                            &mut reopen_backoff,
                            drain_in_flight.is_some(),
                            full_reindex_in_flight.is_some(),
                            &drain_rt,
                            daemon_token,
                            &mut code_watch,
                            docs_control.as_ref(),
                            &mut shutdown_requested,
                            scip_import_in_flight.is_some(),
                        ) {
                            match started {
                                PendingWork::FullReindex(p) => full_reindex_in_flight = Some(p),
                                PendingWork::ScipImport(p) => scip_import_in_flight = Some(p),
                            }
                        }
                    }
                }
            }
        } else if crate::recovery::pending_recovery(&infigraph_dir) {
            // A pending sentinel asks for a full reindex, and only the
            // request-serving branch above can grant it: the drain works by
            // writing a `FullReindex` request file that nothing else
            // consumes. Draining it here would clear the sentinel and leave
            // the request unread -- strictly worse than not draining, since
            // the signal would be gone.
            //
            // So say so instead of failing silently. A project watched only
            // by an in-process MCP thread otherwise sits with a quarantined
            // graph and no symbols indefinitely, which is exactly how one was
            // found five hours after its graph went away.
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "[watch] {} has a pending recovery (its graph was quarantined and needs a \
                     full reindex), but this watcher does not serve write requests -- run \
                     `infigraph daemon` in that directory, or `infigraph rebuild`, or it \
                     will stay empty",
                    root.display()
                );
            }
        }

        // Checked here rather than at the top of the next tick so a
        // `WatchControl { role: Daemon, action: Stop }` reply isn't followed
        // by another `COORDINATOR_TICK` of scheduling work the caller just
        // asked this process to stop doing.
        if shutdown_requested {
            eprintln!("[watch] daemon.stop request received -- shutting down");
            break;
        }

        // Schedule: only when nothing's in flight, so at most one drain runs
        // at a time and this loop never waits on its own background task's
        // `index.lock`.
        if drain_in_flight.is_none()
            && full_reindex_in_flight.is_none()
            && reopen_backoff.should_attempt()
            && !queue.lock().unwrap().is_empty()
        {
            match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
                Ok(IndexOpOutcome::Acquired(guard)) => {
                    match watch_db(root, &shared_registry, &mut held_prism) {
                        Ok(prism) => {
                            reopen_backoff.record_success();
                            // Drained here on the loop thread rather than
                            // inside the task, so a panicking task can't take
                            // its waiters down with it -- `waiter_replies`
                            // survives to be answered by `finish_drain`.
                            let drained = queue.lock().unwrap().drain();
                            let waiter_replies: Vec<PathBuf> = drained
                                .waiters
                                .iter()
                                .map(|w| w.reply_path.clone())
                                .collect();
                            // R3.3.5: captured before `drained` moves into the
                            // task -- `DrainOutcome` only reports successful
                            // extractions, not removals, and a removal's
                            // `backend.remove_file` error is already
                            // swallowed by `execute_drain` itself (best-effort,
                            // matching the rest of that code path), so "was
                            // part of a drain that returned Ok" is the same
                            // confidence level the removal path itself offers.
                            let removed_in_drain: Vec<String> =
                                drained.removals.iter().cloned().collect();
                            let task_prism = Arc::clone(&prism);
                            let handle = drain_rt.spawn_blocking(move || DrainTaskOutput {
                                result: crate::daemon::drain::execute_drain(&task_prism, drained),
                                guard,
                            });
                            drain_in_flight = Some(InFlightDrain {
                                handle,
                                prism,
                                waiter_replies,
                                removed_in_drain,
                            });
                        }
                        Err(e) => log_reopen_failure("watch", &mut reopen_backoff, &e),
                    }
                }
                Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
                    // Queue contents are NOT cleared here -- queue.drain()
                    // only runs inside the Acquired arm above, so whatever
                    // was pending stays queued for the next tick's attempt.
                    eprintln!(
                        "[watch] index operation busy ({}), retrying next tick",
                        o.skip_note().unwrap_or_default()
                    );
                }
                Err(e) => {
                    eprintln!("[watch] index operation busy ({e}), retrying next tick");
                }
            }
        }

        // #149: fold an idle WAL so NEW external readers can open the graph.
        // `checkpoint_if_wal_large` rides `write_lock` and so only fires on
        // the next write, which an idle daemon never performs -- that gap is
        // the whole issue, and it left a 6.6MB WAL blocking every fresh
        // read-only open for ~45 minutes on sittir. Rides this loop's tick
        // like the other periodic checks, but on its own coarse interval:
        // a checkpoint takes the exclusive window, so probing every 200ms
        // would be pure contention. `checkpoint_if_idle` re-checks the WAL's
        // mtime itself, so a busy graph is skipped rather than serialized.
        if last_idle_checkpoint.elapsed() >= IDLE_CHECKPOINT_PROBE && fold_backoff.should_attempt()
        {
            last_idle_checkpoint = std::time::Instant::now();
            let idle_after = Duration::from_secs(crate::graph::store::checkpoint_idle_secs(
                crate::settings_file::ConfigScope::Project(root),
            ));
            let fold = (!idle_after.is_zero())
                .then(|| held_prism.as_ref().and_then(|p| p.graph_store()))
                .flatten()
                .map(|store| store.checkpoint_if_idle(idle_after));
            folded_since_sample |= matches!(
                fold,
                Some(
                    crate::graph::store::IdleFold::Folded
                        | crate::graph::store::IdleFold::NotFolded(_)
                )
            );
            let settled = settle_idle_fold(fold, &mut fold_backoff, &mut last_fold_skip);
            if let Some(line) = settled.log {
                eprintln!("{line}");
            }
            if settled.reopen {
                // A handle that must not fold again: reopen, exactly as a
                // failed drain does. Closing it still runs lbug's
                // checkpoint-on-close, which its Rust `SystemConfig` (0.20.4)
                // has no switch for -- the exposure the drain path already
                // carries, under the same breadcrumb.
                poison_watch_db(&mut held_prism);
            }
        }

        if growth_sample.0.elapsed() >= GROWTH_SAMPLE_EVERY {
            let now = crate::graph::store_util::graph_family_bytes(&live_graph);
            let active: Vec<&str> = [
                (drain_in_flight.is_some(), "drain"),
                (full_reindex_in_flight.is_some(), "full reindex"),
                (scip_import_in_flight.is_some(), "SCIP import"),
                (scip_in_flight.is_some(), "SCIP enrichment"),
                (folded_since_sample, "idle fold"),
            ]
            .into_iter()
            .filter_map(|(on, what)| on.then_some(what))
            .collect();
            if let Some(line) =
                growth_note(growth_sample.1, now, growth_sample.0.elapsed(), &active)
            {
                eprintln!("{line}");
            }
            growth_sample = (std::time::Instant::now(), now);
            folded_since_sample = false;
        }

        std::thread::sleep(COORDINATOR_TICK);
    }

    // Stop the producer before waiting out the in-flight work below: it
    // shares `queue`, and anything it adds after this point would be
    // enqueued for a drain that is never going to run.
    code_watch.stop();

    // A drain still running when the loop exits holds `index.lock` and a
    // connection to the graph. Wait it out rather than returning into a
    // process teardown that would drop both mid-write.
    if let Some(in_flight) = drain_in_flight.take() {
        let (guard, _) = finish_drain(
            drain_rt.block_on(in_flight.handle),
            &in_flight.waiter_replies,
        );
        drop(guard);
    }

    // Same reasoning as the drain cleanup above -- a full reindex or a SCIP
    // task still running when the loop exits holds `index.lock` (and, for
    // the full reindex, a connection to the graph). Wait it out.
    if let Some(in_flight) = full_reindex_in_flight.take() {
        let (guard, _) = finish_full_reindex(
            root,
            &in_flight.reply_path,
            &shared_registry,
            &mut held_prism,
            drain_rt.block_on(in_flight.task.join()),
        );
        std::fs::remove_file(&in_flight.request_path).ok();
        drop(guard);
    }
    // Cancel before waiting, not just wait: the callback's indexer runner
    // checks its token between launches, so a shutdown that lands mid-run
    // stops starting further multi-minute indexers instead of finishing
    // the whole set first. (Its `submit_write_request` poll does not yet
    // observe the token -- an import request it has already dropped for a
    // loop that no longer serves it still waits out that call's timeout.)
    if let Some(in_flight) = scip_in_flight.take() {
        drain_rt.block_on(in_flight.stop());
    }
    // Same reasoning again -- a SCIP import still running when the loop
    // exits also holds `index.lock`. No `on_event_shared` notification pass
    // here (unlike the tick-time reap block above): the process is already
    // tearing down, nothing is left running to act on the notification.
    if let Some(in_flight) = scip_import_in_flight.take() {
        let (guard, _touched_files) = finish_scip_import(
            root,
            &in_flight.reply_path,
            &held_prism,
            drain_rt.block_on(in_flight.task.join()),
        );
        std::fs::remove_file(&in_flight.request_path).ok();
        drop(guard);
    }

    Ok(())
}

/// A drain executing on the background task, plus everything the loop
/// thread needs to finish it: the shared graph connection its downstream
/// steps run against, and the reply paths of the waiters folded into it --
/// retained here, outside the task, precisely so a panic in the task still
/// leaves someone able to answer them.
struct InFlightDrain {
    handle: tokio::task::JoinHandle<DrainTaskOutput>,
    prism: Arc<Infigraph>,
    waiter_replies: Vec<PathBuf>,
    /// R3.3.5: paths this drain's `DrainedQueue.removals` named, captured
    /// before the queue moved into the task -- cleared from the persistent
    /// dirty set alongside `outcome.extractions` once the drain confirms
    /// success. See the capture site's comment for why removals get this
    /// same best-effort treatment rather than a stricter per-path check.
    removed_in_drain: Vec<String>,
}

/// What the background drain task hands back. The `index.lock` guard rides
/// along so the loop thread keeps holding it across the post-drain steps
/// (embedding update, cross-file-call event emission) instead of those
/// running unlocked.
struct DrainTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<crate::daemon::drain::DrainOutcome>,
}

/// How a drain ended, from the perspective of the one decision that turns
/// on it: whether the daemon's shared read handle is still trustworthy.
enum DrainFinish {
    /// The drain ran. `Box` keeps this enum small -- `DrainOutcome` carries
    /// every extraction from the batch, and clippy rightly objects to a
    /// variant that dwarfs its siblings.
    Completed(Box<crate::daemon::drain::DrainOutcome>),
    /// A preflight guard declined the write before it touched the graph
    /// (`is_write_refusal`). Nothing was written, so the read handle is
    /// exactly as valid as it was -- keep it. Poisoning here is what made a
    /// too-large graph unreadable instead of merely un-writable (#161).
    Refused,
    /// The drain failed in a way that may have left the handle stale, or
    /// panicked out of an unknown state. Poison it.
    Failed,
}

/// Collects a finished drain task. Returns the index-op guard to keep held
/// (absent if the task panicked, since it was dropped during the unwind)
/// and what became of the drain.
///
/// Both failure modes -- a panic, and an `execute_drain` error -- answer
/// every waiter with `WriteResult::Err`. Without that a client blocks until
/// its own multi-minute timeout with nothing explaining why: `execute_drain`
/// writes replies as its last step, so a failure anywhere before that
/// leaves them unwritten.
fn finish_drain(
    joined: std::result::Result<DrainTaskOutput, tokio::task::JoinError>,
    waiter_replies: &[PathBuf],
) -> (Option<crate::ops::IndexOpGuard>, DrainFinish) {
    match joined {
        Ok(DrainTaskOutput {
            guard,
            result: Ok(outcome),
        }) => (Some(guard), DrainFinish::Completed(Box::new(outcome))),
        Ok(DrainTaskOutput {
            guard,
            result: Err(e),
        }) => {
            eprintln!("[watch] drain failed: {e}");
            reply_err_to_waiters(waiter_replies, &format!("daemon drain failed: {e}"));
            let finish = if crate::graph::growth_gate::is_write_refusal(&e) {
                DrainFinish::Refused
            } else {
                DrainFinish::Failed
            };
            (Some(guard), finish)
        }
        Err(join_err) => {
            eprintln!("[watch] drain task panicked: {join_err}");
            reply_err_to_waiters(
                waiter_replies,
                &format!("daemon drain task panicked: {join_err}"),
            );
            // A panic can unwind out of anything, including mid-write --
            // assume the handle is suspect.
            (None, DrainFinish::Failed)
        }
    }
}

/// `execute_drain` writes each waiter's reply as the last step of its own
/// internal loop, using `?` to propagate a `write_atomic` failure -- so an
/// ordinary `execute_drain` error can still leave some waiters *earlier* in
/// that loop already answered correctly on disk. Skipping any `reply_path`
/// that already exists avoids clobbering those real answers with a false
/// `Err`; `write_atomic` writes to a temp file and renames into place, so an
/// existing reply file is always a complete, correctly-written one, never a
/// partial write a caller could race against.
fn reply_err_to_waiters(waiter_replies: &[PathBuf], message: &str) {
    let result = crate::daemon_protocol::WriteResult::Err {
        message: message.to_string(),
    };
    let json = match serde_json::to_string(&result) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[watch] could not encode drain failure reply: {e}");
            return;
        }
    };
    for reply_path in waiter_replies {
        if reply_path.exists() {
            continue;
        }
        if let Err(e) = crate::daemon_protocol::write_atomic(reply_path, &json) {
            eprintln!(
                "[watch] could not write drain failure reply to {}: {e}",
                reply_path.display()
            );
        }
    }
}

// (open_transient below opens a short-lived Infigraph instance for batch work.)

fn open_transient(root: &Path, registry: &Arc<crate::lang::LanguageRegistry>) -> Result<Infigraph> {
    // Reuses the watch session's already-built registry (#58): building the
    // 62-pack registry takes seconds in debug builds, and doing it again
    // here put the daemon's first request reply right at the edge of
    // callers' timeouts. `Infigraph::open_shared` exists for exactly this.
    let mut prism = Infigraph::open_shared(root, Arc::clone(registry))?;
    prism.init()?;
    Ok(prism)
}

/// Acquires the watch session's shared DB connection, opening it if not
/// already held. On non-Windows platforms this connection is reused across
/// the whole watch session rather than reopened per batch/event — see
/// `run_write_coordinator`'s doc comment for why that matters. If an
/// operation on the returned connection fails, call `poison_watch_db` so the
/// next call reopens fresh (e.g. after the on-disk database was replaced out
/// from under a live connection, such as a concurrent `infigraph index
/// --full` against a project this watcher is also watching).
///
/// Returns an `Arc` clone rather than a borrow so the background drain task
/// can hold the *same* `Infigraph` -- and therefore the same in-process
/// `kuzu::Database` -- that the loop thread keeps using. Each graph
/// operation opens its own short-lived `Connection` from that shared
/// `Database` (see `GraphStore::connection`), which is the concurrency
/// pattern lbug documents as safe. Opening a second `Database` on the same
/// file for the drain would be a materially weaker guarantee.
#[cfg(not(windows))]
fn watch_db(
    root: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
) -> Result<Arc<Infigraph>> {
    if held.is_none() {
        held.wait_for_retired_store(RETIRED_STORE_WAIT)?;
        let _phase = crate::write_phase::enter(&"daemon: open graph", 0);
        held.set(Arc::new(open_transient(root, registry)?));
    }
    Ok(held.current().expect("just set"))
}

/// Windows' mandatory file locking prevents a second concurrent connection
/// while another handle on the same file is open elsewhere, so each call
/// opens (and the previous one closes) fresh rather than holding one open
/// across the whole session — see `open_transient`.
#[cfg(windows)]
fn watch_db(
    root: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
) -> Result<Arc<Infigraph>> {
    held.set(Arc::new(open_transient(root, registry)?));
    Ok(held.current().expect("just set"))
}

/// Drops the watch session's shared DB connection so the next `watch_db`
/// call reopens fresh. See `watch_db`'s doc comment for when to call this.
fn poison_watch_db(held: &mut HeldPrism) {
    // Dropping the last `Arc` closes the lbug Database, which checkpoints
    // on close -- name it in case that is where the process aborts (#132).
    let _phase = crate::write_phase::enter(&"daemon: drop held graph (checkpoint on close)", 0);
    held.clear();
}

/// How often the coordinator samples the graph's on-disk size.
const GROWTH_SAMPLE_EVERY: Duration = Duration::from_secs(10);

/// Growth between two samples that earns a log line.
const GROWTH_NOTE_MIN_BYTES: u64 = 64 * 1024 * 1024;

/// A daemon-log line for a jump in the graph's on-disk size, naming what was
/// in flight when it happened -- or `None` for ordinary growth.
///
/// #166 could not say which operation took sittir from 2 GB to 21 GB: the
/// breakers only run on write paths, and the log recorded refusals after the
/// fact, never the growth itself. Sampling from the coordinator catches
/// growth from any path -- a drain, SCIP, a fold, lbug's own automatic
/// checkpoint -- without instrumenting each one.
fn growth_note(before: u64, after: u64, over: Duration, active: &[&str]) -> Option<String> {
    const MB: u64 = 1024 * 1024;
    let grew = after.checked_sub(before)?;
    if grew < GROWTH_NOTE_MIN_BYTES {
        return None;
    }
    let during = if active.is_empty() {
        "nothing tracked in flight (a write between samples, or lbug's own checkpoint)".to_string()
    } else {
        active.join(", ")
    };
    Some(format!(
        "[growth] graph grew {} MB -> {} MB (+{} MB) in {}s during: {during}",
        before / MB,
        after / MB,
        grew / MB,
        over.as_secs()
    ))
}

/// What the coordinator does after one idle-fold probe (#166).
#[derive(Debug, PartialEq)]
struct IdleFoldAction {
    /// A line for the daemon log.
    log: Option<String>,
    /// The held graph must be reopened: its handle may not fold again.
    reopen: bool,
}

/// Book-keeping for one idle-fold probe (#166).
///
/// - `Folded` resets the backoff.
/// - `Busy` (another process's fold, or a writer mid-transaction) is
///   transient: no backoff, nothing logged.
/// - `Guarded` backs off and logs once per distinct reason, since the same
///   refusal on every attempt says nothing new; `last_skip` keeps it.
/// - `Failed`/`Disabled` back off, always log, and ask for a reopen. The
///   backoff survives the reopen, so a fold that cannot succeed runs at most
///   once per backoff period rather than once per probe -- sittir logged
///   ~1,600 of them two seconds apart.
fn settle_idle_fold(
    fold: Option<crate::graph::store::IdleFold>,
    backoff: &mut ReopenBackoff,
    last_skip: &mut Option<String>,
) -> IdleFoldAction {
    use crate::graph::store::{FoldError, IdleFold};
    let quiet = IdleFoldAction {
        log: None,
        reopen: false,
    };
    let e = match fold {
        None | Some(IdleFold::NotDue) | Some(IdleFold::NotFolded(FoldError::Busy(_))) => {
            return quiet
        }
        Some(IdleFold::Folded) => {
            backoff.record_success();
            *last_skip = None;
            return quiet;
        }
        Some(IdleFold::NotFolded(e)) => e,
    };
    let delay = backoff.record_failure();
    let reason = e.to_string();
    let line = format!(
        "[daemon] idle fold not done (consecutive: {}, next attempt in {}s): {reason}",
        backoff.consecutive_failures(),
        delay.as_secs()
    );
    if let FoldError::Guarded(_) = e {
        let repeat = last_skip.as_deref() == Some(reason.as_str());
        *last_skip = Some(reason);
        return IdleFoldAction {
            log: (!repeat).then_some(line),
            reopen: false,
        };
    }
    *last_skip = None;
    IdleFoldAction {
        log: Some(line),
        reopen: true,
    }
}

/// Serves a single `.request` file via `serve_one_request`, wrapped in the
/// same `index.lock` acquisition (`begin_index_op`) the pre-`IndexWorkQueue`
/// per-request loop used. `route_or_serve_request`'s in-scope `WriteRequest`
/// variants are coordinated through the shared queue and drain step instead,
/// but its fallback paths (out-of-scope variants, malformed JSON, corrupt
/// sibling extractions files) still execute immediately here -- doing so
/// unlocked would let them race the periodic reindex, the queue's own drain,
/// or a concurrent CLI `infigraph rebuild`, violating the single-writer
/// invariant. On contention the `.request` file is left in place (not
/// deleted) so it's retried on a later tick, matching the old behavior.
/// Does nothing while this daemon's own drain is in flight. That drain holds
/// `index.lock` until the loop thread reaps it -- so blocking here to wait
/// for that lock would park the only thread that can release it, a
/// self-deadlock broken only by the 30s acquire timeout. Returning instead
/// leaves the `.request` file in place, which is already this function's
/// contention behaviour: a later tick, after the drain is reaped, serves it.
fn serve_request_locked(
    root: &Path,
    path: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
    reopen_backoff: &mut ReopenBackoff,
    drain_in_flight: bool,
) {
    // While backing off, the `.request` file stays in place exactly as it
    // does under lock contention below -- served on a later tick.
    if drain_in_flight || !reopen_backoff.should_attempt() {
        return;
    }
    match begin_index_op(root, "infigraph daemon", Duration::from_secs(30)) {
        Ok(IndexOpOutcome::Acquired(_guard)) => match watch_db(root, registry, held) {
            Ok(prism) => {
                reopen_backoff.record_success();
                if let Err(e) = crate::daemon_protocol::serve_one_request(&prism, path) {
                    eprintln!("[daemon] failed to serve request {}: {e}", path.display());
                }
            }
            Err(e) => log_reopen_failure("daemon", reopen_backoff, &e),
        },
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] request-serving busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
        }
        Err(e) => {
            eprintln!("[daemon] request-serving busy ({e}), retrying next tick");
        }
    }
}

/// One log line per failed reopen, carrying the attempt count and the
/// backoff delay so a stuck holder shows up in the daemon log as an
/// escalating series rather than an identical line every tick. `{e:#}`
/// prints the whole context chain, which is where `Infigraph::init`
/// names the lock holder's pid.
fn log_reopen_failure(tag: &str, backoff: &mut ReopenBackoff, e: &anyhow::Error) {
    let delay = backoff.record_failure();
    eprintln!(
        "[{tag}] failed to reopen graph connection (consecutive failures: {}), next attempt in \
         {}s: {e:#}",
        backoff.consecutive_failures(),
        delay.as_secs()
    );
}

/// What the background full-reindex build task computes: a verified-good
/// fresh graph sitting at `graph.rebuilding`, or (via the `Result` this is
/// wrapped in) the reason it isn't. The swap itself, and everything that
/// touches `held`, happens afterward on the loop thread
/// (`finish_full_reindex`) -- `spawn_blocking`'s `'static` bound can't
/// capture `&mut Option<Arc<Infigraph>>`.
struct FullReindexBuildOutcome {
    indexed_files: usize,
    detected_languages: Vec<String>,
}

/// What the background build task hands back. The `index.lock` guard rides
/// along so the loop thread keeps holding it across the swap, matching
/// `DrainTaskOutput`'s shape.
struct FullReindexTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<FullReindexBuildOutcome>,
}

/// A full-reindex build executing on the background `Task<T>`, plus what the
/// loop thread needs to finish it once it completes. `Task<T>` itself
/// doesn't carry the request/reply paths -- those are this loop's own
/// bookkeeping, tracked alongside it.
struct PendingFullReindex {
    task: Task<FullReindexTaskOutput>,
    request_path: PathBuf,
    reply_path: PathBuf,
}

/// What the background SCIP-import task hands back. Mirrors
/// `FullReindexTaskOutput`'s shape (the `index.lock` guard rides along so
/// the loop thread keeps holding it across the reply-write step).
struct ScipImportTaskOutput {
    guard: crate::ops::IndexOpGuard,
    result: Result<crate::scip::ImportStats>,
}

/// A `WriteRequest::ScipImport` executing on the background `Task<T>`, plus
/// what the loop thread needs to finish it once it completes. Mirrors
/// `PendingFullReindex` -- SCIP import used to run synchronously inside
/// `serve_request_locked` (blocking the whole coordinator loop -- no
/// draining, no other request-serving, nothing -- for the entire import
/// duration on a large repo), the exact defect this background-task path
/// fixes.
struct PendingScipImport {
    task: Task<ScipImportTaskOutput>,
    request_path: PathBuf,
    reply_path: PathBuf,
}

/// What `route_or_serve_request` hands back to the coordinator's main loop:
/// either kind of background work it might have started this tick, so the
/// loop can track and reap whichever one it is on a later tick. The two
/// kinds are tracked as separate `Option` fields in the loop's own state
/// (`full_reindex_in_flight`/`scip_import_in_flight`), not merged into one
/// slot -- a full reindex and a client-submitted SCIP import are
/// independent and can be in flight at the same time.
enum PendingWork {
    FullReindex(PendingFullReindex),
    ScipImport(PendingScipImport),
}

/// The expensive, `held`-independent part of a full reindex: build a fresh
/// database at `graph.rebuilding`, scan/extract/upsert/resolve every file,
/// derive TESTED_BY edges. Runs entirely against its own connection, opened
/// fresh inside this call -- deliberately takes no `&mut Option<Arc<Infigraph>>`
/// so it can run inside `spawn_blocking`'s `'static` closure. The live graph
/// is never touched here; only `finish_full_reindex` (loop thread) swaps it
/// in.
///
/// `token` is checked once, right after the cheap leftover-cleanup step and
/// before the expensive Kuzu open+scan+upsert+resolve sequence -- abandoning
/// the build at (or before) this point is always safe, since nothing that
/// touches the live graph has happened yet. Nothing inside the expensive
/// sequence itself is cancellation-aware; a cancellation observed after this
/// checkpoint still runs to completion (still safe, just not responsive).
fn build_full_reindex(
    root: &Path,
    registry: crate::lang::LanguageRegistry,
    token: &CancellationToken,
) -> Result<FullReindexBuildOutcome> {
    const REBUILDING_NAME: &str = "graph.rebuilding";
    let rebuilding_path = root.join(".infigraph").join(REBUILDING_NAME);

    // Clean up any stale leftover from a previously-interrupted rebuild
    // attempt (e.g. the daemon was killed mid-rebuild last time) before
    // starting a new one. Unconditional, and covering the WAL family as
    // well as the base image -- see the original function's comment (now
    // removed) for why a surviving WAL sibling permanently wedges every
    // future full reindex if left in place.
    let _ = std::fs::remove_dir_all(&rebuilding_path);
    let _ = std::fs::remove_file(&rebuilding_path);
    crate::graph::remove_wal_family(&rebuilding_path);

    if token.is_cancelled() {
        return Err(anyhow::anyhow!(
            "full reindex build cancelled before starting"
        ));
    }

    let build_result = Infigraph::open_local_kuzu_at(root, registry, rebuilding_path.clone())
        .and_then(|fresh| {
            let backend = fresh
                .backend()
                .ok_or_else(|| anyhow::anyhow!("freshly-opened backend was not initialized"))?;
            let scan = fresh.scan_changed_files(backend)?;
            let detected_languages: std::collections::HashSet<String> = scan
                .extractions
                .iter()
                .map(|e| e.language.clone())
                .collect();
            if !scan.extractions.is_empty() {
                backend.upsert_files_bulk(&scan.extractions, true)?;
            }
            let _resolve_stats = backend.resolve_calls(&scan.extractions, None)?;
            // The swap replaces the graph wholesale, so whatever TESTED_BY
            // edges the live graph had are about to be discarded -- derive
            // them here or they are gone for good. `None` scope means
            // "everything", matching how the local `--full` path calls it.
            // Non-fatal, mirroring that path's warn-and-continue.
            if let Err(e) = backend.derive_tested_by_edges(None) {
                eprintln!("[daemon] full-reindex: TESTED_BY derivation failed: {e}");
            }
            Ok(FullReindexBuildOutcome {
                indexed_files: scan.extractions.len(),
                detected_languages: detected_languages.into_iter().collect(),
            })
        });

    if build_result.is_err() {
        // The live graph was never touched -- clean up the incomplete
        // rebuild attempt so the next full reindex doesn't inherit a
        // half-built `graph.rebuilding` or a foreign-ID WAL.
        let _ = std::fs::remove_dir_all(&rebuilding_path);
        let _ = std::fs::remove_file(&rebuilding_path);
        crate::graph::remove_wal_family(&rebuilding_path);
    }

    build_result
}

/// Loop-thread entry point for a `WriteRequest::FullReindex`. Does the
/// cheap, synchronous gating (never overlap a queue drain or another full
/// reindex -- both write the same live graph) and, if clear, acquires
/// `index.lock` and hands the expensive build off to `drain_rt` so this loop
/// keeps ticking (accepting fsevents, other requests, the stop signal) for
/// the whole multi-minute rebuild instead of blocking on it. Returns `None`
/// if nothing was started (busy, or an early failure already replied and
/// cleaned up the request file itself); `Some` if a build was scheduled --
/// the caller must track the returned handle and reap it via
/// `finish_full_reindex` on a later tick.
#[allow(clippy::too_many_arguments)]
fn try_start_full_reindex<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::daemon::queue::IndexWorkQueue>>,
    make_registry: &MR,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
) -> Option<PendingFullReindex>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");

    // Never overlap a queue drain or another full reindex -- same
    // invariant every locked write path in this loop preserves; both
    // touch the same live graph. Deferring here (rather than blocking)
    // gets "wait for whichever is in progress to finish first" for free:
    // `begin_index_op` below serializes on the same `index.lock` either
    // one takes regardless, but skipping the attempt when we already know
    // it's busy avoids spawning a task that's certain to lose the race.
    //
    // Deliberately does NOT check for an in-flight SCIP task: SCIP's
    // external-indexer phase touches nothing in the graph (see
    // `run_scip_indexers` in infigraph-cli), and its import phase acquires
    // `index.lock` itself, narrowly, around just that step -- so it
    // already serializes correctly against a concurrent full reindex
    // without needing a loop-level gate here too. Gating on it here would
    // reintroduce the exact regression this split fixed: SCIP's slow,
    // graph-independent indexer-running phase blocking every other write
    // for its entire duration.
    if drain_in_flight || full_reindex_in_flight {
        return None;
    }

    let guard = match begin_index_op(
        root,
        "infigraph daemon (full reindex)",
        Duration::from_secs(30),
    ) {
        Ok(IndexOpOutcome::Acquired(guard)) => guard,
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] full-reindex busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
            return None;
        }
        Err(e) => {
            eprintln!("[daemon] full-reindex busy ({e}), retrying next tick");
            return None;
        }
    };

    // Anything only queued (not yet executing) is genuinely moot -- the
    // full reindex is about to re-scan every file from disk regardless of
    // what was pending. Its waiters still get answered, just with a
    // superseded reply rather than silence.
    let superseded = queue.lock().unwrap().drain();
    for waiter in &superseded.waiters {
        let result = crate::daemon_protocol::WriteResult::Err {
            message: "superseded by a full reindex; resubmit if still needed".to_string(),
        };
        if let Ok(json) = serde_json::to_string(&result) {
            let _ = crate::daemon_protocol::write_atomic(&waiter.reply_path, &json);
        }
    }

    let registry = match make_registry() {
        Ok(r) => r,
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: could not build language registry: {e}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(&reply_path, &json);
            }
            std::fs::remove_file(path).ok();
            drop(guard);
            return None;
        }
    };

    let root_buf = root.to_path_buf();
    // `Task::spawn_blocking` dispatches via the ambient `tokio::task::
    // spawn_blocking`, which needs a runtime context on this (plain OS)
    // thread -- `drain_rt.enter()` scopes that context to just this call, so
    // the task still runs on `drain_rt`'s blocking pool, matching every
    // other `spawn_blocking` in this loop.
    let task = {
        let _guard = drain_rt.enter();
        Task::spawn_blocking(daemon_token, "full-reindex-build", move |token| {
            FullReindexTaskOutput {
                result: build_full_reindex(&root_buf, registry, &token),
                guard,
            }
        })
    };

    Some(PendingFullReindex {
        task,
        request_path: path.to_path_buf(),
        reply_path,
    })
}

/// Loop-thread entry point for a `WriteRequest::ScipImport`. Mirrors
/// `try_start_full_reindex`'s shape, but much simpler: a SCIP import writes
/// directly into the live graph in place (`Infigraph::import_scip`'s own
/// bulk COPY/UNWIND, protected by `GraphStore::write_lock`), there is no
/// build-then-swap -- so no snapshot/retire/rollback machinery is needed
/// here, only the background-task-plus-reply plumbing.
///
/// Returns `None` if nothing was started (busy, or an early failure already
/// replied and cleaned up the request file itself); `Some` if an import was
/// scheduled -- the caller must track the returned handle and reap it via
/// `finish_scip_import` on a later tick.
#[allow(clippy::too_many_arguments)]
fn try_start_scip_import(
    root: &Path,
    path: &Path,
    scip_path: PathBuf,
    enriched_ast_generation: Option<i64>,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
    reopen_backoff: &mut ReopenBackoff,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    scip_import_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
) -> Option<PendingScipImport> {
    let reply_path = path.with_extension("result");

    // Same "never overlap" reasoning as `try_start_full_reindex`'s gate --
    // this writes the same live graph a drain or a full reindex writes.
    // Two SCIP imports at once are also refused: `Infigraph::import_scip`
    // itself serializes via `GraphStore::write_lock`, so a second one would
    // just block inside the background task rather than run concurrently,
    // silently doubling this loop's in-flight bookkeeping for no benefit.
    // While backing off a failed reopen the request stays in place, served
    // on a later tick -- the same contract as `serve_request_locked`.
    // Without this a pending import with nothing held reopened the graph on
    // every 200ms tick (#166).
    if drain_in_flight
        || full_reindex_in_flight
        || scip_import_in_flight
        || !reopen_backoff.should_attempt()
    {
        return None;
    }

    // Needs the daemon's own already-open connection -- `Infigraph::
    // import_scip` must not open a second `Database` on the same live graph
    // path (Kuzu only allows safe concurrent access within one process's
    // `Database` object, not across two, even in the same process).
    let prism = match watch_db(root, registry, held) {
        Ok(p) => {
            reopen_backoff.record_success();
            p
        }
        Err(e) => {
            log_reopen_failure("daemon scip-import", reopen_backoff, &e);
            return None;
        }
    };

    let guard = match begin_index_op(
        root,
        "infigraph daemon (scip import)",
        Duration::from_secs(30),
    ) {
        Ok(IndexOpOutcome::Acquired(guard)) => guard,
        Ok(o @ IndexOpOutcome::AlreadyRunning(_)) => {
            eprintln!(
                "[daemon] scip-import busy ({}), retrying next tick",
                o.skip_note().unwrap_or_default()
            );
            return None;
        }
        Err(e) => {
            eprintln!("[daemon] scip-import busy ({e}), retrying next tick");
            return None;
        }
    };

    let task = {
        let _guard = drain_rt.enter();
        Task::spawn_blocking(daemon_token, "scip-import", move |_token| {
            let result = prism.import_scip_enriched_at(&scip_path, enriched_ast_generation);
            // Best-effort cleanup of the `.scip` file regardless of
            // outcome, mirroring the old direct-write path's behavior.
            let _ = std::fs::remove_file(&scip_path);
            ScipImportTaskOutput { guard, result }
        })
    };

    Some(PendingScipImport {
        task,
        request_path: path.to_path_buf(),
        reply_path,
    })
}

/// Loop-thread finish for a completed SCIP import: logs one structured
/// completion line, refreshes embeddings for anything the import added,
/// writes the reply, and returns the touched-files list (over-approximated
/// -- every file the SCIP index covered, see `ImportStats::touched_files`)
/// so the caller can give them the same `on_event` notification an ordinary
/// drain gives its extracted files.
fn finish_scip_import(
    root: &Path,
    reply_path: &Path,
    held: &HeldPrism,
    joined: std::result::Result<ScipImportTaskOutput, tokio::task::JoinError>,
) -> (Option<crate::ops::IndexOpGuard>, Vec<String>) {
    let ScipImportTaskOutput { guard, result } = match joined {
        Ok(output) => output,
        Err(join_err) => {
            eprintln!("[daemon] scip-import task panicked: {join_err}");
            let write_result = crate::daemon_protocol::WriteResult::Err {
                message: format!("daemon scip-import task panicked: {join_err}"),
            };
            if let Ok(json) = serde_json::to_string(&write_result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (None, Vec::new());
        }
    };

    let touched_files = match result {
        Ok(stats) => {
            eprintln!("[daemon] SCIP import complete: {stats}");
            if let Some(prism) = held.as_ref() {
                if let Some(backend) = prism.backend() {
                    // `update_embeddings` returns the total embedding count
                    // on disk after reconciling, not how many it re-embedded
                    // (unchanged inputs are skipped by hash) -- log it as
                    // exactly that, so four imports in a row printing the
                    // same ~11k don't read as four full re-embeds.
                    match crate::embed::update_embeddings(backend, root, &[]) {
                        Ok(n) if n > 0 => {
                            eprintln!("[daemon] scip-import: embeddings reconciled ({n} symbols)")
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("[daemon] scip-import: embedding update failed: {e}"),
                    }
                }
            }
            let touched = stats.touched_files.clone();
            let write_result = crate::daemon_protocol::WriteResult::ScipImportOk(stats);
            if let Ok(json) = serde_json::to_string(&write_result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            touched
        }
        Err(e) => {
            let write_result = crate::daemon_protocol::WriteResult::Err {
                message: format!("SCIP import failed: {e:#}"),
            };
            if let Ok(json) = serde_json::to_string(&write_result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            Vec::new()
        }
    };

    (Some(guard), touched_files)
}

/// Loop-thread finish for a completed full-reindex build: poison the
/// daemon's own connection, swap the verified-good rebuilt graph in for the
/// live one (still under `graph.lock`, not `index.lock` -- see the
/// `graph_lock` comment below), reopen and reconcile embeddings, and reply.
/// Mirrors `finish_drain`'s "task does the heavy work, loop thread does the
/// `held`-touching part" split.
///
/// Returns the `index.lock` guard to drop (absent if the task panicked, since
/// it was dropped during the unwind -- same convention as `finish_drain`),
/// and `Some(detected_languages)` only when the swap fully succeeded --
/// that's the signal the caller uses to schedule SCIP enrichment. Any
/// failure path returns `None` for languages: don't enrich a reindex that
/// didn't actually land.
fn finish_full_reindex(
    root: &Path,
    reply_path: &Path,
    registry: &Arc<crate::lang::LanguageRegistry>,
    held: &mut HeldPrism,
    joined: std::result::Result<FullReindexTaskOutput, tokio::task::JoinError>,
) -> (Option<crate::ops::IndexOpGuard>, Option<Vec<String>>) {
    let FullReindexTaskOutput { guard, result } = match joined {
        Ok(output) => output,
        Err(join_err) => {
            eprintln!("[watch] full-reindex task panicked: {join_err}");
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("daemon full-reindex task panicked: {join_err}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (None, None);
        }
    };

    let (indexed_files, detected_languages) = match result {
        Ok(outcome) => (outcome.indexed_files, outcome.detected_languages),
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!("full reindex failed: {e:#}"),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (Some(guard), None);
        }
    };

    const LIVE_NAME: &str = "graph";
    const REBUILDING_NAME: &str = "graph.rebuilding";
    let infigraph_dir = root.join(".infigraph");
    let rebuilding_path = infigraph_dir.join(REBUILDING_NAME);
    let live_path = infigraph_dir.join(LIVE_NAME);

    // The live graph was never touched up to this point -- only now, with a
    // verified-good fresh build in hand, do we poison the daemon's own
    // handle and swap.
    poison_watch_db(held);

    // Replacing the live graph on disk is a graph-level write, so it takes
    // the same advisory lock every writer takes -- `index.lock` (held by
    // `guard`) does not cover writers that only take `graph.lock`, notably
    // `init()`'s corruption-retry calling `wipe_graph`. Scoped narrowly to
    // the destructive section, matching `wipe_graph`: the reopen further
    // down re-acquires this same lock through `GraphStore`, so holding it
    // any wider would deadlock against ourselves.
    let graph_lock = match crate::lockfile::acquire(
        &live_path.with_extension("lock"),
        "full-reindex-swap",
        Duration::from_secs(5),
    ) {
        Ok(l) => l,
        Err(e) => {
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!(
                    "full reindex rebuilt successfully but could not take the graph write lock \
                     to swap it in: {e:#}. The live graph at {} was left untouched; the rebuilt \
                     graph remains at {}",
                    live_path.display(),
                    rebuilding_path.display()
                ),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            return (Some(guard), None);
        }
    };

    // Bind the retirement destination rather than discarding it -- if the
    // move-aside succeeds but the rename below fails, `live_path` no longer
    // exists (it's now this destination), so an error message that names
    // `live_path` would point an operator at a path that's already gone and
    // never say where the real data actually went.
    let retired_path: Option<PathBuf> = if live_path.exists() {
        // Two backup mechanisms, deliberately layered. create_snapshot gives
        // a whole-`.infigraph/`-tree safety net matching the local
        // `infigraph rebuild` path (R3.2.1/docs/DESIGN-hardening.md
        // §3.2), so a restore brings back graph and sidecars together.
        // retire_previous_graph then does the actual move-aside of the live
        // graph file -- a *rename*, not a delete, so a failure below (the
        // swap-in rename itself, or the swapped-in graph failing to reopen)
        // can restore the exact prior live graph with a single rename back
        // (see `roll_back_to_retired`), rather than the graph having been
        // permanently removed before the swap was even attempted -- the gap
        // an earlier version of this function had, caught by adversarial
        // review before it shipped.
        match crate::snapshot::create_snapshot(&infigraph_dir) {
            Ok(_snapshot_dest) => {
                match crate::quarantine::retire_previous_graph(&infigraph_dir, LIVE_NAME) {
                    Ok(dest) => Some(dest),
                    Err(e) => {
                        let result = crate::daemon_protocol::WriteResult::Err {
                            message: format!(
                                "full reindex rebuilt successfully but could not move the old \
                                 graph aside: {e:#}. The live graph at {} was left untouched; \
                                 the rebuilt graph remains at {}",
                                live_path.display(),
                                rebuilding_path.display()
                            ),
                        };
                        if let Ok(json) = serde_json::to_string(&result) {
                            let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
                        }
                        drop(graph_lock);
                        return (Some(guard), None);
                    }
                }
            }
            Err(e) => {
                let result = crate::daemon_protocol::WriteResult::Err {
                    message: format!(
                        "full reindex rebuilt successfully but could not snapshot the old graph \
                         aside: {e:#}. The live graph at {} was left untouched; the rebuilt \
                         graph remains at {}",
                        live_path.display(),
                        rebuilding_path.display()
                    ),
                };
                if let Ok(json) = serde_json::to_string(&result) {
                    let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
                }
                drop(graph_lock);
                return (Some(guard), None);
            }
        }
    } else {
        None
    };

    // Unconditional, regardless of which branch above ran: a `graph.wal*`
    // family can still be sitting at `live_path` even after a successful
    // retire (a copy-then-remove fallback can leave the original in place),
    // and when there was no base image to retire at all, an orphaned
    // `graph.wal*` from an earlier crash could still be here. Either way,
    // renaming the rebuilt graph on top of a foreign-ID WAL would make the
    // graph we just swapped in unopenable, so clear it unconditionally
    // right before the swap.
    crate::graph::remove_wal_family(&live_path);

    let swap = std::fs::rename(&rebuilding_path, &live_path);
    if swap.is_ok() {
        // The base image moved; its WAL-family siblings have to follow it
        // or a not-yet-checkpointed WAL belonging to the graph we just
        // swapped in is lost -- and worse, it stays behind at the rebuild
        // path for a later reindex to inherit. Same rename-then-copy
        // fallback the retirement path uses, for the same reason: a silent
        // failure here leaves a foreign WAL where it does damage.
        for src in crate::graph::wal_family_paths(&rebuilding_path) {
            let name = src.file_name().unwrap_or_default().to_string_lossy();
            let suffix = name.strip_prefix(REBUILDING_NAME).unwrap_or(&name);
            let dest = infigraph_dir.join(format!("{LIVE_NAME}{suffix}"));
            if let Err(e) = crate::quarantine::move_wal_sibling(&src, &dest) {
                eprintln!(
                    "[daemon] full-reindex: could not carry WAL sibling {} across to {} ({e:#}) \
                     -- the swapped-in graph may be missing uncheckpointed writes; the leftover \
                     is cleaned up by the next full reindex",
                    src.display(),
                    dest.display()
                );
            }
        }
    }
    if let Err(e) = &swap {
        // The rebuilt graph is verified-good and still sitting at
        // `rebuilding_path` -- nothing was lost. Roll the prior live graph
        // back into place immediately, while `graph_lock` is still held
        // (this is a plain rename, not a `GraphStore::open`, so no
        // deadlock risk) -- a failed swap must not leave the project
        // without a live graph at all, closing the outage window an
        // earlier version of this function had (caught by adversarial
        // review before it shipped).
        let rollback_note = roll_back_to_retired(&live_path, &retired_path);
        drop(graph_lock);
        eprintln!(
            "[daemon] full-reindex swap failed after a successful rebuild: {e} -- \
             {rollback_note}; the rebuilt graph is at {} -- check both by hand",
            rebuilding_path.display()
        );
        let result = crate::daemon_protocol::WriteResult::Err {
            message: format!(
                "full reindex rebuilt successfully but the swap failed: {e}. {rollback_note}; \
                 the rebuilt graph is at {}",
                rebuilding_path.display()
            ),
        };
        if let Ok(json) = serde_json::to_string(&result) {
            let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
        }
        return (Some(guard), None);
    }
    // graph_lock must be released before watch_db below -- it opens a fresh
    // GraphStore, which takes the same lock for schema init, and this
    // process already holding it would deadlock against itself.
    drop(graph_lock);

    // The swap succeeded -- verify the new graph actually opens before
    // declaring success and discarding the ability to roll back.
    let mut reopened = watch_db(root, registry, held);
    // #148: a single failed open in a single process is not a corruption
    // verdict. Examine the image out of process first; if it is fine (or
    // was made fine by setting a torn WAL aside) the reopen gets one more
    // try, and only a genuinely unopenable image is quarantined.
    let mut swap_reopen = None;
    if reopened.is_err() {
        let outcome = recover_or_quarantine_swapped_in_graph(
            &infigraph_dir,
            LIVE_NAME,
            &live_path,
            &retired_path,
        );
        if matches!(outcome, SwapReopen::GraphIsHealthy) {
            // Worth a line even when the retry then succeeds: on the
            // `Recovered` verdict a torn WAL was just filed aside, and an
            // operator reading `FullReindexOk` alone would never know.
            eprintln!(
                "[daemon] full-reindex: the swapped-in graph opens in a fresh probe process, \
                 so the failed reopen was not its fault -- retrying rather than quarantining a \
                 healthy graph (#148)"
            );
            reopened = watch_db(root, registry, held);
        }
        swap_reopen = Some(outcome);
    }
    match reopened {
        Ok(prism) => {
            // R3.1.4d/#100: this is a *verified* healthy checkpoint -- the
            // swap succeeded and the swapped-in graph just reopened -- so
            // it's the right moment to refresh the growth-ratio breaker's
            // baseline. Ordinary incremental writes deliberately do not
            // (see `stamp_healthy_graph_size`'s doc comment).
            crate::graph::stamp_healthy_graph_size(&infigraph_dir, &live_path);

            // Reconcile embeddings against the NEW graph -- update_embeddings
            // queries the live symbol set and prunes anything not in it, so
            // this converges embeddings.bin to the rebuilt graph regardless
            // of whether it was wiped first (it wasn't, deliberately).
            if let Some(backend) = prism.backend() {
                if let Err(e) = crate::embed::update_embeddings(backend, root, &[]) {
                    eprintln!("[daemon] full-reindex: embedding update failed: {e}");
                }
            }
            let result = crate::daemon_protocol::WriteResult::FullReindexOk {
                total_files: indexed_files,
                indexed_files,
                detected_languages: detected_languages.clone(),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            (Some(guard), Some(detected_languages))
        }
        Err(reopen_err) => {
            // Say what actually happened to the graph, per verdict. The
            // old wording announced a quarantine unconditionally, which
            // was already untrue on the lock-failure path and is untrue
            // for every healthy image now that one is possible.
            let disposition = match swap_reopen {
                Some(SwapReopen::Quarantined { rollback_note }) => format!(
                    "the graph opens neither with its WAL nor without it, so it was \
                     quarantined as corruption evidence; {rollback_note}"
                ),
                Some(SwapReopen::GraphIsHealthy) => format!(
                    "a fresh probe process opened AND scanned the swapped-in graph at {} \
                     successfully -- it is not corrupt, and it is still live. Something \
                     transient outlasted two reopen attempts here; refusing to quarantine a \
                     healthy graph. Retry the reindex, or restart the daemon.",
                    live_path.display()
                ),
                Some(SwapReopen::Undecided { note }) => note,
                // Unreachable: `swap_reopen` is set on every path that can
                // leave `reopened` an Err.
                None => "no verdict was reached".to_string(),
            };
            eprintln!(
                "[daemon] full-reindex: swapped-in graph failed to reopen: {reopen_err:#} -- \
                 {disposition}"
            );
            let result = crate::daemon_protocol::WriteResult::Err {
                message: format!(
                    "full reindex swap completed but the new graph failed to reopen: \
                     {reopen_err:#}. {disposition}"
                ),
            };
            if let Ok(json) = serde_json::to_string(&result) {
                let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
            }
            (Some(guard), None)
        }
    }
}

/// What became of a graph the daemon swapped in but could not reopen.
#[derive(Debug)]
enum SwapReopen {
    /// A fresh process opens the graph, so our own reopen failed for a
    /// reason that had nothing to do with the image. Nothing was moved
    /// aside and the rebuilt graph is still live; the caller should try
    /// reopening once more (#148).
    GraphIsHealthy,
    /// The graph opens neither with its WAL nor without it. It has been
    /// filed as corruption evidence and the prior live graph rolled back
    /// into place; the note describes how that rollback went.
    Quarantined { rollback_note: String },
    /// The lock guarding those renames could not be taken, so no verdict
    /// was reached and nothing was moved aside.
    Undecided { note: String },
}

/// `finish_full_reindex` swapped a freshly built graph in and then could not
/// reopen it. Decide what that means for the image and act on it: quarantine
/// it as verified-bad (R3.1.2) and roll the prior live graph back into place
/// rather than leaving a broken graph live or the project graph-less.
///
/// Takes `graph.lock` itself for those renames -- no `GraphStore::open`
/// happens here, so there is no deadlock risk against the lock
/// `finish_full_reindex` dropped before reopening. Without it a concurrent
/// single-file write could race the rollback, the same class of gap
/// `full_reindex_wipe` closes on the local path.
fn recover_or_quarantine_swapped_in_graph(
    infigraph_dir: &Path,
    graph_name: &str,
    live_path: &Path,
    retired_path: &Option<PathBuf>,
) -> SwapReopen {
    recover_or_quarantine_swapped_in_graph_with(
        infigraph_dir,
        graph_name,
        live_path,
        retired_path,
        crate::probe::graph_opens,
    )
}

/// [`recover_or_quarantine_swapped_in_graph`] with the health probe
/// injected. A test binary must never reach the real probe: `current_exe()`
/// there is libtest's harness, which re-runs the whole suite instead of
/// probing (see [`crate::probe`]).
fn recover_or_quarantine_swapped_in_graph_with(
    infigraph_dir: &Path,
    graph_name: &str,
    live_path: &Path,
    retired_path: &Option<PathBuf>,
    probe: impl Fn(&Path) -> bool,
) -> SwapReopen {
    match crate::lockfile::acquire(
        &live_path.with_extension("lock"),
        "full-reindex-rollback",
        std::time::Duration::from_secs(5),
    ) {
        Ok(_lock) => {
            // Ask the same question the other two quarantine sites ask,
            // through the same verdict type, rather than inferring
            // corruption from the one open that happened to fail here.
            // Only `NotRecoverable` permits a quarantine; the other two
            // outcomes mean the image is fine and the caller may retry.
            let verdict = crate::quarantine::try_recover_by_setting_wal_aside_with(
                infigraph_dir,
                graph_name,
                probe,
            );
            if verdict.is_usable() {
                return SwapReopen::GraphIsHealthy;
            }
            debug_assert!(verdict.permits_quarantine());
            let _ = crate::quarantine::quarantine_graph(infigraph_dir, graph_name);
            SwapReopen::Quarantined {
                rollback_note: roll_back_to_retired(live_path, retired_path),
            }
        }
        Err(e) => SwapReopen::Undecided {
            note: format!(
                "could not acquire the graph lock to roll back: {e:#} -- manual recovery needed"
            ),
        },
    }
}

/// Attempt to restore the graph `retire_previous_graph` moved aside (see
/// `finish_full_reindex`'s `retired_path`) back to `live_path`, after
/// either the swap-in rename failed or the swapped-in graph failed to
/// reopen. Returns a human-readable note for both the daemon log and the
/// `WriteResult` error message.
fn roll_back_to_retired(live_path: &Path, retired_path: &Option<PathBuf>) -> String {
    match retired_path {
        Some(prev) if prev.exists() => match std::fs::rename(prev, live_path) {
            Ok(()) => "the prior live graph was restored".to_string(),
            Err(e) => format!(
                "the prior live graph at {} could NOT be restored: {e:#} -- manual recovery needed",
                prev.display()
            ),
        },
        Some(prev) => format!(
            "no prior graph to restore from (expected at {}, already gone)",
            prev.display()
        ),
        None => "there was no prior graph to restore (this was a from-scratch build)".to_string(),
    }
}

/// Parses a `.request` file and either enqueues it (for the four
/// index-shaped `WriteRequest` variants this design coordinates) or falls
/// through to `serve_request_locked` (unchanged `serve_one_request`
/// dispatch, still under `index.lock`) for everything else. Enqueued
/// requests' `.request` file is deleted immediately (the daemon has already
/// accepted responsibility for serving it the moment it's queued) -- the
/// reply arrives later, written by `execute_drain`.
#[allow(clippy::too_many_arguments)]
fn route_or_serve_request<MR>(
    root: &Path,
    path: &Path,
    queue: &Arc<Mutex<crate::daemon::queue::IndexWorkQueue>>,
    registry: &Arc<crate::lang::LanguageRegistry>,
    make_registry: &MR,
    held: &mut HeldPrism,
    reopen_backoff: &mut ReopenBackoff,
    drain_in_flight: bool,
    full_reindex_in_flight: bool,
    drain_rt: &tokio::runtime::Runtime,
    daemon_token: &CancellationToken,
    code_watch: &mut CodeWatch,
    docs_control: Option<&Arc<DocsControl>>,
    // Set to `true` when the request asks the whole daemon to stop; the
    // coordinator's loop reads it to decide whether to break.
    shutdown_requested: &mut bool,
    scip_import_in_flight: bool,
) -> Option<PendingWork>
where
    MR: Fn() -> Result<crate::lang::LanguageRegistry>,
{
    let reply_path = path.with_extension("result");
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return None, // transient; will be retried next tick if the file reappears
    };
    let request: crate::daemon_protocol::WriteRequest = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(_) => {
            // Malformed request JSON -- not this design's concern to
            // recover; hand off to serve_one_request, whose existing
            // corrupt-JSON handling (WriteResult::Err) already covers it.
            serve_request_locked(root, path, registry, held, reopen_backoff, drain_in_flight);
            return None;
        }
    };

    use crate::daemon::queue::{Waiter, WaiterKind};
    use crate::daemon_protocol::WriteRequest;

    // Each arm holds the queue lock across its work-items-plus-waiter pair
    // so a drain scheduled concurrently can never take the items without
    // the waiter that's blocked on them (which would leave that client
    // waiting for a reply no later drain owes it).
    match request {
        WriteRequest::Index { paths: None } => {
            let mut q = queue.lock().unwrap();
            q.mark_whole_project();
            q.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
                paths: None,
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
        }
        WriteRequest::Index { paths: Some(paths) } => {
            let mut q = queue.lock().unwrap();
            let mut rel_paths = Vec::with_capacity(paths.len());
            for p in paths {
                // `p` may be absolute -- `Infigraph::index_file`/`index_files`
                // both forward the caller's path verbatim (absolute paths are
                // an explicitly documented option), and `extract_paths` below
                // joins it onto `root` unnormalized, which for an absolute
                // path is a no-op join that leaves it absolute. Mirrors the
                // batch-flush block above.
                let rel = p
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
                q.add_raw(rel.clone());
                rel_paths.push(rel);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::Index,
                use_learned: false,
                reply_path,
                paths: Some(rel_paths),
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
        }
        WriteRequest::UpsertFilesBulk {
            extractions_path,
            // `existing_hashes_empty` is a snapshot the client captured when
            // it built this request; it's not threaded through the queue
            // because `execute_drain` recomputes the same flag itself, at
            // actual drain time, from the backend's live file-hash state --
            // strictly fresher than the client's snapshot, especially once
            // other producers' work has been folded into the same drain.
            // Dropped deliberately, not an oversight.
            ..
        } => match crate::daemon_protocol::read_extractions_json(&extractions_path) {
            Ok(extractions) => {
                let mut q = queue.lock().unwrap();
                let rel_paths: Vec<String> = extractions.iter().map(|e| e.file.clone()).collect();
                for extraction in extractions {
                    q.add_structured(extraction);
                }
                q.add_waiter(Waiter {
                    kind: WaiterKind::UpsertFilesBulk,
                    use_learned: false,
                    reply_path,
                    paths: Some(rel_paths),
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
                None
            }
            Err(_) => {
                // Sibling extractions file missing/corrupt -- fall
                // through to serve_one_request's existing error path.
                serve_request_locked(root, path, registry, held, reopen_backoff, drain_in_flight);
                None
            }
        },
        WriteRequest::RemoveFiles { files } => {
            let mut q = queue.lock().unwrap();
            let rel_paths = files.clone();
            for f in files {
                q.add_removal(f);
            }
            q.add_waiter(Waiter {
                kind: WaiterKind::RemoveFiles,
                use_learned: false,
                reply_path,
                paths: Some(rel_paths),
            });
            drop(q);
            std::fs::remove_file(path).ok();
            None
        }
        WriteRequest::ResolveCalls {
            extractions_path,
            use_learned,
        } => match crate::daemon_protocol::read_extractions_json(&extractions_path) {
            Ok(extractions) => {
                let mut q = queue.lock().unwrap();
                for extraction in extractions {
                    q.add_resolve_only(extraction);
                }
                q.add_waiter(Waiter {
                    kind: WaiterKind::ResolveCalls,
                    use_learned,
                    reply_path,
                    // ResolveCalls replies carry `ResolveStats` (call-edge
                    // counts), not a file count -- not path-attributable the
                    // way Index/UpsertFilesBulk/RemoveFiles are.
                    paths: None,
                });
                drop(q);
                std::fs::remove_file(&extractions_path).ok();
                std::fs::remove_file(path).ok();
                None
            }
            Err(_) => {
                serve_request_locked(root, path, registry, held, reopen_backoff, drain_in_flight);
                None
            }
        },
        WriteRequest::FullReindex => try_start_full_reindex(
            root,
            path,
            queue,
            make_registry,
            drain_in_flight,
            full_reindex_in_flight,
            drain_rt,
            daemon_token,
        )
        .map(PendingWork::FullReindex),
        WriteRequest::ScipImport {
            scip_path,
            enriched_ast_generation,
        } => try_start_scip_import(
            root,
            path,
            scip_path,
            enriched_ast_generation,
            registry,
            held,
            reopen_backoff,
            drain_in_flight,
            full_reindex_in_flight,
            scip_import_in_flight,
            drain_rt,
            daemon_token,
        )
        .map(PendingWork::ScipImport),
        WriteRequest::WatchControl { role, action } => {
            let outcome = match role {
                // `Enable`/`Disable` differ from `Start`/`Stop` only in
                // whether the *caller* also wrote the persisted flag in
                // config.toml (Phase 4). Their effect on the live task is
                // identical, so this arm treats them the same.
                WatchRole::Code => {
                    match action {
                        WatchAction::Stop | WatchAction::Disable => code_watch.stop(),
                        WatchAction::Start | WatchAction::Enable => code_watch.start(),
                        WatchAction::Restart => {
                            code_watch.stop();
                            code_watch.start();
                        }
                    }
                    Ok(())
                }
                WatchRole::Docs => match docs_control {
                    Some(control) => control(action),
                    None => {
                        Err("this watcher does not own a doc-watch loop to control".to_string())
                    }
                },
                // Only the process's own exit is expressible here: `Start`
                // is meaningless (you are talking to a daemon, so one
                // exists), and a real `Restart` is the *client's* job --
                // this process can only stop itself. Both stop; the reply
                // is written before cancelling so the caller still gets it.
                WatchRole::Daemon => match action {
                    WatchAction::Stop | WatchAction::Restart => Ok(()),
                    _ => {
                        Err("WatchControl { role: Daemon } only supports Stop/Restart".to_string())
                    }
                },
            };
            let daemon_stop = matches!(
                (role, action, &outcome),
                (
                    WatchRole::Daemon,
                    WatchAction::Stop | WatchAction::Restart,
                    Ok(())
                )
            );
            reply_to_watch_control(&reply_path, outcome);
            std::fs::remove_file(path).ok();
            if daemon_stop {
                // Two separate signals, deliberately: the token tears down
                // whatever background work is still spawned beneath it, and
                // the flag tells the coordinator's own loop to stop ticking.
                // The loop must not infer the second from the first -- see
                // `shutdown_requested`'s declaration.
                *shutdown_requested = true;
                daemon_token.cancel();
            }
            None
        }
        _ => {
            serve_request_locked(root, path, registry, held, reopen_backoff, drain_in_flight);
            None
        }
    }
}

/// Answers a `WatchControl` request. Same `write_atomic`/`WriteResult`
/// shape every other reply in this module uses; the counts are zero because
/// watch-control moves no files through the graph.
fn reply_to_watch_control(reply_path: &Path, outcome: std::result::Result<(), String>) {
    let result = match outcome {
        Ok(()) => crate::daemon_protocol::WriteResult::Ok {
            total_files: 0,
            indexed_files: 0,
        },
        Err(message) => crate::daemon_protocol::WriteResult::Err { message },
    };
    if let Ok(json) = serde_json::to_string(&result) {
        let _ = crate::daemon_protocol::write_atomic(reply_path, &json);
    }
}

/// Returns true if the file has any resolved CALLS edges to/from symbols in other files.
fn has_cross_file_calls(prism: &Infigraph, rel_path: &str) -> bool {
    let backend = match prism.backend() {
        Some(b) => b,
        None => return false,
    };
    let escaped = rel_path.replace('\'', "\\'");
    let q = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE a.file = '{escaped}' AND b.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(result) = backend.raw_query(&q) {
        if let Some(row) = result.first() {
            if let Some(val) = row.first() {
                if val.to_string().parse::<u64>().unwrap_or(0) > 0 {
                    return true;
                }
            }
        }
    }
    let q2 = format!(
        "MATCH (a:Symbol)-[:CALLS]->(b:Symbol) WHERE b.file = '{escaped}' AND a.file <> '{escaped}' RETURN count(*) LIMIT 1"
    );
    if let Ok(result) = backend.raw_query(&q2) {
        if let Some(row) = result.first() {
            if let Some(val) = row.first() {
                return val.to_string().parse::<u64>().unwrap_or(0) > 0;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message as _;

    /// #166: a store `clear` released must not be reopened over while
    /// something still holds it -- two `Database`s on one file in a process
    /// (#149). The wait ends as soon as the last other holder drops it, and
    /// gives up with an error, rather than opening anyway, when it never does.
    #[test]
    fn a_released_store_still_held_elsewhere_blocks_a_reopen_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::graph::GraphStore::open(&dir.path().join("graph")).unwrap());
        let mut held = HeldPrism::new();
        held.retired = Some(Arc::downgrade(&store));

        let err = held
            .wait_for_retired_store(Duration::from_millis(200))
            .expect_err("a store still held elsewhere must block the reopen");
        assert!(err.to_string().contains("still held by 1"), "{err}");
        assert!(held.retired.is_some(), "a failed wait keeps watching it");

        let holder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(store);
        });
        held.wait_for_retired_store(Duration::from_secs(10))
            .expect("the wait must end once the last holder drops the store");
        assert!(held.retired.is_none());
        holder.join().unwrap();
    }

    /// #166: a jump in the graph's size is logged with what was running, and
    /// ordinary growth or shrinkage is not.
    #[test]
    fn growth_is_noted_only_past_the_threshold_and_names_the_work_in_flight() {
        const MB: u64 = 1024 * 1024;
        let s = Duration::from_secs(10);
        assert_eq!(
            growth_note(100 * MB, 120 * MB, s, &["drain"]),
            None,
            "small growth"
        );
        assert_eq!(
            growth_note(2000 * MB, 70 * MB, s, &[]),
            None,
            "a rebuild shrinks it"
        );

        let line = growth_note(2000 * MB, 2900 * MB, s, &["SCIP import", "idle fold"]).unwrap();
        assert!(line.contains("2000 MB -> 2900 MB (+900 MB)"), "{line}");
        assert!(line.contains("SCIP import, idle fold"), "{line}");

        let quiet = growth_note(0, 100 * MB, s, &[]).unwrap();
        assert!(quiet.contains("nothing tracked in flight"), "{quiet}");
    }

    mod idle_fold {
        use super::super::{settle_idle_fold, ReopenBackoff};
        use crate::graph::store::{FoldError, IdleFold};

        fn failed(why: &str) -> Option<IdleFold> {
            Some(IdleFold::NotFolded(FoldError::Failed(why.into())))
        }
        fn guarded(why: &str) -> Option<IdleFold> {
            Some(IdleFold::NotFolded(FoldError::Guarded(why.into())))
        }

        /// #166: a fold that fails asks for a reopen -- its handle may not
        /// fold again -- and backs off, so the next attempt waits instead of
        /// following two seconds later on the fresh handle. Sittir logged
        /// ~1,600 failing folds at that cadence.
        #[test]
        fn a_failed_fold_reopens_the_graph_and_backs_off() {
            let mut backoff = ReopenBackoff::new();
            let mut last_skip = None;

            let action =
                settle_idle_fold(failed("buffer pool is full"), &mut backoff, &mut last_skip);

            assert!(action.reopen, "a failed handle must be reopened");
            assert!(action
                .log
                .as_deref()
                .is_some_and(|l| l.contains("buffer pool is full")));
            assert!(
                !backoff.should_attempt(),
                "the next fold must wait out the backoff"
            );
            assert_eq!(backoff.consecutive_failures(), 1);
        }

        /// The reopen request is for the probe that failed only. A later probe
        /// that finds nothing due -- a fresh handle, a busy WAL -- must not
        /// reopen a healthy graph just because the failure count is raised.
        #[test]
        fn only_the_failing_probe_asks_for_a_reopen() {
            let mut backoff = ReopenBackoff::new();
            let mut last_skip = None;
            settle_idle_fold(failed("io"), &mut backoff, &mut last_skip);

            for later in [None, Some(IdleFold::NotDue)] {
                let action = settle_idle_fold(later, &mut backoff, &mut last_skip);
                assert!(!action.reopen && action.log.is_none(), "{action:?}");
            }
        }

        /// A guard refusing the same thing on every attempt is logged once,
        /// and never reopens: the handle is fine, the graph or disk is not.
        #[test]
        fn a_repeated_guard_is_logged_once_and_never_reopens() {
            let mut backoff = ReopenBackoff::new();
            let mut last_skip = None;

            let first = settle_idle_fold(guarded("disk full"), &mut backoff, &mut last_skip);
            let second = settle_idle_fold(guarded("disk full"), &mut backoff, &mut last_skip);
            let changed =
                settle_idle_fold(guarded("past the ceiling"), &mut backoff, &mut last_skip);

            assert!(first.log.is_some() && !first.reopen);
            assert!(second.log.is_none() && !second.reopen, "{second:?}");
            assert!(changed.log.is_some(), "a new reason is news");
            assert_eq!(
                backoff.consecutive_failures(),
                3,
                "every refusal still backs off"
            );
        }

        /// Busy is another process's fold or a writer mid-transaction: it
        /// resolves itself, so it neither backs off nor logs. A successful
        /// fold clears the backoff.
        #[test]
        fn busy_is_silent_and_a_successful_fold_resets_the_backoff() {
            let mut backoff = ReopenBackoff::new();
            let mut last_skip = None;
            settle_idle_fold(guarded("disk full"), &mut backoff, &mut last_skip);

            let busy = settle_idle_fold(
                Some(IdleFold::NotFolded(FoldError::Busy(anyhow::anyhow!(
                    "held"
                )))),
                &mut backoff,
                &mut last_skip,
            );
            assert!(busy.log.is_none() && !busy.reopen);
            assert_eq!(backoff.consecutive_failures(), 1, "busy must not back off");

            settle_idle_fold(Some(IdleFold::Folded), &mut backoff, &mut last_skip);
            assert!(backoff.should_attempt());
            assert_eq!(backoff.consecutive_failures(), 0);
            assert!(last_skip.is_none());
        }
    }

    /// A drain runs on a background task, so a panic inside it unwinds on a
    /// thread the watch loop never sees. Nothing else would ever answer the
    /// ad-hoc requests folded into that drain: `execute_drain` writes every
    /// reply as its final step, so a panic before that point leaves the
    /// clients blocking on a `.result` file that no longer has an author.
    /// They'd sit there until their own multi-minute timeout with no
    /// explanation. `finish_drain` is what turns that silence into a real
    /// `WriteResult::Err`.
    #[test]
    fn drain_task_panic_surfaces_as_write_result_err_not_a_hang() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("waiter-a.result");
        let second = tmp.path().join("waiter-b.result");

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .unwrap();

        // The panic is the point of the test, so its default backtrace
        // message would just look like a failure in the log. Suppressed only
        // across the join -- by the time `block_on` returns, the panic has
        // already been caught and reported through the `JoinError`.
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let joined = rt
            .block_on(rt.spawn_blocking(|| -> DrainTaskOutput { panic!("simulated drain panic") }));
        std::panic::set_hook(previous_hook);

        assert!(
            joined.is_err(),
            "test setup is wrong: the drain task did not actually panic"
        );

        let (guard, finish) = finish_drain(joined, &[first.clone(), second.clone()]);
        assert!(
            guard.is_none(),
            "a panicking task drops its index-op guard during the unwind, \
             so there is none left to hand back"
        );
        assert!(
            matches!(finish, DrainFinish::Failed),
            "a panic can unwind out of a half-finished write, so the shared \
             read handle must be treated as suspect"
        );

        for reply_path in [&first, &second] {
            let contents = std::fs::read_to_string(reply_path).unwrap_or_else(|e| {
                panic!(
                    "a panicked drain must still answer every waiter folded into it, \
                     but {} was never written ({e}) -- that client would block until \
                     its own timeout",
                    reply_path.display()
                )
            });
            let reply: crate::daemon_protocol::WriteResult =
                serde_json::from_str(&contents).unwrap();
            match reply {
                crate::daemon_protocol::WriteResult::Err { message } => assert!(
                    message.contains("panic"),
                    "the reply must say the drain panicked, got: {message}"
                ),
                other => panic!("expected WriteResult::Err, got {other:?}"),
            }
        }
    }

    /// Regression test for a review finding on `finish_drain`: an *ordinary*
    /// `execute_drain` failure (not a panic) used to answer every retained
    /// waiter with `WriteResult::Err`, even though `execute_drain` writes
    /// each waiter's reply sequentially as its own last step -- so a
    /// `write_atomic` failure partway through that loop left *earlier*
    /// waiters already holding a correct, real `Ok` reply on disk.
    /// `finish_drain`'s blanket overwrite told those already-succeeded
    /// clients their write had failed, when it had actually succeeded.
    #[test]
    fn finish_drain_does_not_overwrite_a_reply_execute_drain_already_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let mut prism = Infigraph::open(root, crate::lang::LanguageRegistry::new()).unwrap();
        prism.init().unwrap();

        let ok_reply = root.join("waiter-ok.result");
        let fail_dir = root.join("readonly");
        std::fs::create_dir(&fail_dir).unwrap();
        let fail_reply = fail_dir.join("waiter-fail.result");

        let mut queue = crate::daemon::queue::IndexWorkQueue::new();
        queue.add_waiter(crate::daemon::queue::Waiter {
            kind: crate::daemon::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: ok_reply.clone(),
            paths: None,
        });
        queue.add_waiter(crate::daemon::queue::Waiter {
            kind: crate::daemon::queue::WaiterKind::Index,
            use_learned: false,
            reply_path: fail_reply.clone(),
            paths: None,
        });
        let drained = queue.drain();

        // `write_atomic` calls `File::create` in the reply's parent
        // directory -- read-only permissions make that fail for the second
        // waiter only, after the first waiter's reply has already been
        // written successfully.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fail_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
        let drain_result = crate::daemon::drain::execute_drain(&prism, drained);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fail_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let err = match drain_result {
            Err(e) => e,
            Ok(_) => panic!("test setup is wrong: expected execute_drain to fail on fail_reply"),
        };

        // Confirm the partial-success setup: the first waiter really did
        // get a correct Ok reply from execute_drain itself, and the second
        // was never reached.
        let ok_before: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&ok_reply).unwrap()).unwrap();
        assert!(
            matches!(ok_before, crate::daemon_protocol::WriteResult::Ok { .. }),
            "test setup is wrong: execute_drain should have answered the first waiter"
        );
        assert!(
            !fail_reply.exists(),
            "test setup is wrong: execute_drain should not have reached the second waiter"
        );

        // Route that failure through the real recovery path a background
        // drain task uses, and confirm it does not clobber the first
        // waiter's already-correct reply.
        let guard = match begin_index_op(root, "test", Duration::ZERO).unwrap() {
            IndexOpOutcome::Acquired(g) => g,
            IndexOpOutcome::AlreadyRunning(_) => panic!("test setup is wrong: lock contended"),
        };
        let joined: std::result::Result<DrainTaskOutput, tokio::task::JoinError> =
            Ok(DrainTaskOutput {
                guard,
                result: Err(err),
            });
        let (guard, finish) = finish_drain(joined, &[ok_reply.clone(), fail_reply.clone()]);
        assert!(guard.is_some());
        assert!(matches!(finish, DrainFinish::Failed));

        // #161: a *refused* drain is a different animal. See
        // `refused_drain_keeps_the_read_handle` below.

        let ok_after: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&ok_reply).unwrap()).unwrap();
        match ok_after {
            crate::daemon_protocol::WriteResult::Ok { .. } => {}
            other => panic!(
                "finish_drain overwrote the first waiter's already-correct Ok reply with {other:?}"
            ),
        }

        let fail_after: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&fail_reply).unwrap()).unwrap();
        match fail_after {
            crate::daemon_protocol::WriteResult::Err { .. } => {}
            other => {
                panic!("expected the never-answered waiter to get WriteResult::Err, got {other:?}")
            }
        }
    }

    /// Regression test for a final-review Critical finding: `WriteRequest::Index
    /// { paths: Some(paths) }` used to enqueue each path via
    /// `p.to_string_lossy()` without stripping `root` first, so an absolute
    /// path -- an explicitly documented option on `Infigraph::index_file`/
    /// `index_files`, both of which forward the caller's path verbatim into
    /// this same request type -- ended up keyed by its absolute string in
    /// the queue. `extract_paths` then joins that onto `root`, which for an
    /// absolute path is a no-op join that leaves it absolute, so
    /// `FileExtraction.file` (and therefore the resulting graph node's id)
    /// ended up absolute too: a second `File` node alongside the real
    /// relative one, un-deduplicated and never cleaned up by `remove_file`.
    #[test]
    fn route_or_serve_index_request_normalizes_an_absolute_path_to_relative_before_enqueuing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::write(root.join("foo.py"), "def foo():\n    pass\n").unwrap();

        let abs_path = root.join("foo.py");
        let request = crate::daemon_protocol::WriteRequest::Index {
            paths: Some(vec![abs_path]),
        };
        let request_path = root.join("test.request");
        std::fs::write(&request_path, serde_json::to_string(&request).unwrap()).unwrap();

        let queue = Arc::new(Mutex::new(crate::daemon::queue::IndexWorkQueue::new()));
        let mut held = HeldPrism::new();
        let drain_rt = tokio::runtime::Runtime::new().unwrap();
        let daemon_token = CancellationToken::new();
        let registry = Arc::new(crate::lang::LanguageRegistry::new());
        // Never started -- this test exercises request routing only, and a
        // live producer would race it by queueing its own fsevents.
        let mut code_watch = CodeWatch::new(
            &daemon_token,
            producer::ProducerConfig {
                root: root.clone(),
                registry: Arc::clone(&registry),
                debounce_ms: 50,
                ignore_rebuild_secs: 300,
            },
            Arc::clone(&queue),
            Arc::new(|_evt| {}),
        )
        .unwrap();
        route_or_serve_request(
            &root,
            &request_path,
            &queue,
            &registry,
            &|| Ok(crate::lang::LanguageRegistry::new()),
            &mut held,
            &mut ReopenBackoff::new(),
            false,
            false,
            &drain_rt,
            &daemon_token,
            &mut code_watch,
            None,
            &mut false,
            false,
        );

        let drained = queue.lock().unwrap().drain();
        assert_eq!(drained.items.len(), 1, "expected exactly one queued item");
        assert!(
            drained.items.contains_key("foo.py"),
            "an absolute input path must be normalized to a root-relative key \
             before enqueuing, got keys: {:?}",
            drained.items.keys().collect::<Vec<_>>()
        );

        assert_eq!(
            drained.waiters[0].paths,
            Some(vec!["foo.py".to_string()]),
            "the waiter's own scoped paths must also be relative"
        );
    }

    /// Before the `Task<T>`-based background tracking added here,
    /// `WriteRequest::ScipImport` fell through `route_or_serve_request`'s
    /// match statement to its `_ =>` catch-all, which calls
    /// `serve_request_locked` -> `serve_one_request` *synchronously* on the
    /// coordinator's own thread -- blocking the whole tick loop for the
    /// entire SCIP import. This pins the fix: routing a `ScipImport` request
    /// must return `Some(PendingWork::ScipImport(_))` immediately, with the
    /// `.result` reply not yet written, proving the import was handed off to
    /// a background task rather than run inline.
    /// #166: while a failed reopen is backing off, a pending SCIP import must
    /// wait in place like every other request -- it used to call `watch_db`
    /// on every 200ms tick regardless, reopening the graph five times a
    /// second for as long as the open kept failing.
    #[test]
    fn a_scip_import_waits_out_the_reopen_backoff_without_opening_the_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let scip_path = root.join("index.scip");
        std::fs::write(
            &scip_path,
            scip::types::Index::default().write_to_bytes().unwrap(),
        )
        .unwrap();
        let request_path = root.join("test.request");
        std::fs::write(
            &request_path,
            serde_json::to_string(&crate::daemon_protocol::WriteRequest::ScipImport {
                scip_path,
                enriched_ast_generation: None,
            })
            .unwrap(),
        )
        .unwrap();

        let queue = Arc::new(Mutex::new(crate::daemon::queue::IndexWorkQueue::new()));
        let mut held = HeldPrism::new();
        let drain_rt = tokio::runtime::Runtime::new().unwrap();
        let daemon_token = CancellationToken::new();
        let registry = Arc::new(crate::lang::LanguageRegistry::new());
        let mut code_watch = CodeWatch::new(
            &daemon_token,
            producer::ProducerConfig {
                root: root.clone(),
                registry: Arc::clone(&registry),
                debounce_ms: 50,
                ignore_rebuild_secs: 300,
            },
            Arc::clone(&queue),
            Arc::new(|_evt| {}),
        )
        .unwrap();
        let mut backing_off = ReopenBackoff::new();
        backing_off.record_failure();

        let started = route_or_serve_request(
            &root,
            &request_path,
            &queue,
            &registry,
            &|| Ok(crate::lang::LanguageRegistry::new()),
            &mut held,
            &mut backing_off,
            false,
            false,
            &drain_rt,
            &daemon_token,
            &mut code_watch,
            None,
            &mut false,
            false,
        );

        assert!(started.is_none(), "nothing may start while backing off");
        assert!(held.is_none(), "the graph must not have been opened");
        assert!(
            request_path.exists(),
            "the request must wait for a later tick"
        );
        assert!(
            !root.join(".infigraph").join("graph").exists(),
            "no graph file may have been created by a reopen attempt"
        );
    }

    #[test]
    fn route_or_serve_scip_import_request_is_background_tracked_not_served_synchronously() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // An empty-but-valid SCIP index is enough to exercise the routing
        // and completion path -- `import_scip_index`'s per-document handling
        // isn't what this test is about.
        let scip_path = root.join("index.scip");
        let index = scip::types::Index::default();
        std::fs::write(&scip_path, index.write_to_bytes().unwrap()).unwrap();

        let request = crate::daemon_protocol::WriteRequest::ScipImport {
            scip_path: scip_path.clone(),
            enriched_ast_generation: None,
        };
        let request_path = root.join("test.request");
        std::fs::write(&request_path, serde_json::to_string(&request).unwrap()).unwrap();

        let queue = Arc::new(Mutex::new(crate::daemon::queue::IndexWorkQueue::new()));
        let mut held = HeldPrism::new();
        let drain_rt = tokio::runtime::Runtime::new().unwrap();
        let daemon_token = CancellationToken::new();
        let registry = Arc::new(crate::lang::LanguageRegistry::new());
        let mut code_watch = CodeWatch::new(
            &daemon_token,
            producer::ProducerConfig {
                root: root.clone(),
                registry: Arc::clone(&registry),
                debounce_ms: 50,
                ignore_rebuild_secs: 300,
            },
            Arc::clone(&queue),
            Arc::new(|_evt| {}),
        )
        .unwrap();

        let started = route_or_serve_request(
            &root,
            &request_path,
            &queue,
            &registry,
            &|| Ok(crate::lang::LanguageRegistry::new()),
            &mut held,
            &mut ReopenBackoff::new(),
            false,
            false,
            &drain_rt,
            &daemon_token,
            &mut code_watch,
            None,
            &mut false,
            false,
        );

        let pending = match started {
            Some(PendingWork::ScipImport(p)) => p,
            Some(PendingWork::FullReindex(_)) => {
                panic!("expected PendingWork::ScipImport, got PendingWork::FullReindex")
            }
            None => panic!(
                "expected Some(PendingWork::ScipImport(_)) -- a None here means the request \
                 fell through to the synchronous serve_request_locked path again"
            ),
        };

        // The reply must not exist yet: if the import had run synchronously
        // on this thread, `serve_one_request` would have already written it
        // before `route_or_serve_request` returned.
        assert!(
            !pending.reply_path.exists(),
            "reply was already written -- ScipImport was served synchronously, not backgrounded"
        );

        let joined = drain_rt.block_on(pending.task.join());
        let (guard, _touched_files) = finish_scip_import(&root, &pending.reply_path, &held, joined);
        assert!(
            guard.is_some(),
            "expected the index-op guard back on a successful import"
        );
        assert!(
            pending.reply_path.exists(),
            "finish_scip_import must write the .result reply"
        );
        let reply: crate::daemon_protocol::WriteResult =
            serde_json::from_str(&std::fs::read_to_string(&pending.reply_path).unwrap()).unwrap();
        assert!(
            matches!(reply, crate::daemon_protocol::WriteResult::ScipImportOk(_)),
            "expected WriteResult::ScipImportOk, got {reply:?}"
        );
    }

    // --- SCIP staleness auto-re-enrichment: the pure decision ---

    #[test]
    fn scip_enrichment_due_when_gap_reaches_threshold() {
        assert!(scip_enrichment_due(60, 10, None, 50));
        assert!(!scip_enrichment_due(59, 10, None, 50), "gap 49 < 50");
    }

    #[test]
    fn scip_enrichment_never_due_when_threshold_is_zero() {
        assert!(!scip_enrichment_due(1_000, 1, None, 0));
    }

    #[test]
    fn scip_enrichment_never_due_for_a_never_enriched_graph() {
        // scip_generation == 0 means SCIP has never run here (doctor's
        // R3.3.4 rule) -- the daemon must not start running external
        // indexers on a project that never opted into them.
        assert!(!scip_enrichment_due(1_000, 0, None, 50));
    }

    #[test]
    fn scip_enrichment_not_retried_until_a_full_threshold_of_writes_since_the_last_attempt() {
        // An attempt was already made at ast_generation 60 and SCIP did not
        // catch up (indexers missing, import failed). A single write is not
        // grounds to re-run minutes of external indexers -- the gap must
        // have grown by another `threshold` since that attempt. Otherwise a
        // project whose indexer is broken re-runs it every check interval
        // for as long as the user keeps editing.
        assert!(!scip_enrichment_due(60, 10, Some(60), 50));
        assert!(!scip_enrichment_due(61, 10, Some(60), 50));
        assert!(!scip_enrichment_due(109, 10, Some(60), 50));
        assert!(scip_enrichment_due(110, 10, Some(60), 50));
    }

    #[test]
    fn scip_settings_defaults() {
        let s = scip_settings(tempfile::tempdir().unwrap().path());
        assert_eq!(s.index_staleness_threshold, 50);
        assert_eq!(s.index_staleness_check_secs, 300);
    }
}

#[cfg(test)]
mod watchable_root_tests {
    use super::{ensure_watchable_root, ALLOW_CONTAINER_ROOT_ENV};

    fn project(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("Cargo.toml"), "[package]\n").unwrap();
        p
    }

    /// The case this exists for: a stale instance was found rooted at
    /// `~/GitHub.nosync`, 57 sibling repositories, which it would have
    /// indexed as one project.
    #[test]
    fn a_directory_of_several_projects_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        project(tmp.path(), "repo-a");
        project(tmp.path(), "repo-b");
        let err = ensure_watchable_root(tmp.path())
            .expect_err("a folder holding several projects must not be watched as one");
        let msg = err.to_string();
        assert!(msg.contains("repo-a") && msg.contains("repo-b"), "{msg}");
        assert!(
            msg.contains(ALLOW_CONTAINER_ROOT_ENV),
            "must name the override: {msg}"
        );
    }

    /// A git WORKTREE's `.git` is a file, not a directory. This repo indexes
    /// its own `scratchpad/wt-*` worktrees, so a naive `is_dir` test on
    /// `.git` would refuse roots that work today.
    #[test]
    fn a_worktree_whose_dot_git_is_a_file_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".git"),
            "gitdir: /elsewhere/.git/worktrees/wt",
        )
        .unwrap();
        project(tmp.path(), "child-a");
        project(tmp.path(), "child-b");
        ensure_watchable_root(tmp.path())
            .expect("a worktree is a project even though its .git is a file");
    }

    /// A subdirectory of a repo has no VCS marker at all and is still an
    /// ordinary root -- `crates/infigraph-mcp` in this very workspace is
    /// indexed that way.
    #[test]
    fn a_repo_subdirectory_with_only_a_manifest_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\n").unwrap();
        project(tmp.path(), "sub-a");
        project(tmp.path(), "sub-b");
        ensure_watchable_root(tmp.path()).expect("a manifest makes this a project");
    }

    /// One project inside a plain directory is an ordinary checkout layout,
    /// not the container mistake -- refusing it would be surprising.
    #[test]
    fn a_directory_holding_a_single_project_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        project(tmp.path(), "only-repo");
        ensure_watchable_root(tmp.path()).expect("one child is not a container");
    }

    #[test]
    fn an_empty_directory_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_watchable_root(tmp.path()).expect("nothing to conflate");
    }
}

/// #161: the daemon poisons its shared read handle on drain failure, which
/// is right for a failure that could have left it stale and wrong for a
/// refusal.
///
/// Every `refusing to index --` guard declines *before* the write reaches
/// the graph, so the handle is exactly as valid as it was. Treating that as
/// a failure is what turned a too-large graph from un-writable into
/// unreadable: each refused drain dropped the handle, and only a
/// *successful* drain restores one, which the breaker that refused it
/// guarantees will never arrive. sittir logged 15 refusals and answered
/// every read with "the daemon has no graph open yet", advising a retry
/// that could not help.
#[cfg(test)]
mod refused_drain_tests {
    use super::*;
    use crate::ops::{begin_index_op, IndexOpOutcome};
    use std::time::Duration;

    fn drain_err(root: &std::path::Path, err: anyhow::Error) -> DrainFinish {
        let guard = match begin_index_op(root, "test", Duration::ZERO).unwrap() {
            IndexOpOutcome::Acquired(g) => g,
            IndexOpOutcome::AlreadyRunning(_) => panic!("test setup is wrong: lock contended"),
        };
        let joined: std::result::Result<DrainTaskOutput, tokio::task::JoinError> =
            Ok(DrainTaskOutput {
                guard,
                result: Err(err),
            });
        finish_drain(joined, &[]).1
    }

    #[test]
    fn a_refused_drain_keeps_the_read_handle() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();

        let refusal = anyhow::anyhow!(
            "{}graph at /x/graph is 2019 MB, 31x its recorded healthy size (63 MB)",
            crate::graph::growth_gate::WRITE_REFUSED_PREFIX
        );
        assert!(
            matches!(drain_err(tmp.path(), refusal), DrainFinish::Refused),
            "a preflight refusal never touched the graph, so the read handle \
             must survive it"
        );
    }

    #[test]
    fn an_ordinary_drain_failure_still_poisons() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".infigraph")).unwrap();

        let failure =
            anyhow::anyhow!("Query execution failed: Invalid transaction type to rollback.");
        assert!(
            matches!(drain_err(tmp.path(), failure), DrainFinish::Failed),
            "the distinction must stay narrow -- poisoning exists for a graph \
             replaced under a live connection, and that case must keep working"
        );
    }
}

#[cfg(test)]
mod root_identity_tests {
    use super::*;

    #[test]
    fn an_untouched_root_is_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let started_on = directory_identity(&root);
        assert!(!root_is_gone(&root, started_on));
    }

    #[test]
    fn a_deleted_root_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let started_on = directory_identity(&root);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(root_is_gone(&root, started_on));
    }

    /// The #136 shape: the root vanishes and something (a `create_dir_all`
    /// under `.infigraph/`) puts a new directory back at the same path.
    #[cfg(unix)]
    #[test]
    fn a_deleted_and_recreated_root_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        let started_on = directory_identity(&root);
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        assert!(
            root.exists(),
            "the recreated root exists -- exists() alone would keep the daemon alive"
        );
        // If this ever fails, the message says why rather than just that it
        // did: the two ways the identity can come up equal are the inode
        // being recycled (Linux does this) and the filesystem reporting no
        // birth time to break the tie.
        let started = started_on.expect("identity captured while the root existed");
        let now = directory_identity(&root).expect("the recreated root exists");
        assert!(
            root_is_gone(&root, started_on),
            "a recreated root was not detected as gone.\n               started: {started:?}\n  now:     {now:?}\n               inode recycled: {}\n  birth time available: {}",
            started.1 == now.1,
            started.2.is_some() && now.2.is_some(),
        );
    }

    #[test]
    fn without_a_captured_identity_it_degrades_to_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir(&root).unwrap();
        assert!(!root_is_gone(&root, None));
        std::fs::remove_dir_all(&root).unwrap();
        assert!(root_is_gone(&root, None));
    }
}

/// #148: the full-reindex swap is the last quarantine site that used to
/// infer a corruption verdict from a single failed open in a single process
/// -- the defect class fixed for the read path in 5818aa1 and for
/// `Infigraph::init` in d511a0b. The probe is injected here for the reason
/// [`crate::probe`] documents: in a test binary `current_exe()` is libtest's
/// harness, so the real probe re-runs the whole suite instead of probing.
#[cfg(test)]
mod swapped_in_graph_reopen_tests {
    use super::*;

    /// `.infigraph/` holding a freshly swapped-in `graph` and the prior live
    /// graph already retired to the `previous` pool -- the exact on-disk
    /// state `finish_full_reindex` is in when its reopen fails.
    fn after_a_swap() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let infigraph_dir = dir.path().join(".infigraph");
        std::fs::create_dir_all(&infigraph_dir).unwrap();
        let live = infigraph_dir.join("graph");
        std::fs::write(&live, b"the freshly rebuilt graph").unwrap();
        let retired = infigraph_dir.join("graph.previous.1");
        std::fs::write(&retired, b"the stale prior graph").unwrap();
        (dir, infigraph_dir, live, retired)
    }

    fn corrupt_pool_entries(infigraph_dir: &Path) -> Vec<String> {
        std::fs::read_dir(infigraph_dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.starts_with("graph.corrupt."))
            .collect()
    }

    #[test]
    fn a_swapped_in_graph_a_fresh_process_can_open_is_not_quarantined() {
        let (_dir, infigraph_dir, live, retired) = after_a_swap();

        let outcome = recover_or_quarantine_swapped_in_graph_with(
            &infigraph_dir,
            "graph",
            &live,
            &Some(retired.clone()),
            |_| true,
        );

        assert!(
            matches!(outcome, SwapReopen::GraphIsHealthy),
            "a graph a fresh process opens is not why our own reopen failed, got {outcome:?}"
        );
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"the freshly rebuilt graph",
            "the rebuild must still be live -- it was never shown to be bad"
        );
        assert!(
            corrupt_pool_entries(&infigraph_dir).is_empty(),
            "a healthy graph must never be filed as corruption evidence"
        );
        assert!(
            retired.exists(),
            "the rollback candidate must be left where it is"
        );
    }

    #[test]
    fn a_swapped_in_graph_no_process_can_open_is_quarantined_and_rolled_back() {
        let (_dir, infigraph_dir, live, retired) = after_a_swap();

        let outcome = recover_or_quarantine_swapped_in_graph_with(
            &infigraph_dir,
            "graph",
            &live,
            &Some(retired.clone()),
            |_| false,
        );

        match outcome {
            SwapReopen::Quarantined { rollback_note } => assert!(
                rollback_note.contains("restored"),
                "unexpected rollback note: {rollback_note}"
            ),
            other => panic!("expected a quarantine verdict, got {other:?}"),
        }
        assert_eq!(
            corrupt_pool_entries(&infigraph_dir).len(),
            1,
            "the unopenable graph belongs in the corruption-evidence pool"
        );
        assert!(!retired.exists(), "the retired graph was rolled back");
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"the stale prior graph",
            "the prior live graph is back in place"
        );
    }
}
