//! The read/checkpoint gate: no lbug checkpoint runs while another statement is
//! executing on the same `Database`.
//!
//! lbug 0.20.4 lets a checkpoint (an explicit `CHECKPOINT`, the one a `COPY FROM`
//! forces at its commit, one at the threshold) overlap a read on another
//! connection, and a reader scanning a string or CSR column then dereferences a
//! frame the checkpoint is rewriting (SIGSEGV in `optimisticRead`). Reproduced on
//! sittir graph copies: 3 of 3 runs of a `COPY CALLS` commit with readers died
//! within ~40 rounds; the same commit under this gate ran 379 times clean.
//!
//! A statement holds the gate **shared**; a checkpoint holds it **exclusive**.
//! A waiting exclusive blocks new shared holders (a turnstile), so a stream of
//! reads cannot starve a checkpoint. There is no bypass.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    /// Statements executing.
    shared: usize,
    /// An exclusive holder is in.
    exclusive: bool,
    /// Exclusive requests waiting; while nonzero, new shared requests wait.
    waiting_exclusive: usize,
    /// Shared requests currently blocked (observed by tests, never read in production).
    blocked_shared: usize,
}

#[derive(Default)]
pub struct FoldGate {
    state: Mutex<State>,
    changed: Condvar,
}

pub struct Shared<'a>(&'a FoldGate);
pub struct Exclusive<'a>(&'a FoldGate);

impl FoldGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold the gate for one statement. Waits while a checkpoint runs or waits.
    pub fn shared(&self) -> Shared<'_> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.exclusive || st.waiting_exclusive > 0 {
            st.blocked_shared += 1;
            while st.exclusive || st.waiting_exclusive > 0 {
                st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            st.blocked_shared -= 1;
        }
        st.shared += 1;
        Shared(self)
    }

    /// Hold the gate for a checkpoint. Waits for every running statement, and
    /// turns new ones away meanwhile.
    pub fn exclusive(&self) -> Exclusive<'_> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.waiting_exclusive += 1;
        while st.exclusive || st.shared > 0 {
            st = self.changed.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.waiting_exclusive -= 1;
        st.exclusive = true;
        Exclusive(self)
    }

    /// [`exclusive`](Self::exclusive), giving up after `wait`.
    pub fn try_exclusive_for(&self, wait: Duration) -> Option<Exclusive<'_>> {
        let deadline = Instant::now() + wait;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.waiting_exclusive += 1;
        while st.exclusive || st.shared > 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                st.waiting_exclusive -= 1;
                // Statements this request was turning away may go in now.
                self.changed.notify_all();
                return None;
            }
            st = self
                .changed
                .wait_timeout(st, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.waiting_exclusive -= 1;
        st.exclusive = true;
        Some(Exclusive(self))
    }

    #[cfg(test)]
    pub(super) fn waiting_exclusive(&self) -> usize {
        self.state.lock().unwrap().waiting_exclusive
    }

    #[cfg(test)]
    pub(super) fn blocked_shared(&self) -> usize {
        self.state.lock().unwrap().blocked_shared
    }

    #[cfg(test)]
    pub(super) fn try_shared(&self) -> Option<Shared<'_>> {
        let mut st = self.state.lock().unwrap();
        if st.exclusive || st.waiting_exclusive > 0 {
            return None;
        }
        st.shared += 1;
        Some(Shared(self))
    }
}

impl Drop for Shared<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.shared -= 1;
        if st.shared == 0 {
            self.0.changed.notify_all();
        }
    }
}

impl Drop for Exclusive<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.exclusive = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Spin until `cond`; every wait in this module is on state the gate itself
    /// reports, never on a sleep that decides the outcome.
    fn until(cond: impl Fn() -> bool) {
        let t = Instant::now();
        while !cond() {
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "condition never held"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_running_statement_holds_off_a_checkpoint() {
        let gate = FoldGate::new();
        let reader = gate.shared();
        assert!(
            gate.try_exclusive_for(Duration::ZERO).is_none(),
            "an exclusive holder got in while a statement was executing"
        );
        drop(reader);
        assert!(gate.try_exclusive_for(Duration::ZERO).is_some());
    }

    #[test]
    fn a_waiting_checkpoint_holds_off_new_statements() {
        let gate = FoldGate::new();
        let got_exclusive = AtomicBool::new(false);
        let reader_ran = AtomicBool::new(false);
        let first = gate.shared();
        std::thread::scope(|s| {
            let fold = s.spawn(|| {
                let _x = gate.exclusive();
                got_exclusive.store(true, Ordering::SeqCst);
            });
            until(|| gate.waiting_exclusive() == 1);
            // The turnstile: a new statement must not slip in beside the waiting fold.
            assert!(
                gate.try_shared().is_none(),
                "a new statement overtook the waiting checkpoint"
            );
            let late = s.spawn(|| {
                let _r = gate.shared();
                reader_ran.store(true, Ordering::SeqCst);
            });
            until(|| gate.blocked_shared() == 1);
            assert!(!reader_ran.load(Ordering::SeqCst));
            assert!(
                !got_exclusive.load(Ordering::SeqCst),
                "the fold ran beside a statement"
            );
            drop(first);
            fold.join().unwrap();
            late.join().unwrap();
        });
        assert!(got_exclusive.load(Ordering::SeqCst) && reader_ran.load(Ordering::SeqCst));
    }

    #[test]
    fn an_optional_checkpoint_gives_up_within_its_bound_and_leaves_no_trace() {
        let gate = FoldGate::new();
        let _reader = gate.shared();
        let t = Instant::now();
        assert!(gate.try_exclusive_for(Duration::from_millis(50)).is_none());
        assert!(t.elapsed() >= Duration::from_millis(50));
        assert!(
            t.elapsed() < Duration::from_secs(10),
            "the bound was not honoured"
        );
        // A fold that gave up must not keep turning statements away.
        assert_eq!(gate.waiting_exclusive(), 0);
        assert!(gate.try_shared().is_some());
    }
}
