use infigraph_core::learned::LearnedStore;
use infigraph_core::model::{BridgeKind, RelationKind, StatementKind, SymbolKind};
use infigraph_core::watch::batch::ChangeBatch;
use std::collections::HashSet;
use std::path::PathBuf;

// ==================== SymbolKind::as_str ====================

#[test]
fn test_symbol_kind_as_str_all_variants() {
    assert_eq!(SymbolKind::Function.as_str(), "Function");
    assert_eq!(SymbolKind::Method.as_str(), "Method");
    assert_eq!(SymbolKind::Class.as_str(), "Class");
    assert_eq!(SymbolKind::Struct.as_str(), "Struct");
    assert_eq!(SymbolKind::Interface.as_str(), "Interface");
    assert_eq!(SymbolKind::Trait.as_str(), "Trait");
    assert_eq!(SymbolKind::Enum.as_str(), "Enum");
    assert_eq!(SymbolKind::Module.as_str(), "Module");
    assert_eq!(SymbolKind::Variable.as_str(), "Variable");
    assert_eq!(SymbolKind::Constant.as_str(), "Constant");
    assert_eq!(SymbolKind::Test.as_str(), "Test");
    assert_eq!(SymbolKind::Section.as_str(), "Section");
    assert_eq!(SymbolKind::Route.as_str(), "Route");
    assert_eq!(SymbolKind::Field.as_str(), "Field");
}

// ==================== RelationKind::as_str ====================

#[test]
fn test_relation_kind_as_str_all_variants() {
    assert_eq!(RelationKind::Calls.as_str(), "CALLS");
    assert_eq!(RelationKind::CalledBy.as_str(), "CALLED_BY");
    assert_eq!(RelationKind::Imports.as_str(), "IMPORTS");
    assert_eq!(RelationKind::ImportedBy.as_str(), "IMPORTED_BY");
    assert_eq!(RelationKind::Contains.as_str(), "CONTAINS");
    assert_eq!(RelationKind::ContainedBy.as_str(), "CONTAINED_BY");
    assert_eq!(RelationKind::Inherits.as_str(), "INHERITS");
    assert_eq!(RelationKind::InheritedBy.as_str(), "INHERITED_BY");
    assert_eq!(RelationKind::Implements.as_str(), "IMPLEMENTS");
    assert_eq!(RelationKind::ImplementedBy.as_str(), "IMPLEMENTED_BY");
    assert_eq!(RelationKind::Reads.as_str(), "READS");
    assert_eq!(RelationKind::Writes.as_str(), "WRITES");
    assert_eq!(RelationKind::TestedBy.as_str(), "TESTED_BY");
    assert_eq!(RelationKind::Tests.as_str(), "TESTS");
}

#[test]
fn test_relation_kind_custom_as_str() {
    let custom = RelationKind::Custom("DECORATED_BY".to_string());
    assert_eq!(custom.as_str(), "DECORATED_BY");
}

// ==================== StatementKind::as_str ====================

#[test]
fn test_statement_kind_as_str_all_variants() {
    assert_eq!(StatementKind::If.as_str(), "If");
    assert_eq!(StatementKind::ElseIf.as_str(), "ElseIf");
    assert_eq!(StatementKind::Else.as_str(), "Else");
    assert_eq!(StatementKind::For.as_str(), "For");
    assert_eq!(StatementKind::While.as_str(), "While");
    assert_eq!(StatementKind::DoWhile.as_str(), "DoWhile");
    assert_eq!(StatementKind::Loop.as_str(), "Loop");
    assert_eq!(StatementKind::Match.as_str(), "Match");
    assert_eq!(StatementKind::Case.as_str(), "Case");
    assert_eq!(StatementKind::Try.as_str(), "Try");
    assert_eq!(StatementKind::Catch.as_str(), "Catch");
    assert_eq!(StatementKind::Ternary.as_str(), "Ternary");
    assert_eq!(StatementKind::Guard.as_str(), "Guard");
}

// ==================== BridgeKind::as_str ====================

#[test]
fn test_bridge_kind_as_str_all_variants() {
    assert_eq!(BridgeKind::Ffi.as_str(), "FFI");
    assert_eq!(BridgeKind::Jni.as_str(), "JNI");
    assert_eq!(BridgeKind::Cgo.as_str(), "CGO");
    assert_eq!(BridgeKind::Grpc.as_str(), "GRPC");
    assert_eq!(BridgeKind::PInvoke.as_str(), "P_INVOKE");
    assert_eq!(BridgeKind::Ctypes.as_str(), "CTYPES");
    assert_eq!(BridgeKind::Wasm.as_str(), "WASM");
    assert_eq!(BridgeKind::Com.as_str(), "COM");
}

#[test]
fn test_bridge_kind_other_as_str() {
    let other = BridgeKind::Other("CUSTOM_BRIDGE".to_string());
    assert_eq!(other.as_str(), "CUSTOM_BRIDGE");
}

// ==================== LearnedStore ====================

#[test]
fn test_learned_store_is_empty_initially() {
    let store = LearnedStore::default();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
}

#[test]
fn test_learned_store_len_after_record() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    assert_eq!(store.len(), 1);
    assert!(!store.is_empty());

    store.record_correction("c.py", "bar", "d.py", "d.py::bar");
    assert_eq!(store.len(), 2);
}

#[test]
fn test_learned_store_clear() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    store.record_correction("c.py", "bar", "d.py", "d.py::bar");
    assert_eq!(store.len(), 2);

    store.clear();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
}

#[test]
fn test_learned_store_prune_stale_removes_missing_files() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    store.record_correction("c.py", "bar", "d.py", "d.py::bar");
    store.record_correction("e.py", "baz", "f.py", "f.py::baz");

    let mut existing = HashSet::new();
    existing.insert("b.py".to_string());
    // d.py and f.py not in existing — those patterns should be pruned

    store.prune_stale(&existing);
    assert_eq!(store.len(), 1);
    assert!(store.lookup("a.py", "foo").is_some());
    assert!(store.lookup("c.py", "bar").is_none());
    assert!(store.lookup("e.py", "baz").is_none());
}

#[test]
fn test_learned_store_prune_stale_keeps_all_when_files_exist() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    store.record_correction("c.py", "bar", "d.py", "d.py::bar");

    let mut existing = HashSet::new();
    existing.insert("b.py".to_string());
    existing.insert("d.py".to_string());

    store.prune_stale(&existing);
    assert_eq!(store.len(), 2);
}

#[test]
fn test_learned_store_prune_stale_empty_set_clears_all() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");

    let existing = HashSet::new();
    store.prune_stale(&existing);
    assert!(store.is_empty());
}

#[test]
fn test_learned_store_load_nonexistent_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = LearnedStore::load(tmp.path());
    assert!(store.is_empty());
}

#[test]
fn test_learned_store_save_and_load_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    store.save(tmp.path()).unwrap();

    let loaded = LearnedStore::load(tmp.path());
    assert_eq!(loaded.len(), 1);
    let p = loaded.lookup("a.py", "foo").unwrap();
    assert_eq!(p.resolved_to_symbol, "b.py::foo");
}

#[test]
fn test_learned_store_record_correction_bumps_confidence() {
    let mut store = LearnedStore::default();
    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    let c1 = store.lookup("a.py", "foo").unwrap().confidence;
    assert!((c1 - 0.5).abs() < 0.01);

    store.record_correction("a.py", "foo", "b.py", "b.py::foo");
    let c2 = store.lookup("a.py", "foo").unwrap().confidence;
    assert!((c2 - 0.6).abs() < 0.01);
}

// ==================== ChangeBatch ====================

#[test]
fn test_change_batch_new_is_empty() {
    let batch = ChangeBatch::new(100);
    assert!(batch.is_empty());
    assert_eq!(batch.len(), 0);
}

#[test]
fn test_change_batch_add_and_len() {
    let mut batch = ChangeBatch::new(100);
    batch.add(PathBuf::from("a.rs"));
    assert_eq!(batch.len(), 1);
    assert!(!batch.is_empty());

    batch.add(PathBuf::from("b.rs"));
    assert_eq!(batch.len(), 2);
}

#[test]
fn test_change_batch_deduplicates() {
    let mut batch = ChangeBatch::new(100);
    batch.add(PathBuf::from("a.rs"));
    batch.add(PathBuf::from("a.rs"));
    batch.add(PathBuf::from("a.rs"));
    assert_eq!(batch.len(), 1);
}

#[test]
fn test_change_batch_drain_returns_all_and_empties() {
    let mut batch = ChangeBatch::new(0);
    batch.add(PathBuf::from("a.rs"));
    batch.add(PathBuf::from("b.rs"));
    batch.add(PathBuf::from("c.rs"));

    let drained = batch.drain();
    assert_eq!(drained.len(), 3);
    assert!(batch.is_empty());
    assert_eq!(batch.len(), 0);
}

#[test]
fn test_change_batch_is_ready_after_window() {
    let mut batch = ChangeBatch::new(0);
    batch.add(PathBuf::from("a.rs"));
    // window_ms = 0 means it should be ready immediately
    assert!(batch.is_ready());
}

#[test]
fn test_change_batch_not_ready_when_empty() {
    let batch = ChangeBatch::new(0);
    assert!(!batch.is_ready());
}

#[test]
fn test_change_batch_not_ready_within_window() {
    let mut batch = ChangeBatch::new(60_000);
    batch.add(PathBuf::from("a.rs"));
    // 60s window — should not be ready yet
    assert!(!batch.is_ready());
}
