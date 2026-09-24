//! R5.2 (#19): a long-lived process watches its own resource use.
//!
//! The MCP worker and the daemon run for days, and a slow leak in either
//! used to show up only as hours of degradation (I-1; #150's daemon peaked
//! at 6.8 GB). Each now samples its own resident memory, open file
//! descriptors and threads against two ceilings:
//!
//! - **soft**: log once per episode and drop what can be rebuilt (search
//!   caches, the daemon's idle graph handle);
//! - **hard**: restart cleanly at the next moment nothing is in flight.
//!   State is all on disk, so a restart is cheap; a write is never cut off.
//!
//! Ceilings default to fractions of the machine (physical memory, the
//! descriptor limit) rather than fixed numbers; every one is settable under
//! `[watchdog]` in `config.toml` or as `INFIGRAPH_WATCHDOG_*`.

use std::time::{Duration, Instant};

use clap::Parser;

crate::settings! {
    watchdog {
        rss_soft_pct: u64 = 25,
        rss_hard_pct: u64 = 50,
        fd_soft_pct: u64 = 70,
        fd_hard_pct: u64 = 90,
        threads_soft: u64 = 500,
        threads_hard: u64 = 1000,
        interval_secs: u64 = 30,
    }
}

fn settings() -> Watchdog {
    let cli = RawWatchdog::parse_from(std::iter::empty::<String>());
    Watchdog::resolve(cli, crate::settings_file::ConfigScope::User)
}

/// What a process is using right now. `None` where the platform cannot
/// say, which never counts as a breach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub rss_bytes: Option<u64>,
    pub fds: Option<u64>,
    pub threads: Option<u64>,
}

impl Usage {
    /// This process, now.
    pub fn sample() -> Self {
        Self {
            rss_bytes: own_rss_bytes(),
            fds: own_fd_count(),
            threads: own_thread_count(),
        }
    }
}

/// One resource's two limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub soft: u64,
    pub hard: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceilings {
    pub rss_bytes: Option<Limit>,
    pub fds: Option<Limit>,
    pub threads: Limit,
}

/// The watchdog's reading of one sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Healthy,
    /// Over a soft ceiling: log and drop caches. Names what is over.
    Soft(String),
    /// Over a hard ceiling: restart once idle. Names what is over.
    Hard(String),
}

impl Ceilings {
    /// The configured ceilings, scaled to this machine.
    pub fn configured() -> Self {
        Self::for_machine(&settings(), total_memory_bytes(), fd_limit())
    }

    fn for_machine(s: &Watchdog, memory: Option<u64>, fd_limit: Option<u64>) -> Self {
        let pct = |total: u64, p: u64| (u128::from(total) * u128::from(p) / 100) as u64;
        Self {
            rss_bytes: memory.map(|m| Limit {
                soft: pct(m, s.rss_soft_pct),
                hard: pct(m, s.rss_hard_pct),
            }),
            fds: fd_limit.map(|l| Limit {
                soft: pct(l, s.fd_soft_pct).max(1),
                hard: pct(l, s.fd_hard_pct).max(1),
            }),
            threads: Limit {
                soft: s.threads_soft,
                hard: s.threads_hard,
            },
        }
    }

    /// Pure: compare `usage` to these ceilings. A hard breach outranks a
    /// soft one; the reason names every resource over its limit.
    pub fn judge(&self, usage: &Usage) -> Verdict {
        let checks = [
            ("resident memory", usage.rss_bytes, self.rss_bytes, true),
            ("open file descriptors", usage.fds, self.fds, false),
            ("threads", usage.threads, Some(self.threads), false),
        ];
        let mut hard = Vec::new();
        let mut soft = Vec::new();
        for (name, used, limit, bytes) in checks {
            let (Some(used), Some(limit)) = (used, limit) else {
                continue;
            };
            let show = |n: u64| {
                if bytes {
                    format!("{} MB", n >> 20)
                } else {
                    n.to_string()
                }
            };
            if used >= limit.hard {
                hard.push(format!(
                    "{name} {} >= hard ceiling {}",
                    show(used),
                    show(limit.hard)
                ));
            } else if used >= limit.soft {
                soft.push(format!(
                    "{name} {} >= soft ceiling {}",
                    show(used),
                    show(limit.soft)
                ));
            }
        }
        if !hard.is_empty() {
            Verdict::Hard(hard.join("; "))
        } else if !soft.is_empty() {
            Verdict::Soft(soft.join("; "))
        } else {
            Verdict::Healthy
        }
    }
}

/// What the owner of a [`SelfWatch`] should do after a check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing to do (healthy, not yet time to sample, or a soft breach
    /// already acted on this episode).
    None,
    /// A new soft breach: drop what can be rebuilt. Already logged.
    DropCaches,
    /// A hard breach: restart at the next idle moment. Already logged.
    Restart(String),
}

/// A process's watchdog: samples on an interval and turns verdicts into
/// actions, logging each soft episode once rather than every interval.
pub struct SelfWatch {
    who: &'static str,
    ceilings: Ceilings,
    interval: Duration,
    last: Option<Instant>,
    in_soft_episode: bool,
}

impl SelfWatch {
    /// `who` prefixes every log line (`"daemon"`, `"mcp"`).
    pub fn new(who: &'static str) -> Self {
        Self::with(
            who,
            Ceilings::configured(),
            Duration::from_secs(settings().interval_secs),
        )
    }

    pub fn with(who: &'static str, ceilings: Ceilings, interval: Duration) -> Self {
        Self {
            who,
            ceilings,
            interval,
            last: None,
            in_soft_episode: false,
        }
    }

    /// Sample if the interval has passed, and say what to do.
    pub fn check(&mut self, now: Instant) -> Action {
        if self
            .last
            .is_some_and(|t| now.duration_since(t) < self.interval)
        {
            return Action::None;
        }
        self.last = Some(now);
        self.act(&Usage::sample())
    }

    /// Turn one sample into an action. Split from `check` so the episode
    /// logic is testable against any usage.
    pub fn act(&mut self, usage: &Usage) -> Action {
        match self.ceilings.judge(usage) {
            Verdict::Healthy => {
                if self.in_soft_episode {
                    eprintln!("[{}] watchdog: back under every soft ceiling", self.who);
                }
                self.in_soft_episode = false;
                Action::None
            }
            Verdict::Soft(_) if self.in_soft_episode => Action::None,
            Verdict::Soft(why) => {
                self.in_soft_episode = true;
                eprintln!("[{}] watchdog: {why} -- dropping caches", self.who);
                Action::DropCaches
            }
            Verdict::Hard(why) => {
                eprintln!(
                    "[{}] watchdog: {why} -- restarting once nothing is in flight",
                    self.who
                );
                Action::Restart(why)
            }
        }
    }
}

fn total_memory_bytes() -> Option<u64> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    Some(sys.total_memory()).filter(|m| *m > 0)
}

fn own_rss_bytes() -> Option<u64> {
    let pid = sysinfo::Pid::from_u32(std::process::id());
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid).map(|p| p.memory())
}

#[cfg(unix)]
fn fd_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into the struct we pass and nothing else.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return None;
    }
    // RLIM_INFINITY is not a limit to take a fraction of. `rlim_t` is u64
    // here but not on every unix, hence the cast.
    #[allow(clippy::unnecessary_cast)]
    (lim.rlim_cur != libc::RLIM_INFINITY).then_some(lim.rlim_cur as u64)
}

#[cfg(not(unix))]
fn fd_limit() -> Option<u64> {
    None
}

fn own_fd_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    let dir = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    let dir = "/dev/fd";
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        std::fs::read_dir(dir).ok().map(|d| d.count() as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn own_thread_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task")
            .ok()
            .map(|d| d.count() as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
        // SAFETY: proc_pidinfo fills at most `size` bytes of `info`, which
        // is exactly a proc_taskinfo, and reports how many it wrote.
        let wrote = unsafe {
            libc::proc_pidinfo(
                std::process::id() as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size,
            )
        };
        (wrote == size).then_some(info.pti_threadnum as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn ceilings() -> Ceilings {
        let s = Watchdog {
            rss_soft_pct: 25,
            rss_hard_pct: 50,
            fd_soft_pct: 70,
            fd_hard_pct: 90,
            threads_soft: 500,
            threads_hard: 1000,
            interval_secs: 30,
        };
        Ceilings::for_machine(&s, Some(16 * GB), Some(1000))
    }

    fn usage(rss_gb: u64, fds: u64, threads: u64) -> Usage {
        Usage {
            rss_bytes: Some(rss_gb * GB),
            fds: Some(fds),
            threads: Some(threads),
        }
    }

    #[test]
    fn ceilings_scale_with_the_machine() {
        let c = ceilings();
        assert_eq!(
            c.rss_bytes,
            Some(Limit {
                soft: 4 * GB,
                hard: 8 * GB
            })
        );
        assert_eq!(
            c.fds,
            Some(Limit {
                soft: 700,
                hard: 900
            })
        );
    }

    #[test]
    fn a_process_under_every_ceiling_is_healthy() {
        assert_eq!(ceilings().judge(&usage(1, 50, 20)), Verdict::Healthy);
        assert_eq!(ceilings().judge(&Usage::default()), Verdict::Healthy);
    }

    #[test]
    fn soft_and_hard_breaches_name_what_is_over_and_hard_wins() {
        match ceilings().judge(&usage(5, 50, 20)) {
            Verdict::Soft(why) => assert!(why.contains("resident memory 5120 MB"), "{why}"),
            other => panic!("{other:?}"),
        }
        match ceilings().judge(&usage(5, 950, 20)) {
            Verdict::Hard(why) => {
                assert!(why.contains("open file descriptors 950"), "{why}");
                assert!(
                    !why.contains("resident memory"),
                    "only hard breaches: {why}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_soft_episode_drops_caches_once_and_rearms_after_recovery() {
        let mut w = SelfWatch::with("test", ceilings(), Duration::ZERO);
        assert_eq!(w.act(&usage(5, 50, 20)), Action::DropCaches);
        assert_eq!(w.act(&usage(5, 50, 20)), Action::None, "same episode");
        assert_eq!(w.act(&usage(1, 50, 20)), Action::None);
        assert_eq!(
            w.act(&usage(5, 50, 20)),
            Action::DropCaches,
            "a new episode"
        );
        assert!(matches!(w.act(&usage(9, 50, 20)), Action::Restart(_)));
    }

    #[test]
    fn check_samples_only_once_per_interval() {
        let mut w = SelfWatch::with("test", ceilings(), Duration::from_secs(30));
        let t = Instant::now();
        w.check(t);
        assert_eq!(w.last, Some(t));
        w.check(t + Duration::from_secs(10));
        assert_eq!(w.last, Some(t), "too soon to sample again");
    }

    #[test]
    fn this_process_can_be_sampled() {
        let u = Usage::sample();
        assert!(u.rss_bytes.is_some_and(|b| b > 0));
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(u.fds.is_some_and(|n| n > 0));
            assert!(u.threads.is_some_and(|n| n > 0));
        }
    }
}
