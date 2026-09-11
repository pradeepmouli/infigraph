//! Continuous form of the runaway-growth circuit breaker (#132 gap 1).
//!
//! `store_util::check_graph_growth_ratio` runs as a *preflight* at the
//! start of each write call, so a single long call (thousands of SCIP
//! symbols, a 20-attempt COPY retry loop, a big incremental batch) could
//! push the graph far past the cap before the guard got another look. A
//! `GrowthGate` re-runs the same check every `every` iterations inside
//! such loops; the closure is injected so the cadence is unit-testable
//! without staging real on-disk growth.
//!
//! [`CopyRetryBudget`] is the same idea aimed at a different axis (#157):
//! `GrowthGate` asks "is the graph too big in absolute/relative terms?",
//! which is a property of the whole store, while the budget asks "has *this
//! loop* spent more than the work it is attempting is worth?", which is a
//! property of one call. A loop can be well inside every global cap and
//! still be re-COPYing a batch twenty times over.

use anyhow::{anyhow, Result};

use super::GraphStore;

/// How every preflight guard words its refusal. One constant because the
/// wording is now load-bearing: [`is_write_refusal`] classifies on it.
pub(crate) const WRITE_REFUSED_PREFIX: &str = "refusing to index -- ";

/// Whether a failed write was *refused by a preflight guard* rather than
/// having actually gone wrong.
///
/// The distinction matters to exactly one caller and matters a lot there
/// (#161). The daemon poisons its shared read handle whenever a drain
/// fails, which is right for a failure that could have left the handle
/// stale -- a graph replaced under a live connection by a concurrent
/// `index --full` is the case it exists for. It is wrong for a refusal: a
/// preflight declines *before* the write touches the graph, so the handle
/// is exactly as valid as it was a moment earlier.
///
/// Getting that backwards made a too-large graph unreadable rather than
/// merely un-writable. Every drain was refused by the growth breaker, every
/// refusal dropped the read handle, and only a *successful* drain restores
/// it -- which the breaker guarantees will never come. sittir logged 15
/// such refusals and served "the daemon has no graph open yet" to every
/// read, advising a retry that could not help.
///
/// Walks the cause chain: the error reaches the drain-failure path wrapped
/// in context, so matching only the outermost message would miss it.
pub(crate) fn is_write_refusal(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(WRITE_REFUSED_PREFIX))
}

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
            (self.check)().map_err(|msg| anyhow!("{WRITE_REFUSED_PREFIX}{msg}"))?;
        }
        Ok(())
    }
}

/// A byte budget for a COPY retry loop, proportional to the batch the loop
/// is actually attempting (#157).
///
/// `MAX_SYMBOL_RETRIES` / `MAX_BAD_RECORD_RETRIES` bound their loops by
/// *attempt count*, and every attempt re-COPYs the whole remaining batch, so
/// the cost of exhausting one is `batch x 20` -- unbounded in bytes. Those
/// bytes are real: the #153 incident's own error text reads "Transaction
/// committed successfully, but the post-commit checkpoint failed. The
/// committed data is durable", so a *failed* attempt can still grow the
/// graph by gigabytes.
///
/// Neither existing guard closes that. `growth_max_ratio` is relative to a
/// recorded baseline and, in the incident, fired only after 20GB had been
/// written, because a single attempt can add gigabytes between ticks.
/// `graph.max_bytes` does bound the damage absolutely, but it is a blunt
/// stop that blocks *all* indexing once tripped rather than letting one loop
/// give up early and fall back cleanly.
///
/// This bounds the loop instead: once it has grown the graph by more than
/// `multiple` times the batch's own serialized size, stop retrying and take
/// the caller's fallback path. The comparison is against the size of the
/// batch as *first* attempted -- see `arm`.
pub(crate) struct CopyRetryBudget<F: FnMut() -> u64> {
    multiple: u64,
    /// `None` until `arm`; `Some((baseline_bytes, allowance_bytes))` after.
    armed: Option<(u64, u64)>,
    measure: F,
}

impl<F: FnMut() -> u64> CopyRetryBudget<F> {
    /// `multiple` of 0 disables the budget entirely.
    pub(crate) fn new(multiple: u64, measure: F) -> Self {
        Self {
            multiple,
            armed: None,
            measure,
        }
    }

    /// Record the batch's serialized size and the graph size to measure
    /// growth against. Deliberately once-only: later attempts COPY a
    /// *smaller* batch, because bad records were dropped, and re-arming from
    /// a shrinking batch would tighten the budget with every retry and
    /// abandon a loop that was in fact converging.
    ///
    /// A zero-byte batch leaves the budget disarmed -- there is nothing to
    /// be proportional to, and refusing on that basis would abandon the loop
    /// for a reason it cannot act on.
    pub(crate) fn arm(&mut self, batch_bytes: u64) {
        if self.armed.is_some() || self.multiple == 0 || batch_bytes == 0 {
            return;
        }
        self.armed = Some(((self.measure)(), batch_bytes.saturating_mul(self.multiple)));
    }

    /// `Some(reason)` once retrying again would spend more than the
    /// allowance. Never fires on the first attempt: nothing has been spent
    /// at the moment of arming, so a loop that converges immediately -- or
    /// one whose whole-batch pre-filter settles it in a single retry -- is
    /// never affected.
    pub(crate) fn exceeded(&mut self) -> Option<String> {
        let (baseline, allowance) = self.armed?;
        let spent = (self.measure)().saturating_sub(baseline);
        (spent > allowance).then(|| {
            format!(
                "the retry loop has already grown the graph by {spent} bytes, past the \
                 {allowance} it is allowed ({}x the batch it is retrying)",
                self.multiple
            )
        })
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
        // This store's own file, not the canonical `graph` name: a full
        // reindex builds at `graph.rebuilding` and must be measured against
        // that, not against the graph it is replacing (#156).
        let graph_path = self.db_path().to_path_buf();
        GrowthGate::new(every, move || match dir {
            Some(d) => super::store_util::check_graph_growth_ratio(d, &graph_path),
            None => Ok(()),
        })
    }

    /// A [`CopyRetryBudget`] over this store's own graph file (+ its WAL
    /// family, for the reason `graph_family_bytes` documents). A no-op
    /// budget for an in-memory store, which has no on-disk size to spend.
    pub(crate) fn copy_retry_budget(&self) -> CopyRetryBudget<impl FnMut() -> u64 + '_> {
        // This store's own file, not the canonical `graph` name -- a full
        // reindex builds at `graph.rebuilding` and must be measured against
        // that (#156), same as the gate above.
        let graph_path = self.db_path().to_path_buf();
        let on_disk = self.db_dir().is_some();
        CopyRetryBudget::new(
            super::store_util::copy_retry_max_batch_multiple(
                crate::settings_file::ConfigScope::of_infigraph_dir(self.db_dir()),
            ),
            move || {
                if on_disk {
                    super::store_util::graph_family_bytes(&graph_path)
                } else {
                    0
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{CopyRetryBudget, GrowthGate};
    use anyhow::{anyhow, Context};

    /// #157: `MAX_*_RETRIES` bounds a COPY retry loop by attempt count, and
    /// every attempt re-COPYs the whole remaining batch -- so the cost of
    /// exhausting the budget is `batch x 20`, unbounded in bytes. A failing
    /// COPY can still commit durable data (the #153 incident's own error
    /// text says so), so those bytes are real. The budget makes the loop's
    /// cost proportional to the work it is actually attempting.
    #[test]
    fn a_retry_budget_stops_the_loop_once_growth_outruns_its_own_batch() {
        let size = std::cell::Cell::new(1_000u64);
        let mut budget = CopyRetryBudget::new(4, || size.get());
        budget.arm(100); // allowance = 4 x 100 bytes

        assert!(
            budget.exceeded().is_none(),
            "the first attempt has cost nothing yet"
        );

        size.set(1_000 + 399);
        assert!(
            budget.exceeded().is_none(),
            "just inside the allowance must still retry"
        );

        size.set(1_000 + 401);
        let why = budget
            .exceeded()
            .expect("past the allowance must stop the loop");
        assert!(
            why.contains("401") && why.contains("400"),
            "the reason must name what was spent and what was allowed: {why}"
        );
    }

    /// A later attempt COPYs a *smaller* batch (bad records were dropped).
    /// The allowance is the cost of the work as it was first attempted, so
    /// re-arming from a shrinking batch would tighten the budget with every
    /// retry and abandon a loop that was in fact converging.
    #[test]
    fn a_retry_budget_is_armed_once_and_not_retightened_by_a_shrinking_batch() {
        let size = std::cell::Cell::new(0u64);
        let mut budget = CopyRetryBudget::new(4, || size.get());
        budget.arm(100);
        budget.arm(1);

        size.set(399);
        assert!(
            budget.exceeded().is_none(),
            "the allowance must still be 4 x the batch as first attempted"
        );
    }

    #[test]
    fn a_retry_budget_with_no_multiple_or_no_batch_never_stops_the_loop() {
        let size = std::cell::Cell::new(0u64);

        let mut disabled = CopyRetryBudget::new(0, || size.get());
        disabled.arm(100);
        size.set(u64::MAX / 2);
        assert!(disabled.exceeded().is_none(), "0 disables the budget");

        let size2 = std::cell::Cell::new(0u64);
        let mut unmeasurable = CopyRetryBudget::new(4, || size2.get());
        unmeasurable.arm(0);
        size2.set(u64::MAX / 2);
        assert!(
            unmeasurable.exceeded().is_none(),
            "a zero-byte batch gives nothing to be proportional to"
        );
    }

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

    /// #161: a refused write must be distinguishable from a failed one.
    /// Every `refusing to index --` site declines *before* touching the
    /// graph, so the daemon's live read handle is untouched -- but the
    /// drain-failure path poisoned it regardless, and only a *successful*
    /// drain restores it. With the breaker engaged that never comes, so a
    /// project that was merely too large became unreadable.
    #[test]
    fn a_gate_refusal_is_recognised_as_a_refusal_not_a_failure() {
        let mut gate = GrowthGate::new(1, || Err("graph is 31x its healthy size".to_string()));
        let err = gate.tick().unwrap_err();
        assert!(
            super::is_write_refusal(&err),
            "the gate's own error must classify as a refusal: {err}"
        );
    }

    /// The error reaches the drain-failure path wrapped in context, so
    /// matching only the outermost message would miss it.
    #[test]
    fn a_refusal_is_still_recognised_under_added_context() {
        let mut gate = GrowthGate::new(1, || Err("graph is 31x its healthy size".to_string()));
        let err = gate
            .tick()
            .context("drain batch 3")
            .context("watch loop")
            .unwrap_err();
        assert!(
            super::is_write_refusal(&err),
            "must walk the cause chain, not just the top: {err}"
        );
    }

    /// The distinction has to be narrow, or it silences the poisoning that
    /// exists for real staleness -- a graph replaced under a live
    /// connection by a concurrent `index --full`.
    #[test]
    fn an_ordinary_failure_is_not_a_refusal() {
        let err = anyhow!("Query execution failed: Invalid transaction type to rollback.");
        assert!(!super::is_write_refusal(&err));
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
