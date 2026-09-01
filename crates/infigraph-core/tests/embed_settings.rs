use std::sync::Mutex;

// INFIGRAPH_EMBED_MODEL_DIR / legacy INFIGRAPH_MODEL_DIR are process-wide;
// serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn clear() {
    std::env::remove_var("INFIGRAPH_MODEL_DIR");
    std::env::remove_var("INFIGRAPH_EMBED_MODEL_DIR");
}

#[test]
fn model_dir_legacy_name_wins_over_canonical() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    assert_eq!(
        infigraph_core::embed::embed_settings().model_dir,
        "",
        "empty means unset -- find_model_dir falls through"
    );
    std::env::set_var("INFIGRAPH_EMBED_MODEL_DIR", "/canonical/models");
    assert_eq!(
        infigraph_core::embed::embed_settings().model_dir,
        "/canonical/models"
    );
    std::env::set_var("INFIGRAPH_MODEL_DIR", "/legacy/models");
    assert_eq!(
        infigraph_core::embed::embed_settings().model_dir,
        "/legacy/models",
        "legacy name must win"
    );
    clear();
}
