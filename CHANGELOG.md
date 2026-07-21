# Changelog

All notable changes to Infigraph are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [3.2.4] - 2026-07-21

### Fixed

- Remote (shared-Neo4j) mode: MCP read tools no longer return another project's
  data or `Symbols: 0`. Read paths that queried the global graph are now scoped
  to the repo (`org/repo`), resolved from the group registry:
  - `apply_repo_filter` (MCP reader) resolves the repo namespace from the group
    registry instead of guessing from `INFIGRAPH_ORG`, fixing `Symbols: 0`
    (with `Folders`/`Contains` still populated) when the env org didn't match.
  - `detect_cross_cutting`, `detect_routes`, and `search`/`semantic_search`
    (remote) now return only the queried repo instead of all repos.
  - `get_dependencies` is scoped through the repo's own modules, and the
    `DEPENDS_ON` edge is written only against in-repo modules (previously
    `m.file CONTAINS 'package.json'` cross-linked every repo's manifest).
  - `detect_clusters` scopes CALLS edges and symbols to the repo.
  - `GraphBackend` gains a `repo_filter()` accessor so backend-agnostic analysis
    passes can scope their own Cypher.

## [3.2.3] - 2026-07-21

### Fixed

- Remote (shared-Neo4j) mode: a single-repo webhook no longer re-indexes every
  repo in the group, and combined-graph queries no longer return 0 while
  per-repo queries succeed. Root cause was read/write disagreement on repo
  identity plus a Kùzu-only transaction statement leaking into Neo4j:
  - Webhook now pulls and runs `group build` only — the standalone `index`
    step (wrong namespace + stole the commit-change signal) is removed.
  - `f.repo` is stamped as `org/repo` at write time in both the per-file and
    bulk write paths; the global unfiltered `f.repo` backfill in `upsert_repo`
    (which stole orphan files across repos) is removed.
  - Group indexing scopes reads to the repo being indexed, so reindexing one
    repo no longer deletes every other repo's data from the shared graph.
  - Read filters resolve the same `org/repo` key that writes use, looked up
    from the group registry (source of truth) rather than derived from
    `INFIGRAPH_ORG`. Fixes MCP tools reporting `Symbols: 0 / Files: 0` (with
    globally-counted `Folders`/`Contains` still populated) when the server's
    `INFIGRAPH_ORG` didn't match the org a repo was indexed under.
  - `BEGIN TRANSACTION`/`COMMIT`/`ROLLBACK` are no-ops on the Neo4j backend
    (valid Kùzu, invalid Cypher), fixing concern/taint/reflection/config/
    dynamic-URL analysis in remote mode.
  - Org-scoped groups are usable from the CLI: `group add`/`build`/`index`
    resolve a bare group name to its org-qualified key.
- Remote mode `index` now resolves a repo's `org/repo` namespace from the group
  registry and refuses to index a repo that isn't registered in any group,
  instead of inventing a namespace from the directory name.

## [0.10.1] - 2026-05-11

### Added

- Pascal/Delphi language support (.pas, .pp, .dpr, .dpk, .lpr)
- VB6 language support (.bas, .cls, .frm)
- 30 MCP tools for AI coding agents (Claude Code, Cursor, Copilot, and more)
- Web UI at localhost:9749 with graph explorer, route map, and multi-repo groups
- Multi-repo groups with cross-service HTTP dependency detection
- SCIP index import for compiler-grade symbol enrichment
- lsp-to-scip bridge for any LSP server (C/C++, Zig, Swift, Dart, Elixir, and more)
- Dead code detection, blast radius analysis, git diff impact mapping
- Louvain community detection for functional module discovery
- HTTP route/endpoint detection across 22 frameworks

### Changed

- Graph database migrated to LadybugDB (lbug 0.16, maintained Kuzu fork)
- Embedding model: potion-base-8M 256-dim, bundled — no network or proxy required

[Unreleased]: https://github.com/intuit/infigraph/compare/v0.10.1...HEAD
[0.10.1]: https://github.com/intuit/infigraph/releases/tag/v0.10.1
