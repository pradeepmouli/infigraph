# Degraded Modes Are Visible (#75) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans, with superpowers:test-driven-development inside each task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** When infigraph carries on in a degraded mode, the person can see it in `infigraph doctor`, in the MCP `get_stats` tool and in the tool footers, including when the degradation happened inside the daemon. Today every daemon-side fallback reaches only `.infigraph/daemon.log`.

**Architecture:** One module in `infigraph-core` defines the set of degraded modes and their wording, and one `gather(root)` returns the current ones as a serializable struct. `gather` combines facts it re-derives from disk in the asking process with a list the live daemon reports over the status socket. `doctor`, `get_stats`, the MCP footer and the daemon's one-time warnings all render from it.

**No spec.** Same basis as the pipeline work: the user's standing "straight to plan". Decisions are below.

## Priority rule (user, 2026-10-01)

Pre-daemon paths are low priority. Design and test for the default daemon backend. Where an `INFIGRAPH_BACKEND=kuzu` path differs, note it in the report and do not stop for it.

## Decisions (brainstorm rulings, 2026-10-01)

| # | Decision |
|---|---|
| D1 | The embedder that built `embeddings.bin` is recorded durably, in a **new sibling marker** (not by changing the 8-byte generation marker's format), written by the same code path that writes the generation marker. Absent marker = unknown, which is not reported as degraded. |
| D2 | A query-time embedder that differs from the recorded one is a degraded mode to report. Search behavior does not change in this branch. |
| D3 | Event-type losses (dropped edges on COPY→UNWIND, call resolution failed, embedding update failed) are not modes. They go to #78's "last index result". Out of this branch. |
| D4 | "Polling fallback" is dropped from #75: `create_watcher` builds only `RecommendedWatcher`, and `PollWatcher` appears only in a comment (`watch/producer.rs:53`). No watch-backend reporting. The closing note corrects the issue text. |
| D5 | The daemon reports its own live modes through an additive `degraded` list on `StatusReport` with `#[serde(default)]`. `query_status` takes no lease and starts no daemon; keep that. |
| D6 | #78 is a separate branch after this one, as a thin layer over the same `gather`. |

## Facts at 98936f5

Read by the implementer: `watch/producer.rs:53`, `:396-402`; `watch/mod.rs:400-427`; `StatusReport` (`daemon/read_protocol.rs:132-147`); `health.rs:55-110`; `embed/mod.rs:424-446`. Confirmed by brainstorm: `PollWatcher` occurs only in that comment; `TRIGRAM_FALLBACK` is a process-wide latch (`embed/mod.rs:427`); `write_generation_marker` is at `embed/mod.rs:66`; `check_one_sidecar` at `doctor.rs:1124`; `query_status` at `daemon/control.rs:72`.

**Read by a subagent only, so re-read each before relying on it:** `mcp/tools/search.rs:192-229` (code search embeds at query time when embeddings are absent), `docs/search.rs:274-291` and `:351`, `verify.rs:130-135` (hint says BM25, which is wrong), `doctor.rs:1130`, `cli/info_commands.rs:753-763`, `daemon/backoff.rs`, `mcp_lock.rs:285-349`.

## Modes in scope

| Mode | Source of truth |
|---|---|
| Embeddings built with the trigram embedder | sibling marker (D1); re-derived |
| Query embedder differs from the one that built the index | marker vs this process's embedder; re-derived |
| Model2Vec unavailable in this process | this process's latch |
| Code HNSW index missing or unloadable | file on disk; re-derived |
| Code embeddings absent or stale | file plus generation marker; re-derived |
| Doc embeddings or doc HNSW absent (doc search is BM25-only) | files on disk; re-derived, only when docs are enabled |
| Some directories are not watched (count, first failure) | live daemon, status list |
| Document reads unavailable | live daemon, status list |
| Graph reopen backoff in progress | live daemon, status list |

Already visible and left as they are: the latched daemon fault, the growth breaker, SCIP staleness. `gather` may include them only by calling their existing checks, never by re-deriving them a second way.

## Global constraints

- DRY: one enum, one wording per mode, one `gather`. `health.rs` keeps only its MCP-specific signals (worker restarted, slow lock waits) and renders the rest from core.
- `gather` never starts a daemon, never takes a lease and never creates `.infigraph/`.
- A read never opens a store outside the daemon (AGENTS.md); `gather` uses files, lock probes and `query_status` only.
- The status field is additive and compatible both ways; add the old-shape wire test, as for `DocIndexStats`.
- Tests pin `HOME` and the backend as in the pipeline plan. Real-process tests go through `infigraph doctor` and the MCP `get_stats` tool.

## Tasks

1. **The mode set and `gather` for re-derived modes.** Core module, enum, wording, serializable result. Unit tests per mode with files arranged on disk. Fix the wrong hint in `verify.rs` by rendering from the same wording.
2. **The embedder marker (D1, D2).** One writer beside `write_generation_marker`; reader in `gather`. Tests: built with trigram is reported after a restart; absent marker reports nothing; mismatch is reported.
3. **The daemon's live list (D5).** `StatusReport.degraded`; the daemon records partial watch failures (count and first failure), document reads unavailable and reopen backoff; `gather` merges it. Wire tests for old and new shapes.
4. **Renderers.** `doctor` category, `get_stats` section, MCP footer from core, daemon one-time warning from the same wording.
5. **Real-process test.** First measure which fallback can be forced deterministically on macOS and Linux (a `HOME` with no model for trigram; an unreadable subdirectory for partial watches). Then: a real daemon in that mode, `infigraph doctor` and MCP `get_stats` both show it, and it clears when the cause is removed. Mutation: dropping the daemon's status field fails the test.
6. **Docs and gates.** `docs/DESIGN-hardening.md` §4.2 status, AGENTS.md one line (a new fallback must be added to the core mode set, or it is invisible). Per-crate tests, `cargo test --all`, full hook on the final commit.

## Stop rules

Stop and report, without committing, when: a mode cannot be detected without opening a store outside the daemon; the embedder marker would need a change to an existing file format; a subagent-read fact turns out false in a way that changes the mode table; `StatusReport` cannot take the field additively.

## Closing #75

Close with a note that corrects the issue text (D4, and that code search does not fall back to BM25), and add the D3 items to #78.
