//! Whether this daemon may start a full reindex now (#150).
//!
//! A full reindex is the heaviest thing a daemon does, and every project's
//! daemon decides on its own to run one (a client asked, compaction is due,
//! a faulted graph needs rebuilding). The machine-wide pool in
//! [`crate::slots`] caps how many run at once; this is the daemon's side of
//! it: ask before starting, and if no slot is free, do not start yet.
//!
//! Waiting holds nothing. The request stays in the coordinator's deferred
//! queue and is asked about again on the next tick, so no lock is taken and
//! the graph keeps being read as it was. There is no order between waiting
//! daemons: whichever asks first after a slot frees gets it. A daemon that
//! dies holding a slot frees it at once, because the slot is an OS file
//! lock.
//!
//! A wait is bounded by [`MAX_SLOT_WAIT`]. Past it the rebuild starts
//! without a slot, which is exactly what happened before the cap existed:
//! one daemon with a wedged rebuild must not hold every other project's
//! reindex back for ever.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::degraded::{self, DegradedMode};
use crate::slots::{Slot, SlotPool};

/// The longest a full reindex waits for a slot before starting without one.
pub const MAX_SLOT_WAIT: Duration = Duration::from_secs(600);

/// What [`ReindexGate::admit`] decided.
pub(crate) enum Admission {
    /// Start now. The slot, when there is one, is held until the rebuild
    /// has been swapped in.
    Go(Option<Slot>),
    /// Every slot is taken: ask again next tick.
    Wait,
}

/// One daemon's view of the reindex cap.
pub(crate) struct ReindexGate {
    /// `None`: no cap applies to this process.
    pool: Option<SlotPool>,
    /// When the rebuild now waiting first found no slot free.
    waiting_since: Option<Instant>,
    /// A faulted graph's rebuild is pending; it does not wait.
    recovery_pending: bool,
    warned_unavailable: bool,
}

impl ReindexGate {
    /// The machine's cap. Tests are not capped by the developer's own
    /// daemons; a test that is about the cap builds its own with [`with`].
    ///
    /// [`with`]: ReindexGate::with
    pub(crate) fn machine() -> Self {
        Self::with(if cfg!(test) {
            None
        } else {
            SlotPool::full_reindexes()
        })
    }

    pub(crate) fn with(pool: Option<SlotPool>) -> Self {
        Self {
            pool,
            waiting_since: None,
            recovery_pending: false,
            warned_unavailable: false,
        }
    }

    /// The next rebuild is the recovery of a faulted graph. That project is
    /// unusable until it is rebuilt, so it does not queue behind another
    /// project's housekeeping.
    pub(crate) fn recovery_is_pending(&mut self) {
        self.recovery_pending = true;
    }

    /// May a full reindex of `root` start at `now`?
    pub(crate) fn admit(&mut self, root: &Path, now: Instant) -> Admission {
        let Some(pool) = &self.pool else {
            return Admission::Go(None);
        };
        if std::mem::take(&mut self.recovery_pending) {
            self.stopped_waiting(root);
            return Admission::Go(None);
        }
        match pool.try_claim() {
            Ok(Some(slot)) => {
                self.stopped_waiting(root);
                Admission::Go(Some(slot))
            }
            Ok(None) => {
                let since = *self.waiting_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= MAX_SLOT_WAIT {
                    eprintln!(
                        "[daemon] full-reindex: no machine-wide slot freed in {}s; starting \
                         without one",
                        MAX_SLOT_WAIT.as_secs()
                    );
                    self.stopped_waiting(root);
                    return Admission::Go(None);
                }
                // `set` logs the first report of the episode itself.
                degraded::live::set(
                    root,
                    DegradedMode::ReindexWaitingForSlot { limit: pool.max() },
                );
                Admission::Wait
            }
            // Fail open: a cap that cannot be read must not stop a rebuild.
            Err(e) => {
                if !std::mem::replace(&mut self.warned_unavailable, true) {
                    eprintln!(
                        "[daemon] full-reindex: reindex slots unavailable ({e:#}); running \
                         uncapped"
                    );
                }
                self.stopped_waiting(root);
                Admission::Go(None)
            }
        }
    }

    /// Nothing is waiting any more: the rebuild started, or its request was
    /// withdrawn. The next one to wait gets a full wait of its own.
    pub(crate) fn stopped_waiting(&mut self, root: &Path) {
        if self.waiting_since.take().is_some() {
            degraded::live::clear(root, degraded::REINDEX_WAITING_FOR_SLOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(root: &Path) -> bool {
        degraded::live::for_root(root)
            .iter()
            .any(|n| n.key == degraded::REINDEX_WAITING_FOR_SLOT)
    }

    fn pool_of_one(dir: &Path) -> SlotPool {
        SlotPool::new(dir.join("slots"), 1)
    }

    #[test]
    fn a_free_slot_is_claimed_and_held_until_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = pool_of_one(tmp.path());
        let mut gate = ReindexGate::with(Some(pool.clone()));
        let Admission::Go(slot) = gate.admit(tmp.path(), Instant::now()) else {
            panic!("a free slot was not granted");
        };
        assert!(slot.is_some(), "a capped rebuild started without a slot");
        assert!(pool.try_claim().unwrap().is_none(), "the slot is not held");
        drop(slot);
        assert!(pool.try_claim().unwrap().is_some());
    }

    #[test]
    fn with_every_slot_taken_the_rebuild_waits_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = pool_of_one(tmp.path());
        let other_daemon = pool.try_claim().unwrap().unwrap();
        let mut gate = ReindexGate::with(Some(pool));
        let now = Instant::now();

        assert!(matches!(gate.admit(tmp.path(), now), Admission::Wait));
        assert!(waiting(tmp.path()), "the wait is not reported");

        drop(other_daemon);
        let later = now + Duration::from_secs(30);
        assert!(matches!(
            gate.admit(tmp.path(), later),
            Admission::Go(Some(_))
        ));
        assert!(!waiting(tmp.path()), "still reported after it started");
    }

    #[test]
    fn a_wait_ends_at_the_limit_and_the_rebuild_starts_without_a_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = pool_of_one(tmp.path());
        let _wedged = pool.try_claim().unwrap().unwrap();
        let mut gate = ReindexGate::with(Some(pool));
        let t0 = Instant::now();

        assert!(matches!(gate.admit(tmp.path(), t0), Admission::Wait));
        let almost = t0 + MAX_SLOT_WAIT - Duration::from_secs(1);
        assert!(matches!(gate.admit(tmp.path(), almost), Admission::Wait));
        assert!(matches!(
            gate.admit(tmp.path(), t0 + MAX_SLOT_WAIT),
            Admission::Go(None)
        ));
        assert!(!waiting(tmp.path()));

        // The next rebuild gets its own full wait, not the old clock.
        let next = t0 + MAX_SLOT_WAIT + Duration::from_secs(5);
        assert!(matches!(gate.admit(tmp.path(), next), Admission::Wait));
    }

    #[test]
    fn a_faulted_graphs_rebuild_does_not_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = pool_of_one(tmp.path());
        let _busy = pool.try_claim().unwrap().unwrap();
        let mut gate = ReindexGate::with(Some(pool));
        gate.recovery_is_pending();
        assert!(matches!(
            gate.admit(tmp.path(), Instant::now()),
            Admission::Go(None)
        ));
        // Only that rebuild: the one after it is capped again.
        assert!(matches!(
            gate.admit(tmp.path(), Instant::now()),
            Admission::Wait
        ));
    }

    #[test]
    fn a_pool_that_cannot_be_used_does_not_stop_the_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        // The pool's directory is a file: every claim fails.
        std::fs::write(tmp.path().join("slots"), b"").unwrap();
        let mut gate = ReindexGate::with(Some(pool_of_one(tmp.path())));
        assert!(matches!(
            gate.admit(tmp.path(), Instant::now()),
            Admission::Go(None)
        ));
        assert!(gate.warned_unavailable);
        assert!(!waiting(tmp.path()));
    }

    #[test]
    fn a_withdrawn_request_ends_the_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = pool_of_one(tmp.path());
        let _busy = pool.try_claim().unwrap().unwrap();
        let mut gate = ReindexGate::with(Some(pool));
        let t0 = Instant::now();
        assert!(matches!(gate.admit(tmp.path(), t0), Admission::Wait));

        gate.stopped_waiting(tmp.path());
        assert!(!waiting(tmp.path()));
        // A later request starts its own wait from when it first asks.
        let later = t0 + MAX_SLOT_WAIT;
        assert!(matches!(gate.admit(tmp.path(), later), Admission::Wait));
    }

    #[test]
    fn no_cap_means_no_slot_and_no_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let mut gate = ReindexGate::with(None);
        assert!(matches!(
            gate.admit(tmp.path(), Instant::now()),
            Admission::Go(None)
        ));
    }
}
