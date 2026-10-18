//! Runs the Claude Code enforcement hook (`infigraph-enforce.sh`) the way
//! Claude Code does -- a tool call as JSON on stdin, a deny as JSON on stdout
//! -- to hold the two checks it makes on a file path to what they say:
//!
//! * a Read of source code is denied, a Read of anything else is not;
//! * a Write/Edit of a *test* in the project is denied until
//!   `generate_test_context` has been called. A test is named by its path
//!   components, so a file that merely has "test" inside a longer word
//!   (`vitest_json_flag.md`, `contests/`) is not one, and neither is a file
//!   outside the project.

#![cfg(unix)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn hook() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("resources/integrations/claude-code/.claude/hooks/infigraph-enforce.sh")
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

/// A project (with `.infigraph/`) and a `pgrep` that says an MCP server is
/// running: the hook allows everything when none is.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let root = top.join("project");
        std::fs::create_dir_all(root.join(".infigraph")).unwrap();
        let bin = top.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let pgrep = bin.join("pgrep");
        std::fs::write(&pgrep, "#!/bin/sh\nexit 0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pgrep, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fixture {
            _dir: dir,
            root,
            bin,
        }
    }

    /// Whether the hook denies `tool` on `file`.
    fn denies(&self, tool: &str, file: &Path) -> bool {
        let input = serde_json::json!({
            "tool_name": tool,
            "cwd": self.root,
            "tool_input": { "file_path": file },
        });
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("bash")
            .arg(hook())
            .current_dir(&self.root)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains("\"deny\"")
    }

    fn write_denied(&self, rel: &str) -> bool {
        self.denies("Write", &self.root.join(rel))
    }
}

#[test]
fn a_test_is_named_by_its_path_components_and_only_inside_the_project() {
    if !have("jq") {
        eprintln!("skipped: jq is not installed");
        return;
    }
    let f = Fixture::new();

    // Tests: held to generate_test_context.
    for rel in [
        "tests/helper.rs",
        "src/foo.test.ts",
        "src/foo_test.go",
        "src/test_utils.py",
        "src/foo_spec.rb",
        "src/foo.spec.js",
        "src/__tests__/foo.js",
        "features/login.feature",
    ] {
        assert!(f.write_denied(rel), "{rel} is a test and was not held");
    }

    // Not tests: a longer word that contains "test", a non-code file, a
    // directory that only ends in "tests".
    for rel in [
        "docs/feedback_vitest_json_flag_takes_next_arg.md",
        "src/contests/x.md",
        "src/contests/x.rs",
        "tests/README.md",
        "src/latest_results.rs",
        "src/main.rs",
    ] {
        assert!(!f.write_denied(rel), "{rel} is not a test and was held");
    }

    // A memory note outside any project, named like the one that was caught.
    let outside = f
        .root
        .parent()
        .unwrap()
        .join("memory/feedback_vitest_json_flag_takes_next_arg.md");
    assert!(
        !f.denies("Write", &outside),
        "a file outside the project is not held"
    );
}

#[test]
fn a_fresh_generate_test_context_sentinel_lets_a_test_through() {
    if !have("jq") {
        eprintln!("skipped: jq is not installed");
        return;
    }
    let f = Fixture::new();
    assert!(f.write_denied("tests/helper.rs"));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        f.root.join(".infigraph/.test-context-called"),
        now.to_string(),
    )
    .unwrap();
    assert!(!f.write_denied("tests/helper.rs"));
}

#[test]
fn a_read_is_denied_for_source_code_and_allowed_for_everything_else() {
    if !have("jq") {
        eprintln!("skipped: jq is not installed");
        return;
    }
    let f = Fixture::new();
    for rel in ["src/main.rs", "Makefile", "tests/helper.rs"] {
        assert!(f.denies("Read", &f.root.join(rel)), "{rel} is code");
    }
    for rel in [
        "README.md",
        "Cargo.toml",
        "docs/notes.md",
        "tests/README.md",
    ] {
        assert!(!f.denies("Read", &f.root.join(rel)), "{rel} is not code");
    }
}
