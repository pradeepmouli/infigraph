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

    /// Whether the batch has any pending paths.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }
}
