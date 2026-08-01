use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request for the daemon to perform a write. Carries references (paths),
/// never pre-computed data -- the daemon does its own parsing/extraction
/// using its own local filesystem access. See
/// docs/superpowers/specs/2026-07-31-graph-lock-write-coordination-design.md.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteRequest {
    /// Index specific files. `None` means a full project reindex.
    Index { paths: Option<Vec<PathBuf>> },
    /// Import a SCIP index file at the given path.
    ScipImport { scip_path: PathBuf },
}

/// Small summary of what happened -- never the full `IndexResult` (which
/// carries every file's `FileExtraction`, already written to the graph by
/// the daemon and not needed again by the caller).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WriteResult {
    Ok {
        total_files: usize,
        indexed_files: usize,
    },
    Err {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_request_index_round_trips_through_json() {
        let req = WriteRequest::Index {
            paths: Some(vec![PathBuf::from("src/main.rs")]),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: WriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn write_request_full_reindex_round_trips_through_json() {
        let req = WriteRequest::Index { paths: None };
        let json = serde_json::to_string(&req).unwrap();
        let back: WriteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }

    #[test]
    fn write_result_ok_round_trips_through_json() {
        let res = WriteResult::Ok {
            total_files: 10,
            indexed_files: 8,
        };
        let json = serde_json::to_string(&res).unwrap();
        let back: WriteResult = serde_json::from_str(&json).unwrap();
        assert_eq!(res, back);
    }
}
