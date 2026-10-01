//! Long-lived line-protocol children (R2.5.1): the JVM grammar driver and the
//! pipeline plugins speak newline-delimited JSON over stdin/stdout, one
//! request in flight at a time. Before this module each driver read its
//! child's stdout with a bare blocking `read_line`, so a hung child hung the
//! caller forever, the child's stderr was discarded or inherited, and only the
//! direct child was killed on drop.
//!
//! [`LineChild`] gives them what the one-shot SCIP indexers already have: the
//! child leads its own process group, every read has a deadline, stderr is
//! kept as a bounded tail that rides on the error, and a timeout *poisons* the
//! child -- it is killed with its group, reaped, and every later call fails
//! naming the first failure. There is no respawn: a timed-out child may still
//! answer, and that late reply would be read as the next request's answer; a
//! respawned one would have lost whatever state the protocol loaded into it.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

/// How much of a child's stderr is kept for its failure report.
pub const STDERR_TAIL_BYTES: usize = 8 * 1024;

/// The newest lines of a stream, within [`STDERR_TAIL_BYTES`].
#[derive(Default)]
pub struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
}

impl StderrTail {
    pub fn push(&mut self, mut line: String) {
        if line.len() > STDERR_TAIL_BYTES {
            // One enormous line: keep its end, on a char boundary.
            let mut cut = line.len() - STDERR_TAIL_BYTES;
            while !line.is_char_boundary(cut) {
                cut += 1;
            }
            line.drain(..cut);
        }
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.bytes > STDERR_TAIL_BYTES {
            match self.lines.pop_front() {
                Some(old) => self.bytes -= old.len() + 1,
                None => break,
            }
        }
    }

    pub fn text(&self) -> String {
        self.lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Deadlines for a line-protocol child.
///
/// The defaults are unmeasured guesses: the grammar driver's jar was not
/// available when they were chosen, so JVM start-up and the slowest real
/// request (a large generated file through a big grammar) are unknown. They
/// are generous on purpose -- the point is that a hung child no longer hangs
/// its caller forever -- and a measurement should replace them.
#[derive(Debug, Clone, Copy)]
pub struct ChildTimeouts {
    /// From spawn to the first line (the ready handshake).
    pub ready: Duration,
    /// From a request being written to its one-line reply.
    pub request: Duration,
}

impl ChildTimeouts {
    pub const DEFAULT: ChildTimeouts = ChildTimeouts {
        ready: Duration::from_secs(30),
        request: Duration::from_secs(120),
    };
}

/// Makes `command` lead a process group of its own (pgid == pid, set
/// atomically at spawn), so a signal to that group reaches what the child
/// started and nothing else. The one place a spawn of ours asks for it.
/// Unix only: elsewhere a child cannot be grouped this way (Job Objects would
/// be the equivalent, #208).
pub fn lead_own_group(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Whether `child` has exited, without reaping it: the exit status stays
/// pending and the pid stays ours. That is what makes "signal the group, then
/// reap" possible -- once a leader is reaped its pid is free to be reused, so
/// a group must be signalled before it. Never call after the child was waited.
///
/// Non-unix has no such peek; there it reaps like `try_wait`, and group
/// signalling is a no-op anyway.
pub fn exited_unreaped(child: &mut Child) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        // SAFETY: an all-zero siginfo_t is a valid output buffer for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: P_PID with this child's pid; WNOWAIT leaves it waitable.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: reading the pid out of the siginfo waitid just filled in.
        #[cfg(target_os = "linux")]
        let pid = unsafe { info.si_pid() };
        #[cfg(not(target_os = "linux"))]
        let pid = info.si_pid;
        // waitid leaves si_pid 0 when nothing was waitable (WNOHANG).
        Ok(pid != 0)
    }
    #[cfg(not(unix))]
    {
        child.try_wait().map(|status| status.is_some())
    }
}

/// Kills a process group when dropped before [`disarm`](Self::disarm), for a
/// leader owned by an async `Child` that cannot be a [`GroupChild`]: on a
/// timeout, or when the owning future is dropped (a cancelled batch, daemon
/// shutdown). `kill_on_drop` alone reaches only the direct child.
///
/// Safe against pid reuse because it fires while the leader is still
/// unreaped: a dropped owning future drops this guard before the `Child`
/// (declared earlier, so dropped later) is reaped, and a run whose `wait()`
/// completed has already disarmed it.
///
/// Unix only: on Windows an indexer's grandchildren still outlive a timeout
/// and only the direct child dies (Job Objects, #208).
pub struct GroupKillOnDrop(Option<u32>);

impl GroupKillOnDrop {
    /// Guards the group led by `pid` (a child spawned with [`lead_own_group`]).
    pub fn new(pid: Option<u32>) -> Self {
        Self(pid)
    }

    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupKillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            crate::daemon::lifecycle::kill_process_group(pid);
        }
    }
}

/// A child that leads its own process group *because this type spawned it
/// so*: the group id it signals is, by construction, one it created, never
/// the caller's own group or a group inherited from elsewhere (a wrong
/// `killpg` there takes out the MCP client). Stopping it is TERM, a grace
/// period, then KILL, and every signal goes out while the leader is still
/// unreaped. Dropping it kills the group at once.
pub struct GroupChild {
    child: Child,
    reaped: bool,
    /// How many group signals were sent (test seam).
    #[cfg(test)]
    group_signals: usize,
}

impl GroupChild {
    /// Spawns `command` as its own group leader.
    pub fn spawn(mut command: Command) -> std::io::Result<Self> {
        lead_own_group(&mut command);
        Ok(Self {
            child: command.spawn()?,
            reaped: false,
            #[cfg(test)]
            group_signals: 0,
        })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The `Child`, for taking its pipes. Do not wait on it directly.
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    /// Whether the leader has exited; it stays unreaped until [`reap`] or
    /// [`stop`](Self::stop).
    ///
    /// [`reap`]: Self::reap
    pub fn exited(&mut self) -> std::io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        exited_unreaped(&mut self.child)
    }

    /// Reaps a leader that has exited ([`exited`](Self::exited) said so),
    /// after sweeping what it left in its group. The sweep comes first, while
    /// the zombie leader still holds the pgid.
    pub fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if !self.reaped {
            self.signal_group(false);
        }
        let status = self.child.wait();
        self.reaped = true;
        status
    }

    /// Terminates the group and reaps the leader. SIGTERM first, then it
    /// waits up to `grace` for the leader to exit -- returning as soon as it
    /// does -- and finally KILLs whatever is left in the group, all before
    /// the leader is reaped. A zero `grace` is an immediate KILL. Only the
    /// first call does anything.
    pub fn stop(&mut self, grace: Duration) {
        if self.reaped {
            return;
        }
        if !grace.is_zero() {
            self.signal_group(true);
            let deadline = std::time::Instant::now() + grace;
            while std::time::Instant::now() < deadline {
                if matches!(exited_unreaped(&mut self.child), Ok(true)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.signal_group(false);
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }

    fn signal_group(&mut self, polite: bool) {
        #[cfg(unix)]
        {
            #[cfg(test)]
            {
                self.group_signals += 1;
            }
            if polite {
                crate::daemon::lifecycle::terminate_process_group(self.child.id());
            } else {
                crate::daemon::lifecycle::kill_process_group(self.child.id());
            }
        }
        #[cfg(not(unix))]
        let _ = polite;
    }
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        self.stop(Duration::ZERO);
    }
}

/// A spawned child that speaks one JSON line per request.
pub struct LineChild {
    label: String,
    child: Child,
    lines: Receiver<String>,
    stderr_tail: Arc<Mutex<StderrTail>>,
    /// The first failure; once set, the child is dead and every call errors.
    poisoned: Option<String>,
    /// Whether the group leader has been reaped. Its pid is then free for
    /// the OS to reuse -- possibly by a process that leads its own group,
    /// which every spawn of ours does -- so nothing may signal it again.
    reaped: bool,
    /// How many times the process group was signalled (test seam).
    #[cfg(test)]
    group_kills: usize,
}

impl LineChild {
    /// Spawns `command` as its own process group leader with piped stdin,
    /// stdout and stderr. `tee_stderr` forwards each stderr line to ours as it
    /// arrives (for a child whose stderr used to be inherited); the tail is
    /// kept either way.
    pub fn spawn(mut command: Command, label: &str, tee_stderr: bool) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own group (pgid == pid), so the kill below reaches what the
        // child started too.
        lead_own_group(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn {label}"))?;

        let stdout = child.stdout.take().context("no stdout from child")?;
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let stderr_tail = Arc::new(Mutex::new(StderrTail::default()));
        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    match reader.read_until(b'\n', &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let line = String::from_utf8_lossy(&buf)
                                .trim_end_matches(['\n', '\r'])
                                .to_string();
                            if tee_stderr {
                                eprintln!("{line}");
                            }
                            if let Ok(mut tail) = tail.lock() {
                                tail.push(line);
                            }
                        }
                    }
                }
            });
        }

        Ok(Self {
            label: label.to_string(),
            child,
            lines,
            stderr_tail,
            poisoned: None,
            reaped: false,
            #[cfg(test)]
            group_kills: 0,
        })
    }

    /// Whether a timeout, an exit or a failed write has already killed this
    /// child; every later call fails.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.is_some()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The tail of the child's stderr so far.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|t| t.text())
            .unwrap_or_default()
    }

    /// Writes `line` (a newline is added) and reads the one-line reply within
    /// `timeout`.
    pub fn request(&mut self, line: &str, timeout: Duration) -> Result<String> {
        self.write_line(line)?;
        self.read_line(timeout)
    }

    pub fn write_line(&mut self, line: &str) -> Result<()> {
        self.check_alive()?;
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("{} has no stdin", self.label))?;
        let written = stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush());
        match written {
            Ok(()) => Ok(()),
            Err(e) => Err(self.poison(format!("write to {} failed: {e}", self.label))),
        }
    }

    /// Reads the next line within `timeout`. A timeout or an exited child
    /// poisons it.
    pub fn read_line(&mut self, timeout: Duration) -> Result<String> {
        self.check_alive()?;
        match self.lines.recv_timeout(timeout) {
            Ok(line) => Ok(line),
            Err(RecvTimeoutError::Timeout) => {
                Err(self.poison(format!("{} sent nothing for {timeout:?}", self.label)))
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err(self.poison(format!("{} exited without answering", self.label)))
            }
        }
    }

    /// Waits up to `grace` for the child to exit by itself (after being told
    /// to shut down), then kills it and its group.
    pub fn finish(&mut self, grace: Duration) {
        let deadline = std::time::Instant::now() + grace;
        while std::time::Instant::now() < deadline {
            if matches!(exited_unreaped(&mut self.child), Ok(true)) {
                // It exited by itself and is still unreaped, so it still
                // holds the pgid. Clear what it left in its group now, then
                // reap it, and never signal again.
                self.signal_group();
                let _ = self.child.wait();
                self.reaped = true;
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.kill();
    }

    fn check_alive(&self) -> Result<()> {
        match &self.poisoned {
            Some(first) => bail!(
                "{} was stopped after its first failure: {first}",
                self.label
            ),
            None => Ok(()),
        }
    }

    /// Kills the child and its group, reaps it, and records `reason` (with
    /// the stderr tail) as the failure every later call reports. Returns the
    /// error for the call that hit it.
    pub fn poison(&mut self, reason: String) -> anyhow::Error {
        self.kill();
        // Give the stderr thread a moment to read what the dying child wrote.
        std::thread::sleep(Duration::from_millis(50));
        let tail = self.stderr_tail();
        let full = if tail.trim().is_empty() {
            reason
        } else {
            format!("{reason}; stderr (last {STDERR_TAIL_BYTES} bytes max):\n{tail}")
        };
        self.poisoned = Some(full.clone());
        anyhow!(full)
    }

    /// Kills the child and its group and reaps it. Only the first call does
    /// anything: once the leader is reaped its pid may belong to someone else.
    fn kill(&mut self) {
        if self.reaped {
            return;
        }
        // The leader is not reaped yet, so even a zombie still holds the
        // pgid and the signal can only reach our own group.
        self.signal_group();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reaped = true;
    }

    fn signal_group(&mut self) {
        #[cfg(unix)]
        {
            #[cfg(test)]
            {
                self.group_kills += 1;
            }
            crate::daemon::lifecycle::kill_process_group(self.child.id());
        }
    }
}

impl Drop for LineChild {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    fn alive(pid: u32) -> bool {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn gone_within(pid: u32, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        while alive(pid) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        true
    }

    #[test]
    fn a_request_gets_its_one_line_reply() {
        let mut child = LineChild::spawn(
            sh("echo ready; while read l; do echo \"echo:$l\"; done"),
            "echoer",
            false,
        )
        .unwrap();
        assert_eq!(child.read_line(Duration::from_secs(5)).unwrap(), "ready\n");
        assert_eq!(
            child.request("hello", Duration::from_secs(5)).unwrap(),
            "echo:hello\n"
        );
    }

    #[test]
    fn a_child_that_never_sends_ready_times_out_and_is_killed_with_its_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let mut child = LineChild::spawn(
            sh(&format!(
                "echo oops-no-handshake >&2; sleep 600 & echo $! > '{}'; wait",
                pid_file.display()
            )),
            "mute",
            false,
        )
        .unwrap();
        let direct = child.pid();
        let started = std::time::Instant::now();
        let err = child.read_line(Duration::from_millis(500)).unwrap_err();
        // The requested 500ms, not some other bound.
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "took {:?}",
            started.elapsed()
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("sent nothing"), "{msg}");
        assert!(
            msg.contains("oops-no-handshake"),
            "stderr tail missing: {msg}"
        );
        assert!(gone_within(direct, Duration::from_secs(5)));
        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            gone_within(grandchild, Duration::from_secs(5)),
            "the child's grandchild outlived the poison"
        );
    }

    #[test]
    fn after_a_request_times_out_every_call_fails_naming_the_first_failure() {
        let mut child =
            LineChild::spawn(sh("echo ready; read l; sleep 600"), "stalls", false).unwrap();
        child.read_line(Duration::from_secs(5)).unwrap();
        let first = child
            .request("hang please", Duration::from_millis(300))
            .unwrap_err();
        assert!(format!("{first:#}").contains("sent nothing"));
        assert!(child.is_poisoned());
        let later = child.request("again", Duration::from_secs(5)).unwrap_err();
        let msg = format!("{later:#}");
        assert!(msg.contains("first failure"), "{msg}");
        assert!(
            msg.contains("sent nothing"),
            "later calls must name the first failure: {msg}"
        );
    }

    #[test]
    fn a_child_that_exits_mid_conversation_reports_its_stderr() {
        let mut child = LineChild::spawn(
            sh("echo ready; read l; echo 'boom: bad grammar' >&2; exit 2"),
            "dies",
            false,
        )
        .unwrap();
        child.read_line(Duration::from_secs(5)).unwrap();
        let err = child.request("go", Duration::from_secs(5)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("exited without answering"), "{msg}");
        assert!(msg.contains("boom: bad grammar"), "{msg}");
    }

    #[test]
    fn dropping_the_child_kills_its_group_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let mut bystander = Command::new("sleep").arg("60").spawn().unwrap();
        let child = LineChild::spawn(
            sh(&format!(
                "echo ready; sleep 600 & echo $! > '{}'; wait",
                pid_file.display()
            )),
            "dropped",
            false,
        )
        .unwrap();
        // Wait for the grandchild to exist.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let grandchild: u32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        drop(child);
        let grandchild_gone = gone_within(grandchild, Duration::from_secs(5));
        let bystander_alive = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();
        assert!(grandchild_gone, "the grandchild outlived the drop");
        assert!(
            bystander_alive,
            "the kill reached a process outside the group"
        );
    }

    /// Once the leader has been reaped its pid is free to be reused -- by a
    /// process that leads its own group, which is exactly what our spawns
    /// create -- so a kill that runs again (the Drop after a poison, or after
    /// `finish` saw the child exit) must not signal the group a second time.
    #[test]
    fn a_reaped_child_is_never_signalled_again() {
        let mut poisoned = LineChild::spawn(sh("sleep 600"), "poisoned", false).unwrap();
        let _ = poisoned.read_line(Duration::from_millis(200)).unwrap_err();
        assert_eq!(poisoned.group_kills, 1);
        poisoned.kill(); // what Drop runs
        assert_eq!(
            poisoned.group_kills, 1,
            "the group was signalled again after the reap"
        );

        let mut finished = LineChild::spawn(sh("read l; exit 0"), "finished", false).unwrap();
        finished.write_line("bye").unwrap();
        finished.finish(Duration::from_secs(5));
        assert_eq!(
            finished.group_kills, 1,
            "finish must clear the group once, at the exit"
        );
        finished.kill();
        assert_eq!(
            finished.group_kills, 1,
            "the group was signalled again after the reap"
        );
    }

    #[test]
    fn finish_lets_a_child_exit_by_itself_and_kills_one_that_does_not() {
        let mut polite = LineChild::spawn(sh("read l; exit 0"), "polite", false).unwrap();
        polite.write_line("bye").unwrap();
        let started = std::time::Instant::now();
        polite.finish(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(2));

        let mut stubborn = LineChild::spawn(sh("sleep 600"), "stubborn", false).unwrap();
        let pid = stubborn.pid();
        stubborn.finish(Duration::from_millis(200));
        assert!(gone_within(pid, Duration::from_secs(5)));
    }

    #[test]
    fn the_stderr_tail_keeps_the_newest_lines_within_its_bound() {
        let mut tail = StderrTail::default();
        for i in 0..1000 {
            tail.push(format!("line {i:04} {}", "x".repeat(40)));
        }
        let text = tail.text();
        assert!(text.len() <= STDERR_TAIL_BYTES);
        assert!(text.ends_with(&format!("line 0999 {}", "x".repeat(40))));
        assert!(!text.contains("line 0000"));
        // One enormous line is cut to its end, not kept whole.
        let mut tail = StderrTail::default();
        tail.push("y".repeat(100_000));
        assert!(tail.text().len() <= STDERR_TAIL_BYTES);
    }

    // --- GroupChild: a leader the caller spawned, stopped TERM-then-KILL ---

    fn pgid_of(pid: u32) -> i32 {
        // SAFETY: getpgid on a pid has no memory-safety requirements.
        unsafe { libc::getpgid(pid as i32) }
    }

    fn read_pid(file: &std::path::Path) -> u32 {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !file.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(50));
        std::fs::read_to_string(file)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    #[test]
    fn lead_own_group_makes_the_child_a_group_leader() {
        let mut command = sh("sleep 30");
        lead_own_group(&mut command);
        let mut child = command.spawn().unwrap();
        assert_eq!(pgid_of(child.id()), child.id() as i32);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The peek that makes "signal the group, then reap" possible: it reports
    /// an exit and leaves the zombie, so the pid is still ours.
    #[test]
    fn exited_unreaped_sees_an_exit_without_reaping_it() {
        let mut command = sh("exit 3");
        lead_own_group(&mut command);
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !exited_unreaped(&mut child).unwrap() {
            assert!(std::time::Instant::now() < deadline, "never saw the exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Still there to be reaped: the pid has not been released.
        assert!(alive(child.id()), "the peek reaped the child");
        assert_eq!(child.wait().unwrap().code(), Some(3));
    }

    #[test]
    fn exited_unreaped_is_false_while_the_child_runs() {
        let mut command = sh("sleep 30");
        lead_own_group(&mut command);
        let mut child = command.spawn().unwrap();
        assert!(!exited_unreaped(&mut child).unwrap());
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn stopping_a_group_child_that_exits_on_term_is_quick_and_takes_its_group() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let mut bystander = Command::new("sleep").arg("60").spawn().unwrap();
        let mut child = GroupChild::spawn(sh(&format!(
            "sleep 600 & echo $! > '{}'; wait",
            pid_file.display()
        )))
        .unwrap();
        let grandchild = read_pid(&pid_file);

        let started = std::time::Instant::now();
        child.stop(Duration::from_secs(5));

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} for a child that exits on TERM",
            started.elapsed()
        );
        assert!(gone_within(grandchild, Duration::from_secs(5)));
        let bystander_alive = bystander.try_wait().unwrap().is_none();
        let _ = bystander.kill();
        let _ = bystander.wait();
        assert!(
            bystander_alive,
            "the stop reached a process outside the group"
        );
    }

    #[test]
    fn stopping_a_group_child_that_ignores_term_escalates_to_kill_after_the_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("grandchild.pid");
        let mut child = GroupChild::spawn(sh(&format!(
            "trap '' TERM; sleep 600 & echo $! > '{}'; wait",
            pid_file.display()
        )))
        .unwrap();
        let leader = child.pid();
        let grandchild = read_pid(&pid_file);

        let started = std::time::Instant::now();
        child.stop(Duration::from_millis(400));
        let took = started.elapsed();

        assert!(
            took >= Duration::from_millis(400),
            "gave up early: {took:?}"
        );
        assert!(took < Duration::from_secs(3), "took {took:?}");
        assert!(gone_within(leader, Duration::from_secs(5)));
        assert!(gone_within(grandchild, Duration::from_secs(5)));
    }

    #[test]
    fn stopping_with_no_grace_kills_at_once() {
        let mut child = GroupChild::spawn(sh("trap '' TERM; sleep 600")).unwrap();
        let leader = child.pid();
        let started = std::time::Instant::now();
        child.stop(Duration::ZERO);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(gone_within(leader, Duration::from_secs(5)));
    }

    /// Once the leader is reaped its pid may be someone else's: a second stop
    /// (or the Drop after one) must not signal the group again.
    #[test]
    fn a_stopped_group_child_is_never_signalled_again() {
        let mut child = GroupChild::spawn(sh("sleep 600")).unwrap();
        child.stop(Duration::from_millis(100));
        assert_eq!(child.group_signals, 2, "TERM and KILL, once each");
        child.stop(Duration::from_millis(100));
        assert_eq!(child.group_signals, 2, "signalled after the reap");
    }

    /// A child that exited on its own is swept, then reaped, in that order:
    /// what it left in its group dies while the leader still holds the pgid.
    #[test]
    fn a_group_child_that_exited_by_itself_has_its_group_swept_before_the_reap() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_file = tmp.path().join("left-behind.pid");
        let mut child = GroupChild::spawn(sh(&format!(
            "sleep 600 & echo $! > '{}'; exit 0",
            pid_file.display()
        )))
        .unwrap();
        let left_behind = read_pid(&pid_file);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !child.exited().unwrap() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        // The leader is a zombie, so the pgid is still ours to signal.
        assert!(alive(left_behind));
        let status = child.reap();
        assert!(status.unwrap().success());
        assert!(gone_within(left_behind, Duration::from_secs(5)));
    }
}
