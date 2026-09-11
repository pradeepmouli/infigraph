use std::path::PathBuf;
use std::sync::Mutex;

// INFIGRAPH_REGISTRY_* / legacy INFIGRAPH_ORG are process-wide; serialize
// tests that set them.
static ENV_LOCK: Mutex<()> = Mutex::new(());

const VARS: [&str; 5] = [
    "INFIGRAPH_REGISTRY_HOME",
    "INFIGRAPH_REGISTRY_INSTANCES_DIR",
    "INFIGRAPH_REGISTRY_ORG",
    "INFIGRAPH_ORG",
    // Set by cargo (`.cargo/config.toml`); `restore` puts it back.
    "INFIGRAPH_REGISTRY_REAL_HOME_REDIRECT",
];

/// The environment this process started with, captured before any test
/// mutates it.
fn baseline() -> &'static [(&'static str, Option<std::ffi::OsString>)] {
    static BASELINE: std::sync::OnceLock<Vec<(&'static str, Option<std::ffi::OsString>)>> =
        std::sync::OnceLock::new();
    BASELINE.get_or_init(|| VARS.iter().map(|v| (*v, std::env::var_os(v))).collect())
}

/// Unset every variable under test.
fn clear() {
    baseline();
    for v in VARS {
        std::env::remove_var(v);
    }
}

/// Put back what the process started with, rather than stripping it: a
/// developer's own `INFIGRAPH_REGISTRY_HOME` must survive these tests.
fn restore() {
    for (v, value) in baseline() {
        match value {
            Some(value) => std::env::set_var(v, value),
            None => std::env::remove_var(v),
        }
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
    // And follows the registry home when only that is set.
    std::env::set_var("INFIGRAPH_REGISTRY_HOME", "/tmp/ig-home");
    assert_eq!(
        infigraph_core::instances::instances_dir(),
        PathBuf::from("/tmp/ig-home/.infigraph/instances")
    );
    restore();
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
    restore();
}

/// The guard for the whole class: under cargo, nothing may resolve the
/// project registry or instance files into the developer's real home. A
/// test that saved an in-memory `Registry` overwrote the real one with its
/// fixtures and deregistered every project on the machine. If this fails,
/// `.cargo/config.toml`'s `[env]` is not reaching test processes.
#[test]
fn tests_never_resolve_the_real_registry() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    restore();
    // Resolved the way `infigraph_home` resolves an unset override: `HOME`
    // is not always set on Windows.
    let real = std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(dirs_next::home_dir)
        .expect("a home directory")
        .join(".infigraph");
    for path in [
        infigraph_core::multi::registry_path().unwrap(),
        infigraph_core::instances::instances_dir(),
    ] {
        assert!(
            !path.starts_with(&real),
            "a test process resolved {} -- inside the real {}",
            path.display(),
            real.display()
        );
    }
}

/// The redirect guards only the real home. A test that isolates itself by
/// pointing `HOME` at a tempdir -- and the CLI children it spawns with that
/// `HOME` -- must find the registry there, not in cargo's scratch directory:
/// a redirect that outranked `HOME` broke exactly those tests.
#[test]
fn a_home_pointed_elsewhere_is_not_redirected() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clear();
    let fake_home = tempfile::tempdir().unwrap();
    let orig_home = std::env::var_os("HOME");
    std::env::set_var("INFIGRAPH_REGISTRY_REAL_HOME_REDIRECT", "/tmp/ig-redirect");
    std::env::set_var("HOME", fake_home.path());
    let resolved = infigraph_core::multi::registry_path().unwrap();
    match orig_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
    restore();
    assert_eq!(
        resolved,
        fake_home.path().join(".infigraph").join("registry.json")
    );
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
    restore();
}
