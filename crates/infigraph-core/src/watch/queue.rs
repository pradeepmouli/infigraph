use crate::model::FileExtraction;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// A single file's pending index-shaped work, before it's known whether the
/// file needs a fresh disk read (`Raw`) or already carries pre-parsed
/// content from a client that did its own local parsing (`Structured`,
/// `ResolveOnly`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PendingIndexItem {
    /// Needs fresh extraction from disk at drain time.
    Raw,
    /// Pre-parsed by a client (`UpsertFilesBulk`); needs both upsert and resolve.
    Structured(FileExtraction),
    /// Pre-parsed by a client (`ResolveCalls`) whose content was already
    /// upserted by an earlier, separate drain -- needs resolve only, must
    /// not trigger a redundant re-upsert.
    ResolveOnly(FileExtraction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum WaiterKind {
    Index,
    UpsertFilesBulk,
    RemoveFiles,
    ResolveCalls,
}

/// An ad-hoc daemon-protocol caller blocked on a reply for the drain this
/// waiter was folded into.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct Waiter {
    pub kind: WaiterKind,
    /// `ResolveCalls` waiters only -- ignored for other kinds.
    pub use_learned: bool,
    pub reply_path: PathBuf,
    /// The specific relative paths this waiter's own request named, so its
    /// reply can report a count scoped to what IT asked for rather than the
    /// whole merged drain (a targeted `Index`/`UpsertFilesBulk`/`RemoveFiles`
    /// request should not be told about unrelated concurrent work that rode
    /// along in the same drain). `None` means the waiter is inherently
    /// whole-project scoped (`Index { paths: None }`) or the count is not
    /// path-attributable (`ResolveCalls`), for which the batch-wide count
    /// is the correct, intended answer.
    pub paths: Option<Vec<String>>,
}

/// The full state popped off an `IndexWorkQueue` by `drain()`, ready for
/// the unified drain execution (Task 2) to run against.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub(crate) struct DrainedQueue {
    pub items: HashMap<String, PendingIndexItem>,
    pub removals: HashSet<String>,
    /// Paths removed via a real filesystem removal event, where the watcher
    /// can no longer tell (the path is already gone from disk) whether it
    /// was a file or a directory -- each needs a directory-prefix scan/removal
    /// in addition to the exact-path removal already covered by `removals`.
    /// A subset of `removals`, not a separate universe of paths.
    pub removal_prefixes: HashSet<String>,
    pub whole_project: bool,
    pub waiters: Vec<Waiter>,
}

/// Shared accumulator for index-shaped work across the daemon watch loop's
/// producers (periodic reindex, watch-triggered batch/removal, and four
/// ad-hoc `WriteRequest` variants). Has no timer of its own -- producers own
/// their own timing (see the design spec's "Debounce ownership" section);
/// this type just merges whatever's been contributed since the last drain.
#[derive(Debug, Default)]
pub(crate) struct IndexWorkQueue {
    items: HashMap<String, PendingIndexItem>,
    removals: HashSet<String>,
    removal_prefixes: HashSet<String>,
    whole_project: bool,
    waiters: Vec<Waiter>,
}

impl IndexWorkQueue {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Marks `rel_path` as needing fresh extraction from disk. Cancels any
    /// pending removal for the same path (the file apparently exists again)
    /// and supersedes any `Structured`/`ResolveOnly` entry -- freshness
    /// always wins over a possibly-stale pre-parsed extraction, matching
    /// the "reopen fresh rather than trust a cached view" precedent from
    /// `DaemonKuzuBackend`'s read-staleness fix.
    #[allow(dead_code)]
    pub(crate) fn add_raw(&mut self, rel_path: String) {
        self.removals.remove(&rel_path);
        self.items.insert(rel_path, PendingIndexItem::Raw);
    }

    /// Adds a pre-parsed extraction that needs both upsert and resolve
    /// (`UpsertFilesBulk`). A no-op if a `Raw` entry already exists for this
    /// path (that entry will already trigger a fresh extraction, which
    /// supersedes this possibly-stale one). Overwrites any `ResolveOnly`
    /// entry, since `Structured` is the stronger requirement.
    #[allow(dead_code)]
    pub(crate) fn add_structured(&mut self, extraction: FileExtraction) {
        let rel_path = extraction.file.clone();
        self.removals.remove(&rel_path);
        if matches!(self.items.get(&rel_path), Some(PendingIndexItem::Raw)) {
            return;
        }
        self.items
            .insert(rel_path, PendingIndexItem::Structured(extraction));
    }

    /// Adds a pre-parsed extraction that only needs resolution
    /// (`ResolveCalls`, whose content was already upserted by an earlier,
    /// separate request/drain). A no-op if *any* entry already exists for
    /// this path -- `Raw`/`Structured` will already resolve it as part of
    /// their own upsert; adding `ResolveOnly` on top would be redundant,
    /// never wrong-but-cheaper.
    #[allow(dead_code)]
    pub(crate) fn add_resolve_only(&mut self, extraction: FileExtraction) {
        let rel_path = extraction.file.clone();
        self.removals.remove(&rel_path);
        self.items
            .entry(rel_path)
            .or_insert(PendingIndexItem::ResolveOnly(extraction));
    }

    /// Marks `rel_path` for removal. Always wins over any pending index
    /// intent for the same path -- the file is gone, indexing it makes no
    /// sense regardless of what was queued moments before.
    #[allow(dead_code)]
    pub(crate) fn add_removal(&mut self, rel_path: String) {
        self.items.remove(&rel_path);
        self.removals.insert(rel_path);
    }

    /// Marks `rel_path` for removal exactly like `add_removal`, and
    /// additionally records it as needing a directory-prefix scan/removal.
    /// For a real filesystem removal event, `rel_path` is already gone from
    /// disk by the time this fires, so there is no way to tell whether it
    /// named a file or a directory -- mirrors the pre-`IndexWorkQueue` inline
    /// watch-loop removal handling, which unconditionally ran both
    /// `remove_file` and `remove_files_by_prefix` for every such event.
    /// Not used for protocol-level `RemoveFiles` requests, whose caller
    /// names specific files it already knows are files.
    #[allow(dead_code)]
    pub(crate) fn add_watch_removal(&mut self, rel_path: String) {
        self.add_removal(rel_path.clone());
        self.removal_prefixes.insert(rel_path);
    }

    /// The drain step will additionally compute the full changed-file set
    /// (a whole-project scan + hash-diff), same as `Infigraph::index()`
    /// does today, in addition to whatever's explicitly queued.
    #[allow(dead_code)]
    pub(crate) fn mark_whole_project(&mut self) {
        self.whole_project = true;
    }

    /// Registers a reply target for the next drain this queue produces.
    #[allow(dead_code)]
    pub(crate) fn add_waiter(&mut self, waiter: Waiter) {
        self.waiters.push(waiter);
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
            && self.removals.is_empty()
            && self.removal_prefixes.is_empty()
            && !self.whole_project
            && self.waiters.is_empty()
    }

    /// Returns and clears the full accumulated state in one shot.
    #[allow(dead_code)]
    pub(crate) fn drain(&mut self) -> DrainedQueue {
        DrainedQueue {
            items: std::mem::take(&mut self.items),
            removals: std::mem::take(&mut self.removals),
            removal_prefixes: std::mem::take(&mut self.removal_prefixes),
            whole_project: std::mem::replace(&mut self.whole_project, false),
            waiters: std::mem::take(&mut self.waiters),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extraction(file: &str) -> FileExtraction {
        FileExtraction {
            file: file.to_string(),
            language: "python".to_string(),
            content_hash: "deadbeef".to_string(),
            symbols: Vec::new(),
            relations: Vec::new(),
            statements: Vec::new(),
        }
    }

    #[test]
    fn add_raw_evicts_an_existing_structured_entry_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_structured(extraction("a.py"));
        q.add_raw("a.py".to_string());
        let drained = q.drain();
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn add_structured_after_add_raw_is_a_no_op() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_structured(extraction("a.py"));
        let drained = q.drain();
        assert_eq!(
            drained.items.get("a.py"),
            Some(&PendingIndexItem::Raw),
            "Raw must survive a later add_structured for the same path"
        );
    }

    #[test]
    fn add_structured_overwrites_an_existing_resolve_only_entry() {
        let mut q = IndexWorkQueue::new();
        q.add_resolve_only(extraction("a.py"));
        q.add_structured(extraction("a.py"));
        let drained = q.drain();
        assert!(matches!(
            drained.items.get("a.py"),
            Some(PendingIndexItem::Structured(_))
        ));
    }

    #[test]
    fn add_resolve_only_is_a_no_op_when_a_raw_entry_already_exists() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_resolve_only(extraction("a.py"));
        let drained = q.drain();
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn add_resolve_only_is_a_no_op_when_a_structured_entry_already_exists() {
        let mut q = IndexWorkQueue::new();
        q.add_structured(extraction("a.py"));
        q.add_resolve_only(extraction("a.py"));
        let drained = q.drain();
        assert!(matches!(
            drained.items.get("a.py"),
            Some(PendingIndexItem::Structured(_))
        ));
    }

    #[test]
    fn add_removal_clears_any_pending_index_entry_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_raw("a.py".to_string());
        q.add_removal("a.py".to_string());
        let drained = q.drain();
        assert!(!drained.items.contains_key("a.py"));
        assert!(drained.removals.contains("a.py"));
    }

    #[test]
    fn add_raw_cancels_a_pending_removal_for_the_same_path() {
        let mut q = IndexWorkQueue::new();
        q.add_removal("a.py".to_string());
        q.add_raw("a.py".to_string());
        let drained = q.drain();
        assert!(!drained.removals.contains("a.py"));
        assert_eq!(drained.items.get("a.py"), Some(&PendingIndexItem::Raw));
    }

    #[test]
    fn is_empty_and_drain_round_trip() {
        let mut q = IndexWorkQueue::new();
        assert!(q.is_empty());

        q.add_raw("a.py".to_string());
        assert!(!q.is_empty());

        let drained = q.drain();
        assert_eq!(drained.items.len(), 1);
        assert!(q.is_empty(), "drain must clear all accumulated state");
    }

    #[test]
    fn mark_whole_project_is_reflected_in_the_drained_snapshot_and_reset_after() {
        let mut q = IndexWorkQueue::new();
        q.mark_whole_project();
        assert!(!q.is_empty());
        let drained = q.drain();
        assert!(drained.whole_project);
        assert!(q.is_empty(), "whole_project flag must reset after drain");
    }

    #[test]
    fn waiters_accumulate_across_multiple_add_waiter_calls_before_one_drain() {
        let mut q = IndexWorkQueue::new();
        q.add_waiter(Waiter {
            kind: WaiterKind::Index,
            use_learned: false,
            reply_path: PathBuf::from("/tmp/a.result"),
            paths: None,
        });
        q.add_waiter(Waiter {
            kind: WaiterKind::ResolveCalls,
            use_learned: true,
            reply_path: PathBuf::from("/tmp/b.result"),
            paths: None,
        });
        let drained = q.drain();
        assert_eq!(drained.waiters.len(), 2);
    }

    #[test]
    fn add_watch_removal_marks_both_the_exact_path_and_its_directory_prefix() {
        let mut q = IndexWorkQueue::new();
        q.add_watch_removal("sub".to_string());
        let drained = q.drain();
        assert!(
            drained.removals.contains("sub"),
            "a watch removal must still remove the exact path, same as add_removal"
        );
        assert!(
            drained.removal_prefixes.contains("sub"),
            "a watch removal must additionally be scanned as a possible directory prefix"
        );
    }
}
