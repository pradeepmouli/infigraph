//! Where a long operation spends its time and memory: a wall-clock lap per
//! named phase, each with the process's peak resident size so far, so the
//! phase that raises it is the one named. A SCIP import on a 220k-symbol graph
//! held 2 GB and 693% CPU for minutes with nothing in the log saying which step
//! did it.

use std::time::{Duration, Instant};

/// The process's peak resident set size so far, in bytes. `None` off unix.
pub fn peak_rss_bytes() -> Option<u64> {
    #[cfg(unix)]
    {
        // SAFETY: `getrusage` writes a plain struct we own and zeroed.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return None;
        }
        let raw = u64::try_from(usage.ru_maxrss).ok()?;
        // macOS reports bytes, Linux kilobytes.
        Some(if cfg!(target_os = "macos") {
            raw
        } else {
            raw * 1024
        })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn mebibytes(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// Laps of one operation; see the module doc.
pub struct PhaseClock {
    started: Instant,
    last: Instant,
    laps: Vec<(&'static str, Duration, Option<u64>)>,
}

impl PhaseClock {
    pub fn start() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last: now,
            laps: Vec::new(),
        }
    }

    /// Ends the current phase under `name`.
    pub fn lap(&mut self, name: &'static str) {
        let now = Instant::now();
        self.laps.push((name, now - self.last, peak_rss_bytes()));
        self.last = now;
    }

    /// Whether a phase named `name` has been lapped.
    pub fn has_lap(&self, name: &str) -> bool {
        self.laps.iter().any(|(lap, ..)| *lap == name)
    }

    /// Time since the clock started.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// `preload 1.2s (peak 900 MB), parse 0.4s (peak 900 MB), ... total 9.9s`.
    pub fn report(&self) -> String {
        let mut parts: Vec<String> = self
            .laps
            .iter()
            .map(|(name, took, rss)| match rss {
                Some(bytes) => format!(
                    "{name} {:.1}s (peak {} MB)",
                    took.as_secs_f64(),
                    mebibytes(*bytes)
                ),
                None => format!("{name} {:.1}s", took.as_secs_f64()),
            })
            .collect();
        parts.push(format!(
            "total {:.1}s",
            self.started.elapsed().as_secs_f64()
        ));
        parts.join(", ")
    }
}

/// An operation slower than this leaves a line in the daemon's log.
pub const SLOW_OP: Duration = Duration::from_secs(5);

/// Logs `[daemon] <time> <op> took N.Ns` when it is dropped, if the operation
/// it brackets took [`SLOW_OP`] or more. A daemon at 693% CPU for minutes had
/// no line saying what it was doing; every read, drain and write that takes
/// seconds now names itself.
pub struct SlowOp<'a> {
    /// Named only if the operation turns out slow: a request's name can cost
    /// as much to build as the request does to serve.
    op: Option<Box<dyn FnOnce() -> String + 'a>>,
    started: Instant,
}

impl<'a> SlowOp<'a> {
    pub fn start(op: impl FnOnce() -> String + 'a) -> Self {
        Self {
            op: Some(Box::new(op)),
            started: Instant::now(),
        }
    }

    /// The log line for `op` having taken `took`, if that is slow.
    fn line(op: &str, took: Duration, stamp: &str) -> Option<String> {
        (took >= SLOW_OP).then(|| format!("[daemon] {stamp} {op} took {:.1}s", took.as_secs_f64()))
    }
}

impl Drop for SlowOp<'_> {
    fn drop(&mut self) {
        let took = self.started.elapsed();
        if took < SLOW_OP {
            return;
        }
        if let Some(op) = self.op.take() {
            if let Some(line) = Self::line(&op(), took, &crate::watch::log_stamp()) {
                eprintln!("{line}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_lap_is_named_and_the_total_closes_the_report() {
        let mut clock = PhaseClock::start();
        clock.lap("preload");
        clock.lap("parse");
        let report = clock.report();
        assert!(report.starts_with("preload 0."), "{report}");
        assert!(report.contains(", parse 0."), "{report}");
        assert!(report.contains(", total 0."), "{report}");
    }

    #[cfg(unix)]
    #[test]
    fn the_peak_resident_size_is_reported_in_megabytes_on_unix() {
        let bytes = peak_rss_bytes().expect("getrusage works");
        assert!(bytes > 1024 * 1024, "{bytes}");
        let mut clock = PhaseClock::start();
        clock.lap("x");
        assert!(clock.report().contains(" MB)"), "{}", clock.report());
    }
    #[test]
    fn only_an_operation_of_five_seconds_or_more_is_logged_with_its_name() {
        let t = "2026-10-19T00:00:00Z";
        assert_eq!(SlowOp::line("drain", Duration::from_millis(4_999), t), None);
        assert_eq!(
            SlowOp::line("drain", Duration::from_millis(12_340), t).as_deref(),
            Some("[daemon] 2026-10-19T00:00:00Z drain took 12.3s")
        );
    }
}
