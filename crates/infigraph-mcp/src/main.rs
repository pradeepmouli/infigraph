use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use infigraph_mcp::web;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--worker") {
        return run_worker();
    }

    // Supervisor mode: spawn self as --worker, monitor for segfault, auto-reindex.
    // Remember the repo we were launched in: it's the primary recovery target
    // even when the global registry is empty (standalone use).
    let startup_dir = std::env::current_dir().ok();
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
        for arg in args.iter().skip(1).filter(|a| *a != "--worker") {
            cmd.arg(arg);
        }
        // Let the worker detect supervisor death and exit instead of
        // lingering as an orphan holding the instance lock.
        cmd.env(
            infigraph_mcp::lifecycle::SUPERVISOR_PID_ENV,
            std::process::id().to_string(),
        );
        cmd.stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());

        let status = cmd.status()?;

        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.signal() == Some(11) {
                // SIGSEGV — likely corrupt DB, reindex all registered projects
                mcp_log(
                    "CRASH",
                    "SIGSEGV detected — triggering auto-reindex of registered projects (code + docs)",
                );
                eprintln!("infigraph-mcp: crash detected (SIGSEGV), auto-reindexing code+docs...");
                auto_reindex_all(startup_dir.as_deref());
                // Respawn worker after reindex
                continue;
            }
        }

        #[cfg(windows)]
        {
            if let Some(code) = status.code() {
                if code < 0 {
                    // Negative exit code on Windows = unhandled exception (e.g. access violation)
                    mcp_log(
                        "CRASH",
                        &format!(
                            "Crash detected (exit {code}) — triggering auto-reindex of code+docs"
                        ),
                    );
                    eprintln!("infigraph-mcp: crash detected, auto-reindexing code+docs...");
                    auto_reindex_all(startup_dir.as_deref());
                    continue;
                }
            }
        }

        std::process::exit(status.code().unwrap_or(1));
    }
}

fn auto_reindex_all(startup_dir: Option<&std::path::Path>) {
    let cli = find_infigraph_cli_for_reindex();
    let cli_path = match cli {
        Some(p) => p,
        None => {
            mcp_log("ERROR", "Cannot find infigraph CLI for auto-reindex");
            return;
        }
    };

    // Registry repos are optional extras: an empty/broken registry must not
    // prevent recovery of the repo this MCP server was launched in.
    let registry_paths: Vec<std::path::PathBuf> = match infigraph_core::multi::Registry::load() {
        Ok(r) => r.repos.values().map(|e| e.path.clone()).collect(),
        Err(e) => {
            mcp_log(
                "ERROR",
                &format!("Registry load failed during reindex: {e}"),
            );
            Vec::new()
        }
    };

    let groups_dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs_next::home_dir)
        .map(|h| h.join(".infigraph").join("groups"));

    let targets = infigraph_mcp::recovery::collect_reindex_targets(
        startup_dir,
        &registry_paths,
        groups_dir.as_deref(),
    );
    if targets.is_empty() {
        mcp_log("WARN", "Auto-reindex found no targets with .infigraph");
        return;
    }
    for path in &targets {
        reindex_path(&cli_path, path);
    }
}

/// Spawns a background thread that proactively starts watching the
/// directory this MCP server was launched in. Thin wrapper around the
/// library crate's `recovery::start_daemon_watcher_for_startup_dir`, which
/// does the actual gating (daemon mode + `[watch].auto_start_on_boot`) --
/// kept there rather than here so it's reachable from `infigraph-mcp`'s
/// integration tests, since this `main.rs` is a separate `[[bin]]` target.
/// Runs on its own thread so this doesn't delay this server's readiness to
/// serve the MCP client's `initialize` handshake.
fn start_daemon_watcher_for_startup_dir(startup_dir: Option<&std::path::Path>) {
    let startup_dir = startup_dir.map(|p| p.to_path_buf());
    std::thread::spawn(move || {
        infigraph_mcp::recovery::start_daemon_watcher_for_startup_dir(startup_dir.as_deref());
    });
}

fn reindex_path(cli_path: &std::path::Path, path: &std::path::Path) {
    let path_str = path.to_string_lossy().to_string();
    mcp_log("INFO", &format!("Auto-reindexing: {path_str}"));

    if let Err(e) = infigraph_mcp::recovery::wipe_code_and_docs(path) {
        mcp_log("ERROR", &format!("Reindex wipe skipped: {path_str}: {e:#}"));
        return;
    }

    let result = std::process::Command::new(cli_path)
        .arg("index")
        .current_dir(path)
        .status();
    match result {
        Ok(s) if s.success() => mcp_log("INFO", &format!("Reindex OK: {path_str}")),
        Ok(s) => mcp_log(
            "ERROR",
            &format!("Reindex failed (exit {:?}): {path_str}", s.code()),
        ),
        Err(e) => mcp_log("ERROR", &format!("Reindex spawn failed: {e}")),
    }
}

fn find_infigraph_cli_for_reindex() -> Option<std::path::PathBuf> {
    let bin_name = if cfg!(windows) {
        "infigraph.exe"
    } else {
        "infigraph"
    };
    // Check next to current exe
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent()?.join(bin_name);
        if sibling.exists() {
            return Some(sibling);
        }
    }
    // Check common install locations
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs_next::home_dir);
    if let Some(ref h) = home {
        let local_bin = h.join(".local").join("bin").join(bin_name);
        if local_bin.exists() {
            return Some(local_bin);
        }
    }
    None
}

fn run_worker() -> Result<()> {
    install_panic_hook();

    // Exit if the supervisor dies, instead of surviving as an orphan
    // (PPID 1) that holds the instance lock forever. Stdin EOF alone is
    // not sufficient: --ui/--serve modes never read stdin.
    infigraph_mcp::lifecycle::spawn_parent_monitor();

    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(32 * 1024 * 1024)
        .build_global();

    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(run)
        .expect("failed to spawn MCP worker thread")
        .join()
        .expect("MCP worker thread panicked")
}

fn mcp_log(level: &str, msg: &str) {
    infigraph_mcp::mcp_log(level, msg);
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let bt = std::backtrace::Backtrace::force_capture();
        mcp_log("PANIC", &format!("{payload} at {location}\n{bt}"));
        eprintln!("PANIC: {payload} at {location}");
    }));
}

fn ui_enabled_from(args: &[String]) -> bool {
    args.iter().any(|a| a == "--ui" || a.starts_with("--ui="))
}

fn run() -> Result<()> {
    let mcp_lock_outcome = infigraph_mcp::mcp_lock::acquire_with_takeover();
    let (is_primary, mcp_lock) = match mcp_lock_outcome {
        infigraph_mcp::mcp_lock::AcquireOutcome::Primary(lock) => {
            mcp_log("INFO", "Acquired mcp.lock — running as primary");
            (true, Some(lock))
        }
        infigraph_mcp::mcp_lock::AcquireOutcome::Secondary => {
            mcp_log(
                "WARN",
                "Another MCP instance holds mcp.lock — running without watchers",
            );
            infigraph_mcp::tools::watch::disable_watchers();
            (false, None)
        }
    };

    // `start_daemon_watcher_for_startup_dir` (and the library function it
    // wraps) gate on daemon mode + the `[watch].auto_start_on_boot` config
    // toggle internally -- `is_primary` is the one precondition that belongs
    // here, since it's derived from the mcp.lock outcome above, not from
    // config the library function can read for itself.
    if is_primary {
        let startup_dir = std::env::current_dir().ok();
        start_daemon_watcher_for_startup_dir(startup_dir.as_deref());
    }

    if let Some(mut lock) = mcp_lock {
        std::thread::spawn(move || loop {
            std::thread::sleep(infigraph_mcp::mcp_lock::heartbeat_interval());
            if infigraph_mcp::mcp_lock::heartbeat_and_check_handover(&mut lock) {
                drop(lock);
                std::process::exit(0);
            }
        });
    }

    let args: Vec<String> = std::env::args().collect();
    let mcp_mode = args.iter().any(|a| a == "--mcp");
    let transport = if mcp_mode { "stdio" } else { "http" };
    let project_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let instance_info = infigraph_core::instances::InstanceInfo::current(&project_path, transport);
    let _instance_guard = match infigraph_core::instances::register_instance(&instance_info) {
        Ok(guard) => Some(guard),
        Err(e) => {
            mcp_log("WARN", &format!("Failed to register instance: {e:#}"));
            None
        }
    };

    let reaped = infigraph_core::instances::reap_orphans_once(std::process::id());
    if reaped > 0 {
        mcp_log(
            "INFO",
            &format!("Reaped {reaped} orphaned instance(s) on startup"),
        );
    }

    std::thread::spawn(|| loop {
        std::thread::sleep(infigraph_core::instances::reap_scan_interval());
        let reaped = infigraph_core::instances::reap_orphans_once(std::process::id());
        if reaped > 0 {
            mcp_log(
                "INFO",
                &format!("Reaped {reaped} orphaned instance(s) (periodic scan)"),
            );
        }
    });

    let ui_enabled = ui_enabled_from(&args);
    let port: u16 = args
        .iter()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(9749);

    let serve_mode = args.iter().any(|a| a == "--serve");
    let not_ready = args.iter().any(|a| a == "--not-ready");
    if not_ready {
        web::set_ready(false);
    }
    let mcp_port: u16 = args
        .iter()
        .find(|a| a.starts_with("--mcp-port="))
        .and_then(|a| a.strip_prefix("--mcp-port="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(8642);
    let health_path: String = args
        .iter()
        .find(|a| a.starts_with("--health-path="))
        .and_then(|a| a.strip_prefix("--health-path="))
        .unwrap_or("/health")
        .to_string();

    if ui_enabled {
        if web::start_ui_server(port) {
            eprintln!("Infigraph UI running at http://localhost:{}", port);
            eprintln!("Open: http://localhost:{}/?path=/your/project", port);
        } else {
            eprintln!(
                "Infigraph UI port {} already in use — skipping UI (MCP active)",
                port
            );
        }
        if !mcp_mode && !serve_mode {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    if serve_mode {
        if web::start_mcp_http_server(mcp_port, is_primary, &health_path) {
            eprintln!("Infigraph MCP HTTP server at http://0.0.0.0:{}", mcp_port);
        } else {
            eprintln!("Infigraph MCP HTTP port {} already in use", mcp_port);
        }
        if !mcp_mode {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    mcp_log("INFO", "MCP server started");

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                mcp_log("INFO", &format!("stdin closed: {e}"));
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_response(
                    &stdout,
                    json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    }),
                )?;
                continue;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");

        mcp_log("DEBUG", &format!("method={method}"));

        let response = match method {
            "initialize" => handle_initialize(&id, is_primary),
            "tools/list" => handle_tools_list(&id),
            "tools/call" => {
                let tool = request
                    .pointer("/params/name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("?");
                mcp_log("DEBUG", &format!("tool_call={tool}"));
                handle_tools_call(&id, &request)
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        };

        write_response(&stdout, response)?;
    }

    mcp_log("INFO", "stdin loop exited");

    // Reaching here means the MCP client's stdio connection closed — the
    // two OTHER "loop forever" branches earlier in this function (pure
    // --ui-only, pure --serve-only, neither with --mcp) return before ever
    // entering the stdin read loop, since they never had a stdio client to
    // begin with; they are legitimate standing daemons and are untouched
    // by this block. If --ui is also active, someone might still have the
    // local web UI open, so don't exit instantly — but don't loop forever
    // either (DESIGN-hardening.md I-5 / R2.2.3): self-terminate after an
    // idle grace period.
    if ui_enabled {
        let grace = infigraph_mcp::idle::idle_grace_period();
        let poll = infigraph_mcp::idle::idle_poll_interval();
        mcp_log(
            "INFO",
            &format!(
                "MCP client disconnected; UI still serving — exiting after {}s idle unless reconnected",
                grace.as_secs()
            ),
        );
        let stdin_closed_at = std::time::Instant::now();
        loop {
            std::thread::sleep(poll);
            if infigraph_mcp::idle::should_exit_idle(stdin_closed_at.elapsed(), grace) {
                mcp_log(
                    "INFO",
                    &format!(
                        "Idle grace period ({}s) elapsed since MCP client disconnected — exiting",
                        grace.as_secs()
                    ),
                );
                break;
            }
        }
    }

    Ok(())
}

fn write_response(stdout: &io::Stdout, response: Value) -> Result<()> {
    let msg = serde_json::to_string(&response)?;
    let mut out = stdout.lock();
    out.write_all(msg.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}

fn handle_initialize(id: &Value, is_primary: bool) -> Value {
    infigraph_mcp::handle_initialize(id, is_primary)
}

fn handle_tools_list(id: &Value) -> Value {
    infigraph_mcp::handle_tools_list(id)
}

fn handle_tools_call(id: &Value, request: &Value) -> Value {
    infigraph_mcp::handle_tools_call(id, request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_mcp_flag_does_not_enable_ui() {
        // Regression test: `--mcp` alone previously triggered the UI keep-alive
        // loop, leaking a worker process that never exits on stdin EOF.
        assert!(!ui_enabled_from(&args(&["--mcp"])));
    }

    #[test]
    fn no_flags_does_not_enable_ui() {
        assert!(!ui_enabled_from(&args(&[])));
    }

    #[test]
    fn ui_flag_enables_ui() {
        assert!(ui_enabled_from(&args(&["--ui"])));
    }

    #[test]
    fn ui_flag_with_value_enables_ui() {
        assert!(ui_enabled_from(&args(&["--ui=3000"])));
    }

    #[test]
    fn mcp_and_ui_together_enables_ui() {
        assert!(ui_enabled_from(&args(&["--mcp", "--ui"])));
    }
}
