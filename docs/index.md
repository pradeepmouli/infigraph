---
layout: default
title: Home
nav_order: 1
---

# Infigraph

**AST-powered code intelligence engine.** Indexes codebases into a persistent knowledge graph with full Cypher queries, hybrid semantic search, cross-file call resolution, and **62 programming languages**.

Built in Rust. Zero LLM dependency. Runs locally. No API keys. No network calls.

---

## The Problem

AI agents are **structurally blind** to your codebase. When they need to answer "who calls this function?" or "what breaks if I change this class?", they re-read files, retrace imports, and re-infer relationships — wasting time and tokens.

![The Hidden Cost of Code Blindness in the Age of AI](https://learnbyinsight.com/wp-content/uploads/2026/06/hidden-cost-ai-infigraph.png)

**The cost:** 60–80% of AI agent tokens spent on code rediscovery instead of solving your problem.

---

## The Solution

Infigraph builds a **persistent knowledge graph** before the agent runs. Structural questions that cost hundreds of tokens now resolve in milliseconds.

```
Source Code → Index → Knowledge Graph → AI Agent → Instant Answers
```

**Result:** 10–100x fewer tokens. 1ms instead of 5s file reads. Complete call graphs in milliseconds.

---

## Why Infigraph (Unique in the Market)

**No other tool combines all of this:**
- ✅ **Local-first** — Everything runs offline, no APIs
- ✅ **Persistent knowledge graph** — Query once, reuse forever
- ✅ **62 languages** — Tree-sitter + grammar plugins
- ✅ **AI-native** — Built for MCP agents (Claude Code, Cursor, etc.)
- ✅ **No LLM dependency** — Pure code analysis

Cloud tools (GitHub Copilot, Sourcegraph) require sending code to external APIs. Local tools (ctags, LSP, CodeQL) don't persist a knowledge graph. **Infigraph is the first AI-native, local-first knowledge graph for code.**

[Read the full comparison →](/infigraph#why-infigraph-what-makes-it-unique)

---

## Quick Start

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/intuit/infigraph/main/install.sh | bash
cd /path/to/project && infigraph index
```

**Windows:**
```powershell
iwr https://raw.githubusercontent.com/intuit/infigraph/main/install.ps1 -UseBasicParsing | iex
```

Then ask your AI agent:
```
"Who calls the validate_user function?"
"Show me the blast radius of this change"
"Find authentication logic in this codebase"
```

[Get Started →]({% link getting-started.md %}){: .btn .btn-primary }

---

## Key Capabilities

**🌐 62 Languages**  
Tree-sitter + ANTLR grammar plugins. Zero config.

**🔍 Hybrid Search**  
BM25 + Model2Vec. Find "auth logic" even if function isn't named auth.

**🛢️ Graph Database**  
Full Cypher queries. WITH, OPTIONAL MATCH, variable-length paths.

**⚡ Call Resolution**  
Import-aware cross-file linking. Knows what actually calls what.

**🚀 69 MCP Tools**  
Claude Code, Cursor, VS Code, Copilot, Windsurf. All supported.

**🔒 Offline First**  
Everything runs locally. No APIs. No network. No cloud.

---

## Next Steps

[Getting Started Guide →]({% link getting-started.md %}){: .btn .btn-primary }
[Architecture & Design →]({% link architecture.md %}){: .btn }
[Contributing →]({% link contributing.md %}){: .btn }

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
