use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use infigraph_core::lang::LanguageRegistry;
use infigraph_core::Infigraph;
use infigraph_languages::bundled_registry;
use serde_json::json;

/// Build a language registry with bundled languages + grammar plugins.
/// Grammar plugins are loaded from `~/.infigraph/grammars/` and `<project>/grammars/`.
fn full_registry(project_root: Option<&Path>) -> Result<LanguageRegistry> {
    let mut registry = bundled_registry()?;
    let project_grammars = project_root.map(|r| r.join("grammars"));
    if let Err(e) = infigraph_grammar_plugin::register_grammar_plugins(
        &mut registry,
        project_grammars.as_deref(),
        project_root,
    ) {
        eprintln!("[infigraph] Warning: failed to load grammar plugins: {e}");
    }
    Ok(registry)
}

#[derive(Parser)]
#[command(
    name = "infigraph",
    version,
    about = "AST-powered code analysis and impact review"
)]
struct Cli {
    /// Project root directory (defaults to current directory)
    #[arg(short, long)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize infigraph in the current project
    Init {
        /// Associate with a repo group (writes multi-repo instructions for agents)
        #[arg(long)]
        group: Option<String>,
    },

    /// Parse all files and build the code graph
    Index {
        /// Clean .infigraph and rebuild from scratch
        #[arg(long)]
        full: bool,
        /// Skip embedding generation (faster, disables semantic search)
        #[arg(long)]
        no_embed: bool,
    },

    /// Show graph statistics
    Stats,

    /// List available languages
    Languages,

    /// Show symbols extracted from a file
    Symbols {
        /// File to inspect
        file: String,
    },

    /// Run a raw Cypher query against the graph
    Query {
        /// Cypher query string
        cypher: String,
    },

    /// BM25 text search over indexed symbols
    Search {
        /// Search query
        query: String,

        /// Max results to return
        #[arg(short = 'n', long, default_value = "10")]
        limit: usize,

        /// Balance between BM25 (0.0) and vector (1.0)
        #[arg(short, long, default_value = "0.3")]
        alpha: f32,
    },

    /// Detect potentially dead code (functions/methods with no callers)
    DeadCode,

    /// Show transitive impact of changing a symbol
    Impact {
        /// Symbol ID (e.g., "auth.py::authenticate")
        symbol: String,

        /// Max traversal depth
        #[arg(short, long, default_value = "5")]
        depth: u32,
    },

    /// Install infigraph MCP server config for AI coding agents
    Install,

    /// Uninstall infigraph MCP server config from AI coding agents
    Uninstall,

    /// Benchmark bulk write strategies (dev use)
    #[command(hide = true)]
    Bench {
        #[arg(long, default_value = "134000")]
        n: usize,
    },

    /// Benchmark Parquet vs UNWIND with real data (dev use)
    #[command(hide = true)]
    BenchParquet,

    /// Update infigraph — downloads latest binary and re-registers MCP configs
    Update,

    /// Manage repository groups for multi-repo/microservice analysis
    Group {
        #[command(subcommand)]
        action: GroupAction,
    },

    /// List all registered repositories
    Repos,

    /// Grep-like text search across project files
    SearchCode {
        /// Regex pattern to search for
        pattern: String,

        /// Optional glob filter for file paths (e.g., "*.rs", "**/*.py")
        #[arg(short = 'f', long)]
        file_pattern: Option<String>,

        /// Max results to return
        #[arg(short = 'n', long, default_value = "50")]
        limit: usize,
    },

    /// Retrieve source code for a symbol by its ID
    Snippet {
        /// Symbol ID (e.g., "auth.py::authenticate")
        symbol_id: String,
    },

    /// Show codebase architecture overview (language breakdown, hotspots, hubs, entry points)
    Architecture,

    /// Detect symbols affected by uncommitted or recent git changes
    DetectChanges {
        /// Git ref to diff against (default: HEAD)
        #[arg(short, long, default_value = "HEAD")]
        base: String,

        /// Max traversal depth for blast radius
        #[arg(short, long, default_value = "3")]
        depth: u32,
    },

    /// Detect functional modules via Louvain community detection on the call graph
    Cluster,

    /// Export the code graph in various formats
    Export {
        /// Output format: cypher, graphml, or json
        format: String,

        /// Write to file instead of stdout
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Generate an interactive HTML graph visualization using vis.js
    #[command(alias = "viz")]
    Visualize,

    /// Generate a focused subgraph visualization centered on one symbol
    #[command(alias = "viz-sym")]
    VisualizeSymbol {
        /// Symbol ID (e.g. "src/auth.py::authenticate")
        symbol_id: String,
        /// Hop depth from the symbol
        #[arg(short, long, default_value = "2")]
        depth: u32,
    },

    /// Detect HTTP routes/endpoints from indexed code (Flask, Express, Spring, etc.)
    Routes,

    /// Import a SCIP index.scip file to enrich the graph with compiler-grade symbols
    ScipImport {
        /// Path to the index.scip file
        #[arg(short = 'i', long, default_value = "index.scip")]
        index: PathBuf,
    },

    /// Watch project for file changes and auto-reindex
    Watch {
        /// Debounce interval in milliseconds
        #[arg(short, long, default_value = "500")]
        debounce: u64,
    },

    /// Parse package manifests and index dependencies into the graph
    IndexManifests,

    /// List all external dependencies discovered from manifests
    #[command(alias = "deps")]
    Dependencies {
        /// Filter by ecosystem (npm, cargo, pip, maven, gem, nuget, go, composer, pub)
        #[arg(short, long)]
        ecosystem: Option<String>,
    },

    /// Find every reference location for a symbol (for safe rename/refactor)
    #[command(alias = "refs")]
    FindRefs {
        /// Symbol ID (e.g. "auth.py::authenticate")
        symbol: String,
    },

    /// Show the public API surface: all public symbols and HTTP routes
    #[command(alias = "api")]
    ApiSurface {
        /// Optional file filter
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Show file-level import dependencies (what this file imports and what imports it)
    FileDeps {
        /// Relative file path (e.g. "src/auth.py")
        file: String,
    },

    /// Show full type inheritance hierarchy for a class or interface
    #[command(alias = "hierarchy")]
    TypeHierarchy {
        /// Symbol ID of the class or interface
        symbol: String,
        /// Max hierarchy depth
        #[arg(short, long, default_value = "5")]
        depth: u32,
    },

    /// Show test coverage: which symbols have tests and which don't
    #[command(alias = "coverage")]
    TestCoverage {
        /// Optional file filter
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Scan for security vulnerabilities (SQL injection, hardcoded secrets, eval, pickle, weak crypto, etc.)
    #[command(alias = "sec")]
    Security {
        /// Filter by severity: CRITICAL, HIGH, MEDIUM, LOW
        #[arg(short, long)]
        severity: Option<String>,
        /// Filter by category: SqlInjection, HardcodedSecret, WeakCrypto, etc.
        #[arg(short, long)]
        category: Option<String>,
    },

    /// Show cyclomatic complexity for all functions/methods
    #[command(alias = "cx")]
    Complexity {
        /// Flag symbols at or above this threshold (default: 10)
        #[arg(short, long, default_value = "10")]
        threshold: u32,
        /// Optional file filter
        #[arg(short, long)]
        file: Option<String>,
    },

    /// Symbol-level diff between two git refs (added/removed/signature-changed/moved symbols)
    #[command(alias = "sdiff")]
    SemanticDiff {
        /// Old git ref
        #[arg(long, default_value = "HEAD~1")]
        old: String,
        /// New git ref
        #[arg(long, default_value = "HEAD")]
        new: String,
    },

    /// Generate a Mermaid sequence diagram from the call graph rooted at a symbol
    #[command(alias = "seq")]
    Sequence {
        /// Symbol ID (e.g. "src/main.rs::main")
        symbol_id: String,
        /// Max call depth to traverse
        #[arg(short, long, default_value = "3")]
        depth: u32,
    },

    /// Analyze code for refactoring opportunities
    Refactor {
        /// File path or symbol name to analyze (default: whole project)
        #[arg(short, long)]
        target: Option<String>,
        /// Focus area: all, complexity, duplication, coupling, size
        #[arg(short, long, default_value = "all")]
        focus: String,
        /// Max recommendations
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum GroupAction {
    /// Create a new repository group
    Create { name: String },
    /// Add a repository to a group
    Add {
        group: String,
        /// Name to register this repo as
        repo: String,
    },
    /// Remove a repository from a group
    Remove { group: String, repo: String },
    /// List all groups and their repos
    List,
    /// Index (or reindex) all repos in a group
    Index {
        group: String,
        /// Clean .infigraph and rebuild from scratch
        #[arg(long)]
        full: bool,
    },
    /// Extract and sync contracts across repos in a group
    Sync { group: String },
    /// Show contracts discovered in a group
    Contracts { group: String },
    /// Detect cross-service HTTP dependencies within a group
    Deps { group: String },
    /// Link cross-service dependencies as CALLS_SERVICE edges in caller graphs
    Link { group: String },
    /// Run a Cypher query across all repos in a group
    Query { group: String, cypher: String },
}

fn main() -> Result<()> {
    // ANTLR parsers recurse deeply; Rayon's default 2MB stack overflows.
    let _ = rayon::ThreadPoolBuilder::new()
        .stack_size(32 * 1024 * 1024)
        .build_global();

    let cli = Cli::parse();
    let root = cli.root.unwrap_or_else(|| PathBuf::from("."));

    match cli.command {
        Commands::Init { group } => cmd_init(&root, group.as_deref()),
        Commands::Index { full, no_embed } => cmd_index(&root, full, no_embed),
        Commands::Stats => cmd_stats(&root),
        Commands::Languages => cmd_languages(Some(&root)),
        Commands::Symbols { file } => cmd_symbols(&root, &file),
        Commands::Query { cypher } => cmd_query(&root, &cypher),
        Commands::Search {
            query,
            limit,
            alpha,
        } => cmd_search(&root, &query, limit, alpha),
        Commands::DeadCode => cmd_dead_code(&root),
        Commands::Impact { symbol, depth } => cmd_impact(&root, &symbol, depth),
        Commands::Install => cmd_install(),
        Commands::Uninstall => cmd_uninstall(),
        Commands::Bench { n } => {
            let registry = bundled_registry()?;
            let mut prism = Infigraph::open(&root, registry)?;
            prism.init()?;
            let store = prism.store().context("not initialized")?;
            store.test_parquet_quality()?;
            store.benchmark_bulk_write(n)
        }
        Commands::BenchParquet => {
            let registry = bundled_registry()?;
            let mut prism = Infigraph::open(&root, registry)?;
            prism.init()?;
            let store = prism.store().context("not initialized")?;
            store.benchmark_parquet_vs_csv()
        }
        Commands::Update => cmd_update(),
        Commands::Group { action } => cmd_group(&root, action),
        Commands::Repos => cmd_repos(),
        Commands::SearchCode {
            pattern,
            file_pattern,
            limit,
        } => cmd_search_code(&root, &pattern, file_pattern.as_deref(), limit),
        Commands::Snippet { symbol_id } => cmd_snippet(&root, &symbol_id),
        Commands::Architecture => cmd_architecture(&root),
        Commands::DetectChanges { base, depth } => cmd_detect_changes(&root, &base, depth),
        Commands::Cluster => cmd_cluster(&root),
        Commands::Export { format, output } => cmd_export(&root, &format, output),
        Commands::Visualize => cmd_visualize(&root),
        Commands::VisualizeSymbol { symbol_id, depth } => {
            cmd_visualize_symbol(&root, &symbol_id, depth)
        }
        Commands::Routes => cmd_routes(&root),
        Commands::ScipImport { index } => cmd_scip_import(&root, &index),
        Commands::Watch { debounce } => cmd_watch(&root, debounce),
        Commands::IndexManifests => cmd_index_manifests(&root),
        Commands::Dependencies { ecosystem } => cmd_dependencies(&root, ecosystem.as_deref()),
        Commands::FindRefs { symbol } => cmd_find_refs(&root, &symbol),
        Commands::ApiSurface { file } => cmd_api_surface(&root, file.as_deref()),
        Commands::FileDeps { file } => cmd_file_deps(&root, &file),
        Commands::TypeHierarchy { symbol, depth } => cmd_type_hierarchy(&root, &symbol, depth),
        Commands::TestCoverage { file } => cmd_test_coverage(&root, file.as_deref()),
        Commands::Security { severity, category } => {
            cmd_security(&root, severity.as_deref(), category.as_deref())
        }
        Commands::Complexity { threshold, file } => {
            cmd_complexity(&root, threshold, file.as_deref())
        }
        Commands::SemanticDiff { old, new } => cmd_semantic_diff(&root, &old, &new),
        Commands::Sequence { symbol_id, depth } => cmd_sequence(&root, &symbol_id, depth),
        Commands::Refactor {
            target,
            focus,
            limit,
        } => cmd_refactor(&root, target.as_deref(), &focus, limit),
    }
}

fn cmd_init(root: &Path, group: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    println!("Initialized infigraph in {}", root.display());
    println!("Graph database created at .infigraph/graph/");

    let group_context = if let Some(group_name) = group {
        use infigraph_core::multi::Registry;
        let reg = Registry::load().unwrap_or_default();
        if let Some(g) = reg.groups.get(group_name) {
            let repo_list: Vec<String> = g
                .repos
                .iter()
                .map(|r| {
                    reg.repos
                        .get(r)
                        .map(|e| format!("- `{}` — {}", r, e.path.display()))
                        .unwrap_or_else(|| format!("- `{}`", r))
                })
                .collect();
            Some(format!(
                "\n## This Repo's Group: `{}`\n\nThis repo is part of the `{}` microservice group. Other repos in this group:\n{}\n\nUse `group_query` with group name `{}` to query across all repos.\nUse `group_sync` then `group_deps` to find cross-service HTTP dependencies.\n",
                group_name, group_name, repo_list.join("\n"), group_name
            ))
        } else {
            println!(
                "  Warning: group '{}' not found in registry. Skipping group context.",
                group_name
            );
            None
        }
    } else {
        None
    };

    write_agent_instructions(root, group_context.as_deref())?;

    Ok(())
}

fn infigraph_instructions() -> &'static str {
    r#"# Infigraph — Code Intelligence

This project is indexed by Infigraph. Use Infigraph tools FIRST for all code tasks. Fall back to grep/read only if Infigraph returns nothing or for non-code files.

## Rules
1. Check `list_projects` before indexing — don't re-index
2. **`search`** for ALL code search — hybrid BM25+vector+grep in one call, auto-escalates
3. **`get_doc_context`** before editing any function — returns source+callers+callees in one call
4. **`trace_callers`** / **`find_all_references`** before refactoring — never grep for callers
5. **`trace_callees`** / **`transitive_impact`** for blast radius — never manually trace call chains
6. Read files directly only for non-code files (configs, docs, manifests) or edit tool line-number context

## Workflows
- **Find code:** `search` → if need symbol detail: `get_code_snippet` or `symbol_context`
- **Before editing:** `get_doc_context`
- **Before refactoring:** `find_all_references` → `transitive_impact` → edit
- **Onboarding:** `index_project` → `get_architecture` → `get_stats`
- **Multi-repo:** `group_create` → `group_add` × N → `group_index` → `group_sync` → `group_link`

> Each tool description says what it replaces — check descriptions when unsure which tool to use.

## Session Continuity — MANDATORY
- **On session start:** MUST call `get_latest_session` to resume prior context
- **After context compaction:** if you see "continued from a previous conversation" or a compaction summary, IMMEDIATELY call `save_session` with whatever context survived before doing anything else
- **MUST call `save_session` IMMEDIATELY (before responding to the user)** when ANY of these occur. No session-end signal exists — if you don't save now, context is lost forever:
  1. **Finding** — root cause identified, discovered a bug, learned how something works
  2. **Milestone** — bug fixed and verified, feature committed, test passing, build green
  3. **Decision** — chose an approach, ruled something out, changed strategy
  4. **Task done** — any pending task from a prior session is completed
  5. **Periodic** — if you have NOT called `save_session` in the last 5 exchanges with the user, call it NOW regardless of whether anything dramatic happened. This is a hard rule, not a suggestion.
- Do NOT defer saves ("I'll save later"). Do NOT batch them. Do NOT wait for user to ask.
- "Later" does not exist — context compaction or session end can happen at any moment.
- Same-day saves merge: summary/pending_tasks overwrite, decisions append, files_touched union
- **Narrative dumps:** On every `save_session`, include `narrative` field with full session story — what was explored, found, reasoned, decided, and why. Chronological prose, not terse bullets. Written to `.infigraph/sessions/session_YYYY-MM-DD.md` and embedded for semantic search. On session start, if `get_latest_session` shows a narrative log path, read it when structured fields aren't enough context.
"#
}

struct AgentInstructionTarget {
    path: &'static str,
    wrapper: fn(&str) -> String,
    label: &'static str,
}

fn wrap_plain(content: &str) -> String {
    content.to_string()
}

fn wrap_cursor_mdc(content: &str) -> String {
    format!(
        "---\ndescription: Infigraph code intelligence — use Infigraph MCP tools for all code navigation\nglobs: \nalwaysApply: true\n---\n\n{content}"
    )
}

fn wrap_kiro_rule(content: &str) -> String {
    format!("---\nname: infigraph\ndescription: Use Infigraph MCP tools for code navigation\ntype: always\n---\n\n{content}")
}

const AGENT_INSTRUCTION_TARGETS: &[AgentInstructionTarget] = &[
    AgentInstructionTarget {
        path: ".cursor/rules/infigraph.mdc",
        wrapper: wrap_cursor_mdc,
        label: "Cursor",
    },
    AgentInstructionTarget {
        path: ".github/copilot-instructions.md",
        wrapper: wrap_plain,
        label: "GitHub Copilot",
    },
    AgentInstructionTarget {
        path: ".windsurf/rules/infigraph.md",
        wrapper: wrap_plain,
        label: "Windsurf",
    },
    AgentInstructionTarget {
        path: ".kiro/rules/infigraph.md",
        wrapper: wrap_kiro_rule,
        label: "Kiro",
    },
    AgentInstructionTarget {
        path: "AGENTS.md",
        wrapper: wrap_plain,
        label: "Codex/OpenAI",
    },
    AgentInstructionTarget {
        path: "GEMINI.md",
        wrapper: wrap_plain,
        label: "Gemini CLI",
    },
];

fn write_agent_instructions(root: &std::path::Path, group_context: Option<&str>) -> Result<()> {
    let base = infigraph_instructions();
    let instructions = match group_context {
        Some(ctx) => format!("{base}\n{ctx}"),
        None => base.to_string(),
    };
    let marker = "<!-- infigraph-instructions -->";
    let mut written = Vec::new();

    for target in AGENT_INSTRUCTION_TARGETS {
        let file_path = root.join(target.path);

        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let wrapped = (target.wrapper)(&instructions);
        let block = format!("{marker}\n{wrapped}\n{marker}");

        let existing = std::fs::read_to_string(&file_path).unwrap_or_default();
        let new_content = if existing.contains(marker) {
            let start = existing.find(marker).unwrap();
            let after_first = &existing[start + marker.len()..];
            let end = after_first
                .find(marker)
                .map(|p| start + marker.len() + p + marker.len())
                .unwrap_or(existing.len());
            format!("{}{}{}", &existing[..start], block, &existing[end..])
        } else if existing.is_empty() {
            block
        } else {
            format!("{existing}\n\n{block}")
        };

        std::fs::write(&file_path, new_content)?;
        written.push(target.label);
    }

    if !written.is_empty() {
        println!("  Wrote agent instructions for: {}", written.join(", "));
    }

    Ok(())
}

fn cmd_index(root: &Path, full: bool, no_embed: bool) -> Result<()> {
    if full {
        let tg_dir = root.join(".infigraph");
        if tg_dir.exists() {
            // Sessions are in a separate DB at .infigraph/sessions/db/ — preserve them
            let sessions_dir = tg_dir.join("sessions");
            let sessions_backup = root.join(".infigraph-sessions-backup");
            let had_sessions = sessions_dir.exists();
            if had_sessions {
                let _ = std::fs::rename(&sessions_dir, &sessions_backup);
            }
            std::fs::remove_dir_all(&tg_dir)?;
            if had_sessions {
                std::fs::create_dir_all(&tg_dir)?;
                let _ = std::fs::rename(&sessions_backup, &sessions_dir);
            }
            println!("Cleaned .infigraph/ for full reindex (sessions preserved)");
        }
    }

    let registry = full_registry(Some(root))?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    println!("Indexing project...");
    let result = prism.index()?;
    println!(
        "Indexed {}/{} files",
        result.indexed_files, result.total_files
    );

    let mut by_lang: std::collections::HashMap<&str, (usize, usize)> =
        std::collections::HashMap::new();
    for ext in &result.extractions {
        let entry = by_lang.entry(&ext.language).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += ext.symbols.len();
    }
    for (lang, (files, symbols)) in &by_lang {
        println!("  {}: {} files, {} symbols", lang, files, symbols);
    }

    if result.resolve_stats.total_calls > 0 {
        println!("{}", result.resolve_stats);
    }

    let stats = prism.stats()?;
    println!("\n{}", stats);

    // Compute and save embeddings — only for new/changed symbols
    if no_embed {
        auto_scip(root, &result)?;
        return Ok(());
    }
    {
        let store = prism.store().context("graph not initialized")?;
        let changed: Vec<&str> = result.extractions.iter().map(|e| e.file.as_str()).collect();
        let count = infigraph_core::embed::update_embeddings(store, root, &changed)?;
        println!("Saved {} embeddings to .infigraph/embeddings.bin", count);
    }

    // Auto-SCIP: detect languages and run available SCIP indexers
    auto_scip(root, &result)?;

    Ok(())
}

fn on_path(cmd: &str) -> bool {
    let lookup = if cfg!(windows) { "where" } else { "which" };
    std::process::Command::new(lookup)
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Try to install an LSP server automatically. Returns true if now available.
fn try_install_lsp(lsp_server: &str) -> bool {
    if on_path(lsp_server) {
        return true;
    }

    let os = std::env::consts::OS;
    let has_brew = on_path("brew");
    let has_apt = on_path("apt-get");
    let has_npm = on_path("npm");
    let has_pip = on_path("pip3") || on_path("pip");
    let has_gem = on_path("gem");
    let has_cargo = on_path("cargo");
    let has_opam = on_path("opam");
    let has_ghcup = on_path("ghcup");
    let has_dotnet = on_path("dotnet");

    // (lsp_server, &[(os_or_"any", installer, &[args])])
    #[allow(clippy::type_complexity)]
    let installs: &[(&str, &[(&str, &str, &[&str])])] = &[
        (
            "typescript-language-server",
            &[(
                "any",
                "npm",
                &["install", "-g", "typescript-language-server"],
            )],
        ),
        (
            "pylsp",
            &[
                ("any", "pip3", &["install", "python-lsp-server"]),
                ("any", "pip", &["install", "python-lsp-server"]),
            ],
        ),
        (
            "rust-analyzer",
            &[("any", "rustup", &["component", "add", "rust-analyzer"])],
        ),
        ("solargraph", &[("any", "gem", &["install", "solargraph"])]),
        (
            "lua-language-server",
            &[
                ("macos", "brew", &["install", "lua-language-server"]),
                (
                    "linux",
                    "apt-get",
                    &["install", "-y", "lua-language-server"],
                ),
            ],
        ),
        (
            "clangd",
            &[
                ("macos", "brew", &["install", "llvm"]),
                ("linux", "apt-get", &["install", "-y", "clangd"]),
            ],
        ),
        ("zls", &[("any", "cargo", &["install", "zls"])]),
        (
            "clojure-lsp",
            &[(
                "macos",
                "brew",
                &["install", "clojure-lsp/brew/clojure-lsp-native"],
            )],
        ),
        (
            "ocamllsp",
            &[("any", "opam", &["install", "ocaml-lsp-server"])],
        ),
        (
            "haskell-language-server-wrapper",
            &[("any", "ghcup", &["install", "hls"])],
        ),
        (
            "fsautocomplete",
            &[(
                "any",
                "dotnet",
                &["tool", "install", "-g", "fsautocomplete"],
            )],
        ),
        (
            "pasls",
            &[
                ("macos", "brew", &["install", "fpc"]),
                ("linux", "apt-get", &["install", "-y", "fpc"]),
            ],
        ),
        (
            "intelephense",
            &[("any", "npm", &["install", "-g", "intelephense"])],
        ),
        (
            "erlang-ls",
            &[
                ("macos", "brew", &["install", "erlang-ls"]),
                ("linux", "apt-get", &["install", "-y", "erlang-ls"]),
            ],
        ),
        (
            "jdtls",
            &[
                ("macos", "brew", &["install", "jdtls"]),
                ("linux", "apt-get", &["install", "-y", "jdtls"]),
            ],
        ),
        (
            "gopls",
            &[("any", "go", &["install", "golang.org/x/tools/gopls@latest"])],
        ),
        (
            "omnisharp",
            &[("any", "dotnet", &["tool", "install", "-g", "csharp-ls"])],
        ),
        ("sourcekit-lsp", &[("macos", "brew", &["install", "swift"])]),
        (
            "dart",
            &[
                ("macos", "brew", &["install", "dart"]),
                ("linux", "apt-get", &["install", "-y", "dart"]),
            ],
        ),
        (
            "elixir-ls",
            &[
                ("macos", "brew", &["install", "elixir-ls"]),
                ("linux", "apt-get", &["install", "-y", "elixir-ls"]),
            ],
        ),
        ("pls", &[("any", "cpan", &["App::PerlLanguageServer"])]),
    ];

    let avail = |installer: &str| match installer {
        "npm" => has_npm,
        "pip3" | "pip" => has_pip,
        "gem" => has_gem,
        "cargo" => has_cargo,
        "brew" => has_brew,
        "apt-get" => has_apt,
        "opam" => has_opam,
        "ghcup" => has_ghcup,
        "dotnet" => has_dotnet,
        "rustup" => on_path("rustup"),
        _ => false,
    };

    if let Some((_, cmds)) = installs.iter().find(|(s, _)| *s == lsp_server) {
        for (target_os, installer, args) in *cmds {
            if (*target_os != "any" && *target_os != os) || !avail(installer) {
                continue;
            }
            println!("Auto-SCIP: installing {} via {}...", lsp_server, installer);
            let ok = std::process::Command::new(installer)
                .args(*args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok && on_path(lsp_server) {
                println!("Auto-SCIP: {} installed", lsp_server);
                return true;
            }
            break;
        }
    }

    false
}

fn run_scip_indexer(root: &Path, cmd: &str, args: &[&str], label: &str) -> bool {
    println!("Auto-SCIP: {} found — enriching graph...", label);
    let scip_out = root.join("index.scip");
    match std::process::Command::new(cmd)
        .args(args)
        .current_dir(root)
        .status()
    {
        Ok(s) if s.success() && scip_out.exists() => true,
        Ok(s) => {
            eprintln!("Auto-SCIP: {} exited with {}", label, s);
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: failed to run {}: {}", label, e);
            false
        }
    }
}

fn try_lsp_bridge(root: &Path, lsp_server: &str, lang: &str) -> bool {
    if !on_path("lsp-to-scip") || !on_path(lsp_server) {
        return false;
    }
    println!(
        "Auto-SCIP: lsp-to-scip + {} — enriching graph...",
        lsp_server
    );
    let scip_out = root.join("index.scip");
    match std::process::Command::new("lsp-to-scip")
        .args([
            "--server",
            lsp_server,
            "--lang",
            lang,
            "--out",
            "index.scip",
        ])
        .current_dir(root)
        .status()
    {
        Ok(s) if s.success() && scip_out.exists() => true,
        Ok(s) => {
            eprintln!("Auto-SCIP: lsp-to-scip exited with {}", s);
            false
        }
        Err(e) => {
            eprintln!("Auto-SCIP: lsp-to-scip failed: {}", e);
            false
        }
    }
}

fn import_scip_and_cleanup(root: &Path) {
    let scip_out = root.join("index.scip");
    if !scip_out.exists() {
        return;
    }
    let registry = match bundled_registry() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    let mut prism = match Infigraph::open(root, registry) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Auto-SCIP: import failed: {e}");
            return;
        }
    };
    if prism.init().is_err() {
        return;
    }
    let store = match prism.store() {
        Some(s) => s,
        None => return,
    };
    match infigraph_core::scip::import_scip_index(&scip_out, store) {
        Ok(stats) => println!(
            "Auto-SCIP: enriched {} symbols, {} relations added",
            stats.symbols_enriched, stats.relations_added
        ),
        Err(e) => eprintln!("Auto-SCIP: import failed: {e}"),
    }
    let _ = std::fs::remove_file(&scip_out);
}

fn auto_scip(root: &Path, result: &infigraph_core::IndexResult) -> Result<()> {
    // Count files per language; run SCIP only for the dominant language
    let mut lang_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for ext in &result.extractions {
        *lang_counts.entry(ext.language.clone()).or_insert(0) += 1;
    }
    if lang_counts.is_empty() {
        return Ok(());
    }
    let dominant = lang_counts
        .iter()
        .max_by_key(|(_, c)| *c)
        .map(|(l, _)| l.clone())
        .unwrap();

    // (lang tags, scip cmd, scip args, lsp server, lsp lang flag, install hint)
    #[allow(clippy::type_complexity)]
    let entries: &[(&[&str], &str, &[&str], &str, &str, &str)] = &[
        (&["typescript","javascript","tsx"], "scip-typescript", &["index"],             "typescript-language-server", "typescript", "npm i -g @sourcegraph/scip-typescript"),
        (&["python"],                        "scip-python",      &["index","--cwd","."], "pylsp",                      "python",     "pip install scip-python"),
        (&["rust"],                          "rust-analyzer",    &["scip","."],          "rust-analyzer",              "rust",       "rustup component add rust-analyzer"),
        (&["java","kotlin"],                 "scip-java",        &["index"],             "jdtls",                      "java",       "brew install scip-java  # or download from github.com/sourcegraph/scip-java"),
        (&["go"],                            "scip-go",          &["--cwd","."],         "gopls",                      "go",         "go install github.com/sourcegraph/scip-go@latest"),
        (&["c","cpp"],                       "",                 &[],                    "clangd",                     "cpp",        "brew install llvm  # provides clangd"),
        (&["csharp"],                        "",                 &[],                    "omnisharp",                  "csharp",     "dotnet tool install -g csharp-ls"),
        (&["ruby"],                          "",                 &[],                    "solargraph",                 "ruby",       "gem install solargraph"),
        (&["swift"],                         "",                 &[],                    "sourcekit-lsp",              "swift",      "brew install swift  # includes sourcekit-lsp"),
        (&["dart"],                          "",                 &[],                    "dart",                       "dart",       "brew install dart"),
        (&["elixir"],                        "",                 &[],                    "elixir-ls",                  "elixir",     "brew install elixir-ls"),
        (&["haskell"],                       "",                 &[],                    "haskell-language-server-wrapper", "haskell", "ghcup install hls"),
        (&["lua"],                           "",                 &[],                    "lua-language-server",        "lua",        "brew install lua-language-server"),
        (&["php"],                           "",                 &[],                    "intelephense",               "php",        "npm i -g intelephense"),
        (&["zig"],                           "",                 &[],                    "zls",                        "zig",        "brew install zls"),
        (&["pascal"],                        "DelphiLSP64.exe",  &[],                    "pasls",                      "pascal",     "Windows only: place DelphiLSP64.exe on PATH  # https://github.com/castle-engine/pascal-language-server"),
        (&["fsharp"],                        "",                 &[],                    "fsautocomplete",             "fsharp",     "dotnet tool install -g fsautocomplete"),
        (&["clojure"],                       "",                 &[],                    "clojure-lsp",                "clojure",    "brew install clojure-lsp/brew/clojure-lsp-native"),
        (&["erlang"],                        "",                 &[],                    "erlang-ls",                  "erlang",     "brew install erlang-ls"),
        (&["perl"],                          "",                 &[],                    "pls",                        "perl",       "cpan App::PerlLanguageServer"),
        (&["ocaml"],                         "",                 &[],                    "ocamllsp",                   "ocaml",      "opam install ocaml-lsp-server"),
    ];

    for (lang_tags, scip_cmd, scip_args, lsp_server, lsp_lang, install_hint) in entries {
        if !lang_tags.iter().any(|t| *t == dominant) {
            continue;
        }

        let has_scip = !scip_cmd.is_empty() && on_path(scip_cmd);
        // Try auto-install LSP if neither SCIP indexer nor LSP server found
        let has_lsp = on_path(lsp_server) || (!has_scip && try_install_lsp(lsp_server));

        if !has_scip && !has_lsp {
            println!(
                "Auto-SCIP: {} detected but no indexer found — for compiler-grade enrichment install:\n  {}",
                lang_tags[0], install_hint
            );
            continue;
        }

        let indexed = if has_scip {
            let ok = run_scip_indexer(root, scip_cmd, scip_args, scip_cmd);
            // Rustup proxy may exist but component not installed — install and retry once
            if !ok && try_install_lsp(scip_cmd) {
                run_scip_indexer(root, scip_cmd, scip_args, scip_cmd)
            } else {
                ok
            }
        } else {
            try_lsp_bridge(root, lsp_server, lsp_lang)
        };

        if indexed {
            import_scip_and_cleanup(root);
        }
    }

    Ok(())
}

fn cmd_stats(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let stats = prism.stats()?;
    println!("{}", stats);
    Ok(())
}

fn cmd_languages(project_root: Option<&Path>) -> Result<()> {
    let registry = full_registry(project_root)?;
    println!("Available languages:");
    for pack in registry.languages() {
        let backend = match &pack.backend {
            infigraph_core::lang::ParserBackend::TreeSitter { .. } => "tree-sitter",
            infigraph_core::lang::ParserBackend::Custom(_) => "grammar-plugin",
        };
        println!(
            "  {} ({}) [{}]",
            pack.name,
            pack.extensions.join(", "),
            backend
        );
    }
    Ok(())
}

fn cmd_symbols(root: &Path, file: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let symbols = gq.symbols_in_file(file)?;
    if symbols.is_empty() {
        println!(
            "No symbols found for '{}'. Run 'infigraph index' first.",
            file
        );
        return Ok(());
    }

    println!("Symbols in {}:", file);
    for s in &symbols {
        println!(
            "  {:>8} {:30} L{}-{}",
            s.kind, s.name, s.start_line, s.end_line
        );
    }
    Ok(())
}

fn cmd_query(root: &Path, cypher: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let rows = gq.raw_query(cypher)?;
    for row in &rows {
        println!("{}", row.join(" | "));
    }
    if rows.is_empty() {
        println!("(no results)");
    }
    Ok(())
}

fn cmd_dead_code(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    // Find functions/methods that are never called
    let rows = gq.raw_query(
        "MATCH (s:Symbol) WHERE s.kind IN ['Function', 'Method'] AND NOT EXISTS { MATCH ()-[:CALLS]->(s) } RETURN s.name, s.kind, s.file ORDER BY s.file, s.name",
    )?;

    if rows.is_empty() {
        println!("No dead code found (all functions/methods have callers).");
        return Ok(());
    }

    // Filter out common entry points
    let entry_points = ["main", "__init__", "setUp", "tearDown"];
    let dead: Vec<&Vec<String>> = rows
        .iter()
        .filter(|row| !entry_points.contains(&row[0].as_str()))
        .collect();

    if dead.is_empty() {
        println!("No dead code found (all non-entry-point functions have callers).");
        return Ok(());
    }

    println!("Potentially dead code ({} symbols):", dead.len());
    let mut current_file = "";
    for row in &dead {
        if row[2] != current_file {
            current_file = &row[2];
            println!("\n  {}:", current_file);
        }
        println!("    {:>8} {}", row[1], row[0]);
    }

    Ok(())
}

fn cmd_impact(root: &Path, symbol: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let impacted = gq.transitive_impact(symbol, depth)?;

    if impacted.is_empty() {
        println!("No symbols affected by changes to '{}'", symbol);
        return Ok(());
    }

    println!(
        "Symbols affected by changes to '{}' (depth={}):",
        symbol, depth
    );
    for row in &impacted {
        println!("  {:>8} {:30} {}", row.kind, row.name, row.file);
    }

    Ok(())
}

fn cmd_group(root: &Path, action: GroupAction) -> Result<()> {
    use infigraph_core::multi::Registry;

    let mut registry = Registry::load().unwrap_or_default();

    match action {
        GroupAction::Create { name } => {
            registry.create_group(&name)?;
            registry.save()?;
            println!("Created group '{}'", name);
        }
        GroupAction::Add { group, repo } => {
            let reg = bundled_registry()?;
            let mut prism = Infigraph::open(root, reg)?;
            prism.init()?;
            registry.register_repo(&repo, root, &prism)?;
            registry.group_add(&group, &repo)?;
            registry.save()?;
            println!("Added repo '{}' to group '{}'", repo, group);
        }
        GroupAction::Remove { group, repo } => {
            registry.group_remove(&group, &repo)?;
            registry.save()?;
            println!("Removed repo '{}' from group '{}'", repo, group);
        }
        GroupAction::List => {
            if registry.groups.is_empty() {
                println!("No groups defined.");
            } else {
                for (name, group) in &registry.groups {
                    println!("{}:", name);
                    for r in &group.repos {
                        println!("  - {}", r);
                    }
                }
            }
        }
        GroupAction::Index { group, full } => {
            let g = registry
                .groups
                .get(&group)
                .context(format!("group '{}' not found", group))?
                .clone();
            println!("Indexing {} repos in group '{}'...", g.repos.len(), group);
            for repo_name in &g.repos {
                let entry = registry
                    .repos
                    .get(repo_name)
                    .context(format!("repo '{}' not in registry", repo_name))?
                    .clone();
                println!("\n--- {} ({}) ---", repo_name, entry.path.display());
                if full {
                    let tg_dir = entry.path.join(".infigraph");
                    if tg_dir.exists() {
                        let sess_dir = tg_dir.join("sessions");
                        let sess_bak = entry.path.join(".infigraph-sessions-backup");
                        let had = sess_dir.exists();
                        if had {
                            let _ = std::fs::rename(&sess_dir, &sess_bak);
                        }
                        std::fs::remove_dir_all(&tg_dir)?;
                        if had {
                            std::fs::create_dir_all(&tg_dir)?;
                            let _ = std::fs::rename(&sess_bak, &sess_dir);
                        }
                        println!("  Cleaned .infigraph/ for full reindex (sessions preserved)");
                    }
                }
                let reg = bundled_registry()?;
                let mut prism = Infigraph::open(&entry.path, reg)?;
                prism.init()?;
                let result = prism.index()?;
                println!(
                    "  Indexed {}/{} files",
                    result.indexed_files, result.total_files
                );
                registry.register_repo(repo_name, &entry.path, &prism)?;
            }
            println!(
                "\nDone. All {} repos in group '{}' indexed.",
                g.repos.len(),
                group
            );
        }
        GroupAction::Sync { group } => {
            let count = infigraph_core::multi::sync_group_contracts(
                &mut registry,
                &group,
                bundled_registry,
            )?;
            println!("Synced {} contracts in group '{}'", count, group);
        }
        GroupAction::Contracts { group } => {
            let g = registry
                .groups
                .get(&group)
                .context(format!("group '{}' not found", group))?;
            if g.contracts.is_empty() {
                println!(
                    "No contracts discovered in group '{}'. Run 'infigraph group sync {}' first.",
                    group, group
                );
            } else {
                println!("Contracts in group '{}':", group);
                for c in &g.contracts {
                    println!(
                        "  {} {:>4} {:30} ({}) {}",
                        c.service, c.method, c.path, c.symbol_id, c.file
                    );
                }
            }
        }
        GroupAction::Deps { group } => {
            let deps = infigraph_core::multi::detect_cross_service_deps(
                &registry,
                &group,
                bundled_registry,
            )?;
            if deps.is_empty() {
                println!("No cross-service dependencies found in group '{}'. Run 'infigraph group sync {}' first.", group, group);
            } else {
                println!("Cross-service dependencies in group '{}':", group);
                for d in &deps {
                    println!(
                        "  {} ({}) → {} {} {} [{}]",
                        d.caller_service,
                        d.caller_symbol,
                        d.target_service,
                        d.target_method,
                        d.target_path,
                        d.caller_file
                    );
                }
                println!("\n{} dependencies found.", deps.len());
            }
        }
        GroupAction::Link { group } => {
            let count = infigraph_core::multi::link_cross_service_calls(
                &registry,
                &group,
                bundled_registry,
            )?;
            println!(
                "Linked {} cross-service CALLS_SERVICE edges in group '{}'.",
                count, group
            );
        }
        GroupAction::Query { group, cypher } => {
            let results = registry.group_query(&group, &cypher, bundled_registry)?;
            for (repo, rows) in &results {
                println!("--- {} ---", repo);
                for row in rows {
                    println!("  {}", row.join(" | "));
                }
            }
        }
    }

    Ok(())
}

fn cmd_repos() -> Result<()> {
    use infigraph_core::multi::Registry;

    let registry = Registry::load().unwrap_or_default();

    if registry.repos.is_empty() {
        println!(
            "No repositories registered. Use 'infigraph group add <group> <repo>' to register."
        );
        return Ok(());
    }

    println!("Registered repositories:");
    for (name, entry) in &registry.repos {
        println!(
            "  {} — {} ({} symbols, {} modules)",
            name,
            entry.path.display(),
            entry.symbol_count,
            entry.module_count
        );
    }

    Ok(())
}

/// Locate the infigraph-mcp binary: first check the same directory as the running
/// binary, then fall back to searching PATH.
fn find_mcp_binary() -> Result<PathBuf> {
    let bin_name = if cfg!(windows) {
        "infigraph-mcp.exe"
    } else {
        "infigraph-mcp"
    };

    // Check sibling of the running binary
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.parent().unwrap().join(bin_name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    // Fall back to PATH (use `where` on Windows, `which` elsewhere)
    let lookup = if cfg!(windows) { "where" } else { "which" };
    if let Ok(output) = std::process::Command::new(lookup).arg(bin_name).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let path = stdout.lines().next().unwrap_or("").trim().to_string();
            if !path.is_empty() {
                return Ok(PathBuf::from(path));
            }
        }
    }

    anyhow::bail!(
        "Could not find infigraph-mcp binary. \
         Build it with `cargo build -p infigraph-mcp` or ensure it is on your PATH."
    )
}

/// Config file format for an agent target.
#[derive(Clone, Copy, PartialEq)]
enum ConfigFormat {
    /// Standard JSON with `{ "mcpServers": { "infigraph": { ... } } }`
    Json,
    /// TOML with `[mcp]` section (Codex)
    Toml,
}

/// Agent configuration target: directory name, config file, format, and display label.
struct AgentTarget {
    dir_name: &'static str,
    config_file: &'static str,
    format: ConfigFormat,
    label: &'static str,
}

const AGENT_TARGETS: &[AgentTarget] = &[
    AgentTarget {
        dir_name: ".claude",
        config_file: "CLAUDE_CODE_SPECIAL",
        format: ConfigFormat::Json,
        label: "Claude Code",
    },
    AgentTarget {
        dir_name: ".cursor",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "Cursor",
    },
    AgentTarget {
        dir_name: ".vscode",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "VS Code",
    },
    AgentTarget {
        dir_name: ".codex",
        config_file: "config.toml",
        format: ConfigFormat::Toml,
        label: "Codex",
    },
    AgentTarget {
        dir_name: ".gemini",
        config_file: "settings.json",
        format: ConfigFormat::Json,
        label: "Gemini CLI",
    },
    AgentTarget {
        dir_name: ".zed",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "Zed",
    },
    AgentTarget {
        dir_name: ".opencode",
        config_file: "config.json",
        format: ConfigFormat::Json,
        label: "OpenCode",
    },
    AgentTarget {
        dir_name: ".aider",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "Aider",
    },
    AgentTarget {
        dir_name: ".windsurf",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "Windsurf",
    },
    AgentTarget {
        dir_name: ".kiro",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "Kiro",
    },
    AgentTarget {
        dir_name: ".copilot",
        config_file: "mcp.json",
        format: ConfigFormat::Json,
        label: "GitHub Copilot CLI",
    },
];

fn install_json_target(config_path: &std::path::Path, mcp_path_str: &str) -> Result<()> {
    // Parse existing config or start fresh
    let mut config: serde_json::Value = if config_path.is_file() {
        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", config_path.display()))?
    } else {
        json!({})
    };

    // Ensure mcpServers object exists
    if config.get("mcpServers").is_none() {
        config["mcpServers"] = json!({});
    }

    // Set (or overwrite) the infigraph entry
    config["mcpServers"]["infigraph"] = json!({
        "command": mcp_path_str,
        "args": ["--ui", "--mcp", "--port=9749"]
    });

    // Write the config
    let pretty = serde_json::to_string_pretty(&config)?;
    std::fs::write(config_path, pretty.as_bytes())
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(())
}

fn install_toml_target(config_path: &std::path::Path, mcp_path_str: &str) -> Result<()> {
    // Read existing content (if any) and update/add the [mcp] section
    let existing = if config_path.is_file() {
        std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {}", config_path.display()))?
    } else {
        String::new()
    };

    let mcp_block = format!(
        "[mcp]\ninfigraph = {{ command = \"{}\", args = [\"--ui\", \"--mcp\", \"--port=9749\"] }}\n",
        mcp_path_str
    );

    let new_content = if existing.is_empty() {
        mcp_block
    } else if let Some(start) = existing.find("[mcp]") {
        // Find the extent of the existing [mcp] section (up to the next section or EOF)
        let after_header = start + "[mcp]".len();
        let section_end = existing[after_header..]
            .find("\n[")
            .map(|pos| after_header + pos + 1) // keep the newline before [
            .unwrap_or(existing.len());
        format!(
            "{}{}{}",
            &existing[..start],
            mcp_block,
            &existing[section_end..]
        )
    } else {
        // Append the [mcp] section
        let sep = if existing.ends_with('\n') { "" } else { "\n" };
        format!("{}{}\n{}", existing, sep, mcp_block)
    };

    std::fs::write(config_path, new_content.as_bytes())
        .with_context(|| format!("Failed to write {}", config_path.display()))?;

    Ok(())
}

fn cmd_install() -> Result<()> {
    let mcp_path = find_mcp_binary()?;
    let mcp_path_str = mcp_path.to_string_lossy().to_string();

    println!("Found infigraph-mcp at: {}", mcp_path_str);

    let home = dirs::home_dir().context("Could not determine home directory")?;
    let mut configured = Vec::new();

    for target in AGENT_TARGETS {
        let dir = home.join(target.dir_name);

        // Create directory if needed
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create directory {}", dir.display()))?;

        let config_path = if target.config_file == "CLAUDE_CODE_SPECIAL" {
            home.join(".claude.json")
        } else {
            dir.join(target.config_file)
        };

        match target.format {
            ConfigFormat::Json => install_json_target(&config_path, &mcp_path_str)?,
            ConfigFormat::Toml => install_toml_target(&config_path, &mcp_path_str)?,
        }

        configured.push(target.label);
        println!("  Configured {} ({})", target.label, config_path.display());
    }

    if configured.is_empty() {
        println!("No agents were configured.");
    } else {
        println!(
            "\nInstalled infigraph MCP server for {} agent(s): {}",
            configured.len(),
            configured.join(", ")
        );
    }

    // Write primary search instructions to ~/.claude/CLAUDE.md
    let claude_md = home.join(".claude").join("CLAUDE.md");
    let marker = "<!-- infigraph-primary-search -->";
    let instructions = format!(
        r#"
{marker}
## Infigraph — Primary Code Intelligence

Infigraph MCP is indexed. Use Infigraph tools FIRST for all code tasks. Fall back to grep/Read only if Infigraph returns nothing or for non-code files.

### Rules
1. Check `list_projects` before indexing — don't re-index
2. **`search`** for ALL code search — hybrid BM25+vector+grep in one call, auto-escalates
3. **`get_doc_context`** before editing any function — returns source+callers+callees in one call
4. **`trace_callers`** / **`find_all_references`** before refactoring — never grep for callers
5. **`trace_callees`** / **`transitive_impact`** for blast radius — never manually trace call chains
6. Read files directly only for non-code files (configs, docs, manifests) or Edit tool line-number context

### Workflows
- **Find code:** `search` → if need symbol detail: `get_code_snippet` or `symbol_context`
- **Before editing:** `get_doc_context`
- **Before refactoring:** `find_all_references` → `transitive_impact` → edit
- **Onboarding:** `index_project` → `get_architecture` → `get_stats`
- **Multi-repo:** `group_create` → `group_add` × N → `group_index` → `group_sync` → `group_link`

### Verbose tools — delegate to subagent
`get_architecture`, `transitive_impact`, `detect_dead_code`, `detect_clusters`, `detect_clones`, `export_graph`, `query_graph`, `trace_callers`/`trace_callees` (deep), `group_query`, `group_index`

> All other Infigraph tools are safe to call inline. Each tool description says what it replaces — check descriptions when unsure which tool to use.

**Reindex:** `/infigraph-reindex [path]` — always runs in subagent.

### Session Continuity — MANDATORY
- **On session start:** MUST call `get_latest_session` to resume prior context
- **After context compaction:** if you see "continued from a previous conversation" or a compaction summary, IMMEDIATELY call `save_session` with whatever context survived before doing anything else
- **MUST call `save_session` IMMEDIATELY (before responding to the user)** when ANY of these occur. No session-end signal exists — if you don't save now, context is lost forever:
  1. **Finding** — root cause identified, discovered a bug, learned how something works
  2. **Milestone** — bug fixed and verified, feature committed, test passing, build green
  3. **Decision** — chose an approach, ruled something out, changed strategy
  4. **Task done** — any pending task from a prior session is completed
  5. **Periodic** — if you have NOT called `save_session` in the last 5 exchanges with the user, call it NOW regardless of whether anything dramatic happened. This is a hard rule, not a suggestion.
- Do NOT defer saves ("I'll save later"). Do NOT batch them. Do NOT wait for user to ask.
- "Later" does not exist — context compaction or session end can happen at any moment.
- Same-day saves merge: summary/pending_tasks overwrite, decisions append, files_touched union
- **Narrative dumps:** On every `save_session`, include `narrative` field with full session story — what was explored, found, reasoned, decided, and why. Chronological prose, not terse bullets. Written to `.infigraph/sessions/session_YYYY-MM-DD.md` and embedded for semantic search. On session start, if `get_latest_session` shows a narrative log path, read it when structured fields aren't enough context.

### Session Field Guide
- **decisions** — structured format: `Goal: X. Decision: Y. Why: Z. Invalidates-if: W.`
- **constraints** — things that failed: `Tried: X. Failed because: Y. Do not retry unless: Z.`
- **assumptions** — what current approach depends on: `Assumes: X. If X changes: Y.`
- **blockers** — stuck items needing human input or external dependency
- **narrative** — full session story: explorations, findings, reasoning, code changes, decisions in chronological order. Write as prose, not structured fields.
"#
    );

    let existing = std::fs::read_to_string(&claude_md).unwrap_or_default();
    let new_content = if let Some(start) = existing.find(marker) {
        // Replace existing block up to next HTML comment marker or EOF
        let after = &existing[start..];
        let end = after[marker.len()..]
            .find("\n<!-- ")
            .map(|p| start + marker.len() + p + 1)
            .unwrap_or(existing.len());
        format!("{}{}{}", &existing[..start], instructions, &existing[end..])
    } else {
        format!("{}\n{}", existing, instructions)
    };
    std::fs::write(&claude_md, new_content)?;
    println!(
        "  Updated primary search instructions in {}",
        claude_md.display()
    );

    // Write .cursorrules to ~/.cursor/rules/infigraph.mdc
    let cursor_rules_dir = home.join(".cursor").join("rules");
    if home.join(".cursor").exists() {
        std::fs::create_dir_all(&cursor_rules_dir)?;
        let cursor_rule = cursor_rules_dir.join("infigraph.mdc");
        let cursor_content = format!(
            "---\ndescription: Infigraph primary code intelligence rules\nglobs: \nalwaysApply: true\n---\n\n{instructions}"
        );
        std::fs::write(&cursor_rule, cursor_content)?;
        println!("  Updated Cursor rules in {}", cursor_rule.display());
    }

    // Write .windsurfrules to ~/.windsurf/rules/infigraph.md
    let windsurf_rules_dir = home.join(".windsurf").join("rules");
    if home.join(".windsurf").exists() {
        std::fs::create_dir_all(&windsurf_rules_dir)?;
        let windsurf_rule = windsurf_rules_dir.join("infigraph.md");
        std::fs::write(&windsurf_rule, &instructions)?;
        println!("  Updated Windsurf rules in {}", windsurf_rule.display());
    }

    // Write /infigraph-reindex command to ~/.claude/commands/
    let commands_dir = home.join(".claude").join("commands");
    std::fs::create_dir_all(&commands_dir)?;
    let reindex_cmd = commands_dir.join("infigraph-reindex.md");
    let reindex_content = r#"# Infigraph Reindex

Reindex the current project in a subagent to avoid polluting main context with index output.

## Usage

```
/infigraph-reindex [path]
```

If `path` is omitted, uses the current working directory.

## Agent Instructions

You are a Infigraph reindex subagent. Your only job is to reindex the project and report results.

1. Determine project path: use the argument provided, or fall back to the current working directory.
2. Call `mcp__infigraph__index_project` with that path.
3. Report back in this exact format (nothing else):

```
Reindexed: <path>
Files: <N> | Symbols: <N> | Calls: <N> resolved / <N> unresolved
Languages: <comma-separated list with file counts>
```

If indexing fails, report the error verbatim. Do not attempt fixes.
"#;
    if !reindex_cmd.exists() {
        std::fs::write(&reindex_cmd, reindex_content)?;
        println!(
            "  Added /infigraph-reindex command to {}",
            reindex_cmd.display()
        );
    } else {
        println!(
            "  /infigraph-reindex command already exists at {}",
            reindex_cmd.display()
        );
    }

    // Install PreToolUse enforcement hook for Claude Code
    install_enforcement_hook(&home)?;
    install_session_save_hook(&home)?;

    // Copy model files to ~/.infigraph/models/ so the binary works from any directory
    install_models(&mcp_path, &home)?;

    Ok(())
}

const ENFORCE_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph PreToolUse enforcement hook
# Warns when raw search/file tools are used in Infigraph-indexed projects.
# stdin: JSON {tool_name, tool_input, cwd}
# exit 0 = allow (with warning on stderr)

input=$(cat)
tool=$(echo "$input" | jq -r '.tool_name // empty')
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Guard: only enforce in projects with a .infigraph directory
[ -d "$cwd/.infigraph" ] || exit 0

case "$tool" in
  Grep)
    echo "WARNING: Prefer mcp__infigraph__search (unified search) over Grep. Infigraph is indexed for this project." >&2
    ;;
  Glob)
    echo "WARNING: Prefer mcp__infigraph__list_files over Glob. Infigraph is indexed for this project." >&2
    ;;
  Bash)
    cmd=$(echo "$input" | jq -r '.tool_input.command // empty')
    if echo "$cmd" | grep -qE '(^|\s|/)(grep|egrep|fgrep|rg|ripgrep|ag|ack)(\s|$)'; then
      echo "WARNING: Prefer mcp__infigraph__search over grep/rg. Infigraph is indexed for this project." >&2
    fi
    if echo "$cmd" | grep -qE '(^|\s)find\s.*-name\s'; then
      echo "WARNING: Prefer mcp__infigraph__list_files over find. Infigraph is indexed for this project." >&2
    fi
    ;;
esac

exit 0
"#;

fn install_enforcement_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    // Write the hook script
    let hook_path = hooks_dir.join("infigraph-enforce.sh");
    std::fs::write(&hook_path, ENFORCE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("  Installed enforcement hook: {}", hook_path.display());

    // Add PreToolUse entry to ~/.claude/settings.json
    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    let hook_entry = json!({
        "matcher": "Grep|Glob|Bash",
        "hooks": [{
            "type": "command",
            "command": hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    // Check if PreToolUse already has infigraph-enforce entry
    let pre_tool = settings["hooks"]
        .get("PreToolUse")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let already_exists = pre_tool.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-enforce"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !already_exists {
        let mut arr = pre_tool;
        arr.push(hook_entry);
        settings["hooks"]["PreToolUse"] = serde_json::Value::Array(arr);

        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
        println!("  Added PreToolUse hook to {}", settings_path.display());
    } else {
        // Update the script but don't duplicate the settings entry
        println!(
            "  PreToolUse hook already configured in {}",
            settings_path.display()
        );
    }

    Ok(())
}

const SESSION_SAVE_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph UserPromptSubmit hook — session save reminder
# Counts user exchanges per Claude session. Every 5th exchange, emits a
# reminder to call save_session. Resets when the PostToolUse reset hook fires.
# stdin: JSON {prompt, session_id, cwd, ...}

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

counter_dir="${TMPDIR:-/tmp}/infigraph-sessions"
mkdir -p "$counter_dir" 2>/dev/null
counter_file="$counter_dir/$session_id.count"

count=0
[ -f "$counter_file" ] && count=$(cat "$counter_file" 2>/dev/null || echo 0)
count=$((count + 1))
echo "$count" > "$counter_file"

if [ $((count % 5)) -eq 0 ]; then
  cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"INFIGRAPH SESSION SAVE: You have NOT called save_session in the last 5 exchanges. Call mcp__infigraph__save_session NOW with a summary of work done so far, pending tasks, and decisions made. Do NOT defer this."}}
ENDJSON
fi

exit 0
"#;

const SESSION_RESET_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph PostToolUse hook — resets session save counter after save_session
# stdin: JSON {tool_name, tool_input, ...}

input=$(cat)
tool=$(echo "$input" | jq -r '.tool_name // empty')

[ "$tool" = "mcp__infigraph__save_session" ] || exit 0

session_id=$(echo "$input" | jq -r '.session_id // empty')
[ -z "$session_id" ] && exit 0

counter_file="${TMPDIR:-/tmp}/infigraph-sessions/$session_id.count"
echo "0" > "$counter_file" 2>/dev/null

exit 0
"#;

const SESSION_START_HOOK_SCRIPT: &str = r#"#!/usr/bin/env bash
# Infigraph SessionStart hook — session continuity on startup/resume/compaction
# stdin: JSON {session_id, cwd, source, ...}
# source: "startup" | "resume" | "clear" | "compact"

input=$(cat)
cwd=$(echo "$input" | jq -r '.cwd // empty')

# Only enforce in Infigraph-indexed projects
[ -d "$cwd/.infigraph" ] || exit 0

source_type=$(echo "$input" | jq -r '.source // "startup"')

case "$source_type" in
  compact)
    cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION SAVE (COMPACTION): Context was just compacted. Pre-compaction decisions and context are at risk of being lost. Call mcp__infigraph__save_session NOW with summary, pending_tasks, decisions, constraints, assumptions, and blockers from this session. Then call mcp__infigraph__get_latest_session to reload saved context. Do NOT skip this."}}
ENDJSON
    ;;
  startup|resume)
    cat <<'ENDJSON'
{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"INFIGRAPH SESSION RESTORE: Call mcp__infigraph__get_latest_session to recover prior session context (decisions, constraints, blockers, pending tasks). Do NOT start work without checking prior session state."}}
ENDJSON
    ;;
esac

exit 0
"#;

fn install_session_save_hook(home: &std::path::Path) -> Result<()> {
    let hooks_dir = home.join(".claude").join("hooks");
    std::fs::create_dir_all(&hooks_dir)?;

    // Write the UserPromptSubmit script
    let save_hook_path = hooks_dir.join("infigraph-session-save.sh");
    std::fs::write(&save_hook_path, SESSION_SAVE_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&save_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write the PostToolUse reset script
    let reset_hook_path = hooks_dir.join("infigraph-session-reset.sh");
    std::fs::write(&reset_hook_path, SESSION_RESET_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&reset_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Write the SessionStart script
    let start_hook_path = hooks_dir.join("infigraph-session-start.sh");
    std::fs::write(&start_hook_path, SESSION_START_HOOK_SCRIPT)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&start_hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    println!(
        "  Installed session save hook: {}",
        save_hook_path.display()
    );
    println!(
        "  Installed session reset hook: {}",
        reset_hook_path.display()
    );
    println!(
        "  Installed session start hook: {}",
        start_hook_path.display()
    );

    // Add hooks to ~/.claude/settings.json
    let settings_path = home.join(".claude").join("settings.json");
    let mut settings: serde_json::Value = if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str(&content)?
    } else {
        json!({})
    };

    if settings.get("hooks").is_none() {
        settings["hooks"] = json!({});
    }

    // UserPromptSubmit hook for session save reminder
    let save_hook_entry = json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": save_hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let user_prompt = settings["hooks"]
        .get("UserPromptSubmit")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let save_exists = user_prompt.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-save"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    let mut settings_changed = false;

    if !save_exists {
        let mut arr = user_prompt;
        arr.push(save_hook_entry);
        settings["hooks"]["UserPromptSubmit"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!(
            "  Added UserPromptSubmit hook to {}",
            settings_path.display()
        );
    } else {
        println!("  UserPromptSubmit session hook already configured");
    }

    // PostToolUse hook for counter reset
    let reset_hook_entry = json!({
        "matcher": "mcp__infigraph__save_session",
        "hooks": [{
            "type": "command",
            "command": reset_hook_path.to_string_lossy(),
            "timeout": 5,
            "async": true
        }]
    });

    let post_tool = settings["hooks"]
        .get("PostToolUse")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let reset_exists = post_tool.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-reset"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !reset_exists {
        let mut arr = post_tool;
        arr.push(reset_hook_entry);
        settings["hooks"]["PostToolUse"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!(
            "  Added PostToolUse reset hook to {}",
            settings_path.display()
        );
    } else {
        println!("  PostToolUse session reset hook already configured");
    }

    // SessionStart hook for compaction save + startup/resume restore
    let start_hook_entry = json!({
        "hooks": [{
            "type": "command",
            "command": start_hook_path.to_string_lossy(),
            "timeout": 5
        }]
    });

    let session_start = settings["hooks"]
        .get("SessionStart")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let start_exists = session_start.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hooks| {
                hooks.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains("infigraph-session-start"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });

    if !start_exists {
        let mut arr = session_start;
        arr.push(start_hook_entry);
        settings["hooks"]["SessionStart"] = serde_json::Value::Array(arr);
        settings_changed = true;
        println!("  Added SessionStart hook to {}", settings_path.display());
    } else {
        println!("  SessionStart session hook already configured");
    }

    if settings_changed {
        let pretty = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&settings_path, pretty)?;
    }

    Ok(())
}

fn install_models(mcp_path: &std::path::Path, home: &std::path::Path) -> Result<()> {
    let dest = home
        .join(".infigraph")
        .join("models")
        .join("potion-base-8M");

    // Find source models dir: walk up from binary location
    let model_files = ["config.json", "model.safetensors", "tokenizer.json"];
    let mut src: Option<std::path::PathBuf> = None;
    let mut dir = mcp_path.parent().unwrap_or(std::path::Path::new("/"));
    loop {
        let candidate = dir.join("models").join("potion-base-8M");
        if candidate.join("model.safetensors").exists() {
            src = Some(candidate);
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }

    let Some(src) = src else {
        println!("  Model files not found near binary — skipping model install (semantic search will use trigram fallback)");
        return Ok(());
    };

    // Skip if already installed and up-to-date (check safetensors size matches)
    let src_size = std::fs::metadata(src.join("model.safetensors"))
        .map(|m| m.len())
        .unwrap_or(0);
    let dest_size = std::fs::metadata(dest.join("model.safetensors"))
        .map(|m| m.len())
        .unwrap_or(0);
    if dest_size > 0 && dest_size == src_size {
        println!("  Model already installed at {}", dest.display());
        return Ok(());
    }

    std::fs::create_dir_all(&dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    for file in &model_files {
        std::fs::copy(src.join(file), dest.join(file))
            .with_context(|| format!("Failed to copy model file {file}"))?;
    }
    println!("  Installed semantic model to {}", dest.display());
    Ok(())
}

fn uninstall_json_target<'a>(
    config_path: &std::path::Path,
    label: &'a str,
) -> Result<Option<&'a str>> {
    if !config_path.is_file() {
        println!("  Skipping {} (no config found)", label);
        return Ok(None);
    }

    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;
    let mut config: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse {}", config_path.display()))?;

    if let Some(servers) = config.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        if servers.remove("infigraph").is_some() {
            let pretty = serde_json::to_string_pretty(&config)?;
            std::fs::write(config_path, pretty.as_bytes())
                .with_context(|| format!("Failed to write {}", config_path.display()))?;
            println!(
                "  Removed infigraph from {} ({})",
                label,
                config_path.display()
            );
            return Ok(Some(label));
        } else {
            println!("  Skipping {} (infigraph entry not found)", label);
        }
    } else {
        println!("  Skipping {} (no mcpServers in config)", label);
    }

    Ok(None)
}

fn uninstall_toml_target<'a>(
    config_path: &std::path::Path,
    label: &'a str,
) -> Result<Option<&'a str>> {
    if !config_path.is_file() {
        println!("  Skipping {} (no config found)", label);
        return Ok(None);
    }

    let content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read {}", config_path.display()))?;

    if let Some(start) = content.find("[mcp]") {
        // Find the extent of the [mcp] section
        let after_header = start + "[mcp]".len();
        let section_end = content[after_header..]
            .find("\n[")
            .map(|pos| after_header + pos + 1)
            .unwrap_or(content.len());

        // Check if this section mentions infigraph
        let section_text = &content[start..section_end];
        if section_text.contains("infigraph") {
            let new_content = format!("{}{}", &content[..start], &content[section_end..]);
            let trimmed = new_content.trim_end().to_string();
            let final_content = if trimmed.is_empty() {
                String::new()
            } else {
                format!("{}\n", trimmed)
            };
            std::fs::write(config_path, final_content.as_bytes())
                .with_context(|| format!("Failed to write {}", config_path.display()))?;
            println!(
                "  Removed infigraph from {} ({})",
                label,
                config_path.display()
            );
            return Ok(Some(label));
        } else {
            println!("  Skipping {} (infigraph entry not found in [mcp])", label);
        }
    } else {
        println!("  Skipping {} (no [mcp] section in config)", label);
    }

    Ok(None)
}

fn cmd_uninstall() -> Result<()> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    let mut removed = Vec::new();

    for target in AGENT_TARGETS {
        let config_path = if target.config_file == "CLAUDE_CODE_SPECIAL" {
            home.join(".claude.json")
        } else {
            home.join(target.dir_name).join(target.config_file)
        };

        let result = match target.format {
            ConfigFormat::Json => uninstall_json_target(&config_path, target.label)?,
            ConfigFormat::Toml => uninstall_toml_target(&config_path, target.label)?,
        };

        if let Some(label) = result {
            removed.push(label);
        }
    }

    if removed.is_empty() {
        println!("No agents had infigraph configured.");
    } else {
        println!(
            "\nUninstalled infigraph MCP server from {} agent(s): {}",
            removed.len(),
            removed.join(", ")
        );
    }

    // Remove primary search instructions from ~/.claude/CLAUDE.md
    let claude_md = home.join(".claude").join("CLAUDE.md");
    let marker = "<!-- infigraph-primary-search -->";
    if claude_md.exists() {
        let content = std::fs::read_to_string(&claude_md)?;
        if let Some(start) = content.find(marker) {
            let new_content = content[..start].trim_end().to_string();
            std::fs::write(
                &claude_md,
                if new_content.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", new_content)
                },
            )?;
            println!(
                "  Removed primary search instructions from {}",
                claude_md.display()
            );
        }
    }

    // Remove Cursor rules
    let cursor_rule = home.join(".cursor").join("rules").join("infigraph.mdc");
    if cursor_rule.exists() {
        std::fs::remove_file(&cursor_rule)?;
        println!("  Removed Cursor rules: {}", cursor_rule.display());
    }

    // Remove Windsurf rules
    let windsurf_rule = home.join(".windsurf").join("rules").join("infigraph.md");
    if windsurf_rule.exists() {
        std::fs::remove_file(&windsurf_rule)?;
        println!("  Removed Windsurf rules: {}", windsurf_rule.display());
    }

    // Remove /infigraph-reindex skill from ~/.claude/commands/
    let reindex_cmd = home
        .join(".claude")
        .join("commands")
        .join("infigraph-reindex.md");
    if reindex_cmd.exists() {
        std::fs::remove_file(&reindex_cmd)?;
        println!("  Removed skill: {}", reindex_cmd.display());
    }

    // Remove hooks
    let hooks_dir = home.join(".claude").join("hooks");
    for hook_file in &[
        "infigraph-enforce.sh",
        "infigraph-session-save.sh",
        "infigraph-session-reset.sh",
        "infigraph-session-start.sh",
    ] {
        let hook_path = hooks_dir.join(hook_file);
        if hook_path.exists() {
            std::fs::remove_file(&hook_path)?;
            println!("  Removed hook: {}", hook_path.display());
        }
    }

    // Remove hook entries from ~/.claude/settings.json
    let settings_path = home.join(".claude").join("settings.json");
    if settings_path.is_file() {
        let content = std::fs::read_to_string(&settings_path)?;
        if let Ok(mut settings) = serde_json::from_str::<serde_json::Value>(&content) {
            let infigraph_hook = |entry: &serde_json::Value| -> bool {
                entry
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("infigraph-"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            };
            for event in &[
                "PreToolUse",
                "UserPromptSubmit",
                "PostToolUse",
                "SessionStart",
            ] {
                if let Some(arr) = settings["hooks"]
                    .get_mut(*event)
                    .and_then(|v| v.as_array_mut())
                {
                    let before = arr.len();
                    arr.retain(|entry| !infigraph_hook(entry));
                    if arr.len() < before {
                        println!(
                            "  Removed {} hook(s) from {}",
                            event,
                            settings_path.display()
                        );
                    }
                }
            }
            let pretty = serde_json::to_string_pretty(&settings)?;
            std::fs::write(&settings_path, pretty)?;
        }
    }

    // Remove binaries from ~/.local/bin/
    for bin in &["infigraph", "infigraph-mcp"] {
        let bin_path = home.join(".local").join("bin").join(bin);
        if bin_path.exists() {
            std::fs::remove_file(&bin_path)?;
            println!("  Removed binary: {}", bin_path.display());
        }
    }

    // Remove model cache ~/.infigraph/
    let model_cache = home.join(".infigraph");
    if model_cache.exists() {
        std::fs::remove_dir_all(&model_cache)?;
        println!("  Removed model cache: {}", model_cache.display());
    }

    Ok(())
}

fn cmd_update() -> Result<()> {
    println!("Updating infigraph...");
    println!("Downloading latest install script and running it.");
    println!("This will fetch the latest binary and re-register MCP configs.\n");

    let gh_host = std::env::var("INFIGRAPH_GH_HOST").unwrap_or_else(|_| "github.com".to_string());
    let gh_owner = std::env::var("INFIGRAPH_GH_OWNER").unwrap_or_else(|_| "intuit".to_string());
    let gh_repo = "infigraph";

    let is_ghe = gh_host != "github.com";
    let script_url = if is_ghe {
        format!(
            "https://{}/api/v3/repos/{}/{}/contents/install.sh",
            gh_host, gh_owner, gh_repo
        )
    } else {
        format!(
            "https://raw.githubusercontent.com/{}/{}/main/install.sh",
            gh_owner, gh_repo
        )
    };

    let cmd = if is_ghe {
        format!(
            "gh api -H 'Accept: application/vnd.github.raw' --hostname {} '{}' | bash",
            gh_host, script_url
        )
    } else {
        format!("curl -fsSL '{}' | bash", script_url)
    };

    let status = std::process::Command::new("bash")
        .arg("-c")
        .arg(cmd)
        .status()
        .context("failed to run install script — is `gh` or `curl` installed?")?;

    if !status.success() {
        anyhow::bail!("update failed (exit code {:?})", status.code());
    }

    Ok(())
}

fn cmd_search(root: &Path, query: &str, limit: usize, alpha: f32) -> Result<()> {
    use infigraph_core::embed;

    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    // Get all symbols with name, kind, file, docstring
    let rows = gq.raw_query("MATCH (s:Symbol) RETURN s.id, s.name, s.kind, s.file, s.docstring")?;

    if rows.is_empty() {
        println!("No symbols found. Run 'infigraph index' first.");
        return Ok(());
    }

    // Build text for each symbol
    let docs: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            let id = row[0].clone();
            let name = &row[1];
            let kind = &row[2];
            let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
            let text = if doc.is_empty() {
                format!("{} {}", kind, name)
            } else {
                format!("{} {}: {}", kind, name, doc)
            };
            (id, text)
        })
        .collect();

    // BM25 index
    let bm25_index = infigraph_core::search::BM25Index::build(docs.clone());

    // Load cached embeddings or recompute
    let embedder = embed::best_embedder();
    let emb_path = root.join(".infigraph").join("embeddings.bin");
    let symbol_embeddings: Vec<(String, Vec<f32>)> = if emb_path.exists() {
        embed::load_embeddings_cached(&emb_path)?
    } else {
        eprintln!("hint: run 'infigraph index' to cache embeddings for faster search");
        docs.iter()
            .map(|(id, text)| (id.clone(), embedder.embed(text).unwrap_or_default()))
            .collect()
    };

    let hnsw_path = root.join(".infigraph").join("hnsw_index.usearch");
    let results = infigraph_core::search::hybrid_search(
        query,
        &bm25_index,
        embedder.as_ref(),
        &symbol_embeddings,
        limit,
        alpha,
        Some(&hnsw_path),
        Some(&emb_path),
    )?;

    if results.is_empty() {
        println!("No results for '{}'", query);
        return Ok(());
    }

    println!("Results for '{}' (alpha={:.1}):", query, alpha);
    for r in &results {
        if let Some(row) = rows.iter().find(|row| row[0] == r.symbol_id) {
            println!(
                "  {:.3} (bm25:{:.2} vec:{:.2})  {:>8} {:30} {}",
                r.score, r.bm25_score, r.vector_score, row[2], row[1], row[3]
            );
            let doc = row.get(4).map(|s| s.as_str()).unwrap_or("");
            if !doc.is_empty() {
                let preview: String = doc.chars().take(80).collect();
                println!("         {}", preview);
            }
        }
    }

    Ok(())
}

fn cmd_search_code(
    root: &Path,
    pattern: &str,
    file_pattern: Option<&str>,
    limit: usize,
) -> Result<()> {
    let root = root.canonicalize().context("invalid project root")?;

    let matches = infigraph_core::search::grep_search(&root, pattern, file_pattern, limit)?;

    if matches.is_empty() {
        println!("No matches for '{}'", pattern);
        return Ok(());
    }

    println!("{} match(es):", matches.len());
    for m in &matches {
        println!("  {}:{}: {}", m.file, m.line_number, m.line_text);
    }

    Ok(())
}

fn cmd_snippet(root: &Path, symbol_id: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let detail = gq
        .find_symbol_by_id(symbol_id)?
        .context(format!("symbol '{}' not found in graph", symbol_id))?;

    let file_path = prism.root().join(&detail.file);
    let snippet = infigraph_core::search::read_lines_from_file(
        &file_path,
        detail.start_line,
        detail.end_line,
    )?;

    println!(
        "// {} {} ({}:L{}-{})",
        detail.kind, detail.name, detail.file, detail.start_line, detail.end_line
    );
    println!("{}", snippet);

    Ok(())
}

fn cmd_architecture(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let report = build_architecture_report(&gq)?;
    println!("{}", report);
    Ok(())
}

fn build_architecture_report(gq: &infigraph_core::graph::GraphQuery) -> Result<String> {
    let mut out = String::new();

    // 1. Language breakdown
    out.push_str("=== Language Breakdown ===\n");
    let lang_rows =
        gq.raw_query("MATCH (m:Module) RETURN m.language, count(m) ORDER BY count(m) DESC")?;
    if lang_rows.is_empty() {
        out.push_str("  (no modules indexed)\n");
    } else {
        for row in &lang_rows {
            out.push_str(&format!("  {:>20}: {} files\n", row[0], row[1]));
        }
    }

    // 2. Total symbols by kind
    out.push_str("\n=== Symbols by Kind ===\n");
    let kind_rows =
        gq.raw_query("MATCH (s:Symbol) RETURN s.kind, count(s) ORDER BY count(s) DESC")?;
    if kind_rows.is_empty() {
        out.push_str("  (no symbols indexed)\n");
    } else {
        for row in &kind_rows {
            out.push_str(&format!("  {:>20}: {}\n", row[0], row[1]));
        }
    }

    // 3. Hotspots: files with most symbols
    out.push_str("\n=== Hotspot Files (most symbols) ===\n");
    let hotspot_rows =
        gq.raw_query("MATCH (s:Symbol) RETURN s.file, count(s) AS cnt ORDER BY cnt DESC LIMIT 10")?;
    if hotspot_rows.is_empty() {
        out.push_str("  (no symbols indexed)\n");
    } else {
        for (i, row) in hotspot_rows.iter().enumerate() {
            out.push_str(&format!(
                "  {:>2}. {:60} {} symbols\n",
                i + 1,
                row[0],
                row[1]
            ));
        }
    }

    // 4. Hub functions: most-called
    out.push_str("\n=== Hub Functions (most callers) ===\n");
    let hub_rows = gq.raw_query(
        "MATCH ()-[r:CALLS]->(s:Symbol) RETURN s.name, s.file, count(r) AS calls ORDER BY calls DESC LIMIT 10",
    )?;
    if hub_rows.is_empty() {
        out.push_str("  (no call edges found)\n");
    } else {
        for (i, row) in hub_rows.iter().enumerate() {
            out.push_str(&format!(
                "  {:>2}. {:30} {:40} {} callers\n",
                i + 1,
                row[0],
                row[1],
                row[2]
            ));
        }
    }

    // 5. Entry points: functions that call others but are not called themselves
    out.push_str("\n=== Entry Points (call others, never called) ===\n");
    let entry_rows = gq.raw_query(
        "MATCH (s:Symbol)-[:CALLS]->() WHERE s.kind IN ['Function', 'Method'] AND NOT EXISTS { MATCH ()-[:CALLS]->(s) } RETURN DISTINCT s.name, s.kind, s.file ORDER BY s.file, s.name LIMIT 20",
    )?;
    if entry_rows.is_empty() {
        out.push_str("  (none found)\n");
    } else {
        for row in &entry_rows {
            out.push_str(&format!("  {:>8} {:30} {}\n", row[1], row[0], row[2]));
        }
    }

    Ok(out)
}

fn cmd_cluster(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;

    println!("Running Louvain community detection...");
    let stats = infigraph_core::cluster::detect_clusters(&conn)?;
    println!("{}", stats);
    Ok(())
}

fn cmd_export(root: &Path, format: &str, output: Option<PathBuf>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    match output {
        Some(path) => {
            let file = std::fs::File::create(&path)
                .with_context(|| format!("failed to create output file: {}", path.display()))?;
            let mut writer = std::io::BufWriter::new(file);
            export_to_writer(&gq, format, &mut writer)?;
            println!("Exported {} to {}", format, path.display());
        }
        None => {
            let stdout = std::io::stdout();
            let mut writer = std::io::BufWriter::new(stdout.lock());
            export_to_writer(&gq, format, &mut writer)?;
        }
    }

    Ok(())
}

fn export_to_writer<W: std::io::Write>(
    gq: &infigraph_core::graph::GraphQuery,
    format: &str,
    writer: &mut W,
) -> Result<()> {
    match format {
        "cypher" => infigraph_core::export::export_cypher(gq, writer),
        "graphml" => infigraph_core::export::export_graphml(gq, writer),
        "json" => infigraph_core::export::export_json(gq, writer),
        _ => anyhow::bail!(
            "unknown export format '{}'. Supported formats: cypher, graphml, json",
            format
        ),
    }
}

fn cmd_detect_changes(root: &Path, base: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let report = build_detect_changes_report(prism.root(), &gq, base, depth)?;
    println!("{}", report);
    Ok(())
}

/// Parse git diff output and map changed lines to symbols in the graph.
fn build_detect_changes_report(
    project_root: &std::path::Path,
    gq: &infigraph_core::graph::GraphQuery,
    base: &str,
    depth: u32,
) -> Result<String> {
    use std::collections::HashSet;

    // 1. Get changed files
    let name_output = std::process::Command::new("git")
        .args(["diff", "--name-only", base])
        .current_dir(project_root)
        .output()
        .context("failed to run git diff --name-only")?;

    if !name_output.status.success() {
        let stderr = String::from_utf8_lossy(&name_output.stderr);
        anyhow::bail!("git diff failed: {}", stderr.trim());
    }

    let changed_files: Vec<String> = String::from_utf8_lossy(&name_output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect();

    if changed_files.is_empty() {
        return Ok("No changes detected.".to_string());
    }

    // 2. Get unified diff with zero context to extract changed line ranges
    let diff_output = std::process::Command::new("git")
        .args(["diff", "--unified=0", base])
        .current_dir(project_root)
        .output()
        .context("failed to run git diff --unified=0")?;

    let diff_text = String::from_utf8_lossy(&diff_output.stdout);
    let hunks = parse_diff_hunks(&diff_text);

    // 3. For each changed file+range, find overlapping symbols
    let mut directly_changed: Vec<(String, String, String, u32, u32)> = Vec::new(); // (id, name, file, start, end)
    let mut seen_ids: HashSet<String> = HashSet::new();

    for (file, start, end) in &hunks {
        let symbols = gq.symbols_in_range(file, *start, *end)?;
        for s in symbols {
            if seen_ids.insert(s.id.clone()) {
                directly_changed.push((s.id, s.name, s.file, s.start_line, s.end_line));
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("=== Change Detection (base: {}) ===\n\n", base));
    out.push_str(&format!("Changed files: {}\n", changed_files.len()));
    for f in &changed_files {
        out.push_str(&format!("  {}\n", f));
    }

    out.push_str(&format!(
        "\n=== Directly Changed Symbols ({}) ===\n",
        directly_changed.len()
    ));
    if directly_changed.is_empty() {
        out.push_str("  (no indexed symbols overlap with changed lines)\n");
    } else {
        for (id, name, file, start, end) in &directly_changed {
            out.push_str(&format!("  {:30} {} L{}-{}\n", name, file, start, end));
            let _ = id; // used below for impact
        }
    }

    // 4. Compute blast radius via transitive impact for each directly changed symbol
    if !directly_changed.is_empty() && depth > 0 {
        let mut indirectly_affected: Vec<(String, String, String, String)> = Vec::new(); // (id, name, file, kind)
        let mut indirect_ids: HashSet<String> = HashSet::new();

        for (id, _, _, _, _) in &directly_changed {
            if let Ok(impacted) = gq.transitive_impact(id, depth) {
                for row in impacted {
                    if !seen_ids.contains(&row.id) && indirect_ids.insert(row.id.clone()) {
                        indirectly_affected.push((row.id, row.name, row.file, row.kind));
                    }
                }
            }
        }

        out.push_str(&format!(
            "\n=== Blast Radius (depth={}, {} indirectly affected) ===\n",
            depth,
            indirectly_affected.len()
        ));
        if indirectly_affected.is_empty() {
            out.push_str("  (no additional symbols affected)\n");
        } else {
            for (_, name, file, kind) in &indirectly_affected {
                out.push_str(&format!("  {:>8} {:30} {}\n", kind, name, file));
            }
        }
    }

    Ok(out)
}

/// Parse unified diff output (with --unified=0) to extract (file, start_line, end_line) hunks.
fn parse_diff_hunks(diff: &str) -> Vec<(String, u32, u32)> {
    let mut hunks = Vec::new();
    let mut current_file = String::new();

    for line in diff.lines() {
        // Detect file header: +++ b/path/to/file
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_file = path.to_string();
            continue;
        }

        // Detect hunk header: @@ -old_start,old_count +new_start,new_count @@
        if line.starts_with("@@") && !current_file.is_empty() {
            // Parse the +new_start,new_count part
            if let Some(plus_part) = line.split('+').nth(1) {
                let range_part = plus_part.split(' ').next().unwrap_or("");
                let parts: Vec<&str> = range_part.split(',').collect();
                let start: u32 = parts[0].parse().unwrap_or(0);
                let count: u32 = if parts.len() > 1 {
                    parts[1].parse().unwrap_or(1)
                } else {
                    1
                };
                if start > 0 {
                    let end = if count == 0 { start } else { start + count - 1 };
                    hunks.push((current_file.clone(), start, end));
                }
            }
        }
    }

    hunks
}

fn cmd_visualize(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let output_path = prism.root().join(".infigraph").join("graph.html");
    let path = infigraph_core::viz::generate_html(&gq, &output_path)?;
    println!("Graph visualization written to: {}", path);
    Ok(())
}

fn cmd_routes(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let routes = infigraph_core::routes::detect_routes(&gq)?;
    println!("{}", infigraph_core::routes::format_routes(&routes));
    Ok(())
}

fn cmd_visualize_symbol(root: &Path, symbol_id: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let safe_name: String = symbol_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let output_path = prism
        .root()
        .join(".infigraph")
        .join(format!("symbol-{safe_name}.html"));
    let path = infigraph_core::viz::generate_symbol_html(&gq, symbol_id, depth, &output_path)?;
    println!("Symbol subgraph written to: {}", path);
    Ok(())
}

fn cmd_watch(root: &Path, debounce: u64) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    println!(
        "Watching {} (debounce {}ms) — Ctrl-C to stop",
        root.display(),
        debounce
    );

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();

    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    })
    .ok();

    infigraph_core::watch::watch_project(&prism, debounce, stop_rx, |evt| {
        println!("[watch] {evt}");
    })?;

    println!("Watch stopped.");
    Ok(())
}

fn cmd_scip_import(root: &Path, index_path: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let abs_index = if index_path.is_absolute() {
        index_path.to_path_buf()
    } else {
        root.join(index_path)
    };

    println!("Importing SCIP index from {}", abs_index.display());
    let stats = infigraph_core::scip::import_scip_index(&abs_index, store)?;
    println!(
        "SCIP import complete:\n  files processed: {}\n  symbols added: {}\n  symbols enriched: {}\n  relations added: {}\n  references added: {}",
        stats.files_processed,
        stats.symbols_added,
        stats.symbols_enriched,
        stats.relations_added,
        stats.references_added,
    );
    Ok(())
}

fn cmd_index_manifests(root: &Path) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let results = infigraph_core::manifest::index_manifests(root, store)?;
    if results.is_empty() {
        println!("No manifests found.");
        return Ok(());
    }
    let total: usize = results.iter().map(|r| r.deps.len()).sum();
    println!(
        "Indexed {} manifests, {} dependencies:\n",
        results.len(),
        total
    );
    for r in &results {
        println!(
            "  {} [{}]: {} deps",
            r.manifest_file,
            r.ecosystem,
            r.deps.len()
        );
    }
    Ok(())
}

fn cmd_dependencies(root: &Path, ecosystem: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let mut deps = infigraph_core::manifest::query_deps(store)?;
    if let Some(eco) = ecosystem {
        deps.retain(|d| d.ecosystem == eco);
    }
    if deps.is_empty() {
        println!("No dependencies found. Run 'infigraph index-manifests' first.");
        return Ok(());
    }
    println!("Dependencies ({}):\n", deps.len());
    let mut cur_eco = String::new();
    for d in &deps {
        if d.ecosystem != cur_eco {
            println!("  [{}]", d.ecosystem);
            cur_eco = d.ecosystem.clone();
        }
        let dev_tag = if d.is_dev { " (dev)" } else { "" };
        println!("    {}@{}{}", d.name, d.version, dev_tag);
    }
    Ok(())
}

fn cmd_find_refs(root: &Path, symbol: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let refs = gq.find_all_references(symbol)?;
    if refs.is_empty() {
        println!("No references found for '{}'", symbol);
        return Ok(());
    }
    println!("References to '{}' ({} total):\n", symbol, refs.len());
    for r in &refs {
        println!("  {}:{:<6} in {}", r.file, r.line, r.caller_name);
    }
    Ok(())
}

fn cmd_api_surface(root: &Path, file_filter: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let mut syms = gq.get_api_surface()?;
    if let Some(f) = file_filter {
        syms.retain(|s| s.file.contains(f));
    }

    println!("API Surface ({} symbols):\n", syms.len());
    let mut cur_file = String::new();
    for s in &syms {
        if s.file != cur_file {
            println!("  {}", s.file);
            cur_file = s.file.clone();
        }
        println!("    [{:<10}] L{:<5} {}", s.kind, s.line, s.name);
    }
    Ok(())
}

fn cmd_file_deps(root: &Path, file: &str) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let deps = gq.get_file_deps(file)?;
    println!("File dependencies for '{}':\n", file);
    println!("  Imports ({}):", deps.imports.len());
    for f in &deps.imports {
        println!("    → {}", f);
    }
    if deps.imports.is_empty() {
        println!("    (none)");
    }
    println!("\n  Imported by ({}):", deps.imported_by.len());
    for f in &deps.imported_by {
        println!("    ← {}", f);
    }
    if deps.imported_by.is_empty() {
        println!("    (none)");
    }
    Ok(())
}

fn cmd_type_hierarchy(root: &Path, symbol: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let hier = gq.get_type_hierarchy(symbol, depth)?;
    println!("Type hierarchy for '{}':\n", hier.root_name);
    println!("  Ancestors ({}):", hier.ancestors.len());
    for a in &hier.ancestors {
        println!("    ↑ {} [{}]  ({})", a.name, a.kind, a.file);
    }
    if hier.ancestors.is_empty() {
        println!("    (none — root type)");
    }
    println!("\n  Descendants ({}):", hier.descendants.len());
    for d in &hier.descendants {
        println!("    ↓ {} [{}]  ({})", d.name, d.kind, d.file);
    }
    if hier.descendants.is_empty() {
        println!("    (none — leaf type)");
    }
    Ok(())
}

fn cmd_test_coverage(root: &Path, file_filter: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let mut cov = gq.get_test_coverage()?;
    if let Some(f) = file_filter {
        cov.covered.retain(|s| s.file.contains(f));
        cov.uncovered.retain(|s| s.file.contains(f));
        let total = cov.covered.len() + cov.uncovered.len();
        cov.coverage_pct = (cov.covered.len() * 100).checked_div(total).unwrap_or(0);
        cov.covered_count = cov.covered.len();
        cov.uncovered_count = cov.uncovered.len();
    }

    println!(
        "Test Coverage: {}%  ({} covered / {} uncovered)\n",
        cov.coverage_pct, cov.covered_count, cov.uncovered_count
    );

    if !cov.uncovered.is_empty() {
        println!("Uncovered ({}):", cov.uncovered.len());
        for s in cov.uncovered.iter().take(50) {
            println!("  ✗  {:<40} [{}]  {}", s.symbol_name, s.kind, s.file);
        }
        if cov.uncovered.len() > 50 {
            println!("  ... and {} more", cov.uncovered.len() - 50);
        }
    }
    Ok(())
}

fn cmd_security(root: &Path, severity: Option<&str>, category: Option<&str>) -> Result<()> {
    let canonical = root.canonicalize().context("invalid project root")?;
    let mut scan = infigraph_core::security::scan_project(&canonical)?;

    if let Some(sev) = severity {
        let sev_upper = sev.to_uppercase();
        scan.findings
            .retain(|f| f.severity.to_string() == sev_upper);
    }
    if let Some(cat) = category {
        let cat_norm = cat.to_lowercase().replace(' ', "");
        scan.findings
            .retain(|f| f.category.to_string().to_lowercase().replace(' ', "") == cat_norm);
    }

    println!("{}", infigraph_core::security::format_scan_results(&scan));
    Ok(())
}

fn cmd_complexity(root: &Path, threshold: u32, file: Option<&str>) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("graph not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);

    let base_q = if let Some(f) = file {
        format!(
            "MATCH (s:Symbol) WHERE (s.kind = 'Function' OR s.kind = 'Method' OR s.kind = 'Test') AND s.file CONTAINS '{}' RETURN s.name, s.file, s.start_line, s.complexity ORDER BY s.complexity DESC",
            f.replace('\'', "\\'")
        )
    } else {
        "MATCH (s:Symbol) WHERE (s.kind = 'Function' OR s.kind = 'Method' OR s.kind = 'Test') RETURN s.name, s.file, s.start_line, s.complexity ORDER BY s.complexity DESC".to_string()
    };

    let rows = gq.raw_query(&base_q)?;
    if rows.is_empty() {
        println!("No symbols found. Run 'infigraph index' first.");
        return Ok(());
    }

    let total: u32 = rows
        .iter()
        .filter_map(|r| r.get(3).and_then(|v| v.parse::<u32>().ok()))
        .sum();
    let avg = total as f64 / rows.len() as f64;
    let hotspots: Vec<_> = rows
        .iter()
        .filter(|r| r.get(3).and_then(|v| v.parse::<u32>().ok()).unwrap_or(0) >= threshold)
        .collect();

    println!(
        "Complexity: {} symbols, avg {:.1}, {} hotspots (>= {})\n",
        rows.len(),
        avg,
        hotspots.len(),
        threshold
    );

    for row in rows.iter().take(30) {
        let name = row.first().map(|s| s.as_str()).unwrap_or("?");
        let file = row.get(1).map(|s| s.as_str()).unwrap_or("?");
        let line = row.get(2).map(|s| s.as_str()).unwrap_or("?");
        let cplx = row.get(3).map(|s| s.as_str()).unwrap_or("0");
        let flag = if cplx.parse::<u32>().unwrap_or(0) >= threshold {
            " ⚠"
        } else {
            ""
        };
        println!("  [{cplx:>3}] {name}  ({file}:{line}){flag}");
    }
    Ok(())
}

fn cmd_refactor(root: &Path, target: Option<&str>, focus: &str, limit: usize) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;

    let store = prism.store().context("not initialized")?;
    let conn = store.connection()?;

    let emb_path = root.join(".infigraph").join("embeddings.bin");
    let emb_ref = if emb_path.exists() {
        Some(emb_path.as_path())
    } else {
        None
    };

    let focus = infigraph_core::refactor::Focus::parse(focus);
    let recs = infigraph_core::refactor::analyze(&conn, emb_ref, target, focus, limit)?;
    print!(
        "{}",
        infigraph_core::refactor::format_recommendations(&recs, target)
    );
    Ok(())
}

fn cmd_semantic_diff(root: &Path, old_ref: &str, new_ref: &str) -> Result<()> {
    let canonical = root.canonicalize().context("invalid project root")?;
    let registry = bundled_registry()?;
    let diff = infigraph_core::diff::semantic_diff(&canonical, old_ref, new_ref, &registry)?;
    println!("{}", infigraph_core::diff::format_diff(&diff));
    Ok(())
}

fn cmd_sequence(root: &Path, symbol_id: &str, depth: u32) -> Result<()> {
    let registry = bundled_registry()?;
    let mut prism = Infigraph::open(root, registry)?;
    prism.init()?;
    let store = prism.store().context("not initialized")?;
    let conn = store.connection()?;
    let gq = infigraph_core::graph::GraphQuery::new(&conn);
    let diagram = infigraph_core::sequence::generate_sequence_mermaid(&gq, symbol_id, depth)?;
    println!("{}", diagram);
    Ok(())
}
