use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;

pub use super::session::{session_date_id, session_epoch};

/// Resolve a path argument to a project root containing `.infigraph/`.
/// 1. If the path itself has `.infigraph/`, use it directly.
/// 2. Walk UP from the path looking for `.infigraph/` (handles subdirectory CWD).
/// 3. Check the global registry for a project whose path starts with (or contains) this path.
/// 4. Fall back to the original path (let downstream error).
pub fn resolve_project_path(path: &str) -> String {
    let start = if path == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    };

    let resolved = infigraph_core::project::resolve_project_root(&start);

    // The registry fallback stays here: it answers "an indexed project lives
    // *below* the path I was given", which is a lookup against global state
    // rather than a property of the path itself.
    if resolved == start && !start.join(".infigraph").join("graph").exists() {
        if let Ok(registry) = infigraph_core::multi::Registry::load() {
            for entry in registry.repos.values() {
                if entry.path.starts_with(&start) {
                    return entry.path.to_string_lossy().to_string();
                }
            }
        }
    }

    resolved.to_string_lossy().to_string()
}

/// The project this MCP process belongs to: the directory it was launched
/// in, resolved once at startup to the project that owns it (#196). A
/// session started in `packages/foo/src` is a session on the repo, not on
/// that directory, so everything that asks "which project am I" -- startup
/// watching, instance registration, `doctor`'s default, a tool call with no
/// `path` or `path: "."` -- gets the same answer the CLI's `main` gives.
pub fn startup_project() -> PathBuf {
    static PROJECT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PROJECT
        .get_or_init(|| PathBuf::from(resolve_project_path(".")))
        .clone()
}

/// Tools whose `path` is required and never defaulted or resolved: each
/// acts on exactly the store it names. `delete_project` resolving "this
/// stray subdirectory store" to the project root would delete the
/// project's index.
pub const EXPLICIT_PATH_TOOLS: &[&str] = &["delete_project"];

/// Whether `tool_name`'s schema takes a `path`, from the advertised tool
/// list itself so the two cannot drift.
fn takes_path(tool_name: &str) -> bool {
    static WITH_PATH: std::sync::OnceLock<std::collections::HashSet<String>> =
        std::sync::OnceLock::new();
    WITH_PATH
        .get_or_init(|| {
            crate::build_tools_list()
                .iter()
                .filter(|t| t["inputSchema"]["properties"].get("path").is_some())
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect()
        })
        .contains(tool_name)
}

/// Scope a tool call to its project, once, before anything acts on it
/// (#196). Every tool's `path` names the project the call is about, but
/// agents pass whatever directory they are working in, and each helper that
/// saw the raw value decided on its own whether to resolve it. The ones that
/// did not created `.infigraph/` in the subdirectory and started a daemon
/// on it. Resolving here, at dispatch, makes a project root the only thing
/// downstream code ever receives -- and an omitted `path` means this
/// server's own project.
pub fn scope_to_project(tool_name: &str, mut args: Value) -> Value {
    if EXPLICIT_PATH_TOOLS.contains(&tool_name) || !takes_path(tool_name) {
        return args;
    }
    let resolved = match args.get("path").and_then(|p| p.as_str()) {
        Some(path) => resolve_project_path(path),
        None => startup_project().to_string_lossy().to_string(),
    };
    if args.is_null() {
        args = json!({});
    }
    if let Some(obj) = args.as_object_mut() {
        obj.insert("path".to_string(), json!(resolved));
    }
    args
}

pub fn open_prism(args: &Value) -> Result<Infigraph> {
    let raw_path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    let path = resolve_project_path(raw_path);
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(&path), registry)?;
    prism.init()?;
    apply_repo_filter(&mut prism, &path);
    Ok(prism)
}

pub fn open_prism_read_only(args: &Value) -> Result<Infigraph> {
    let raw_path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    let path = resolve_project_path(raw_path);
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(&path), registry)?;
    prism.init_read_only()?;
    apply_repo_filter(&mut prism, &path);
    Ok(prism)
}

/// Like [`open_prism_read_only`], but degrades to a pre-crash snapshot
/// instead of failing outright on a dead-holder WAL (R3.1.4b). A sibling
/// function rather than a change to `open_prism_read_only` itself, so the
/// other 47+ callers of that function are unaffected -- only the two call
/// sites that render a degrade banner (`get_code_snippet`, `search`) use
/// this one.
pub fn open_prism_read_only_or_degrade(
    args: &Value,
) -> Result<(Infigraph, Option<infigraph_core::graph::DegradeReason>)> {
    let raw_path = args
        .get("path")
        .and_then(|p| p.as_str())
        .context("missing 'path' argument")?;
    let path = resolve_project_path(raw_path);
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(&PathBuf::from(&path), registry)?;
    let reason = prism.init_read_only_or_degrade()?;
    apply_repo_filter(&mut prism, &path);
    Ok((prism, reason))
}

/// Banner for a response that served a degraded read (R3.1.4b), for
/// `crate::banner::prepend` -- more severe than `search.rs`'s
/// `staleness_banner` (that one warns about a few files lagging the index;
/// this one means the whole graph is a pre-crash snapshot), so callers that
/// compose both prepend this one last, putting it first.
pub fn degrade_banner(reason: &infigraph_core::graph::DegradeReason) -> String {
    match reason {
        infigraph_core::graph::DegradeReason::PreCrashSnapshot { snapshot_path, .. } => format!(
            "serving results from a pre-crash snapshot ({}) -- a WAL corruption was just \
             detected and an automatic rebuild has been triggered in the background; results \
             may lag recent changes until it completes",
            snapshot_path.display()
        ),
    }
}

/// In Neo4j (remote) mode, scope read queries to the repo matching this path.
/// Read and write MUST agree on the `org/repo` key or repo-scoped queries return
/// nothing (files/symbols show 0 while global folders/contains stay populated).
///
/// The group registry is the source of truth for a repo's `org/repo` identity, so
/// resolve from it first. Only fall back to deriving from `INFIGRAPH_ORG` + directory
/// name when the path isn't registered — that fallback is guaranteed to match the
/// write key only when the env org equals the group's org, which is exactly why the
/// registry lookup is preferred.
#[cfg(feature = "remote")]
fn apply_repo_filter(prism: &mut Infigraph, raw_path: &str) {
    if !infigraph_core::daemon::lifecycle::is_remote_backend() {
        return;
    }
    let path = std::path::Path::new(raw_path);
    if let Ok(reg) = infigraph_core::multi::Registry::load() {
        if let Some(ns) = reg.resolve_repo_namespace(path) {
            prism.set_repo_filter(&ns);
            return;
        }
    }
    let repo_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| raw_path.to_string());
    let org = infigraph_core::multi::default_org();
    let key = if org.is_empty() {
        repo_name
    } else {
        format!("{org}/{repo_name}")
    };
    prism.set_repo_filter(&key);
}

#[cfg(not(feature = "remote"))]
fn apply_repo_filter(_prism: &mut Infigraph, _raw_path: &str) {}

/// The `infigraph` CLI this server spawns for indexing: a sibling of this
/// executable (or one directory up, the `cargo test` layout -- the same
/// resolution the daemon spawn path uses, `resolve_cli_binary_sibling_of`),
/// falling back to whatever `infigraph` is on PATH. Whichever is found is
/// checked against this process's own build once (#141): a stale binary
/// here writes a graph on another lbug storage version, which this server
/// then cannot read.
pub fn find_infigraph_cli() -> Option<std::path::PathBuf> {
    let found = std::env::current_exe()
        .ok()
        .and_then(|exe| infigraph_core::daemon::lifecycle::resolve_cli_binary_sibling_of(&exe).ok())
        .or_else(find_infigraph_cli_on_path)?;
    infigraph_core::daemon::warn_if_cli_build_differs(&found);
    Some(found)
}

fn find_infigraph_cli_on_path() -> Option<std::path::PathBuf> {
    let which_cmd = if cfg!(windows) { "where" } else { "which" };
    if let Ok(out) = std::process::Command::new(which_cmd)
        .arg("infigraph")
        .output()
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(std::path::PathBuf::from(path));
            }
        }
    }
    None
}

/// The id of the narrowest interval in `file` that contains `line`.
///
/// Narrowest, not first: symbols nest (a method inside its class inside its
/// file's module), and the first match in row order is usually the outermost
/// one, which says nothing about where the line actually is (#167).
pub fn find_containing_symbol<'id>(
    intervals: &[(&str, usize, usize, &'id str)],
    file: &str,
    line: usize,
) -> Option<&'id str> {
    intervals
        .iter()
        .filter(|(f, start, end, _)| *f == file && *start <= line && line <= *end)
        .min_by_key(|(_, start, end, _)| end - start)
        .map(|(_, _, _, id)| *id)
}

pub fn save_analysis(path: &str, tool_name: &str, content: &str) -> Result<String> {
    let root = PathBuf::from(path);
    let dir = root.join(".infigraph").join("sessions").join("analysis");
    std::fs::create_dir_all(&dir)?;

    let date = session_date_id().replace("session_", "");
    let filename = format!("{tool_name}_{date}.md");
    let filepath = dir.join(&filename);
    std::fs::write(&filepath, content)?;

    let lines = content.lines().count();
    let summary: String = content.lines().take(5).collect::<Vec<_>>().join("\n");
    Ok(format!(
        "Saved to {}\n({} lines, {} bytes)\n\n{}",
        filepath.display(),
        lines,
        content.len(),
        summary
    ))
}

pub fn log_activity(tool_name: &str, args: &Value) {
    if matches!(
        tool_name,
        "get_latest_session"
            | "save_session"
            | "search_sessions"
            | "purge_sessions"
            | "list_projects"
    ) {
        return;
    }
    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
    if path.is_empty() {
        return;
    }
    // Record into a project's store, never create one: an activity log is
    // not a reason for a directory to become a project (#196).
    let store = PathBuf::from(path).join(".infigraph");
    if !store.is_dir() {
        return;
    }
    let sessions_dir = store.join("sessions");
    if std::fs::create_dir_all(&sessions_dir).is_err() {
        return;
    }
    let date = session_date_id().replace("session_", "");
    let log_path = sessions_dir.join(format!("activity_{date}.jsonl"));
    let ts = session_epoch();
    let mut key_args = serde_json::Map::new();
    if let Some(obj) = args.as_object() {
        for (k, v) in obj {
            if k == "path" {
                continue;
            }
            if let Some(s) = v.as_str() {
                let truncated = if s.len() > 120 { &s[..120] } else { s };
                key_args.insert(k.clone(), json!(truncated));
            }
        }
    }
    let entry = json!({"ts": ts, "tool": tool_name, "args": key_args});
    if let Ok(line) = serde_json::to_string(&entry) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn glob_matches(glob: &str, path: &str) -> bool {
    // Simple glob: * matches any sequence, ? matches one char
    let gi = glob.chars().peekable();
    let pi = path.chars().peekable();
    glob_match_inner(&gi.collect::<Vec<_>>(), &pi.collect::<Vec<_>>())
}

pub fn glob_match_inner(glob: &[char], path: &[char]) -> bool {
    match (glob.first(), path.first()) {
        (None, None) => true,
        (Some('*'), _) => {
            // ** matches path separators too; * stops at /
            let greedy = glob.first() == Some(&'*') && glob.get(1) == Some(&'*');
            if greedy {
                // try consuming 0..=n chars including /
                for i in 0..=path.len() {
                    if glob_match_inner(&glob[2..], &path[i..]) {
                        return true;
                    }
                }
                false
            } else {
                for i in 0..=path.len() {
                    if path.get(i) == Some(&'/') && i > 0 {
                        break;
                    }
                    if glob_match_inner(&glob[1..], &path[i..]) {
                        return true;
                    }
                }
                false
            }
        }
        (Some('?'), Some(_)) => glob_match_inner(&glob[1..], &path[1..]),
        (Some(g), Some(p)) if g.eq_ignore_ascii_case(p) => glob_match_inner(&glob[1..], &path[1..]),
        _ => false,
    }
}
