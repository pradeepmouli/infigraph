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

    /// Whether the hook denies a tool call given as the JSON Claude Code
    /// sends it (`cwd` is added).
    fn denies_input(&self, mut input: serde_json::Value) -> bool {
        input["cwd"] = serde_json::json!(self.root);
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

    /// Whether the hook denies `tool` on `file`.
    fn denies(&self, tool: &str, file: &Path) -> bool {
        self.denies_input(serde_json::json!({
            "tool_name": tool,
            "tool_input": { "file_path": file },
        }))
    }

    fn bash_denied(&self, command: &str) -> bool {
        self.denies_input(serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": command },
        }))
    }

    fn grep_tool_denied(&self, input: serde_json::Value) -> bool {
        self.denies_input(serde_json::json!({ "tool_name": "Grep", "tool_input": input }))
    }

    /// The files the search cases name.
    fn with_search_files(self) -> Self {
        for (rel, text) in [
            ("src/main.rs", "fn main() {}\n"),
            ("README.md", "# hi\n"),
            ("Cargo.toml", "[p]\n"),
            ("package.json", "{}\n"),
            ("docs/a.md", "x\n"),
            ("node_modules/x/index.js", "x\n"),
        ] {
            let path = self.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        self
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

/// A grep, rg or Grep-tool call is denied only where it can reach source code
/// infigraph indexes: a source file or a directory inside the project, the
/// project tree by default, or a target the hook cannot determine. Docs,
/// configs, files outside the project, ignored directories and stdin are not
/// denied.
#[test]
fn a_search_is_denied_only_where_it_can_reach_source_code() {
    if !have("jq") {
        eprintln!("skipped: jq is not installed");
        return;
    }
    let f = Fixture::new().with_search_files();
    let bash_cases: &[(bool, &str)] = &[
        // Non-code targets, outside the project, ignored, stdin.
        (false, "grep -n foo README.md"),
        (false, "grep foo Cargo.toml"),
        (false, "rg foo package.json"),
        (false, "grep -rn foo docs/a.md"),
        (false, "grep foo /etc/hosts"),
        (false, "grep foo ~/.zshrc"),
        (false, "grep -r foo node_modules"),
        (false, "cat README.md | grep foo"),
        (false, "git log --oneline | rg fix"),
        (false, "grep foo < README.md"),
        (false, "grep foo <<EOF\nline\nEOF"),
        (false, "grep -e foo -e bar README.md"),
        (false, "grep -A 3 foo README.md"),
        (false, "grep foo README.md > /tmp/out"),
        (false, "grep foo README.md 2>&1"),
        // A glob, include or type that holds it to non-code.
        (false, "grep -r --include=*.md foo docs"),
        (false, "rg -g \"*.md\" foo"),
        (false, "rg -t md foo"),
        (false, "git grep foo -- '*.md'"),
        // A command word that is not a search.
        (false, "echo grep is great"),
        (false, "gh issue create --title \"grep is blocked\""),
        (false, "cargo build --release"),
        (false, ""),
        // Source code, a directory, the project tree.
        (true, "grep foo src/main.rs"),
        (true, "grep \"foo bar\" src/main.rs"),
        (true, "grep foo README.md src/main.rs"),
        (true, "grep foo < src/main.rs"),
        (true, "grep -r foo ."),
        (true, "grep -r foo docs"),
        (true, "grep -rn foo not_there"),
        (false, "grep -r --include=*.md foo not_there"),
        (true, "rg foo"),
        (true, "rg foo README.md src/"),
        (true, "rg -g \"*\" foo"),
        (true, "rg -t rust foo"),
        (true, "git grep foo"),
        (true, "git grep foo -- src/"),
        (true, "grep foo README.md; grep foo src/main.rs"),
        // Targets the hook cannot determine.
        (true, "grep foo \"$FILE\""),
        // Brace expansion: the shell expands what the hook cannot judge.
        (true, "grep foo {README.md,src/main.rs}"),
        (true, "grep foo src/{a,b}.rs"),
        (true, "grep foo docs/{a,b}.md"),
        (true, "grep foo < {README.md,src/main.rs}"),
        // The pattern is not a target, quoted or not.
        (false, "grep 'a{2}' README.md"),
        (false, "grep a{2} README.md"),
        // A process substitution is another command's output, like a pipe.
        (false, "grep foo <(cat src/main.rs)"),
        // A joined `-tmd` is not parsed as a type: the tree is unrestricted.
        (true, "rg -tmd foo"),
        (true, "grep foo $(ls)"),
        (true, "grep foo \"src/main.rs"),
        (true, "ls | xargs grep foo"),
        (true, "for f in a b; do grep -n foo \"$f\"; done"),
    ];
    for (denied, command) in bash_cases {
        assert_eq!(
            f.bash_denied(command),
            *denied,
            "`{command}` should be {}",
            if *denied { "denied" } else { "allowed" }
        );
    }

    let grep_tool_cases: &[(bool, serde_json::Value)] = &[
        (
            false,
            serde_json::json!({"pattern": "foo", "path": "README.md"}),
        ),
        (false, serde_json::json!({"pattern": "foo", "glob": "*.md"})),
        (false, serde_json::json!({"pattern": "foo", "type": "md"})),
        (false, serde_json::json!({"pattern": "foo", "path": "/etc"})),
        (true, serde_json::json!({"pattern": "foo"})),
        (true, serde_json::json!({"pattern": "foo", "path": "src"})),
        (true, serde_json::json!({"pattern": "foo", "type": "rust"})),
        (true, serde_json::json!({"pattern": "foo", "glob": "**/*"})),
    ];
    for (denied, input) in grep_tool_cases {
        assert_eq!(
            f.grep_tool_denied(input.clone()),
            *denied,
            "Grep {input} should be {}",
            if *denied { "denied" } else { "allowed" }
        );
    }
}

/// The denial still names the tool to use, and says how a search of non-code
/// files gets through.
#[test]
fn a_denied_search_names_the_tool_and_the_way_through() {
    if !have("jq") {
        return;
    }
    let f = Fixture::new().with_search_files();
    let input = serde_json::json!({
        "tool_name": "Bash",
        "cwd": f.root,
        "tool_input": { "command": "grep foo src/main.rs" },
    });
    let path = format!(
        "{}:{}",
        f.bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = Command::new("bash")
        .arg(hook())
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    assert!(out.contains("mcp__infigraph__search"), "{out}");
    assert!(out.contains("--include='*.md'"), "{out}");
    assert!(out.contains("do not work around this hook"), "{out}");
}

/// The hook failing is not a denial: input it cannot read allows.
#[test]
fn input_the_hook_cannot_read_is_allowed() {
    if !have("jq") {
        return;
    }
    let f = Fixture::new();
    let mut child = Command::new("bash")
        .arg(hook())
        .env(
            "PATH",
            format!(
                "{}:{}",
                f.bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"not json at all")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stdout).contains("deny"));
}

/// The ripgrep type names the hook treats as non-code are names, not
/// extensions, so they are listed once and held to the one list of what is
/// source: each, read as an extension, is not source by `is_source_code`.
#[test]
fn every_non_code_rg_type_is_not_source_by_is_source_code() {
    let text = std::fs::read_to_string(hook()).unwrap();
    let types = text
        .lines()
        .find_map(|l| l.strip_prefix("NON_CODE_RG_TYPES=\""))
        .and_then(|l| l.strip_suffix('"'))
        .expect("NON_CODE_RG_TYPES in the hook");
    assert!(!types.is_empty());
    let start = text.find("is_source_code() {").expect("is_source_code");
    let end = start + text[start..].find("\n}\n").unwrap() + 3;
    let function = &text[start..end];
    let script = format!(
        "{function}\nfor t in {types}; do is_source_code \"x.$t\" && echo \"$t\"; done; true"
    );
    let out = Command::new("bash").arg("-c").arg(script).output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "",
        "these ripgrep types name source code, so they cannot be non-code"
    );
}
