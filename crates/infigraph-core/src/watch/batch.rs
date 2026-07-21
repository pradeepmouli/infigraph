use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct ChangeBatch {
    paths: HashSet<PathBuf>,
    window: Duration,
    last_event: Option<Instant>,
}

impl ChangeBatch {
    pub fn new(window_ms: u64) -> Self {
        Self {
            paths: HashSet::new(),
            window: Duration::from_millis(window_ms),
            last_event: None,
        }
    }

    /// Add a path to the current batch.
    pub fn add(&mut self, path: PathBuf) {
        self.paths.insert(path);
        self.last_event = Some(Instant::now());
    }

    /// Check if the batch window has closed (enough time since last event).
    pub fn is_ready(&self) -> bool {
        match self.last_event {
            Some(t) => t.elapsed() >= self.window,
            None => false,
        }
    }

    /// Drain the batch and return accumulated paths.
    pub fn drain(&mut self) -> Vec<PathBuf> {
        self.last_event = None;
        self.paths.drain().collect()
    }

    /// Merge previously-drained paths back into the pending batch (e.g. a
    /// flush attempt lost a lock race) and re-arm the flush window so the
    /// next attempt waits a full window before retrying, rather than
    /// spinning on an immediate retry.
    pub fn readd(&mut self, paths: Vec<PathBuf>) {
        self.paths.extend(paths);
        self.last_event = Some(Instant::now());
    }

    /// Whether the batch has any pending paths.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readd_merges_paths_back_into_pending_batch() {
        let mut batch = ChangeBatch::new(10_000);
        assert!(batch.is_empty());

        let drained = vec![PathBuf::from("a.py"), PathBuf::from("b.py")];
        batch.readd(drained);

        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn readd_merges_with_paths_added_after_drain() {
        let mut batch = ChangeBatch::new(10_000);
        batch.add(PathBuf::from("c.py"));

        // Simulate: a flush drained the batch, lost the lock race, and
        // meanwhile a new event arrived for a different file before readd.
        let drained = vec![PathBuf::from("a.py")];
        batch.readd(drained);
        batch.add(PathBuf::from("c.py")); // duplicate of an already-pending path

        assert_eq!(batch.len(), 2, "a.py + c.py, deduped by HashSet");
    }

    #[test]
    fn readd_rearms_the_flush_window() {
        let mut batch = ChangeBatch::new(50);
        batch.readd(vec![PathBuf::from("a.py")]);

        // Window was just re-armed, so it shouldn't be ready immediately.
        assert!(
            !batch.is_ready(),
            "readd must re-arm the window rather than leaving the batch immediately ready"
        );

        std::thread::sleep(Duration::from_millis(80));
        assert!(
            batch.is_ready(),
            "batch should become ready once the re-armed window elapses"
        );
    }

    #[test]
    fn readd_on_empty_batch_makes_it_non_empty_and_pending() {
        let mut batch = ChangeBatch::new(10_000);
        assert!(!batch.is_ready(), "no events yet, nothing to flush");

        batch.readd(vec![PathBuf::from("retry.py")]);

        assert!(!batch.is_empty());
        assert!(
            !batch.is_ready(),
            "freshly re-armed window should not be ready yet"
        );
    }
}
