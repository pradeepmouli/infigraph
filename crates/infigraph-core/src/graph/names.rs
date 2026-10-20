//! The names of the code graph's files inside `.infigraph/`, in one place.
//!
//! Every name is the store's stem plus a suffix, so changing where the code
//! graph lives is a change to [`stem!`] and nothing else. A name that is not
//! derived here is a name that will be missed the day the stem moves.

use std::path::{Path, PathBuf};

/// The stem as a literal, for `concat!` (a `const` cannot be an argument to
/// it). The one place the code graph's file name is spelled.
macro_rules! stem {
    () => {
        "code.kuzu"
    };
}

/// File name of the live code graph within `.infigraph/`.
pub const STEM: &str = stem!();
/// The graph a full reindex builds at the side, renamed over [`STEM`] when
/// done.
pub const REBUILDING: &str = concat!(stem!(), ".rebuilding");
/// What a rebuild sets the previous graph aside as, before the swap.
pub const ASIDE: &str = concat!(stem!(), ".aside");
/// Health stamp (`stamp_healthy_graph_size`) of the live graph.
pub const HEALTH: &str = concat!(stem!(), ".health.json");
/// Lock of the live graph (`store::db_lock_path` of [`STEM`]).
pub const LOCK: &str = concat!(stem!(), ".lock");
/// Prefix of the sentinel an opener leaves while it is inside the graph open
/// (`<stem>-opening.<pid>.<n>`).
pub const OPEN_SENTINEL_PREFIX: &str = concat!(stem!(), "-opening.");
/// Prefix of a quarantined graph (`<stem>.corrupt.<ts>`).
pub const CORRUPT_PREFIX: &str = concat!(stem!(), ".corrupt.");
/// Prefix of a graph retired by a restore (`<stem>.previous.<ts>`).
pub const PREVIOUS_PREFIX: &str = concat!(stem!(), ".previous.");
/// Staging stem of a restore in progress.
pub const RESTORING: &str = concat!(stem!(), ".restoring");
/// Snapshot note: the live WAL was left out.
pub const WAL_OMITTED_NOTE: &str = concat!(stem!(), ".wal.omitted.json");
/// Snapshot note: the live graph was left out.
pub const GRAPH_OMITTED_NOTE: &str = concat!(stem!(), ".omitted.json");

/// `<stem><suffix>`: a file derived from the live graph's name by a suffix
/// that has no constant of its own (`.wal`, `.wal.checkpoint`, `.compaction.json`, a
/// pool entry's `.corrupt.<ts>`).
pub fn sibling(suffix: &str) -> String {
    format!("{STEM}{suffix}")
}

/// The live code graph of the project whose `.infigraph/` is `infigraph_dir`.
pub fn live(infigraph_dir: &Path) -> PathBuf {
    infigraph_dir.join(STEM)
}

/// The live code graph of the project rooted at `root`.
pub fn live_of(root: &Path) -> PathBuf {
    live(&root.join(".infigraph"))
}

/// What the live graph was called before the stem moved, and the derived
/// names that went with it. The one place the old literals are spelled;
/// `graph::migrate` is the only code that acts on them, everything else asks
/// [`is_code_store_entry`] / [`store_present`].
pub mod legacy {
    pub const STEM: &str = "graph";
    /// The lock the previous build takes around a write. The migration holds
    /// it so a previous-build writer excludes it and is excluded.
    pub const LOCK: &str = "graph.lock";
    pub const CORRUPT_PREFIX: &str = "graph.corrupt.";
    pub const PREVIOUS_PREFIX: &str = "graph.previous.";
    pub const OPEN_SENTINEL_PREFIX: &str = "graph-opening.";
    /// Files that start with the old stem and are not the store: the
    /// visualisation output and the experimental Cozo export.
    const NOT_THE_STORE: [&str; 2] = ["graph.html", "graph.cozo"];

    /// Whether `name` is the old graph or one of its derived files.
    pub fn is_entry(name: &str) -> bool {
        if NOT_THE_STORE.iter().any(|n| name.starts_with(n)) {
            return false;
        }
        name == STEM || name.starts_with("graph.") || name.starts_with(OPEN_SENTINEL_PREFIX)
    }
}

/// Whether `name` is the live graph or one of its derived files, under the
/// current stem or the one before it (a store the migration has not reached).
pub fn is_code_store_entry(name: &str) -> bool {
    name == STEM
        || name
            .strip_prefix(STEM)
            .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("-opening."))
        || legacy::is_entry(name)
}

/// Whether `infigraph_dir` holds a code graph: the live one, or an older one
/// the migration has yet to rename. Existence checks ask this, never
/// `live(..).exists()`, so a store waiting for its migration is not mistaken
/// for no store (and then created afresh beside it).
pub fn store_present(infigraph_dir: &Path) -> bool {
    live(infigraph_dir).exists()
        || (legacy::STEM != STEM && infigraph_dir.join(legacy::STEM).exists())
        || super::migrate::journal_exists(infigraph_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_derived_name_starts_with_the_stem() {
        for name in [
            REBUILDING,
            ASIDE,
            HEALTH,
            LOCK,
            CORRUPT_PREFIX,
            PREVIOUS_PREFIX,
            RESTORING,
            WAL_OMITTED_NOTE,
            GRAPH_OMITTED_NOTE,
        ] {
            assert!(is_code_store_entry(name), "{name}");
        }
    }

    #[test]
    fn the_old_stem_is_still_recognised_as_the_store_it_was() {
        for name in [
            legacy::STEM,
            legacy::LOCK,
            legacy::CORRUPT_PREFIX,
            "graph.wal",
            "graph-opening.1.0",
        ] {
            assert!(is_code_store_entry(name), "{name}");
        }
        assert!(!is_code_store_entry("graph.html"));
        assert!(!is_code_store_entry("graph.cozo"));
    }

    #[test]
    fn a_sibling_that_merely_shares_letters_is_not_a_store_entry() {
        assert!(!is_code_store_entry(&format!("{STEM}x")));
        assert!(!is_code_store_entry("docs.kuzu"));
    }

    #[test]
    fn the_live_path_is_the_stem_below_infigraph() {
        assert_eq!(
            live_of(Path::new("/p")),
            Path::new("/p/.infigraph").join(STEM)
        );
    }
}
