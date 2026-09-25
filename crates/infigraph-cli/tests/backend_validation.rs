//! #74: a typo'd INFIGRAPH_BACKEND stops the CLI with a Config error that
//! names the variable, rather than silently running some backend.

fn infigraph() -> std::process::Command {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_infigraph"));
    cmd.env_remove("INFIGRAPH_WATCH_DAEMON");
    cmd
}

/// `stats` opens the store, which `init*` already validates; `watch-status`
/// never does, and reads only the lenient helpers -- the startup check is
/// what stops that one.
#[test]
fn an_unknown_backend_fails_before_any_work() {
    for command in ["stats", "watch-status"] {
        let tmp = tempfile::tempdir().unwrap();
        let out = infigraph()
            .current_dir(tmp.path())
            .env("INFIGRAPH_BACKEND", "kuzuu")
            .arg(command)
            .output()
            .unwrap();
        assert!(!out.status.success(), "`{command}` must fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("INFIGRAPH_BACKEND") && stderr.contains("kuzu, daemon, neo4j"),
            "`{command}`: {stderr}"
        );
        assert!(
            !tmp.path().join(".infigraph").exists(),
            "`{command}`: nothing may be created"
        );
    }
}

#[test]
fn doctor_runs_and_reports_an_invalid_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let out = infigraph()
        .current_dir(tmp.path())
        .env("INFIGRAPH_BACKEND", "kuzuu")
        .args(["doctor"])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("INFIGRAPH_BACKEND"),
        "doctor must run and report it: {text}"
    );
    assert!(text.contains("kuzuu"), "{text}");
}

/// Review minor: installing and updating are how a user gets a fixed build,
/// so a bad backend setting must not block them -- and they never open a
/// store. `--dry-run` with an isolated HOME keeps this read-only.
#[test]
fn install_is_not_blocked_by_a_bad_backend() {
    let tmp = tempfile::tempdir().unwrap();
    let out = infigraph()
        .current_dir(tmp.path())
        .env("HOME", tmp.path())
        .env("INFIGRAPH_BACKEND", "kuzuu")
        .args(["install", "--dry-run"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("invalid backend setting"),
        "install must not be gated on the backend: {stderr}"
    );
}

/// #199: not just the backend -- a bad value in any settings group stops
/// the CLI at startup, naming it, and doctor (exempt) reports it.
#[test]
fn a_bad_value_in_any_settings_group_fails_startup_and_doctor_reports_it() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |command: &str| {
        let out = infigraph()
            .current_dir(tmp.path())
            .env("INFIGRAPH_GRAPH_MAX_BYTES", "lots")
            .arg(command)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    };
    let (ok, text) = run("watch-status");
    assert!(!ok, "a bad setting must stop startup: {text}");
    assert!(text.contains("INFIGRAPH_GRAPH_MAX_BYTES"), "{text}");
    let (_, text) = run("doctor");
    assert!(
        text.contains("INFIGRAPH_GRAPH_MAX_BYTES") && text.contains("\"lots\""),
        "doctor must report it: {text}"
    );
}
