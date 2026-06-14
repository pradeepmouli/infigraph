---
layout: default
title: Infigraph — AST-powered Local-first Code Intelligence
---

<style>
  .hero-section {
    text-align: center;
    margin-bottom: 3rem;
    padding: 2rem 0;
  }
  .hero-image {
    max-width: 100%;
    height: auto;
    margin: 2rem 0;
  }
  .cta-button {
    display: inline-block;
    background-color: #0693e3;
    color: white;
    padding: 12px 30px;
    margin: 1rem 0.5rem;
    border-radius: 5px;
    text-decoration: none;
    font-weight: bold;
    transition: background-color 0.3s;
  }
  .cta-button:hover {
    background-color: #005a87;
    text-decoration: none;
    color: white;
  }
  .highlights-grid {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 2rem;
    margin: 2rem 0;
  }
  .highlight-item h3 {
    margin-top: 0;
    color: #0693e3;
  }
  @media (max-width: 600px) {
    .highlights-grid {
      grid-template-columns: 1fr;
    }
  }
</style>

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

<a href="/infigraph/getting-started" class="cta-button">→ Get Started (2 minutes)</a>

---

## Key Capabilities

<div class="highlights-grid">
  <div class="highlight-item">
    <h3>🌐 62 Languages</h3>
    <p>Tree-sitter + ANTLR grammar plugins. Zero config.</p>
  </div>
  <div class="highlight-item">
    <h3>🔍 Hybrid Search</h3>
    <p>BM25 + Model2Vec. Find "auth logic" even if function isn't named auth.</p>
  </div>
  <div class="highlight-item">
    <h3>🛢️ Graph Database</h3>
    <p>Full Cypher queries. WITH, OPTIONAL MATCH, variable-length paths.</p>
  </div>
  <div class="highlight-item">
    <h3>⚡ Call Resolution</h3>
    <p>Import-aware cross-file linking. Knows what actually calls what.</p>
  </div>
  <div class="highlight-item">
    <h3>🚀 69 MCP Tools</h3>
    <p>Claude Code, Cursor, VS Code, Copilot, Windsurf. All supported.</p>
  </div>
  <div class="highlight-item">
    <h3>🔒 Offline First</h3>
    <p>Everything runs locally. No APIs. No network. No cloud.</p>
  </div>
</div>

---

## Learn More

<a href="/infigraph/getting-started" class="cta-button">Getting Started Guide</a>
<a href="/infigraph/architecture" class="cta-button">Architecture & Design</a>
<a href="/infigraph/contributing" class="cta-button">Contributing</a>

---

**[View on GitHub](https://github.com/intuit/infigraph)** • **[License: Apache 2.0](https://github.com/intuit/infigraph/blob/main/LICENSE)**

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
