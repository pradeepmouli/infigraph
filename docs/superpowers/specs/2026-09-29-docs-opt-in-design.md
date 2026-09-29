# Document indexing is opt-in per project

## Problem

Every daemon creates `.infigraph/docs.kuzu` at startup. The read service's
probe, `daemon_row_source` (`crates/infigraph-docs/src/daemon_source.rs`),
calls `DocStore::open` on it to fail fast, and `DocStore::open` creates a
store that does not exist. The daemon's doc thread attaches once
`docs.kuzu` exists, so it then attaches and runs its catch-up reindex. The
result: **every project silently gets a document index, embeddings and a
doc watcher**, whether or not anyone asked for one. It also re-created a
removed worktree's `.infigraph/` on 2026-09-28: a daemon that outlived its
worktree opened the docs store once more and brought the directory back.

Separately, `infigraph index-docs` does not work under the default daemon
backend. It indexes, then fails its write with "document writes are not
routed through the daemon (upsert_docs)", because documents have no daemon
write path (#204 routed only code-graph writes). MCP's `index_docs` shells
out to the same command. Documents only get indexed today because of the
probe's side effect.

## Decisions (user, 2026-09-28/29)

1. Document indexing is **opt-in per project**: on after `index-docs` has
   run once there, or `[docs] enabled = true`. Off otherwise.
2. **Existing indexes stay on.** A project that already has a `docs.kuzu`
   is treated as opted in; only new projects start opted out.
3. `index-docs` works under the daemon backend by **asking the daemon to
   index** (approach A), not by shipping document writes over the socket
   (B) or opening the store outside the daemon (C).

## Goals

- A fresh project gets no `docs.kuzu`, no doc embeddings and no doc watcher.
- `index-docs` (CLI and MCP) succeeds under the default daemon backend,
  prints real counts, and opts the project in.
- `clean-docs` opts the project out and stays out.
- Nothing re-creates a deleted `.infigraph/` as a side effect.
- One rule, one function: every "is doc indexing on here?" question goes
  through `docs_enabled(root)`.

Non-goals: remote/Postgres mode (its `index-docs` path writes directly and is
unchanged); changing how documents are chunked, embedded or searched.

## Design

### 1. The switch and the migration

- **Setting.** `[docs] enabled: bool`, default `false`, env override
  `INFIGRAPH_DOCS_ENABLED`, declared once in a new `settings!` `docs` group in
  `infigraph-docs`. `docs_enabled(root) -> bool` resolves it for a project
  (project `config.toml`, then env, as every group resolves).
- **Writer.** The settings system only reads today. Add one writer next to
  the reader in `infigraph-core`'s `settings_file`:
  `set_project_setting(root, section, key, value)`. It edits
  `.infigraph/config.toml` with `toml_edit`, preserving comments, formatting
  and other keys, and writes atomically. It is the only way code sets
  `[docs] enabled`; nothing hand-edits TOML.
- **Who writes it.** Exactly three writers: the executor in section 3
  (`true`), `clean-docs` (`false`), and the startup migration below (`true`,
  only when the key is absent).
- **Recorded vs resolved.** The migration needs to tell *absent* from
  *false*, which the resolved value hides. `docs_enabled_recorded(root) ->
  Option<bool>` reads the raw key from the project's `config.toml`.
- **Migration (keep existing indexes on).** Once, at daemon startup, before
  the doc thread starts: if `docs.kuzu` exists **and** the key is absent,
  record `enabled = true`. An explicit `false` is respected, even with a
  `docs.kuzu` present, so a user who ran `clean-docs` or opted out is never
  switched back on. This is the only migration site.

### 2. The probe and the watcher gate

- **The probe creates nothing.** `daemon_row_source` still registers the docs
  row source. At startup it opens `docs.kuzu` only if the file exists (to
  keep failing fast on a broken store). Each request checks the file first:
  a missing index answers `documents are not indexed for this project; run
  \`infigraph index-docs\`` and creates nothing. Without the per-request
  check a stray `search_docs` would open, create, and so opt the project
  back in.
- **The doc thread is gated on the switch, not the file.** It attaches when
  `docs_enabled(root)` is true; its first catch-up reindex creates
  `docs.kuzu` if missing (so setting `[docs] enabled = true` by hand works).
  It detaches when the switch turns false. It already polls for the file;
  it reads the setting on the same poll instead.
- **MCP's in-process fallback**, `auto_start_doc_watch_inner`
  (`crates/infigraph-mcp/src/tools/docs.rs`), uses the same
  `docs_enabled` gate in place of its `docs.kuzu`-exists check.
- A worktree made by `infigraph worktree init` clones the main checkout's
  `.infigraph/`, `config.toml` included, so its switch matches the main
  project's.

### 3. `IndexDocs`: the daemon indexes

- **One executor.** `index_docs(root, namespace: Option<String>, full: bool)
  -> Result<DocIndexStats>` records `enabled = true`, then runs the existing
  indexer (`DocIndex::init` + `index`; with `full`, the wipe-and-rebuild
  `reindex-docs` performs today). `DocIndexStats` carries what `index-docs`
  prints: files scanned, files indexed, chunks created, documents and chunks
  in the store.
- **Two callers.** The daemon runs it for a new
  `WriteRequest::IndexDocs { namespace, full }` received over #204's socket
  transport. The CLI runs it directly when the process has opted out of the
  daemon (`INFIGRAPH_BACKEND=kuzu`), which already opens stores itself.
  There is no second indexer and no document data on the socket.
- **Where it runs.** On a background task, like `FullReindex`, never on the
  coordinator thread, so a long first index (embeddings included) never
  blocks code-graph writes.
- **Concurrency with the doc watcher.** `DocStore::open` holds the
  process-wide `DB_LOCK` for the store's lifetime, so an `IndexDocs` run and
  the watcher's catch-up serialize; the later one finds no changed content
  (content hashes) and does little.
- **Reply and errors.** #204's semantics, unchanged: admission, then a new
  `WriteResult::DocsIndexed(DocIndexStats)` (a variant of its own, because
  `WriteResult::Ok`'s fields describe code-graph writes), or `Lost`, or the
  daemon's recorded fault. `index-docs` uses a long timeout, like `FullReindex`. A client that
  disconnects before the op starts withdraws it; a started op finishes.
- `WriteRequest::kind()` needs no change (it reads the variant name from
  serde).

### 4. Callers

| Caller | Change |
|---|---|
| `infigraph index-docs` | submits `IndexDocs` under the daemon backend and prints the reply; calls the executor directly under `INFIGRAPH_BACKEND=kuzu`. `--namespace` passes through. |
| `infigraph reindex-docs` | the same path with `full: true` |
| `infigraph clean-docs` | records `enabled = false` **first**, so the doc thread detaches and stops writing, then deletes the index (stop the writer, then delete). |
| MCP `index_docs`, `clean_docs`, `watch_docs` | unchanged; they call the CLI or controls, which now work |
| `search_docs` (CLI and MCP) | on a project that is not enabled, returns the "run `infigraph index-docs`" message instead of empty results |
| `doctor` | one line: docs enabled or disabled, and whether `docs.kuzu` agrees (enabled but missing: run `index-docs`; present but disabled: run `clean-docs` to reclaim the space) |
| Groups (`group_build`, combined doc search) | include a repo's documents only if that repo is enabled; they check for `docs.kuzu` today and route through `docs_enabled` instead |

Remote/Postgres mode keeps its direct write path.

## Testing

Each test is written to fail first.

- **Switch and migration (unit).** `docs_enabled` defaults off and honours
  config and env. `set_project_setting` preserves comments and other keys.
  Migration: absent key + `docs.kuzu` present becomes on; explicit `false`
  stays off; absent key with no `docs.kuzu` stays off.
- **The side effect.** A daemon started on a fresh project creates no
  `docs.kuzu`. A `search_docs` there returns the message and still creates
  nothing.
- **Watcher gate.** Turning the switch on attaches and indexes, even with no
  `docs.kuzu`; turning it off detaches.
- **End to end, real daemon.** `infigraph index-docs` under the default
  daemon backend succeeds, prints counts, turns the switch on, and
  `search_docs` then finds the document. The same under
  `INFIGRAPH_BACKEND=kuzu`. `reindex-docs` rebuilds.
- **clean-docs.** Disables, deletes, and the daemon does not re-create
  `docs.kuzu` afterwards, checked across several poll intervals.
- **Groups.** A disabled repo contributes nothing to combined doc search.
- **Existing tests** that relied on the probe creating `docs.kuzu` (the
  doc-watcher attach tests, `watch_daemon_docs`) opt in explicitly. None is
  weakened.

## Review focus

1. **Every `DocStore::open` call site.** Opening creates the store, so any
   path besides the probe that opens on a missing file opts a project back
   in. The plan audits all of them and routes each through an explicit
   "exists or enabled" decision.
2. **Absent vs `false`** in the migration read.
3. **`IndexDocs` overlapping the watcher's catch-up** (serialized by
   `DB_LOCK`; confirm neither run can see a half-written store).
