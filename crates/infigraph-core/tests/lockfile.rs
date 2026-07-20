use infigraph_core::build_hash;

#[test]
fn test_build_hash_is_nonempty() {
    let h = build_hash();
    assert!(!h.is_empty());
    // In a git checkout this is a short sha, possibly "-dirty"; outside git it's "unknown".
    assert!(h == "unknown" || h.len() >= 7, "unexpected build hash: {h}");
}
