use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use infigraph_mcp::tools;
use infigraph_mcp::tools::docs::{auto_start_doc_watch, init_doc_watchers};
use infigraph_mcp::tools::watch::{auto_start_watch, init_watchers};
use infigraph_mcp::web;

fn main() -> Result<()> {
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

fn run() -> Result<()> {
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

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
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

        let response = match method {
            "initialize" => handle_initialize(&id),
            "tools/list" => handle_tools_list(&id),
            "tools/call" => handle_tools_call(&id, &request),
            "notifications/initialized" | "notifications/cancelled" => continue,
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        };

        write_response(&stdout, response)?;
    }

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

fn handle_initialize(id: &Value) -> Value {
    // Auto-start watchers for all registered projects
    std::thread::spawn(|| {
        init_watchers();
        init_doc_watchers();

        let registry = match infigraph_core::multi::Registry::load() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[init] Failed to load registry: {e}");
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

fn handle_tools_call(id: &Value, request: &Value) -> Value {
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    tools::helpers::log_activity(tool_name, &args);

    let result = infigraph_mcp::dispatch_tool(tool_name, &args);

    match result {
        Ok(content) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": content }]
            }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("Error: {e}") }],
                "isError": true
            }
        }),
    }
}
