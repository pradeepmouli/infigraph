//! How much a daemon is still needed (#38, #124): client processes holding a
//! lease on its read socket, and when anything last touched it. Owned by the
//! coordinator for the daemon's whole run and shared with the read service,
//! never created inside the service -- the service is rebuilt when its socket
//! is rebound (#187), and a count living there would reset under open leases.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Liveness {
    leases: AtomicUsize,
    pub(crate) last_activity: AtomicU64,
}

impl Default for Liveness {
    fn default() -> Self {
        Self::new()
    }
}

impl Liveness {
    /// Starting counts as activity, so a fresh daemon gets a full grace.
    pub fn new() -> Self {
        Self {
            leases: AtomicUsize::new(0),
            last_activity: AtomicU64::new(now_secs()),
        }
    }

    pub fn lease_opened(&self) {
        self.leases.fetch_add(1, Ordering::SeqCst);
    }

    /// Stamps activity too: the grace runs from the moment the last client
    /// left, not from its last query.
    pub fn lease_closed(&self) {
        self.touch();
        self.leases.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn touch(&self) {
        self.last_activity.store(now_secs(), Ordering::SeqCst);
    }

    /// Backdate the last activity -- for tests that need an idle daemon
    /// without waiting one out.
    #[doc(hidden)]
    pub fn last_activity_for_test(&self, secs: u64) {
        self.last_activity.store(secs, Ordering::SeqCst);
    }

    pub fn leases(&self) -> usize {
        self.leases.load(Ordering::SeqCst)
    }

    /// `None` while any lease is held -- a leased daemon is never idle.
    pub fn idle_for(&self, now_secs: u64) -> Option<Duration> {
        if self.leases() > 0 {
            return None;
        }
        let last = self.last_activity.load(Ordering::SeqCst);
        Some(Duration::from_secs(now_secs.saturating_sub(last)))
    }
}

/// Whether the daemon should exit for idleness now. `grace == 0` disables
/// the feature; in-flight work always wins, so an exit never cuts a write.
pub fn idle_exit_due(idle_for: Option<Duration>, grace: Duration, work_in_flight: bool) -> bool {
    if grace.is_zero() || work_in_flight {
        return false;
    }
    idle_for.is_some_and(|idle| idle >= grace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const G: Duration = Duration::from_secs(10);

    #[test]
    fn exit_is_due_at_the_grace_boundary_inclusive() {
        assert!(!idle_exit_due(Some(Duration::from_secs(9)), G, false));
        assert!(idle_exit_due(Some(G), G, false));
    }

    #[test]
    fn a_held_lease_blocks_exit() {
        assert!(!idle_exit_due(None, G, false));
    }

    #[test]
    fn in_flight_work_blocks_exit() {
        assert!(!idle_exit_due(Some(Duration::from_secs(3600)), G, true));
    }

    #[test]
    fn zero_grace_disables_idle_exit() {
        assert!(!idle_exit_due(
            Some(Duration::from_secs(3600)),
            Duration::ZERO,
            false
        ));
    }

    #[test]
    fn leases_count_and_closing_one_stamps_activity() {
        let l = Liveness::new();
        let start = now_secs();
        l.lease_opened();
        l.lease_opened();
        assert_eq!(l.leases(), 2);
        assert_eq!(l.idle_for(start + 100), None, "held leases mean not idle");
        l.lease_closed();
        l.lease_closed();
        assert_eq!(l.leases(), 0);
        let idle = l.idle_for(now_secs() + 5).unwrap();
        assert!(idle >= Duration::from_secs(5) && idle < Duration::from_secs(7));
    }

    #[test]
    fn touch_resets_idle_time() {
        let l = Liveness::new();
        l.last_activity
            .store(now_secs() - 100, std::sync::atomic::Ordering::Relaxed);
        assert!(l.idle_for(now_secs()).unwrap() >= Duration::from_secs(100));
        l.touch();
        assert!(l.idle_for(now_secs()).unwrap() < Duration::from_secs(2));
    }
}
