//! Shared "why did this process exit" logging primitive, used by both
//! `infigraph daemon` and `infigraph-mcp` so a future incident leaves an
//! actual diagnostic trail instead of nothing. Every voluntary exit path
//! (clean shutdown, a caught signal, a panic) can and should call this
//! before exiting -- the one exit reason that can never be logged this way
//! is an uncatchable kill (SIGKILL, or the OS's own resource-pressure
//! reaper), since no code runs for those at all.

use std::io::Write;
use std::path::Path;

/// Write one line to stderr and ignore the outcome. `eprintln!` PANICS when the write fails -- a
/// broken pipe, which is what stderr becomes once the client that launched an MCP server or a
/// daemon has closed its end -- and a logging call must never take its caller down with it.
pub fn stderr_line(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// Append one timestamped `[epoch] LEVEL: msg` line to `path`, rotating it
/// first if it's grown past 10MB. Best-effort throughout: a logging
/// failure must never fail (or even meaningfully slow down) the caller's
/// own shutdown -- this is diagnostics, not a durability guarantee.
pub fn write_log_line(path: &Path, level: &str, msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{ts}] {level}: {msg}");
    stderr_line(&line);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    crate::logrotate::rotate_if_over(path, 10 * 1024 * 1024);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// `"<payload> at <file>:<line>:<col>"` for a panic, the same text every process's panic hook logs.
pub fn describe_panic(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    };
    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".to_string());
    format!("{payload} at {location}")
}

/// Install a panic hook that hands `on_panic` the description and a forced backtrace of every
/// panic in this process. The one hook installer for `infigraph daemon` and `infigraph-mcp`.
///
/// A panic inside a panic hook aborts the process (SIGABRT) and `catch_unwind` cannot stop it
/// (std tracks "in hook" and aborts on the nested panic itself), so `on_panic` must not panic:
/// write through `write_log_line` and `stderr_line`, which ignore a failed write, never `eprintln!`.
pub fn install_panic_hook(on_panic: impl Fn(&str, &str) + Send + Sync + 'static) {
    std::panic::set_hook(Box::new(move |info| {
        let desc = describe_panic(info);
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        on_panic(&desc, &bt);
    }));
}

#[cfg(test)]
mod tests {
    use super::write_log_line;

    #[test]
    fn writes_a_timestamped_line_and_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("proc.log");
        write_log_line(&path, "INFO", "hello");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("] INFO: hello"), "got: {content}");
    }

    #[test]
    fn rotates_before_appending_once_over_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("proc.log");
        std::fs::write(&path, vec![0u8; 11 * 1024 * 1024]).unwrap();
        write_log_line(&path, "INFO", "after rotation");
        let rotated = tmp.path().join("proc.log.1");
        assert!(rotated.exists(), "oversized log must be rotated aside");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("after rotation"));
    }
}
