use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

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
    /// Ingest structured data using a schema already discoverable by the
    /// daemon itself (via discover_schemas) -- looked up by schema_id, not
    /// serialized into the request.
    IngestStructured {
        schema_id: String,
        source: IngestSource,
    },
    /// Create a Repo node and link this project's files to it.
    UpsertRepo { namespace: String },
    /// Derive TESTED_BY edges. `files` scopes to changed files for
    /// incremental runs; `None` means a full derivation pass.
    DeriveTestedBy { files: Option<Vec<String>> },
}

/// Where IngestStructured's data comes from. `Inline` carries no data
/// itself -- the actual array lives in a sibling `.data.json` file next to
/// the request (see write_ingest_inline_sibling / read at
/// `handle_ingest_structured`), following the same reference-not-payload
/// convention paths already use.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IngestSource {
    File(PathBuf),
    Directory(PathBuf),
    Inline,
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
    /// Real SCIP import stats -- `Ok`'s two usize fields can't represent
    /// ImportStats's seven fields without losing data.
    ScipImportOk(crate::scip::ImportStats),
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

/// Process-local disambiguator: `SystemTime::now()`'s nanosecond field is
/// not guaranteed nanosecond-*resolution* on every platform, so two
/// threads in the same process racing this function could otherwise
/// collide on the same request name -- `write_atomic` overwrites
/// unconditionally (no existence check), so a collision would silently
/// drop one caller's request rather than erroring.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `request` as a `.request` file into `staging_dir` (unique name:
/// pid + nanosecond timestamp + a process-local counter -- no new
/// dependency, no UUID crate), then polls for the matching `.result` file
/// until `timeout` expires. Bounded-wait-with-backoff, same idiom as
/// `lockfile::acquire`.
pub fn submit_write_request(
    staging_dir: &Path,
    request: &WriteRequest,
    timeout: Duration,
) -> anyhow::Result<WriteResult> {
    std::fs::create_dir_all(staging_dir)?;
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        counter
    );
    let request_path = staging_dir.join(format!("{name}.request"));
    let result_path = staging_dir.join(format!("{name}.result"));

    write_atomic(&request_path, &serde_json::to_string(request)?)?;

    let start = Instant::now();
    let mut delay = Duration::from_millis(10);
    loop {
        if result_path.exists() {
            let contents = std::fs::read_to_string(&result_path)?;
            std::fs::remove_file(&result_path).ok();
            return Ok(serde_json::from_str(&contents)?);
        }
        if start.elapsed() >= timeout {
            std::fs::remove_file(&request_path).ok();
            anyhow::bail!(
                "no daemon responded to write request within {:?} ({})",
                timeout,
                request_path.display()
            );
        }
        std::thread::sleep(delay.min(timeout.saturating_sub(start.elapsed())));
        delay = (delay * 2).min(Duration::from_millis(200));
    }
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

#[cfg(test)]
mod submit_tests {
    use super::{submit_write_request, write_atomic, WriteRequest, WriteResult};
    use std::time::Duration;

    #[test]
    fn submit_write_request_writes_request_file_and_returns_matching_result() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let request = WriteRequest::Index {
            paths: Some(vec!["src/main.rs".into()]),
        };

        // Simulate the server: a background thread watches for the request
        // file to appear, then writes a matching result file.
        let staging_dir_clone = staging_dir.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            loop {
                let entries: Vec<_> = std::fs::read_dir(&staging_dir_clone)
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "request"))
                    .collect();
                if let Some(entry) = entries.first() {
                    let result_path = entry.path().with_extension("result");
                    let result = WriteResult::Ok {
                        total_files: 1,
                        indexed_files: 1,
                    };
                    write_atomic(&result_path, &serde_json::to_string(&result).unwrap()).unwrap();
                    std::fs::remove_file(entry.path()).unwrap();
                    return;
                }
                if start.elapsed() > Duration::from_secs(2) {
                    panic!("test server never saw a request file appear");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let result = submit_write_request(&staging_dir, &request, Duration::from_secs(2)).unwrap();
        assert_eq!(
            result,
            WriteResult::Ok {
                total_files: 1,
                indexed_files: 1,
            }
        );

        handle.join().unwrap();
    }

    #[test]
    fn submit_write_request_times_out_cleanly_when_no_result_appears() {
        let dir = tempfile::tempdir().unwrap();
        let staging_dir = dir.path().join("requests");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let request = WriteRequest::Index { paths: None };
        let result = submit_write_request(&staging_dir, &request, Duration::from_millis(200));
        assert!(
            result.is_err(),
            "expected a timeout error when no server ever responds"
        );
    }
}

use crate::Infigraph;

/// Reads the request at `request_path`, executes it against `infigraph`
/// (an already-open, write-mode `Infigraph` -- the daemon's own persistent
/// connection), writes the matching `.result` file, then removes the
/// request file. Never panics on a failed operation -- a request that
/// fails to index still produces an `Err` result file, so the caller's
/// `submit_write_request` poll resolves instead of timing out. A request
/// file that fails to even parse (corrupt/truncated write) also produces
/// an `Err` result rather than silently leaving the caller to time out.
///
/// Assumes the single-daemon invariant this whole design is built on: at
/// most one process ever calls this function for a given `request_path`.
/// Under that invariant there's no read-execute-write-remove TOCTOU risk;
/// this function does not defend against a second concurrent caller (e.g.
/// via a claim-by-rename step) since that scenario should never arise in
/// the real architecture -- if it's ever reused somewhere that invariant
/// doesn't hold, add that defense first.
pub fn serve_one_request(infigraph: &Infigraph, request_path: &Path) -> anyhow::Result<()> {
    let result_path = request_path.with_extension("result");

    let result = match std::fs::read_to_string(request_path)
        .map_err(anyhow::Error::from)
        .and_then(|contents| Ok(serde_json::from_str::<WriteRequest>(&contents)?))
    {
        Ok(request) => match &request {
            WriteRequest::Index { paths: None } => match infigraph.index() {
                Ok(r) => WriteResult::Ok {
                    total_files: r.total_files,
                    indexed_files: r.indexed_files,
                },
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            },
            WriteRequest::Index { paths: Some(paths) } => match infigraph.index_files(paths) {
                Ok(r) => WriteResult::Ok {
                    total_files: r.total_files,
                    indexed_files: r.indexed_files,
                },
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            },
            WriteRequest::ScipImport { scip_path } => match infigraph.import_scip(scip_path) {
                Ok(stats) => WriteResult::ScipImportOk(stats),
                Err(e) => WriteResult::Err {
                    message: e.to_string(),
                },
            },
            WriteRequest::IngestStructured { schema_id, source } => {
                match handle_ingest_structured(infigraph, schema_id, source, request_path) {
                    Ok(r) => WriteResult::Ok {
                        total_files: r.nodes_created + r.edges_created,
                        indexed_files: r.nodes_created,
                    },
                    Err(e) => WriteResult::Err {
                        message: e.to_string(),
                    },
                }
            }
            WriteRequest::UpsertRepo { namespace } => match infigraph.backend() {
                Some(b) => match b.upsert_repo(namespace) {
                    Ok(()) => WriteResult::Ok {
                        total_files: 0,
                        indexed_files: 0,
                    },
                    Err(e) => WriteResult::Err {
                        message: e.to_string(),
                    },
                },
                None => WriteResult::Err {
                    message: "graph not initialized".to_string(),
                },
            },
            WriteRequest::DeriveTestedBy { files } => {
                let files_ref: Option<Vec<&str>> = files
                    .as_ref()
                    .map(|f| f.iter().map(String::as_str).collect());
                match infigraph.backend() {
                    Some(b) => match b.derive_tested_by_edges(files_ref.as_deref()) {
                        Ok(count) => WriteResult::Ok {
                            total_files: 0,
                            indexed_files: count,
                        },
                        Err(e) => WriteResult::Err {
                            message: e.to_string(),
                        },
                    },
                    None => WriteResult::Err {
                        message: "graph not initialized".to_string(),
                    },
                }
            }
        },
        Err(e) => WriteResult::Err {
            message: format!("failed to read/parse request: {e}"),
        },
    };

    write_atomic(&result_path, &serde_json::to_string(&result)?)?;
    // Tolerate the request file already being gone: a caller that timed
    // out (submit_write_request) removes its own request file, and this
    // function may race that removal if it finishes serving right around
    // the caller's timeout. The result file above is still written either
    // way -- an accepted, documented gap (unconsumed result files
    // accumulating in the staging directory) that the watcher-wiring plan
    // needs to address with a cleanup/TTL pass, not silently ignored here.
    std::fs::remove_file(request_path).ok();
    Ok(())
}

fn handle_ingest_structured(
    infigraph: &Infigraph,
    schema_id: &str,
    source: &IngestSource,
    request_path: &Path,
) -> anyhow::Result<crate::structured::IngestResult> {
    let backend = infigraph
        .backend()
        .ok_or_else(|| anyhow::anyhow!("graph not initialized"))?;
    let schemas = crate::structured::discover_schemas(infigraph.root())?;
    let (_, schema) = schemas
        .iter()
        .find(|(_, s)| s.schema.schema_id == schema_id)
        .ok_or_else(|| anyhow::anyhow!("schema '{schema_id}' not found"))?;

    match source {
        IngestSource::File(path) => {
            let full_path = infigraph.root().join(path);
            backend.ingest_structured_file(&schema.schema, &full_path)
        }
        IngestSource::Directory(path) => {
            let full_path = infigraph.root().join(path);
            backend.ingest_structured_directory(&schema.schema, &full_path)
        }
        IngestSource::Inline => {
            let sibling_path = request_path.with_extension("data.json");
            let contents = std::fs::read_to_string(&sibling_path)?;
            let data: Vec<serde_json::Value> = serde_json::from_str(&contents)?;
            let result = backend.ingest_structured_data(&schema.schema, &data)?;
            std::fs::remove_file(&sibling_path).ok();
            Ok(result)
        }
    }
}

/// Writes `data` as a sibling `.data.json` file next to where a request
/// named `request_path` will be written, using the same atomic-write
/// guarantee as the request/result files themselves. Returns the path the
/// server-side handler will read (request_path.with_extension("data.json")).
///
/// Note: this helper needs the FINAL request path to derive the sibling
/// path, but `submit_write_request` currently generates that path
/// internally and doesn't expose it before writing the request. Task 13's
/// wrapper will need `submit_write_request` (or a variant of it) to write
/// the sibling file *before* the request file, using the same generated
/// name -- this only establishes the naming convention and the
/// server-side read/cleanup (see `handle_ingest_structured`'s
/// `IngestSource::Inline` arm).
pub fn write_ingest_inline_sibling(
    request_path: &Path,
    data: &[serde_json::Value],
) -> anyhow::Result<PathBuf> {
    let sibling_path = request_path.with_extension("data.json");
    write_atomic(&sibling_path, &serde_json::to_string(data)?)?;
    Ok(sibling_path)
}
