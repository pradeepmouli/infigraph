//! `scripts/prune-build-cache.sh`: orphan `.o` files out of a cargo target
//! directory, `incremental/` only when that is safe, nothing while a cargo or
//! rustc process exists. Unix only (it is a bash script).
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/prune-build-cache.sh")
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; bytes]).unwrap();
}

/// A target directory shaped like the real one: orphan objects beside real
/// artifacts, an incremental cache, and a directory that is not a profile.
fn target() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let t = dir.path();
    write(&t.join("debug/deps/a-1.o"), 100);
    write(&t.join("debug/deps/b-2.o"), 100);
    write(&t.join("debug/deps/libx-3.rlib"), 100);
    write(&t.join("debug/deps/tool-4"), 100);
    write(&t.join("debug/incremental/crate-9/s-1/dep-graph.bin"), 500);
    write(&t.join("release/deps/c-5.o"), 100);
    write(&t.join("flycheck0/stdout"), 10);
    dir
}

/// `processes` stands in for `ps -axo comm=`: one name per line.
fn run(target: &Path, args: &[&str], processes: &str, incremental: Option<&str>) -> Output {
    let mut command = Command::new("bash");
    command
        .arg(script())
        .args(["--target-dir", target.to_str().unwrap()])
        .args(args)
        .env("PRUNE_BUILD_CACHE_PROCESSES", processes)
        .env_remove("CARGO_INCREMENTAL")
        .env_remove("CARGO_TARGET_DIR");
    if let Some(value) = incremental {
        command.env("CARGO_INCREMENTAL", value);
    }
    command.output().unwrap()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_dry_run_reports_and_deletes_nothing() {
    let t = target();

    let out = run(t.path(), &[], "bash\nlaunchd\n", None);

    assert!(out.status.success(), "{}", text(&out));
    let said = text(&out);
    assert!(said.contains("would remove 3 orphan .o files"), "{said}");
    assert!(said.contains("dry run"), "{said}");
    for kept in ["debug/deps/a-1.o", "debug/deps/b-2.o", "release/deps/c-5.o"] {
        assert!(
            t.path().join(kept).exists(),
            "{kept} was deleted by a dry run"
        );
    }
}

#[test]
fn apply_removes_only_the_object_files() {
    let t = target();

    let out = run(t.path(), &["--apply"], "bash\n", None);

    assert!(out.status.success(), "{}", text(&out));
    for gone in ["debug/deps/a-1.o", "debug/deps/b-2.o", "release/deps/c-5.o"] {
        assert!(!t.path().join(gone).exists(), "{gone} survived");
    }
    for kept in [
        "debug/deps/libx-3.rlib",
        "debug/deps/tool-4",
        "flycheck0/stdout",
        "debug/incremental/crate-9/s-1/dep-graph.bin",
    ] {
        assert!(t.path().join(kept).exists(), "{kept} was removed");
    }
    assert!(
        text(&out).contains("left incremental/"),
        "it should say why incremental/ stayed: {}",
        text(&out)
    );
}

#[test]
fn incremental_goes_only_when_cargo_incremental_is_zero_or_asked_for() {
    let incremental = |t: &tempfile::TempDir| t.path().join("debug/incremental").exists();

    let t = target();
    run(t.path(), &["--apply"], "bash\n", Some("1"));
    assert!(incremental(&t), "CARGO_INCREMENTAL=1 must keep it");

    let t = target();
    run(t.path(), &["--apply"], "bash\n", Some("0"));
    assert!(!incremental(&t), "CARGO_INCREMENTAL=0 allows removing it");

    let t = target();
    run(
        t.path(),
        &["--apply", "--include-incremental"],
        "bash\n",
        None,
    );
    assert!(!incremental(&t), "--include-incremental removes it");
}

#[test]
fn it_refuses_while_cargo_or_rustc_is_running() {
    for running in ["cargo", "rustc"] {
        let t = target();

        let out = run(t.path(), &["--apply"], &format!("bash\n{running}\n"), None);

        assert!(!out.status.success(), "{running}: {}", text(&out));
        assert!(text(&out).contains("refusing to prune"), "{}", text(&out));
        assert!(
            t.path().join("debug/deps/a-1.o").exists(),
            "{running}: deleted under a running build"
        );
    }
}

#[test]
fn a_path_like_process_name_is_judged_by_its_file_name() {
    let t = target();

    let out = run(t.path(), &["--apply"], "/Users/me/.cargo/bin/cargo\n", None);

    assert!(!out.status.success(), "{}", text(&out));
}
