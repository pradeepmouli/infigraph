---
layout: default
title: Infigraph
---

# Infigraph

**AST-powered code intelligence engine.** Indexes codebases into a persistent knowledge graph with full Cypher queries, hybrid semantic search, cross-file call resolution, and **62 programming languages**.

Built in Rust. Zero LLM dependency. Runs locally.

## Key Highlights

- **62 Languages:** Tree-sitter parsing for 62 languages + ANTLR grammar plugins for custom DSLs. Zero config.
- **Graph Database:** Full Cypher queries on your codebase — WITH, OPTIONAL MATCH, variable-length paths.
- **Semantic Search:** BM25 + Model2Vec hybrid search. Finds "retry logic" even if the function isn't named retry.
- **SCIP Integration:** Auto-downloads compiler-grade indexers (TypeScript, Python, Java, Go, Rust, C#, Ruby, Scala). Falls back to lsp-to-scip bridge for 14+ more languages.
- **Cross-File Resolution:** Import-aware call resolution links function calls to actual definitions across files.
- **HTTP Route-Aware:** Maps your API surface across 22 frameworks (Flask, Express, Spring, Actix, Phoenix, Rails, etc.).
- **Multi-Repo/Microservice:** Group repos, cross-repo Cypher queries, HTTP contract extraction, cross-service dependency detection.
- **PR Review & CI:** Symbol-level diff review with optional LLM enrichment. Configurable CI check gates (security, complexity, dead code, vulns).
- **OSV Vulnerability Scanning:** Scans dependencies against the OSV database for known vulnerabilities.
- **Design Pattern Detection:** Identifies Singleton, Factory, Observer, Strategy, Builder, and other patterns.
- **Refactor Analysis:** Complexity hotspots, coupling, near-duplicate detection, dead code — ranked by impact/effort.
- **Document Indexing:** Index PDF, DOCX, PPTX, HTML, Markdown with hybrid search.
- **Confluence Wiki Crawler:** BFS wiki crawl with incremental sync — indexes pages into the same search pipeline as code.
- **Auto-Watch:** File watcher auto-starts after indexing. Index stays fresh without manual intervention.
- **HNSW Vector Index:** Approximate nearest neighbor search for fast similarity queries at scale (~2ms for 500K symbols).
- **Session Continuity:** Persists context across AI agent sessions — summary, pending tasks, decisions, touched files.
- **69 MCP Tools:** Full AI agent integration for 11 coding agents (Claude Code, Cursor, VS Code, Copilot, Windsurf, etc.).
- **Sequence Diagrams:** Auto-generates Mermaid sequence diagrams from call graphs.
- **Cross-Language Detection:** Delphi↔COM, VB6↔COM, C#↔JNI, FFI, gRPC, WASM bridges.
- **Grammar Plugins:** Drop `.g4` + `plugin.toml` — parse any custom/internal DSL without Rust compilation.
- **Web UI:** Built-in graph explorer, search, route map at localhost:9749.
- **Export:** Neo4j Cypher, GraphML, JSON — take your graph anywhere.

## Offline-First Design

Infigraph is **built for offline operation** — everything runs locally, no cloud APIs or network access needed. The ML embedding model (`potion-base-8M`, 29MB) is bundled for immediate use without additional downloads.

This means:
- Semantic search works out of the box after cloning
- No external dependencies or API keys required
- Your codebase never leaves your machine
- Works on air-gapped systems

## Quick Start

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash

# Windows (PowerShell)
iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
```

Then ask your AI coding agent:
```
"search for authentication logic in this project"
"who calls the validate_user function?"
"show me the architecture of this codebase"
"find dead code"
```

**[Get Started →](/infigraph/getting-started)**

## Learn More

- **[Architecture & Design](/infigraph/architecture)** — How Infigraph works end-to-end
- **[Contributing](/infigraph/contributing)** — Build, test, and contribute
- **[GitHub Repository](https://github.com/intuit/infigraph)** — Source code and issue tracking
- **[License](https://github.com/intuit/infigraph/blob/main/LICENSE)** — Apache 2.0

---

Built with ❤️ by [Intuit](https://intuit.com)
