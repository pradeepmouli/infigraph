use anyhow::{Context, Result};
use serde_json::Value;

use infigraph_core::embed;

use super::docs::{auto_start_doc_watch_opportunistic as auto_start_doc_watch, open_doc_index};
use super::helpers::{find_infigraph_cli, open_prism};
use super::watch::auto_start_watch_opportunistic as auto_start_watch;

#[cfg(feature = "remote")]
fn is_remote_mode() -> bool {
    std::env::var("INFIGRAPH_BACKEND")
        .map(|v| v == "neo4j")
        .unwrap_or(false)
}

/// Anchored empirically: this repo's own reindex takes ~10s cold, so 3 minutes
/// comfortably covers real large-repo indexing while still catching a hang far
/// short of the hour-plus incidents this session's own corrupted-graph events
/// caused (an `infigraph index` subprocess stuck at ~100% CPU with a bogus ~8.4TB
/// VSZ, blocking its MCP caller indefinitely with zero feedback).
const INDEX_SUBPROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Outcome of a bounded-wait subprocess run.
enum RunOutcome {
    Completed { success: bool, output: String },
    TimedOut,
}

/// Run `cmd` to completion, killing it if it doesn't finish within `timeout`.
///
/// Uses `try_wait()` polling rather than the blocking `.output()`/`.wait()` so a hung
/// child (e.g. `infigraph index` stuck opening a corrupted graph DB) can be killed
/// instead of blocking this thread forever. stdout/stderr are drained continuously on
/// background threads while polling -- `Stdio::piped()` without draining would let the
/// child block on a full pipe buffer during a long, verbose index run.
fn run_with_timeout(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Result<RunOutcome> {
    use std::io::Read;

    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().context("failed to spawn subprocess")?;

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(50);
    let status = loop {
        match child
            .try_wait()
            .context("failed to poll subprocess status")?
        {
            Some(status) => break Some(status),
            None => {
                if start.elapsed() >= timeout {
                    break None;
                }
                std::thread::sleep(poll_interval);
            }
        }
    };

    match status {
        Some(status) => {
            let stdout = stdout_thread.join().unwrap_or_default();
            let stderr = stderr_thread.join().unwrap_or_default();
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            Ok(RunOutcome::Completed {
                success: status.success(),
                output: combined,
            })
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(RunOutcome::TimedOut)
        }
    }
}

/// Run an indexing attempt with automatic recovery from a hung/corrupted graph.
///
/// `attempt(full)` should run one bounded-timeout attempt (typically wrapping
/// `run_with_timeout`) and return its outcome. The sequence is exactly three
/// attempts: as originally requested, a plain retry with the same `full` value
/// (handles transient slowness), then one escalated attempt with `full` forced to
/// `true` (two consecutive timeouts is evidence of corruption, not slowness -- and
/// since indexing's whole job is already to rebuild the graph, self-healing via a
/// full reindex here doesn't cross the same line the general wipe-on-failure
/// caution warns about for other subsystems). A `Result::Err` from `attempt` itself
/// (e.g. a genuine spawn failure) propagates immediately without retrying -- only a
/// `TimedOut` outcome triggers the retry/escalate sequence.
fn run_with_recovery(
    mut attempt: impl FnMut(bool) -> Result<RunOutcome>,
    full: bool,
    timeout: std::time::Duration,
) -> Result<(bool, String)> {
    let attempts = [full, full, true];
    for (i, &attempt_full) in attempts.iter().enumerate() {
        match attempt(attempt_full)? {
            RunOutcome::Completed { success, output } => return Ok((success, output)),
            RunOutcome::TimedOut => {
                if i == 0 {
                    crate::mcp_log(
                        "WARN",
                        &format!(
                            "infigraph index timed out after {timeout:?}, killing and retrying once"
                        ),
                    );
                } else if i == 1 {
                    crate::mcp_log(
                        "WARN",
                        "infigraph index timed out twice in a row -- escalating to --full reindex",
                    );
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "infigraph index timed out 3 times in a row (including one --full attempt) after \
         {timeout:?} each -- likely unrecoverable graph corruption. Manually inspect/remove \
         .infigraph/graph and .infigraph/graph.wal, then retry."
    ))
}

pub fn tool_index_project(args: &Value) -> Result<String> {
    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let full = args.get("full").and_then(|f| f.as_bool()).unwrap_or(false);

    if let Some(cli) = find_infigraph_cli() {
        let build_cmd = |attempt_full: bool| {
            let mut cmd = std::process::Command::new(&cli);
            cmd.arg("index").current_dir(path);
            if attempt_full {
                cmd.arg("--full");
            }
            cmd
        };

        let (success, combined) = run_with_recovery(
            |attempt_full| run_with_timeout(&mut build_cmd(attempt_full), INDEX_SUBPROCESS_TIMEOUT),
            full,
            INDEX_SUBPROCESS_TIMEOUT,
        )?;

        if !success {
            return Err(anyhow::anyhow!("infigraph index failed:\n{}", combined));
        }
        let mut out = combined;

        // Register in global registry so watchers auto-start on next MCP init
        if let Ok(prism) = open_prism(args) {
            let project_name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string());
            let mut registry = infigraph_core::multi::Registry::load().unwrap_or_default();
            let _ = registry.register_repo(&project_name, &std::path::PathBuf::from(path), &prism);
        }

        if let Some(msg) = auto_start_watch(path) {
            out.push_str(&format!("\n{}", msg));
        }
        auto_start_doc_watch(path);
        return Ok(out);
    }

    // Fallback: run inline if CLI not found
    let root = std::path::Path::new(path);
    let op = infigraph_core::ops::begin_index_op(
        root,
        "index_project (mcp)",
        std::time::Duration::ZERO,
    )?;
    let _op_guard = match op {
        infigraph_core::ops::IndexOpOutcome::Acquired(g) => g,
        o @ infigraph_core::ops::IndexOpOutcome::AlreadyRunning(_) => {
            return Ok(o.skip_note().unwrap());
        }
    };

    #[allow(unused_mut)]
    let mut prism = open_prism(args)?;
    #[cfg(feature = "remote")]
    if is_remote_mode() {
        let repo_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        prism.set_namespace(&repo_name);
    }
    let result = prism.index()?;

    let mut out = if result.indexed_files == 0 {
        format!(
            "All {} files up-to-date, nothing to reindex\n",
            result.total_files
        )
    } else {
        format!(
            "Indexed {} files ({} up-to-date, {} total)\n",
            result.indexed_files,
            result.total_files - result.indexed_files,
            result.total_files
        )
    };
    let mut by_lang: std::collections::HashMap<&str, (usize, usize)> =
        std::collections::HashMap::new();
    for ext in &result.extractions {
        let entry = by_lang.entry(&ext.language).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ext.symbols.len();
    }
    for (lang, (files, symbols)) in &by_lang {
        out.push_str(&format!(
            "  {}: {} files, {} symbols\n",
            lang, files, symbols
        ));
    }
    if result.resolve_stats.total_calls > 0 {
        out.push_str(&format!("{}\n", result.resolve_stats));
    }
    if let Some(backend) = prism.backend() {
        let root = std::path::PathBuf::from(path);
        let changed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
        #[allow(unused_mut)]
        let mut embed_done = false;
        #[cfg(feature = "remote")]
        if is_remote_mode() {
            if let Ok(pg) = infigraph_core::meta::PostgresMetaStore::connect_from_env_cached() {
                match embed::update_embeddings_remote(backend, pg, &changed) {
                    Ok(n) => out.push_str(&format!("Saved {} embeddings to pgvector\n", n)),
                    Err(e) => {
                        out.push_str(&format!("warning: remote embedding update failed: {e}\n"))
                    }
                }
                embed_done = true;
            }
        }
        if !embed_done {
            match embed::update_embeddings(backend, &root, &changed) {
                Ok(n) => out.push_str(&format!("Saved {} embeddings\n", n)),
                Err(e) => out.push_str(&format!("warning: embedding update failed: {e}\n")),
            }
        }
    }
    let stats = prism.stats()?;
    out.push_str(&format!("\n{}", stats));

    // Register in global registry so watchers auto-start on next MCP init
    {
        let project_name = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let mut registry = infigraph_core::multi::Registry::load().unwrap_or_default();
        let _ = registry.register_repo(&project_name, &std::path::PathBuf::from(path), &prism);
    }

    if let Some(msg) = auto_start_watch(path) {
        out.push_str(&format!("\n{}", msg));
    }
    // Index docs before starting the doc watcher: auto_start_doc_watch only starts
    // watching when .infigraph/docs.kuzu already exists, and this in-process fallback
    // (unlike the CLI-subprocess path above, whose `infigraph index` invocation already
    // indexes docs) never created it otherwise -- making the call below a silent no-op.
    match open_doc_index(args).and_then(|idx| idx.index()) {
        Ok(doc_result) => out.push_str(&format!(
            "Document indexing complete.\n  Files scanned: {}\n  Files indexed: {}\n  Chunks created: {}\n",
            doc_result.total_files, doc_result.indexed_files, doc_result.total_chunks
        )),
        Err(e) => out.push_str(&format!("warning: doc indexing failed: {e}\n")),
    }
    auto_start_doc_watch(path);
    if let Err(e) = infigraph_core::claude_md::ensure_project_claude_md(std::path::Path::new(path))
    {
        out.push_str(&format!(
            "\nwarning: failed to update project CLAUDE.md: {e}"
        ));
    }
    Ok(out)
}

pub fn tool_get_dependencies(args: &Value) -> Result<String> {
    let prism = open_prism(args)?;
    let backend = prism.backend().context("not initialized")?;
    let eco_filter = args.get("ecosystem").and_then(|v| v.as_str());

    let mut deps = infigraph_core::manifest::query_deps(backend)?;
    if let Some(eco) = eco_filter {
        deps.retain(|d| d.ecosystem == eco);
    }

    if deps.is_empty() {
        return Ok("No dependencies found. Run index_manifests first.".to_string());
    }

    let mut out = format!("Dependencies ({}):\n\n", deps.len());
    let mut cur_eco = String::new();
    for d in &deps {
        if d.ecosystem != cur_eco {
            out.push_str(&format!("## {} \n", d.ecosystem));
            cur_eco = d.ecosystem.clone();
        }
        let dev_tag = if d.is_dev { " [dev]" } else { "" };
        out.push_str(&format!("  {}@{}{}\n", d.name, d.version, dev_tag));
    }
    Ok(out)
}

pub fn tool_scip_import(args: &Value) -> Result<String> {
    let prism = open_prism(args)?;
    let root = prism.root().to_path_buf();
    let backend = prism.backend().context("not initialized")?;

    let index_rel = args
        .get("index")
        .and_then(|v| v.as_str())
        .unwrap_or("index.scip");
    let index_path = if std::path::Path::new(index_rel).is_absolute() {
        std::path::PathBuf::from(index_rel)
    } else {
        root.join(index_rel)
    };

    let stats = backend.import_scip_index(&index_path, Some(&root))?;
    let mut out = format!(
        "SCIP import complete:\n  files processed: {}\n  symbols added: {}\n  symbols enriched: {}\n  relations added: {}\n  references added: {}\n  corrections learned: {}",
        stats.files_processed,
        stats.symbols_added,
        stats.symbols_enriched,
        stats.relations_added,
        stats.references_added,
        stats.corrections_learned,
    );
    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    if let Some(msg) = auto_start_watch(path) {
        out.push_str(&format!("\n{}", msg));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_completes_fast_command_successfully() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hello; echo world 1>&2"]);
        let outcome = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        match outcome {
            RunOutcome::Completed { success, output } => {
                assert!(success);
                assert!(output.contains("hello"), "missing stdout: {output}");
                assert!(output.contains("world"), "missing stderr: {output}");
            }
            RunOutcome::TimedOut => panic!("expected completion, got TimedOut"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_reports_nonzero_exit_as_not_success() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exit 1"]);
        let outcome = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        match outcome {
            RunOutcome::Completed { success, .. } => assert!(!success),
            RunOutcome::TimedOut => panic!("expected completion, got TimedOut"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn run_with_timeout_kills_and_reports_timeout_for_a_hung_command() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        let start = std::time::Instant::now();
        let outcome = run_with_timeout(&mut cmd, Duration::from_millis(200)).unwrap();
        let elapsed = start.elapsed();
        assert!(matches!(outcome, RunOutcome::TimedOut));
        // Must return close to the timeout, not wait for the full 30s sleep --
        // proves the child was actually killed, not merely abandoned.
        assert!(
            elapsed < Duration::from_secs(5),
            "took {elapsed:?}, should have returned shortly after the 200ms timeout"
        );
    }

    #[test]
    fn run_with_recovery_returns_immediately_on_first_success() {
        let mut calls: Vec<bool> = Vec::new();
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                Ok(RunOutcome::Completed {
                    success: true,
                    output: "ok".to_string(),
                })
            },
            false,
            Duration::from_secs(1),
        );
        assert!(result.is_ok());
        let (success, output) = result.unwrap();
        assert!(success);
        assert_eq!(output, "ok");
        assert_eq!(calls, vec![false], "only one attempt should have run");
    }

    #[test]
    fn run_with_recovery_retries_once_with_same_flags_before_escalating() {
        let mut calls: Vec<bool> = Vec::new();
        let mut call_count = 0;
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                call_count += 1;
                if call_count == 1 {
                    Ok(RunOutcome::TimedOut)
                } else {
                    Ok(RunOutcome::Completed {
                        success: true,
                        output: "recovered".to_string(),
                    })
                }
            },
            false,
            Duration::from_millis(1),
        );
        assert!(result.is_ok());
        let (success, output) = result.unwrap();
        assert!(success);
        assert_eq!(output, "recovered");
        assert_eq!(
            calls,
            vec![false, false],
            "second attempt (the plain retry) must use the same `full` value as the first, not escalate yet"
        );
    }

    #[test]
    fn run_with_recovery_escalates_to_full_after_two_timeouts() {
        let mut calls: Vec<bool> = Vec::new();
        let mut call_count = 0;
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                call_count += 1;
                if call_count <= 2 {
                    Ok(RunOutcome::TimedOut)
                } else {
                    Ok(RunOutcome::Completed {
                        success: true,
                        output: "healed by full reindex".to_string(),
                    })
                }
            },
            false,
            Duration::from_millis(1),
        );
        assert!(result.is_ok());
        let (success, _) = result.unwrap();
        assert!(success);
        assert_eq!(
            calls,
            vec![false, false, true],
            "third attempt must be escalated to full=true regardless of the originally requested value"
        );
    }

    #[test]
    fn run_with_recovery_escalates_even_when_full_was_already_requested() {
        // If the caller already asked for --full, attempt 3 stays full=true (no
        // meaningful distinction to escalate further into), and the sequence is
        // still exactly 3 attempts, not fewer or more.
        let mut calls: Vec<bool> = Vec::new();
        let result = run_with_recovery(
            |full| {
                calls.push(full);
                Ok(RunOutcome::TimedOut)
            },
            true,
            Duration::from_millis(1),
        );
        assert!(result.is_err());
        assert_eq!(calls, vec![true, true, true]);
    }

    #[test]
    fn run_with_recovery_gives_up_with_actionable_error_after_three_timeouts() {
        let result = run_with_recovery(
            |_full| Ok(RunOutcome::TimedOut),
            false,
            Duration::from_millis(1),
        );
        let err = result.expect_err("three consecutive timeouts must return Err");
        let msg = err.to_string();
        assert!(
            msg.contains("3 times") || msg.contains("three"),
            "error should mention the attempt count: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("graph") || msg.to_lowercase().contains("corrupt"),
            "error should point at the graph/corruption as the likely cause: {msg}"
        );
    }

    #[test]
    fn run_with_recovery_propagates_a_spawn_error_immediately_without_retrying() {
        let mut calls = 0;
        let result = run_with_recovery(
            |_full| {
                calls += 1;
                Err(anyhow::anyhow!("failed to spawn subprocess: no such file"))
            },
            false,
            Duration::from_secs(1),
        );
        assert!(result.is_err());
        assert_eq!(
            calls, 1,
            "a genuine spawn error (not a timeout) must not trigger the retry/escalate loop"
        );
    }
}
