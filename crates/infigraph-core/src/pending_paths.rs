//! The set of paths an indexer has been asked to look at and has not yet,
//! with the rules for merging new asks into it. One type for every queue that
//! coalesces path work: the code graph's [`IndexWorkQueue`](crate::daemon::queue::IndexWorkQueue),
//! the document store's deferred writes, and a sink's debounce window.
//!
//! The rules are what a queue must not get wrong:
//! - a removal wins over a pending upsert of the same path;
//! - an upsert after a removal cancels the removal;
//! - a removal that may name a directory (`remove_tree`: the watcher cannot
//!   tell, the path is already gone) also drops the pending upserts below it
//!   -- below `dir/`, never a sibling that merely shares the prefix (`dir2`);
//! - a whole-project request absorbs the rest and survives every later merge.
//!
//! `T` is what the queue keeps per upserted path: `()` for a path to read from
//! disk, the code queue's `PendingIndexItem` for a pre-parsed extraction.

use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct PendingPaths<T = ()> {
    upserts: HashMap<String, T>,
    removals: HashSet<String>,
    /// Removals that may name a directory: each also removes everything below
    /// it. A subset of `removals`.
    removal_prefixes: HashSet<String>,
    whole: bool,
}

impl<T> Default for PendingPaths<T> {
    fn default() -> Self {
        Self {
            upserts: HashMap::new(),
            removals: HashSet::new(),
            removal_prefixes: HashSet::new(),
            whole: false,
        }
    }
}

/// What [`PendingPaths::take`] hands over.
#[derive(Debug)]
pub struct Taken<T = ()> {
    pub upserts: HashMap<String, T>,
    pub removals: HashSet<String>,
    pub removal_prefixes: HashSet<String>,
    pub whole: bool,
}

impl<T> PendingPaths<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks for `rel` to be indexed with `item`, replacing any earlier ask for
    /// it and cancelling a pending removal of the path itself.
    pub fn upsert(&mut self, rel: String, item: T) {
        self.removals.remove(&rel);
        self.upserts.insert(rel, item);
    }

    /// Cancels a pending removal of exactly `rel`: the path apparently exists
    /// again. For a queue that decides what to keep per path itself.
    pub fn cancel_removal(&mut self, rel: &str) {
        self.removals.remove(rel);
    }

    /// Asks for `rel`'s rows to go. Wins over a pending upsert of `rel`.
    pub fn remove(&mut self, rel: String) {
        self.upserts.remove(&rel);
        self.removals.insert(rel);
    }

    /// [`remove`](Self::remove) for a path that may be a directory: also
    /// removes everything below `rel/`, and drops the pending upserts there.
    pub fn remove_tree(&mut self, rel: String) {
        let below = format!("{rel}/");
        self.upserts.retain(|path, _| !path.starts_with(&below));
        self.removal_prefixes.insert(rel.clone());
        self.remove(rel);
    }

    /// Asks for a whole-project pass.
    pub fn mark_whole(&mut self) {
        self.whole = true;
    }

    pub fn wants_whole(&self) -> bool {
        self.whole
    }

    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty()
            && self.removals.is_empty()
            && self.removal_prefixes.is_empty()
            && !self.whole
    }

    pub fn upserts(&self) -> &HashMap<String, T> {
        &self.upserts
    }

    pub fn upserts_mut(&mut self) -> &mut HashMap<String, T> {
        &mut self.upserts
    }

    /// Folds `newer` into this set: its asks come after ours, so they win.
    pub fn merge(&mut self, newer: PendingPaths<T>) {
        let Taken {
            upserts,
            removals,
            removal_prefixes,
            whole,
        } = newer.take();
        for rel in removals {
            if removal_prefixes.contains(&rel) {
                self.remove_tree(rel);
            } else {
                self.remove(rel);
            }
        }
        for (rel, item) in upserts {
            self.upsert(rel, item);
        }
        self.whole |= whole;
    }

    /// Returns and clears everything.
    pub fn take(self) -> Taken<T> {
        Taken {
            upserts: self.upserts,
            removals: self.removals,
            removal_prefixes: self.removal_prefixes,
            whole: self.whole,
        }
    }

    /// [`take`](Self::take) in place.
    pub fn take_in_place(&mut self) -> Taken<T> {
        std::mem::take(self).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(set: &PendingPaths) -> Vec<String> {
        let mut keys: Vec<String> = set.upserts().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn a_removal_wins_over_a_pending_upsert() {
        let mut set = PendingPaths::new();
        set.upsert("a.md".into(), ());
        set.remove("a.md".into());
        assert!(paths(&set).is_empty());
        assert!(set.removals.contains("a.md"));
    }

    #[test]
    fn an_upsert_after_a_removal_cancels_it() {
        let mut set = PendingPaths::new();
        set.remove("a.md".into());
        set.upsert("a.md".into(), ());
        assert!(set.removals.is_empty());
        assert_eq!(paths(&set), ["a.md"]);
    }

    #[test]
    fn removing_a_tree_drops_the_upserts_below_it_and_not_a_sibling_sharing_the_prefix() {
        let mut set = PendingPaths::new();
        for p in [
            "dir/a.md",
            "dir/deep/b.md",
            "dir2/c.md",
            "dir.md",
            "other/d.md",
        ] {
            set.upsert(p.into(), ());
        }
        set.remove_tree("dir".into());
        assert_eq!(paths(&set), ["dir.md", "dir2/c.md", "other/d.md"]);
        assert!(set.removals.contains("dir"));
        assert!(set.removal_prefixes.contains("dir"));
    }

    #[test]
    fn a_whole_project_request_survives_every_later_merge() {
        let mut set = PendingPaths::new();
        set.mark_whole();
        let mut newer = PendingPaths::new();
        newer.upsert("a.md".into(), ());
        set.merge(newer);
        set.merge(PendingPaths::new());
        assert!(set.wants_whole());
        assert!(!set.is_empty());
        let taken = set.take_in_place();
        assert!(taken.whole);
        assert!(set.is_empty(), "taking clears the flag too");
    }

    #[test]
    fn a_newer_set_wins_a_merge() {
        let mut set = PendingPaths::new();
        set.upsert("a.md".into(), ());
        set.remove("b.md".into());
        let mut newer = PendingPaths::new();
        newer.remove_tree("a.md".into());
        newer.upsert("b.md".into(), ());
        set.merge(newer);
        assert_eq!(paths(&set), ["b.md"]);
        assert!(set.removals.contains("a.md"));
        assert!(!set.removals.contains("b.md"));
    }
}
