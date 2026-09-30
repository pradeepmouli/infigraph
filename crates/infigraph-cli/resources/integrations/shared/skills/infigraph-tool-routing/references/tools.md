<!-- Generated from the MCP tools/list by crates/infigraph-mcp/tests/tool_reference.rs. Do not edit: run `INFIGRAPH_BLESS=1 cargo test -p infigraph-mcp --test tool_reference`. -->

# Infigraph MCP tools

Every tool the Infigraph MCP server advertises (98 in all), grouped by task. Read only the section you need.

- [Find and read code](#find-and-read-code) — search, search_symbols, search_code, semantic_search, get_symbols_in_file, get_code_snippet, symbol_context, get_doc_context, get_skeleton, list_files
- [Callers, references and change impact](#callers-references-and-change-impact) — trace_callers, trace_callees, find_all_references, transitive_impact, detect_changes, semantic_diff, get_type_hierarchy, get_file_deps, get_dependencies, generate_sequence_diagram
- [Architecture, quality and review](#architecture-quality-and-review) — get_architecture, get_stats, get_graph_schema, get_api_surface, get_complexity, get_test_coverage, generate_test_context, detect_dead_code, detect_clusters, detect_clones, detect_cross_cutting, detect_bridges, review, refactor, git_summary
- [Security, routes and data flow](#security-routes-and-data-flow) — detect_security_issues, detect_taint_flows, detect_interprocedural_taint, detect_path_traversal, detect_dynamic_urls, detect_reflection, detect_config_bindings, detect_routes
- [Graph queries and export](#graph-queries-and-export) — query_graph, export_graph, visualize, visualize_symbol
- [Indexing and projects](#indexing-and-projects) — index_project, list_projects, delete_project, list_languages, scip_import, index_manifests, ingest_structured, doctor
- [Watching for changes](#watching-for-changes) — watch_project, stop_watch, restart_watch, enable_watch, disable_watch, get_watch_status
- [Documents and Confluence](#documents-and-confluence) — index_docs, search_docs, reindex_docs, clean_docs, index_confluence, index_confluence_pages, watch_docs, stop_watch_docs, restart_watch_docs, enable_watch_docs, disable_watch_docs
- [Multi-repo groups](#multi-repo-groups) — group_list, group_create, group_add, group_index, group_build, group_sync, group_link, group_link_docs, group_query, group_search, group_search_docs, group_contracts, group_deps
- [Data pipelines](#data-pipelines) — pipeline_plugins, pipeline_query, pipeline_deps, pipeline_impact, pipeline_compliance
- [Sessions and memory](#sessions-and-memory) — get_latest_session, save_session, search_sessions, purge_sessions, memory_context, consolidate_memory
- [Context compression](#context-compression) — compress, get_compression_stats

## Common parameters

- `path`: Project root path. Optional: defaults to the project this server was started in; a subdirectory resolves to the project that owns it
- `symbol_id`: Symbol ID (e.g. 'auth.py::authenticate')

## Find and read code

### `search`

PRIMARY: Unified search — finds symbols by name, meaning, or text pattern in one call. Runs keyword-hybrid (BM25+vector) AND semantic-hybrid AND a text search together: ranked symbols, plus every line containing the query under 'Text matches', each naming its enclosing symbol (symbols holding a match rank first). To enumerate every occurrence of a string or pattern (e.g. all call sites before a rename), set regex=true: all matching lines are listed, not just `limit`. Auto-escalates internally when results are weak — no need to retry with different tools. Use this INSTEAD OF grep/ripgrep/find for ALL search. Set scope='docs' for document-only search.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  | Search query (symbol name, natural language, or text pattern) |
| `detail` | boolean |  | `false` | If true, return full source snippets and doc excerpts. Default (false) returns compact one-line-per-result format. |
| `file_pattern` | string |  |  | Optional: glob to restrict text search (e.g. '*.py') |
| `kind` | string |  |  | Optional: filter by symbol kind (Function, Method, Class, etc.) |
| `limit` | integer |  | `20` |  |
| `path` | string |  |  | see Common parameters |
| `regex` | boolean |  | `false` | If true, treat query as a raw regex (not escaped), rank the symbols containing a match first, and list every matching line rather than the first `limit` -- the way to enumerate all occurrences |
| `scope` | `code` \| `docs` \| `all` |  | `"all"` | Search scope: code (symbols and text matches), docs (documents only), all (both) |

### `search_symbols`

Advanced: Find symbols by name with keyword-weighted hybrid search (alpha=0.3). Prefer the unified `search` tool for most use cases.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  | Search query |
| `limit` | integer |  | `10` |  |
| `path` | string |  |  | see Common parameters |

### `search_code`

Advanced: Regex text search across all project files. Supports file pattern filters. Prefer the unified `search` tool for most use cases.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `pattern` | string | yes |  |  |
| `file_pattern` | string |  |  |  |
| `limit` | integer |  | `50` |  |
| `path` | string |  |  | see Common parameters |

### `semantic_search`

Advanced: Find code by meaning using semantic-weighted hybrid search (alpha=0.85). Prefer the unified `search` tool for most use cases.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  | Natural language description of what you're looking for |
| `kind` | string |  |  | Optional: filter by symbol kind (Function, Method, Class, etc.) |
| `limit` | integer |  | `10` |  |
| `path` | string |  |  | see Common parameters |

### `get_symbols_in_file`

PRIMARY: List all symbols in a file. Use INSTEAD OF reading entire files to find what's defined. Returns functions, classes, methods, variables with line numbers.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string | yes |  | Relative file path |
| `path` | string |  |  | see Common parameters |

### `get_code_snippet`

PRIMARY: Get source code for a symbol by ID. Use INSTEAD OF reading files to view function/class source. Returns exact source with context. symbol_id is discovered in two steps: run `search`/`search_symbols` first, then pass the exact id from the results (format 'file/path::name', e.g. 'app/auth.py::login').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `path` | string |  |  | see Common parameters |

### `symbol_context`

PRIMARY: Complete context for a symbol in one call — callers, callees, parent scope, file, kind, docstring. Use BEFORE modifying any function to understand its role. symbol_id is discovered in two steps: run `search`/`search_symbols` first, then pass the exact id from the results (format 'file/path::name', e.g. 'app/auth.py::login').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `path` | string |  |  | see Common parameters |

### `get_doc_context`

PRIMARY: Full documentation context for a symbol — signature, docstring, source, callers, callees, file. One call replaces get_code_snippet + trace_callers + trace_callees. Use BEFORE modifying any function. Default returns compact summary (no source); set detail=true for full source. symbol_id is discovered in two steps: run `search`/`search_symbols` first, then pass the exact id from the results (format 'file/path::name', e.g. 'app/auth.py::login').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `detail` | boolean |  | `false` | If true, return full source code. Default (false) returns signature + callers/callees only. |
| `path` | string |  |  | see Common parameters |

### `get_skeleton`

Compact annotated file skeleton. Shows one line per symbol: line number, signature, and annotations (complexity, statement count, fan-in). Class/struct members indented. Use INSTEAD OF reading whole files for structural overview.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string |  |  | File path (relative to project root) |
| `path` | string |  |  | see Common parameters |

### `list_files`

PRIMARY: List all source files in project. Use INSTEAD OF find/ls/glob for file discovery. Supports glob patterns (e.g. '*.rs', 'src/**').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `glob` | string |  |  | Optional glob pattern to filter files (e.g. '*.rs', 'src/**') |
| `path` | string |  |  | see Common parameters |


## Callers, references and change impact

### `trace_callers`

PRIMARY: Find all direct callers of a symbol. Use INSTEAD OF grep for 'who calls this function'. Returns caller symbol IDs, files, and line numbers. Set include_tests=false to exclude callers that are test functions/methods (useful when auditing production call paths). WARNING: if symbol_id is one method of a multi-method class/interface, callers of SIBLING methods on the same class are NOT included by default — the response will say so when this applies. Set expand_interface=true to aggregate across every sibling method when you actually want the interface-wide blast radius (e.g. before changing the interface itself, not just this one method). The symbol_id is a two-step lookup: first find it via `search` or `search_symbols` (results include the exact id, e.g. 'app/auth.py::login'), then pass that id here.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `expand_interface` | boolean |  | `false` | Aggregate callers across every sibling method on the same class/interface, not just symbol_id. Use when assessing the impact of changing the whole interface. |
| `include_tests` | boolean |  | `true` | Include test-function/method callers. Set false to see only non-test callers. |
| `path` | string |  |  | see Common parameters |

### `trace_callees`

PRIMARY: Find all symbols called by a given symbol. Use INSTEAD OF reading function body to find calls. Returns callee symbol IDs, files, and line numbers. Set include_tests=false to exclude callees that are test functions/methods. The symbol_id is a two-step lookup: first find it via `search` or `search_symbols` (results include the exact id, e.g. 'app/auth.py::login'), then pass that id here.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `include_tests` | boolean |  | `true` | Include test-function/method callees. Set false to see only non-test callees. |
| `path` | string |  |  | see Common parameters |

### `find_all_references`

PRIMARY: Find every location where a symbol is referenced. Use INSTEAD OF grep for rename/refactor safety. Default groups by file; set detail=true for per-line calling context. symbol_id is discovered in two steps: run `search`/`search_symbols` first, then pass the exact id from the results (format 'file/path::name', e.g. 'app/auth.py::login').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `detail` | boolean |  | `false` | If true, return full per-reference context. Default (false) groups by file. |
| `path` | string |  |  | see Common parameters |

### `transitive_impact`

PRIMARY: Find all symbols transitively affected by changes to a symbol. Use BEFORE any refactor to understand blast radius. Follows CALLS edges in reverse. WARNING: if symbol_id is one method of a multi-method class/interface, this only traces callers of THAT method — callers reaching the interface through a sibling method are NOT included, and the response will say so when this applies. Set expand_interface=true for the true interface-wide blast radius (e.g. before changing the interface's contract, not just one method's implementation).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `depth` | integer |  | `5` |  |
| `expand_interface` | boolean |  | `false` | Aggregate impact across every sibling method on the same class/interface, not just symbol_id. Use when assessing the impact of changing the whole interface. |
| `path` | string |  |  | see Common parameters |

### `detect_changes`

PRIMARY: Map git changes to affected symbols and blast radius. Use INSTEAD OF git diff + manual tracing. Shows exactly which functions changed and what depends on them.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `base` | string |  | `"HEAD"` |  |
| `depth` | integer |  | `3` |  |
| `path` | string |  |  | see Common parameters |

### `semantic_diff`

PRIMARY: Symbol-level diff between git refs. Use INSTEAD OF git diff for understanding what changed. Shows added/removed/moved/signature-changed symbols, not line noise.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `new_ref` | string |  | `"HEAD"` | New git ref (default: HEAD) |
| `old_ref` | string |  | `"HEAD~1"` | Old git ref (commit, branch, tag) |
| `path` | string |  |  | see Common parameters |

### `get_type_hierarchy`

PRIMARY: Full inheritance tree. Use INSTEAD OF grep for class hierarchy. Returns ancestors and descendants of a class/interface.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `depth` | integer |  | `5` |  |
| `path` | string |  |  | see Common parameters |

### `get_file_deps`

PRIMARY: File-level import graph. Use INSTEAD OF reading imports manually. Shows what this file imports and what imports it.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string | yes |  | Relative file path |
| `path` | string |  |  | see Common parameters |

### `get_dependencies`

PRIMARY: List external dependencies. Use INSTEAD OF reading package.json/Cargo.toml/go.mod manually. Filter by ecosystem (npm/cargo/pip/maven/gem/nuget/go/composer/pub).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `ecosystem` | string |  |  |  |
| `path` | string |  |  | see Common parameters |

### `generate_sequence_diagram`

PRIMARY: Generate Mermaid sequence diagram from call graph. Use to visualize control flow through a function. Participants = files, messages = calls.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `depth` | integer |  | `3` | Max call depth to traverse (default: 3) |
| `path` | string |  |  | see Common parameters |


## Architecture, quality and review

### `get_architecture`

PRIMARY: Codebase architecture overview. Use FIRST when onboarding to a new project. Default returns compact summary (top-5 per section); set detail=true for full listing including all entry points.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `detail` | boolean |  | `false` | If true, return full listing. Default (false) returns top-5 per section. |
| `path` | string |  |  | see Common parameters |

### `get_stats`

Graph statistics: total symbols, modules, call edges, inheritance edges, contains edges.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `get_graph_schema`

Show graph schema: node types, edge types, counts, and property names.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `get_api_surface`

PRIMARY: Public API surface — all public symbols and HTTP routes in one call. Use INSTEAD OF reading every file to find public interfaces. Set include_tests=false to exclude test/e2e symbols (useful for languages like Python where any non-underscore name is 'public' by convention, which otherwise swamps the real API surface with test helpers).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string |  |  | Relative file path |
| `include_tests` | boolean |  | `true` | Include test-function/method symbols. Set false to see only non-test public symbols. |
| `path` | string |  |  | see Common parameters |

### `get_complexity`

PRIMARY: Cyclomatic complexity metrics. Use to find complex/hard-to-maintain functions. Shows per-symbol scores, hotspots above threshold, and file averages.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string |  |  | Optional: filter to a specific file |
| `path` | string |  |  | see Common parameters |
| `threshold` | integer |  | `10` | Flag symbols at or above this complexity (default: 10) |

### `get_test_coverage`

PRIMARY: Test coverage analysis — covered %, uncovered symbols. Use to find untested code before writing tests.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string |  |  | Relative file path |
| `path` | string |  |  | see Common parameters |

### `generate_test_context`

PRIMARY: Generate prioritized test generation context. Finds untested symbols, ranks by complexity and callers, includes example test as style reference, control-flow branches, source code, and framework-specific templates with conventions and scaffolds per test type (unit/integration/functional/e2e). Use to guide LLM test generation.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `file` | string |  |  | Optional: filter to symbols in files matching this substring |
| `limit` | integer |  | `10` | Max number of target symbols to return (default: 10) |
| `path` | string |  |  | see Common parameters |
| `test_type` | string |  |  | Optional: filter templates to a specific test type (unit, integration, functional, e2e). Omit to get all applicable templates. |

### `detect_dead_code`

PRIMARY: Find unreachable functions/methods with zero callers. Use INSTEAD OF manual analysis for dead code cleanup. Excludes entry points and test fixtures.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `detect_clusters`

Louvain community detection on the call graph to discover functional modules.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `detect_clones`

PRIMARY: Find near-duplicate functions using vector similarity. Use to identify copy-paste code and refactoring opportunities. Stores SIMILAR_TO edges for later querying.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `kinds` | string |  | `"Function,Method"` | Comma-separated symbol kinds to check (default: Function,Method) |
| `limit` | integer |  | `20` | Max clone pairs to return |
| `path` | string |  |  | see Common parameters |
| `store_edges` | boolean |  | `true` | Write SIMILAR_TO edges to graph for later querying |
| `threshold` | number |  | `0.92` | Similarity threshold 0.0-1.0 (default: 0.92). Lower = more results but more false positives. |

### `detect_cross_cutting`

PRIMARY: Detect cross-cutting concerns from annotations/decorators. Finds authorization (@PreAuthorize, @login_required, [Authorize]), validation, caching, transactions, rate limiting, audit logging, feature flags, CORS, async, retry patterns across Java, Python, TypeScript, C#, Ruby, Go, Rust.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `kind` | string |  |  | Filter by concern kind: Authorization, Validation, Caching, Transaction, RateLimiting, AuditLogging, FeatureFlag, Cors, Async, Retry (default: all) |
| `path` | string |  |  | see Common parameters |

### `detect_bridges`

PRIMARY: Find cross-language boundaries — FFI, JNI, cgo, gRPC, P/Invoke, ctypes, WASM, COM. Use to map how languages interact in polyglot projects.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `kind` | string |  |  | Filter by kind: FFI, JNI, CGO, GRPC, P_INVOKE, CTYPES, WASM, COM (default: all) |
| `path` | string |  |  | see Common parameters |

### `review`

PR review: auto-detects PR type and scope. Runs: semantic diff, blast radius, affected tests, API surface, security scan, complexity, dead code, clones. Set llm=true for LLM-augmented review.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `base_ref` | string |  |  | Git ref (default HEAD~1) |
| `context` | string |  |  |  |
| `dry_run` | boolean |  |  |  |
| `group` | string |  |  |  |
| `limit` | integer |  |  |  |
| `llm` | boolean |  |  |  |
| `path` | string |  |  |  |

### `refactor`

PRIMARY: Analyze code for refactoring opportunities — file size, complexity hotspots, coupling (fan-in/fan-out), near-duplicate functions, dead code. Returns ranked recommendations with impact/effort scores. Use instead of manually running detect_clones + get_complexity + detect_dead_code separately.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `focus` | `all` \| `complexity` \| `duplication` \| `coupling` \| `size` |  | `"all"` | Focus area: all, complexity, duplication, coupling, size |
| `limit` | integer |  | `10` | Max recommendations to return |
| `path` | string |  |  | see Common parameters |
| `target` | string |  |  | File path or symbol name to analyze (default: whole project) |

### `git_summary`

PRIMARY: Symbol-level commit history. Use INSTEAD OF git log for understanding recent changes. Shows which functions were added/removed/modified per commit, not just file names.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `author` | string |  |  | Optional: filter by author name/email |
| `file` | string |  |  | Optional: filter to a specific file path |
| `n_commits` | integer |  | `10` | Number of recent commits to summarize (default: 10) |
| `path` | string |  |  | see Common parameters |


## Security, routes and data flow

### `detect_security_issues`

PRIMARY: Security vulnerability scan. Use INSTEAD OF manual grep for security patterns. Detects SQL injection, hardcoded secrets, eval/exec, path traversal, SSRF, XXE, weak crypto, command injection, XSS, open redirect. Returns file, line, severity, fix.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `category` | string |  |  | Filter by category e.g. SqlInjection, HardcodedSecret, WeakCrypto |
| `path` | string |  |  | see Common parameters |
| `severity` | string |  |  | Filter: CRITICAL, HIGH, MEDIUM, LOW (default: all) |

### `detect_taint_flows`

PRIMARY: Intra-procedural taint analysis. Traces data from user-controlled sources (HTTP params, body, headers, file reads, env vars) to dangerous sinks (SQL, commands, HTML, file access, redirects, deserialization). Tracks variable assignments, detects sanitizers. Emits TAINT_FLOW edges.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `category` | string |  |  | Filter by sink category: SqlInjection, CommandInjection, XssRisk, PathTraversal, OpenRedirect, InsecureDeserialization, LdapInjection, XPathInjection (default: all) |
| `path` | string |  |  | see Common parameters |
| `show_sanitized` | boolean |  | `false` | Include sanitized (suppressed) flows in output |

### `detect_interprocedural_taint`

Inter-procedural taint analysis. Traces taint across function call boundaries via CALLS graph edges. Finds source functions (HTTP input) that reach sink functions (SQL, commands, etc.) through call chains up to max_depth.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `category` | string |  |  | Filter by sink category (default: all) |
| `max_depth` | integer |  | `5` | Max call chain depth (default: 5) |
| `path` | string |  |  | see Common parameters |

### `detect_path_traversal`

Multi-layer path traversal detection. Combines intra and inter-procedural taint analysis focused on file path operations. Checks for sanitizers (realpath, canonicalize, secure_filename) across call chains.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `max_depth` | integer |  | `5` | Max call chain depth for inter-procedural analysis (default: 5) |
| `path` | string |  |  | see Common parameters |

### `detect_dynamic_urls`

Detect dynamic URL construction in HTTP client calls. Finds fetch, axios, requests, HttpClient, etc. with string concatenation or template literals. Matches against known routes. Emits CALLS_SERVICE edges for matched URLs.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `detect_reflection`

PRIMARY: Detect reflection/dynamic invocation sites. Finds Class.forName (Java), ServiceLoader.load (Java), getattr/importlib (Python), dynamic import/require (JS/TS), Activator.CreateInstance (C#), .send (Ruby), reflect (Go). Resolves targets via config files and graph symbols. Emits RESOLVES_TO edges.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `mechanism` | string |  |  | Filter by mechanism: ClassForName, ServiceLoader, JavaReflection, Getattr, ImportModule, DynamicRequire, DynamicImport, CSharpReflection, RubySend, GoPlugin (default: all) |
| `path` | string |  |  | see Common parameters |

### `detect_config_bindings`

PRIMARY: Detect config-driven conditional resolution. Finds @Profile, @ConditionalOnProperty, @Qualifier (Spring), settings.DEBUG (Django), IsDevelopment() (.NET), Rails.env, #[cfg(feature)] (Rust), //go:build (Go), process.env (Node.js). Also discovers config files (application.yml, appsettings.json, .env, etc.).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `kind` | string |  |  | Filter by binding kind: Profile, Qualifier, Environment, DjangoSetting, RailsEnv, BuildTag, FeatureGate, EnvConfig (default: all) |
| `path` | string |  |  | see Common parameters |
| `profile` | string |  |  | Filter by profile name (e.g. 'production', 'default') |

### `detect_routes`

PRIMARY: Detect HTTP routes/endpoints. Use INSTEAD OF grep for route decorators. Supports Flask, FastAPI, Express, NestJS, Spring, Gin, Actix, etc.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |


## Graph queries and export

### `query_graph`

Advanced: Execute Cypher query against code knowledge graph. Use for complex cross-cutting queries not covered by other tools. Full Cypher support.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `cypher` | string | yes |  | Cypher query string |
| `path` | string |  |  | see Common parameters |

### `export_graph`

Export the code graph as cypher, graphml, or json.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `format` | `cypher` \| `graphml` \| `json` | yes |  |  |
| `path` | string |  |  | see Common parameters |

### `visualize`

Generate interactive HTML graph visualization using vis.js.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `visualize_symbol`

Generate a focused HTML subgraph centered on one symbol. Traverses callers, callees, and inheritance up to `depth` hops. Root symbol highlighted in gold. Much faster than full visualize for large codebases.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `symbol_id` | string | yes |  | see Common parameters |
| `depth` | integer |  | `2` | Hop depth from the symbol (2 = callers+callees of callers+callees) |
| `path` | string |  |  | see Common parameters |


## Indexing and projects

### `index_project`

REQUIRED FIRST STEP: Parse all source files and build the code knowledge graph. Must run before any other infigraph tool. Auto-indexes 60+ languages.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `full` | boolean |  | `false` | Force a full reindex from scratch instead of incremental (default: false) |
| `path` | string |  |  | see Common parameters |

### `list_projects`

List all indexed projects from the global registry.

No parameters.

### `delete_project`

Remove a project's .infigraph directory and unregister from global registry.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string | yes |  | see Common parameters |

### `list_languages`

List all 60+ supported programming languages and their file extensions.

No parameters.

### `scip_import`

Import a SCIP index.scip to enrich the graph with compiler-grade symbols, spans, and relationships.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `index` | string |  | `"index.scip"` |  |
| `path` | string |  |  | see Common parameters |

### `index_manifests`

Parse package manifests (package.json, Cargo.toml, go.mod, pom.xml, requirements.txt, Gemfile, composer.json, pubspec.yaml, *.csproj) and store dependencies in the graph.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `ingest_structured`

Ingest structured data (YAML/JSON) into the graph using plug-n-play TOML schemas. Schemas define node tables, columns, edges. Discovers schemas from .infigraph/structured-schemas/ and ~/.infigraph/structured-schemas/. Call without schema_id to list available schemas.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `data` | any[] |  |  | Inline JSON array of records to ingest |
| `data_file` | string |  |  | Path to .json or .yaml/.yml data file |
| `path` | string |  |  | see Common parameters |
| `schema_id` | string |  |  | Schema ID to use for ingestion |

### `doctor`

Health checks for the infigraph installation: registry consistency, lock status, watcher liveness, disk space, sidecar freshness, toolchain validity. Defaults to the current project; set scope='global' to sweep every registered project.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |
| `scope` | `project` \| `global` |  | `"project"` | 'project' checks only the current project (default); 'global' sweeps every registered project |


## Watching for changes

### `watch_project`

Start a background file watcher that auto-reindexes changed files. Returns immediately with a watcher ID. Detects when changed files have cross-file call edges and warns (or auto-resolves with auto_resolve=true) so call resolution stays accurate. Use get_watch_status to check for pending reindexes.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `auto_resolve` | boolean |  | `false` | If true, automatically runs full index_project when cross-file call edges are affected by a change |
| `debounce_ms` | integer |  | `500` | Debounce interval in ms before reindexing a changed file |
| `path` | string |  |  | see Common parameters |

### `stop_watch`

Stop a running file watcher started by watch_project.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `watcher_id` | string | yes |  | Watcher ID returned by watch_project |

### `restart_watch`

Restart code-watching for a project without changing the enabled/disabled policy.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `enable_watch`

Enable code-watching for a project: persists the enabled policy to .infigraph/config.toml and starts a watcher if none is running. Works whether a daemon is running or not.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `disable_watch`

Disable code-watching for a project: persists the disabled policy to .infigraph/config.toml and stops the running watcher, if any. Works whether a daemon is running or not.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `get_watch_status`

Check the status of running watchers. Shows pending files that need a full reindex due to cross-file call edge changes. Omit watcher_id to list all watchers: this worker's own (by ID) plus watcher daemons and CLI watchers in other processes (by PID), and any dead holder that left a stale watch.lock.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `watcher_id` | string |  |  | Specific watcher ID to check (optional — omit to list all) |


## Documents and Confluence

### `index_docs`

Index documents (PDF, DOCX, PPTX, XLSX, Markdown, TXT, RST, HTML) into a document graph. Incremental — skips unchanged files.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  |  |

### `search_docs`

Search indexed documents by meaning or keywords. Returns matching chunks with file, heading, page, and text snippet.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  |  |
| `limit` | integer |  |  |  |
| `path` | string |  |  |  |

### `reindex_docs`

Force full document reindex from scratch.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  |  |

### `clean_docs`

Delete document index, embeddings, and HNSW index.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  |  |

### `index_confluence`

Fetch and index Confluence pages into the document graph. Supports incremental sync. Requires PAT or email+api_token auth.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `base_url` | string | yes |  |  |
| `space` | string | yes |  |  |
| `api_token` | string |  |  |  |
| `email` | string |  |  |  |
| `follow_depth` | integer |  |  |  |
| `follow_links` | boolean |  |  |  |
| `max_pages` | integer |  |  |  |
| `page_ids` | string[] |  |  |  |
| `pat` | string |  |  |  |
| `path` | string |  |  |  |

### `index_confluence_pages`

Index pre-fetched Confluence page content. Pass array of pages with page_id, title, content fields.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `pages` | object[] | yes |  |  |
| `space` | string | yes |  |  |
| `path` | string |  |  |  |

### `watch_docs`

Start background watcher that auto-reindexes changed documents.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `debounce_ms` | integer |  |  |  |
| `path` | string |  |  |  |

### `stop_watch_docs`

Stop a running document file watcher.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `watcher_id` | string | yes |  |  |

### `restart_watch_docs`

Restart doc-watching for a project without changing the enabled/disabled policy.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `enable_watch_docs`

Enable doc-watching for a project: persists the enabled policy to .infigraph/config.toml and starts a doc watcher if none is running. Works whether a daemon is running or not.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `disable_watch_docs`

Disable doc-watching for a project: persists the disabled policy to .infigraph/config.toml and stops the running doc watcher, if any. Works whether a daemon is running or not.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |


## Multi-repo groups

### `group_list`

List all repo groups and their members.

No parameters.

### `group_create`

Create a new repo group for organizing related repos (e.g. microservices). Use 'org' to scope group names per-team (prevents collisions on shared Postgres).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | yes |  | Group name |
| `org` | string |  |  | Organization scope (defaults to INFIGRAPH_ORG env var). Groups stored as org/name. |

### `group_add`

Add a repository to a group.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |
| `repo_name` | string | yes |  |  |
| `path` | string |  |  |  |

### `group_index`

PRIMARY: Index (or reindex) all repos in a group in one call. Use for batch indexing microservice repos.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |
| `full` | boolean |  | `false` | Clean and rebuild from scratch |

### `group_build`

PRIMARY: Full group rebuild in one command. Builds both the combined code graph and physical combined document store with merged embeddings. After build, use group_search for code or group_search_docs for documents.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |
| `full` | boolean |  | `false` | Clean and rebuild from scratch |

### `group_sync`

Extract HTTP contracts from all repos in a group.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |

### `group_link`

Link cross-service HTTP dependencies as CALLS_SERVICE edges in each caller repo's graph. Run after group_sync + group_deps. Enables cross-repo call graph traversal.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |

### `group_link_docs`

Rebuild the physical combined document store for a group from existing per-repo document indexes, including cross-repo LINKS_TO edges and merged embeddings.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |

### `group_query`

Run a Cypher query across all repos in a group.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `cypher` | string | yes |  |  |
| `group_name` | string | yes |  |  |

### `group_search`

PRIMARY: Hybrid BM25+vector search across the combined graph of a group. Searches all symbols across all repos in one call. Requires group_build first. Use deep=true when initial results seem incomplete — enriches results with cross-repo graph edges (who calls what across services) for LLM reasoning.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |
| `query` | string | yes |  | Search query |
| `alpha` | number |  | `0.3` | BM25/vector blend (0=pure BM25, 1=pure vector) |
| `deep` | boolean |  | `false` | Deep mode: enrich results with cross-repo call graph context. Use when tracing cross-service chains or when initial search misses related code in other repos. |
| `limit` | integer |  | `20` |  |

### `group_search_docs`

PRIMARY: Hybrid BM25+vector search across the physical combined document store for a group. Searches document chunks from every repository. Requires group_build first.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |
| `query` | string | yes |  | Document search query |
| `alpha` | number |  | `0.5` | BM25/vector blend (0=pure BM25, 1=pure vector) |
| `limit` | integer |  | `10` |  |

### `group_contracts`

List HTTP contracts discovered in a group.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |

### `group_deps`

PRIMARY: Detect cross-service HTTP dependencies within a group. Scans code for URL strings and matches to known routes in other services.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `group_name` | string | yes |  |  |


## Data pipelines

### `pipeline_plugins`

List loaded pipeline plugins and their configuration.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `pipeline_query`

Query a plugin-specific pipeline table by field value. Generic escape hatch for plugin-specific queries.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `field` | string | yes |  | Column name to search |
| `plugin_id` | string | yes |  | Pipeline plugin ID (e.g. 'intuit', 'dbt') |
| `value` | string | yes |  | Value to match (case-insensitive contains) |
| `path` | string |  |  | see Common parameters |

### `pipeline_deps`

List pipeline dependency edges (which pipelines feed into which).

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |

### `pipeline_impact`

Transitive impact analysis: what pipelines are affected if a table/dataset changes.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `table_name` | string | yes |  | Table or dataset name to analyze impact for |
| `max_depth` | integer |  | `3` | Max traversal depth for transitive impact |
| `path` | string |  |  | see Common parameters |

### `pipeline_compliance`

Query pipelines by compliance scope (e.g. 'IRS 7216', 'PII', 'SOX').

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `scope` | string | yes |  | Compliance scope to search for |
| `path` | string |  |  | see Common parameters |
| `plugin_id` | string |  |  | Plugin ID to query (default: 'intuit') |


## Sessions and memory

### `get_latest_session`

Retrieve recent session context from graph DB. Call at START of every new session to resume where you left off. Default: all sessions updated within 72h of the newest save (compact index). Use name to recall a named session in full. Use detail=true for full fields when multiple sessions match. Use limit for raw recent-N history.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `detail` | boolean |  | `false` | When multiple sessions match, return full fields for each instead of compact index |
| `limit` | integer |  |  | Return N most recent sessions by update time (skips 72h window clustering) |
| `name` | string |  |  | Recall a named session by its label (e.g. 'perf-optimization'). Returns full detail for that session. |
| `path` | string |  |  | see Common parameters |

### `save_session`

Save session context to a dedicated session DB for cross-session continuity. Stores Session node + semantic embedding. Multiple calls per day merge: summary/pending_tasks/constraints/assumptions/blockers overwrite, decisions append, files_touched union. Use `narrative` for full session story — written to .infigraph/sessions/session_YYYY-MM-DD.md and embedded for semantic search. Use `name` to save a named session that can be recalled later by identity.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `summary` | string | yes |  | Brief summary of what was accomplished this session |
| `assumptions` | string |  |  | What current approach depends on: 'Assumes: X. If X changes: Y.' |
| `blockers` | string |  |  | Stuck items needing human input or external dependency |
| `constraints` | string |  |  | What was tried and failed: 'Tried: X. Failed because: Y. Do not retry unless: Z.' |
| `decisions` | string |  |  | Structured decisions: 'Goal: X. Decision: Y. Why: Z. Invalidates-if: W.' Use \| to separate multiple decisions |
| `files_touched` | string |  |  | Comma-separated list of files modified |
| `name` | string |  |  | Optional name/label for this session (e.g. 'perf-optimization', 'auth-refactor'). Named sessions are stored separately from daily auto-saves and can be recalled by name via get_latest_session. |
| `narrative` | string |  |  | Full session story: what was explored, found, reasoned, decided, and why. Raw chronological dump. Appended to .infigraph/sessions/session_YYYY-MM-DD.md with timestamp. Use for rich context recovery in future sessions. |
| `path` | string |  |  | see Common parameters |
| `pending_tasks` | string |  |  | Tasks remaining / next steps |

### `search_sessions`

Semantic search across past sessions. Finds sessions by meaning, not just keywords. Returns matching sessions ranked by relevance with summaries and narrative file paths.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  | Natural language query to search sessions (e.g. 'authentication refactoring', 'VB6 grammar debugging') |
| `limit` | integer |  | `5` | Max results to return (default: 5) |
| `path` | string |  |  | see Common parameters |

### `purge_sessions`

Delete sessions older than specified days. Use to clean up old session history.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `older_than_days` | integer |  | `30` | Delete sessions older than this many days (default: 30) |
| `path` | string |  |  | see Common parameters |

### `memory_context`

LM2 output gate: Adaptive context assembly in one call. Searches code symbols (BM25+vector), sessions (semantic), and file skeletons. Ranks by relevance with L1/L2/L3 hierarchical depth. L1=anchor file symbols, L2=+callers/callees/deps, L3=full hybrid search. Auto-selects depth from query complexity. Replaces manual search+symbol_context+search_sessions chains.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `query` | string | yes |  | What context is needed (natural language) |
| `depth` | `auto` \| `L1` \| `L2` \| `L3` |  | `"auto"` | Retrieval depth: L1=anchor file only, L2=+callers/callees/deps, L3=full hybrid search, auto=heuristic selection |
| `file` | string |  |  | Optional anchor file — boosts symbols in/near this file, includes its skeleton |
| `limit` | integer |  | `10` | Max code results to return (default 10) |
| `path` | string |  |  | see Common parameters |
| `sources` | string |  | `"code,sessions,skeleton"` | Comma-separated source filter: code, sessions, skeleton |

### `consolidate_memory`

LM2 memory update: Merges similar sessions into consolidated summaries. Groups by embedding similarity, creates merged session with combined decisions/constraints/assumptions. Source sessions preserved with reduced confidence. Run when session count grows large.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `path` | string |  |  | see Common parameters |
| `threshold` | number |  | `0.7` | Similarity threshold for grouping sessions (0.0-1.0, default 0.7) |


## Context compression

### `compress`

Compress arbitrary text (JSON, logs, build output, stack traces, tables). Auto-detects content type and applies type-specific compression. Returns detected type, token savings, and compressed text.

| Parameter | Type | Required | Default | Description |
|---|---|---|---|---|
| `text` | string | yes |  | Text to compress |

### `get_compression_stats`

Show compression metrics for the current session: compression level, token budget usage, dedup entries, call count.

No parameters.
