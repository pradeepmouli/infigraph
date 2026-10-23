//! An argument a tool's advertised schema does not list must not vanish
//! silently: a `project=<worktree>` sent to `search` (whose parameter is
//! `path`) was ignored and answered from the server's own project.
//!
//! Tier 1: a near-miss of `path` with no `path` is an error naming the valid
//! keys. Tier 2: any other unlisted key is ignored but reported in the
//! response footer.

use infigraph_mcp::tools::helpers::{check_args, DISPATCH_KEYS};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn text_of(response: &Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

#[test]
fn a_near_miss_of_path_without_path_is_an_error_naming_the_valid_keys() {
    for key in ["project", "root", "repo"] {
        let err = check_args("search", &json!({ key: "/w", "query": "x" })).unwrap_err();
        assert!(err.contains(&format!("`{key}`")), "{err}");
        assert!(err.contains("did you mean `path`"), "{err}");
        for valid in ["path", "query", "limit", "regex"] {
            assert!(err.contains(valid), "{err} lacks {valid}");
        }
    }
}

#[test]
fn a_near_miss_beside_a_path_is_only_reported() {
    let ignored = check_args(
        "search",
        &json!({"path": "/w", "project": "/w", "query": "x"}),
    );
    assert_eq!(ignored.unwrap(), vec!["project".to_string()]);
}

#[test]
fn any_other_unlisted_key_is_reported_and_the_call_goes_on() {
    let ignored = check_args("search", &json!({"query": "x", "bogus": 1, "zzz": 2}));
    assert_eq!(
        ignored.unwrap(),
        vec!["bogus".to_string(), "zzz".to_string()]
    );
}

#[test]
fn a_tool_without_a_path_has_no_did_you_mean_path() {
    let ignored = check_args("list_languages", &json!({"project": "/w"}));
    assert_eq!(ignored.unwrap(), vec!["project".to_string()]);
}

#[test]
fn listed_keys_and_the_dispatchers_own_are_quiet() {
    let args = json!({"path": "/w", "query": "x", "limit": 3, "detail": true, "for_edit": true});
    assert!(check_args("search", &args).unwrap().is_empty());
    assert!(check_args("search", &json!({})).unwrap().is_empty());
    assert!(check_args("search", &Value::Null).unwrap().is_empty());
}

#[test]
fn an_unknown_tool_is_left_to_the_dispatcher() {
    assert!(check_args("no_such_tool", &json!({"x": 1}))
        .unwrap()
        .is_empty());
}

#[test]
fn a_tools_call_with_a_near_miss_is_refused_before_anything_runs() {
    let request = json!({"params": {"name": "search",
        "arguments": {"project": "/some/worktree", "query": "x"}}});
    let response = infigraph_mcp::handle_tools_call(&json!(1), &request);
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert!(
        text_of(&response).contains("did you mean `path`"),
        "{response}"
    );
}

#[test]
fn a_tools_call_with_an_unlisted_key_says_so_in_the_footer() {
    let request = json!({"params": {"name": "list_languages",
        "arguments": {"bogus": 1}}});
    let response = infigraph_mcp::handle_tools_call(&json!(1), &request);
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text = text_of(&response);
    assert!(text.contains("ignored unknown argument"), "{text}");
    assert!(text.contains("bogus"), "{text}");
}

/// Every key a handler reads must be in its tool's schema (or be one the
/// dispatcher reads for every tool), or a client sending it is told it was
/// ignored. Scans the handlers' source, so a new `args.get("k")` that the
/// schema does not advertise fails here rather than in a client.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The literal keys read as `args.get("k")` / `args["k"]` in `text`.
fn keys_read(text: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for needle in ["args.get(\"", "args[\"", "args\n            .get(\""] {
        let mut rest = text;
        while let Some(at) = rest.find(needle) {
            rest = &rest[at + needle.len()..];
            if let Some(end) = rest.find('"') {
                keys.insert(rest[..end].to_string());
            }
        }
    }
    keys
}

#[test]
fn every_key_a_handler_reads_is_advertised() {
    let tools = infigraph_mcp::build_tools_list();
    let schema_keys = |tool: &str| -> BTreeSet<String> {
        tools
            .iter()
            .find(|t| t["name"] == tool)
            .map(|t| {
                t["inputSchema"]["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    };
    let everywhere: BTreeSet<String> = tools
        .iter()
        .flat_map(|t| schema_keys(t["name"].as_str().unwrap()))
        .chain(DISPATCH_KEYS.iter().map(|k| k.to_string()))
        .collect();

    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut missing = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        // Unit-test modules build their own argument objects.
        let source = source.split("#[cfg(test)]").next().unwrap();
        // Per handler: `fn tool_<name>` up to the next top-level fn.
        let mut handlers = 0;
        for chunk in source.split("\nfn ").chain(source.split("\npub fn ")) {
            let Some(name) = chunk
                .strip_prefix("tool_")
                .and_then(|r| r.split(['(', '<']).next())
            else {
                continue;
            };
            let Some(tool) = tools.iter().find(|t| t["name"] == name) else {
                continue;
            };
            handlers += 1;
            let allowed = schema_keys(tool["name"].as_str().unwrap());
            for key in keys_read(chunk) {
                if !allowed.contains(&key) && !DISPATCH_KEYS.contains(&key.as_str()) {
                    missing.push(format!("{name} reads `{key}` ({})", file.display()));
                }
            }
        }
        // Anything else (shared helpers, dispatcher): some tool advertises it.
        for key in keys_read(source) {
            if !everywhere.contains(&key) {
                missing.push(format!("`{key}` is read in {}", file.display()));
            }
        }
        let _ = handlers;
    }
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "keys read but not advertised:\n{}",
        missing.join("\n")
    );
}
