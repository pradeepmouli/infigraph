//! The breadcrumb an idle daemon leaves when it exits (#38): without it,
//! "no watcher running" after an idle exit reads like a watcher that died, and
//! nothing says that the next use starts one again.
//!
//! `.infigraph/daemon-idle-exit` is written by the daemon as it leaves because
//! nothing leased it, and cleared by the next daemon's start. It is derived
//! state (a rebuild or a clone need not carry it) and is never created in a
//! project that has no `.infigraph/` directory.

use std::path::Path;

const FILE: &str = "daemon-idle-exit";

/// An idle exit, as recorded.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdleExit {
    pub pid: u32,
    /// Unix seconds when the daemon left.
    pub at_secs: u64,
    /// How long nothing had touched it.
    pub idle_secs: u64,
}

/// Records that this daemon is leaving because nobody leased it. Best effort:
/// an exit is never held up by a breadcrumb.
pub fn record(root: &Path, idle_secs: u64) {
    let dir = root.join(".infigraph");
    if !dir.is_dir() {
        return;
    }
    let exit = IdleExit {
        pid: std::process::id(),
        at_secs: super::liveness::now_secs(),
        idle_secs,
    };
    if let Ok(json) = serde_json::to_vec(&exit) {
        let _ = std::fs::write(dir.join(FILE), json);
    }
}

/// Forgets the last idle exit: a daemon is running again.
pub fn clear(root: &Path) {
    let _ = std::fs::remove_file(root.join(".infigraph").join(FILE));
}

/// The last idle exit, if no daemon has started since.
pub fn last(root: &Path) -> Option<IdleExit> {
    let bytes = std::fs::read(root.join(".infigraph").join(FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// What to add to "no watcher running" when the last daemon left on purpose.
pub fn note(root: &Path) -> Option<String> {
    let exit = last(root)?;
    let ago = super::liveness::now_secs().saturating_sub(exit.at_secs);
    Some(format!(
        "its daemon (PID {}) exited {} ago after {}s with no client, as designed; the next use \
         of this project starts one again",
        exit.pid,
        humanize(ago),
        exit.idle_secs
    ))
}

fn humanize(secs: u64) -> String {
    match secs {
        0..=89 => format!("{secs}s"),
        90..=5399 => format!("{}m", secs.div_ceil(60)),
        _ => format!("{}h", secs.div_ceil(3600)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_exit_is_recorded_read_back_and_cleared_by_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".infigraph")).unwrap();
        assert_eq!(last(dir.path()), None);

        record(dir.path(), 1800);
        let exit = last(dir.path()).expect("written on the way out");
        assert_eq!(exit.pid, std::process::id());
        assert_eq!(exit.idle_secs, 1800);
        let text = note(dir.path()).unwrap();
        assert!(text.contains("1800s with no client"), "{text}");
        assert!(text.contains("starts one again"), "{text}");

        clear(dir.path());
        assert_eq!(last(dir.path()), None);
        assert!(note(dir.path()).is_none());
    }

    #[test]
    fn a_project_without_an_infigraph_directory_gets_none_created() {
        let dir = tempfile::tempdir().unwrap();

        record(dir.path(), 5);

        assert!(!dir.path().join(".infigraph").exists());
        assert_eq!(last(dir.path()), None);
    }
}
