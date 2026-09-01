use clap::Parser;
use std::sync::Mutex;

// INFIGRAPH_GRAPH_* vars are process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn resolve_graph() -> infigraph_core::graph::Graph {
    let cli = infigraph_core::graph::RawGraph::parse_from(std::iter::empty::<String>());
    infigraph_core::graph::Graph::resolve(cli, None)
}

#[test]
fn graph_group_defaults_match_the_pre_migration_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for var in [
        "INFIGRAPH_GRAPH_GROWTH_MAX_RATIO",
        "INFIGRAPH_GRAPH_QUARANTINE_MAX_BYTES",
        "INFIGRAPH_GRAPH_SLOW_LOCK_MS",
        "INFIGRAPH_GRAPH_DOC_HNSW_THRESHOLD",
    ] {
        std::env::remove_var(var);
    }
    let g = resolve_graph();
    assert_eq!(g.growth_max_ratio, 10);
    assert_eq!(g.quarantine_max_bytes, 1024 * 1024 * 1024);
    assert_eq!(g.slow_lock_ms, 2000);
    assert_eq!(g.doc_hnsw_threshold, 200_000);
}

#[test]
fn growth_max_ratio_reads_its_env_var() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_GRAPH_GROWTH_MAX_RATIO", "3");
    assert_eq!(resolve_graph().growth_max_ratio, 3);
    std::env::remove_var("INFIGRAPH_GRAPH_GROWTH_MAX_RATIO");
}
