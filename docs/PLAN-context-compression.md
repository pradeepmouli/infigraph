# Infigraph Context Engine — Implementation Plan

Unified context compression layer combining Headroom-style generic compression with Infigraph's semantic graph awareness. Works for ALL content, gets smarter when graph data is available.

**Goal:** 70-90% token reduction with 95%+ answer quality preservation. Zero quality loss on lossless compression types.

---

## Table of Contents

1. [Architecture](#architecture)
2. [Competitive Analysis: Headroom vs Infigraph Context Engine](#competitive-analysis)
3. [Phase 0: Baseline Measurement](#phase-0-baseline-measurement)
4. [Phase 1: Cache-Aligned Live-Zone Compression](#phase-1-cache-aligned-live-zone-compression)
5. [Phase 2: Tool Output Shaping](#phase-2-tool-output-shaping)
6. [Phase 3: Session Context Tracking](#phase-3-session-context-tracking)
7. [Phase 4: Generic Content Compressors + ML Text Compression](#phase-4-generic-content-compressors--ml-text-compression)
8. [Phase 5: Cross-Agent Context Sharing](#phase-5-cross-agent-context-sharing)
9. [Phase 6: Budget-Aware Scaling](#phase-6-budget-aware-scaling)
10. [Phase 7: Compress MCP Tool](#phase-7-compress-mcp-tool)
11. [Phase 8: A/B Testing and Production Rollout](#phase-8-ab-testing-and-production-rollout)
12. [Test Strategy](#test-strategy)
13. [Quality Benchmarks](#quality-benchmarks)
14. [Success Criteria](#success-criteria)
15. [Risk Register](#risk-register)

---

## Architecture

### 6-Layer Compression Stack

```
Layer 0: CACHE PROTECTION (CacheAligner)
  │  Identify frozen prefix (system prompt, tool defs, old turns)
  │  NEVER mutate frozen prefix — only compress live zone
  │  Live zone = latest tool output + latest user message
  │
Layer 1: CONTENT CLASSIFICATION (ContentRouter)
  │  Detect content type → route to optimal compressor
  │
Layer 2: TYPE-SPECIFIC COMPRESSION
  │  ├── Code      → Graph-aware (signatures + edges, not full source)
  │  ├── JSON      → Statistical sampling (Kneedle for representative subset)
  │  ├── Logs      → Pattern dedup + error preservation
  │  ├── Build     → Collapse compile lines, keep errors/warnings
  │  ├── Stack     → Keep app frames, collapse framework frames
  │  ├── Prose     → ML extractive summary (Kompress or local model)
  │  ├── File tree → Count collapse (src/ (47 files))
  │  └── Table     → Header + row count + samples
  │
Layer 3: SESSION DEDUP
  │  Track seen files/symbols per turn
  │  Same content hash → "(seen: auth.rs::login, turn 3)"
  │  Changed content → show full (content hash differs)
  │  Stale (>10 turns) → show full (may have scrolled out)
  │
Layer 4: BUDGET-AWARE SCALING
  │  >70% budget remaining → minimal compression
  │  50-70% → standard compression
  │  20-50% → aggressive (shorter summaries)
  │  <20% → minimal (one-liners only)
  │
Layer 5: CROSS-AGENT SHARING
     Subagent spawns get compressed context, not full replay
     Shared compressed store with agent provenance
     Auto-dedup across agents working on same files
```

### Data flow detail

```
Tool call (e.g. search "auth login")
    │
    ▼
Raw output (2000 tokens)
    │
    ▼
┌──────────────────────┐
│ L0: Cache Protection  │ ← Is this in the frozen prefix? → SKIP
│     (Live-zone gate)  │    Only compress new content
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ L1: Classify          │ ← "search results with code snippets"
│     → Code type       │
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ L2: Graph-aware       │ ← 20 results → 20 one-liners with
│     compression       │    symbol kind, line range, edge counts
│                       │    (500 tokens, ZERO info loss on existence)
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ L3: Session dedup     │ ← 5 of 20 already seen → mark "(seen)"
│                       │    (400 tokens)
└──────────┬───────────┘
           ▼
┌──────────────────────┐
│ L4: Budget check      │ ← 60% budget left → standard level OK
│                       │    (400 tokens, no further compression)
└──────────┬───────────┘
           ▼
Compressed output (400 tokens = 80% reduction)
```

### Core principles

1. **Cache-first** — never break the LLM provider's KV cache prefix
2. **Never compress what's being edited** — edit targets always get full source
3. **Never reduce result counts** — show ALL results, just shorter format per result
4. **Lossless where possible** — dedup, skip-seen, schema extraction are lossless
5. **Progressive disclosure** — summary first, detail on `detail=true` request
6. **Budget-aware** — compress harder as context fills up
7. **Zero retrieval tax** — no decompress round-trip, just ask with `detail=true`
8. **Cross-agent efficient** — subagents get compressed context, not full replay
9. **Measurable** — every compression logged for quality monitoring

### Where it lives

Built into `crates/infigraph-mcp/` as response middleware in `dispatch_tool`. Not a proxy. For non-Infigraph content, exposed as a `compress` MCP tool.

---

## Competitive Analysis

### Headroom vs Infigraph Context Engine

| Feature | Headroom | Infigraph CE | Winner |
|---------|----------|-------------|--------|
| **Cache alignment** | ✅ CacheAligner, live-zone only | ✅ L0 Cache Protection | Tie |
| **Content routing** | ✅ ContentRouter | ✅ L1 Classifier | Tie |
| **JSON compression** | ✅ SmartCrusher (Kneedle statistical sampling) | ✅ Statistical sampling (adopted) | Tie |
| **Code compression** | ✅ Tree-sitter AST body collapse | ✅ Graph-aware (knows callers/callees/edges) | **Infigraph** |
| **Log compression** | ✅ LogCompressor (93% reduction) | ✅ Pattern dedup + error preservation | Tie |
| **ML text compression** | ✅ Kompress-v2-base (HF model) | ✅ Extractive summary (local model or Kompress) | Tie |
| **Image compression** | ✅ ML router | ❌ Not planned (low priority for code tools) | Headroom |
| **Reversible retrieval** | ✅ headroom_retrieve + BM25 search | ✅ detail=true parameter (no extra tool call) | **Infigraph** |
| **Session dedup** | ❌ No cross-turn memory | ✅ L3 seen-tracking with content hashes | **Infigraph** |
| **Graph-aware ranking** | ❌ No code graph | ✅ Cluster results, relevance by graph distance | **Infigraph** |
| **Budget-aware scaling** | ❌ Static compression | ✅ L4 adaptive by remaining tokens | **Infigraph** |
| **Edit protection** | ❌ Doesn't know what you're editing | ✅ Never compress edit targets | **Infigraph** |
| **Cross-agent sharing** | ✅ SharedContext + provenance | ✅ L5 compressed context passing | Tie |
| **Stack trace compression** | ❌ Not specialized | ✅ App/framework frame separation | **Infigraph** |
| **Build output compression** | ⚠️ Part of LogCompressor | ✅ Dedicated, compile-line collapse | **Infigraph** |
| **File tree compression** | ❌ Not mentioned | ✅ Count collapse | **Infigraph** |
| **Domain-specific compressors** | 6 types | 8 types | **Infigraph** |
| **Deployment** | Library, proxy, MCP, wrapper | Native MCP middleware (zero overhead) | **Infigraph** |

**Summary:** Headroom has 3 things we adopted (CacheAligner, statistical JSON sampling, ML text compression). We have 7 things Headroom doesn't (graph-awareness, session dedup, budget scaling, edit protection, stack traces, build output, file trees). Our retrieval mechanism (detail=true) is simpler and cheaper than their headroom_retrieve tool call.

---

## Phase 0: Baseline Measurement ✅

**Goal:** Establish current token usage patterns before any optimization.

### Task 0.1: Instrument current tool outputs
- [x] Add token counting to `handle_tools_call` in `crates/infigraph-mcp/src/main.rs` (not dispatch_tool — tests call that directly)
- [x] Used word-count heuristic (words * 1.4) instead of tiktoken-rs — no new dependency, sub-microsecond
- [x] Log to `.infigraph/compression_metrics.jsonl`: `{tool, timestamp, raw_tokens, compressed_tokens, compression_ratio, detail_requested, args_summary}`
- [x] Gated behind `INFIGRAPH_METRICS=1` env var

### Task 0.2: Collect baseline data
- [x] Run 20 representative tasks across 5 categories (see Quality Benchmarks below)
- [x] Record per-task: total tokens consumed, tokens per tool call, number of tool calls
- [x] Identified top-2: get_doc_context (49%) and search (38%) = 87% of all output tokens

### Task 0.3: Build eval harness
- [x] Create `tests/compression_eval/` directory
- [x] Define 20 eval tasks as structured test cases:
  ```rust
  struct EvalTask {
      id: String,
      category: Category,  // Search, Edit, Refactor, Understand, Debug
      description: String,
      setup: Vec<ToolCall>,    // tool calls to set up context
      query: ToolCall,          // the tool call being evaluated
      expected_answer: String,  // ground truth
      quality_check: QualityCheck, // how to verify answer quality
  }
  ```
- [ ] Categories (4 tasks each):
  - **Search** (5): find symbol, find usage pattern, find config, find test, find doc
  - **Edit** (5): fix bug, add parameter, change return type, add error handling, rename
  - **Refactor** (5): extract function, rename across files, move to module, change signature, remove dead code
  - **Understand** (5): explain flow, trace data path, identify dependencies, architecture overview, find blast radius

### Task 0.4: Run baseline eval
- [x] Execute all 20 tasks with current (uncompressed) tool outputs
- [x] Record: tokens_used, answer_correct (binary), answer_quality (1-5 human rating)
- [x] Store results in `tests/compression_eval/baseline_results.json`
- [x] This becomes the control group for all subsequent phases

### Deliverable ✅
- `compression_metrics.jsonl` logging in place
- 20 eval tasks defined and baselined
- Top-2 token-heavy tools identified: get_doc_context (49%), search (38%)

---

## Phase 1: Cache-Aligned Live-Zone Compression ✅

**Goal:** Ensure compression never breaks the LLM provider's KV cache prefix. This is the foundation — all subsequent phases build on top of this.

**Outcome:** Verified by construction — no new code needed. MCP server only touches live zone (fresh tool results in `handle_tools_call`). Tool defs use static `vec![]` with `BTreeMap`-backed `serde_json` (deterministic key order). Two guard tests added in `tool_parity.rs`.

### Task 1.1: Define zone boundaries
- [x] Verified: MCP tool outputs are returned from `handle_tools_call` as new content — always live zone
- [x] `compress_tool_output` runs only in `handle_tools_call` on fresh results, never touches frozen prefix

### Task 1.2: Implement live-zone gate
- [x] Not needed as separate module — compression only runs in `handle_tools_call` by construction

### Task 1.3: Stable tool definitions
- [x] `build_tools_list()` is a static `vec![]`, `serde_json::Map` uses `BTreeMap` (deterministic)
- [x] `detail` parameter added statically at startup, not mid-conversation

### Task 1.4: Verify cache preservation
- [x] Added `tool_definitions_are_byte_stable` test — asserts identical bytes across calls
- [x] Added `tool_schema_token_budget` test — ~4k tokens, under 10k cap

### Task 1.5: System prompt and tool definition optimization
- [x] Descriptions already terse from prior work. ~4k tokens total.

### Deliverable ✅
- Live-zone safety verified by construction
- Tool definition byte-stability test passing
- Commit: `6ce9c1d`

---

## Phase 2: Tool Output Shaping ✅

**Goal:** Add summary/detail modes to the top-5 token-heavy tools. Target: 50-70% reduction on these tools. Never reduce result counts — only reduce verbosity per result.

**Outcome:** 4 compressors implemented (search 55.7%, get_doc_context 88.3%, find_all_references 39%, get_architecture 54.3%). Combined 72.5% savings exceeds 40% gate. trace_callers/callees bypassed (5-7 tokens). Compression in `compress.rs`, wired into `handle_tools_call` in `main.rs`. Commit: `d62cd17`.

### Task 2.1: Design summary format per tool ✅
- [x] Summary/detail modes designed and implemented for search, get_doc_context, find_all_references, get_architecture
- [x] trace_callers/callees bypassed (output already 5-7 tokens)

### Task 2.2: Implement compression middleware ✅
- [x] Created `compress.rs` with `CompressionLevel` enum (Off, Summary, Aggressive, Minimal)
- [x] `compress_tool_output()` wired into `handle_tools_call` in `main.rs`
- [x] Per-tool level caps via `effective_level()` — search capped at Summary (Phase 6.4 finding)

### Task 2.3: Implement per-tool compressors ✅
- [x] `compress_search`, `compress_doc_context`, `compress_references`, `compress_architecture`

### Task 2.4: Add `detail` parameter ✅
- [x] `detail: bool` added to tool definitions in `build_tools_list`
- [x] `for_edit=true` bypasses compression

### Task 2.5: Implement metrics logging ✅
- [x] Metrics logged to `.infigraph/compression_metrics.jsonl` (gated INFIGRAPH_METRICS=1)
- [x] `get_compression_stats` MCP tool (implemented in Phase 8.2, commit `abb34ca`)

### Task 2.6: Phase 2 eval ✅
- [x] Covered by Phase 6.4 eval (4-level sweep, 20 tasks, must_contain assertions)

### Task 2.7: Compression bypass rules ✅
- [x] `should_bypass()` in compress.rs: security tools, small outputs (<100 tokens), errors, detail=true, for_edit=true

### Task 2.8: Smart detail prefetch ⏭️ DEFERRED
- Not needed — detail=true is cheap and explicit

### Task 2.9: Compression failure fallback ✅
- Compressors already fall through to raw on parse failure (pattern match returns raw.to_string())
- `catch_unwind` around compress+dedup in `handle_tools_call`; panic → log, `record_compress_failure()`, return raw
- `get_compression_stats` reports `Compress failures` count

### Deliverable ✅
- Core tool compressors: search, get_doc_context, find_all_references, get_architecture
- Later additions: list_files, detect_dead_code, get_api_surface, git_summary, **search_sessions** (Decisions/Files truncation by level)
- Metrics logging in handle_tools_call (gated INFIGRAPH_METRICS=1)
- Bypass rules: security tools, small outputs, errors, detail=true, for_edit=true, focus set (3.3)
- Per-tool level caps (search capped at Summary) — commit `651ec59`
- Compression failure fallback (2.9): `compress_pipeline_safe` + Compress failures in stats

---

## Phase 3: Session Context Tracking ✅ (core) / partial (deferred items)

**Goal:** Avoid re-sending content the LLM already has in context. Target: additional 20-30% reduction.

**Outcome:** Core seen-dedup (3.1+3.2+3.4) implemented in `session_context.rs`. FNV-1a hashing, 6-call staleness window, dedup on by default (`INFIGRAPH_DEDUP=0` to disable). Focus tracking (3.3) and cross-session dedup (3.7) shipped. Still deferred: 3.6 (graph-aware compaction — undetectable from MCP).

### Task 3.1: Design session context store ✅

- [x] Created `session_context.rs` with `SessionContext`, `SeenEntry`, `CompressionLevel`
- [x] Global `SESSION: Mutex<Option<SessionContext>>`
- [x] FNV-1a content hashing, token budget tracking

### Task 3.2: Implement seen-detection

- [x] On every tool response, record content hash via FNV-1a
- [x] On subsequent calls, check if content was seen:
  - Same content hash → `"(seen in turn 3: auth.rs::login)"`
  - Different hash → show full (content changed)
  - Seen > 10 turns ago → show full (may have scrolled out of context window)
- [x] Configurable staleness threshold (set to 6 calls, not 10 — tight window bounds damage from stale dedup)

### Task 3.3: Implement focus tracking ✅

- [x] Track files/symbols from `get_doc_context` with `for_edit=true` and `get_code_snippet`
- [x] Never compress content in the focus set (`should_bypass` → `is_in_focus`)
- [x] Focus ages out after staleness window (default 6 calls via `focus_clock`)
- [x] File path from `symbol_id` (`path::name`) is also tracked so sibling symbols in the same file stay uncompressed
- [x] Focused args also skip `(seen …)` dedup placeholders in `apply_seen_dedup`
- [x] Free-text `query` is not a focus candidate (avoids false-positive bypass on `search`)

### Task 3.4: Wire into compression middleware

- [x] `apply_seen_dedup` called after `compress_tool_output` in `handle_tools_call` (via `compress_pipeline_safe`)
- [x] Dedup runs on already-compressed output
- [x] Dedup on by default; `INFIGRAPH_DEDUP=0` disables

### Task 3.5: Run Phase 3 eval ✅
- [x] Integration test `phase3_dedup_eval` in compression_eval.rs
- [x] Tests 4 tools with duplicate calls: search, get_doc_context, find_all_references, get_architecture
- [x] Results: 42.6% additional savings on repeat calls, 67% dedup rate on eligible outputs (>50 tokens)
- [x] D2 (get_doc_context 49 tokens) exempt — below 50-token dedup threshold
- [x] D4 (get_architecture) not deduped — content key format issue (no primary arg)
- [x] Quality preserved: dedup only on identical content (hash-verified)
- [x] **Gate: PASSED** — dedup rate ≥ 50%, no quality loss

### Task 3.6: Graph-aware context compaction

- [ ] When Claude Code triggers context compaction (conversation too long), provide a better summary than generic LLM summarization
- [ ] Hook: detect compaction event (conversation history suddenly shorter)
- [ ] Generate graph-aware summary of what was learned:
  ```
  Session context (graph-aware compaction):
    Files modified: auth.rs (login, verify_token), routes/auth.rs (handler)
    Symbols explored: login (12 callers traced), verify_token (5 callers)
    Decisions: "use JWT not session cookies" (turn 5)
    Pending: update 3 remaining callers of old session API
    Graph state: 47 symbols touched, 12 edges traversed
  ```
- [ ] This replaces ~5000 tokens of conversation replay with ~200 tokens of structured context
- [ ] Store compacted summary in SessionContext for cross-compaction continuity

### Task 3.7: LM2 session integration ✅

- [x] Persist dedup hashes to `.infigraph/dedup_state.json` (FNV-1a content hashes)
- [x] Load prior hashes on SessionContext init for cross-session continuity
- [x] Content-verified dedup (Option B): compare fresh hash against stored — zero false-dedup risk
- [x] Stale hash handling: if content changed, remove prior hash and show full output
- [x] Migrate prior hits to seen map on match (no double-lookup)
- [x] Auto-persist every 5 tool calls via `maybe_persist()`
- [x] 4 unit tests: matching hash, stale hash, migration to seen, persist interval

### Deliverable (partial) ✅
- [x] Session context tracking with seen-dedup (session_context.rs, 8 tests)
- [x] Phase 3 eval: 42.6% additional savings, 67% dedup rate, quality preserved
- [x] LM2 session integration: cross-session dedup via persisted content hashes (3.7, 4 tests)
- [x] Focus-aware compression (3.3 — `record_focus` / `is_in_focus` bypasses compress + dedup)
- [ ] Graph-aware context compaction (deferred: 3.6 — undetectable from MCP server)

---

## Phase 4: Generic Content Compressors + ML Text Compression ✅

**Goal:** Compress non-Infigraph content (bash output, file reads, JSON blobs, prose text). Target: 50-80% reduction on these content types.

**Outcome (Phase 4a):** Added tool-specific compressors: `list_files` (dir tree collapse), `detect_dead_code` (group by file), `get_api_surface` (collapse symbols, keep routes), `git_summary` (truncate symbol lists >5), later `search_sessions` (truncate Decisions pipes; level-aware Files Touched).

**Outcome (Phase 4b):** Content classifier (8 types: Json, JsonArray, LogOutput, StackTrace, BuildOutput, FileTree, Table, PlainText) + 7 generic compressors (JSON schema+sample, log dedup, stack trace framework collapse, build output compile collapse, file tree node collapse, table truncation, PlainText passthrough). `compress` MCP tool wired up for arbitrary text compression. ML prose (Task 4.9/4.10) deferred — extractive summarizer needs no new deps but PlainText is passthrough for now.

### Task 4.1: Content classifier ✅
- [x] `classify_content()` in compress.rs — 8 types: Json, JsonArray, LogOutput, StackTrace, BuildOutput, FileTree, Table, PlainText
- [x] Log/build checked before JSON to avoid false positives on `[INFO]` lines

### Task 4.2: JSON compressor ✅
- [x] Schema inference + count + 2 samples for arrays; top-level structure for objects

### Task 4.3: Log compressor ✅
- [x] Pattern dedup, error/warning preservation, count annotation

### Task 4.4: Stack trace compressor ✅
- [x] App frame preservation, framework frame collapse

### Task 4.5: File tree compressor ✅
- [x] Node collapse with file counts

### Task 4.6: Table compressor ✅
- [x] Header + row count + first/last rows

### Task 4.7: Build output compressor ✅
- [x] Compile line collapse, error/warning preservation

### Task 4.8: Run Phase 4 eval ✅
- [x] Integration test `phase4_generic_compressor_eval` in compression_eval.rs
- [x] Tests 5 content types: JSON array (50 items), logs (100 lines), stack trace, build output (30 crates), prose
- [x] Results: 78.7% overall savings — Log 90.7%, Build 83.6%, Prose 55.6%, Stack 7.1%
- [x] JSON: char-length reduction confirmed (word-count misleading for compact JSON)
- [x] **Gate: PASSED** — >30% overall savings, all compressors functional

### Task 4.9: Extractive summarizer for prose ✅

- [x] Implemented `compress_prose` in `compress.rs` (no separate ml.rs needed)
- [x] Primary: Potion8M embedding-based sentence scoring (cosine similarity to document centroid)
- [x] Fallback: TF-IDF sentence scoring when embedder unavailable
- [x] Position bonus (first 1.5x, last 1.2x) on both scorers
- [x] Keeps top 40% of sentences (min 2), preserves original order
- [x] Caveman-style filler word stripping (articles, hedging, verbose phrases) — ~15% additional reduction
- [x] Kompress/ONNX deferred — Potion + filler stripping is sufficient and zero new deps

### Task 4.10: Prose compressor integration ✅

- [x] Wired into `compress_generic` for `PlainText` content type (was passthrough)
- [x] `compress_prose(text: &str) -> String` with markdown-aware parsing
- [x] Preserves: headings, code blocks, list items, blockquotes, table rows, blank lines
- [x] Skips compression for text < 200 tokens
- [x] 8 tests: small passthrough, heading preservation, code block preservation, paragraph reduction, generic dispatch, sentence splitting, filler word stripping, filler stripping in prose

### Task 4.11: Kompress ML token compression ✅

- [x] Added `ort` (ONNX Runtime) + `ndarray` + `tokenizers` deps to infigraph-mcp
- [x] `kompress` module in `compress.rs`: download-on-first-use from HuggingFace, ONNX Session with Mutex
- [x] Uses `kompress-small` (70M params, ModernBERT + dual head: token classifier + span conv)
- [x] Model (~275MB) downloaded to `~/.infigraph/models/kompress-small/` on first use via curl
- [x] Wired into `compress_prose`: tries kompress first when `ml_compression="kompress"`, falls back to extractive
- [x] Config: `ml_compression` field in `[compression]` section of config.toml, `INFIGRAPH_ML_COMPRESSION` env var
- [x] Long text chunking (350 words per chunk, 20-word overlap) for texts exceeding 8192 tokens
- [x] Subword token reconstruction: Ġ prefix handling for clean output
- [x] Integration test in `compression_eval.rs`: 33.4% savings on prose, key content preserved
- [x] Unit test `test_kompress_direct` verifying model inference and token reconstruction

### Deliverable ✅
- Content classifier + 6 compressors
- [x] Content classifier + 8 generic compressors (JSON, log, stack, build, file tree, table, prose extractive)
- [x] `compress` MCP tool for arbitrary text compression
- [x] 4 more tool compressors: list_files, detect_dead_code, get_api_surface, git_summary
- [x] `search_sessions` compressor (truncate Decisions; level-aware Files Touched)
- [x] Extractive prose compressor (TF-IDF/Potion sentence scoring + filler stripping)
- [x] Kompress ML token compression (opt-in, download-on-first-use)
- [x] Phase 4 eval: 78.7% generic savings, 33.4% kompress prose savings

---

## Phase 5: Cross-Agent Context Sharing ⏭️ SKIPPED

**Goal:** When subagents are spawned (via Agent tool or workflows), pass compressed context instead of full replay. Eliminate redundant work across agents working on the same codebase.

**Why skipped:** Not feasible server-side. MCP protocol carries no agent identifier (only tool name + arguments). Server cannot distinguish which agent is calling, making provenance tracking (5.3) and agent-specific dedup (5.4) impossible. Additionally, we don't control subagent spawning (5.2) — Claude Code does. Process model (shared vs separate MCP instances per agent) is also unclear, making in-memory SharedContext unreliable.

### Task 5.1: SharedContext store

- [ ] Create `crates/infigraph-mcp/src/compress/agents.rs`
- [ ] Implement shared compressed context store:
  ```rust
  struct SharedContext {
      compressed_snapshots: HashMap<String, CompressedSnapshot>,
      agent_provenance: HashMap<AgentId, Vec<String>>,  // which snapshots each agent produced
  }
  
  struct CompressedSnapshot {
      key: String,           // e.g. "arch:project_root" or "file:auth.rs"
      compressed: String,    // compressed content
      content_hash: String,  // detect staleness
      created_by: AgentId,
      created_at_turn: usize,
      token_count: usize,
  }
  ```
- [ ] Store persists across agent spawns within a session
- [ ] Eviction: LRU with max 50 snapshots or 100k compressed tokens

### Task 5.2: Context packaging for subagents

- [ ] When spawning a subagent, package relevant context:
  - Architecture snapshot (compressed `get_architecture` output)
  - File focus set (which files are being edited)
  - Relevant symbol summaries (compressed `get_doc_context` for symbols in scope)
  - Session decisions and constraints
- [ ] Package format: single compressed block < 2000 tokens
- [ ] Subagent gets oriented without repeating the 5-10 tool calls the parent already made

### Task 5.3: Agent provenance tracking

- [ ] Track which agent produced which findings/edits
- [ ] When agent B works on same file as agent A, pass A's compressed findings
- [ ] Prevent duplicate analysis: if agent A already traced callers of `login()`, agent B gets the compressed result
- [ ] Provenance metadata: `{agent_id, task_description, files_touched, findings_summary}`

### Task 5.4: Auto-dedup across agents

- [ ] Before spawning agent, check SharedContext for relevant existing analysis
- [ ] If 80%+ of requested context already exists compressed → inject, don't re-analyze
- [ ] Dedup key: `(file_path, analysis_type, content_hash)` — same file + same analysis + same content = cache hit
- [ ] Log dedup hits and token savings in metrics

### Task 5.5: Run Phase 5 eval

- [ ] Design 5 multi-agent eval tasks:
  - Parallel code review (3 agents reviewing different files)
  - Workflow: find → verify → fix pipeline
  - Architecture exploration then targeted edit
  - Cross-file refactor with blast radius check
  - Bug investigation with trace + fix
- [ ] Measure: tokens per agent (with/without sharing), total session tokens, answer quality
- [ ] **Gate:** sharing must save ≥ 30% tokens in multi-agent scenarios with zero quality loss

### Deliverable
- SharedContext store with agent provenance
- Context packaging for subagent spawns
- Auto-dedup across concurrent agents
- Phase 5 eval results

---

## Phase 6: Budget-Aware Scaling ✅ (incl. 6.4 eval)

**Goal:** Dynamically adjust compression aggressiveness based on remaining token budget.

### Task 6.1: Token budget tracking ✅

- [x] Add `token_budget` + `total_tokens_sent` fields to `SessionContext`
- [x] Track cumulative tokens sent via `track_tokens()` called from `handle_tools_call`
- [x] Estimate remaining budget: `budget - total_sent`
- [x] Default budget: 150k tokens (configurable via `INFIGRAPH_TOKEN_BUDGET` env var)

### Task 6.2: Adaptive compression levels ✅

- [x] Define budget thresholds:
  ```
  > 70% remaining: level = Off (no compression needed)
  50-70% remaining: level = Summary (default compression)
  20-50% remaining: level = Aggressive (shorter summaries, more dedup)
  < 20% remaining: level = Minimal (one-line per result, max dedup)
  ```
- [x] Implement `auto_level()` on `SessionContext`
- [x] Public `get_compression_level()` and `track_tokens()` API

### Task 6.3: Per-level compression rules ✅

- [x] **Off**: pass through raw output (bypass all compressors)
- [x] **Summary**: existing Phase 2/4 compression (all callers/callees, all results)
- [x] **Aggressive**:
  - Search: top-3 results, drop text/doc matches
  - Doc context: top-3 callers/callees
  - Architecture: top-3 languages/hotspots/hubs
  - References: grouped by file (unchanged from Summary)
  - API surface: collapsed per-file (unchanged from Summary)
  - Seen-dedup window: 8 calls (vs default 6)
- [x] **Minimal**:
  - Search: top-1 result only, no text/doc matches
  - Doc context: 0 callers/callees (count only)
  - Architecture: top-2 languages, no hotspots/hubs
  - References: count + file count only
  - API surface: count + file count only
  - Seen-dedup window: 12 calls
- [x] Level logged in compression metrics as `compression_level`

### Task 6.4: Run Phase 6 eval ✅
- [x] Added `INFIGRAPH_COMPRESSION_LEVEL` env override for eval (session_context.rs)
- [x] Rewrote `run_eval.py` for 4-level sweep with must_contain quality assertions
- [x] Fixed tasks.json: symbol_ids, structural must_contain checks
- [x] Results (14 level-sensitive tasks across search, get_doc_context, find_all_references, get_architecture):
  - **Off**: 0% savings, 100% quality
  - **Summary**: 68.7% savings, 100% quality ← safe maximum
  - **Aggressive**: 86.7% savings, quality cliff on search (33-67% retention, top-3 drops important results)
  - **Minimal**: 89.7% savings, search unusable (0-33%), other tools still 100%
- [x] Quality cliff: Aggressive level, search-only (top-3 result limit)
- [x] Current `auto_level()` thresholds validated as safe defaults
- [x] Future optimization: per-tool level overrides (search stays Summary while others go Aggressive)

### Task 6.5: Multi-provider cache model adaptation ⏭️ DEFERRED

- MCP protocol doesn't expose provider metadata, making auto-detection impossible
- Deferred until MCP spec adds client capability negotiation

### Deliverable ✅
- Budget-aware auto-scaling with 4 compression levels
- Per-tool level caps: search capped at Summary, others uncapped (commit `651ec59`)
- `INFIGRAPH_COMPRESSION_LEVEL` env override for testing/eval
- 63 tests passing (session_context + compress level tests + effective_level test)
- Phase 6.4 eval: Summary=68.7% savings/100% quality, quality cliff at Aggressive for search only

---

## Phase 7: Compress MCP Tool ✅

**Goal:** Expose compression as a standalone MCP tool for non-Infigraph content.

### Task 7.1: Implement `compress` MCP tool ✅
- [x] `compress` tool in MCP registry with `text` param + content type auto-detection
- [x] Returns compressed content (routed through content classifier → generic compressors)
- [x] Bypassed from compression itself (in BYPASS_TOOLS list)

### Task 7.2: CLAUDE.md integration ✅
- [x] Added Context Compression section to auto-generated CLAUDE.md template (claude_md.rs)
- [x] Bumped VERSION 1→2 so existing installs get updated
- [x] Documents: auto-scaling, search cap, security bypass, code snippet passthrough

### Task 7.3: Document the tool ✅
- [x] Tool description in `build_tools_list` already present
- [x] Full docs in `docs/CONTEXT-COMPRESSION.md`
- [x] README.md: TOC entry, Key Highlights bullet, Technical Deep Dives link

### Deliverable ✅
- `compress` MCP tool available for any content
- CLAUDE.md integration instructions (auto-generated v2)
- Full documentation in CONTEXT-COMPRESSION.md + README

---

## Phase 8: A/B Testing and Production Rollout ✅

**Outcome:** Config.toml support, get_compression_stats MCP tool, quality monitoring with auto-tuning, dedup enabled by default. Commit: `abb34ca`.

### Task 8.1: Config.toml support ✅

- [x] Added `[compression]` section to `.infigraph/config.toml`:
  ```toml
  [compression]
  enabled = true           # false to disable all compression
  level = "auto"           # off | summary | aggressive | minimal | auto
  dedup = true             # false to disable session dedup
  token_budget = 150000    # total token budget for auto-scaling
  staleness_window = 6     # dedup staleness window (calls)
  ```
- [x] Walk-up search from cwd to find config file
- [x] Priority: env var > config.toml > defaults

### Task 8.2: Metrics dashboard ✅

- [x] `get_compression_stats` MCP tool — shows current session metrics:
  - Compression level (with source: auto/env/config)
  - Token budget, tokens sent, remaining %
  - Tool calls tracked, dedup entries
  - Per-tool detail-request rates with ⚠ flag when >30%

### Task 8.3: Quality monitoring ✅

- [x] `record_tool_call()` tracks total + detail=true requests per tool
- [x] `should_reduce_compression()` returns true when detail-rate >30% (min 5 calls)
- [x] `effective_level()` auto-caps tools to Summary when rate too high
- [ ] Log when LLM asks follow-up questions that suggest information was lost (not feasible from MCP server)
- [ ] Weekly quality audit (manual process, not automated)

### Task 8.4: Production rollout ✅

- [x] Phase A: Compression enabled by default (auto_level scales with budget)
- [x] Phase B: Dedup enabled by default (`INFIGRAPH_DEDUP=0` to disable)
- [x] Phase C: Detail-rate monitoring with auto-cap at >30%
- [x] Phase D: Full production with budget-aware auto-scaling + config.toml

### Deliverable ✅
- Production-ready compression with config.toml, metrics, quality monitoring
- Dedup on by default, compression auto-scales with budget
- `get_compression_stats` MCP tool for observability

---

## Test Strategy

### Unit Tests per Compressor

Each compressor must have dedicated unit tests with explicit input → expected output pairs.

#### JSON compressor tests (`compress/tests/json_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_json_array_basic` | `[{"id":1,"name":"alice"},{"id":2,"name":"bob"},...{"id":100,"name":"zoe"}]` | `JSON array (100 items), schema: {id: int, name: str}\nSample: {"id":1,"name":"alice"}\nSample: {"id":100,"name":"zoe"}` |
| `test_json_object_nested` | `{"config":{"db":{"host":"localhost","port":5432,"pool":{"min":5,"max":20,"timeout":30000}}}}` | Top-level keys preserved, nested values truncated at depth 3 |
| `test_json_empty_array` | `[]` | `JSON array (0 items)` — no compression, pass through |
| `test_json_single_item` | `[{"id":1}]` | Pass through raw — too small to compress |
| `test_json_mixed_types` | Array with heterogeneous objects | Schema shows union of all keys with `?` for optional |
| `test_json_malformed` | `{"broken": tru` | Fallback to raw output (not valid JSON) |
| `test_json_large_values` | Object with 10KB string values | Values truncated to 100 chars with `...(truncated)` |

#### Log compressor tests (`compress/tests/log_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_log_repeated_lines` | 500 identical `[INFO] Processing...` lines | `[INFO] Processing...\n... (498 similar lines)\n[INFO] Processing...` |
| `test_log_errors_preserved` | 100 INFO + 1 ERROR + 100 INFO | All INFO collapsed, ERROR shown in full with 1 line before/after |
| `test_log_warnings_preserved` | Mix of INFO/WARN/ERROR | All WARN and ERROR preserved, INFO collapsed |
| `test_log_no_timestamps` | Plain text that looks like logs but no timestamps | Falls back to PlainText classifier |
| `test_log_mixed_formats` | syslog + JSON-structured logs mixed | Handles both formats, collapses each pattern independently |
| `test_log_empty` | `""` | Pass through (bypass rule: < 100 tokens) |
| `test_log_single_error` | One ERROR line | Pass through (nothing to collapse) |

#### Stack trace compressor tests (`compress/tests/stack_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_stack_rust_backtrace` | Rust panic with 30 frames | App frames kept, std/tokio frames collapsed to `... (N framework frames)` |
| `test_stack_python_traceback` | Python traceback with site-packages | App frames kept, site-packages collapsed |
| `test_stack_java_stacktrace` | Java NPE with spring/hibernate frames | App frames kept, framework collapsed |
| `test_stack_nested_cause` | Error with 3-level cause chain | All cause messages preserved, frames collapsed per cause |
| `test_stack_single_frame` | One-frame error | Pass through |
| `test_stack_no_app_frames` | All framework frames | Keep all (can't determine app frames) with warning |

#### File tree compressor tests (`compress/tests/tree_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_tree_deep_nesting` | 5-level deep tree with 200 files | Leaf dirs collapsed: `models/ (7 files)`, intermediate dirs preserved |
| `test_tree_flat` | 3 files, no dirs | Pass through |
| `test_tree_single_deep_path` | `src/a/b/c/d/file.rs` | Full path preserved (only 1 file) |
| `test_tree_mixed_depth` | Some dirs deep, some shallow | Collapse only leaf dirs with >3 files |

#### Table compressor tests (`compress/tests/table_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_table_markdown` | 50-row markdown table | Header + `(50 rows)` + first 3 rows + last row |
| `test_table_tsv` | Tab-separated 100 rows | Same strategy, column alignment preserved |
| `test_table_small` | 3-row table | Pass through (too small) |
| `test_table_wide` | 20 columns | Keep all columns, compress rows |

#### Build output compressor tests (`compress/tests/build_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_build_cargo_success` | `Compiling` 47 crates, no errors | `Compiling 47 crates...\nBuild succeeded` |
| `test_build_cargo_errors` | 45 Compiling + 2 errors + 1 warning | Compiling collapsed, errors/warnings in full |
| `test_build_npm_install` | 200 `added` lines | `Installed 200 packages` + any warnings |
| `test_build_mixed_output` | Compile + link + test output | Each phase collapsed separately |
| `test_build_all_errors` | Only errors, no success | Pass through all errors |

#### Prose/ML compressor tests (`compress/tests/prose_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_prose_extractive_basic` | 1000-word markdown doc | Top-K sentences by TF-IDF score, headings preserved |
| `test_prose_preserves_code_blocks` | Markdown with code fences | Code blocks passed through untouched, surrounding prose compressed |
| `test_prose_preserves_links` | Text with URLs and references | All URLs preserved in output |
| `test_prose_preserves_lists` | Bulleted/numbered lists | Lists preserved, prose paragraphs compressed |
| `test_prose_short_text` | 50-word paragraph | Pass through (< 200 tokens threshold) |
| `test_prose_headings_only` | Just headings, no body | Pass through |

#### Tool-specific compressor tests (`compress/tests/tool_tests.rs`)
| Test | Input | Expected output |
|------|-------|----------------|
| `test_search_summary_format` | Raw search output with 10 results | One-liner per result with score, kind, line range, edge counts |
| `test_search_detail_passthrough` | Same input with `detail=true` | Raw output unchanged |
| `test_doc_context_summary` | Full get_doc_context output | Signature + edge summary + complexity, no source |
| `test_doc_context_edit_mode` | Same with `for_edit=true` | Full source of target, summary of callers |
| `test_trace_summary` | trace_callers output depth=3 | Tree of names grouped by level, no source |
| `test_architecture_summary` | Full architecture output | Language breakdown + top-5 hotspots only |
| `test_refs_summary` | find_all_references output | File:line list grouped by file, no source |

### Edge Case Test Matrix

```rust
#[cfg(test)]
mod edge_cases {
    // Input size edge cases
    #[test] fn test_empty_input() // → pass through, no crash
    #[test] fn test_single_char() // → pass through
    #[test] fn test_single_line() // → pass through (< 100 tokens)
    #[test] fn test_exactly_100_tokens() // → boundary: bypass threshold
    #[test] fn test_101_tokens() // → compress
    #[test] fn test_50k_tokens() // → compress, verify no OOM or timeout
    #[test] fn test_100k_tokens() // → compress within 50ms budget

    // Content edge cases
    #[test] fn test_unicode_cjk() // → token count differs from word count
    #[test] fn test_unicode_emoji() // → preserve in output
    #[test] fn test_binary_content() // → detect and pass through
    #[test] fn test_mixed_content_json_in_log() // → classify as LogOutput, preserve JSON errors
    #[test] fn test_mixed_content_code_in_markdown() // → classify as Markdown, preserve code blocks
    #[test] fn test_ansi_color_codes() // → strip before classifying, preserve in output if configured
    #[test] fn test_null_bytes() // → handle gracefully, don't panic
    #[test] fn test_very_long_single_line() // → handle (some outputs have no newlines)

    // Compression correctness
    #[test] fn test_all_entity_names_preserved() // → every symbol/file name in raw appears in compressed
    #[test] fn test_all_error_messages_preserved() // → errors never compressed away
    #[test] fn test_line_numbers_preserved() // → file:line references intact
    #[test] fn test_no_hallucinated_content() // → compressed output is subset of raw, never adds text
}
```

### Golden File / Snapshot Tests

- [ ] Create `tests/compression_eval/golden/` directory
- [ ] For each compressor, store input/output pairs as golden files:
  ```
  golden/
    json/
      array_100.input.json
      array_100.expected.txt
      object_nested.input.json
      object_nested.expected.txt
    log/
      repeated_info.input.txt
      repeated_info.expected.txt
    stack/
      rust_panic.input.txt
      rust_panic.expected.txt
    ...
  ```
- [ ] Test runner loads each pair, runs compressor, asserts output matches expected
- [ ] On intentional format change: update golden files, require review of diff
- [ ] CI gate: golden file test failures block merge

### Round-Trip Tests

Verify that summary → detail=true returns ALL information from raw output.

```rust
#[cfg(test)]
mod round_trip {
    // For every tool with summary/detail modes:
    #[test]
    fn test_search_round_trip() {
        let raw = get_real_search_output();
        let summary = compress_search(raw, detail=false);
        let detail = compress_search(raw, detail=true);
        // detail must equal raw (passthrough)
        assert_eq!(detail, raw);
        // summary must contain all result identifiers (file::symbol)
        for result in parse_results(raw) {
            assert!(summary.contains(&result.identifier));
        }
    }

    #[test]
    fn test_doc_context_round_trip() {
        let raw = get_real_doc_context_output();
        let summary = compress_doc_context(raw, detail=false);
        // summary must contain: function name, all caller names, all callee names
        let parsed = parse_doc_context(raw);
        assert!(summary.contains(&parsed.function_name));
        for caller in &parsed.callers {
            assert!(summary.contains(&caller.name));
        }
    }

    // Repeat for trace_callers, trace_callees, get_architecture, find_all_references
}
```

### Performance / Latency Benchmarks

```rust
// Using criterion for microsecond-accurate benchmarks
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_compressors(c: &mut Criterion) {
    let json_input = load_fixture("json_array_1000.json");    // ~10KB
    let log_input = load_fixture("server_log_5000.txt");       // ~50KB
    let stack_input = load_fixture("java_stacktrace.txt");     // ~5KB
    let tree_input = load_fixture("large_repo_tree.txt");      // ~20KB
    let build_input = load_fixture("cargo_build_output.txt");  // ~30KB
    let prose_input = load_fixture("readme_large.md");         // ~15KB

    c.bench_function("compress_json_10kb", |b| b.iter(|| compress_json(&json_input)));
    c.bench_function("compress_log_50kb", |b| b.iter(|| compress_log(&log_input)));
    c.bench_function("compress_stack_5kb", |b| b.iter(|| compress_stack_trace(&stack_input)));
    c.bench_function("compress_tree_20kb", |b| b.iter(|| compress_file_tree(&tree_input)));
    c.bench_function("compress_build_30kb", |b| b.iter(|| compress_build_output(&build_input)));
    c.bench_function("compress_prose_15kb", |b| b.iter(|| compress_prose(&prose_input)));
    c.bench_function("classify_content", |b| b.iter(|| classify_content(&json_input)));
    c.bench_function("full_pipeline_50kb", |b| b.iter(|| compress_tool_output(&log_input, "bash", &args, &config)));
}

// Assertions (run as #[test], not benchmark):
// - Each individual compressor: < 10ms for typical input
// - Full pipeline: < 50ms for 50KB input
// - classify_content: < 1ms
// - No compressor allocates > 2x input size
```

### Content Classifier Tests

```rust
#[cfg(test)]
mod classifier {
    #[test] fn test_classify_json_object() { assert_eq!(classify("{}"), Json); }
    #[test] fn test_classify_json_array() { assert_eq!(classify("[1,2]"), JsonArray); }
    #[test] fn test_classify_json_nested() { assert_eq!(classify(r#"{"a":{"b":1}}"#), Json); }
    #[test] fn test_classify_log_syslog() { assert_eq!(classify("2026-07-10 12:00:00 INFO ..."), LogOutput); }
    #[test] fn test_classify_log_json_structured() { assert_eq!(classify(r#"{"level":"info","ts":"..."}"#), LogOutput); } // JSON-structured logs → LogOutput not Json
    #[test] fn test_classify_stack_rust() { assert_eq!(classify("thread 'main' panicked at ..."), StackTrace); }
    #[test] fn test_classify_stack_python() { assert_eq!(classify("Traceback (most recent call last):"), StackTrace); }
    #[test] fn test_classify_stack_java() { assert_eq!(classify("Exception in thread \"main\" java.lang.NullPointerException\n\tat com.foo.Bar.baz(Bar.java:42)"), StackTrace); }
    #[test] fn test_classify_file_tree() { assert_eq!(classify("├── src/\n│   ├── main.rs"), FileTree); }
    #[test] fn test_classify_table_markdown() { assert_eq!(classify("| col1 | col2 |\n| --- | --- |"), Table); }
    #[test] fn test_classify_source_rust() { assert_eq!(classify("fn main() {\n    println!(\"hello\");\n}"), SourceCode { language: "rust".into() }); }
    #[test] fn test_classify_source_python() { assert_eq!(classify("def main():\n    print('hello')"), SourceCode { language: "python".into() }); }
    #[test] fn test_classify_markdown() { assert_eq!(classify("# Title\n\nSome paragraph text."), Markdown); }
    #[test] fn test_classify_plain_text() { assert_eq!(classify("just some random text"), PlainText); }
    #[test] fn test_classify_build_cargo() { assert_eq!(classify("   Compiling foo v0.1.0\n   Compiling bar v0.2.0"), BuildOutput); }
    #[test] fn test_classify_build_npm() { assert_eq!(classify("added 150 packages in 3s"), BuildOutput); }
    #[test] fn test_classify_ambiguous_prefers_specific() // JSON-like log line → LogOutput wins over Json
    #[test] fn test_classify_empty() { assert_eq!(classify(""), PlainText); }
    #[test] fn test_classify_whitespace_only() { assert_eq!(classify("   \n\n  "), PlainText); }
    #[test] fn test_classify_with_hint() { assert_eq!(classify_with_hint("...", "json"), Json); } // hint overrides heuristic
}
```

### Session Dedup Tests

```rust
#[cfg(test)]
mod session_dedup {
    #[test]
    fn test_first_seen_passes_through() {
        let mut ctx = SessionContext::new();
        let output = "auth.rs::login source code...";
        let result = apply_dedup(output, "auth.rs", "login", &mut ctx);
        assert_eq!(result, output); // first time → full output
    }

    #[test]
    fn test_same_hash_deduped() {
        let mut ctx = SessionContext::new();
        let output = "auth.rs::login source code...";
        apply_dedup(output, "auth.rs", "login", &mut ctx); // first time
        ctx.increment_turn();
        let result = apply_dedup(output, "auth.rs", "login", &mut ctx);
        assert!(result.contains("(seen in turn 1: auth.rs::login)"));
        assert!(!result.contains("source code")); // full source removed
    }

    #[test]
    fn test_different_hash_shows_full() {
        let mut ctx = SessionContext::new();
        apply_dedup("version 1", "auth.rs", "login", &mut ctx);
        ctx.increment_turn();
        let result = apply_dedup("version 2", "auth.rs", "login", &mut ctx);
        assert_eq!(result, "version 2"); // content changed → show full
    }

    #[test]
    fn test_stale_after_threshold_shows_full() {
        let mut ctx = SessionContext::new();
        let output = "auth.rs::login source code...";
        apply_dedup(output, "auth.rs", "login", &mut ctx);
        for _ in 0..11 { ctx.increment_turn(); } // 11 turns later
        let result = apply_dedup(output, "auth.rs", "login", &mut ctx);
        assert_eq!(result, output); // stale → show full again
    }

    #[test]
    fn test_focus_set_never_deduped() {
        let mut ctx = SessionContext::new();
        ctx.set_focus(vec!["auth.rs".to_string()]);
        let output = "auth.rs::login source code...";
        apply_dedup(output, "auth.rs", "login", &mut ctx);
        ctx.increment_turn();
        let result = apply_dedup(output, "auth.rs", "login", &mut ctx);
        assert_eq!(result, output); // in focus set → always full
    }

    #[test]
    fn test_dedup_token_savings_logged() {
        let mut ctx = SessionContext::new();
        let output = "x".repeat(1000);
        apply_dedup(&output, "big.rs", "func", &mut ctx);
        ctx.increment_turn();
        apply_dedup(&output, "big.rs", "func", &mut ctx);
        assert!(ctx.dedup_tokens_saved > 900); // nearly all tokens saved
    }
}
```

### Concurrent Agent Tests

```rust
#[cfg(test)]
mod concurrent_agents {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_shared_context_concurrent_writes() {
        let ctx = Arc::new(Mutex::new(SharedContext::new()));
        let mut handles = vec![];
        for i in 0..10 {
            let ctx = ctx.clone();
            handles.push(tokio::spawn(async move {
                let mut ctx = ctx.lock().await;
                ctx.store_snapshot(format!("key_{i}"), format!("data_{i}"), format!("agent_{i}"));
            }));
        }
        for h in handles { h.await.unwrap(); }
        let ctx = ctx.lock().await;
        assert_eq!(ctx.snapshot_count(), 10);
    }

    #[tokio::test]
    async fn test_shared_context_concurrent_read_write() {
        let ctx = Arc::new(Mutex::new(SharedContext::new()));
        ctx.lock().await.store_snapshot("key_0", "data_0", "agent_0");
        
        let mut handles = vec![];
        // 5 readers + 5 writers simultaneously
        for i in 0..5 {
            let ctx = ctx.clone();
            handles.push(tokio::spawn(async move {
                let ctx = ctx.lock().await;
                ctx.get_snapshot("key_0") // reader
            }));
            let ctx2 = ctx.clone();
            handles.push(tokio::spawn(async move {
                let mut ctx = ctx2.lock().await;
                ctx.store_snapshot(format!("key_{}", i+1), format!("data_{}", i+1), format!("agent_{}", i+1));
            }));
        }
        for h in handles { h.await.unwrap(); }
    }

    #[tokio::test]
    async fn test_dedup_across_agents() {
        let ctx = Arc::new(Mutex::new(SharedContext::new()));
        // Agent A analyzes auth.rs
        {
            let mut ctx = ctx.lock().await;
            ctx.store_snapshot("trace:auth.rs:login", "12 callers in 5 files", "agent_a");
        }
        // Agent B requests same analysis
        {
            let ctx = ctx.lock().await;
            let existing = ctx.get_snapshot("trace:auth.rs:login");
            assert!(existing.is_some()); // cache hit, no re-analysis needed
            assert_eq!(existing.unwrap().created_by, "agent_a");
        }
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let mut ctx = SharedContext::with_max_snapshots(5);
        for i in 0..10 {
            ctx.store_snapshot(format!("key_{i}"), format!("data_{i}"), "agent");
        }
        assert_eq!(ctx.snapshot_count(), 5); // oldest 5 evicted
        assert!(ctx.get_snapshot("key_0").is_none()); // evicted
        assert!(ctx.get_snapshot("key_9").is_some()); // still there
    }
}
```

### Compression Ratio Regression Tests

```rust
#[cfg(test)]
mod ratio_regression {
    // These tests FAIL if compression ratio drifts beyond acceptable bounds.
    // Update expected ratios intentionally when changing compressor logic.

    #[test]
    fn test_json_array_ratio() {
        let input = load_fixture("json_array_1000.json"); // 1000-item array
        let output = compress_json(&input);
        let ratio = output.len() as f64 / input.len() as f64;
        assert!(ratio < 0.15, "JSON array compression ratio {ratio:.2} exceeds 0.15 (>85% reduction expected)");
        assert!(ratio > 0.01, "JSON array compression ratio {ratio:.2} suspiciously low — data loss?");
    }

    #[test]
    fn test_log_repeated_ratio() {
        let input = "[INFO] Processing item\n".repeat(500);
        let output = compress_log(&input);
        let ratio = output.len() as f64 / input.len() as f64;
        assert!(ratio < 0.05, "Log compression ratio {ratio:.2} exceeds 0.05 (>95% reduction expected for repeated lines)");
    }

    #[test]
    fn test_search_output_ratio() {
        let input = load_fixture("search_20_results.txt"); // typical search output
        let output = compress_search(&input, false);
        let ratio = output.len() as f64 / input.len() as f64;
        assert!(ratio < 0.25, "Search compression ratio {ratio:.2} exceeds 0.25 (>75% reduction expected)");
    }

    #[test]
    fn test_trace_callers_ratio() {
        let input = load_fixture("trace_callers_depth3.txt"); // 47 callers
        let output = compress_trace(&input, false);
        let ratio = output.len() as f64 / input.len() as f64;
        assert!(ratio < 0.20, "Trace compression ratio {ratio:.2} exceeds 0.20 (>80% reduction expected)");
    }

    #[test]
    fn test_stack_trace_ratio() {
        let input = load_fixture("rust_panic_30_frames.txt");
        let output = compress_stack_trace(&input);
        let ratio = output.len() as f64 / input.len() as f64;
        assert!(ratio < 0.30, "Stack trace compression ratio {ratio:.2} exceeds 0.30 (>70% reduction expected)");
    }

    // Smoke test: NO compressor should ever INCREASE output size
    #[test]
    fn test_no_compressor_increases_size() {
        let fixtures = load_all_fixtures();
        for (name, input) in fixtures {
            let output = compress_tool_output(&input, "test", &default_args(), &config());
            assert!(output.len() <= input.len() + 50, // +50 for metadata headers
                "Compressor increased size for {name}: {input_len} → {output_len}",
                input_len = input.len(), output_len = output.len());
        }
    }
}
```

### Integration Tests with Real MCP Flow

```rust
#[cfg(test)]
mod integration {
    // End-to-end: call tool → compress → verify output is useful

    #[tokio::test]
    async fn test_search_compressed_still_parseable() {
        let server = start_test_mcp_server(compression_enabled=true);
        let result = server.call_tool("search", json!({"query": "login", "path": "/test/repo"})).await;
        // Compressed output must contain:
        assert!(result.contains("results for")); // header
        assert!(result.contains("::")); // symbol references
        assert!(result.contains("detail=true")); // retrieval hint
    }

    #[tokio::test]
    async fn test_detail_retrieval_works() {
        let server = start_test_mcp_server(compression_enabled=true);
        let summary = server.call_tool("search", json!({"query": "login"})).await;
        assert!(!summary.contains("fn login")); // no source in summary
        let detail = server.call_tool("search", json!({"query": "login", "detail": true})).await;
        assert!(detail.contains("fn login")); // full source in detail
    }

    #[tokio::test]
    async fn test_compression_metrics_logged() {
        let server = start_test_mcp_server(compression_enabled=true);
        server.call_tool("search", json!({"query": "login"})).await;
        let metrics = read_metrics_file();
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].compression_ratio < 1.0);
        assert!(metrics[0].raw_tokens > metrics[0].compressed_tokens);
    }

    #[tokio::test]
    async fn test_bypass_on_error() {
        let server = start_test_mcp_server(compression_enabled=true);
        let result = server.call_tool("search", json!({"query": "login", "path": "/nonexistent"})).await;
        // Error output should NOT be compressed
        let metrics = read_metrics_file();
        assert!(metrics.last().unwrap().bypassed);
    }

    #[tokio::test]
    async fn test_session_dedup_across_calls() {
        let server = start_test_mcp_server(compression_enabled=true);
        let r1 = server.call_tool("search", json!({"query": "login"})).await;
        let r2 = server.call_tool("search", json!({"query": "login"})).await;
        // Second call should have "(seen)" markers
        assert!(r2.contains("(seen"));
        assert!(r2.len() < r1.len()); // dedup made it shorter
    }

    #[tokio::test]
    async fn test_full_pipeline_under_50ms() {
        let server = start_test_mcp_server(compression_enabled=true);
        let start = Instant::now();
        server.call_tool("search", json!({"query": "login"})).await;
        let elapsed = start.elapsed();
        let baseline = measure_without_compression();
        let overhead = elapsed - baseline;
        assert!(overhead < Duration::from_millis(50), "Compression overhead {overhead:?} exceeds 50ms");
    }
}
```

### Test File Structure

Update the file structure section to add test files. Find the existing `tests/compression_eval/` block and replace with:

```
tests/compression_eval/
    tasks.json          — 20 eval task definitions
    baseline.json       — Phase 0 baseline results
    phase1.json         — Phase 1 results
    phase2.json         — Phase 2 results
    phase3.json         — Phase 3 results
    run_eval.rs         — eval harness
    golden/             — golden file snapshot tests
      json/             — JSON compressor input/expected pairs
      log/              — Log compressor input/expected pairs
      stack/            — Stack trace compressor input/expected pairs
      tree/             — File tree compressor input/expected pairs
      table/            — Table compressor input/expected pairs
      build/            — Build output compressor input/expected pairs
      prose/            — Prose compressor input/expected pairs
      tools/            — Per-tool compressor input/expected pairs
    fixtures/           — Large realistic test inputs
      json_array_1000.json
      server_log_5000.txt
      rust_panic_30_frames.txt
      large_repo_tree.txt
      cargo_build_output.txt
      readme_large.md
      search_20_results.txt
      trace_callers_depth3.txt

crates/infigraph-mcp/src/compress/tests/
    mod.rs              — test module root
    json_tests.rs       — JSON compressor unit tests
    log_tests.rs        — Log compressor unit tests
    stack_tests.rs      — Stack trace compressor unit tests
    tree_tests.rs       — File tree compressor unit tests
    table_tests.rs      — Table compressor unit tests
    build_tests.rs      — Build output compressor unit tests
    prose_tests.rs      — Prose/ML compressor unit tests
    tool_tests.rs       — Per-tool compressor unit tests
    classify_tests.rs   — Content classifier unit tests
    session_tests.rs    — Session dedup unit tests
    agent_tests.rs      — Concurrent agent / SharedContext tests
    ratio_tests.rs      — Compression ratio regression tests
    edge_cases.rs       — Edge case test matrix
    round_trip.rs       — Summary → detail round-trip tests
```

---

## Quality Benchmarks

### Eval task categories

#### Category 1: Code Search (5 tasks)
| ID | Task | Quality check |
|----|------|--------------|
| S1 | Find where authentication is handled | Correct file + function identified |
| S2 | Find all API route definitions | All routes found (count match) |
| S3 | Find configuration loading code | Correct config source identified |
| S4 | Find test files for auth module | All test files found |
| S5 | Find error handling patterns | Correct pattern identified |

#### Category 2: Code Editing (5 tasks)
| ID | Task | Quality check |
|----|------|--------------|
| E1 | Fix null check bug in specific function | Same diff as uncompressed |
| E2 | Add parameter to function + update callers | All callers updated |
| E3 | Change return type of function | All type references updated |
| E4 | Add error handling to function | Correct error handling added |
| E5 | Add input validation | Correct validation logic |

#### Category 3: Refactoring (5 tasks)
| ID | Task | Quality check |
|----|------|--------------|
| R1 | Rename function across codebase | All references updated |
| R2 | Extract helper function | Correct extraction, callers updated |
| R3 | Move function to different module | Imports updated everywhere |
| R4 | Change function signature | All call sites updated |
| R5 | Remove dead code | Correct code removed, nothing else |

#### Category 4: Understanding (5 tasks)
| ID | Task | Quality check |
|----|------|--------------|
| U1 | Explain authentication flow | Key steps mentioned (checklist) |
| U2 | Trace data from API to database | Correct path identified |
| U3 | Identify downstream dependencies | All deps found (count match) |
| U4 | Architecture overview | Key components mentioned |
| U5 | Blast radius of changing function X | Correct impact set |

### Measurement protocol

For each task, record:
```json
{
  "task_id": "S1",
  "phase": "baseline",
  "tokens_input": 12500,
  "tokens_output": 800,
  "tool_calls": 3,
  "tool_tokens": [4200, 3800, 2100],
  "answer_correct": true,
  "answer_quality": 5,
  "detail_retrievals": 0,
  "time_to_answer_ms": 8500
}
```

### Quality scoring

| Score | Meaning |
|-------|---------|
| 5 | Perfect match with uncompressed answer |
| 4 | Same conclusion, minor detail difference |
| 3 | Correct direction, missing some context |
| 2 | Partially correct, important info lost |
| 1 | Wrong answer due to missing context |

**Threshold:** mean quality score ≥ 4.5 across all tasks per phase.

---

## Success Criteria

| Metric | Phase 2 | Phase 3 | Phase 4 | Phase 5 | Full |
|--------|---------|---------|---------|---------|------|
| Token savings (mean) | ≥40% | ≥55% | ≥65% | ≥70% | ≥75% |
| Answer quality (mean) | ≥4.5/5 | ≥4.5/5 | ≥4.5/5 | ≥4.5/5 | ≥4.5/5 |
| Quality match rate | ≥95% | ≥95% | ≥95% | ≥95% | ≥95% |
| Detail retrieval rate | ≤25% | ≤20% | ≤20% | ≤15% | ≤10% |
| Latency overhead | ≤30ms | ≤40ms | ≤50ms | ≤50ms | ≤60ms |
| Multi-agent savings | — | — | — | ≥30% | ≥40% |
| Compression failures | ≤5% | ≤3% | ≤2% | ≤1% | ≤0.5% |

---

## Risk Register

| Risk | Impact | Likelihood | Mitigation |
|------|--------|-----------|------------|
| LLM produces wrong edits due to missing context | High | Medium | Never compress edit targets; progressive disclosure |
| Detail retrieval creates more tokens than savings | Medium | Low | Monitor retrieval rate; reduce compression if > 30% |
| Session context gets out of sync | Medium | Medium | Content hash verification; staleness threshold |
| Compression latency impacts responsiveness | Low | Low | All compression is simple string ops, < 50ms |
| Different LLMs need different compression levels | Medium | Medium | Make compression level configurable per model |
| Build output compression hides real errors | High | Low | Always preserve errors + warnings; test with real failures |
| JSON schema inference incorrect | Low | Medium | Fall back to truncation if schema can't be inferred |
| ML model dependency (Kompress) | Medium | Low | Default to extractive (no model); Kompress opt-in only |
| Cross-agent sync race conditions | Medium | Medium | SharedContext behind Mutex; content-hash based dedup is idempotent |
| Cache alignment breaks on provider changes | High | Low | Monitor cache_read_tokens; alert if cache hit rate drops below 80% |
| Context compaction loses compression state | Medium | High | Persist SessionContext to LM2; restore on session resume |
| Token counting inaccuracy skews budget decisions | Medium | Medium | Calibrate tokenizer per content type; use tiktoken-rs for accuracy |
| Smart prefetch wastes tokens on wrong predictions | Low | Medium | Track prefetch accuracy; auto-disable if < 50% hit rate |

---

## File structure

```
crates/infigraph-mcp/src/
  compress/
    mod.rs          — CompressionConfig, compress_tool_output()
    classify.rs     — ContentType enum, classify_content()
    tools.rs        — per-tool compressors (search, doc_context, etc.)
    generic.rs      — generic compressors (json, log, stack, tree, table)
    session.rs      — SessionContext, seen-dedup logic
    metrics.rs      — CompressionMetrics, logging, stats
    budget.rs       — budget tracking, auto-level selection
    cache.rs        — CacheAligner, live-zone gate, frozen prefix detection
    ml.rs           — ML extractive summarizer, optional Kompress integration
    agents.rs       — SharedContext, agent provenance, cross-agent dedup
  
tests/compression_eval/
    tasks.json          — 20 eval task definitions
    baseline.json       — Phase 0 baseline results
    phase1.json         — Phase 1 results
    phase2.json         — Phase 2 results
    phase3.json         — Phase 3 results
    run_eval.rs         — eval harness
    golden/             — golden file snapshot tests
      json/             — JSON compressor input/expected pairs
      log/              — Log compressor input/expected pairs
      stack/            — Stack trace compressor input/expected pairs
      tree/             — File tree compressor input/expected pairs
      table/            — Table compressor input/expected pairs
      build/            — Build output compressor input/expected pairs
      prose/            — Prose compressor input/expected pairs
      tools/            — Per-tool compressor input/expected pairs
    fixtures/           — Large realistic test inputs
      json_array_1000.json
      server_log_5000.txt
      rust_panic_30_frames.txt
      large_repo_tree.txt
      cargo_build_output.txt
      readme_large.md
      search_20_results.txt
      trace_callers_depth3.txt

crates/infigraph-mcp/src/compress/tests/
    mod.rs              — test module root
    json_tests.rs       — JSON compressor unit tests
    log_tests.rs        — Log compressor unit tests
    stack_tests.rs      — Stack trace compressor unit tests
    tree_tests.rs       — File tree compressor unit tests
    table_tests.rs      — Table compressor unit tests
    build_tests.rs      — Build output compressor unit tests
    prose_tests.rs      — Prose/ML compressor unit tests
    tool_tests.rs       — Per-tool compressor unit tests
    classify_tests.rs   — Content classifier unit tests
    session_tests.rs    — Session dedup unit tests
    agent_tests.rs      — Concurrent agent / SharedContext tests
    ratio_tests.rs      — Compression ratio regression tests
    edge_cases.rs       — Edge case test matrix
    round_trip.rs       — Summary → detail round-trip tests
```

---

## Timeline estimate

| Phase | Effort | Depends on |
|-------|--------|-----------|
| Phase 0: Baseline | 1 day | — |
| Phase 1: Cache alignment | 1-2 days | — |
| Phase 2: Tool output shaping | 3-4 days | Phase 0 |
| Phase 3: Session tracking | 2 days | Phase 2 |
| Phase 4: Generic compressors + ML | 3-4 days | Phase 2 |
| Phase 5: Cross-agent sharing | 2-3 days | Phase 3 |
| Phase 6: Budget-aware | 1-2 days | Phase 3 |
| Phase 7: Compress MCP tool | 1 day | Phase 4 |
| Phase 8: A/B + rollout | 3 weeks (gradual) | All phases |

**Total build time: ~15-18 days + 3 weeks rollout monitoring**
