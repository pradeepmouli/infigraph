//! Deterministic (not incidental/ambient) reproduction of a concurrency question
//! left open by the paused lbug 0.16.0 -> 0.18.3 version-bump investigation:
//! does a read-only query return CORRECT data (or fail cleanly) when a writer
//! is ACTIVELY reindexing concurrently, the way `infigraph watch` holds one
//! connection open and reindexes on every file-change batch
//! (crates/infigraph-core/src/watch/mod.rs)?
//!
//! **The reader runs in a separate process, deliberately.** An earlier version
//! kept it on a thread in the test process, so a second Kuzu `Database` object
//! was open on the same path as the writer's. That is the one arrangement this
//! codebase documents as unsafe -- "Kuzu only allows safe concurrent access
//! within one process's `Database` object, not across two, even in the same
//! process" (crates/infigraph-core/src/daemon/mod.rs) -- and it did exactly
//! what that warning implies: SIGSEGV on both macOS and ubuntu, killing the
//! whole test binary before the harness had even printed the test's name.
//!
//! That arrangement is also not the one production runs in. Real concurrent
//! reads are an `infigraph-mcp` process querying while a separate daemon
//! process writes: two processes, one `Database` each. This test now exercises
//! that, so a failure here says something about the configuration that ships
//! rather than about one the code already forbids.
//!
//! Goes directly through `KuzuBackend::open_read_only` + `get_symbols_for_search`
//! (`MATCH (s:Symbol) RETURN ...`) -- the exact query at the heart of the
//! original finding (crates/infigraph-mcp/src/tools/search.rs's
//! `get_search_data_local` calls this to build BM25/vector search) -- rather
//! than through the full `search` MCP tool, whose BM25/embeddings rebuild cost
//! made two earlier attempts too slow to get enough concurrent read samples
//! (4 samples in 8s of wall-clock).
//!
//! One file (`mod_stable.py`) is indexed once and never touched again -- its
//! symbol's presence is guaranteed data-wise for the whole test, so any read
//! failure to find it while the writer churns a SEPARATE file can only be a
//! concurrent-access artifact, not a fixture bug.

use infigraph_core::graph::{GraphBackend, KuzuBackend};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Project root the reader child should query. Its presence is also what
/// tells the child it is the reader rather than an ordinary test run.
const READER_ROOT_ENV: &str = "INFIGRAPH_TEST_CONCURRENT_READER_ROOT";
/// File whose appearance tells the reader child the writer has finished.
const READER_STOP_ENV: &str = "INFIGRAPH_TEST_CONCURRENT_READER_STOP";

/// How long the writer churns: long enough for many reads, short enough that
/// this stays a test rather than a soak.
const WRITE_WINDOW: Duration = Duration::from_secs(6);
/// Hard ceiling for the child, so a parent that dies without writing the stop
/// sentinel cannot leave it running forever.
const READER_CEILING: Duration = Duration::from_secs(60);

fn write_fixture(dir: &Path, filename: &str, content: &str) {
    std::fs::write(dir.join(filename), content).expect("write fixture file");
}

/// Counts parsed back out of the child's stdout.
#[derive(Debug, Default)]
struct ReaderCounts {
    attempts: usize,
    correct: usize,
    clean_errors: usize,
    wrong: usize,
    /// Distinct `churn_fn_N` symbols the reader observed. The writer renames
    /// that symbol every iteration, so this is the freshness signal:
    /// `correct` counts only `stable_marker_fn`, which is present in every
    /// version of the graph and therefore cannot distinguish a current read
    /// from a stale snapshot.
    distinct_churn: usize,
}

/// Re-exec helper: the reader half, in its own process.
///
/// Returns immediately during an ordinary suite run -- only the parent test
/// below, which sets [`READER_ROOT_ENV`], turns this into the reader. Mirrors
/// `infigraph-core/tests/abort_breadcrumb.rs`'s `abort_helper`.
#[test]
fn concurrent_reader_helper() {
    let Some(root) = std::env::var_os(READER_ROOT_ENV) else {
        return;
    };
    let root = PathBuf::from(root);
    let stop = PathBuf::from(
        std::env::var_os(READER_STOP_ENV).expect("reader child needs a stop-sentinel path"),
    );
    let db_path = root.join(".infigraph").join("graph");

    let mut counts = ReaderCounts::default();
    let mut samples: Vec<String> = Vec::new();
    let mut seen_churn: std::collections::HashSet<String> = std::collections::HashSet::new();
    let deadline = Instant::now() + READER_CEILING;

    // Progress goes to STDERR, deliberately, and one line per phase.
    //
    // This child segfaulted on the macOS runner and its stdout carried only
    // "running 1 test" -- nothing about how far it got. That is not because
    // it printed nothing: Rust's stdout is BLOCK-buffered when it is a pipe
    // (which it is here, the parent captures it), so a SIGSEGV takes the
    // whole buffer with it. Rust's stderr is unbuffered, so the last line
    // written survives the crash.
    //
    // Three phases rather than one heartbeat, because they have different
    // causes: `open` constructs a `Database` against a file another process
    // is writing, `query` reads through it, and `drop` tears the connection
    // down -- and closing a database while a writer is mid-transaction is
    // just as plausible a crash site as opening one. Whichever phase the
    // last surviving line names is where it died.
    while !stop.exists() && Instant::now() < deadline {
        counts.attempts += 1;
        eprintln!("READER_AT n={} phase=open", counts.attempts);
        match KuzuBackend::open_read_only(&db_path) {
            Ok(reader) => {
                eprintln!("READER_AT n={} phase=query", counts.attempts);
                match reader.get_symbols_for_search() {
                    Ok(rows) if rows.iter().any(|r| r[1] == "stable_marker_fn") => {
                        counts.correct += 1;
                        // Freshness probe: the writer renames the churn symbol
                        // on every iteration, so the number of DISTINCT churn
                        // names this reader observes says whether it is seeing
                        // new commits or replaying one snapshot.
                        // `stable_marker_fn` alone cannot tell those apart --
                        // it is present in every version of the graph, stale
                        // or current.
                        for r in &rows {
                            if r[1].starts_with("churn_fn_") {
                                seen_churn.insert(r[1].clone());
                            }
                        }
                    }
                    Ok(rows) => {
                        counts.wrong += 1;
                        if samples.len() < 3 {
                            samples.push(format!(
                                "query succeeded but missing stable_marker_fn ({} rows)",
                                rows.len()
                            ));
                        }
                    }
                    Err(e) => {
                        counts.clean_errors += 1;
                        if samples.len() < 3 {
                            samples.push(format!("query error: {e}"));
                        }
                    }
                }
                eprintln!("READER_AT n={} phase=drop", counts.attempts);
                drop(reader);
            }
            Err(e) => {
                counts.clean_errors += 1;
                if samples.len() < 3 {
                    samples.push(format!("open error: {e}"));
                }
            }
        }
    }
    // Reaching here at all distinguishes "finished the window" from "died
    // mid-loop": without it, a crash on the final attempt and a clean exit
    // would leave identical trailing output.
    eprintln!("READER_AT done n={}", counts.attempts);

    // Machine-readable single line: the parent asserts on these numbers, so
    // they have to survive the harness's output handling intact.
    println!(
        "READER_RESULT attempts={} correct={} clean_errors={} wrong={} distinct_churn={}",
        counts.attempts,
        counts.correct,
        counts.clean_errors,
        counts.wrong,
        seen_churn.len()
    );
    for s in &samples {
        println!("READER_SAMPLE {s}");
    }
}

/// Last `n` lines of `text`, for reporting where the child got to without
/// pasting a few thousand progress lines into the failure message.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn parse_counts(stdout: &str) -> Option<ReaderCounts> {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("READER_RESULT"))?;
    let mut c = ReaderCounts::default();
    for field in line.split_whitespace().skip(1) {
        let (k, v) = field.split_once('=')?;
        let v: usize = v.parse().ok()?;
        match k {
            "attempts" => c.attempts = v,
            "correct" => c.correct = v,
            "clean_errors" => c.clean_errors = v,
            "wrong" => c.wrong = v,
            "distinct_churn" => c.distinct_churn = v,
            _ => {}
        }
    }
    Some(c)
}

#[test]
fn concurrent_writer_reader_raw_query_correctness_under_load() {
    let project = tempfile::tempdir().expect("project tmpdir");
    let root = project.path().to_path_buf();
    let db_path = root.join(".infigraph").join("graph");
    let stop_path = root.join(".infigraph").join("reader.stop");

    write_fixture(
        &root,
        "mod_stable.py",
        "def stable_marker_fn():\n    pass\n",
    );
    write_fixture(&root, "mod_churn.py", "def churn_fn_0():\n    pass\n");
    {
        let registry = infigraph_languages::bundled_registry().expect("bundled registry");
        let mut prism = infigraph_core::Infigraph::open(&root, registry).expect("open initial");
        prism.init().expect("init initial");
        prism.index().expect("index");
    }

    // Control: confirm the stable symbol is findable via the exact same
    // direct-query path, with zero concurrent activity. Scoped so this
    // reader's `Database` is dropped before the writer below opens its own --
    // two live `Database` objects in one process is precisely the arrangement
    // this test now exists to stay out of.
    {
        let reader = KuzuBackend::open_read_only(&db_path).expect("control open_read_only");
        let rows = reader.get_symbols_for_search().expect("control query");
        assert!(
            rows.iter().any(|r| r[1] == "stable_marker_fn"),
            "control case (no concurrent writer) should find the indexed symbol: {rows:?}"
        );
    }

    // --- Reader: a separate process, started before the writer so its reads
    // straddle the whole write window. ---
    let reader_child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["concurrent_reader_helper", "--exact", "--nocapture"])
        .env(READER_ROOT_ENV, &root)
        .env(READER_STOP_ENV, &stop_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn reader child");

    // --- Writer: this process opens ONE connection and keeps it alive for the
    // whole window (matching watch_db's held-open-connection design),
    // repeatedly overwriting the SAME churn file (keeps corpus size, and so
    // per-iteration write cost, constant) so many fast iterations fit in a
    // short window. ---
    {
        let registry = infigraph_languages::bundled_registry().expect("bundled registry writer");
        let mut prism = infigraph_core::Infigraph::open(&root, registry).expect("open writer");
        prism.init().expect("init writer");
        let deadline = Instant::now() + WRITE_WINDOW;
        let mut i = 0usize;
        while Instant::now() < deadline {
            i += 1;
            write_fixture(
                &root,
                "mod_churn.py",
                &format!("def churn_fn_{i}():\n    pass\n"),
            );
            prism.index().expect("index");
        }
        // Sentinel first, then drop: the reader's final iterations then still
        // land while a writer is genuinely attached, which is the window this
        // test is about.
        std::fs::write(&stop_path, b"").expect("write stop sentinel");
    }

    let out = reader_child
        .wait_with_output()
        .expect("reader child output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr_full = String::from_utf8_lossy(&out.stderr);
    // The child emits a READER_AT line per phase per attempt, so its stderr
    // runs to thousands of lines. Only the tail matters: the last surviving
    // line names the attempt and the phase it died in.
    let stderr = tail(&stderr_full, 12);

    assert!(
        out.status.success(),
        "the reader process must exit cleanly -- a crash here is the finding, not a flake.\n\
         status: {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status
    );

    let counts = parse_counts(&stdout).unwrap_or_else(|| {
        panic!("reader child printed no READER_RESULT line.\nstdout:\n{stdout}\nstderr:\n{stderr}")
    });

    println!(
        "RESULT: {} concurrent read attempts (separate process) while writer actively reindexed \
         -- {} correct, {} clean errors, {} silently wrong, {} distinct churn symbols seen",
        counts.attempts, counts.correct, counts.clean_errors, counts.wrong, counts.distinct_churn
    );
    for line in stdout
        .lines()
        .filter(|l| l.trim_start().starts_with("READER_SAMPLE"))
    {
        println!("  {}", line.trim_start());
    }

    assert!(
        counts.attempts > 0,
        "the reader never completed an attempt, so this proved nothing about concurrency.\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );

    assert_eq!(
        counts.wrong, 0,
        "BUG REPRODUCED: {} of {} concurrent read-only queries succeeded (no error) but returned \
         INCORRECT/INCOMPLETE data (missing 'stable_marker_fn', which is guaranteed present and \
         never touched by the writer) while a writer in another process was actively reindexing. \
         This is the silent-partial-results failure mode -- a lock conflict should either be \
         transparent (correct data) or fail loudly (a clean error), never silently wrong.\n\
         stdout:\n{stdout}",
        counts.wrong, counts.attempts
    );

    assert_eq!(
        counts.correct + counts.clean_errors,
        counts.attempts,
        "accounting mismatch: correct={} clean_errors={} attempts={}",
        counts.correct,
        counts.clean_errors,
        counts.attempts
    );
}
