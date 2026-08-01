use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

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

/// Writes `contents` to `path` atomically: a temp file in the same
/// directory, then `rename(2)` over the target. A reader must never
/// observe a partially-written request or result file. Same pattern as
/// R3.3.1's sidecar-atomicity convention (`DESIGN-hardening.md`).
pub fn write_atomic(path: &Path, contents: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent directory: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp_path = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("path has no file name: {}", path.display()))?
            .to_string_lossy(),
        std::process::id()
    ));
    let mut file = std::fs::File::create(&tmp_path)?;
    if let Err(e) = file.write_all(contents.as_bytes()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    if let Err(e) = file.sync_all() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    drop(file);
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod atomic_write_tests {
    use super::write_atomic;

    #[test]
    fn write_atomic_creates_file_with_exact_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, r#"{"hello":"world"}"#).unwrap();
        let read_back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_back, r#"{"hello":"world"}"#);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, "content").unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one file, got {entries:?}"
        );
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        write_atomic(&path, "first").unwrap();
        write_atomic(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    #[cfg(unix)]
    fn write_atomic_cleans_up_temp_file_on_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("sub");
        std::fs::create_dir(&subdir).unwrap();

        let path = subdir.join("test.json");

        // Create a file first (this will succeed)
        write_atomic(&path, "initial").unwrap();

        // Make the directory read-only to cause subsequent write_atomic to fail
        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Try to write again (this will fail)
        let result = write_atomic(&path, "should fail");

        // Restore permissions so we can check directory contents
        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "write should have failed");

        // Verify only the original file exists; no temp files should be orphaned
        let entries: Vec<_> = std::fs::read_dir(&subdir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();

        assert_eq!(
            entries.len(),
            1,
            "should only have the original file, no temp files. Found: {entries:?}"
        );
        assert_eq!(entries[0], "test.json");
    }
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
