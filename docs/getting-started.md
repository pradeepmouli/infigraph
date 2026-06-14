---
layout: default
title: Getting Started
---

# Getting Started with Infigraph

## Installation

### macOS / Linux

```bash
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
```

This will:
- Download pre-built binaries from GitHub releases (if available)
- Fall back to cloning + `cargo build --release` (installs Rust if needed)
- Add `infigraph`, `infigraph-mcp`, and `lsp-to-scip` to `~/.local/bin`
- Register MCP server for all 11 AI coding agents
- Write primary search instructions to `~/.claude/CLAUDE.md`

**System dependency:** `cmake` is required to build the graph database.
```bash
# macOS
brew install cmake

# Linux (Ubuntu/Debian)
sudo apt install cmake
```

### Windows

Run this single command from **PowerShell**:

```powershell
iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
```

This downloads and runs the full installer — which fetches the pre-built binary and registers the MCP server.

## Usage with AI Coding Agents

Infigraph is designed to be used through AI coding agents (Claude Code, Cursor, Copilot, etc.) rather than directly from the CLI.

### With Claude Code (Recommended)

After installation, just start working. Infigraph indexes automatically on first use:

```
> Ask Claude: "search for authentication logic in this project"
> Ask Claude: "who calls the validate_user function?"
> Ask Claude: "show me the architecture of this codebase"
> Ask Claude: "find dead code"
> Ask Claude: "what's the blast radius if I change this function?"
```

Claude Code auto-indexes the project on first use. No manual `infigraph index` needed. A file watcher starts automatically after indexing to keep the graph in sync with code changes.

### With Other AI Agents

Any agent with MCP support (Cursor, VS Code + Copilot, Windsurf, etc.) can use Infigraph tools after `infigraph install`. The agent calls `index_project` automatically when needed.

### Manual CLI (Optional)

```bash
cd /path/to/project
infigraph index              # Index the project
infigraph search "auth"      # Hybrid search
infigraph query "MATCH ..."  # Cypher query
infigraph routes             # HTTP endpoints
infigraph dead-code          # Unused functions
```

## Update

Re-run the installer to pull latest and rebuild:

```bash
# macOS / Linux
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash

# Or if building manually:
cd /path/to/infigraph && git pull && cargo build --release
infigraph update
```

`infigraph update` re-registers MCP server paths and refreshes CLAUDE.md instructions.

Infigraph also checks for updates in the background (once per 24h) and prints a hint when a newer version is available.

## Uninstall

```bash
infigraph uninstall
```

This removes:
- MCP server config from all 11 AI agents
- Primary search instructions from `~/.claude/CLAUDE.md`

It does NOT delete the binary — remove `~/.local/bin/infigraph` and `~/.local/bin/infigraph-mcp` manually if desired.

## Next Steps

- **[Learn the Architecture](/infigraph/architecture)** — How Infigraph works internally
- **[Grammar Plugins](/infigraph/architecture#grammar-plugins)** — Add support for custom languages
- **[Contributing](/infigraph/contributing)** — Help improve Infigraph

---

Questions? [Open an issue](https://github.com/intuit/infigraph/issues) or check the [documentation](https://github.com/intuit/infigraph).
