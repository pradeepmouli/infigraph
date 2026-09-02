# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Working convention

For any non-trivial analysis, review, or investigation (not routine edits), end with a **confidence** (High/Medium/Low) and a **recommendation** (e.g. merge/hold/needs more info) so the result is decision-ready.

## What this is

Infigraph is an AST-powered code intelligence engine: indexes codebases (62 languages) into a persistent embedded graph database (LadybugDB) with Cypher queries, hybrid search, cross-file call resolution, and MCP tools for AI coding agents. Rust workspace, zero LLM dependency, fully local/offline.

## Build, test, lint

```bash
cargo build --release -p infigraph-cli -p infigraph-mcp
cargo test --all
cargo fmt --all
cargo clippy --all-targets -- -D warnings
```

Requires `cmake`. CI runs fmt-check, clippy, and the full test suite across macOS/Linux/Windows — match locally before pushing.

**Change-safety workflow**: check blast radius before editing a shared/foundational path (e.g. via cross-file reference lookups) — a change in `infigraph-core` can silently break the CLI/MCP crates on top of it. Run targeted tests for the area you touched first, then `cargo test --all` before considering it done — this repo has real process-level integration tests (e.g. under `infigraph-mcp/tests/`), not just unit tests, so `--all` matters. The pre-commit hook (tracked source: `.cargo-husky/hooks/pre-commit`) enforces fmt/clippy across the whole workspace, not just changed files, and then runs three `--ignored` perf tests (`write_lock_perf`, `groups_watch_perf`, `index_perf`; see "CI / toolchain gotchas" for their known flakes) — pre-existing unrelated drift elsewhere can block an otherwise-clean change; worth checking `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings` once at the start of a session to know your baseline.

Release process: `./release.sh <version>` (see README's release section). Windows has a few build quirks around the graph DB's static-lib size and CRT linkage — see root `Cargo.toml` comments.

## Architecture

### Crate layout (`crates/`)

- **`infigraph-core`** — the engine: models, language registry, AST extraction, graph storage, search, cross-file resolution, multi-repo/groups, file watching, SCIP enrichment, and the analysis passes (taint, concerns, reflection, config binding).
- **`infigraph-languages`** — tree-sitter language packs (query files, not Rust code).
- **`infigraph-grammar-plugin`** — runtime ANTLR-based plugin system for languages without a tree-sitter grammar.
- **`infigraph-pipeline-plugin`** — runtime plugin system for data-pipeline metadata extraction.
- **`infigraph-docs`** / **`infigraph-confluence`** — document and wiki indexing, feeding the same search pipeline.
- **`infigraph-cli`** — the `infigraph` binary.
- **`infigraph-mcp`** — the MCP server binary, plus the embedded web UI and session persistence.
- **`lsp-to-scip`** — generic LSP→SCIP bridge for languages without a dedicated SCIP indexer.

### Data flow

Source → AST extraction → cross-file resolution → graph → optional SCIP enrichment → search index. Both local (embedded graph DB) and remote (`--features remote`: Neo4j + Postgres, multi-repo) modes share the same schema — see `docs/REMOTE-MULTI-REPO.md`.

### Cross-cutting invariants worth knowing before editing

- The graph DB is single-writer — write paths take an advisory lock; a new write path needs one too.
- Indexing respects `.gitignore`/`.infigraphignore` — don't hand-roll path filtering.
- The graph's edge/node schema is extensible per-language-plugin, not a fixed enum.
- File-watching uses a lock file for cross-process dedup — anything that starts a watcher must hold it for the watcher's lifetime.
- Route/decorator extraction differs by language syntax (query-based vs. AST-sibling-scan) — check both before assuming a language isn't covered.
- SCIP enrichment's scratch files are run-unique (`.infigraph/scip-tmp/<indexer>.<pid>-<nanos>.scip`, `temp_dir()/infigraph_scip_inherits.<pid>.<nanos>.parquet`; #139) because the daemon's own staleness-triggered enrichment can run alongside a user's `scip-enrich`/`index --full`. Producers remove only their own files (the directory only once empty) and sweep `.scip` leftovers older than 6h — put any new scratch file on the same pattern; never `remove_dir_all` a shared scratch dir. `.infigraph/scip-enrich.log` (the detached `scip-enrich` subprocess's stderr) is still one fixed, appended file.
- SCIP import only touches files the graph already has a `File` node for (#114). The indexers are external tools that never read `.gitignore`/`.infigraphignore`, so their output covers ignored paths (`dist/`, a gitignored `scratchpad/` of nested worktrees) and files deleted since they ran; `import_scip_index` partitions documents against `MATCH (f:File)` and skips the rest (`ImportStats::files_skipped`). A Symbol whose `file` names no File node is orphaned by construction — no DEFINES edge, unreachable by the file/prefix removal paths, an `infigraph verify` FAIL — so never write one from any new path either.
- An uncaught C++ exception inside lbug aborts the process with no Rust frame to catch it (#132). Bracket any new lbug write or open with `write_phase::enter(&"...", n)` so the daemon's SIGABRT breadcrumb names it, and run `GraphStore::growth_gate(n).tick()` inside any new long write loop so the runaway-growth breaker (#100) re-checks mid-call, not just at the call boundary.
- Never call `kuzu::Database::new` on an unvalidated path — a truncated/corrupt file can make Kuzu's parser read a bogus size field and request a huge allocation, which aborts the whole process on some platforms (observed on Linux, not macOS) before any `Result` exists to catch it. Check file size against a sane minimum first (see `GraphStore::open`/`DocStore::open`).
- Any "wipe and rebuild on open failure" logic must distinguish transient errors (lock contention, resource pressure) from actual corruption — treating every failure as corruption risks destroying real data mid-race with a concurrent reader/writer.

For deeper detail on any of these, see the matching skill/rule (`code-indexing-pipeline` for language/resolution/SCIP/watch; `analysis-subsystems` for multi-repo/taint).

### CI / toolchain gotchas

- CI's `dtolnay/rust-toolchain@stable` floats to whatever "stable" resolves to at run time — a newer clippy than your local toolchain can fail on lints your local run never sees. If local and CI disagree on fmt/clippy, suspect this before assuming a bug — confirm by installing CI's exact version locally (`rustup show active-toolchain` with a temporary `rust-toolchain.toml` override) rather than guessing.
- If a PR gets merged into `main` directly *and* separately into a feature branch, GitHub's synthetic PR-merge-commit computation can silently duplicate content (e.g. two copies of the same test module) even when neither branch alone shows a problem — this only appears in the merge commit itself. Rebase the feature branch onto current `main` to fix it for real, rather than trying to resolve it in a synthetic merge.
- Local test runs under high parallelism (`--test-threads` default) can produce resource-contention failures (e.g. mmap/buffer-manager exhaustion from multiple embedded-DB test processes) that aren't real bugs — always confirm a suspected regression with `--test-threads=1` before treating it as one.
- `infigraph-core --test daemon_protocol_watcher_wiring`'s `watch_loop_serves_write_requests_when_serve_requests_is_true` used to hang reproducibly on one long-lived local dev machine (#58; 5s reply budget, `--test-threads=1`, fresh worktree). Since 4290093 (the watch session builds its language registry once, not twice) and the budget's move to 30s it has not been observed: three isolated runs on 2026-09-02 each passed in ~5s. If it comes back, reproduce at your branch's merge-base in a throwaway worktree before investigating, and note that debug-build registry construction alone can approach several seconds on a loaded machine.
- `cargo test -p infigraph-mcp` (notably `--test groups_watch_perf`) does **not** rebuild the separate `infigraph-cli` binary, yet the test indexes its fixtures by spawning it: `tool_index_project` runs `infigraph index` resolved relative to the test executable (`<target-dir>/debug/infigraph`, falling back silently to whatever `infigraph` is on PATH), and that CLI then auto-starts a daemon from the same binary. After switching between branches with different `lbug` (Kuzu) versions the stale binary used to fail silently ("No results across repos"; the daemon side only logs to `<indexed repo>/.infigraph/daemon.log`, a tempdir for this test). Now the fixture fails up front with a "stale `infigraph` CLI at …" assertion naming both build hashes (#141), every CLI/daemon spawn path warns once per process on a build mismatch (`daemon::warn_if_cli_build_differs`), and `Infigraph::init` refuses a storage-version mismatch instead of wiping (#140). The pre-commit hook builds the CLI before running the test; for a manual run, `cargo build -p infigraph-cli` (same profile as the tests) first.
- `write_lock_perf::test_contended_lock_throughput` (pre-commit hook) gates on the *median* of five warmed-up 4-thread/single-thread ratio samples, bound `8×` by default, and prints every sample (#142). If it fails, rerun the single test (`cargo test -p infigraph-core --test write_lock_perf -- --ignored test_contended_lock_throughput`, seconds) and read the printed samples: a median far beyond the bound is a real regression in the lock path (a clap parse inside the held lock region pushed it to 5/5 failures the day #142 was filed) — investigate, never widen. Only for a machine known to be heavily loaded (load average well above the core count) with a *marginal* median, widen that one run with `INFIGRAPH_TEST_LOCK_CONTENTION_RATIO=<n>`.
- Dedup persists `dedup_state.json` into the nearest ancestor `.infigraph/` of the process cwd (`session_context::dedup_state_path`; `crates/infigraph-mcp/.infigraph/` for that crate's test binaries when the runtime dir exists, otherwise the repo root's) every fifth dedup call. `compression_eval.rs::phase3_dedup_eval` now clears it before and after itself (#136); any new test that drives `apply_seen_dedup` for real must do the same, or it fails on its second run of the day with the "(seen)" placeholder on the first call.

### Test fixtures

`tests/fixtures/microservices/` — real microservice repos (Python/Flask, TypeScript/Express, Rust/Actix) for route and cross-service tests. Prefer extending these over new synthetic fixtures.
