//! The graph engine's version is written in two places that must agree: the `lbug` entry of
//! `[workspace.dependencies]` (what cargo resolves) and `[env] LBUG_VERSION` in
//! `.cargo/config.toml` (what lbug's build script downloads as its prebuilt library; unset,
//! it takes the release it calls "latest" and caches that under the key `latest`). A crate
//! that states its own `lbug` version would be a third place.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_toml(path: &Path) -> toml::Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn pinned_in_workspace() -> String {
    let manifest = read_toml(&workspace_root().join("Cargo.toml"));
    let dep = &manifest["workspace"]["dependencies"]["kuzu"];
    assert_eq!(
        dep["package"].as_str(),
        Some("lbug"),
        "kuzu must be the lbug package"
    );
    let req = dep["version"].as_str().expect("kuzu needs a version");
    req.strip_prefix("= ")
        .unwrap_or_else(|| panic!("lbug must be pinned exactly (`= x.y.z`), got {req:?}"))
        .to_owned()
}

#[test]
fn the_build_script_is_told_the_version_cargo_resolves() {
    let config = read_toml(&workspace_root().join(".cargo/config.toml"));
    let env = config["env"]["LBUG_VERSION"]
        .as_str()
        .expect("[env] LBUG_VERSION must be set in .cargo/config.toml");
    assert_eq!(
        env,
        pinned_in_workspace(),
        "[env] LBUG_VERSION and the lbug entry of [workspace.dependencies] must name one release"
    );
}

#[test]
fn no_crate_states_its_own_lbug_version() {
    let crates = workspace_root().join("crates");
    for entry in std::fs::read_dir(crates).unwrap().flatten() {
        let path = entry.path().join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        let manifest = read_toml(&path);
        for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = manifest.get(table).and_then(|t| t.as_table()) else {
                continue;
            };
            for (name, dep) in deps {
                let is_lbug =
                    name == "lbug" || dep.get("package").and_then(|p| p.as_str()) == Some("lbug");
                if is_lbug {
                    assert_eq!(
                        dep.get("workspace").and_then(|w| w.as_bool()),
                        Some(true),
                        "{} [{table}] {name}: use `{{ workspace = true }}`, not its own version",
                        path.display()
                    );
                }
            }
        }
    }
}

/// Cargo reads `.cargo/config.toml` from the directory it runs in, not from the manifest it
/// builds. The upgrade job builds the previous release from a worktree; run from HEAD's
/// directory (`--manifest-path ../previous/Cargo.toml`) it would hand HEAD's
/// `[env] LBUG_VERSION` to the old release's lbug build script, so once the old tag
/// downloads a prebuilt library the "previous" binary would link HEAD's.
#[test]
fn the_upgrade_job_builds_the_previous_release_from_its_own_directory() {
    let ci = std::fs::read_to_string(workspace_root().join(".github/workflows/ci.yml")).unwrap();
    let step = ci
        .split("- name: Build the previous release")
        .nth(1)
        .expect("ci.yml has no 'Build the previous release' step")
        .split("\n      - name:")
        .next()
        .unwrap();
    assert!(
        step.contains("cd ../previous"),
        "build the previous release from ../previous so HEAD's .cargo/config.toml does not apply:\n{step}"
    );
    assert!(
        !step.contains("--manifest-path"),
        "--manifest-path leaves cargo reading HEAD's config:\n{step}"
    );
}
