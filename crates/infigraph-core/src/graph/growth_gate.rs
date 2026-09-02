//! Continuous form of the runaway-growth circuit breaker (#132 gap 1).
//!
//! `store_util::check_graph_growth_ratio` runs as a *preflight* at the
//! start of each write call, so a single long call (thousands of SCIP
//! symbols, a 20-attempt COPY retry loop, a big incremental batch) could
//! push the graph far past the cap before the guard got another look. A
//! `GrowthGate` re-runs the same check every `every` iterations inside
//! such loops; the closure is injected so the cadence is unit-testable
//! without staging real on-disk growth.

use anyhow::{anyhow, Result};

use super::GraphStore;

pub(crate) struct GrowthGate<F: FnMut() -> std::result::Result<(), String>> {
    every: usize,
    seen: usize,
    check: F,
}

impl<F: FnMut() -> std::result::Result<(), String>> GrowthGate<F> {
    pub(crate) fn new(every: usize, check: F) -> Self {
        Self {
            every,
            seen: 0,
            check,
        }
    }

    /// Call once per loop iteration; runs the check on every `every`-th
    /// call and turns a refusal into the same "refusing to index --" error
    /// the call-boundary preflight raises.
    pub(crate) fn tick(&mut self) -> Result<()> {
        self.seen += 1;
        if self.every > 0 && self.seen.is_multiple_of(self.every) {
            (self.check)().map_err(|msg| anyhow!("refusing to index -- {msg}"))?;
        }
        Ok(())
    }
}

impl GraphStore {
    /// A gate over this store's own `.infigraph/graph` (+ WAL siblings),
    /// using the same baseline and cap as the preflight. A no-op gate for
    /// an in-memory store.
    pub(crate) fn growth_gate(
        &self,
        every: usize,
    ) -> GrowthGate<impl FnMut() -> std::result::Result<(), String> + '_> {
        let dir = self.db_dir();
        GrowthGate::new(every, move || match dir {
            Some(d) => super::store_util::check_graph_growth_ratio(d, &d.join("graph")),
            None => Ok(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::GrowthGate;

    #[test]
    fn runs_the_check_every_n_ticks_and_not_before() {
        let mut calls = 0usize;
        {
            let mut gate = GrowthGate::new(3, || {
                calls += 1;
                Ok(())
            });
            for _ in 0..2 {
                gate.tick().unwrap();
            }
        }
        assert_eq!(calls, 0, "no check before the cadence is reached");

        let mut calls = 0usize;
        {
            let mut gate = GrowthGate::new(3, || {
                calls += 1;
                Ok(())
            });
            for _ in 0..7 {
                gate.tick().unwrap();
            }
        }
        assert_eq!(calls, 2, "checked after the 3rd and 6th tick");
    }

    #[test]
    fn a_failing_check_stops_the_loop_with_the_refusal_message() {
        let mut gate = GrowthGate::new(1, || Err("graph is 12x its healthy size".to_string()));
        let err = gate.tick().unwrap_err().to_string();
        assert!(
            err.starts_with("refusing to index -- graph is 12x"),
            "same prefix as the call-boundary preflight so logs read alike: {err}"
        );
    }
}
