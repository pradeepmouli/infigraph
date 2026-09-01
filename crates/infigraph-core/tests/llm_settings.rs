use std::sync::Mutex;

// INFIGRAPH_LLM_* are process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const VARS: [&str; 4] = [
    "INFIGRAPH_LLM_MODEL",
    "INFIGRAPH_LLM_BASE_URL",
    "INFIGRAPH_LLM_MAX_TOKENS",
    "INFIGRAPH_LLM_EXTRACT",
];

fn clear() {
    for v in VARS {
        std::env::remove_var(v);
    }
}

#[test]
fn llm_defaults_match_the_pre_migration_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    let l = infigraph_core::review::llm::llm_settings();
    assert_eq!(l.model, "claude-sonnet-4-20250514");
    assert_eq!(l.base_url, "https://api.anthropic.com");
    assert_eq!(l.max_tokens, 16384);
    assert!(!l.extract.0, "LLM extraction is opt-in");
}

#[test]
fn llm_vars_already_fit_the_convention() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_LLM_MODEL", "some-model");
    std::env::set_var("INFIGRAPH_LLM_MAX_TOKENS", "42");
    let l = infigraph_core::review::llm::llm_settings();
    assert_eq!(l.model, "some-model");
    assert_eq!(l.max_tokens, 42);
    clear();
}

#[test]
fn llm_extract_uses_permissive_truthy_parsing() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_LLM_EXTRACT", "1");
    assert!(infigraph_core::review::llm::llm_settings().extract.0);
    // Approved behavior change: "0"/"false" now mean off (any value used to
    // enable it).
    std::env::set_var("INFIGRAPH_LLM_EXTRACT", "0");
    assert!(!infigraph_core::review::llm::llm_settings().extract.0);
    clear();
}
