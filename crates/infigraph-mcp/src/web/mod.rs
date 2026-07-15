mod handlers_analysis;
mod handlers_chat;
mod handlers_git;
mod handlers_symbol;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::Result;
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use tiny_http::{Header, Response, Server};

static READY: AtomicBool = AtomicBool::new(true);
static REINDEXING: AtomicBool = AtomicBool::new(false);

pub fn set_ready(val: bool) {
    READY.store(val, Ordering::SeqCst);
}

pub fn is_ready() -> bool {
    READY.load(Ordering::SeqCst)
}

use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

use handlers_analysis::*;
use handlers_chat::api_chat;
use handlers_git::api_git_summary;
use handlers_symbol::*;

/// Start the web UI server on the given port. Runs in a background thread.
pub fn start_ui_server(port: u16) -> bool {
    let addr = format!("0.0.0.0:{}", port);
    // Pre-check: try binding before spawning thread so caller knows outcome
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(_) => return false,
    };
    thread::spawn(move || {
        let server = server;

        for mut request in server.incoming_requests() {
            let url = request.url().to_string();
            let method = request.method().to_string();

            // Strip query string for route matching
            let route = url.split('?').next().unwrap_or(&url);

            let response = match (method.as_str(), route) {
                ("GET", "/") => serve_html(INDEX_HTML, "text/html"),
                ("GET", "/api/health") => serve_json(json!({"status": "ok"})),

                // API endpoints
                ("POST", "/api/index") => handle_api_post(&mut request, api_index),
                ("POST", "/api/search") => handle_api_post(&mut request, api_search),
                ("POST", "/api/query") => handle_api_post(&mut request, api_query),
                ("POST", "/api/architecture") => handle_api_post(&mut request, api_architecture),
                ("POST", "/api/dead-code") => handle_api_post(&mut request, api_dead_code),
                ("POST", "/api/symbols") => handle_api_post(&mut request, api_symbols),
                ("POST", "/api/symbol-context") => {
                    handle_api_post(&mut request, api_symbol_context)
                }
                ("POST", "/api/snippet") => handle_api_post(&mut request, api_snippet),
                ("POST", "/api/graph-data") => handle_api_post(&mut request, api_graph_data),
                ("POST", "/api/stats") => handle_api_post(&mut request, api_stats),
                ("POST", "/api/cluster") => handle_api_post(&mut request, api_cluster),
                ("POST", "/api/chat") => handle_api_post(&mut request, api_chat),
                ("POST", "/api/routes") => handle_api_post(&mut request, api_routes),
                ("POST", "/api/groups") => handle_api_post(&mut request, api_groups),
                ("POST", "/api/contracts") => handle_api_post(&mut request, api_contracts),
                ("POST", "/api/complexity") => handle_api_post(&mut request, api_complexity),
                ("POST", "/api/security") => handle_api_post(&mut request, api_security),
                ("POST", "/api/bridges") => handle_api_post(&mut request, api_bridges),
                ("POST", "/api/clones") => handle_api_post(&mut request, api_clones),
                ("POST", "/api/git-summary") => handle_api_post(&mut request, api_git_summary),

                _ => serve_html("404 Not Found", "text/plain"),
            };

            let _ = request.respond(response);
        }
    });
    true
}

pub fn start_mcp_http_server(port: u16, is_primary: bool, health_path: &str) -> bool {
    let addr = format!("0.0.0.0:{}", port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let health_path = health_path.to_string();
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let url = request.url().to_string();
            let method = request.method().to_string();
            let route = url.split('?').next().unwrap_or(&url);

            if method == "GET" && route == health_path {
                let (status, body) = if is_ready() {
                    (200, json!({"status": "ok"}))
                } else {
                    (503, json!({"status": "indexing"}))
                };
                let _ = request.respond(serve_json_status(status, body));
                continue;
            }

            if method == "POST" && route == "/ready" {
                if !check_auth(&request) {
                    let _ =
                        request.respond(serve_json_status(401, json!({"error": "Unauthorized"})));
                    continue;
                }
                set_ready(true);
                let _ = request.respond(serve_json_status(200, json!({"status": "ok"})));
                continue;
            }

            if method == "POST" && route == "/webhook/reindex" {
                let resp = handle_webhook_reindex(&mut request);
                let _ = request.respond(resp);
                continue;
            }

            if !check_auth(&request) {
                let _ = request.respond(serve_json_status(
                    401,
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": "Unauthorized"}}),
                ));
                continue;
            }

            let response = match (method.as_str(), route) {
                ("POST", "/tools/mcp") => handle_mcp_post(&mut request, is_primary),
                ("OPTIONS", _) => handle_cors_preflight(),
                _ => serve_json_status(
                    404,
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32000, "message": "Not found"}}),
                ),
            };

            let _ = request.respond(response);
        }
    });
    true
}

fn handle_mcp_post(
    request: &mut tiny_http::Request,
    is_primary: bool,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);

    let rpc: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return serve_json(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("Parse error: {e}")}
            }))
        }
    };

    let id = rpc.get("id").cloned().unwrap_or(Value::Null);
    let method = rpc.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let response = match method {
        "initialize" => crate::handle_initialize(&id, is_primary),
        "tools/list" => crate::handle_tools_list(&id),
        "tools/call" => crate::handle_tools_call(&id, &rpc),
        "notifications/initialized" | "notifications/cancelled" => {
            json!({"jsonrpc": "2.0", "id": id, "result": {}})
        }
        _ => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("Method not found: {method}")}})
        }
    };
    serve_json(response)
}

fn check_auth(request: &tiny_http::Request) -> bool {
    let key = std::env::var("INFIGRAPH_API_KEY").ok();
    match key {
        None => true,
        Some(k) => request.headers().iter().any(|h| {
            let field: &str = h.field.as_str().as_str();
            field.eq_ignore_ascii_case("authorization")
                && h.value.as_str() == format!("Bearer {}", k).as_str()
        }),
    }
}

fn handle_cors_preflight() -> Response<std::io::Cursor<Vec<u8>>> {
    let data = Vec::new();
    let h1 = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
    let h2 = Header::from_bytes("Access-Control-Allow-Methods", "POST, GET, OPTIONS").unwrap();
    let h3 = Header::from_bytes(
        "Access-Control-Allow-Headers",
        "Content-Type, Authorization, MCP-Session-Id",
    )
    .unwrap();
    Response::from_data(data)
        .with_status_code(204)
        .with_header(h1)
        .with_header(h2)
        .with_header(h3)
}

fn serve_json_status(status: u16, value: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string(&value).unwrap_or_default();
    let data = body.into_bytes();
    let ct = Header::from_bytes("Content-Type", "application/json").unwrap();
    let cors = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
    Response::from_data(data)
        .with_status_code(status)
        .with_header(ct)
        .with_header(cors)
}

fn serve_html(body: &str, content_type: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let data = body.as_bytes().to_vec();
    let header = Header::from_bytes("Content-Type", content_type).unwrap();
    Response::from_data(data).with_header(header)
}

fn serve_json(value: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_string(&value).unwrap_or_default();
    let data = body.into_bytes();
    let ct = Header::from_bytes("Content-Type", "application/json").unwrap();
    let cors = Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap();
    Response::from_data(data).with_header(ct).with_header(cors)
}

fn handle_api_post(
    request: &mut tiny_http::Request,
    handler: fn(&Value) -> Value,
) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);

    let params: Value = serde_json::from_str(&body).unwrap_or(json!({}));
    let result = handler(&params);
    serve_json(result)
}

fn handle_webhook_reindex(request: &mut tiny_http::Request) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);

    let signature = request
        .headers()
        .iter()
        .find(|h| {
            h.field
                .as_str()
                .as_str()
                .eq_ignore_ascii_case("x-hub-signature-256")
        })
        .map(|h| h.value.as_str().to_string());

    if !validate_webhook_signature(&body, signature.as_deref()) {
        return serve_json_status(401, json!({"error": "Invalid signature"}));
    }

    let event: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return serve_json_status(400, json!({"error": "Invalid JSON"})),
    };

    let repo_name = event
        .pointer("/repository/name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let ref_str = event.get("ref").and_then(|v| v.as_str()).unwrap_or("");
    let default_branch = event
        .pointer("/repository/default_branch")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    let expected_ref = format!("refs/heads/{}", default_branch);
    if ref_str != expected_ref {
        return serve_json_status(
            200,
            json!({"status": "ignored", "reason": "non-default branch"}),
        );
    }

    if REINDEXING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return serve_json_status(
            200,
            json!({"status": "queued", "reason": "reindex already in progress"}),
        );
    }

    let clone_dir = std::env::var("CLONE_DIR").unwrap_or_else(|_| "/app/data/repos".to_string());
    let group = std::env::var("GROUP_NAME").unwrap_or_else(|_| "org".to_string());
    let infigraph_bin =
        std::env::var("INFIGRAPH_BIN").unwrap_or_else(|_| "/app/infigraph".to_string());

    let repo_name_log = repo_name.clone();
    crate::mcp_log(
        "INFO",
        &format!("webhook: reindex triggered for {repo_name_log}"),
    );

    let repo_name_response = repo_name.clone();
    thread::spawn(move || {
        let repo_path = format!("{}/{}", clone_dir, repo_name);

        if std::path::Path::new(&repo_path).join(".git").exists() {
            crate::mcp_log("INFO", &format!("webhook: pulling {repo_name}"));
            let _ = Command::new("git")
                .args(["-C", &repo_path, "pull", "--ff-only"])
                .status();
        }

        crate::mcp_log("INFO", &format!("webhook: indexing {repo_name}"));
        let _ = Command::new(&infigraph_bin)
            .arg("index")
            .current_dir(&repo_path)
            .status();

        crate::mcp_log("INFO", &format!("webhook: rebuilding group {group}"));
        let _ = Command::new(&infigraph_bin)
            .args(["group", "build", &group])
            .status();

        crate::mcp_log("INFO", "webhook: reindex complete");
        REINDEXING.store(false, Ordering::SeqCst);
    });

    serve_json_status(
        200,
        json!({"status": "accepted", "repo": repo_name_response}),
    )
}

fn validate_webhook_signature(body: &str, signature: Option<&str>) -> bool {
    let secret = match std::env::var("WEBHOOK_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => return true,
    };
    let sig = match signature {
        Some(s) => s,
        None => return false,
    };
    let hex_sig = match sig.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected = match hex::decode(hex_sig) {
        Ok(b) => b,
        Err(_) => return false,
    };
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body.as_bytes());
    mac.verify_slice(&expected).is_ok()
}

pub(super) fn open_prism(params: &Value) -> Result<Infigraph> {
    let path = params.get("path").and_then(|p| p.as_str()).unwrap_or(".");
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(path), registry)?;
    prism.init()?;
    Ok(prism)
}

// The full HTML UI -- embedded as a const string
const INDEX_HTML: &str = include_str!("../ui.html");

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn http_get(port: u16, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port)).unwrap();
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            path
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    #[test]
    fn test_health_default_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = free_port();
        set_ready(true);
        assert!(start_mcp_http_server(port, false, "/health"));
        thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/health");
        assert_eq!(status, 200);
        assert!(body.contains("\"ok\""), "body: {}", body);
    }

    #[test]
    fn test_health_custom_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = free_port();
        set_ready(true);
        assert!(start_mcp_http_server(port, false, "/health/full"));
        thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/health/full");
        assert_eq!(status, 200);
        assert!(body.contains("\"ok\""), "body: {}", body);
    }

    #[test]
    fn test_health_returns_503_when_not_ready() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = free_port();
        set_ready(false);
        assert!(start_mcp_http_server(port, false, "/health"));
        thread::sleep(std::time::Duration::from_millis(100));

        let (status, body) = http_get(port, "/health");
        assert_eq!(status, 503);
        assert!(body.contains("\"indexing\""), "body: {}", body);
        set_ready(true);
    }

    #[test]
    fn test_health_wrong_path_not_matched() {
        let _guard = TEST_LOCK.lock().unwrap();
        let port = free_port();
        set_ready(true);
        assert!(start_mcp_http_server(port, false, "/health/full"));
        thread::sleep(std::time::Duration::from_millis(100));

        let (status, _) = http_get(port, "/health");
        assert_ne!(
            status, 200,
            "/health should not match when health_path is /health/full"
        );
    }
}
