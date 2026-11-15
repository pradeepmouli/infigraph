//! Which project answered: a call that names no `path` is answered for the
//! project its server started in, and when that project has linked worktrees the
//! footer says so once per period. The server cannot see where the caller is, so
//! it never says that, only which project answered and what exists.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use infigraph_mcp::health::{
    compose_footer, unnamed_root_notice, HealthState, Signals, UnnamedRoot,
    ROOT_NOTICE_REPEAT_AFTER,
};

fn root() -> PathBuf {
    PathBuf::from("/work/sittir")
}

fn view_with(n: usize, indexed: &[&str]) -> impl Fn(&Path) -> Option<UnnamedRoot> {
    let indexed: Vec<String> = indexed.iter().map(|s| s.to_string()).collect();
    move |r: &Path| {
        Some(UnnamedRoot {
            root: r.to_path_buf(),
            linked: n,
            indexed: indexed.clone(),
        })
    }
}

fn footer_for(notice: Option<UnnamedRoot>) -> Option<String> {
    compose_footer(&Signals {
        unnamed_root: notice,
        ..Default::default()
    })
}

#[test]
fn an_unnamed_call_to_a_project_with_worktrees_says_so_once_per_period() {
    let state = HealthState::new();
    let view = view_with(2, &["wt-docs", "wt-hnsw"]);
    let t0 = Instant::now();
    let r = root();
    let first = unnamed_root_notice(&state, "search", Some(&r), false, t0, &view);
    let line = footer_for(first).expect("the first call says it");
    assert!(line.contains("answered for /work/sittir"), "{line}");
    assert!(line.contains("no `path`"), "{line}");
    assert!(line.contains("2 linked worktrees"), "{line}");
    assert!(line.contains("(indexed: wt-docs, wt-hnsw)"), "{line}");
    assert!(
        line.contains("separate project reached with `path`"),
        "{line}"
    );
    // The server does not know where the caller is, and never claims to.
    assert!(!line.contains("you are"), "{line}");

    for secs in [1, 600, 1799] {
        let again = unnamed_root_notice(
            &state,
            "search",
            Some(&r),
            false,
            t0 + Duration::from_secs(secs),
            &view,
        );
        assert!(again.is_none(), "again after {secs}s");
    }
    let later = unnamed_root_notice(
        &state,
        "search",
        Some(&r),
        false,
        t0 + ROOT_NOTICE_REPEAT_AFTER,
        &view,
    );
    assert!(later.is_some(), "due again after the period");
}

#[test]
fn a_call_that_names_a_path_gets_nothing() {
    let state = HealthState::new();
    let r = root();
    let n = unnamed_root_notice(
        &state,
        "search",
        Some(&r),
        true,
        Instant::now(),
        &view_with(2, &[]),
    );
    assert!(n.is_none());
}

#[test]
fn a_project_without_linked_worktrees_gets_nothing() {
    let state = HealthState::new();
    let r = root();
    let none = |_: &Path| None::<UnnamedRoot>;
    assert!(
        unnamed_root_notice(&state, "search", Some(&r), false, Instant::now(), &none).is_none()
    );
}

/// An unknown answer (git could not list) prints nothing and fails nothing.
#[test]
fn a_call_with_no_project_gets_nothing() {
    let state = HealthState::new();
    let n = unnamed_root_notice(
        &state,
        "list_projects",
        None,
        false,
        Instant::now(),
        &view_with(2, &[]),
    );
    assert!(n.is_none());
}

#[test]
fn a_tool_that_names_its_project_is_not_told_again() {
    let state = HealthState::new();
    let r = root();
    for tool in ["get_stats", "doctor", "doc_entry"] {
        let n = unnamed_root_notice(
            &state,
            tool,
            Some(&r),
            false,
            Instant::now(),
            &view_with(2, &[]),
        );
        assert!(n.is_none(), "{tool}");
    }
}

#[test]
fn each_project_has_its_own_period() {
    let state = HealthState::new();
    let view = view_with(1, &[]);
    let t0 = Instant::now();
    let (a, b) = (PathBuf::from("/a"), PathBuf::from("/b"));
    assert!(unnamed_root_notice(&state, "search", Some(&a), false, t0, &view).is_some());
    assert!(unnamed_root_notice(&state, "search", Some(&b), false, t0, &view).is_some());
    assert!(unnamed_root_notice(&state, "search", Some(&a), false, t0, &view).is_none());
}

#[test]
fn up_to_three_indexed_names_then_a_count() {
    let state = HealthState::new();
    let r = root();
    let view = view_with(6, &["a", "b", "c", "d", "e"]);
    let n = unnamed_root_notice(&state, "search", Some(&r), false, Instant::now(), &view);
    let line = footer_for(n).unwrap();
    assert!(line.contains("(indexed: a, b, c and 2 more)"), "{line}");
    assert!(line.contains("6 linked worktrees"), "{line}");
}

#[test]
fn one_worktree_is_singular() {
    let state = HealthState::new();
    let r = root();
    let n = unnamed_root_notice(
        &state,
        "search",
        Some(&r),
        false,
        Instant::now(),
        &view_with(1, &[]),
    );
    let line = footer_for(n).unwrap();
    assert!(line.contains("1 linked worktree,"), "{line}");
    assert!(!line.contains("indexed:"), "{line}");
}
