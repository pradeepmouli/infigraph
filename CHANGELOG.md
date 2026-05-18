# Changelog

All notable changes to Infigraph are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
