//! The seen-dedup placeholder says "use detail=true to force full output". A
//! caller that does exactly that must get the full text, never the placeholder
//! again, whatever the earlier calls were.

use infigraph_mcp::compress::compress_pipeline_safe;
use infigraph_mcp::session_context::{dedup_state_path, reset_session, SESSION_TEST_LOCK};
use serde_json::json;

/// A listing long enough to clear the dedup's 50-token floor.
fn listing() -> String {
    (0..40)
        .map(|i| {
            format!(
                "Function pad{i:<2}  L{}-{}  id=closure.ts::pad{i}\n",
                i + 1,
                i + 1
            )
        })
        .collect()
}

/// `dedup_state.json` persists into the nearest `.infigraph/` of the cwd every
/// fifth call (#136): start clean and leave nothing behind, even on a failure.
struct CleanDedupState(Option<std::path::PathBuf>);

impl CleanDedupState {
    fn clear(&self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

impl Drop for CleanDedupState {
    fn drop(&mut self) {
        self.clear();
        reset_session();
    }
}

fn is_placeholder(text: &str) -> bool {
    text.starts_with("(seen")
}

#[test]
fn the_placeholders_own_advice_gets_the_full_output() {
    let _session = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_DEDUP", "1");
    let state = CleanDedupState(dedup_state_path());
    state.clear();
    reset_session();

    let raw = listing();
    let plain = json!({"path": "/p", "file": "closure.ts"});
    let detail = json!({"path": "/p", "file": "closure.ts", "detail": true});

    let first = compress_pipeline_safe(&raw, "get_symbols_in_file", &plain);
    assert!(!is_placeholder(&first), "{first}");
    let second = compress_pipeline_safe(&raw, "get_symbols_in_file", &plain);
    assert!(
        is_placeholder(&second) && second.contains("use detail=true"),
        "the repeat is the placeholder: {second}"
    );

    // The advice, taken literally: the same call with detail=true.
    let third = compress_pipeline_safe(&raw, "get_symbols_in_file", &detail);
    assert!(
        !is_placeholder(&third),
        "detail=true got the placeholder: {third}"
    );
    assert!(third.contains("pad39"), "not the full listing: {third}");

    // And again: it is never the placeholder's answer, however often it is asked.
    let fourth = compress_pipeline_safe(&raw, "get_symbols_in_file", &detail);
    assert!(!is_placeholder(&fourth), "{fourth}");
}

#[test]
fn for_edit_is_never_answered_with_the_placeholder() {
    let _session = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_DEDUP", "1");
    let state = CleanDedupState(dedup_state_path());
    state.clear();
    reset_session();

    let raw = listing();
    let plain = json!({"path": "/p", "file": "closure.ts"});
    let edit = json!({"path": "/p", "file": "closure.ts", "for_edit": true});
    compress_pipeline_safe(&raw, "get_symbols_in_file", &plain);
    compress_pipeline_safe(&raw, "get_symbols_in_file", &plain);
    let out = compress_pipeline_safe(&raw, "get_symbols_in_file", &edit);
    assert!(!is_placeholder(&out), "{out}");
}

/// The full call refreshes what the session has seen: the next plain call for
/// the same content is the placeholder again, pointing at a call that has the text.
#[test]
fn a_full_call_refreshes_the_seen_entry() {
    let _session = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_DEDUP", "1");
    let state = CleanDedupState(dedup_state_path());
    state.clear();
    reset_session();

    let raw = listing();
    let plain = json!({"path": "/p", "file": "closure.ts"});
    let detail = json!({"path": "/p", "file": "closure.ts", "detail": true});
    // First seen with detail=true: nothing is placeholder-able yet, and the
    // entry is recorded.
    let first = compress_pipeline_safe(&raw, "get_symbols_in_file", &detail);
    assert!(!is_placeholder(&first));
    let next = compress_pipeline_safe(&raw, "get_symbols_in_file", &plain);
    assert!(is_placeholder(&next), "{next}");
}

/// Changed content is shown in full whether or not detail is asked for.
#[test]
fn changed_content_is_shown_in_full() {
    let _session = SESSION_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("INFIGRAPH_DEDUP", "1");
    let state = CleanDedupState(dedup_state_path());
    state.clear();
    reset_session();

    let plain = json!({"path": "/p", "file": "closure.ts"});
    compress_pipeline_safe(&listing(), "get_symbols_in_file", &plain);
    let changed = format!(
        "{}Method sightings  L3-3  id=closure.ts::Seam::sightings\n",
        listing()
    );
    let out = compress_pipeline_safe(&changed, "get_symbols_in_file", &plain);
    assert!(!is_placeholder(&out), "{out}");
}
