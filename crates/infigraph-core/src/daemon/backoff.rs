//! Exponential backoff for the coordinator's "reopen the graph connection"
//! attempts.
//!
//! Before this, both reopen sites in the coordinator (`serve_request_locked`
//! and the drain scheduler) retried on every 200ms tick for as long as the
//! graph stayed locked by another process. Each attempt costs a 3-second
//! lock-wait budget and -- until `open_kuzu_with_retry` stopped re-opening
//! to poll -- leaked file descriptors; the sittir daemon logged 891 such
//! retries over 24 hours against a holder that never went away. A held
//! lock is not a transient condition worth hammering: back off from 5s up
//! to 10 minutes between attempts, and reset the moment one succeeds.

use std::time::{Duration, Instant};

const INITIAL: Duration = Duration::from_secs(5);
const MAX: Duration = Duration::from_secs(600);

#[derive(Debug)]
pub(crate) struct ReopenBackoff {
    consecutive_failures: u32,
    retry_after: Option<Instant>,
}

impl ReopenBackoff {
    pub(crate) fn new() -> Self {
        Self {
            consecutive_failures: 0,
            retry_after: None,
        }
    }

    /// Whether a reopen may be attempted now.
    pub(crate) fn should_attempt(&self) -> bool {
        self.should_attempt_at(Instant::now())
    }

    fn should_attempt_at(&self, now: Instant) -> bool {
        self.retry_after.is_none_or(|t| now >= t)
    }

    /// Record a failed reopen; the next attempt is allowed only after the
    /// (doubling, capped) delay. Returns that delay so the caller can log it.
    pub(crate) fn record_failure(&mut self) -> Duration {
        self.record_failure_at(Instant::now())
    }

    fn record_failure_at(&mut self, now: Instant) -> Duration {
        let delay = Self::delay_for(self.consecutive_failures);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.retry_after = Some(now + delay);
        delay
    }

    pub(crate) fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.retry_after = None;
    }

    pub(crate) fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    fn delay_for(failures: u32) -> Duration {
        INITIAL
            .checked_mul(1u32.checked_shl(failures).unwrap_or(u32::MAX))
            .unwrap_or(MAX)
            .min(MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_backoff_allows_an_attempt() {
        assert!(ReopenBackoff::new().should_attempt());
    }

    #[test]
    fn failure_blocks_until_delay_elapses_then_doubles() {
        let t0 = Instant::now();
        let mut b = ReopenBackoff::new();
        assert_eq!(b.record_failure_at(t0), Duration::from_secs(5));
        assert!(!b.should_attempt_at(t0 + Duration::from_secs(4)));
        assert!(b.should_attempt_at(t0 + Duration::from_secs(5)));
        assert_eq!(b.record_failure_at(t0), Duration::from_secs(10));
        assert_eq!(b.record_failure_at(t0), Duration::from_secs(20));
        assert_eq!(b.consecutive_failures(), 3);
    }

    #[test]
    fn delay_caps_at_ten_minutes_and_never_overflows() {
        let mut b = ReopenBackoff::new();
        let t0 = Instant::now();
        let mut last = Duration::ZERO;
        for _ in 0..40 {
            last = b.record_failure_at(t0);
        }
        assert_eq!(last, MAX);
    }

    #[test]
    fn success_resets_to_immediate_attempts() {
        let t0 = Instant::now();
        let mut b = ReopenBackoff::new();
        b.record_failure_at(t0);
        b.record_failure_at(t0);
        b.record_success();
        assert!(b.should_attempt_at(t0));
        assert_eq!(b.consecutive_failures(), 0);
        assert_eq!(b.record_failure_at(t0), Duration::from_secs(5));
    }
}
