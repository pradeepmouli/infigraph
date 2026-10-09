//! `daemon.idle_grace_secs` and its siblings replaced `[daemon_idle]`'s keys.
//! The old keys keep working -- a config written for them must not break --
//! and the new ones win when both are stated.

use infigraph_core::daemon::{
    deprecated_daemon_idle_keys, idle_settings_from_layers, DaemonIdleSettings,
};

fn doc(text: &str) -> toml_edit::DocumentMut {
    text.parse().unwrap()
}

fn resolve(texts: &[&str]) -> DaemonIdleSettings {
    let docs: Vec<_> = texts.iter().map(|t| doc(t)).collect();
    let layers: Vec<&toml_edit::Item> = docs.iter().map(|d| d.as_item()).collect();
    idle_settings_from_layers(&layers)
}

fn defaults() -> DaemonIdleSettings {
    DaemonIdleSettings {
        grace_secs: 1800,
        check_secs: 60,
        client_release_secs: 1800,
    }
}

/// These read the process environment; make sure no test machine's own
/// variables leak in.
fn clean_env() {
    for name in [
        "INFIGRAPH_DAEMON_IDLE_GRACE_SECS",
        "INFIGRAPH_DAEMON_IDLE_CHECK_SECS",
        "INFIGRAPH_DAEMON_IDLE_CLIENT_RELEASE_SECS",
        "INFIGRAPH_DAEMON_CLIENT_RELEASE_SECS",
    ] {
        std::env::remove_var(name);
    }
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn nothing_stated_gives_the_defaults() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    assert_eq!(resolve(&[]), defaults());
}

#[test]
fn the_new_keys_are_read() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    let got = resolve(&[
        "[daemon]\nidle_grace_secs = 300\nidle_check_secs = 5\nclient_release_secs = 120\n",
    ]);
    assert_eq!(
        got,
        DaemonIdleSettings {
            grace_secs: 300,
            check_secs: 5,
            client_release_secs: 120
        }
    );
}

#[test]
fn the_old_keys_still_work() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    let got =
        resolve(&["[daemon_idle]\ngrace_secs = 300\ncheck_secs = 5\nclient_release_secs = 120\n"]);
    assert_eq!(
        got,
        DaemonIdleSettings {
            grace_secs: 300,
            check_secs: 5,
            client_release_secs: 120
        }
    );
}

#[test]
fn the_new_key_wins_over_the_old_one_even_when_it_is_the_default_value() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    // Stated explicitly as 1800: not "unset", so the old 300 must not win.
    let got = resolve(&["[daemon]\nidle_grace_secs = 1800\n\n[daemon_idle]\ngrace_secs = 300\n"]);
    assert_eq!(got.grace_secs, 1800);
    // Across layers too: the nearer layer states the old key, the farther the
    // new one -- the new one still wins.
    let got = resolve(&[
        "[daemon_idle]\ngrace_secs = 300\n",
        "[daemon]\nidle_grace_secs = 900\n",
    ]);
    assert_eq!(got.grace_secs, 900);
}

#[test]
fn zero_is_a_value_not_an_absence() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    assert_eq!(resolve(&["[daemon]\nidle_grace_secs = 0\n"]).grace_secs, 0);
    assert_eq!(
        resolve(&["[daemon]\nidle_grace_secs = 0\n\n[daemon_idle]\ngrace_secs = 300\n"]).grace_secs,
        0
    );
}

#[test]
fn the_environment_names_work_new_and_old() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    clean_env();
    // Grace and check keep their env names (the new group's convention name
    // is the same string).
    std::env::set_var("INFIGRAPH_DAEMON_IDLE_GRACE_SECS", "42");
    assert_eq!(resolve(&[]).grace_secs, 42);
    clean_env();
    // client_release_secs moved: the old name still reads, the new one wins.
    std::env::set_var("INFIGRAPH_DAEMON_IDLE_CLIENT_RELEASE_SECS", "7");
    assert_eq!(resolve(&[]).client_release_secs, 7);
    std::env::set_var("INFIGRAPH_DAEMON_CLIENT_RELEASE_SECS", "9");
    assert_eq!(resolve(&[]).client_release_secs, 9);
    clean_env();
}

#[test]
fn deprecated_keys_are_named_with_their_replacement() {
    let d = doc("[daemon_idle]\ngrace_secs = 300\nclient_release_secs = 5\n\n[daemon]\nidle_check_secs = 5\n");
    let layers = [d.as_item()];
    let mut got = deprecated_daemon_idle_keys(&layers);
    got.sort();
    assert_eq!(
        got,
        vec![
            (
                "[daemon_idle] client_release_secs".to_string(),
                "[daemon] client_release_secs".to_string()
            ),
            (
                "[daemon_idle] grace_secs".to_string(),
                "[daemon] idle_grace_secs".to_string()
            ),
        ]
    );
    let clean = doc("[daemon]\nidle_grace_secs = 300\n");
    assert!(deprecated_daemon_idle_keys(&[clean.as_item()]).is_empty());
}
