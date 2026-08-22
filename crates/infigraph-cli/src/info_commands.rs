use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use infigraph_core::daemon_protocol::{WatchAction, WatchRole};
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use serde_json::json;

pub(crate) fn cmd_stats(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let stats = prism.stats()?;
    println!("{}", stats);
    Ok(())
}

pub(crate) fn cmd_languages(project_root: Option<&Path>) -> Result<()> {
    let registry = crate::full_registry(project_root)?;
    println!("Available languages:");
    for pack in registry.languages() {
        let backend = match &pack.backend {
            infigraph_core::lang::ParserBackend::TreeSitter { .. } => "tree-sitter",
            infigraph_core::lang::ParserBackend::Custom(_) => "grammar-plugin",
        };
        println!(
            "  {} ({}) [{}]",
            pack.name,
            pack.extensions.join(", "),
            backend
        );
    }
    Ok(())
}

pub(crate) fn cmd_symbols(root: &Path, file: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let symbols = backend.symbols_in_file(file)?;
    if symbols.is_empty() {
        println!(
            "No symbols found for '{}'. Run 'infigraph index' first.",
            file
        );
        return Ok(());
    }

    println!("Symbols in {}:", file);
    for s in &symbols {
        println!(
            "  {:>8} {:30} L{}-{}",
            s.kind, s.name, s.start_line, s.end_line
        );
    }
    Ok(())
}

pub(crate) fn cmd_skeleton(root: &Path, file: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let result = backend.skeleton(file)?;
    print!("{}", result);
    Ok(())
}

pub(crate) fn cmd_ingest(
    root: &Path,
    schema_id: Option<&str>,
    data_file: Option<&str>,
    source_dir: Option<&str>,
) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let schemas = infigraph_core::structured::discover_schemas(root)?;

    if schemas.is_empty() {
        println!("No structured schemas found.");
        println!("Create .toml schema files in .infigraph/structured-schemas/ or ~/.infigraph/structured-schemas/");
        return Ok(());
    }

    let sid = match schema_id {
        Some(id) => id,
        None => {
            println!("Available schemas:\n");
            for (path, schema) in &schemas {
                println!(
                    "  {} — {} (table: {}, {} columns, {} edges)\n    Source: {}\n",
                    schema.schema.schema_id,
                    schema.schema.name,
                    schema.schema.node_table,
                    schema.schema.columns.len(),
                    schema.schema.edges.len(),
                    path.display(),
                );
            }
            return Ok(());
        }
    };

    let (_, schema) = schemas
        .iter()
        .find(|(_, s)| s.schema.schema_id == sid)
        .context(format!("schema '{}' not found", sid))?;

    let backend = prism.backend().context("graph not initialized")?;

    if let Some(dir) = source_dir {
        let result =
            backend.ingest_structured_directory(&schema.schema, std::path::Path::new(dir))?;
        println!(
            "Ingested directory '{}' using schema '{}': {} nodes, {} edges",
            dir, sid, result.nodes_created, result.edges_created
        );
    } else {
        let file =
            data_file.context("--data-file or --source required when --schema is specified")?;
        let result = backend.ingest_structured_file(&schema.schema, std::path::Path::new(file))?;
        println!(
            "Ingested '{}' using schema '{}': {} nodes, {} edges",
            file, sid, result.nodes_created, result.edges_created
        );
    }
    Ok(())
}

pub(crate) fn cmd_index_manifests(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let backend = prism.backend().context("graph not initialized")?;
    let results = infigraph_core::manifest::index_manifests(root, backend)?;
    if results.is_empty() {
        println!("No manifests found.");
        return Ok(());
    }
    let total: usize = results.iter().map(|r| r.deps.len()).sum();
    println!(
        "Indexed {} manifests, {} dependencies:\n",
        results.len(),
        total
    );
    for r in &results {
        println!(
            "  {} [{}]: {} deps",
            r.manifest_file,
            r.ecosystem,
            r.deps.len()
        );
    }

    // Create LINKS_TO edges from manifests to indexed docs via doc_urls
    if let Ok(mut doc_idx) = infigraph_docs::DocIndex::open(root) {
        if doc_idx.init().is_ok() {
            if let Some(doc_store) = doc_idx.store() {
                let all_doc_ids: std::collections::HashSet<String> = doc_store
                    .get_doc_hashes()
                    .unwrap_or_default()
                    .keys()
                    .cloned()
                    .collect();
                for r in &results {
                    if !r.doc_urls.is_empty() {
                        infigraph_docs::links::link_manifest_doc_urls(
                            doc_store,
                            &r.manifest_file,
                            &r.doc_urls,
                            &all_doc_ids,
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

pub(crate) fn cmd_dependencies(root: &Path, ecosystem: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let backend = prism.backend().context("graph not initialized")?;
    let mut deps = infigraph_core::manifest::query_deps(backend)?;
    if let Some(eco) = ecosystem {
        deps.retain(|d| d.ecosystem == eco);
    }
    if deps.is_empty() {
        println!("No dependencies found. Run 'infigraph index-manifests' first.");
        return Ok(());
    }
    println!("Dependencies ({}):\n", deps.len());
    let mut cur_eco = String::new();
    for d in &deps {
        if d.ecosystem != cur_eco {
            println!("  [{}]", d.ecosystem);
            cur_eco = d.ecosystem.clone();
        }
        let dev_tag = if d.is_dev { " (dev)" } else { "" };
        println!("    {}@{}{}", d.name, d.version, dev_tag);
    }
    Ok(())
}

pub(crate) fn cmd_api_surface(root: &Path, file_filter: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let backend = prism.backend().context("graph not initialized")?;
    let mut syms = backend.get_api_surface()?;
    if let Some(f) = file_filter {
        syms.retain(|s| s.file.contains(f));
    }

    println!("API Surface ({} symbols):\n", syms.len());
    let mut cur_file = String::new();
    for s in &syms {
        if s.file != cur_file {
            println!("  {}", s.file);
            cur_file = s.file.clone();
        }
        println!("    [{:<10}] L{:<5} {}", s.kind, s.line, s.name);
    }
    Ok(())
}

pub(crate) fn cmd_file_deps(root: &Path, file: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let backend = prism.backend().context("graph not initialized")?;
    let deps = backend.get_file_deps(file)?;
    println!("File dependencies for '{}':\n", file);
    println!("  Imports ({}):", deps.imports.len());
    for f in &deps.imports {
        println!("    → {}", f);
    }
    if deps.imports.is_empty() {
        println!("    (none)");
    }
    println!("\n  Imported by ({}):", deps.imported_by.len());
    for f in &deps.imported_by {
        println!("    ← {}", f);
    }
    if deps.imported_by.is_empty() {
        println!("    (none)");
    }
    Ok(())
}

pub(crate) fn cmd_type_hierarchy(root: &Path, symbol: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let backend = prism.backend().context("graph not initialized")?;
    let hier = backend.get_type_hierarchy(symbol, depth)?;
    println!("Type hierarchy for '{}':\n", hier.root_name);
    println!("  Ancestors ({}):", hier.ancestors.len());
    for a in &hier.ancestors {
        println!("    ↑ {} [{}]  ({})", a.name, a.kind, a.file);
    }
    if hier.ancestors.is_empty() {
        println!("    (none — root type)");
    }
    println!("\n  Descendants ({}):", hier.descendants.len());
    for d in &hier.descendants {
        println!("    ↓ {} [{}]  ({})", d.name, d.kind, d.file);
    }
    if hier.descendants.is_empty() {
        println!("    (none — leaf type)");
    }
    Ok(())
}

pub(crate) fn cmd_test_coverage(root: &Path, file_filter: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;
    let backend = prism.backend().context("graph not initialized")?;
    let mut cov = backend.get_test_coverage()?;
    if let Some(f) = file_filter {
        cov.covered.retain(|s| s.file.contains(f));
        cov.uncovered.retain(|s| s.file.contains(f));
        let total = cov.covered.len() + cov.uncovered.len();
        cov.coverage_pct = (cov.covered.len() * 100).checked_div(total).unwrap_or(0);
        cov.covered_count = cov.covered.len();
        cov.uncovered_count = cov.uncovered.len();
    }

    println!(
        "Test Coverage: {}%  ({} covered / {} uncovered)\n",
        cov.coverage_pct, cov.covered_count, cov.uncovered_count
    );

    if !cov.uncovered.is_empty() {
        println!("Uncovered ({}):", cov.uncovered.len());
        for s in cov.uncovered.iter().take(50) {
            println!("  ✗  {:<40} [{}]  {}", s.symbol_name, s.kind, s.file);
        }
        if cov.uncovered.len() > 50 {
            println!("  ... and {} more", cov.uncovered.len() - 50);
        }
    }
    Ok(())
}

/// R5.4 (#79) daemon shutdown watchdog: the ordinary (nothing in flight)
/// grace period before considering a hard exit.
const SHUTDOWN_WATCHDOG_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Backstop ceiling for how long the watchdog will keep deferring while
/// `index.lock` is genuinely still held by this process -- long enough for
/// a real full reindex ("can take minutes"), short enough to still catch a
/// truly stuck write eventually.
const SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING: std::time::Duration =
    std::time::Duration::from_secs(600);

/// Whether the daemon shutdown watchdog should keep deferring its hard
/// exit rather than kill the process now. Split out from the `ctrlc`
/// handler so this budget-vs-in-flight-work decision is directly testable
/// with synthetic durations, without spawning a real daemon and waiting
/// out real timers.
///
/// True for the first `grace` (the original, unconditional R5.4 budget --
/// covers the ordinary case where nothing is in flight and the loop is
/// either already exiting or genuinely wedged for some other reason).
/// After that, true only while `index_op_in_progress` is still reported,
/// up to `in_progress_ceiling` -- confirmed real write work in progress is
/// not evidence of a wedge, and killing through it is exactly what leaves
/// an unreplayed WAL behind (github.com/pradeepmouli/infigraph#92).
fn watchdog_should_defer(
    elapsed_since_signal: std::time::Duration,
    grace: std::time::Duration,
    in_progress_ceiling: std::time::Duration,
    index_op_in_progress: bool,
) -> bool {
    if elapsed_since_signal < grace {
        return true;
    }
    index_op_in_progress && elapsed_since_signal < in_progress_ceiling
}

pub(crate) fn cmd_daemon(root: &Path, debounce: u64) -> Result<()> {
    // Belt-and-braces: spawn_daemon (watch/daemon.rs) already strips
    // INFIGRAPH_BACKEND when it spawns this process normally. This
    // covers the case where someone runs `infigraph daemon` directly
    // from a shell that happens to have INFIGRAPH_BACKEND=daemon set --
    // without this, the daemon's own Infigraph::open (reached via
    // watch_project -> open_transient) would select DaemonKuzu on
    // itself and deadlock waiting on a request nothing serves.
    std::env::remove_var("INFIGRAPH_BACKEND");

    if infigraph_core::watch::daemon::is_remote_backend() {
        println!(
            "File watching is not supported in remote mode (Neo4j backend). \
             Reindexing is triggered via webhooks instead."
        );
        return Ok(());
    }
    // Hold exclusive lock for lifetime — signals liveness to ensure_watcher_running.
    let lock_path = root.join(".infigraph").join("watch.lock");
    let _lock = acquire_watch_lock(&lock_path)?;

    println!(
        "Watching {} (debounce {}ms) — Ctrl-C to stop",
        root.display(),
        debounce
    );

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    // The daemon-lifetime token every cancellable task in this process hangs
    // off: the code-watch producer (spawned inside run_write_coordinator),
    // full-reindex builds, and SCIP enrichment. Cancelling it tears all of
    // them down; the coordinator's own loop exits on `stop_rx` instead, which
    // the handler below sends alongside the cancel.
    let daemon_token = tokio_util::sync::CancellationToken::new();

    let watchdog_root = root.to_path_buf();
    let watchdog_token = daemon_token.clone();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
        watchdog_token.cancel();
        // R5.4 (#79): bound the graceful path. The watch loop's shutdown
        // waits out in-flight drains; if one is wedged (stuck on a lock or
        // a hung query), that wait never ends and the daemon becomes an
        // unkillable-by-SIGTERM orphan -- exactly what graceful shutdown
        // exists to prevent.
        //
        // The original version of this watchdog used a flat 5s timer, which
        // conflated "wedged" with "still doing a legitimately long write" --
        // a full reindex can take minutes (see run_write_coordinator's
        // own shutdown comment), and killing straight through one corrupts
        // the graph exactly like any other unclean shutdown (an unreplayed
        // WAL left behind for the next opener to trip over -- see
        // github.com/pradeepmouli/infigraph#92; this is exactly how that
        // hazard occurs, not just a theoretical risk). `index.lock` held by
        // this same process is the same signal the watch loop's own
        // shutdown path already waits on, so use it here too: give the
        // ordinary (nothing in flight) case its original 5s budget, but
        // defer the hard exit for as long as real write work is confirmed
        // still in progress, up to a much longer ceiling that remains as a
        // backstop against a genuinely stuck write.
        let watchdog_root = watchdog_root.clone();
        std::thread::spawn(move || {
            const POLL: std::time::Duration = std::time::Duration::from_millis(500);

            let start = std::time::Instant::now();
            while watchdog_should_defer(
                start.elapsed(),
                SHUTDOWN_WATCHDOG_GRACE,
                SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
                infigraph_core::ops::index_op_held_by_self(&watchdog_root),
            ) {
                std::thread::sleep(POLL);
            }

            eprintln!("[daemon] graceful shutdown exceeded its budget -- hard exit");
            std::process::exit(1);
        });
    })
    .ok();

    let doc_watch = std::sync::Arc::new(std::sync::Mutex::new(DocWatchThread::new(
        root.to_path_buf(),
        debounce,
    )));
    doc_watch.lock().unwrap().start();

    // Lets `WatchControl { role: Docs, .. }` requests reach this thread from
    // the coordinator, which lives in infigraph-core and knows nothing about
    // doc-watching. Doc-watching deliberately stays on its existing
    // thread + `Arc<AtomicBool>` shape here: only its external control
    // surface is unified in this pass, not its internals (those live in
    // infigraph-docs).
    let doc_watch_for_control = std::sync::Arc::clone(&doc_watch);
    let docs_control: std::sync::Arc<infigraph_core::watch::DocsControl> =
        std::sync::Arc::new(move |action| {
            use infigraph_core::daemon_protocol::WatchAction;
            let mut doc_watch = doc_watch_for_control.lock().unwrap();
            match action {
                WatchAction::Stop | WatchAction::Disable => doc_watch.stop(),
                WatchAction::Start | WatchAction::Enable => doc_watch.start(),
                WatchAction::Restart => {
                    doc_watch.stop();
                    doc_watch.start();
                }
            }
            Ok(())
        });

    let on_full_reindex: std::sync::Arc<infigraph_core::watch::FullReindexCallback> =
        std::sync::Arc::new(
            move |prism: std::sync::Arc<infigraph_core::Infigraph>,
                  detected_languages: Vec<String>,
                  token: tokio_util::sync::CancellationToken| {
                let languages: std::collections::HashSet<String> =
                    detected_languages.into_iter().collect();
                let root = prism.root().to_path_buf();
                // Part A (running the external indexer binaries) is
                // deliberately unlocked -- it can take several minutes on a
                // real multi-language repo and touches nothing in the graph.
                // Only the import step below needs `index.lock`. `token` is
                // this callback's own child of `daemon_token`, checked
                // between each indexer launch so a daemon shutdown stops
                // starting further indexers without discarding results from
                // ones that already finished.
                let results = crate::index::run_scip_indexers(&root, &languages, &token);
                if results.is_empty() {
                    return;
                }
                match infigraph_core::ops::begin_index_op(
                    &root,
                    "infigraph daemon (scip import)",
                    std::time::Duration::from_secs(30),
                ) {
                    Ok(infigraph_core::ops::IndexOpOutcome::Acquired(guard)) => {
                        crate::index::import_scip_results_and_embed(&root, &prism, &results);
                        drop(guard);
                    }
                    Ok(o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_)) => {
                        eprintln!(
                            "[daemon] scip-import busy ({}), skipping this round",
                            o.skip_note().unwrap_or_default()
                        );
                    }
                    Err(e) => {
                        eprintln!("[daemon] scip-import busy ({e}), skipping this round");
                    }
                }
            },
        );

    let coordinator = infigraph_core::watch::run_write_coordinator(
        root,
        bundled_registry,
        debounce,
        stop_rx,
        |evt| {
            println!("[watch] {evt}");
        },
        0,
        None::<fn(&infigraph_core::IndexResult)>,
        true,
        Some(on_full_reindex),
        &daemon_token,
        Some(docs_control),
    );

    // Unconditional, so the paths that leave the loop without going through
    // the token (stop_rx, the watch.stop sentinel, a vanished root) still
    // tear down everything hanging off it.
    daemon_token.cancel();
    doc_watch.lock().unwrap().stop();
    coordinator?;

    println!("Watch stopped.");
    Ok(())
}

/// The daemon's doc-watch thread and the shutdown flag it polls, bundled so
/// a `WatchControl { role: Docs, .. }` request can stop and restart it. Each
/// start gets a *fresh* flag: `watch_docs_daemon_loop` only ever reads it,
/// and a reused one would still be latched at `true` from the last stop.
struct DocWatchThread {
    root: std::path::PathBuf,
    debounce: u64,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DocWatchThread {
    fn new(root: std::path::PathBuf, debounce: u64) -> Self {
        DocWatchThread {
            root,
            debounce,
            shutdown: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handle: None,
        }
    }

    fn start(&mut self) {
        if self.handle.is_some() {
            return;
        }
        self.shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let root = self.root.clone();
        let debounce = self.debounce;
        let shutdown = std::sync::Arc::clone(&self.shutdown);
        self.handle = Some(std::thread::spawn(move || {
            if let Err(e) = infigraph_docs::watch::watch_docs_daemon_loop(&root, debounce, shutdown)
            {
                eprintln!("[doc-watch-daemon] error: {e}");
            }
        }));
    }

    fn stop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) fn cmd_watch_stop(root: &Path) -> Result<()> {
    let sentinel = root.join(".infigraph").join("watch.stop");
    let lock_path = root.join(".infigraph").join("watch.lock");

    if !infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        println!("No watcher running.");
        return Ok(());
    }

    std::fs::write(&sentinel, b"")?;
    println!("Stop signal sent. Watcher will exit within ~1 second.");
    Ok(())
}

pub(crate) fn cmd_watch_status(root: &Path) -> Result<()> {
    let lock_path = root.join(".infigraph").join("watch.lock");

    if infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        println!("Watcher is running.");
    } else {
        println!("No watcher running.");
    }
    Ok(())
}

/// How long the CLI waits for a running daemon to reply to a `WatchControl`
/// request before giving up. The request/reply protocol's round trip is
/// normally sub-second; this is generous headroom for a daemon that's
/// mid-write on something else when the request lands.
const WATCH_CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(crate) fn cmd_daemon_stop(root: &Path) -> Result<()> {
    // Without this check, submitting into an unattended staging dir just
    // sits there until WATCH_CONTROL_TIMEOUT and then reports an opaque
    // protocol error -- an everyday case (no daemon running yet), not a
    // misconfiguration. Mirrors cmd_watch_stop's existing early-exit and
    // index.rs's FullReindex path (see its own comment re: incident #100).
    let lock_path = root.join(".infigraph").join("watch.lock");
    if !infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        println!("No daemon running.");
        return Ok(());
    }
    let registry = bundled_registry()?;
    let prism = Infigraph::open(root, registry)?;
    prism.submit_watch_control_and_await(
        WatchRole::Daemon,
        WatchAction::Stop,
        WATCH_CONTROL_TIMEOUT,
    )?;
    println!("Daemon stopped.");
    Ok(())
}

pub(crate) fn cmd_daemon_restart(root: &Path) -> Result<()> {
    // Same liveness check as cmd_daemon_stop -- if nothing is running,
    // there is nothing to stop-and-wait-for, so skip straight to spawning.
    let lock_path = root.join(".infigraph").join("watch.lock");
    if infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        let registry = bundled_registry()?;
        let prism = Infigraph::open(root, registry)?;
        // `WatchRole::Daemon`'s `Restart` action (per Task 10's
        // route_or_serve_request arm) only cancels daemon_token -- the
        // process exiting means there's nothing left to ask to "start
        // itself" from inside. Re-spawn from the CLI side instead,
        // mirroring `ensure_daemon_running`'s existing pattern.
        prism.submit_watch_control_and_await(
            WatchRole::Daemon,
            WatchAction::Stop,
            WATCH_CONTROL_TIMEOUT,
        )?;

        // Wait for the process to actually exit before respawning (poll
        // watch.lock's liveness, matching wait_for_daemon_ready's shape in
        // crates/infigraph-core/src/watch/daemon.rs).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
            if std::time::Instant::now() > deadline {
                anyhow::bail!("daemon did not exit within 10s of a stop request");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    let watch_binary = std::env::current_exe()?;
    match infigraph_core::watch::daemon::ensure_daemon_running_required(root, &watch_binary) {
        infigraph_core::watch::daemon::DaemonStartOutcome::Spawned => {
            println!("Daemon restarted.");
            Ok(())
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::AlreadyRunning => {
            println!("Daemon already running (unexpected after a confirmed stop).");
            Ok(())
        }
        infigraph_core::watch::daemon::DaemonStartOutcome::Failed(e) => {
            anyhow::bail!("failed to restart daemon: {e}")
        }
    }
}

// TODO(Task 14): replace with the real infigraph-core version in
// crates/infigraph-core/src/watch/config.rs.
fn write_watch_policy_to_config(_root: &Path, _role: WatchRole, _enabled: bool) -> Result<()> {
    Ok(())
}

pub(crate) fn watch_cli_action_to_watch_action(action: &crate::WatchCliAction) -> WatchAction {
    match action {
        crate::WatchCliAction::Enable => WatchAction::Enable,
        crate::WatchCliAction::Disable => WatchAction::Disable,
        crate::WatchCliAction::Start => WatchAction::Start,
        crate::WatchCliAction::Stop => WatchAction::Stop,
        crate::WatchCliAction::Restart => WatchAction::Restart,
    }
}

pub(crate) fn cmd_watch_control(
    root: &Path,
    role: WatchRole,
    action: crate::WatchCliAction,
) -> Result<()> {
    let watch_action = watch_cli_action_to_watch_action(&action);
    if matches!(watch_action, WatchAction::Enable | WatchAction::Disable) {
        // Persisted regardless of whether a daemon is currently running --
        // the policy is meant to survive restarts, so it must apply the
        // next time one starts even if none is up right now.
        write_watch_policy_to_config(root, role, watch_action == WatchAction::Enable)?;
    }

    // Same liveness check as cmd_daemon_stop/cmd_daemon_restart -- avoids a
    // WATCH_CONTROL_TIMEOUT hang on the everyday "no daemon running" case.
    // Enable/Disable already did their durable work above; Start/Stop/
    // Restart have nothing to act on without a live daemon.
    let lock_path = root.join(".infigraph").join("watch.lock");
    if !infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        if matches!(watch_action, WatchAction::Enable | WatchAction::Disable) {
            println!("{role:?}: policy persisted; no daemon currently running to notify.");
        } else {
            println!("No daemon running.");
        }
        return Ok(());
    }

    let registry = bundled_registry()?;
    let prism = Infigraph::open(root, registry)?;
    prism.submit_watch_control_and_await(role, watch_action, WATCH_CONTROL_TIMEOUT)?;
    println!("{role:?}: {watch_action:?} sent.");
    Ok(())
}

pub(crate) fn acquire_watch_lock(lock_path: &Path) -> Result<infigraph_core::lockfile::LockFile> {
    infigraph_core::lockfile::try_acquire(lock_path, "cli-watch")?.ok_or_else(|| {
        // Name the actual holder (#100 item 4's confusion: "another watcher
        // is already running" fired right after an auto-watch had spawned
        // one -- factually true, but unexplained). read_holder is
        // best-effort: a mid-write payload still yields the generic form.
        match infigraph_core::lockfile::read_holder(lock_path) {
            Some(h) => anyhow::anyhow!(
                "another watcher is already running: {} (PID {}) -- often the auto-watch \
                 spawned by a recent infigraph command; `infigraph ps` lists it, \
                 `infigraph watch-stop` stops it",
                h.role,
                h.pid
            ),
            None => anyhow::anyhow!("another watcher is already running"),
        }
    })
}

pub(crate) fn cmd_scip_import(root: &Path, index_path: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let backend = prism.backend().context("graph not initialized")?;
    let abs_index = if index_path.is_absolute() {
        index_path.to_path_buf()
    } else {
        root.join(index_path)
    };

    println!("Importing SCIP index from {}", abs_index.display());
    let stats = backend.import_scip_index(&abs_index, Some(root))?;
    println!(
        "SCIP import complete:\n  files processed: {}\n  symbols added: {}\n  symbols enriched: {}\n  relations added: {}\n  references added: {}\n  corrections learned: {}",
        stats.files_processed,
        stats.symbols_added,
        stats.symbols_enriched,
        stats.relations_added,
        stats.references_added,
        stats.corrections_learned,
    );
    Ok(())
}

pub(crate) fn cmd_index_docs(root: &Path, namespace: Option<&str>) -> Result<()> {
    let start = std::time::Instant::now();
    let mut idx = infigraph_docs::DocIndex::open(root)?;
    if let Some(ns) = namespace {
        idx.set_namespace(ns);
    }

    #[cfg(feature = "remote")]
    let is_remote = std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false);
    #[cfg(not(feature = "remote"))]
    let is_remote = false;

    if is_remote {
        idx.set_skip_file_embeddings(true);
    }

    idx.init()?;
    let result = idx.index()?;
    let elapsed = start.elapsed();
    println!(
        "Document indexing complete in {:.1}s\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}",
        elapsed.as_secs_f64(), result.total_files, result.indexed_files, result.total_chunks
    );
    if let Some(store) = idx.store() {
        let stats = store.stats()?;
        println!(
            "  Total documents in store: {}\n  Total chunks in store: {}",
            stats.document_count, stats.chunk_count
        );
    }

    #[cfg(feature = "remote")]
    if is_remote {
        let pg = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached()?;
        pg.init_schema()?;
        let store = idx.store().context("doc store not initialized")?;
        let chunk_refs: Vec<&infigraph_docs::chunk::Chunk> = result.new_chunks.iter().collect();
        let changed_refs: Vec<&str> = result.changed_files.iter().map(|s| s.as_str()).collect();
        let count = infigraph_docs::embed::update_doc_embeddings_remote(
            store,
            &pg,
            &chunk_refs,
            &changed_refs,
        )?;
        if count > 0 {
            println!("Saved {} doc embeddings to Postgres pgvector", count);
        }
    }

    Ok(())
}

pub(crate) fn cmd_reindex_docs(root: &Path) -> Result<()> {
    let start = std::time::Instant::now();
    let mut idx = infigraph_docs::DocIndex::open(root)?;
    let result = idx.reindex()?;
    let elapsed = start.elapsed();
    println!(
        "Document full reindex complete in {:.1}s\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}",
        elapsed.as_secs_f64(), result.total_files, result.indexed_files, result.total_chunks
    );
    Ok(())
}

pub(crate) fn cmd_clean_docs(root: &Path) -> Result<()> {
    let mut idx = infigraph_docs::DocIndex::open(root)?;
    idx.clean()?;
    println!("Document index cleaned.");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_index_confluence(
    root: &Path,
    base_url: &str,
    space: &str,
    page_ids: Option<Vec<String>>,
    pat: Option<String>,
    email: Option<String>,
    api_token: Option<String>,
    follow_links: bool,
    follow_depth: usize,
    max_pages: usize,
) -> Result<()> {
    let client = if let Some(pat) = pat {
        infigraph_confluence::ConfluenceClient::new(base_url, &pat)
    } else if let (Some(email), Some(token)) = (email, api_token) {
        infigraph_confluence::ConfluenceClient::new_basic(base_url, &email, &token)
    } else {
        anyhow::bail!("Provide either --pat or both --email and --api-token for authentication");
    };

    let crawl = if follow_links {
        infigraph_confluence::CrawlOptions {
            follow_links: true,
            follow_depth,
            max_pages,
            same_space_only: true,
        }
    } else {
        infigraph_confluence::CrawlOptions::no_follow()
    };

    let start = std::time::Instant::now();
    let sync = infigraph_confluence::ConfluenceSync::new(client, space);

    let mut idx = infigraph_docs::DocIndex::open(root)?;
    idx.init()?;
    let store = idx.store().context("DocStore not initialized")?;

    let ids = page_ids.as_deref();
    let result = sync.sync_with_options(store, root, ids, &crawl)?;
    let elapsed = start.elapsed();

    println!(
        "Confluence sync complete in {:.1}s\n  Pages fetched: {}\n  Pages indexed: {}\n  Pages deleted: {}\n  Chunks created: {}\n  Links created: {}",
        elapsed.as_secs_f64(),
        result.pages_fetched,
        result.pages_indexed,
        result.pages_deleted,
        result.chunks_created,
        result.links_created,
    );

    let stats = store.stats()?;
    println!(
        "  Total documents in store: {}\n  Total chunks in store: {}",
        stats.document_count, stats.chunk_count
    );
    Ok(())
}

pub(crate) fn cmd_list_files(root: &Path, glob: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let rows = backend.raw_query("MATCH (s:Symbol) RETURN DISTINCT s.file ORDER BY s.file")?;

    if rows.is_empty() {
        println!("No files indexed. Run 'infigraph index' first.");
        return Ok(());
    }

    let glob_pat = glob.unwrap_or("");
    let mut files: Vec<&str> = rows
        .iter()
        .filter_map(|row| row.first().map(|s| s.as_str()))
        .filter(|f| glob_pat.is_empty() || infigraph_mcp::tools::helpers::glob_matches(glob_pat, f))
        .collect();
    files.dedup();

    println!("{} source files:", files.len());
    for f in &files {
        println!("  {}", f);
    }
    Ok(())
}

pub(crate) fn cmd_generate_test_context(
    root: &Path,
    file: Option<&str>,
    limit: usize,
) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init_read_only()?;

    let backend = prism.backend().context("graph not initialized")?;
    let ctx = backend.generate_test_context(file, limit, None)?;

    println!("Test Generation Context\n");
    println!("Framework: {}", ctx.framework);

    if let Some(ref ex) = ctx.example_test {
        println!("\nExample Test (style reference):");
        println!(
            "  {} — {}:{}-{}",
            ex.name, ex.file, ex.start_line, ex.end_line
        );
        let file_path = root.join(&ex.file);
        if let Ok(source) = std::fs::read_to_string(&file_path) {
            let lines: Vec<&str> = source.lines().collect();
            let start = (ex.start_line as usize).saturating_sub(1);
            let end = (ex.end_line as usize).min(lines.len());
            if start < end {
                for (i, line) in lines[start..end].iter().enumerate() {
                    println!("  {:4}  {}", start + i + 1, line);
                }
            }
        }
    }

    println!(
        "\nTargets ({} uncovered symbols, priority-ranked):\n",
        ctx.targets.len()
    );

    for (i, t) in ctx.targets.iter().enumerate() {
        println!(
            "{}. {} [{}] — {}:{}-{} (priority: {})",
            i + 1,
            t.name,
            t.kind,
            t.file,
            t.start_line,
            t.end_line,
            t.priority_score
        );
        if !t.visibility.is_empty() {
            println!("   visibility: {}", t.visibility);
        }
        if !t.parameters.is_empty() {
            println!("   params: {}", t.parameters);
        }
        if !t.return_type.is_empty() {
            println!("   returns: {}", t.return_type);
        }
        if t.complexity > 1 {
            println!("   complexity: {}", t.complexity);
        }
        if !t.callers.is_empty() {
            println!("   callers: {}", t.callers.join(", "));
        }
        if !t.callees.is_empty() {
            println!("   callees: {}", t.callees.join(", "));
        }
        if !t.branches.is_empty() {
            println!("   branches ({}):", t.branches.len());
            for b in &t.branches {
                let indent = "   ".repeat(b.depth as usize + 2);
                if b.condition.is_empty() {
                    println!("{}L{}: {}", indent, b.line, b.kind);
                } else {
                    println!("{}L{}: {} ({})", indent, b.line, b.kind, b.condition);
                }
            }
        }

        let file_path = root.join(&t.file);
        if let Ok(source) = std::fs::read_to_string(&file_path) {
            let lines: Vec<&str> = source.lines().collect();
            let start = (t.start_line as usize).saturating_sub(1);
            let end = (t.end_line as usize).min(lines.len());
            if start < end {
                for (i, line) in lines[start..end].iter().enumerate() {
                    println!("   {:4}  {}", start + i + 1, line);
                }
            }
        }
        println!();
    }

    Ok(())
}

pub(crate) fn cmd_delete_project(root: &Path) -> Result<()> {
    let project_path = PathBuf::from(root);

    // Stop watcher before removing data
    let lock_path = project_path.join(".infigraph").join("watch.lock");
    if infigraph_core::watch::daemon::daemon_is_alive(&lock_path) {
        let sentinel = project_path.join(".infigraph").join("watch.stop");
        let _ = std::fs::write(&sentinel, b"");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Remove the .infigraph directory within the project
    let infigraph_dir = project_path.join(".infigraph");
    if infigraph_dir.exists() {
        std::fs::remove_dir_all(&infigraph_dir).context("failed to remove .infigraph directory")?;
    }

    // Unregister from the global registry
    use infigraph_core::multi::Registry;
    let mut registry = Registry::load()?;
    let to_remove = registry.deregister_by_path(&project_path);
    registry.save()?;

    if to_remove.is_empty() {
        println!(
            "Removed .infigraph directory from {}. (Project was not in the global registry.)",
            root.display()
        );
    } else {
        println!(
            "Removed .infigraph directory and unregistered '{}' from global registry.",
            to_remove.join(", ")
        );
    }
    Ok(())
}

pub(crate) fn cmd_memory_context(
    root: &Path,
    query: &str,
    file: Option<&str>,
    depth: &str,
    sources: &str,
    limit: usize,
) -> Result<()> {
    let path = root.to_string_lossy();
    let mut args = json!({
        "path": path.as_ref(),
        "query": query,
        "depth": depth,
        "sources": sources,
        "limit": limit,
    });
    if let Some(f) = file {
        args["file"] = json!(f);
    }
    let result = infigraph_mcp::tools::memory_context::tool_memory_context(&args)?;
    println!("{result}");
    Ok(())
}

pub(crate) fn cmd_consolidate_memory(root: &Path, threshold: f64) -> Result<()> {
    let path = root.to_string_lossy();
    let args = json!({
        "path": path.as_ref(),
        "threshold": threshold,
    });
    let result = infigraph_mcp::tools::session::tool_consolidate_memory(&args)?;
    println!("{result}");
    Ok(())
}

pub(crate) fn cmd_purge_sessions(root: &Path, days: u32) -> Result<()> {
    let path = root.to_string_lossy();
    let args = json!({
        "path": path.as_ref(),
        "older_than_days": days,
    });
    let result = infigraph_mcp::tools::session::tool_purge_sessions(&args)?;
    println!("{result}");
    Ok(())
}

/// Color only when stdout is a real terminal and the user hasn't opted out
/// via `NO_COLOR` (https://no-color.org/) — piped/redirected output (logs,
/// CI, `| less`) stays plain so it greps and diffs cleanly.
fn doctor_output_is_colorized() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

pub(crate) fn cmd_doctor(root: &Path, global: bool) -> Result<()> {
    use infigraph_core::doctor::{
        assemble_context, format_report, run_doctor, CheckStatus, DoctorScope,
    };

    let scope = if global {
        DoctorScope::Global
    } else {
        let canonical_root = root.canonicalize().context("invalid project root")?;
        DoctorScope::Project(canonical_root)
    };
    let ctx = assemble_context(scope);
    let report = run_doctor(ctx);
    print!("{}", format_report(&report, doctor_output_is_colorized()));

    match report.worst_status() {
        CheckStatus::Pass => Ok(()),
        CheckStatus::Warn => anyhow::bail!("doctor found warnings"),
        CheckStatus::Fail => std::process::exit(2),
    }
}

/// `infigraph gc` (R7.1): evict registry entries for deleted projects,
/// optionally also age-stale ones. Planning/mutation live in
/// `infigraph_core::gc` (pure, tested there); this owns the user-facing
/// report, the confirmation-free-but-auditable persistence, and the R6.3
/// audit lines -- written only AFTER the registry save succeeds, so the
/// audit never records an eviction that didn't actually persist.
pub(crate) fn cmd_gc(root: &Path, dry_run: bool, stale_days: Option<u64>) -> Result<()> {
    let mut registry = infigraph_core::multi::Registry::load()?;
    let plan =
        infigraph_core::gc::plan_registry_gc(&registry, stale_days, std::time::SystemTime::now());

    // Independent of the registry plan above: a daemon can be watching a
    // deleted directory (a test's ephemeral tempdir, a project someone
    // `rm -rf`'d without `infigraph delete`) that was never a registry
    // entry in the first place, so this must not be gated on
    // `plan.is_empty()`. See `find_orphaned_daemons`'s doc comment for why
    // this needs a live-process sweep rather than the registry/lock-file
    // path the eviction above uses.
    let orphaned_daemons = infigraph_core::watch::daemon::find_orphaned_daemons();

    if plan.is_empty() && orphaned_daemons.is_empty() {
        println!("Registry is clean -- nothing to evict.");
        return Ok(());
    }

    for c in &plan.evictions {
        println!("evict: {} ({}) -- {}", c.name, c.path.display(), c.reason);
    }
    for (group, member) in &plan.dangling_group_members {
        println!("prune: group '{group}' member '{member}' (no longer registered)");
    }
    for d in &orphaned_daemons {
        println!(
            "kill: infigraph daemon pid {} -- watched root no longer exists ({})",
            d.pid,
            d.cwd.display()
        );
    }

    if dry_run {
        println!(
            "\nDry run -- nothing changed. Re-run without --dry-run to evict {} entr{} and kill {} orphaned daemon{}.",
            plan.evictions.len(),
            if plan.evictions.len() == 1 { "y" } else { "ies" },
            orphaned_daemons.len(),
            if orphaned_daemons.len() == 1 { "" } else { "s" },
        );
        return Ok(());
    }

    infigraph_core::gc::execute_registry_gc(&mut registry, &plan);
    registry.save()?;

    for c in &plan.evictions {
        infigraph_core::audit::audit_log(
            "gc",
            "evict-registry-entry",
            &c.reason.to_string(),
            &c.path.display().to_string(),
        );
    }
    for (group, member) in &plan.dangling_group_members {
        infigraph_core::audit::audit_log(
            "gc",
            "prune-group-member",
            "member no longer registered",
            &format!("{group}/{member}"),
        );
    }
    for d in &orphaned_daemons {
        infigraph_core::watch::daemon::kill_orphaned_daemon(d.pid);
        infigraph_core::audit::audit_log(
            "gc",
            "kill-orphaned-daemon",
            "watched root no longer exists",
            &format!("pid {} ({})", d.pid, d.cwd.display()),
        );
    }
    // Every orphaned daemon killed above was already confirmed watching a
    // directory that no longer exists, so there's provably no WAL left
    // behind by *that* kill to find. This still runs, same as `kill`'s own
    // call: it sweeps every project this process knows about, so it also
    // catches damage from something else entirely (a `kill -9` run outside
    // `infigraph kill`, a crashed process) -- the same reasoning `doctor`
    // itself runs on every invocation rather than only after a change.
    if !orphaned_daemons.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        report_post_kill_wal_integrity(root);
    }

    println!(
        "\nEvicted {} registry entr{}, pruned {} group member(s), killed {} orphaned daemon{}. Audit: ~/.infigraph/logs/audit.log",
        plan.evictions.len(),
        if plan.evictions.len() == 1 { "y" } else { "ies" },
        plan.dangling_group_members.len(),
        orphaned_daemons.len(),
        if orphaned_daemons.len() == 1 { "" } else { "s" },
    );
    Ok(())
}

/// `infigraph ps` (R2.2.4): every process the durable state knows about.
pub(crate) fn cmd_ps(root: &Path) -> Result<()> {
    let registry = infigraph_core::multi::Registry::load().unwrap_or_default();
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let scope = infigraph_core::ps::ps_scope(&registry, &canonical_root);
    let scope_refs: Vec<&Path> = scope.iter().map(|p| p.as_path()).collect();
    let rows = infigraph_core::ps::list_infigraph_processes(&scope_refs);

    if rows.is_empty() {
        println!("No infigraph processes recorded (no instance registrations, no lock holders).");
        return Ok(());
    }

    println!(
        "{:<8} {:<6} {:<10} {:<10} {:<28} PROJECT / EVIDENCE",
        "PID", "STATE", "UPTIME", "RSS", "ROLE"
    );
    for r in &rows {
        let state = if r.alive { "live" } else { "dead" };
        let uptime = r
            .uptime_secs
            .map(format_uptime)
            .unwrap_or_else(|| "-".to_string());
        let rss = r
            .rss_bytes
            .map(|b| format!("{} MB", b / (1024 * 1024)))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<8} {:<6} {:<10} {:<10} {:<28} {} [{}]",
            r.pid,
            state,
            uptime,
            rss,
            r.roles.join(","),
            r.projects.join(", "),
            r.evidence.join(",")
        );
        if !r.alive {
            println!(
                "         ^ stale lock -- holder is gone; `infigraph doctor` explains, deleting the lock file is safe"
            );
        }
    }
    Ok(())
}

fn format_uptime(secs: u64) -> String {
    if secs >= 86_400 {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// `infigraph kill` (R2.2.4): guarded terminate, audited (R6.3).
pub(crate) fn cmd_kill(root: &Path, pid: u32, force: bool) -> Result<()> {
    match infigraph_core::ps::kill_infigraph_process(pid, force) {
        Ok(name) => {
            let how = if force { "SIGKILL" } else { "SIGTERM" };
            infigraph_core::audit::audit_log(
                "kill",
                if force {
                    "kill-forced"
                } else {
                    "kill-graceful"
                },
                "operator requested via infigraph kill",
                &format!("pid={pid} name={name}"),
            );
            println!("Sent {how} to {name} (pid {pid}). Audit: ~/.infigraph/logs/audit.log");

            // A graceful SIGTERM needs a moment to actually exit before a WAL
            // it left mid-write would even show up as "holder is dead" --
            // give it the same window `ensure_daemon_running`'s own prune
            // path allows. A forced SIGKILL is already dead by the time
            // kill() returns, so there's nothing to wait for.
            if !force {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            report_post_kill_wal_integrity(root);
            Ok(())
        }
        Err(refusal) => anyhow::bail!("refusing to kill pid {pid}: {refusal}"),
    }
}

/// Sweeps every project this process knows about (the registry, plus
/// `root` if it isn't already registered) for a WAL a just-killed process
/// left mid-write -- the exact damage `infigraph_core::doctor::
/// check_one_wal_integrity` exists to catch, run here so a `kill`/`gc`
/// action surfaces it immediately instead of waiting for the next `doctor`
/// run (or, worse, the next unrelated command that happens to try opening
/// that graph -- see github.com/pradeepmouli/infigraph#92). Cheap: this
/// only stats/reads lock and WAL files, never opens the database.
pub(crate) fn report_post_kill_wal_integrity(root: &Path) {
    let registry = infigraph_core::multi::Registry::load().unwrap_or_default();
    let mut projects: Vec<PathBuf> = registry.repos.values().map(|e| e.path.clone()).collect();
    if let Ok(canonical_root) = root.canonicalize() {
        if !projects.iter().any(|p| p == &canonical_root) {
            projects.push(canonical_root);
        }
    }

    let mut header_printed = false;
    for project in &projects {
        let result = infigraph_core::doctor::check_one_wal_integrity(project);
        if result.status == infigraph_core::doctor::CheckStatus::Pass {
            continue;
        }
        if !header_printed {
            println!("\nPost-kill check found graph damage:");
            header_printed = true;
        }
        println!("  ! {}: {}", result.name, result.message);
        if let Some(remediation) = &result.remediation {
            println!("    -> {remediation}");
        }
    }
}

/// `infigraph verify` (R3.4.1): offline consistency check, doctor-style
/// output, CI-friendly exit codes (0 pass / 1 warn / 2 fail).
pub(crate) fn cmd_verify(root: &Path) -> Result<()> {
    use infigraph_core::doctor::{format_report, CheckStatus, DoctorReport, DoctorScope};

    let canonical_root = root.canonicalize().context("invalid project root")?;
    let checks = infigraph_core::verify::run_verify(&canonical_root);
    let report = DoctorReport {
        checks,
        scope: DoctorScope::Project(canonical_root),
    };
    print!("{}", format_report(&report, doctor_output_is_colorized()));
    match report.worst_status() {
        CheckStatus::Pass => Ok(()),
        CheckStatus::Warn => anyhow::bail!("verify found warnings"),
        CheckStatus::Fail => std::process::exit(2),
    }
}

#[cfg(test)]
mod shutdown_watchdog_tests {
    use super::{
        watchdog_should_defer, SHUTDOWN_WATCHDOG_GRACE, SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
    };
    use std::time::Duration;

    /// Within the ordinary grace period, defer regardless of whether any
    /// write work is in progress -- this is the original, unconditional
    /// R5.4 budget for the common (nothing in flight) case.
    #[test]
    fn defers_unconditionally_within_the_grace_period() {
        assert!(watchdog_should_defer(
            Duration::from_secs(1),
            SHUTDOWN_WATCHDOG_GRACE,
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
            false,
        ));
        assert!(watchdog_should_defer(
            Duration::from_secs(1),
            SHUTDOWN_WATCHDOG_GRACE,
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
            true,
        ));
    }

    /// Past the grace period with nothing in flight: this is the original
    /// wedged-drain case the watchdog exists for -- must NOT keep
    /// deferring, or a truly stuck daemon becomes unkillable again.
    #[test]
    fn does_not_defer_past_grace_with_nothing_in_flight() {
        assert!(!watchdog_should_defer(
            SHUTDOWN_WATCHDOG_GRACE + Duration::from_millis(1),
            SHUTDOWN_WATCHDOG_GRACE,
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
            false,
        ));
    }

    /// The actual bug being fixed: past the grace period, with `index.lock`
    /// still held by this process (real write work in progress, e.g. a
    /// multi-minute full reindex), the watchdog must keep deferring rather
    /// than hard-kill mid-write and leave an unreplayed WAL behind.
    #[test]
    fn defers_past_grace_while_an_index_op_is_genuinely_in_progress() {
        assert!(watchdog_should_defer(
            SHUTDOWN_WATCHDOG_GRACE + Duration::from_secs(30),
            SHUTDOWN_WATCHDOG_GRACE,
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
            true,
        ));
    }

    /// Even confirmed in-progress write work doesn't defer forever -- the
    /// ceiling remains as a backstop against a write that's actually stuck
    /// (e.g. wedged on a lock while still technically holding index.lock).
    #[test]
    fn stops_deferring_once_the_in_progress_ceiling_is_exceeded() {
        assert!(!watchdog_should_defer(
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING + Duration::from_secs(1),
            SHUTDOWN_WATCHDOG_GRACE,
            SHUTDOWN_WATCHDOG_IN_PROGRESS_CEILING,
            true,
        ));
    }
}

#[cfg(test)]
mod watch_cli_action_tests {
    use super::watch_cli_action_to_watch_action;
    use crate::WatchCliAction;
    use infigraph_core::daemon_protocol::WatchAction;

    #[test]
    fn maps_each_cli_action_to_its_protocol_counterpart() {
        assert_eq!(
            watch_cli_action_to_watch_action(&WatchCliAction::Enable),
            WatchAction::Enable
        );
        assert_eq!(
            watch_cli_action_to_watch_action(&WatchCliAction::Disable),
            WatchAction::Disable
        );
        assert_eq!(
            watch_cli_action_to_watch_action(&WatchCliAction::Start),
            WatchAction::Start
        );
        assert_eq!(
            watch_cli_action_to_watch_action(&WatchCliAction::Stop),
            WatchAction::Stop
        );
        assert_eq!(
            watch_cli_action_to_watch_action(&WatchCliAction::Restart),
            WatchAction::Restart
        );
    }
}

#[cfg(test)]
mod daemon_liveness_guard_tests {
    use super::{cmd_daemon_stop, cmd_watch_control};
    use crate::WatchCliAction;
    use infigraph_core::daemon_protocol::WatchRole;

    // Regression coverage for a Task 12 review finding: without an upfront
    // daemon_is_alive check, these commands would submit into an unattended
    // staging dir and block for the full WATCH_CONTROL_TIMEOUT (30s) before
    // reporting an opaque protocol error -- the exact failure mode
    // index.rs's FullReindex path already guards against (see its own
    // comment re: incident #100). No daemon is ever started here; a bound
    // far below the 30s timeout is what proves the guard actually fired
    // rather than the call happening to return quickly for some other
    // reason.
    const GUARD_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

    #[test]
    fn cmd_daemon_stop_returns_promptly_when_no_daemon_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        cmd_daemon_stop(tmp.path()).unwrap();
        assert!(
            start.elapsed() < GUARD_BOUND,
            "cmd_daemon_stop should fail fast on a missing daemon, not wait out the protocol timeout"
        );
    }

    // cmd_daemon_restart has no dedicated test here: once daemon_is_alive
    // is false, it falls through to ensure_daemon_running_required, which
    // actually spawns a detached daemon process -- not something to trigger
    // from a fast unit test (would leak a real orphan process pointed at a
    // tempdir). Its liveness guard is structurally identical to
    // cmd_daemon_stop's (same check, same early condition), verified by
    // code review rather than a spawning test.

    #[test]
    fn cmd_watch_control_start_returns_promptly_when_no_daemon_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        cmd_watch_control(tmp.path(), WatchRole::Code, WatchCliAction::Start).unwrap();
        assert!(
            start.elapsed() < GUARD_BOUND,
            "cmd_watch_control(Start) should fail fast on a missing daemon"
        );
    }

    #[test]
    fn cmd_watch_control_enable_still_persists_policy_when_no_daemon_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        // Enable/Disable must not hang either, and -- unlike Start/Stop/
        // Restart -- must still succeed: the policy is meant to survive
        // restarts, so it has to apply the next time a daemon starts even
        // if none is running right now.
        let result = cmd_watch_control(tmp.path(), WatchRole::Code, WatchCliAction::Enable);
        assert!(
            start.elapsed() < GUARD_BOUND,
            "cmd_watch_control(Enable) should not block on notifying a daemon that isn't running"
        );
        assert!(
            result.is_ok(),
            "Enable's durable policy write must still succeed with no daemon running"
        );
    }
}
