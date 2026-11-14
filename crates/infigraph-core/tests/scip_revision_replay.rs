//! The SCIP import ledger's revision rule, through the public API that the
//! daemon's replay and doctor both call. Languages are judged, not labels: a
//! language is behind when an entry serves it and none serving it records the
//! current `SCIP_IMPORT_REVISION`.

use std::collections::HashSet;

use infigraph_core::scip::{self, LedgerEntry, RETIRED_LABELS, SCIP_IMPORT_REVISION};
use infigraph_core::scip_switch::{languages_behind, replay_languages};

/// A copy of the shape of a live ledger from before revisions: four entries,
/// none with `importer_revision`. A fixture, not the live file.
const LIVE_SHAPE: &str = r#"{
  "rust-analyzer": {"hash": 5536960326523201910, "scip_generation": 2},
  "scip-go": {"hash": 181646230573971328, "scip_generation": 1333},
  "scip-python": {"hash": 16813754624391336735, "scip_generation": 1283},
  "scip-typescript": {"hash": 4920164732017185868, "scip_generation": 2}
}"#;

fn ledger_dir(text: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("scip-imports.json"), text).unwrap();
    dir
}

fn entry(label: &str, revision: Option<u64>) -> LedgerEntry {
    LedgerEntry {
        label: label.to_string(),
        revision,
    }
}

fn detected(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn the_live_ledger_shape_parses_to_entries_with_no_revision() {
    let dir = ledger_dir(LIVE_SHAPE);
    let mut entries = scip::ledger_entries(dir.path()).expect("the ledger parses");
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(
        labels,
        ["rust-analyzer", "scip-go", "scip-python", "scip-typescript"]
    );
    assert!(entries.iter().all(|e| e.revision.is_none()));
}

#[test]
fn the_live_shape_leaves_its_languages_behind() {
    let dir = ledger_dir(LIVE_SHAPE);
    let entries = scip::ledger_entries(dir.path()).unwrap();
    // The four scip-tsrs languages come from the retired scip-typescript label.
    assert_eq!(
        languages_behind(&entries),
        [
            "go",
            "javascript",
            "jsx",
            "python",
            "rust",
            "tsx",
            "typescript"
        ]
    );
}

#[test]
fn behind_languages_are_filtered_by_what_the_project_has() {
    let dir = ledger_dir(LIVE_SHAPE);
    let entries = scip::ledger_entries(dir.path()).unwrap();
    let project = tempfile::tempdir().unwrap();
    let has = detected(&["rust", "typescript", "cobol"]);
    assert_eq!(
        replay_languages(project.path(), 5, &entries, &has),
        ["rust", "typescript"]
    );
}

#[test]
fn a_graph_never_enriched_replays_nothing() {
    let dir = ledger_dir(LIVE_SHAPE);
    let entries = scip::ledger_entries(dir.path()).unwrap();
    let project = tempfile::tempdir().unwrap();
    assert!(replay_languages(project.path(), 0, &entries, &detected(&["rust"])).is_empty());
}

#[test]
fn scip_typescript_at_no_revision_is_behind_with_scip_tsrs_at_the_current_one() {
    // scip-tsrs at the current revision serves typescript, so it is not behind,
    // whatever the superseded scip-typescript entry still says.
    let entries = vec![
        entry("scip-typescript", None),
        entry("scip-tsrs", Some(SCIP_IMPORT_REVISION)),
    ];
    assert!(languages_behind(&entries).is_empty());
}

#[test]
fn scip_typescript_alone_is_behind_and_replays_its_languages() {
    let entries = vec![entry("scip-typescript", None)];
    assert_eq!(
        languages_behind(&entries),
        ["javascript", "jsx", "tsx", "typescript"]
    );
    let project = tempfile::tempdir().unwrap();
    assert_eq!(
        replay_languages(
            project.path(),
            5,
            &entries,
            &detected(&["typescript", "python"])
        ),
        ["typescript"]
    );
}

#[test]
fn an_unknown_label_is_ignored() {
    assert!(languages_behind(&[entry("scip-bogus", None)]).is_empty());
}

#[test]
fn a_language_no_entry_serves_is_not_behind() {
    // Only go has an entry, and it is current: nothing else is behind.
    let entries = vec![entry("scip-go", Some(SCIP_IMPORT_REVISION))];
    assert!(languages_behind(&entries).is_empty());
}

#[test]
fn every_retired_label_names_a_successor_the_table_lists() {
    for (retired, successor) in RETIRED_LABELS {
        assert!(
            !scip::indexer_languages(successor).is_empty(),
            "{retired} names {successor}, which INDEXER_LANGUAGES does not list"
        );
        assert!(
            scip::indexer_languages(retired).is_empty(),
            "{retired} is retired and must not also be listed"
        );
    }
}

#[test]
fn a_retired_label_resolves_to_its_successors_languages() {
    assert_eq!(
        scip::ledger_label_languages("scip-typescript"),
        scip::indexer_languages("scip-tsrs")
    );
    assert!(scip::ledger_label_languages("scip-bogus").is_empty());
}

#[test]
fn an_unreadable_ledger_is_none_so_nothing_replays() {
    let missing = tempfile::tempdir().unwrap();
    assert!(scip::ledger_entries(missing.path()).is_none());
    assert!(scip::ledger_entries(ledger_dir("not json").path()).is_none());
    assert!(scip::ledger_entries(ledger_dir("[]").path()).is_none());
}

#[test]
fn a_replayed_ledger_is_not_behind_and_keeps_its_superseded_entries() {
    let dir = ledger_dir(LIVE_SHAPE);
    for label in ["rust-analyzer", "scip-go", "scip-python", "scip-tsrs"] {
        scip::record_scip_import(dir.path(), label, 42, 7);
    }
    let entries = scip::ledger_entries(dir.path()).unwrap();
    assert!(languages_behind(&entries).is_empty());
    let text = std::fs::read_to_string(dir.path().join("scip-imports.json")).unwrap();
    assert!(
        text.contains("scip-typescript"),
        "the superseded entry is kept, not pruned"
    );
}
