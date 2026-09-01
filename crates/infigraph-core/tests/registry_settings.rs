use std::path::PathBuf;
use std::sync::Mutex;

// INFIGRAPH_REGISTRY_* / legacy INFIGRAPH_ORG are process-wide; serialize
// tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const VARS: [&str; 4] = [
    "INFIGRAPH_REGISTRY_HOME",
    "INFIGRAPH_REGISTRY_INSTANCES_DIR",
    "INFIGRAPH_REGISTRY_ORG",
    "INFIGRAPH_ORG",
];

fn clear() {
    for v in VARS {
        std::env::remove_var(v);
    }
}

#[test]
fn instances_dir_reads_the_renamed_var_and_falls_back_to_home() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_REGISTRY_INSTANCES_DIR", "/tmp/ig-instances");
    assert_eq!(
        infigraph_core::instances::instances_dir(),
        PathBuf::from("/tmp/ig-instances")
    );
    clear();
    let fallback = infigraph_core::instances::instances_dir();
    assert!(
        fallback.ends_with(".infigraph/instances"),
        "unset must fall back to $HOME/.infigraph/instances, got {}",
        fallback.display()
    );
}

#[test]
fn registry_path_honors_registry_home() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    std::env::set_var("INFIGRAPH_REGISTRY_HOME", "/tmp/ig-home");
    assert_eq!(
        infigraph_core::multi::registry_path().unwrap(),
        PathBuf::from("/tmp/ig-home/.infigraph/registry.json")
    );
    clear();
}

#[test]
fn default_org_legacy_name_wins_over_canonical() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    assert_eq!(infigraph_core::multi::default_org(), "");
    std::env::set_var("INFIGRAPH_REGISTRY_ORG", "canonical-org");
    assert_eq!(infigraph_core::multi::default_org(), "canonical-org");
    std::env::set_var("INFIGRAPH_ORG", "legacy-org");
    assert_eq!(
        infigraph_core::multi::default_org(),
        "legacy-org",
        "legacy name must win"
    );
    clear();
}
