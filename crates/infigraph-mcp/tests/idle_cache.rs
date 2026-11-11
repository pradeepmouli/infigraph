//! A connected client must not pin its search context indefinitely.

use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

mod support;

#[test]
fn connected_worker_evicts_idle_search_data_and_serves_the_next_search() {
    support::disable_background_watchers();
    let project = support::TestProject::with_files(&[(
        "cache_fixture.py",
        "def idle_cache_probe(value):\n    return value + 1\n",
    )]);
    let cli = infigraph_mcp::tools::helpers::find_infigraph_cli().unwrap();
    let index = Command::new(cli)
        .args(["index", "--no-embed"])
        .current_dir(project.path())
        .env(infigraph_core::BACKEND_ENV, infigraph_core::LOCAL_BACKEND)
        .output()
        .unwrap();
    assert!(index.status.success(), "index: {index:?}");

    let mut server = support::start_supervisor(&[("INFIGRAPH_CACHE_IDLE_SECS", "2")]);
    let search = |id| {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "search", "arguments": {
                "path": project.path(), "query": "idle_cache_probe", "scope": "code", "limit": 5
            }
        }})
    };
    let assert_found = |reply: Value| {
        assert!(reply.get("error").is_none(), "{reply}");
        assert_ne!(reply["result"]["isError"], true, "{reply}");
        assert!(reply.to_string().contains("cache_fixture.py"), "{reply}");
    };
    server.send(search(1));
    assert_found(server.reply(1, Duration::from_secs(60)));

    let log = server._tmp.path().join("mcp.log");
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("idle search/vector caches") {
            break;
        }
        assert!(
            Instant::now() < until,
            "idle cache was not released: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(server.child.try_wait().unwrap().is_none());
    server.send(search(2));
    assert_found(server.reply(2, Duration::from_secs(60)));
}
