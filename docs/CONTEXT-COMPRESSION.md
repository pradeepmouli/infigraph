# Context Compression

Infigraph's context compression engine reduces AI agent token usage by 70-90% while preserving answer quality. It works automatically — no configuration needed.

## How It Works

Every tool response passes through a 4-layer compression stack before reaching the AI agent:

```
Raw tool output (e.g. 1722 tokens)
    │
    ▼
Layer 1: Content Classification
    │  Detect type → route to optimal compressor
    ▼
Layer 2: Type-Specific Compression
    │  Tool-aware: search, doc_context, refs, architecture
    │  Generic: JSON, logs, stack traces, build output, tables, file trees
    ▼
Layer 3: Session Dedup
    │  Skip content already in context (FNV-1a hash tracking)
    ▼
Layer 4: Budget-Aware Scaling
    │  Compress harder as token budget fills up
    ▼
Compressed output (e.g. 113 tokens = 93% reduction)
```

## Compression Levels

The engine automatically selects a compression level based on remaining token budget:

| Budget Remaining | Level | Behavior |
|-----------------|-------|----------|
| > 70% | **Off** | Raw output, no compression |
| 50-70% | **Summary** | Structured summaries, all results preserved |
| 20-50% | **Aggressive** | Shorter summaries, fewer callers/callees |
| < 20% | **Minimal** | One-line summaries, counts only |

Per-tool safety caps prevent quality loss — search is capped at Summary (eval showed quality cliff at Aggressive), while other tools safely compress through Minimal.

## What Gets Compressed

### Tool-Specific Compressors

| Tool | Summary Mode | Savings |
|------|-------------|---------|
| `search` | Score + symbol + location per result, drop snippets | ~55% |
| `get_doc_context` | Header + callers/callees, drop source code | ~88% |
| `find_all_references` | File:line grouped by file, no source | ~39% |
| `get_architecture` | Top-N languages/hotspots/hubs | ~54% |
| `list_files` | Directory tree with file counts | varies |
| `get_api_surface` | Collapsed per-file, keep routes | varies |
| `git_summary` | Truncate symbol lists > 5 | varies |

### Generic Compressors (via `compress` tool)

| Content Type | Strategy | Typical Savings |
|-------------|----------|----------------|
| JSON arrays | Schema inference + count + 2 samples | ~85% |
| JSON objects | Top-level structure, truncate nested values | ~60% |
| Log output | Pattern dedup, preserve errors/warnings | ~90% |
| Stack traces | Keep app frames, collapse framework frames | ~70% |
| Build output | Collapse compile lines, keep errors/warnings | ~80% |
| File trees | Node collapse with file counts | ~60% |
| Tables | Header + row count + first/last rows | ~70% |

## Getting Full Output

Compression never blocks access to full data:

```
# Summary mode (default)
search query="auth handler"
→ 10 one-liner results with scores

# Full mode (explicit)
search query="auth handler" detail=true
→ Full source snippets per result

# Edit mode (automatic)
get_doc_context symbol_id="..." for_edit=true
→ Full source (compression bypassed)
```

## Bypass Rules

These are never compressed:
- `get_code_snippet` output (always needs full source)
- Security tools (`detect_security_issues`, `detect_taint_flows`, etc.)
- Error responses and small outputs (< 100 tokens)
- Requests with `detail=true` or `for_edit=true`

## Session Dedup

When the same content is requested again within a session, the engine returns a compact placeholder instead of re-sending:

```
(seen 2 calls ago: get_doc_context:src/lib.rs::dispatch_tool, 580 tokens — use detail=true to force full output)
```

Dedup is enabled by default. Disable with `INFIGRAPH_DEDUP=0`.

## The `compress` MCP Tool

For non-Infigraph content (bash output, JSON blobs, log files), use the `compress` tool directly:

```json
{
  "tool": "compress",
  "arguments": {
    "text": "<large text to compress>"
  }
}
```

The content type is auto-detected and the appropriate compressor is applied.

## Configuration

### Config file (`.infigraph/config.toml`)

```toml
[compression]
enabled = true           # false to disable all compression
level = "auto"           # off | summary | aggressive | minimal | auto
dedup = true             # false to disable session dedup
token_budget = 150000    # total token budget for auto-scaling
staleness_window = 6     # dedup staleness window (calls)
```

### Environment variables (override config file)

| Variable | Default | Description |
|----------|---------|-------------|
| `INFIGRAPH_COMPRESSION_LEVEL` | (auto) | Force level: `off`, `summary`, `aggressive`, `minimal` |
| `INFIGRAPH_TOKEN_BUDGET` | `150000` | Total token budget for auto-scaling |
| `INFIGRAPH_DEDUP` | (on) | `0` to disable session dedup |
| `INFIGRAPH_METRICS` | (off) | `1` to log compression metrics to `.infigraph/compression_metrics.jsonl` |

## Quality Monitoring

The engine tracks when agents request full output (`detail=true`). If the detail-request rate exceeds 30% for any tool (minimum 5 calls), compression is automatically reduced to Summary level for that tool.

Use the `get_compression_stats` tool to see current session metrics including detail-request rates per tool.

## Eval Results

Phase 6.4 eval across 20 tasks (search, doc_context, references, architecture):

| Level | Token Savings | Quality Retention |
|-------|--------------|-------------------|
| Off | 0% | 100% |
| Summary | 68.7% | 100% |
| Aggressive | 86.7% | 100% (with per-tool caps) |
| Minimal | 72.9% | 100% (with per-tool caps) |

Quality is measured via must_contain assertions — key facts (symbol names, file paths, structural info) that must survive compression for the task to be answerable.

## Architecture

Source files:
- `crates/infigraph-mcp/src/compress.rs` — Tool-specific + generic compressors, content classifier, level caps
- `crates/infigraph-mcp/src/session_context.rs` — Session state, dedup, budget tracking, auto-level

See [Implementation Plan](PLAN-context-compression.md) for full design details and eval methodology.
