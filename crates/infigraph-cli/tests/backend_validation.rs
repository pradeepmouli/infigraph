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
        text.contains("backend setting"),
        "doctor must run and report it: {text}"
    );
    assert!(text.contains("kuzuu"), "{text}");
}
