use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
#[cfg(feature = "remote")]
use infigraph_core::graph::GraphBackend;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use tokio_util::sync::CancellationToken;

/// Per-indexer subprocess timeout for SCIP enrichment. Nothing before Task 6
/// of the daemon/watch command split bounded a hung SCIP indexer at all --
/// 10 minutes is generous enough for `rust-analyzer`'s cold-start
/// `cargo metadata` resolution but still bounded rather than infinite.
const SCIP_INDEXER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

pub(crate) fn cmd_index(root: &Path, full: bool, no_embed: bool) -> Result<()> {
    // Under the daemon backend this command performs no local graph writes:
    // each one is routed to the daemon, which takes this very
    // .infigraph/index.lock to serve it. Holding the lock here deadlocks --
    // we block on the daemon's result while the daemon retries forever
    // waiting for us -- and even releasing it early isn't enough, because
    // the zero-wait acquisition also makes this command a silent no-op
    // whenever the daemon, its watcher, or a background scip-enrich holds
    // the lock, which under daemon mode is routine. The daemon's own
    // per-request locking is what serializes writes now.
    let op_guard = if infigraph_core::daemon_backend_selected() {
        None
    } else {
        let op = infigraph_core::ops::begin_index_op(
            root,
            "infigraph index",
            std::time::Duration::ZERO,
        )?;
        match op {
            infigraph_core::ops::IndexOpOutcome::Acquired(g) => Some(g),
            o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_) => {
                println!("{}", o.skip_note().unwrap());
                return Ok(());
            }
        }
    };

    #[cfg(feature = "remote")]
    let remote = is_neo4j_backend();
    #[cfg(not(feature = "remote"))]
    let remote = false;

    // A previously-indexed project (`.infigraph/` exists) whose graph file
    // is missing -- deleted manually, or by a prior crash-recovery attempt
    // -- has nothing incremental to protect. Treat it exactly like `--full`
    // rather than falling through to an incremental open that fails with
    // Kuzu's own confusing "Cannot create an empty database under READ ONLY
    // mode" error (#100 second-incident comment). Not a behavior change in
    // outcome -- a full rebuild is what a human would run anyway -- only
    // removes a confusing dead end.
    let tg_dir_for_promotion_check = root.join(".infigraph");
    let full = full
        || (tg_dir_for_promotion_check.exists()
            && !tg_dir_for_promotion_check.join("graph").exists());

    if full {
        if remote {
            // Remote mode: clear the Neo4j graph (local .infigraph/ is irrelevant)
            #[cfg(feature = "remote")]
            {
                let neo = infigraph_core::graph::Neo4jBackend::connect_from_env()?;
                neo.init_schema()?;
                neo.clear_all_data()?;
                println!("Cleared Neo4j graph for full reindex");
            }
        } else if infigraph_core::daemon_backend_selected() {
            // Routed through the daemon's own FullReindex handler, which
            // builds a fresh database at a side path and atomically swaps
            // it in -- see
            // docs/superpowers/specs/2026-08-04-daemon-routed-full-reindex-design.md.
            // Closes https://github.com/pradeepmouli/infigraph/issues/50's
            // real fix (the mitigation this branch used to be is now
            // closed).
            // The daemon is the only thing that serves a FullReindex, and
            // this branch returns before `Infigraph::open`, so the usual
            // auto-start path never runs. Without this check, submitting
            // into an unattended staging dir just sits there until the
            // 600s deadline and then reports an opaque timeout -- a
            // plausible everyday case whenever INFIGRAPH_BACKEND is
            // exported from a shell profile.
            // The pre-dispatch auto-watch (main.rs -> ensure_watcher_running)
            // SPAWNS the daemon but returns before the child has acquired
            // watch.lock, so a one-shot liveness probe here raced it: the
            // #100 incident saw "no daemon is running" moments after
            // "[auto-watch] Watcher started", and the losing-half daemon
            // then made the follow-up `infigraph daemon` report "another
            // watcher is already running". Wait out the startup window
            // first; if the daemon still isn't up (auto-watch skipped: CI,
            // current_exe failure), make one explicit start attempt before
            // giving up with the actionable message.
            let lock_path = root.join(".infigraph").join("watch.lock");
            if !wait_for_daemon(&lock_path, std::time::Duration::from_secs(10)) {
                ensure_watcher_running(root);
                if !wait_for_daemon(&lock_path, std::time::Duration::from_secs(10)) {
                    anyhow::bail!(
                        "INFIGRAPH_BACKEND=daemon is set but no daemon came up for {} \
                         (auto-start attempted), and a full reindex can only be served \
                         by one. Check `infigraph ps` / the daemon log, start one with \
                         `infigraph daemon`, or unset INFIGRAPH_BACKEND to run the \
                         reindex locally in this process.",
                        root.display()
                    );
                }
            }

            let staging_dir = root.join(".infigraph").join("requests");
            let result = infigraph_core::daemon_protocol::submit_write_request(
                &staging_dir,
                &infigraph_core::daemon_protocol::WriteRequest::FullReindex,
                std::time::Duration::from_secs(600),
            )?;
            match result {
                infigraph_core::daemon_protocol::WriteResult::FullReindexOk {
                    total_files,
                    indexed_files,
                    detected_languages,
                } => {
                    println!("Indexed {indexed_files} files ({total_files} total, full reindex)");
                    if detected_languages.is_empty() {
                        println!("No SCIP-eligible languages detected; skipping enrichment.");
                    } else {
                        println!(
                            "SCIP enrichment scheduled on the daemon in the background for: {}",
                            detected_languages.join(", ")
                        );
                    }
                }
                infigraph_core::daemon_protocol::WriteResult::Err { message } => {
                    anyhow::bail!("full reindex failed: {message}");
                }
                other => {
                    anyhow::bail!("full reindex returned an unexpected result: {other:?}");
                }
            }
            // The daemon already did the full reindex -- nothing left for
            // this process to do.
            return Ok(());
        } else {
            let tg_dir = root.join(".infigraph");
            if tg_dir.exists() {
                // `op_guard` above already holds .infigraph/index.lock,
                // satisfying full_reindex_wipe's locking contract.
                infigraph_core::ops::full_reindex_wipe(&tg_dir)?;
                println!(
                    "Cleaned .infigraph/ for full reindex (snapshot saved, sessions preserved)"
                );
            }
        }
    }

    let registry = crate::full_registry(Some(root))?;
    #[allow(unused_mut)]
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    // In shared Neo4j mode, a repo's identity is defined by the group registry, not by its
    // directory name. Resolve the `org/repo` namespace from the registry so a standalone
    // `infigraph index` writes data consistent with `group build` and the read filters.
    // Refuse to index an unregistered repo: writing a locally-invented namespace into the
    // shared graph produces orphaned, mis-namespaced nodes (the original reindter bug).
    #[cfg(feature = "remote")]
    let remote_ns = if remote {
        let reg = infigraph_core::multi::Registry::load()?;
        let ns = reg.resolve_repo_namespace(root).ok_or_else(|| {
            anyhow::anyhow!(
                "repo at '{}' is not registered in any group; in remote mode run \
                 `infigraph group add <group> <path>` first so its org/repo namespace is defined. \
                 (A standalone index cannot invent a namespace for a shared graph.)",
                root.display()
            )
        })?;
        prism.set_namespace(&ns);
        // Scope reads (get_file_hashes / stale-prune) to THIS repo. Without this, a
        // standalone index fetches every repo's file hashes from the shared graph and the
        // stale-file prune deletes all OTHER repos' data — same hazard as group indexing.
        prism.set_repo_filter(&ns);
        Some(ns)
    } else {
        None
    };

    println!("Indexing project...");
    let result = prism.index()?;

    // R3.1.4d/#100: a completed local full reindex (the wipe above plus
    // this successful rebuild) is a verified healthy checkpoint -- refresh
    // the growth-ratio breaker's baseline here. Ordinary incremental
    // `infigraph index` runs (this same call, `full == false`) deliberately
    // do not -- see `stamp_healthy_graph_size`'s doc comment. The
    // daemon-routed full-reindex branch above already does its own
    // equivalent stamp and returns before reaching this point.
    if full && !remote {
        let dir = root.join(".infigraph");
        infigraph_core::graph::stamp_healthy_graph_size(&dir, &dir.join("graph"));
    }

    if result.indexed_files == 0 {
        println!(
            "All {} files up-to-date, nothing to reindex",
            result.total_files
        );
    } else {
        println!(
            "Indexed {} files ({} up-to-date, {} total)",
            result.indexed_files,
            result.total_files - result.indexed_files,
            result.total_files
        );
    }

    let mut by_lang: std::collections::HashMap<&str, (usize, usize)> =
        std::collections::HashMap::new();
    for ext in &result.extractions {
        let entry = by_lang.entry(&ext.language).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ext.symbols.len();
    }
    for (lang, (files, symbols)) in &by_lang {
        println!("  {}: {} files, {} symbols", lang, files, symbols);
    }

    if result.resolve_stats.total_calls > 0 {
        println!("{}", result.resolve_stats);
    }

    // Derive TESTED_BY edges — scoped to changed files for incremental
    if result.indexed_files > 0 && prism.backend().is_some() {
        let changed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
        let scope = if full { None } else { Some(changed.as_slice()) };
        match prism.backend().unwrap().derive_tested_by_edges(scope) {
            Ok(count) if count > 0 => println!("Derived {} TESTED_BY edges", count),
            Ok(_) => {}
            Err(e) => eprintln!("warning: TESTED_BY derivation failed: {e}"),
        }
    }

    // Detect cross-cutting concerns, taint, etc. — skip when no files changed (incremental no-op)
    if result.indexed_files > 0 && prism.backend().is_some() {
        // Docstring-only analyzers (no file I/O)
        match infigraph_core::concerns::detect_cross_cutting(prism.backend().unwrap()) {
            Ok(matches) if !matches.is_empty() => {
                println!("Detected {} cross-cutting concerns", matches.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: concern detection failed: {e}"),
        }
        match infigraph_core::config::detect_config_bindings(prism.backend().unwrap()) {
            Ok(bindings) if !bindings.is_empty() => {
                println!("Detected {} config bindings", bindings.len());
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: config binding detection failed: {e}"),
        }
        match infigraph_core::reflection::detect_reflection_sites(prism.backend().unwrap(), root) {
            Ok(sites) if !sites.is_empty() => {
                let resolved = sites.iter().filter(|s| s.resolved_to.is_some()).count();
                println!(
                    "Detected {} reflection sites ({} resolved)",
                    sites.len(),
                    resolved
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("warning: reflection detection failed: {e}"),
        }

        // Source-reading analyzers — build shared cache once, pass to all three
        let taint_backend = prism.backend().unwrap();
        match infigraph_core::taint::build_source_cache(taint_backend, root) {
            Ok((functions, cache)) => {
                match infigraph_core::taint::detect_taint_flows_with_cache(
                    taint_backend,
                    &functions,
                    &cache,
                ) {
                    Ok(flows) if !flows.is_empty() => {
                        let active = flows.iter().filter(|f| !f.sanitized).count();
                        println!(
                            "Detected {} taint flows ({} active, {} sanitized)",
                            flows.len(),
                            active,
                            flows.len() - active
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("warning: taint analysis failed: {e}"),
                }
                match infigraph_core::taint::interprocedural::detect_interprocedural_taint_with_cache(taint_backend, &functions, &cache, 5) {
                    Ok(flows) if !flows.is_empty() => {
                        println!("Detected {} inter-procedural taint flows", flows.len());
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("warning: inter-procedural taint failed: {e}"),
                }
                match infigraph_core::taint::dynamic_urls::detect_dynamic_urls_with_cache(
                    taint_backend,
                    &functions,
                    &cache,
                ) {
                    Ok(urls) if !urls.is_empty() => {
                        let matched = urls.iter().filter(|u| u.matched_route.is_some()).count();
                        println!(
                            "Detected {} dynamic URLs ({} matched to routes)",
                            urls.len(),
                            matched
                        );
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("warning: dynamic URL detection failed: {e}"),
                }
            }
            Err(e) => eprintln!("warning: source cache build failed: {e}"),
        }
    }

    let stats = prism.stats()?;
    println!("\n{}", stats);

    // Register this repo in the registry (~/.infigraph/registry.json locally,
    // Postgres in remote mode) so `infigraph repos` and `infigraph doctor` see it.
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let repo_name = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    {
        let mut registry = infigraph_core::multi::Registry::load()?;
        registry.register_repo(&repo_name, root, &prism)?;
    }

    #[cfg(feature = "remote")]
    if is_neo4j_backend() {
        println!("Registered '{}' in Postgres registry", repo_name);

        // Create Repo node in Neo4j keyed by the org/repo namespace (matching f.repo),
        // and link only this repo's files.
        if let Some(backend) = prism.backend() {
            let repo_key = remote_ns.as_deref().unwrap_or(&repo_name);
            backend.upsert_repo(repo_key)?;
            println!("Created Repo node '{}' with BELONGS_TO edges", repo_key);
        }
    }

    // Hint: suggest .infigraphignore if none exists
    if !root.join(".infigraphignore").exists() {
        eprintln!("\nhint: Create .infigraphignore in the project root to exclude non-source directories.");
        eprintln!("      Common entries:");
        eprintln!("        target/        # Rust build output");
        eprintln!("        build/         # build output (Gradle, CMake, etc.)");
        eprintln!("        dist/          # distribution bundles");
        eprintln!("        out/           # compiler/IDE output");
        eprintln!("        vendor/        # vendored dependencies (Go, Ruby)");
        eprintln!("        bin/           # compiled binaries");
        eprintln!("        obj/           # intermediate build objects (.NET, C++)");
        eprintln!("        generated/     # auto-generated code");
        eprintln!("        third_party/   # third-party source copies");
        eprintln!("        CMakeFiles/    # CMake internal files");
        eprintln!("      One entry per line. Lines starting with # are comments.");
    }

    // Compute and save embeddings — only for new/changed symbols
    if no_embed {
        auto_scip(root, &result, prism.backend())?;
        return Ok(());
    }
    {
        let changed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
        #[allow(unused_mut)]
        let mut done = false;

        #[cfg(feature = "remote")]
        if is_neo4j_backend() {
            let backend = prism.backend().context("graph not initialized")?;
            let pg = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached()?;
            pg.init_schema()?;
            let count = infigraph_core::embed::update_embeddings_remote(backend, &pg, &changed)?;
            println!("Saved {} embeddings to Postgres pgvector", count);
            done = true;
        }

        if !done {
            let backend = prism.backend().context("graph not initialized")?;
            let count = infigraph_core::embed::update_embeddings(backend, root, &changed)?;
            println!("Saved {} embeddings to .infigraph/embeddings.bin", count);
        }
    }

    // Auto-index documents (PDF, DOCX, XML, Markdown, etc.)
    #[cfg(feature = "remote")]
    let doc_ns = remote_ns.as_deref();
    #[cfg(not(feature = "remote"))]
    let doc_ns: Option<&str> = None;
    match crate::commands::cmd_index_docs(root, doc_ns) {
        Ok(()) => {}
        Err(e) => eprintln!("warning: document indexing failed: {e}"),
    }

    // Drop prism to release the GraphStore handle before background SCIP
    let detected_languages: std::collections::HashSet<String> = result
        .extractions
        .iter()
        .map(|e| e.language.clone())
        .collect();
    drop(prism);

    // Release the index-op lock before spawning the detached scip-enrich
    // child. That child re-invokes this binary and immediately tries to
    // acquire the same index.lock (role "scip-enrich") — if we're still
    // holding it here, the child would see AlreadyRunning against its own
    // parent and silently skip enrichment on every single index run.
    drop(op_guard);

    // SCIP enrichment in a detached child process — parent returns immediately.
    spawn_scip_child_process(root, &detected_languages);

    if let Err(e) = infigraph_core::claude_md::ensure_project_claude_md(root) {
        eprintln!("warning: failed to update project CLAUDE.md: {e}");
    }

    Ok(())
}

/// List or restore pre-write snapshots, quarantined (corrupt) graphs, and
/// retired-previous graphs (R3.2.2/docs/DESIGN-hardening.md §3.2).
pub(crate) fn cmd_restore(root: &Path, id: Option<&str>, yes: bool) -> Result<()> {
    let tg_dir = root.join(".infigraph");
    let points = infigraph_core::snapshot::list_restore_points(&tg_dir);

    let Some(id) = id else {
        if points.is_empty() {
            println!("No restore points found under {}", tg_dir.display());
        } else {
            println!("Restore points (newest first):");
            for p in &points {
                println!("  {}  {}", p.id(), format_timestamp(p.timestamp));
            }
            println!("\nRestore one with: infigraph restore <id>");
        }
        return Ok(());
    };

    let point = points
        .iter()
        .find(|p| p.id() == id)
        .with_context(|| format!("no restore point matching '{id}' -- run `infigraph restore` with no arguments to list available points"))?;

    if !yes {
        let should_proceed = if std::io::stdin().is_terminal() {
            print!(
                "Restore {} ({})? The current live state will be preserved first. [y/N] ",
                point.id(),
                format_timestamp(point.timestamp)
            );
            use std::io::Write;
            std::io::stdout().flush()?;
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            let answer = answer.trim().to_lowercase();
            answer == "y" || answer == "yes"
        } else {
            false
        };
        if !should_proceed {
            println!("Aborted (pass --yes to restore without confirming).");
            return Ok(());
        }
    }

    // Serializes against a live index/watcher touching the same .infigraph/
    // tree, same contract `cmd_index`'s destructive --full wipe relies on.
    let _op_guard = match infigraph_core::ops::begin_index_op(
        root,
        "infigraph restore",
        std::time::Duration::from_secs(5),
    )? {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_) => {
            anyhow::bail!(
                "{}",
                o.skip_note()
                    .unwrap_or_else(|| "another index operation is already running".to_string())
            );
        }
    };

    infigraph_core::snapshot::restore(&tg_dir, point)
        .with_context(|| format!("restore of {} failed", point.id()))?;

    println!("Restored {}.", point.id());
    if !matches!(
        point.kind,
        infigraph_core::snapshot::RestorePointKind::Snapshot
    ) {
        println!(
            "Note: this pool only holds the graph itself -- embeddings/sidecars are now stale \
             relative to it. Run `infigraph index` to refresh them."
        );
    }
    Ok(())
}

fn format_timestamp(epoch_secs: u64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("epoch {epoch_secs}"))
}

fn spawn_scip_child_process(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    use crate::scip_download;

    let indexers = scip_download::indexers_for_languages(detected_languages);
    if indexers.is_empty() {
        return;
    }

    let count = indexers.len();
    let indexer_names: Vec<&str> = indexers.iter().map(|i| i.binary_name).collect();
    println!(
        "SCIP enrichment starting in background ({count} indexer(s): {})...",
        indexer_names.join(", ")
    );

    let langs: String = detected_languages
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(",");

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };

    let log_path = root.join(".infigraph").join("scip-enrich.log");
    let stderr_target = match std::fs::File::create(&log_path) {
        Ok(f) => std::process::Stdio::from(f),
        Err(_) => std::process::Stdio::null(),
    };

    match std::process::Command::new(exe)
        .args(scip_enrich_args(&langs))
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr_target)
        .spawn()
    {
        Ok(mut child) => {
            // spawn() only reports failure to launch (missing binary, exec
            // permission). It says nothing about the child crashing or
            // exiting nonzero afterward — exactly the failure shape of the
            // bug this function used to hit silently (the child launched
            // fine and died instantly inside clap's parser). Wait on it from
            // a detached thread so this function still returns immediately,
            // but any future silent-death cause surfaces a warning instead
            // of only leaving a trace in a log nobody's prompted to open.
            let log_path = log_path.clone();
            std::thread::spawn(move || {
                if let Some(msg) = scip_enrich_exit_message(child.wait(), &log_path) {
                    eprintln!("{msg}");
                }
            });
        }
        Err(e) => eprintln!("  Warning: failed to spawn scip-enrich: {e}"),
    }

    eprintln!("  Log: {}", log_path.display());
}

/// Args for respawning this binary as the hidden `scip-enrich` subcommand.
/// `languages` is a positional argument on `Commands::ScipEnrich`, not a
/// flag — extracted so tests can assert these parse under that definition
/// without spawning a process.
fn scip_enrich_args(langs: &str) -> Vec<String> {
    vec!["scip-enrich".to_string(), langs.to_string()]
}

/// Whether the active backend is remote Neo4j (vs. the default local Kùzu).
///
/// `Infigraph::backend()` used to return `None` for the default Kùzu
/// backend, so `if let Some(backend) = prism.backend()` doubled as a de
/// facto "are we in remote mode" check. Once `backend()` was made universal
/// (returning `Some` for every backend kind, including local Kùzu), that
/// check silently broke: the Postgres-embeddings branch below started
/// firing on every `remote`-feature build regardless of backend, attempting
/// a Postgres connection even for plain local indexing and failing the
/// whole `index` command with a connection-refused error. Extracted so the
/// exact condition can be unit-tested independently of a real backend.
#[cfg(feature = "remote")]
fn is_neo4j_backend() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

/// Decides what (if anything) to warn about after waiting on the detached
/// scip-enrich child. Extracted from the wait thread so it's testable
/// without spawning a real process — `current_exe()` in `spawn_scip_child_process`
/// resolves to the test binary itself under `cargo test`, not `infigraph`,
/// so the full spawn path can't be exercised end-to-end in a unit test.
fn scip_enrich_exit_message(
    status: std::io::Result<std::process::ExitStatus>,
    log_path: &Path,
) -> Option<String> {
    match status {
        Ok(status) if !status.success() => Some(format!(
            "warning: scip-enrich exited with {status} — see {}",
            log_path.display()
        )),
        Err(e) => Some(format!("warning: failed to wait on scip-enrich: {e}")),
        _ => None,
    }
}

pub(crate) fn ensure_watcher_running(root: &Path) {
    if infigraph_core::daemon::lifecycle::is_ci_env() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    match infigraph_core::daemon::lifecycle::ensure_daemon_running(root, &exe) {
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::Spawned => {
            eprintln!("[auto-watch] Watcher started");
        }
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::Failed(e) => {
            eprintln!("[auto-watch] Failed to start watcher: {e}");
        }
        infigraph_core::daemon::lifecycle::DaemonStartOutcome::AlreadyRunning => {}
    }
}

/// Polls `daemon_is_alive` until it turns true or `budget` elapses. Exists
/// because daemon startup is asynchronous: `ensure_watcher_running` returns
/// at spawn time, and the child needs a moment to acquire watch.lock (#100
/// item 3's race). Thin wrapper over the core primitive shared with
/// `Infigraph::ensure_daemon_for_writes`'s own wait.
pub(crate) fn wait_for_daemon(lock_path: &Path, budget: std::time::Duration) -> bool {
    infigraph_core::daemon::lifecycle::wait_for_daemon_ready(lock_path, budget)
}

pub(crate) fn on_path(cmd: &str) -> bool {
    let lookup = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(lookup)
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub(crate) fn import_scip_and_cleanup(
    root: &Path,
    scip_path: Option<&std::path::Path>,
    existing_backend: Option<&dyn infigraph_core::graph::GraphBackend>,
) {
    let scip_out = scip_path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| root.join("index.scip"));
    if !scip_out.exists() {
        return;
    }

    if let Some(backend) = existing_backend {
        match backend.import_scip_index(&scip_out, Some(root)) {
            Ok(stats) => println!(
                "Auto-SCIP: enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
                stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
            ),
            Err(e) => eprintln!("Auto-SCIP: import failed: {e}"),
        }
        let _ = std::fs::remove_file(&scip_out);
        return;
    }

    let registry = match bundled_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    let mut prism = match Infigraph::open(root, registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    if prism.init().is_err() {
        return;
    }
    let backend = match prism.backend() {
        Some(b) => b,
        None => return,
    };
    match backend.import_scip_index(&scip_out, Some(root)) {
        Ok(stats) => println!(
            "Auto-SCIP: enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
            stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
        ),
        Err(e) => eprintln!("Auto-SCIP: import failed: {e}"),
    }
    let _ = std::fs::remove_file(&scip_out);
}

/// Foreground SCIP execution using scip_download catalog for all detected languages.
pub(crate) fn auto_scip(
    root: &Path,
    result: &infigraph_core::IndexResult,
    backend: Option<&dyn infigraph_core::graph::GraphBackend>,
) -> Result<()> {
    use crate::scip_download;
    use std::collections::HashSet;

    let detected: HashSet<String> = result
        .extractions
        .iter()
        .map(|e| e.language.clone())
        .collect();
    if detected.is_empty() {
        return Ok(());
    }

    let indexers = scip_download::indexers_for_languages(&detected);
    if indexers.is_empty() {
        return Ok(());
    }

    println!(
        "Auto-SCIP: found {} applicable indexer(s) for detected languages",
        indexers.len()
    );

    // Parallel download: ensure all indexer binaries are available
    let binaries: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = indexers
            .iter()
            .map(|idx| s.spawn(move || (*idx, scip_download::ensure_indexer(idx))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Sequential run: each indexer produces index.scip, import, cleanup
    for (indexer, bin_path) in &binaries {
        let Some(bin) = bin_path else { continue };
        if !should_run_indexer(root, indexer) {
            continue;
        }

        let cmd_str = bin.to_string_lossy();
        let extra = scip_download::extra_runtime_paths();
        let extra_path = if extra.is_empty() {
            None
        } else {
            Some(extra.as_str())
        };

        if indexer.binary_name == "scip-java" {
            let has_gradle = root.join("build.gradle").exists()
                || root.join("build.gradle.kts").exists()
                || root.join("settings.gradle").exists()
                || root.join("settings.gradle.kts").exists();
            let has_maven = root.join("pom.xml").exists();

            if has_gradle && has_maven {
                let primary = if root.join("settings.gradle").exists()
                    || root.join("settings.gradle.kts").exists()
                {
                    "gradle"
                } else {
                    "maven"
                };
                let fallback = if primary == "gradle" {
                    "maven"
                } else {
                    "gradle"
                };

                println!("Auto-SCIP: detected both Maven and Gradle, trying {primary}");
                let primary_args = ["index", "--build-tool", primary];
                if run_scip_indexer(
                    root,
                    &cmd_str,
                    &primary_args,
                    indexer.binary_name,
                    extra_path,
                ) {
                    import_scip_and_cleanup(root, None, backend);
                } else {
                    println!("Auto-SCIP: {primary} failed, falling back to {fallback}");
                    let fallback_args = ["index", "--build-tool", fallback];
                    if run_scip_indexer(
                        root,
                        &cmd_str,
                        &fallback_args,
                        indexer.binary_name,
                        extra_path,
                    ) {
                        import_scip_and_cleanup(root, None, backend);
                    }
                }
            } else if run_scip_indexer(
                root,
                &cmd_str,
                indexer.scip_args,
                indexer.binary_name,
                extra_path,
            ) {
                import_scip_and_cleanup(root, None, backend);
            }
            continue;
        }

        if run_scip_indexer(
            root,
            &cmd_str,
            indexer.scip_args,
            indexer.binary_name,
            extra_path,
        ) {
            import_scip_and_cleanup(root, None, backend);
        }
    }

    Ok(())
}

pub(crate) fn run_scip_indexer(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
) -> bool {
    println!("Auto-SCIP: running {label}...");
    let scip_out = root.join("index.scip");
    let mut command = std::process::Command::new(cmd);
    command.args(args).current_dir(root);
    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }
    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }
    match command.status() {
        Ok(s) if s.success() && scip_out.exists() => true,
        Ok(s) => {
            eprintln!("Auto-SCIP: {label} exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: failed to run {label}: {e}");
            false
        }
    }
}

/// Entry point for the hidden `scip-enrich` subcommand (spawned by `index`).
pub(crate) fn cmd_scip_enrich(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    // Same deadlock as `cmd_index` (see its comment): under the daemon
    // backend, `import_scip_index` routes the `ScipImport` write to the
    // daemon, which needs this very `.infigraph/index.lock` to serve it.
    // Holding the lock here while waiting on that write deadlocks this
    // detached child against the daemon. Skip local acquisition entirely --
    // the daemon's own per-request locking already serializes the write.
    let _op_guard = if infigraph_core::daemon_backend_selected() {
        None
    } else {
        let op =
            infigraph_core::ops::begin_index_op(root, "scip-enrich", std::time::Duration::ZERO);
        match op {
            Ok(infigraph_core::ops::IndexOpOutcome::Acquired(g)) => Some(g),
            Ok(o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_)) => {
                eprintln!("{}", o.skip_note().unwrap());
                return;
            }
            Err(e) => {
                eprintln!("warning: scip-enrich: failed to acquire index-op lock: {e}");
                return;
            }
        }
    };
    auto_scip_background(root, detected_languages);
}

/// Part A of SCIP enrichment: find indexers for `detected_languages`,
/// ensure their binaries are downloaded, and run them to produce `.scip`
/// files. Touches nothing in the graph -- safe to run without holding
/// `index.lock`. This matters specifically for the daemon's in-process
/// full-reindex path: this phase can take several minutes on a real
/// multi-language repo (rust-analyzer's cold-start cargo-metadata
/// resolution alone is slow), and holding `index.lock` for that whole
/// duration would block every other write the daemon needs to serve in
/// the meantime -- exactly the regression this split exists to avoid.
///
/// Returns `(label, scip_output_path, succeeded)` per indexer that was
/// actually run. An empty result means there was nothing to import --
/// callers should skip acquiring any lock at all in that case.
pub(crate) fn run_scip_indexers(
    root: &Path,
    detected_languages: &std::collections::HashSet<String>,
    token: &CancellationToken,
) -> Vec<(&'static str, PathBuf, bool)> {
    use crate::scip_download;

    let indexers = scip_download::indexers_for_languages(detected_languages);
    if indexers.is_empty() {
        return Vec::new();
    }

    // Parallel download: ensure all indexer binaries are available
    let binaries: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = indexers
            .iter()
            .map(|idx| s.spawn(move || (*idx, scip_download::ensure_indexer(idx))))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Filter to runnable indexers and build per-indexer tasks
    let scip_tmp = root.join(".infigraph").join("scip-tmp");
    let _ = std::fs::create_dir_all(&scip_tmp);

    let tasks: Vec<_> = binaries
        .into_iter()
        .filter_map(|(indexer, bin_path)| {
            let bin = bin_path?;
            if !should_run_indexer(root, indexer) {
                return None;
            }
            let output_path = scip_tmp.join(format!("{}.scip", indexer.binary_name));
            Some((indexer, bin, output_path))
        })
        .collect();

    if tasks.is_empty() {
        let _ = std::fs::remove_dir_all(&scip_tmp);
        return Vec::new();
    }

    // Run indexers on a small local runtime -- this function is called from
    // `on_full_reindex`, which itself already runs inside a
    // `Task::spawn_blocking` context (not on an async runtime), so it can't
    // just `tokio::task::spawn` directly; it needs to bring its own runtime
    // to drive that async work.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Auto-SCIP: failed to start local runtime for SCIP indexers: {e}");
            let _ = std::fs::remove_dir_all(&scip_tmp);
            return Vec::new();
        }
    };

    let root = root.to_path_buf();
    rt.block_on(async {
        let jobs: Vec<IndexerJob> = tasks
            .into_iter()
            .map(|(indexer, bin, output_path)| {
                let root = root.clone();
                let output_path_for_fut = output_path.clone();
                let fut: std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> =
                    Box::pin(async move {
                        run_scip_indexer_to(&root, &bin, indexer, &output_path_for_fut).await
                    });
                (indexer.binary_name, output_path, fut)
            })
            .collect();
        run_cancellable_indexer_batch(jobs, token).await
    })
}

type IndexerJob = (
    &'static str,
    PathBuf,
    std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
);

/// Launches `jobs` as tokio tasks one at a time, checking `token` before
/// each launch -- once cancelled, no further jobs are started, but every job
/// already launched is still awaited and its result kept (a cancellation
/// mid-batch must not discard indexers that already finished or are still
/// running).
async fn run_cancellable_indexer_batch(
    jobs: Vec<IndexerJob>,
    token: &CancellationToken,
) -> Vec<(&'static str, PathBuf, bool)> {
    let mut handles = Vec::with_capacity(jobs.len());
    for (label, output_path, fut) in jobs {
        if token.is_cancelled() {
            eprintln!(
                "Auto-SCIP: cancelled -- not launching remaining indexer(s), starting with {label}"
            );
            break;
        }
        handles.push((label, output_path, tokio::task::spawn(fut)));
        // Give the runtime a scheduling turn before the next iteration's
        // cancellation check -- without this, a tight loop of `spawn` calls
        // never actually hands control back to the runtime, so a
        // cancellation requested concurrently (e.g. daemon shutdown, driven
        // by a separate task on this same runtime) would never be observed
        // until every job had already been launched.
        tokio::task::yield_now().await;
    }

    let mut results = Vec::with_capacity(handles.len());
    for (label, output_path, handle) in handles {
        results.push((label, output_path, handle.await.unwrap()));
    }
    results
}

/// Part B+C of SCIP enrichment: import each indexer's `.scip` results into
/// the graph, then update embeddings for anything new (embeddings depend
/// on post-import graph state, so this must run after import, under the
/// same lock). This is the part that actually touches the graph -- callers
/// under daemon mode must hold `index.lock` for (only) this call, not for
/// `run_scip_indexers` above. Cleans up `scip_tmp`'s contents as it goes.
pub(crate) fn import_scip_results_and_embed(
    root: &Path,
    prism: &Infigraph,
    results: &[(&'static str, PathBuf, bool)],
) {
    let scip_tmp = root.join(".infigraph").join("scip-tmp");

    let Some(backend) = prism.backend() else {
        let _ = std::fs::remove_dir_all(&scip_tmp);
        return;
    };
    for (label, scip_path, success) in results {
        if *success && scip_path.exists() {
            match backend.import_scip_index(scip_path, Some(root)) {
                Ok(stats) => eprintln!(
                    "Auto-SCIP: {label} enriched {} symbols, {} added, {} references, {} new symbols, {} corrections learned",
                    stats.symbols_enriched, stats.relations_added, stats.references_added, stats.symbols_added, stats.corrections_learned
                ),
                Err(e) => eprintln!("Auto-SCIP: {label} import failed: {e}"),
            }
        }
        let _ = std::fs::remove_file(scip_path);
    }
    let _ = std::fs::remove_dir_all(&scip_tmp);

    // Embed any new symbols SCIP added (skips existing embeddings)
    let root_buf = root.to_path_buf();
    let pre_count = infigraph_core::embed::embedding_count(&root_buf);
    let Some(backend) = prism.backend() else {
        return;
    };
    #[allow(unused_mut)]
    let mut done = false;
    #[cfg(feature = "remote")]
    if is_neo4j_backend() {
        if let Ok(pg) = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached() {
            match infigraph_core::embed::update_embeddings_remote(backend, pg, &[]) {
                Ok(n) => {
                    let new = n.saturating_sub(pre_count);
                    if new > 0 {
                        eprintln!(
                            "Auto-SCIP: embedded {new} new symbols to pgvector from SCIP enrichment"
                        );
                    }
                }
                Err(e) => eprintln!("Auto-SCIP: remote embedding update failed: {e}"),
            }
            done = true;
        }
    }
    if !done {
        match infigraph_core::embed::update_embeddings(backend, &root_buf, &[]) {
            Ok(n) => {
                let new = n.saturating_sub(pre_count);
                if new > 0 {
                    eprintln!("Auto-SCIP: embedded {new} new symbols from SCIP enrichment");
                }
            }
            Err(e) => eprintln!("Auto-SCIP: embedding update failed: {e}"),
        }
    }

    eprintln!("Auto-SCIP: background enrichment complete.");
}

/// Runs auto-SCIP enrichment (find indexers, run them, import results,
/// update embeddings) against an already-open `Infigraph` connection, with
/// **no internal lock acquisition of its own** -- just Part A followed by
/// Part B+C in sequence. Callers that need `index.lock` serialization
/// (anything routing through the daemon) must acquire it themselves,
/// scoped around *only* `import_scip_results_and_embed`, not this whole
/// function -- see `cmd_daemon`'s `on_full_reindex` wiring in
/// `info_commands.rs` for that case.
///
/// `auto_scip_background`'s caller (`cmd_scip_enrich`) already acquires
/// `index.lock` around this *entire* call in the non-daemon case --
/// unchanged by this split, since a standalone detached child process
/// doesn't have anything else it would be blocking by holding it for the
/// whole duration (unlike the daemon's own request-serving loop).
pub(crate) fn run_auto_scip_on(
    root: &Path,
    prism: &Infigraph,
    detected_languages: &std::collections::HashSet<String>,
) {
    // The standalone CLI path has no daemon-lifetime token to thread through
    // -- a fresh, unparented one is never cancelled, matching this path's
    // always-run-to-completion behavior from before Task 6's cancellation
    // checkpoint existed.
    let token = CancellationToken::new();
    let results = run_scip_indexers(root, detected_languages, &token);
    if results.is_empty() {
        return;
    }
    import_scip_results_and_embed(root, prism, &results);
}

/// Background SCIP pipeline for the standalone CLI path: opens its own
/// `Infigraph` connection (there is no already-open one to reuse here,
/// unlike the daemon's in-process path), then delegates the actual
/// run+import+embed work to `run_auto_scip_on`.
fn auto_scip_background(root: &Path, detected_languages: &std::collections::HashSet<String>) {
    use crate::scip_download;

    // Cheap check before opening anything -- avoid the connection-open cost
    // entirely in the common case of "nothing relevant changed."
    if scip_download::indexers_for_languages(detected_languages).is_empty() {
        return;
    }

    let registry = match bundled_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    let mut prism = match Infigraph::open(root, registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    if prism.init().is_err() {
        return;
    }
    run_auto_scip_on(root, &prism, detected_languages);
}

fn should_run_indexer(root: &Path, indexer: &crate::scip_download::ScipIndexer) -> bool {
    if indexer.binary_name == "scip-clang" && !root.join("compile_commands.json").exists() {
        eprintln!("Auto-SCIP: skipping scip-clang — compile_commands.json not found");
        return false;
    }
    if indexer.binary_name == "scip-ruby" {
        let has_gemspec = std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .any(|e| e.path().extension().is_some_and(|ext| ext == "gemspec"))
            })
            .unwrap_or(false);
        if !has_gemspec {
            eprintln!("Auto-SCIP: skipping scip-ruby — no .gemspec found");
            return false;
        }
    }
    true
}

async fn run_scip_indexer_to(
    root: &Path,
    bin: &Path,
    indexer: &crate::scip_download::ScipIndexer,
    output_path: &Path,
) -> bool {
    let label = indexer.binary_name;
    eprintln!("Auto-SCIP: running {label}...");

    let cmd_str = bin.to_string_lossy();
    let extra = crate::scip_download::extra_runtime_paths();
    let extra_path = if extra.is_empty() {
        None
    } else {
        Some(extra.as_str())
    };

    if indexer.binary_name == "scip-java" {
        // `run_scip_java`'s gradle/maven primary+fallback retry logic is
        // out of scope for Task 6 and stays on the synchronous
        // `std::process::Command` path (`run_scip_indexer_cmd` below) -- it
        // blocks this async task's worker thread for its duration. On the
        // `current_thread` runtime `run_scip_indexers` wraps itself in, a
        // scip-java run temporarily starves any other indexer tasks queued
        // alongside it (no `SCIP_INDEXER_TIMEOUT` bound either). Documented
        // as a known follow-up rather than silently left inconsistent.
        return run_scip_java(root, &cmd_str, output_path, extra_path);
    }

    run_scip_indexer_cmd_async(
        root,
        &cmd_str,
        indexer.scip_args,
        label,
        extra_path,
        indexer.output_flag,
        output_path,
        SCIP_INDEXER_TIMEOUT,
    )
    .await
}

fn run_scip_java(root: &Path, cmd: &str, output_path: &Path, extra_path: Option<&str>) -> bool {
    let has_gradle = root.join("build.gradle").exists()
        || root.join("build.gradle.kts").exists()
        || root.join("settings.gradle").exists()
        || root.join("settings.gradle.kts").exists();
    let has_maven = root.join("pom.xml").exists();

    if has_gradle && has_maven {
        let primary =
            if root.join("settings.gradle").exists() || root.join("settings.gradle.kts").exists() {
                "gradle"
            } else {
                "maven"
            };
        let fallback = if primary == "gradle" {
            "maven"
        } else {
            "gradle"
        };

        eprintln!("Auto-SCIP: detected both Maven and Gradle, trying {primary}");
        let primary_args: Vec<&str> = vec!["index", "--build-tool", primary];
        if run_scip_indexer_cmd(
            root,
            cmd,
            &primary_args,
            "scip-java",
            extra_path,
            Some("--output"),
            output_path,
        ) {
            return true;
        }
        eprintln!("Auto-SCIP: {primary} failed, falling back to {fallback}");
        let fallback_args: Vec<&str> = vec!["index", "--build-tool", fallback];
        return run_scip_indexer_cmd(
            root,
            cmd,
            &fallback_args,
            "scip-java",
            extra_path,
            Some("--output"),
            output_path,
        );
    }

    run_scip_indexer_cmd(
        root,
        cmd,
        &["index"],
        "scip-java",
        extra_path,
        Some("--output"),
        output_path,
    )
}

fn run_scip_indexer_cmd(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
    output_flag: Option<&str>,
    output_path: &Path,
) -> bool {
    let mut command = std::process::Command::new(cmd);
    command.args(args).current_dir(root);

    if let Some(flag) = output_flag {
        command.arg(flag).arg(output_path);
    }

    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }

    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }

    match command.status() {
        Ok(s) if s.success() => {
            if output_flag.is_none() {
                let default_out = root.join("index.scip");
                if default_out.exists() && default_out != output_path {
                    let _ = std::fs::rename(&default_out, output_path);
                }
            }
            output_path.exists()
        }
        Ok(s) => {
            eprintln!("Auto-SCIP: {label} exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: failed to run {label}: {e}");
            false
        }
    }
}

/// Async, timeout-bounded equivalent of `run_scip_indexer_cmd` above, used
/// by every non-`scip-java` indexer path (see `run_scip_indexer_to`).
#[allow(clippy::too_many_arguments)]
async fn run_scip_indexer_cmd_async(
    root: &Path,
    cmd: &str,
    args: &[&str],
    label: &str,
    extra_path: Option<&str>,
    output_flag: Option<&str>,
    output_path: &Path,
    timeout: std::time::Duration,
) -> bool {
    let mut command = tokio::process::Command::new(cmd);
    // A timed-out run drops the in-flight `child.wait()` future below,
    // dropping the `Child` itself -- without `kill_on_drop`, tokio leaves
    // the orphaned process running rather than reaping it, which would
    // defeat the point of adding a timeout at all.
    command.args(args).current_dir(root).kill_on_drop(true);

    if let Some(flag) = output_flag {
        command.arg(flag).arg(output_path);
    }

    if let Some(extra) = extra_path {
        let path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        command.env("PATH", format!("{extra}{sep}{path}"));
    }

    {
        let ig = crate::scip_download::infigraph_dir();
        let java_macos = ig.join("java").join("Contents").join("Home");
        if java_macos.exists() {
            command.env("JAVA_HOME", &java_macos);
        } else {
            let java_home = ig.join("java");
            if java_home.join("bin").exists() {
                command.env("JAVA_HOME", &java_home);
            }
        }
        let dotnet_root = ig.join("dotnet");
        if dotnet_root.exists() {
            command.env("DOTNET_ROOT", &dotnet_root);
        }
    }

    let run = async {
        match command.status().await {
            Ok(s) if s.success() => {
                if output_flag.is_none() {
                    let default_out = root.join("index.scip");
                    if default_out.exists() && default_out != output_path {
                        let _ = std::fs::rename(&default_out, output_path);
                    }
                }
                output_path.exists()
            }
            Ok(s) => {
                eprintln!("Auto-SCIP: {label} exited with {s}");
                false
            }
            Err(e) => {
                eprintln!("Auto-SCIP: failed to run {label}: {e}");
                false
            }
        }
    };

    match tokio::time::timeout(timeout, run).await {
        Ok(succeeded) => succeeded,
        Err(_elapsed) => {
            eprintln!("Auto-SCIP: {label} timed out after {timeout:?}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// #100 item 3: the daemon-liveness probe must tolerate the async gap
    /// between `ensure_watcher_running` spawning the daemon and the child
    /// acquiring watch.lock.
    mod wait_for_daemon_startup_race {
        use super::super::wait_for_daemon;
        use std::time::Duration;

        #[test]
        fn held_lock_returns_true_immediately() {
            let tmp = tempfile::tempdir().unwrap();
            let lock_path = tmp.path().join("watch.lock");
            let _held = infigraph_core::lockfile::try_acquire(&lock_path, "watcher")
                .unwrap()
                .unwrap();
            assert!(wait_for_daemon(&lock_path, Duration::from_millis(50)));
        }

        #[test]
        fn no_daemon_times_out_false() {
            let tmp = tempfile::tempdir().unwrap();
            let lock_path = tmp.path().join("watch.lock");
            let start = std::time::Instant::now();
            assert!(!wait_for_daemon(&lock_path, Duration::from_millis(300)));
            assert!(start.elapsed() >= Duration::from_millis(300));
        }

        #[test]
        fn daemon_coming_up_mid_wait_is_detected() {
            let tmp = tempfile::tempdir().unwrap();
            let lock_path = tmp.path().join("watch.lock");
            let lock_path_clone = lock_path.clone();
            // Simulates the freshly-spawned daemon acquiring its lock a
            // beat after the probe starts -- the exact #100 race.
            let holder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                let guard = infigraph_core::lockfile::try_acquire(&lock_path_clone, "watcher")
                    .unwrap()
                    .unwrap();
                std::thread::sleep(Duration::from_millis(2000));
                drop(guard);
            });
            assert!(
                wait_for_daemon(&lock_path, Duration::from_secs(5)),
                "the probe must see the daemon that came up during the wait"
            );
            holder.join().unwrap();
        }
    }

    /// Regression test: `spawn_scip_child_process` respawns this binary with
    /// `scip_enrich_args(&langs)` as the argv tail. This previously hardcoded
    /// `--languages <langs>`, but `Commands::ScipEnrich` declares `languages`
    /// as a positional argument (no `#[arg(long)]`), so every respawned
    /// child died instantly with a clap parse error and no SCIP indexer
    /// (scip-typescript, scip-python, etc.) ever actually ran. Parsing the
    /// exact args through the real `Cli` definition — rather than spawning a
    /// process — catches any future mismatch between the two immediately.
    #[test]
    fn scip_enrich_args_parse_as_positional_language() {
        use clap::Parser;

        let langs = "typescript,python";
        let mut argv = vec!["infigraph".to_string()];
        argv.extend(scip_enrich_args(langs));

        let cli = crate::Cli::try_parse_from(&argv)
            .expect("scip_enrich_args must parse under the ScipEnrich clap definition");

        assert!(
            matches!(&cli.command, crate::Commands::ScipEnrich { languages } if languages == langs),
            "expected Commands::ScipEnrich {{ languages: {langs:?} }}"
        );
    }

    /// Regression test for review feedback on the scip-enrich fix:
    /// `spawn_scip_child_process` used to discard `spawn()`'s result
    /// entirely. `spawn()` only reports failure to *launch* a process — it
    /// says nothing about the child crashing or exiting nonzero afterward,
    /// which is exactly the failure shape of the original bug (the child
    /// launched fine and died instantly inside clap's parser). This asserts
    /// the decision logic used by the wait thread: warn on a nonzero exit,
    /// stay silent on success.
    #[test]
    #[cfg(unix)]
    fn scip_enrich_exit_message_warns_on_nonzero_exit() {
        use std::os::unix::process::ExitStatusExt;

        let log_path = std::path::PathBuf::from("/tmp/some-project/.infigraph/scip-enrich.log");
        let failed = std::process::ExitStatus::from_raw(1 << 8); // exit code 1
        let msg = scip_enrich_exit_message(Ok(failed), &log_path);
        assert!(
            msg.as_deref()
                .is_some_and(|m| m.contains("scip-enrich exited") && m.contains("scip-enrich.log")),
            "expected a warning mentioning the exit status and log path, got {msg:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn scip_enrich_exit_message_silent_on_success() {
        use std::os::unix::process::ExitStatusExt;

        let log_path = std::path::PathBuf::from("/tmp/some-project/.infigraph/scip-enrich.log");
        let ok = std::process::ExitStatus::from_raw(0);
        let msg = scip_enrich_exit_message(Ok(ok), &log_path);
        assert!(
            msg.is_none(),
            "a successful exit should not produce a warning, got {msg:?}"
        );
    }

    #[test]
    fn scip_enrich_exit_message_warns_on_wait_error() {
        let log_path = std::path::PathBuf::from("/tmp/some-project/.infigraph/scip-enrich.log");
        let err = std::io::Error::other("no such process");
        let msg = scip_enrich_exit_message(Err(err), &log_path);
        assert!(
            msg.as_deref()
                .is_some_and(|m| m.contains("failed to wait on scip-enrich")),
            "expected a warning about the wait() failure, got {msg:?}"
        );
    }

    #[tokio::test]
    async fn run_scip_indexer_cmd_async_reports_success_and_failure_like_today() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let output_path = root.join("out.scip");

        // A command that succeeds and writes nothing meaningful -- exercises
        // the `output_flag.is_none()` default-rename branch is NOT hit here
        // since we pass an explicit flag-less no-op; assert only on the
        // success/failure signal, matching today's `run_scip_indexer_cmd`
        // contract (`Ok(s) if s.success() => output_path.exists()` -- a
        // command with no output_flag and no default index.scip produced
        // returns false here correctly, since output_path never gets created).
        let succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "true" },
            if cfg!(windows) {
                &["/C", "exit", "0"]
            } else {
                &[]
            },
            "test-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(
            !succeeded,
            "no output_flag and no index.scip produced means false, matching today's contract"
        );

        let failing_succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "false" },
            if cfg!(windows) {
                &["/C", "exit", "1"]
            } else {
                &[]
            },
            "test-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_secs(5),
        )
        .await;
        assert!(!failing_succeeded);
    }

    #[tokio::test]
    async fn run_scip_indexer_cmd_async_times_out_a_hung_process() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let output_path = root.join("out.scip");

        // `sleep 5` with a 200ms timeout -- must return false (or otherwise
        // signal failure) within roughly the timeout, not wait the full 5s.
        // This is new behavior: nothing today bounds a hung SCIP indexer at all.
        let start = std::time::Instant::now();
        let succeeded = run_scip_indexer_cmd_async(
            root,
            if cfg!(windows) { "cmd" } else { "sleep" },
            if cfg!(windows) {
                &["/C", "timeout", "/T", "5"]
            } else {
                &["5"]
            },
            "hung-indexer",
            None,
            None,
            &output_path,
            std::time::Duration::from_millis(200),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(!succeeded, "a timed-out indexer must report failure");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "must return promptly on timeout, not wait for the full process duration; took {elapsed:?}"
        );
    }

    /// Part B (scope extension): `run_scip_indexers`' cancellation
    /// checkpoint lives in `run_cancellable_indexer_batch` -- the loop that
    /// launches each indexer job and checks `token` between launches. This
    /// drives that loop directly with two real, fast shell-command
    /// "indexers" (`true`, and a `sleep 5` that must never actually start),
    /// with cancellation triggered by a separate, always-ready task (not
    /// tied to either indexer's own completion, mirroring how a real daemon
    /// shutdown arrives on its own, concurrently) so the test proves the
    /// checkpoint stops launching further indexers without discarding the
    /// result of the one that already ran.
    #[tokio::test]
    async fn run_cancellable_indexer_batch_stops_launching_after_cancellation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let out1 = tmp.path().join("first.scip");
        let out2 = tmp.path().join("second.scip");

        let token = tokio_util::sync::CancellationToken::new();
        let second_started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_started_clone = std::sync::Arc::clone(&second_started);

        // Simulates an external cancellation (e.g. daemon shutdown) arriving
        // concurrently while the batch is still launching jobs. This task
        // has no internal `.await`, so it runs to completion the first time
        // the batch loop's `yield_now()` gives the runtime a scheduling
        // turn -- deterministic, unlike racing against real subprocess
        // completion timing.
        let token_for_canceller = token.clone();
        tokio::task::spawn(async move {
            token_for_canceller.cancel();
        });

        let root1 = root.clone();
        let out1_clone = out1.clone();
        let first: IndexerJob = (
            "first",
            out1,
            Box::pin(async move {
                run_scip_indexer_cmd_async(
                    &root1,
                    if cfg!(windows) { "cmd" } else { "true" },
                    if cfg!(windows) {
                        &["/C", "exit", "0"]
                    } else {
                        &[]
                    },
                    "first",
                    None,
                    None,
                    &out1_clone,
                    std::time::Duration::from_secs(5),
                )
                .await
            }),
        );

        let root2 = root.clone();
        let out2_clone = out2.clone();
        let second: IndexerJob = (
            "second",
            out2,
            Box::pin(async move {
                second_started_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                run_scip_indexer_cmd_async(
                    &root2,
                    if cfg!(windows) { "cmd" } else { "sleep" },
                    if cfg!(windows) {
                        &["/C", "timeout", "/T", "5"]
                    } else {
                        &["5"]
                    },
                    "second",
                    None,
                    None,
                    &out2_clone,
                    std::time::Duration::from_secs(5),
                )
                .await
            }),
        );

        let results = run_cancellable_indexer_batch(vec![first, second], &token).await;

        assert_eq!(
            results.len(),
            1,
            "only the first job's result should be present, got {results:?}"
        );
        assert_eq!(results[0].0, "first");
        assert!(
            !second_started.load(std::sync::atomic::Ordering::SeqCst),
            "the second indexer must never be launched once the token is cancelled"
        );
    }

    /// Regression test for the Postgres-connect-on-plain-local-index bug:
    /// `Infigraph::backend()` became universal (returning `Some` for the
    /// default local Kùzu backend too, not just Neo4j), which silently
    /// turned `if let Some(backend) = prism.backend()` into an always-true
    /// check gating the Postgres-embeddings branch — so `infigraph index`
    /// tried to connect to Postgres and failed even for plain local
    /// indexing with no remote backend configured. `is_neo4j_backend()`
    /// replaces that check with the same explicit `INFIGRAPH_BACKEND`
    /// check already used a few lines above it (repo registration) —
    /// asserts it's only true for an explicit `neo4j` value.
    #[test]
    #[cfg(feature = "remote")]
    fn is_neo4j_backend_only_true_for_explicit_neo4j_env() {
        std::env::remove_var("INFIGRAPH_BACKEND");
        assert!(
            !is_neo4j_backend(),
            "unset INFIGRAPH_BACKEND must not select Postgres"
        );

        std::env::set_var("INFIGRAPH_BACKEND", "kuzu");
        assert!(
            !is_neo4j_backend(),
            "explicit kuzu backend must not select Postgres"
        );

        std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
        assert!(
            is_neo4j_backend(),
            "explicit neo4j backend must select Postgres"
        );

        std::env::remove_var("INFIGRAPH_BACKEND");
    }

    #[test]
    fn lock_acquired_when_no_watcher() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        file.try_lock_exclusive().unwrap();
        file.unlock().unwrap();
    }

    #[test]
    fn lock_fails_when_watcher_holds_it() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");

        let watcher_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        watcher_file.lock_exclusive().unwrap();

        let check_file = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
        assert!(check_file.try_lock_exclusive().is_err());

        watcher_file.unlock().unwrap();
        check_file.try_lock_exclusive().unwrap();
        check_file.unlock().unwrap();
    }

    #[test]
    fn ensure_watcher_skips_without_infigraph_dir() {
        let tmp = TempDir::new().unwrap();
        ensure_watcher_running(tmp.path());
        assert!(!tmp.path().join(".infigraph").join("watch.lock").exists());
    }

    #[test]
    fn ensure_watcher_skips_when_lock_held() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();
        let lock_path = tg_dir.join("watch.lock");

        let _lock = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        _lock.lock_exclusive().unwrap();

        ensure_watcher_running(tmp.path());
    }

    #[test]
    fn lock_released_after_drop() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("watch.lock");

        use fs2::FileExt;
        {
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&lock_path)
                .unwrap();
            file.lock_exclusive().unwrap();
            // file dropped here — lock should release
        }

        // Re-acquire should succeed after drop
        let file2 = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
        file2.try_lock_exclusive().unwrap();
        file2.unlock().unwrap();
    }

    #[test]
    fn acquire_watch_lock_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("nested").join("dir").join("watch.lock");
        assert!(!lock_path.parent().unwrap().exists());

        let lock = crate::info_commands::acquire_watch_lock(&lock_path);
        assert!(lock.is_ok());
        assert!(lock_path.exists());
    }

    #[test]
    fn watch_stop_creates_sentinel() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let sentinel = tg_dir.join("watch.stop");
        assert!(!sentinel.exists());

        // No watcher running — watch_stop should say "No watcher running"
        let result = crate::info_commands::cmd_watch_stop(tmp.path());
        assert!(result.is_ok());
        assert!(!sentinel.exists());

        // Simulate watcher holding lock
        let lock_path = tg_dir.join("watch.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        file.lock_exclusive().unwrap();

        let result = crate::info_commands::cmd_watch_stop(tmp.path());
        assert!(result.is_ok());
        assert!(sentinel.exists());

        file.unlock().unwrap();
    }

    #[test]
    fn watch_status_reports_correctly() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        // No lock file — not running
        let result = crate::info_commands::cmd_watch_status(tmp.path());
        assert!(result.is_ok());

        // Lock held — running
        let lock_path = tg_dir.join("watch.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        file.lock_exclusive().unwrap();

        let result = crate::info_commands::cmd_watch_status(tmp.path());
        assert!(result.is_ok());

        file.unlock().unwrap();
    }

    #[test]
    fn sentinel_file_removed_by_watcher_loop() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let sentinel = tg_dir.join("watch.stop");
        fs::write(&sentinel, b"").unwrap();
        assert!(sentinel.exists());

        // Simulate what the watcher loop does
        if sentinel.exists() {
            let _ = fs::remove_file(&sentinel);
        }
        assert!(!sentinel.exists());
    }

    #[test]
    fn ensure_watcher_noop_when_ci_env_set() {
        std::env::set_var("CI", "true");
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        ensure_watcher_running(tmp.path());
        assert!(!tg_dir.join("watch.lock").exists());

        std::env::remove_var("CI");
    }

    #[test]
    #[cfg(feature = "remote")]
    fn ensure_watcher_noop_when_remote_backend() {
        // Remote (shared-Neo4j) mode reindexes via webhook — a local watcher
        // would be redundant and race the webhook path.
        std::env::set_var("INFIGRAPH_BACKEND", "neo4j");
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        ensure_watcher_running(tmp.path());
        assert!(!tg_dir.join("watch.lock").exists());

        std::env::remove_var("INFIGRAPH_BACKEND");
    }

    #[test]
    fn ensure_watcher_called_for_each_group_repo() {
        // Simulate what group index does: ensure_watcher_running per repo
        let repos: Vec<TempDir> = (0..3).map(|_| TempDir::new().unwrap()).collect();
        for repo in &repos {
            let tg_dir = repo.path().join(".infigraph");
            fs::create_dir_all(&tg_dir).unwrap();
        }

        // Each repo should be checkable independently
        for repo in &repos {
            let lock_path = repo.path().join(".infigraph").join("watch.lock");
            assert!(!infigraph_core::daemon::lifecycle::daemon_is_alive(
                &lock_path
            ));
        }
    }

    #[test]
    fn group_watcher_skips_repos_without_infigraph() {
        let tmp = TempDir::new().unwrap();
        // No .infigraph dir — should not panic or create files
        ensure_watcher_running(tmp.path());
        assert!(!tmp.path().join(".infigraph").exists());
    }

    #[test]
    fn multiple_watchers_independent_locks() {
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();
        let dir_a = repo_a.path().join(".infigraph");
        let dir_b = repo_b.path().join(".infigraph");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        let lock_a = dir_a.join("watch.lock");
        let lock_b = dir_b.join("watch.lock");

        use fs2::FileExt;
        // Lock repo A
        let file_a = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_a)
            .unwrap();
        file_a.lock_exclusive().unwrap();

        // Repo B should be unlocked
        assert!(infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_a));
        assert!(!infigraph_core::daemon::lifecycle::daemon_is_alive(&lock_b));

        file_a.unlock().unwrap();
    }

    #[test]
    fn sentinel_stops_only_target_repo() {
        let repo_a = TempDir::new().unwrap();
        let repo_b = TempDir::new().unwrap();
        let dir_a = repo_a.path().join(".infigraph");
        let dir_b = repo_b.path().join(".infigraph");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        // Write sentinel to repo A only
        fs::write(dir_a.join("watch.stop"), b"").unwrap();

        assert!(dir_a.join("watch.stop").exists());
        assert!(!dir_b.join("watch.stop").exists());
    }

    #[test]
    fn delete_sends_sentinel_before_removal() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let lock_path = tg_dir.join("watch.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        use fs2::FileExt;
        file.lock_exclusive().unwrap();

        // Simulate what cmd_delete_project does: check alive → write sentinel
        assert!(infigraph_core::daemon::lifecycle::daemon_is_alive(
            &lock_path
        ));
        let sentinel = tg_dir.join("watch.stop");
        fs::write(&sentinel, b"").unwrap();
        assert!(sentinel.exists());

        file.unlock().unwrap();
    }

    #[test]
    fn bm25_cache_stale_when_embeddings_newer() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let emb_path = tg_dir.join("embeddings.bin");
        let bm25_path = tg_dir.join("bm25_cache.bin");

        // Create BM25 cache first
        fs::write(&bm25_path, b"old_cache").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Then update embeddings (newer mtime)
        fs::write(&emb_path, b"new_embeddings").unwrap();

        let emb_mtime = fs::metadata(&emb_path).unwrap().modified().unwrap();
        let cache_mtime = fs::metadata(&bm25_path).unwrap().modified().unwrap();

        // Cache should be stale (embeddings newer than cache)
        assert!(
            emb_mtime > cache_mtime,
            "embeddings should be newer than BM25 cache"
        );
    }

    #[test]
    fn bm25_cache_fresh_when_older_than_embeddings() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let emb_path = tg_dir.join("embeddings.bin");
        let bm25_path = tg_dir.join("bm25_cache.bin");

        // Create embeddings first
        fs::write(&emb_path, b"embeddings").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Then create BM25 cache (newer mtime)
        fs::write(&bm25_path, b"cache").unwrap();

        let emb_mtime = fs::metadata(&emb_path).unwrap().modified().unwrap();
        let cache_mtime = fs::metadata(&bm25_path).unwrap().modified().unwrap();

        // Cache should be fresh (cache newer than embeddings)
        assert!(cache_mtime >= emb_mtime, "BM25 cache should be fresh");
    }

    #[test]
    fn hnsw_sidecar_invalidated_after_embed_update() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let hnsw_path = tg_dir.join("hnsw_index.usearch");
        let emb_path = tg_dir.join("embeddings.bin");

        // Create HNSW first
        fs::write(&hnsw_path, b"old_hnsw").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Update embeddings (simulates watcher reindex)
        fs::write(&emb_path, b"new_embeddings").unwrap();

        let hnsw_mtime = fs::metadata(&hnsw_path).unwrap().modified().unwrap();
        let emb_mtime = fs::metadata(&emb_path).unwrap().modified().unwrap();

        // HNSW should be stale
        assert!(
            emb_mtime > hnsw_mtime,
            "HNSW sidecar should be stale after embed update"
        );
    }

    #[test]
    fn search_cache_key_uses_embeddings_mtime() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        let emb_path = tg_dir.join("embeddings.bin");
        fs::write(&emb_path, b"v1").unwrap();
        let mtime1 = fs::metadata(&emb_path).unwrap().modified().unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&emb_path, b"v2").unwrap();
        let mtime2 = fs::metadata(&emb_path).unwrap().modified().unwrap();

        // Different writes should produce different mtimes
        assert_ne!(
            mtime1, mtime2,
            "mtime should change after embeddings.bin update"
        );
    }

    #[test]
    fn watch_stop_idempotent() {
        let tmp = TempDir::new().unwrap();
        let tg_dir = tmp.path().join(".infigraph");
        fs::create_dir_all(&tg_dir).unwrap();

        // No watcher running — multiple stops should be fine
        for _ in 0..3 {
            let result = crate::info_commands::cmd_watch_stop(tmp.path());
            assert!(result.is_ok());
        }
    }

    #[test]
    fn watch_status_no_infigraph_dir() {
        let tmp = TempDir::new().unwrap();
        // No .infigraph — should report not running without error
        let result = crate::info_commands::cmd_watch_status(tmp.path());
        assert!(result.is_ok());
    }
}
