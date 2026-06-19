use infigraph_mcp::tools::helpers::{
    glob_match_inner, glob_matches, session_date_id, session_epoch,
};

// ==================== glob_matches ====================

#[test]
fn test_glob_exact_match() {
    assert!(glob_matches("foo.rs", "foo.rs"));
}

#[test]
fn test_glob_no_match() {
    assert!(!glob_matches("foo.rs", "bar.rs"));
}

#[test]
fn test_glob_star_extension() {
    assert!(glob_matches("*.rs", "main.rs"));
    assert!(glob_matches("*.rs", "lib.rs"));
    assert!(!glob_matches("*.rs", "main.py"));
}

#[test]
fn test_glob_star_does_not_cross_slash() {
    assert!(!glob_matches("*.rs", "src/main.rs"));
}

#[test]
fn test_glob_double_star_crosses_slash() {
    assert!(glob_matches("**/*.rs", "src/main.rs"));
    assert!(glob_matches("**/*.rs", "src/graph/store.rs"));
}

#[test]
fn test_glob_question_mark() {
    assert!(glob_matches("?.rs", "a.rs"));
    assert!(!glob_matches("?.rs", "ab.rs"));
}

#[test]
fn test_glob_prefix_star() {
    assert!(glob_matches("src/*", "src/lib.rs"));
    assert!(!glob_matches("src/*", "tests/lib.rs"));
}

#[test]
fn test_glob_case_insensitive() {
    assert!(glob_matches("Foo.RS", "foo.rs"));
    assert!(glob_matches("*.RS", "main.rs"));
}

#[test]
fn test_glob_empty_pattern_and_path() {
    assert!(glob_matches("", ""));
    assert!(!glob_matches("", "a"));
    assert!(!glob_matches("a", ""));
}

#[test]
fn test_glob_star_matches_empty() {
    assert!(glob_matches("*", ""));
    assert!(glob_matches("*", "anything"));
}

#[test]
fn test_glob_double_star_matches_deep_path() {
    assert!(glob_matches("**/test.rs", "crates/core/tests/test.rs"));
    assert!(glob_matches("**", "a/b/c/d"));
}

// ==================== glob_match_inner ====================

#[test]
fn test_glob_match_inner_direct() {
    let glob: Vec<char> = "*.txt".chars().collect();
    let path: Vec<char> = "file.txt".chars().collect();
    assert!(glob_match_inner(&glob, &path));

    let path2: Vec<char> = "file.rs".chars().collect();
    assert!(!glob_match_inner(&glob, &path2));
}

// ==================== session_epoch ====================

#[test]
fn test_session_epoch_reasonable() {
    let epoch = session_epoch();
    assert!(epoch > 1_700_000_000, "epoch {epoch} too small");
    assert!(epoch < 2_000_000_000, "epoch {epoch} too large");
}

#[test]
fn test_session_epoch_monotonic() {
    let a = session_epoch();
    let b = session_epoch();
    assert!(b >= a);
}

// ==================== session_date_id ====================

#[test]
fn test_session_date_id_format() {
    let id = session_date_id();
    assert!(
        id.starts_with("session_"),
        "should start with 'session_': {id}"
    );
    let date_part = &id["session_".len()..];
    assert_eq!(
        date_part.len(),
        10,
        "date part should be YYYY-MM-DD: {date_part}"
    );
    assert_eq!(&date_part[4..5], "-");
    assert_eq!(&date_part[7..8], "-");
}

#[test]
fn test_session_date_id_valid_components() {
    let id = session_date_id();
    let date_part = &id["session_".len()..];
    let year: i32 = date_part[0..4].parse().expect("year should be numeric");
    let month: i32 = date_part[5..7].parse().expect("month should be numeric");
    let day: i32 = date_part[8..10].parse().expect("day should be numeric");
    assert!(year >= 2024 && year <= 2030, "year {year} out of range");
    assert!((1..=12).contains(&month), "month {month} out of range");
    assert!((1..=31).contains(&day), "day {day} out of range");
}

#[test]
fn test_session_date_id_stable_within_second() {
    let a = session_date_id();
    let b = session_date_id();
    assert_eq!(a, b, "same-second calls should produce same date id");
}
