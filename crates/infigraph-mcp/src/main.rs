use std::io::{self, BufRead, Write};

use anyhow::Result;
use fs2::FileExt;
use serde_json::{json, Value};

use infigraph_mcp::tools;
use infigraph_mcp::tools::docs::{auto_start_doc_watch, init_doc_watchers};
use infigraph_mcp::tools::watch::{auto_start_watch, init_watchers};
use infigraph_mcp::web;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--worker") {
        return run_worker();
    }

    // Supervisor mode: spawn self as --worker, monitor for segfault, auto-reindex
    loop {
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--worker");
        for arg in args.iter().skip(1).filter(|a| *a != "--worker") {
            cmd.arg(arg);
        }
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
                    "SIGSEGV detected — triggering auto-reindex of registered projects",
                );
                eprintln!("infigraph-mcp: crash detected (SIGSEGV), auto-reindexing...");
                auto_reindex_all();
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
                        &format!("Crash detected (exit {code}) — triggering auto-reindex"),
                    );
                    eprintln!("infigraph-mcp: crash detected, auto-reindexing...");
                    auto_reindex_all();
                    continue;
                }
            }
        }

        std::process::exit(status.code().unwrap_or(1));
    }
}

fn auto_reindex_all() {
    let cli = find_infigraph_cli_for_reindex();
    let cli_path = match cli {
        Some(p) => p,
        None => {
            mcp_log("ERROR", "Cannot find infigraph CLI for auto-reindex");
            return;
        }
    };

    let registry = match infigraph_core::multi::Registry::load() {
        Ok(r) => r,
        Err(e) => {
            mcp_log(
                "ERROR",
                &format!("Registry load failed during reindex: {e}"),
            );
            return;
        }
    };

    // Reindex individual projects
    for entry in registry.repos.values() {
        let path = &entry.path;
        if !path.join(".infigraph").exists() {
            continue;
        }
        reindex_path(&cli_path, path);
    }

    // Reindex group combined graphs
    let groups_dir = std::env::var("HOME")
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".infigraph")
                .join("groups")
        })
        .ok();
    if let Some(ref gd) = groups_dir {
        if let Ok(entries) = std::fs::read_dir(gd) {
            for entry in entries.flatten() {
                let group_path = entry.path();
                if group_path.join(".infigraph").exists() {
                    reindex_path(&cli_path, &group_path);
                }
            }
        }
    }
}

fn reindex_path(cli_path: &std::path::Path, path: &std::path::Path) {
    let path_str = path.to_string_lossy().to_string();
    mcp_log("INFO", &format!("Auto-reindexing: {path_str}"));

    let graph_path = path.join(".infigraph").join("graph");
    if graph_path.exists() {
        let _ = std::fs::remove_file(&graph_path);
        let _ = std::fs::remove_dir_all(&graph_path);
    }
    let wal_path = path.join(".infigraph").join("graph.wal");
    let _ = std::fs::remove_file(&wal_path);

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
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
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

fn log_file_path() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".infigraph")
        .join("mcp.log")
}

fn mcp_log(level: &str, msg: &str) {
    use std::io::Write;
    let path = log_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{ts}] {level}: {msg}");
    }
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

fn acquire_instance_lock() -> Option<std::fs::File> {
    let lock_path = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".infigraph")
        .join("mcp.lock");
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            mcp_log("INFO", "Acquired MCP instance lock");
            Some(file)
        }
        Err(_) => {
            mcp_log(
                "WARN",
                "Another MCP instance holds the lock — running without watchers",
            );
            None
        }
    }
}

fn run() -> Result<()> {
    let instance_lock = acquire_instance_lock();
    let is_primary = instance_lock.is_some();
    let _lock_guard = instance_lock;

    if !is_primary {
        infigraph_mcp::tools::watch::disable_watchers();
    }

    let args: Vec<String> = std::env::args().collect();
    let ui_enabled = args
        .iter()
        .any(|a| a == "--ui" || a.starts_with("--ui=") || a == "--mcp");
    let port: u16 = args
        .iter()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port="))
        .and_then(|p| p.parse().ok())
        .unwrap_or(9749);

    let mcp_mode = args.iter().any(|a| a == "--mcp");

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

    // If UI mode is active, keep process alive after stdin EOF (web server still serving)
    if ui_enabled {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
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
    mcp_log("INFO", &format!("initialize called (primary={is_primary})"));
    if is_primary {
        std::thread::spawn(|| {
            mcp_log("DEBUG", "init_watchers start");
            init_watchers();
            mcp_log("DEBUG", "init_doc_watchers start");
            init_doc_watchers();

            let registry = match infigraph_core::multi::Registry::load() {
                Ok(r) => {
                    mcp_log(
                        "DEBUG",
                        &format!("registry loaded: {} repos", r.repos.len()),
                    );
                    r
                }
                Err(e) => {
                    mcp_log("ERROR", &format!("registry load failed: {e}"));
                    return;
                }
            };

            for entry in registry.repos.values() {
                let path = entry.path.to_string_lossy().to_string();
                if !entry.path.join(".infigraph").exists() {
                    continue;
                }
                auto_start_watch(&path);
                auto_start_doc_watch(&path);
            }
        });
    } else {
        mcp_log("INFO", "Skipping watchers — not primary instance");
    }

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "infigraph",
                "version": "0.1.0"
            }
        }
    })
}

fn handle_tools_list(id: &Value) -> Value {
    let tools = infigraph_mcp::build_tools_list();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": tools
        }
    })
}

fn estimate_tokens(text: &str) -> usize {
    // Calibrated heuristic: split on whitespace + punctuation boundaries.
    // Code ~1.5x words, prose ~1.3x. Use 1.4 as middle ground for mixed output.
    let words = text.split_whitespace().count();
    ((words as f64) * 1.4).ceil() as usize
}

fn handle_tools_call(id: &Value, request: &Value) -> Value {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    tools::helpers::log_activity(tool_name, &args);

    let metrics_enabled = std::env::var("INFIGRAPH_METRICS").is_ok_and(|v| v == "1");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        infigraph_mcp::dispatch_tool(tool_name, &args)
    }));

    match result {
        Ok(Ok(content)) => {
            let compressed =
                infigraph_mcp::compress::compress_tool_output(&content, tool_name, &args);
            if metrics_enabled {
                let raw_tokens = estimate_tokens(&content);
                let comp_tokens = estimate_tokens(&compressed);
                let ratio = if raw_tokens > 0 {
                    comp_tokens as f64 / raw_tokens as f64
                } else {
                    1.0
                };
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let args_summary = args
                    .get("query")
                    .or_else(|| args.get("symbol"))
                    .or_else(|| args.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let detail = args
                    .get("detail")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let line = json!({
                    "tool": tool_name,
                    "timestamp": ts,
                    "raw_tokens": raw_tokens,
                    "compressed_tokens": comp_tokens,
                    "compression_ratio": (ratio * 100.0).round() / 100.0,
                    "detail_requested": detail,
                    "args_summary": args_summary,
                });
                if let Some(dir) = args
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|p| std::path::PathBuf::from(p).join(".infigraph"))
                    .or_else(|| {
                        std::env::var("HOME")
                            .ok()
                            .map(|h| std::path::PathBuf::from(h).join(".infigraph"))
                    })
                {
                    let _ = std::fs::create_dir_all(&dir);
                    let path = dir.join("compression_metrics.jsonl");
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                    {
                        use std::io::Write;
                        let _ = writeln!(f, "{}", line);
                    }
                }
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": compressed }]
                }
            })
        }
        Ok(Err(e)) => {
            mcp_log("ERROR", &format!("tool={tool_name} err={e}"));
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": format!("Error: {e}") }],
                    "isError": true
                }
            })
        }
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            mcp_log("ERROR", &format!("tool={tool_name} PANIC: {msg}"));
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": format!("Error: panic in {tool_name}: {msg}") }],
                    "isError": true
                }
            })
        }
    }
}
