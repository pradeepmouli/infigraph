//! Search data is shared by independent socket clients, observes writes,
//! expires while connected, and releases when the last client/read leaves.

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use infigraph_core::daemon::{
    liveness::Liveness, read_endpoint::ReadEndpoint, read_protocol::write_attach,
    read_service::ReadService,
};
use infigraph_core::graph::GraphStore;
use infigraph_core::search::service::{self, CacheKey, SearchMode, SearchRequest};

#[test]
fn shared_search_reuses_data_observes_writes_and_releases_unused_state() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path();
    std::fs::create_dir_all(root.join(".infigraph")).unwrap();
    std::fs::write(root.join("app.py"), "def cache_probe():\n    pass\n").unwrap();
    let store = Arc::new(
        GraphStore::open(
            &root
                .join(".infigraph")
                .join(infigraph_core::graph::names::STEM),
        )
        .unwrap(),
    );
    let create = |name: &str| {
        let conn = store.connection().unwrap();
        conn.query(&format!(
        "CREATE (:Symbol {{id: '{name}', name: '{name}', kind: 'function', file: 'app.py', docstring: '', start_line: 1, end_line: 2}})" )).unwrap();
    };
    create("cache_probe");
    let liveness = Arc::new(Liveness::new());
    let source_store = store.clone();
    let server = ReadService::start_serving(
        root,
        Arc::new(move || Some(source_store.clone())),
        None,
        4,
        liveness.clone(),
        None,
    )
    .unwrap();
    let mut lease = ReadEndpoint::for_root(root).connect().unwrap();
    write_attach(&mut lease, std::process::id()).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while liveness.leases() != 1 {
        assert!(Instant::now() < until, "lease did not attach");
        std::thread::sleep(Duration::from_millis(10));
    }
    service::drop_cache();
    // The production client helper also owns a process lease. Give this
    // fixture's automatic lease a short release period, so both client
    // connections can end without waiting the production 30 minutes.
    infigraph_core::daemon::lease::set_release_after_for_test(root, Duration::from_millis(10));
    let request = SearchRequest {
        mode: SearchMode::Symbols,
        query: "cache_probe".into(),
        limit: 5,
        kind: None,
        file_pattern: None,
        regex: false,
    };
    let barrier = Arc::new(Barrier::new(2));
    let clients: Vec<_> = (0..2)
        .map(|_| {
            let root = root.to_path_buf();
            let barrier = barrier.clone();
            let request = request.clone();
            std::thread::spawn(move || {
                barrier.wait();
                infigraph_core::daemon::search::symbols(&root, request).unwrap()
            })
        })
        .collect();
    let mut replies = clients.into_iter().map(|h| h.join().unwrap());
    let first = replies.next().unwrap();
    let second = replies.next().unwrap();
    assert_eq!(
        first.cache_generation, second.cache_generation,
        "two clients must reuse one build"
    );
    assert_eq!(first.text, second.text);
    assert!(first.text.contains("cache_probe"));
    let key = CacheKey::local(
        root,
        Arc::as_ptr(&store) as usize,
        store.current_ast_generation().unwrap(),
    );
    let local = service::execute(key, &request, || {
        panic!("warm shared cache must not query the graph again")
    })
    .unwrap();
    assert_eq!(
        local.text, first.text,
        "local and routed execution share rendering"
    );

    create("new_after_cache");
    let updated = infigraph_core::daemon::search::symbols(
        root,
        SearchRequest {
            query: "new_after_cache".into(),
            ..request.clone()
        },
    )
    .unwrap();
    assert!(
        updated.text.contains("new_after_cache"),
        "a committed write must invalidate cached rows"
    );
    assert!(updated.cache_generation > first.cache_generation);

    std::thread::sleep(Duration::from_millis(5));
    assert!(
        service::evict_idle(Duration::from_millis(1)),
        "idle data expires while a client stays attached"
    );
    let rebuilt = infigraph_core::daemon::search::symbols(root, request.clone()).unwrap();
    assert!(rebuilt.cache_generation > updated.cache_generation);
    drop(lease);
    let until = Instant::now() + Duration::from_secs(5);
    while liveness.leases() != 0 {
        assert!(Instant::now() < until, "lease did not detach");
        std::thread::sleep(Duration::from_millis(10));
    }
    let active = liveness.begin_read();
    assert_eq!(
        infigraph_core::daemon::search::maintain_caches(&liveness, Duration::from_nanos(1)),
        0,
        "an active read protects the cache after disconnect"
    );
    drop(active);
    assert!(
        infigraph_core::daemon::search::maintain_caches(&liveness, Duration::from_secs(300)) > 0,
        "last-client release must clear data before the idle TTL"
    );
    let after_disconnect = infigraph_core::daemon::search::symbols(root, request).unwrap();
    assert!(after_disconnect.cache_generation > rebuilt.cache_generation);
    assert!(after_disconnect.text.contains("cache_probe"));
    server.shutdown();
}
