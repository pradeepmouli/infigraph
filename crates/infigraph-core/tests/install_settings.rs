use std::sync::Mutex;

// INFIGRAPH_INSTALL_* / legacy INFIGRAPH_{BIN,GH_HOST,GH_OWNER} are
// process-wide; serialize tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const VARS: [&str; 7] = [
    "INFIGRAPH_INSTALL_DIR",
    "INFIGRAPH_INSTALL_BIN",
    "INFIGRAPH_INSTALL_GH_HOST",
    "INFIGRAPH_INSTALL_GH_OWNER",
    "INFIGRAPH_BIN",
    "INFIGRAPH_GH_HOST",
    "INFIGRAPH_GH_OWNER",
];

fn clear() {
    for v in VARS {
        std::env::remove_var(v);
    }
}

#[test]
fn install_defaults_match_the_pre_migration_values() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    let i = infigraph_core::install_settings();
    assert_eq!(
        i.dir, "",
        "empty dir means callers fall back to ~/.local/bin"
    );
    assert_eq!(i.bin, "/app/infigraph");
    assert_eq!(i.gh_host, "github.com");
    assert_eq!(i.gh_owner, "intuit");
}

#[test]
fn install_dir_already_fits_the_convention() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_INSTALL_DIR", "/opt/ig/bin");
    assert_eq!(infigraph_core::install_settings().dir, "/opt/ig/bin");
    clear();
}

#[test]
fn legacy_names_win_over_canonical_names() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_INSTALL_GH_HOST", "canonical.example");
    assert_eq!(
        infigraph_core::install_settings().gh_host,
        "canonical.example",
        "the canonical name works on its own"
    );
    std::env::set_var("INFIGRAPH_GH_HOST", "legacy.example");
    std::env::set_var("INFIGRAPH_BIN", "/legacy/infigraph");
    std::env::set_var("INFIGRAPH_GH_OWNER", "legacy-owner");
    let i = infigraph_core::install_settings();
    assert_eq!(i.gh_host, "legacy.example", "legacy name must win");
    assert_eq!(i.bin, "/legacy/infigraph");
    assert_eq!(i.gh_owner, "legacy-owner");
    clear();
}
