# LM2 — Large Memory Model

Session-aware memory system for AI coding agents. Extends Infigraph's session continuity with confidence decay, tiered retrieval, auto-injection, and memory consolidation.

---

## The 5 Phases

### Phase 1: Output Gate (`memory_context`)

Single tool call that gathers code symbols, session context, and skeleton in one response — replaces 3-4 manual tool calls.

**How it works:**
- `gather_code` — hybrid search (BM25 + vector) over indexed symbols
- `gather_sessions` — searches session embeddings, weights by confidence (score × 0.7 × confidence)
- `gather_skeleton` — file-level structure (function signatures, class outlines)
- Anchor boost: if caller provides an anchor file, symbols in that file get +0.15 score boost (capped at 1.0)
- Constraints/blockers always included regardless of score

**Source:** `crates/infigraph-mcp/src/tools/memory_context.rs`

---

### Phase 2: Tiered Retrieval (L1 / L2 / L3)

Three depth levels control how much context is gathered:

| Level | Scope | When used |
|-------|-------|-----------|
| **L1** | Anchor file symbols + recent edits + active session | Requires anchor file; auto-escalates to L2 if < 3 results |
| **L2** | Related files via call graph + session matches | Default when no anchor provided; auto-escalates to L3 if < 5 results |
| **L3** | Full semantic archive search | Fallback — current `search` behavior |

**Auto-depth selection** (when depth not specified):
- Anchor file provided → L1
- No anchor, no auto flag → L2
- Explicit `"depth": "L3"` → L3

**Auto-escalation:**
- L1 with < 3 results → L2
- L2 with < 5 results → L3

**Source:** `Depth` enum and `gather_code` in `memory_context.rs`

---

### Phase 3: Confidence Decay

Sessions lose confidence over time unless accessed.

| Constant | Value | Location |
|----------|-------|----------|
| `INITIAL_CONFIDENCE` | 0.7 | `session_store.rs:35` |
| `DECAY_PER_WEEK` | 0.05 | `session_store.rs:33` |
| `ARCHIVE_THRESHOLD` | 0.3 | `session_store.rs:34` |

**Formula:** `confidence = initial - (weeks_since_created × 0.05)`

**Timeline (default 0.7 initial):**
- Week 0: 0.70
- Week 1: 0.65
- Week 4: 0.50
- Week 8: 0.30 (archived)

**Touch-on-access:** Accessing a session via `get_latest_session` or auto-injection resets the decay clock (`touch_session`).

**Selective initial scoring** (`score_session_value` in `session.rs`):
| Content type | Initial confidence |
|-------------|-------------------|
| Decisions with invalidation conditions, constraints, security/compliance | 0.9 |
| Constraints or blockers present | 0.85 |
| Decisions or assumptions only | 0.7 |
| Summary-only | 0.5 |

**Source:** `crates/infigraph-core/src/graph/session_store.rs`, `crates/infigraph-mcp/src/tools/session.rs`

---

### Phase 4: Auto-Injection

Relevant session context is automatically appended to `symbol_context` and `get_doc_context` output.

**Thresholds:**
- Relevance: cosine similarity ≥ 0.7
- Confidence: ≥ 0.5 (after decay)
- Budget: 20% of main output length

**What gets injected:**
1. Decisions (if present)
2. Constraints (if present)
3. Summary (only if no decisions/constraints)

**Format:**
```
**Prior context:**
  [session_id] (confidence: 0.85): Decisions: Goal: X. Decision: Y...
```

**Source:** `auto_inject_session_context` in `crates/infigraph-mcp/src/tools/graph.rs`

---

### Phase 5: Memory Consolidation

Merges related sessions to reduce redundancy and boost signal.

**Algorithm:**
1. Load all session embeddings from `.infigraph/sessions/embeddings.bin`
2. Compute pairwise cosine similarity
3. Union-find clustering: merge sessions with similarity ≥ threshold (default: 0.7)
4. For each cluster ≥ 2 sessions:
   - Merge summaries, decisions, constraints, assumptions, blockers, files touched
   - New consolidated session ID: `consolidated_{earliest_created_timestamp}`
   - Consolidated confidence: 0.9
   - Source sessions preserved with halved confidence
   - Re-embed consolidated session

**Auto-trigger:** When session count exceeds 50, consolidation runs automatically after `save_session`.

**Source:** `tool_consolidate_memory` in `crates/infigraph-mcp/src/tools/session.rs`

---

## MCP Tools

| Tool | Description |
|------|-------------|
| `memory_context` | Intelligent context gathering (code + sessions + skeleton) with auto-depth L1/L2/L3 |
| `consolidate_memory` | Merge related sessions, boost confidence, reduce redundancy |
| `save_session` | Persist session context with TOUCHED edges + semantic embedding. Optional `name` param for named identity sessions |
| `get_latest_session` | Retrieve most recent session — summary, pending tasks, decisions, linked files. Optional `name` param |
| `search_sessions` | Semantic search across past sessions by meaning |
| `purge_sessions` | Delete sessions older than N days (default: 30) |

## CLI Commands

| Command | Alias | Description |
|---------|-------|-------------|
| `memory-context` | `mc` | Same as MCP `memory_context` |
| `consolidate-memory` | `consolidate` | Same as MCP `consolidate_memory` |
| `purge-sessions` | — | Same as MCP `purge_sessions` |

## Storage

| Path | Contents |
|------|----------|
| `.infigraph/sessions/db/` | KuzuDB instance for session nodes + TOUCHED edges |
| `.infigraph/sessions/embeddings.bin` | Session embedding vectors for semantic search |
| `.infigraph/sessions/session_YYYY-MM-DD.md` | Narrative logs (human-readable, appended per save) |
| `.infigraph/sessions/named_*.json` | Named identity sessions |
