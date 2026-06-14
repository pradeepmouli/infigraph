# Infigraph

[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.70%2B-orange)](https://www.rust-lang.org/)
[![GitHub Release](https://img.shields.io/github/v/release/intuit/infigraph?sort=semver)](https://github.com/intuit/infigraph/releases)

**AST-powered code intelligence engine.** Indexes codebases into a persistent knowledge graph with full Cypher queries, hybrid semantic search, cross-file call resolution, and **62 programming languages**.

Built in Rust. Zero LLM dependency. Runs locally. No API keys. No network calls.

## The Problem

AI agents are **structurally blind** to your codebase. When they need to answer "who calls this function?" or "what breaks if I change this class?", they have to re-read files, retrace imports, and re-infer relationships — wasting time and tokens.

![The Hidden Cost of Code Blindness in the Age of AI](https://learnbyinsight.com/wp-content/uploads/2026/06/hidden-cost-ai-infigraph.png)

**The cost:** 60–80% of AI agent tokens spent on code rediscovery instead of solving your problem.

## Why Infigraph (What Makes It Unique)

The market has two categories of solutions — neither solves this:

**Cloud-based tools** (GitHub Copilot, Sourcegraph, Codeium, Tabnine):
- ❌ Require sending your code to external APIs
- ❌ Expensive per-query (token costs compound)
- ❌ Latency-bound by network round-trips
- ❌ Privacy concerns for proprietary code
- ✅ Good AI integration

**Local analysis tools** (ctags, Language Servers, ripgrep, CodeQL):
- ✅ Run locally, offline-capable
- ✅ No privacy/security concerns
- ❌ No persistent knowledge graph
- ❌ Limited language support (1-3 languages)
- ❌ Not designed for AI agents
- ❌ Scattered results, high token overhead for agents

**The gap Infigraph fills:**
| Feature | Cloud Tools | Local Tools | Infigraph |
|---------|-------------|------------|-----------|
| **Local-first operation** | ❌ | ✅ | ✅ |
| **Persistent knowledge graph** | ❌ | ❌ | ✅ |
| **62 language support** | ❌ | ❌ | ✅ |
| **AI agent integration** (MCP) | Limited | ❌ | ✅ |
| **Offline operation** | ❌ | ✅ | ✅ |
| **No external APIs** | ❌ | ✅ | ✅ |
| **Cypher queries on code** | ❌ | ❌ | ✅ |
| **Cross-file resolution** | ❌ | Limited | ✅ |

**No other product or OSS combines all of these.** Infigraph is the first AI-native, local-first knowledge graph engine for code.

---

## The Solution

Infigraph builds a **persistent knowledge graph** of your codebase before the agent runs. Structural questions that would cost hundreds of tokens to answer with raw file reads now resolve in milliseconds:

```
Source Code (62 languages)
    ↓
Infigraph Index (one-time, < 1min for most projects)
    ↓
Knowledge Graph (symbols, calls, imports, routes, patterns)
    ↓
AI Agent Queries (instant, precise, token-efficient)

Examples:
  "Search for auth logic" → 10-100x fewer tokens
  "Who calls validate_user?" → 1ms instead of 5s file reads
  "Blast radius of this change?" → Complete call graph traversal
```

---

## Table of Contents

- [The Problem](#the-problem) — What Infigraph solves
- [Why Infigraph](#why-infigraph-what-makes-it-unique) — Unique market position
- [The Solution](#the-solution) — How it works
- [Key Highlights](#key-highlights) — Core capabilities at a glance
- [Quick Start](#quick-start) — Install and run in 2 minutes
- [How It Works](#how-it-works) — Integration with AI coding agents
- [Offline-First Design](#offline-first-design) — No APIs, no network calls
- [Installation](#installation) — Setup for all platforms
- [Usage Examples](#usage-examples) — CLI commands, Web UI, tasks
- [Features & Architecture](#features--architecture) — Full capabilities list
- [Supported Languages (62)](#supported-languages-62) — All 62 languages
- [Contributing](#contributing) — Build from source, add languages, contribute
- [License](#license)

---

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

---

## Quick Start

### Install (one-liner)

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
```

**Windows (PowerShell):**
```powershell
iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
```

### Index your project
```bash
cd /path/to/project
infigraph index
```

### Ask your AI agent
```
"Who calls the validate_user function?"
"Show me the blast radius of this change"
"Find authentication logic in this codebase"
"What's the architecture of this project?"
```

Infigraph auto-indexes on first query. No manual setup needed for Claude Code, Cursor, or other AI agents with MCP support.

---

## How It Works

### With Claude Code (Recommended)
After install, start using Claude Code normally. Infigraph indexes your project automatically and transparently:

```
> You: "Search for authentication logic"
> Claude: [Scans via Infigraph, returns precise results with 60-80% fewer tokens]
```

No CLI commands. No separate indexing step. Just ask.

### With Other AI Agents
Any agent with MCP support (Cursor, VS Code + Copilot, Windsurf, etc.) gets 69 Infigraph tools automatically after `infigraph install`.

### Manual CLI (Optional)
```bash
infigraph search "auth"                    # Hybrid search
infigraph query "MATCH (...)"              # Cypher queries
infigraph trace-callers "function_name"   # Who calls this?
infigraph dead-code                        # Find unused functions
infigraph impact "auth.py::authenticate"   # Blast radius
```

---

## Offline-First Design

Infigraph is **built for offline operation** — everything runs locally, no cloud APIs or network access needed. The ML embedding model (`potion-base-8M`, 29MB) is bundled in this repository for immediate use without additional downloads.

This means:
- Semantic search works out of the box after cloning
- No external dependencies or API keys required
- Your codebase never leaves your machine
- Works on air-gapped systems

## Installation

### System Requirements

| Platform | Required |
|----------|----------|
| macOS | Rust (rustup), `brew install cmake` |
| Linux | Rust (rustup), `sudo apt install cmake` |
| Windows | Rust (rustup), Visual Studio Build Tools (C++20) |

**No Docker, no Python, no Node.js required** — everything is self-contained.

### macOS / Linux

```bash
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
```

This:
- Downloads pre-built binaries from GitHub releases (if available)
- Falls back to cloning + `cargo build --release` (installs Rust if needed)
- Adds `infigraph`, `infigraph-mcp`, and `lsp-to-scip` to `~/.local/bin`
- Registers MCP server for all 11 AI coding agents
- Writes primary search instructions to `~/.claude/CLAUDE.md`

> **System dependency:** `cmake` is required to build the graph database.
> Install before building: `brew install cmake` (macOS) or `sudo apt install cmake` (Linux).

### Windows

Run this single command from **PowerShell**:

```powershell
iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
```

This downloads and runs the full installer — which fetches the pre-built binary and registers the MCP server.

## Update

Re-run the installer to pull latest and rebuild:

```bash
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
```

Or if building manually:

```bash
cd /path/to/infigraph && git pull && cargo build --release
infigraph update
```

`infigraph update` re-registers MCP server paths and refreshes CLAUDE.md instructions.

Infigraph also checks for updates in the background (once per 24h) and prints a hint when a newer version is available.

## Uninstall

```bash
infigraph uninstall
```

Removes:
- MCP server config from all 11 AI agents
- Primary search instructions from `~/.claude/CLAUDE.md`

Does NOT delete the binary — remove `~/.local/bin/infigraph` and `~/.local/bin/infigraph-mcp` manually if desired.

---

## Usage Examples

### Indexing

The first time you run Infigraph on a project, it indexes all source files and builds the knowledge graph:

```bash
cd /path/to/project
infigraph index              # Builds graph (~30s–2min depending on size)
infigraph index --full       # Clean rebuild from scratch
```

### Common Tasks

| Task | Command |
|------|---------|
| **Search** | `infigraph search "auth logic"` |
| **Who calls this?** | `infigraph trace-callers "function_name"` |
| **What does this call?** | `infigraph trace-callees "function_name"` |
| **Blast radius** | `infigraph impact "file.py::function"` |
| **Dead code** | `infigraph dead-code` |
| **Routes/endpoints** | `infigraph routes` |
| **Cypher query** | `infigraph query "MATCH (s:Symbol) RETURN s.name"` |
| **Architecture** | `infigraph architecture` |
| **Design patterns** | `infigraph detect-patterns` |
| **Export** | `infigraph export json --output graph.json` |

### Web UI

Infigraph includes a built-in graph explorer at `http://localhost:9749/?path=/your/project`:

```bash
infigraph-mcp --ui --port=9749
# Opens interactive graph explorer, search, route map
```

---

## Features & Architecture

### Grammar Plugins (ANTLR)

Infigraph supports runtime-loaded ANTLR grammar plugins. Drop `.g4` grammar files + a `plugin.toml` config into a directory — infigraph parses the language automatically via a JVM subprocess. **No Rust compilation needed.**

### Quick start: adding a new language

1. Create a grammar plugin directory:
   ```
   ~/.infigraph/grammars/my-lang/
   ├── MyLang_Lexer.g4      # ANTLR lexer grammar
   ├── MyLang_Parser.g4     # ANTLR parser grammar
   └── plugin.toml          # Extension mapping + extraction rules
   ```

2. Write `plugin.toml`:
   ```toml
   [language]
   name = "my-lang"
   extensions = [".ml", ".myl"]
   entry_rule = "program"
   lexer = "MyLang_Lexer.g4"
   parser = "MyLang_Parser.g4"
   strip_preprocessor = false   # true if files have #include/#ifdef lines

   [[entities]]
   rule = "functionDecl"        # ANTLR parser rule name
   kind = "Function"            # Symbol kind (Function, Method, Class, Variable, etc.)
   name_child = "identifier"    # Child rule that holds the name
   scope = true                 # Creates a scope (section/function boundary)

   [[entities]]
   rule = "classDecl"
   kind = "Class"
   name_child = "identifier"
   scope = true

   [[relations]]
   rule = "functionCall"
   kind = "Calls"
   target_child = "identifier"

   [[relations]]
   rule = "fieldAccess"
   kind = "Reads"
   target_child = "fieldName"
   condition = "has_token:."    # Only match when DOT token is present
   ```

3. Index your project — infigraph discovers the plugin automatically:
   ```bash
   infigraph -r /path/to/project index
   ```

### Plugin discovery

Plugins are loaded from two locations:
- `~/.infigraph/grammars/*/plugin.toml` — user-level (all projects)
- `<project>/grammars/*/plugin.toml` — project-level (per repo)

Project-level plugins take precedence.

### Requirements

- **Java 11+** — the ANTLR interpreter runs in a JVM subprocess
- **No Rust toolchain needed** — grammar plugins are pure config + `.g4` files

### How it works

```
plugin.toml + .g4 files
        │
        ▼
┌─────────────────┐     stdin/stdout JSON     ┌──────────────────┐
│  Rust host       │ ◄──────────────────────► │  JVM subprocess   │
│  (infigraph)     │                          │  (ANTLR interp.)  │
│                  │  1. Load grammar          │                   │
│  Discovers       │  2. Parse file            │  Grammar.load()   │
│  plugin.toml     │  3. Get parse tree JSON   │  ParserInterpreter│
│  Walks tree      │                          │                   │
│  → Symbol/Rel    │                          │                   │
└─────────────────┘                          └──────────────────┘
```

1. **Startup**: infigraph scans plugin directories, spawns a single persistent JVM process
2. **Load**: sends `.g4` grammar paths to the JVM driver, which loads them in ANTLR interpreter mode
3. **Parse**: for each source file, sends content to JVM, receives JSON parse tree
4. **Extract**: Rust walks the JSON parse tree using `plugin.toml` entity/relation rules, produces `Symbol` + `Relation` objects
5. **Graph**: extracted data flows into the same graph/search/analysis pipeline as all other languages

### Grammar imports

If your grammar uses `import` (e.g., `import Base_Lexer;`), place the imported `.g4` file in the same plugin directory. ANTLR resolves imports from the grammar file's directory.

### Extraction rule reference

#### Entity mappings (`[[entities]]`)

| Field | Required | Description |
|-------|----------|-------------|
| `rule` | yes | ANTLR parser rule name to match |
| `kind` | yes | `Function`, `Method`, `Class`, `Struct`, `Variable`, `Constant`, `Section`, `Module`, `Field`, `Test`, `Route` |
| `name_child` | yes | Child rule that holds the entity name |
| `scope` | no | If `true`, pushes a scope (nested symbols get `parent::name` IDs) |

#### Relation mappings (`[[relations]]`)

| Field | Required | Description |
|-------|----------|-------------|
| `rule` | yes | ANTLR parser rule name to match |
| `kind` | yes | `Calls`, `Imports`, `Reads`, `Writes`, `Inherits`, `Implements`, `Contains` |
| `target_child` | yes | Child rule that holds the target name |
| `condition` | no | `has_child:RULE`, `has_token:TEXT` — only match when condition is true |

### Performance

Grammar plugins use the ANTLR interpreter (no code generation). Slower than native tree-sitter but requires zero compilation:

| Metric | Value |
|--------|-------|
| JVM cold start + grammar load | ~450ms (once) |
| Per-file parse (100 lines) | ~90ms |
| Per-file parse (500 lines) | ~770ms |
| 55-file batch | ~18s (324ms/file avg) |

The JVM stays alive for the duration of the infigraph session — subsequent parses only pay the per-file cost.

### Architecture: tree-sitter vs grammar plugins

| | Tree-sitter | Grammar Plugin |
|---|---|---|
| Grammar format | `.scm` queries | `.g4` grammars |
| Runtime | Native (compiled C) | JVM subprocess (ANTLR interpreter) |
| Adding a language | Write `.scm` query files | Drop `.g4` + `plugin.toml` |
| Compilation needed | No (queries are text) | No (interpreter mode) |
| Performance | ~1ms/file | ~100-800ms/file |
| Best for | Mainstream languages | Custom/internal DSLs |

Both backends produce the same `Symbol` + `Relation` output. Everything downstream (graph, search, analysis, MCP tools) is backend-agnostic.

## Building from Source

### Prerequisites

| Platform | Required |
|----------|---------|
| macOS | `brew install cmake`, Rust (`rustup`) |
| Linux | `sudo apt install cmake`, Rust (`rustup`) |
| Windows | Rust (`rustup`), Docker (for cross-compilation) |

### macOS (native — ARM64 or x86_64)

```bash
brew install cmake
git clone https://github.com/intuit/infigraph.git
cd infigraph
cargo build --release -p infigraph-cli -p infigraph-mcp
cp target/release/infigraph target/release/infigraph-mcp ~/.local/bin/
infigraph install
```

### Linux (x86_64 or ARM64)

```bash
sudo apt update && sudo apt install -y cmake
git clone https://github.com/intuit/infigraph.git
cd infigraph
cargo build --release -p infigraph-cli -p infigraph-mcp
cp target/release/infigraph target/release/infigraph-mcp ~/.local/bin/
infigraph install
```

### Windows (native build)

Build natively on a Windows machine. **Cross-compiling from macOS is not currently supported** — LadybugDB (lbug) requires C++20 `<format>` (GCC 13+), but available cross-compilation Docker images ship GCC 9.

```powershell
# Install Rust
winget install Rustlang.Rustup

# Install CMake (required by lbug graph DB)
winget install Kitware.CMake

# Install Visual Studio Build Tools with C++ workload
winget install Microsoft.VisualStudio.2022.BuildTools

# Clone and build
git clone https://github.com/intuit/infigraph.git
cd infigraph
cargo build --release -p infigraph-cli -p infigraph-mcp

# Binaries at:
#   target\release\infigraph.exe
#   target\release\infigraph-mcp.exe
```

### Cross-compiling macOS targets on ARM64 machine

```bash
# Add Intel target
rustup target add x86_64-apple-darwin

# Build for Intel Mac (from ARM64 machine)
cargo build --release --target x86_64-apple-darwin -p infigraph-cli -p infigraph-mcp
```

### Releasing a new version (maintainers)

The `release.sh` script builds, signs, packages with the bundled model, and uploads to GHE releases:

```bash
./release.sh v1.0.0
```

What it does:
1. Builds `aarch64-apple-darwin` targets (Intel commented out — add when needed)
2. Ad-hoc signs both binaries with `codesign --sign -` (no Apple Developer cert required)
3. Packages `infigraph`, `infigraph-mcp`, and `models/` into `infigraph-<target>.tar.gz`
4. Creates a GitHub release at `github.com/intuit/infigraph` and uploads the archive

**Requirements:** `gh` CLI authenticated with `github.com`, `cmake`, Rust toolchain

## Setup as Primary Search

After install, register Infigraph as the primary search engine for all AI coding agents:

```bash
infigraph install
```

This does four things:
1. **Registers `infigraph-mcp`** as an MCP server for 11 agents: Claude Code, Cursor, VS Code, Codex, Gemini CLI, Zed, OpenCode, Aider, Windsurf, Kiro, GitHub Copilot
2. **Writes primary search instructions** to `~/.claude/CLAUDE.md` so AI agents prefer Infigraph over raw grep/glob
3. **Installs Claude Code hooks** — enforcement hook (warns on raw search) + session save hooks (auto-save context on compaction)
4. **Configures Claude Code allowlist** — adds all Infigraph MCP tools to `~/.claude/settings.local.json` permissions

Then index your projects:
```bash
cd /path/to/project
infigraph index
# Full reindex (clean rebuild from scratch):
infigraph index --full
```

Every search, symbol lookup, and code navigation now goes through Infigraph's graph — saving 60-80% of tokens versus raw file reads.

## Web UI

Infigraph includes a built-in web UI for visual code exploration.

**It starts automatically** when the MCP server launches — open it at:

```
http://localhost:9749/?path=/your/project
```

The UI is served by the same `infigraph-mcp` process Claude Code spawns. No separate process needed.

**Multiple Claude sessions:** the first session to start binds port 9749 and serves the UI. All subsequent sessions skip the port bind silently and still get full MCP tool access. The UI stays up as long as at least one session is open.

**From Claude:** ask Claude to open the UI — it can launch a browser or give you the URL for your indexed project.

## Troubleshooting

### MCP tools not available in Claude Code

**Symptom:** `mcp__infigraph__*` tools appear as "deferred" in ToolSearch but aren't active, or Claude says Infigraph isn't loaded.

**Most common cause:** A project-level `.claude/settings.json` with an `mcpServers` block overrides the global `~/.claude/settings.json` where `infigraph install` wrote its config. Claude Code merges settings but project-level `mcpServers` takes precedence.

**Fix:** Run `infigraph install` from inside the project directory — or manually add the infigraph entry to the project's `.claude/settings.json`:

```json
{
  "mcpServers": {
    "infigraph": {
      "command": "/path/to/infigraph-mcp",
      "args": ["--ui", "--mcp", "--port=9749"]
    }
  }
}
```

Replace `/path/to/infigraph-mcp` with the output of `which infigraph-mcp`.

**Other causes:**
- Binary not found: verify `which infigraph-mcp` returns a path
- `infigraph install` failed silently during setup: re-run it manually and check for errors
- Invalid JSON in existing `settings.json`: fix the JSON, then re-run `infigraph install`

## Features

### Core
- **62 languages** — see full list below
- **LadybugDB graph database** — columnar embedded graph DB (Kuzu successor), full Cypher including WITH, OPTIONAL MATCH, variable-length paths, mutations
- **Hybrid search** — BM25 text ranking + Model2Vec neural embeddings (bundled, no network) with tunable alpha
- **HNSW vector index** — approximate nearest neighbor search for fast similarity queries at scale (~2ms for 500K symbols)
- **Embedding cache** — vectors saved to `.infigraph/embeddings.bin`, loaded on next search (first index = cache, subsequent = instant)
- **Cross-file call resolution** — import-aware resolution links function calls to actual definitions across files
- **Auto-watch** — file watcher auto-starts after indexing, keeps graph in sync via OS filesystem events with 500ms debounce
- **Session continuity** — persists session context (summary, pending tasks, decisions, touched files) across AI agent sessions
- **Docstring extraction** — captures docstrings/comments from AST, indexed for richer search
- **Function signatures** — extracts parameter lists and return types from AST for richer embeddings and search
- **Louvain community detection** — discovers functional modules/clusters in the call graph
- **Multi-repo groups** — group microservice repos, cross-repo Cypher queries, HTTP contracts, cross-service dependency detection
- **Custom edge types** — extensible relation system: language plugins can define custom edges (DECORATED_BY, SPAWNS, etc.) that persist in the graph
- **Gitignore-aware** — respects `.gitignore` and `.infigraphignore` patterns during indexing via the `ignore` crate
- **Learned resolution** — records successful cross-file call resolutions to improve accuracy on subsequent indexes

### Analysis
- **Refactor analysis** — complexity hotspots, coupling (fan-in/fan-out), near-duplicate detection, dead code, file size — ranked by impact/effort
- **PR review** — symbol-level diff review with optional LLM enrichment (Anthropic API), cross-repo blast radius
- **CI check runner** — configurable checks (security, complexity, dead code, vuln scan) with pass/fail gates
- **OSV vulnerability scanning** — scans dependencies against the OSV database for known vulnerabilities
- **Design pattern detection** — identifies Singleton, Factory, Observer, Strategy, Builder, and other patterns
- Dead code detection (uncalled functions/methods)
- Transitive impact / blast radius analysis
- Git diff → affected symbols mapping
- Architecture overview (language breakdown, hotspots, hub functions, entry points)
- HTTP route/endpoint detection across 22 languages (Flask, Express, Spring, Django, Gin, Actix, Phoenix, Rails, NestJS, and more)
- Cross-service HTTP dependency detection (`group deps`) — scans URL strings, matches to contracts across repos
- Bridge-to-call promotion — promotes detected cross-language bridges to CALLS edges for unified call graph analysis

### SCIP Integration (Compiler-grade Enrichment)
Infigraph natively imports [SCIP](https://sourcegraph.com/blog/announcing-scip) indexes to enrich the graph with precise compiler-grade symbols, types, and cross-file relationships. SCIP indexers are **auto-downloaded** — `infigraph index` detects project languages and fetches the right indexer binaries (with portable runtimes for Node.js, JRE, .NET, Dart, PHP) on first use:

```bash
# Generate SCIP index with an existing indexer
scip-typescript index --cwd .          # TypeScript/JavaScript
scip-python index --cwd .              # Python
scip-java index                        # Java/Kotlin
scip-go --cwd .                        # Go
# Then import into Infigraph
infigraph scip-import --index index.scip
```

**Languages with dedicated SCIP indexers:** TypeScript, JavaScript, Python, Java, Kotlin, Go, Rust (rust-analyzer), C# (scip-dotnet), Ruby (scip-ruby), Scala

**Languages via lsp-to-scip bridge:** C/C++, Zig, Swift, Dart, Elixir, PHP, Lua, Haskell, F#, Clojure, Erlang, Perl, OCaml, and any language with an LSP server

#### lsp-to-scip Bridge

Generic tool that spawns any LSP server and generates `index.scip`:

```bash
# C/C++ via clangd
lsp-to-scip --server clangd --root /path/to/project --lang cpp

# Zig via zls
lsp-to-scip --server zls --root . --lang zig

# Swift via sourcekit-lsp
lsp-to-scip --server sourcekit-lsp --root . --lang swift

# Elixir via elixir-ls
lsp-to-scip --server "elixir-ls" --root . --lang elixir

# Dart
lsp-to-scip --server "dart language-server" --root . --lang dart

# Haskell
lsp-to-scip --server haskell-language-server-wrapper --root . --lang haskell

# F#
lsp-to-scip --server fsautocomplete --root . --lang fsharp

# Clojure
lsp-to-scip --server clojure-lsp --root . --lang clojure

# Erlang
lsp-to-scip --server erlang-ls --root . --lang erlang

# Then import
infigraph scip-import --index index.scip
```

### Integration
- **69 MCP tools** for AI coding agents
- **11 agent auto-configs** — Claude Code, Cursor, VS Code, Codex, Gemini CLI, Zed, OpenCode, Aider, Windsurf, Kiro, GitHub Copilot
- **Web UI** at localhost:9749 with graph explorer, search, route map, multi-repo groups, contracts
- **Export** — Neo4j Cypher, GraphML, JSON

## Supported Languages (62)

| Category | Languages |
|----------|-----------|
| **Systems** | Rust, C, C++, Zig, CUDA, Verilog, Assembly |
| **JVM** | Java, Kotlin, Scala, Groovy, Clojure |
| **Web** | JavaScript, TypeScript, TSX, PHP, HTML, CSS, GraphQL |
| **Python** | Python (+ Django, Flask, FastAPI route detection) |
| **Mobile** | Swift, Kotlin, Dart, Objective-C |
| **Functional** | Haskell, OCaml, F#, Elm, Elixir, Erlang, Common Lisp, Emacs Lisp, Clojure |
| **Scripting** | Ruby, Perl, Lua, Bash, PowerShell, R, Julia, MATLAB |
| **Go ecosystem** | Go |
| **Config/Data** | TOML, YAML, JSON, XML, HCL, Makefile, CMake, Dockerfile, Starlark, INI, SQL, Protobuf |
| **Other** | Fortran, GLSL, Nix, Svelte, Markdown |
| **NEW** | **Pascal/Delphi** (.pas, .pp, .dpr, .dpk, .lpr) |
| **Grammar Plugins** | Any ANTLR4-compatible language via [grammar plugins](GRAMMAR_PLUGINS.md) |

Every language includes:
- `entities.scm` — symbols (functions, classes, methods, types, variables, constants, routes)
- `relations.scm` — call edges, imports, inheritance

## MCP Server

```bash
# Start MCP server (stdio)
infigraph-mcp

# Start with web UI
infigraph-mcp --ui --port=9749
# Open http://localhost:9749/?path=/your/project
```

### 69 MCP Tools

| Tool | Description |
|------|-------------|
| **Search & Navigation** | |
| `index_project` | Parse all source files and build code knowledge graph (60+ languages) |
| `search` | Unified code search — keyword-hybrid + semantic-hybrid + regex grep in one call, auto-escalates |
| `search_symbols` | Find symbols by name (keyword-weighted hybrid, alpha=0.3) |
| `semantic_search` | Find code by meaning (semantic-weighted hybrid, alpha=0.85) |
| `search_code` | Regex text search across all project files |
| `get_symbols_in_file` | List all symbols in a file with line numbers |
| `get_code_snippet` | Source code for a symbol by ID |
| `get_doc_context` | Full context: signature + docstring + source + callers + callees in one call |
| `symbol_context` | 360° view: callers, callees, parent scope, file, kind, docstring |
| `list_files` | List source files with optional glob pattern filter |
| **Analysis** | |
| `trace_callers` | Direct callers of a symbol |
| `trace_callees` | What a symbol calls |
| `transitive_impact` | Blast radius — all symbols transitively affected by changes |
| `find_all_references` | Every location where a symbol is referenced |
| `detect_dead_code` | Unreachable functions/methods with zero callers |
| `detect_changes` | Git diff → affected symbols and blast radius |
| `semantic_diff` | Symbol-level diff between git refs (added/removed/moved/changed) |
| `git_summary` | Symbol-level commit history (which functions changed per commit) |
| `get_complexity` | Cyclomatic complexity metrics per symbol |
| `detect_clones` | Near-duplicate functions via vector similarity |
| `detect_clusters` | Louvain community detection on call graph |
| `detect_security_issues` | Security scan: SQL injection, secrets, eval, path traversal, XSS, etc. |
| `detect_bridges` | Cross-language boundaries: FFI, JNI, cgo, gRPC, WASM, COM |
| `detect_routes` | HTTP route/endpoint detection (22 frameworks) |
| `get_test_coverage` | Test coverage analysis — covered %, uncovered symbols |
| `refactor` | Refactoring analysis — complexity, coupling, duplication, size, dead code. Ranked recommendations with impact/effort scores |
| **Architecture** | |
| `get_architecture` | Codebase overview: language breakdown, hotspots, hub functions, entry points |
| `get_api_surface` | Public API surface — all public symbols and HTTP routes |
| `get_file_deps` | File-level import graph (what imports what) |
| `get_type_hierarchy` | Full inheritance tree (ancestors + descendants) |
| `get_graph_schema` | Node/edge types, counts, property names |
| `get_stats` | Graph statistics |
| `generate_sequence_diagram` | Mermaid sequence diagram from call graph |
| **Dependencies** | |
| `index_manifests` | Parse package manifests (package.json, Cargo.toml, go.mod, etc.) |
| `get_dependencies` | List external dependencies by ecosystem |
| `scip_import` | Import SCIP index for compiler-grade enrichment |
| **Review & CI** | |
| `review` | PR review — symbol-level diff analysis with optional LLM enrichment |
| **Document Search** | |
| `index_docs` | Index documents (PDF, DOCX, PPTX, Markdown, HTML) |
| `reindex_docs` | Reindex all documents from scratch |
| `clean_docs` | Remove document index |
| `search_docs` | Search indexed documents with hybrid BM25+semantic |
| `watch_docs` | Watch document directory for changes |
| `stop_watch_docs` | Stop document watcher |
| **Confluence** | |
| `index_confluence` | Crawl and index Confluence wiki space |
| `index_confluence_pages` | Index specific Confluence pages by ID |
| **Multi-repo Groups** | |
| `group_create` | Create a repo group |
| `group_add` | Add repo to group |
| `group_list` | List groups and members |
| `group_index` | Index all repos in a group |
| `group_query` | Cross-repo Cypher query |
| `group_sync` | Extract HTTP contracts across repos |
| `group_contracts` | List discovered contracts |
| `group_deps` | Cross-service HTTP dependency detection |
| `group_link` | Link cross-service deps as CALLS_SERVICE edges |
| **Export & Visualization** | |
| `export_graph` | Export as Cypher/GraphML/JSON |
| `visualize` | Interactive HTML graph visualization |
| `visualize_symbol` | Focused subgraph centered on one symbol |
| **Project Management** | |
| `list_projects` | All indexed repos |
| `delete_project` | Remove project data |
| `list_languages` | Supported languages and extensions |
| **File Watching** | |
| `watch_project` | Background file watcher with auto-reindex |
| `stop_watch` | Stop a running watcher |
| `get_watch_status` | Check watcher status and pending reindexes |
| **Session Context** | |
| `save_session` | Save session context to graph DB with TOUCHED edges + semantic embedding. Auto-purges after configurable days (default: 30) |
| `get_latest_session` | Retrieve most recent session — summary, pending tasks, decisions, linked files. Suggests purge if old sessions exist |
| `purge_sessions` | Delete sessions older than N days (default: 30). User-initiated cleanup |

## Architecture

> For a detailed technical design — including graph schema, hybrid search internals, incremental indexing, cross-file call resolution, and design rationale — see [ARCHITECTURE.md](ARCHITECTURE.md).

```
infigraph/
├── crates/
│   ├── infigraph-core/          # Graph DB, parsing, search, analysis
│   │   └── src/
│   │       ├── model/           # Symbol, Relation, FileExtraction types
│   │       ├── lang/            # LanguagePack, LanguageRegistry
│   │       ├── extract/         # AST entity + relation extraction
│   │       ├── graph/           # LadybugDB store, schema, queries
│   │       ├── search/          # BM25, hybrid search, grep
│   │       ├── embed/           # Model2Vec (bundled potion-base-8M, 256-dim)
│   │       ├── resolve/         # Cross-file call resolution
│   │       ├── cluster/         # Louvain community detection
│   │       ├── multi/           # Multi-repo registry, groups, contracts, cross-service deps
│   │       ├── routes/          # HTTP route detection (22 frameworks)
│   │       ├── scip/            # SCIP index import (compiler-grade enrichment)
│   │       ├── refactor/        # Refactoring analysis (complexity, coupling, clones, dead code)
│   │       ├── watch/           # File watcher with auto-start after indexing + change batching
│   │       ├── session/         # Session context persistence across AI sessions
│   │       ├── review/          # PR review engine with optional LLM enrichment
│   │       ├── check/           # CI check runner (security, complexity, dead code gates)
│   │       ├── vuln/            # OSV vulnerability scanning
│   │       ├── patterns/        # Design pattern detection
│   │       ├── learned/         # Learned resolution patterns for cross-file calls
│   │       ├── viz/             # HTML graph visualization
│   │       └── export/          # Cypher, GraphML, JSON export
│   ├── infigraph-docs/          # Document indexing (PDF, DOCX, PPTX, HTML, Markdown)
│   ├── infigraph-confluence/    # Confluence wiki crawler with incremental sync
│   ├── infigraph-languages/     # 59 tree-sitter language packs
│   │   └── languages/           # entities.scm + relations.scm per language
│   ├── infigraph-grammar-plugin/   # Runtime ANTLR grammar plugin system (JVM subprocess)
│   │   └── src/                 # Driver, config-driven extractor, plugin discovery
│   ├── infigraph-cli/           # 50+ CLI commands
│   ├── infigraph-mcp/           # 69-tool MCP server + web UI
│   └── lsp-to-scip/             # Generic LSP → SCIP bridge binary
├── driver/                          # Java ANTLR grammar driver (JVM subprocess)
│   ├── infigraph-driver.jar       # Fat jar (ANTLR4 runtime bundled)
│   └── src/                       # GrammarDriver.java
├── grammars/                        # Grammar plugin definitions (user-provided)
├── models/
│   └── potion-base-8M/          # Bundled Model2Vec embeddings (256-dim, ~30MB)
├── release.sh                   # Local release builder (macOS ARM64 → GHE)
├── install.sh                   # One-line installer (pre-built binary or source)
└── tests/
    └── fixtures/microservices/  # Test fixtures: Python/TS/Rust microservices
```

## Graph Schema

### Nodes
- **Symbol** — functions, methods, classes, structs, enums, variables, constants, tests, sections, routes (with parameters, return_type, docstring, complexity)
- **Module** — file-level grouping
- **Cluster** — Louvain-detected community
- **File** — source file
- **Folder** — directory
- **Dependency** — external package dependency (name, version, ecosystem, is_dev)

### Edges
- **CALLS** — function/method call (cross-file resolved + SCIP-enriched)
- **INHERITS** — class/interface inheritance
- **IMPLEMENTS** — interface implementation (from SCIP)
- **CONTAINS** — module contains symbol
- **TESTED_BY** — test function tests symbol
- **IMPORTS** — module imports module
- **READS** / **WRITES** — variable access
- **MEMBER_OF** — symbol belongs to cluster
- **SIMILAR_TO** — semantic similarity (embedding-based)
- **BRIDGE_TO** — cross-language bridge (FFI, JNI, gRPC, COM, WASM)
- **CALLS_SERVICE** — cross-service HTTP call (with method, path, target_service)
- **DEPENDS_ON** — module depends on external package
- **DEFINES** — file defines symbol
- **CONTAINS_FILE** — folder contains file
- **CONTAINS_FOLDER** — folder contains subfolder
- **Custom edges** — extensible via language plugins (DECORATED_BY, SPAWNS, etc.)

## Tech Stack

- **Rust** — core engine
- **LadybugDB** (`lbug`) — embedded columnar graph database (Kuzu successor)
- **tree-sitter** 0.26 — AST parsing for 59 languages
- **Model2Vec** — neural embeddings (potion-base-8M, 256-dim, bundled — no proxy/network needed)
- **SCIP** — Sourcegraph Code Intelligence Protocol for compiler-grade enrichment
- **vis.js** — graph visualization
- **tiny_http** — embedded web server

## Contributing: Route & Decorator Extraction

Infigraph captures HTTP route decorators/attributes and stores them in symbol docstrings for semantic search. When a user searches "GET /api/users", the route-decorated function scores highest.

### How it works

Two mechanisms capture decorators:

1. **Tree-sitter query capture** (`entities.scm`) — for languages where decorators are syntactic wrappers (Python `decorated_definition`, Java annotation modifiers)
2. **AST sibling scan** (`extract/entities.rs`) — for languages where attributes are preceding sibling nodes (Rust `attribute_item`, C# `attribute_list`, Kotlin `annotation`, PHP `attribute_list`)

### HTTP Route Coverage

| Language | Framework examples | Status |
|----------|--------------------|--------|
| Python | Flask `@app.route`, FastAPI `@get`, Django `@api_view` | Working |
| Java | Spring `@GetMapping`, `@RequestMapping`, `@RestController` | Working |
| Rust | Actix `#[get]`, Axum, Rocket `#[get]` | Working |
| C# | ASP.NET `[HttpGet]`, `[Route]`, `[ApiController]` | Working |
| Kotlin | Spring `@GetMapping`, Ktor | Working |
| PHP | Laravel `#[Route]` (PHP 8+) | Working |
| TypeScript | NestJS `@Get()`, `@Controller()` | Working |
| JavaScript | Express `router.get("/path", ...)` | Working |
| Go | `r.GET("/path", handler)` (Gin, Chi, gorilla) | Working |
| Ruby | Rails `get "/path" do...end` | Working |
| Elixir | Phoenix `get "/path" do...end` | Working |
| Swift | Vapor `router.get("/path")` | Working |
| Dart | shelf `get("/path", handler)` | Working |
| Haskell | Servant `type API = "path" :> Get` | Working |
| Lua | Lapis `app:get("/path", ...)` | Working |
| Perl | Mojolicious `get "/path" => sub {...}` | Working |
| Clojure | Compojure `GET "/path"` | Working |
| Erlang | Cowboy `dispatch_rules` / `handle` | Working |
| GraphQL | Schema definitions | Working |
| Scala | Play `GET("path")` | Working |
| Django | `path("/url", view)` | Working |
| F# | Giraffe/Saturn (AST scan) | Deferred |

### Adding route support for a new language

#### Option A: Query-based (decorator-wrapper syntax)

Edit `crates/infigraph-languages/languages/<lang>/entities.scm`:

```scheme
; Capture route method, path, handler
(call_expression
  function: (identifier) @route.method
  arguments: (arguments
    (string_literal) @route.path)
  (#match? @route.method "^(get|post|put|delete|patch)$")) @route.def
```

#### Option B: AST sibling (attribute-preceded syntax)

Add the tree-sitter node kind to `ATTR_KINDS` in `crates/infigraph-core/src/extract/entities.rs`:

```rust
const ATTR_KINDS: &[&str] = &[
    "attribute_item",    // Rust
    "attribute_list",    // C#, PHP 8
    "annotation",        // Kotlin, Scala
    "decorator",         // TypeScript, JavaScript
    "marker_annotation", // Java
    // Add your language's attribute node kind here
];
```

### Test fixtures

`tests/fixtures/microservices/` contains realistic microservice repos for testing:
- `user-service/` — Python Flask (3 files, 6 routes)
- `order-service/` — TypeScript Express (4 files, 5 routes)
- `payment-service/` — Rust Actix-web (5 files, 4 routes)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for build instructions, code style, and how to add a language or submit a PR.

## License

Apache-2.0
